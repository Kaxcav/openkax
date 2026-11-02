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
| vercel | `https://ai-gateway.vercel.sh/v1` | **credit** | modelo | a medir | só modelos com preço 0 em `/v1/models` |
| zai | `https://api.z.ai/api/paas/v4` | free | modelo | 60 RPM, `max_concurrent = 1`, 128k | `disable_thinking = { thinking = { type = "disabled" } }` (documentado) |
| cloudflare | `https://api.cloudflare.com/client/v4/accounts/${CLOUDFLARE_ACCOUNT_ID}/ai/v1` | free | provider | 10k neurons/dia (sem bucket equivalente) | desligado até existir `CLOUDFLARE_API_TOKEN` |
| cohere | `https://api.cohere.ai/compatibility/v1` | free (trial) | provider | 20 RPM, rpd 30, 1000/mês | `disable_thinking = { reasoning_effort = "none" }` (documentado) |
| sambanova | `https://api.sambanova.ai/v1` | free | modelo | 20 RPM, 20 RPD, 200k TPD | buckets corrigidos pelos headers `x-ratelimit-*` (nomes via smoke) |
| deepseek | `https://api.deepseek.com/v1` | **paid** | provider | 60 RPM | — |

### Rotas (free → credit → paid, validado no parse)

| Classe | Rota |
|---|---|
| `kax/coder-fast` | groq → zai(glm-4.7-flash) → sambanova(deepseek-v3.1 → llama-3.3-70b → gpt-oss-120b) → nvidia(minimax) → gemini-flash-lite → vercel → deepseek |
| `kax/coder-smart` | gemini-flash → nvidia(kimi) → mistral(codestral) → zai(glm-4.7-flash) → github → sambanova(deepseek-v3.1) → openrouter → vercel → deepseek |
| `kax/reviewer` | mistral → cohere → zai → nvidia(deepseek-v4.1-flash) → gemini → deepseek |
| `kax/summarizer` | gemini-flash-lite → gemini-flash → cloudflare → openrouter → vercel |
| `kax/cheap-long` | gemini-flash → mistral → nvidia(minimax) — `max_ctx` do pacote filtra |

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
- **N6 — mensais**: `requests_per_month`/`tokens_per_month` são só informativos até o snapshot da F0-10; o pacing é via `rpd`.
- **Headers de saldo**: `quirks.ratelimit_headers = { rpm?, tpm?, rpd?, tpd? }` mapeia nomes de header → bucket. O valor corrige o escopo dono do limite (modelo antes do provider), em sucesso e em erro.
- **Pin de depuração** (F0-14): `x-openkax-pin: provider/modelo`.
  - Fixa o candidato, até fora da rota, mas passa por gate, reserva e ledger.
  - Modelo desabilitado → 409 com o motivo.
  - A resposta traz os headers de limite do upstream como `x-openkax-upstream-<nome>`, numa lista branca: `x-ratelimit-*`, `ratelimit*`, `retry-after`, `x-request-limit*`.

## Consequências

- `providers.toml` antigo com `paid = true` não parseia mais (`deny_unknown_fields`): use `tier = "paid"`.
- Cliente sem streaming recebe só `message.content`, sem raciocínio.
- Uma request com raciocínio longo e sem conteúdo em 30 s cai no próximo candidato, mesmo que o upstream fosse responder.
- Z.ai com `max_concurrent = 1`: rajadas paralelas de `coder-fast` escorrem para SambaNova/NVIDIA, que é o comportamento desejado.
