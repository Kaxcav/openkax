# 0004 — Providers de cauda, tiers e raciocínio

Data: 2026-11-02
Status: aceita

## Contexto

Os free tiers do plano (Gemini, Mistral, Groq, OpenRouter) não bastam para manter as rotas sempre vivas. Seis providers OpenAI-compat entram como cauda. Eles trazem formas novas de limite:

- concorrência (Z.ai);
- cota mensal (Cohere);
- cota diária por modelo com headers próprios (SambaNova);
- créditos (Vercel);
- `base_url` com ID de conta (Cloudflare).

Vários servem modelos de raciocínio, que mandam deltas `reasoning_content`/`reasoning` antes do conteúdo.

## Decisão

### Providers

IDs conferidos em `/models` em 2026-11-02 (Cohere pela API nativa `/v1/models`; Cloudflare sem token).

| Provider | base_url | Tier | Escopo de limite | Limites declarados | Observações |
|---|---|---|---|---|---|
| nvidia | `https://integrate.api.nvidia.com/v1` | free | provider | 40 RPM | natureza dos créditos incerta (doctor mede); `/models` não dá `max_ctx` |
| vercel | `https://ai-gateway.vercel.sh/v1` | **credit** | modelo | a medir | só modelos com preço 0 em `/v1/models`; **desligada e fora das rotas** desde a F0-22 (403 sem cartão) |
| zai | `https://api.z.ai/api/paas/v4` | free | modelo | 60 RPM, `max_concurrent = 1`, 128k | `disable_thinking = { thinking = { type = "disabled" } }` (documentado) |
| cloudflare | `https://api.cloudflare.com/client/v4/accounts/${CLOUDFLARE_ACCOUNT_ID}/ai/v1` | free | provider | 10k neurons/dia (sem bucket equivalente) | desligado até existir `CLOUDFLARE_API_TOKEN` |
| cohere | `https://api.cohere.ai/compatibility/v1` | free (trial) | provider | 20 RPM, rpd 30, 1000/mês | `command-a-03-2025` (sem raciocínio) + `ascii_json` — ver F0-19 |
| sambanova | `https://api.sambanova.ai/v1` | free | modelo | 20 RPM, 20 RPD, 200k TPD | buckets corrigidos pelos headers `x-ratelimit-*` (nomes via smoke) |
| deepseek | `https://api.deepseek.com/v1` | **paid** | provider | 60 RPM | — |

### Rotas (free → credit → paid, validado no parse)

Revisadas em F0-20 (2026-11-02), depois do smoke F0-14, e na F0-22 (sem GitHub e sem Vercel):

| Classe | Rota |
|---|---|
| `kax/coder-fast` | groq(qwen3.8-27b) → mistral(codestral) → zai(glm-4.7-flash) → sambanova(deepseek-v3.1 → llama-3.3-70b → gpt-oss-120b) → nvidia(minimax-m3.1-flash) → gemini-flash-lite → deepseek-flash |
| `kax/coder-smart` | mistral(codestral) → gemini-flash → nvidia(minimax-m3.1-flash) → zai(glm-4.7-flash) → sambanova(deepseek-v3.1) → openrouter(north-mini-code) → deepseek-flash |
| `kax/reviewer` | mistral(codestral) → cohere(command-a) → zai → nvidia(deepseek-v4.1-flash) → gemini-flash → deepseek-pro — `x-openkax-exclude-family` garante revisor de família diferente do autor |
| `kax/summarizer` | gemini-flash-lite → gemini-flash → cloudflare → openrouter(gemma-4-31b) |
| `kax/cheap-long` | gemini-flash → mistral(codestral) → nvidia(minimax) — `max_ctx` do pacote filtra |

Mudanças em relação à rota original: codestral entra na frente do `coder-smart` (o `gemini-flash` pensa sem mandar deltas e estourava o 1º token; ver F0-18) e como 2º degrau do `coder-fast`; o `nvidia(kimi)` saiu (404 no smoke); `github(github-mini)` e os degraus `vercel` saíram na F0-22.

### Requisitos novos

