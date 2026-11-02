# 0003 — Escopo de limites, reservas e max_tokens

Data: 2026-11-02
Status: aceita

## Contexto

O plano (§3.3) modelava um bucket RPM/TPM/RPD/TPD **por provider**. Na prática os limites têm escopos diferentes:

- **Groq** e **Gemini** (free tier) e **GitHub Models** limitam **por modelo**.
- **Mistral**, **OpenRouter** e **DeepSeek** limitam **por conta/workspace**.

Além disso, requests concorrentes podem passar juntas pela checagem de saldo e estourar o TPM, e clientes como o Claude Code mandam `max_tokens: 32000`, que o Groq conta no TPM (8k). Com isso o Groq daria "nunca cabe" em 100% das requests.

## Decisão

### A1 — dois escopos de limite

- `[[provider]].limits` é o escopo compartilhado (opcional).
- `models = [{ id, upstream, limits?, max_ctx? }]` é o escopo por modelo (opcional). `id` é o nome local usado nas rotas; `upstream` é o ID real, conferido em `/models` (obrigatório em provider habilitado).
- `reserve()` debita nos **dois** escopos que existirem, atomicamente. Os locks são sempre tomados na ordem fixa provider → modelo, com seção crítica curta e nunca através de `.await` (a API do scheduler é toda síncrona).
- Transições por escopo:
  - 401/403 → **provider** `Disabled` até reload.
  - 404 → só o **modelo** `Disabled` até reload.
  - 429 → cooldown no escopo que estourou: o modelo, se ele tem `limits` próprios; senão o provider. O bucket RPM desse escopo é zerado.
  - 5xx, timeout, erro de rede, erro de protocolo e outros 4xx → cooldown do **provider** pela tabela `cooldown`.
  - 400/413/422 (culpa da request) → nenhum cooldown. Um contador `request_faults` no modelo é exposto em `/quota`, e no despacho entra no máximo 1 candidato extra antes de devolver o erro ao cliente (A3).
- Teto do cooldown de 429 = tempo até o próximo reset diário do escopo (A4). Se o RPD/TPD do escopo zerou ou o corpo indica cota diária, o cooldown vai até o reset.

### A2 — `Reservation` com `Drop`

`pick()` não debita; `reserve()` re-checa e debita 1 request + `reserve_tokens`. A reserva fecha de um destes jeitos:

| Como fecha | Tokens | Request (RPM/RPD) | Estado |
|---|---|---|---|
| `settle(usage)` | reconcilia com `usage` real (refund ou charge); recalibra chars/token por EMA | contada | — |
| `fail(err)` antes do 1º token | devolvidos | contada | transição da tabela acima |
| `fail(err)` depois do 1º token (vindo do upstream) | cobra o estimado pelo texto transmitido | contada | cooldown `on_5xx_s` no provider |
| `Drop` antes do 1º token | devolvidos | contada | **nenhuma** |
| `Drop` depois do 1º token (cliente desconectou) | cobra o estimado pelo texto transmitido | contada | **nenhuma** |

Desconexão do cliente nunca gera cooldown no provider.

### Decisão 2 — `max_tokens`

Duas quirks por provider:

- `tpm_counts_max_tokens` (true só no Groq);
- `max_tokens_cap`.

Com `max_tokens_cap`, o `max_tokens`/`max_completion_tokens` enviado ao upstream é **clampado** no cap (sem `max_tokens` do cliente, envia o cap). Com `tpm_counts_max_tokens`, a reserva é `entrada + min(max_tokens, cap)`. Sem essa quirk, a reserva é só a entrada e o `settle` acerta pelo `usage`.

Para o Groq: cap 2000, então 6000 (pacote máximo de `coder-fast`) + 2000 = 8000 TPM.

### Complementos

- `stream_options.include_usage = true` é enviado a todo provider sem a quirk `no_stream_usage`. `strip_params` roda por último e pode remover qualquer campo.
- Providers com IDs ainda não verificados ficam `enabled = false` (dispensam `upstream`).
- Pago sempre por último em cada rota (validado no parse).

## Consequências

- O `providers.toml` mudou de formato (`models` virou lista de tabelas). Configs antigas não parseiam (`deny_unknown_fields`), o que é intencional.
- Um modelo pode estar em cooldown com o provider saudável; `/quota` mostra os dois escopos.
- O saldo de TPM pode ficar negativo após um `settle` acima da reserva, o que freia o escopo naturalmente.
- `tokens_per_month` continua informativo até existir snapshot persistente dos buckets.