- **N1 — `${VAR}`** em `base_url` e `extra_headers`.
  - A sintaxe é validada no parse e o valor resolvido no start (`AppState`), da mesma fonte de segredos (`Secrets`).
  - Variável ausente → provider `Disabled(missing_env)`, com os nomes em `/quota.providers[].missing_env`.
- **N2 — `limits.max_concurrent`** por escopo.
  - O slot é tomado no `reserve()` e solto em qualquer fechamento da `Reservation` (`settle`, `fail`, `Drop`).
  - Escopo no teto = "não cabe agora" (dica de espera de 1s), então o despacho segue para o próximo candidato sem request.
- **N3 — `tier`**: `free | credit | paid` substitui `paid: bool`.
  - Ordem não decrescente obrigatória em cada rota.
  - O ledger grava `tier` por request e o `/quota` expõe `served_by_tier`.
- **N4 — raciocínio**.
  - Deltas `reasoning_content`/`reasoning`/`reasoning_details` não são repassados.
  - Um chunk que fica vazio depois da remoção não é enviado.
  - Antes do 1º token, raciocínio estende o prazo até `defaults.first_token_timeout_reasoning_ms` (30 s), contado do início da tentativa, mas **não é commit**: EOF/erro/timeout ainda caem no próximo candidato.
  - Os chars de raciocínio entram na estimativa de saída (são cobrados).
  - A quirk `disable_thinking` só leva parâmetro documentado pelo provider.
  - O ledger marca `reasoning: true`.
- **N5 — `family`** por modelo.
  - `x-openkax-exclude-family: a,b` (sem diferenciar caixa) tira essas famílias da rota (`Skip::Excluded`, que não entra no `Retry-After`).
  - É o hook para o `kax/reviewer` de família diferente na Fase 2.
- **N6 — mensais**: `requests_per_month`/`tokens_per_month` viraram buckets de verdade na F0-10 (mês UTC, persistidos no snapshot; ADR 0005). O `rpd` continua espalhando a cota ao longo do mês.
- **Headers de saldo**: `quirks.ratelimit_headers = { rpm?, tpm?, rpd?, tpd? }` mapeia nomes de header → bucket. O valor corrige o escopo dono do limite (modelo antes do provider), em sucesso e em erro.
- **Teto de tier** (F0-14, aprovado na F0-20): header de request `x-openkax-max-tier: free|credit|paid`.
  - Degraus de tier acima do teto viram `Skip::Excluded` (não entram no `Retry-After`); valor inválido → 400.
  - É o jeito de o lead (e o smoke) rodarem classes sem tocar crédito nem dinheiro: com `free`, uma classe só esgotada em free devolve 429 em vez de cair no DeepSeek (pago).
- **chars/token por modelo** (F0-20): a EMA que recalibra a estimativa a cada `usage` vive no escopo do **modelo** (um provider como a NVIDIA serve tokenizers diferentes). `/quota` expõe `providers[].models[].chars_per_token`; o provider não tem mais esse campo.
- **Pin de depuração** (F0-14): `x-openkax-pin: provider/modelo`.
  - Fixa o candidato, até fora da rota, mas passa por gate, reserva e ledger.
  - Modelo desabilitado → 409 com o motivo.
  - A resposta traz os headers de limite do upstream como `x-openkax-upstream-<nome>`, numa lista branca: `x-ratelimit-*`, `ratelimit*`, `retry-after`, `x-request-limit*`.

### F0-19 (2026-11-02) — Cohere

- `command-a-plus-05-2026` tem a feature `reasoning` e vazava "The user says…" no `content` mesmo com `reasoning_effort = "none"`. Trocado por `command-a-03-2025`, o `command-a` sem `reasoning` em `/v1/models?endpoint=chat`. Sem raciocínio, sai a quirk `disable_thinking`.
- No smoke apareceu outro defeito: prompt PT-BR em UTF-8 cru gerava saída degenerada (`W32{A+B}W32…` até o `max_tokens`) ou 422 `NO_VALID_RESPONSE_GENERATED` em 5 de 10 chamadas; o mesmo corpo com não-ASCII escapado (`\uXXXX`) deu 7/7 limpas direto e 5/5 pelo gateway. Quirk nova **`ascii_json`**: só muda a codificação do JSON (mesmo valor), nunca o texto. Nada de limpeza heurística de saída.
- Sem vazamento de raciocínio com o modelo novo → a Cohere continua no `kax/reviewer`.

### F0-21 (2026-11-02) — providers que faltavam

Conferido do PC de casa (sem a rede do notebook):

- **Groq** habilitado. `/models` não tem mais `qwen-coder` nem llama 70B; entra `qwen/qwen3.8-27b` (local `qwen3.8-27b`, ctx 131 072, RPD 1000 e TPM 8000 pelos headers `x-ratelimit-limit-*`). O `reasoning_format` default da Groq é `raw` (`<think>` no content); `reasoning_effort: "none"` é documentado para o Qwen 3.8 27B e vai em `disable_thinking`.
- **GitHub Models** segue desligado: aposentado em 2026-07-30 (docs oficiais). O `200 OK` text/plain era placeholder, não proxy. O doctor agora trata 2xx não-JSON como falha.
- **Cloudflare** habilitada com ID do catálogo vivo (`/ai/models/search?task=Text Generation`), ignorando `deprecation`/`planned_deprecation` e `require_workers_paid`. O probe de chat do doctor também escolhe pelo catálogo.
- **Vercel** habilitada no TOML, mas o AI Gateway responde 403 "requires a valid credit card on file" até em modelo de preço 0 → `auth_rejected` em runtime. Cadastrar cartão é decisão do usuário; nenhum crédito é comprado.
- **410 Gone** = modelo descontinuado → `UpstreamError::Gone` → só o modelo fica `Disabled(model_gone)`, sem cooldown no provider e sem entrar no `Retry-After`.
- **Headers IETF** (`quirks.ratelimit_headers.ietf = true`): `RateLimit: "p";r=…;t=…` + `RateLimit-Policy: "p";q=…;w=…`, casados pelo nome da política (com várias, vale a de menor `r`). Viram um bucket de janela (`q` a cada `w` s, refill contínuo) no escopo do **provider**, debitado a cada request; `r ≤ 0` → cooldown do provider até `t`. `/quota` expõe `providers[].window_remaining`. Na Cloudflare isso é o limite da API da conta; os 10k neurons/dia seguem com pacing por `rpd` e o 429 diário (A4).

### F0-22 (2026-11-02) — GitHub Models removido, Vercel desligada

Decisões do usuário sobre o que a F0-21 deixou em aberto:

- **GitHub Models removido.** Foi aposentado em 2026-07-30 (changelog oficial), então não é mais "desligado até conferir". O provider saiu do `providers.toml`, da rota do `coder-smart`, do `.env.example` (`GITHUB_MODELS_TOKEN`), do `doctor.sh`/`doctor.ps1` e da tabela do PLANO_MESTRE §1.2. A checagem de 2xx não-JSON do doctor continua, porque é genérica.
- **Vercel sem cartão.** O provider fica no TOML com `enabled = false` e o comentário "exige cartão de crédito cadastrado (403), mesmo para modelo grátis", e sai de todas as rotas (`coder-fast`, `coder-smart`, `summarizer`). Com isso nenhuma rota tem degrau `credit`; o tier e a validação de ordem continuam valendo para quando houver. Religar = cadastrar cartão, `enabled = true` e devolver os degraus às rotas.
- O teste `repo_providers_toml_content` fixa as duas coisas: `github` ausente e nenhum degrau `vercel` em rota.

## Consequências

- `providers.toml` antigo com `paid = true` não parseia mais (`deny_unknown_fields`): use `tier = "paid"`.
- Cliente sem streaming recebe só `message.content`, sem raciocínio.
- Uma request com raciocínio longo e sem conteúdo em 30 s cai no próximo candidato, mesmo que o upstream fosse responder.
- Z.ai com `max_concurrent = 1`: rajadas paralelas de `coder-fast` escorrem para SambaNova/NVIDIA, que é o comportamento desejado.
