# OpenKax — Plano Mestre

> Gateway local em Rust + sistema de orquestração onde Claude (Max 20x) é o líder técnico e um pool de modelos free/baratos são os operários. Este documento é a fonte de verdade para o Claude Code conduzir o projeto.

---

## 0. Tese e reality check

**Tese:** um lead forte (Claude Opus/Sonnet) com contexto completo + N operários baratos em paralelo + verificação determinística (compilador, tipos, testes) produz software robusto — não porque os operários são bons, mas porque **o sistema nunca confia neles**.

**O que muda com o plano Max 20x:** a cota deixa de ser o problema principal. O valor do pool free muda de "necessidade" para:
1. **Paralelismo a custo marginal zero** — 10–20 tarefas simultâneas enquanto o lead pensa.
2. **Autonomia noturna** — filas rodando em cotas que resetam diariamente, lead revisa de manhã.
3. **Nunca travar** — se qualquer provider cai, o pipeline continua.
4. **Cota do lead vai pra julgamento**, não pra digitar boilerplate.

**Onde é mais lento:** tarefas com muita integração e pouco output (um edit cirúrgico). **Onde é mais rápido:** tarefas output-heavy e paralelizáveis (testes, docs, CRUDs, adapters, fixtures, migrações mecânicas).

**O que torna robusto (independe do OpenKax — começa hoje):**
- Contract-first: tipos, interfaces, schemas e testes antes de qualquer fan-out.
- Worktree isolado por tarefa; nada entra na branch principal sem passar no `verify`.
- Lead revisa **diff**, nunca prosa.
- Decision log (ADRs) e métricas de aceitação por classe/provider com rebaixamento automático.

---

## 1. Arquitetura

```
┌─────────────────────────────────────────────────────────────────┐
│  Claude Code (LEAD — Opus plano/revisão, Sonnet integração)      │
│  CLAUDE.md = sistema operacional do processo                     │
└───────┬───────────────────────────────┬─────────────────────────┘
        │ ANTHROPIC_BASE_URL            │ MCP stdio: kax-workers
        ▼                               ▼
┌──────────────────────── OpenKax (binário único, ~5–10 MB) ──────┐
│  HTTP :8000                                                      │
│   /v1/messages            ← Claude Code (Anthropic Messages)     │
│   /v1/messages/count_tokens                                      │
│   /v1/chat/completions    ← Continue / Cline / Roo / scripts     │
│   /v1/models  /quota  /health                                    │
│                                                                  │
│  Router:                                                         │
│   claude-*  → passthrough Anthropic (auth do cliente intocado)   │
│   kax/<classe> → Scheduler → Provider                            │
│                                                                  │
│  Scheduler: token buckets (RPM/TPM/RPD/TPD) por provider,        │
│   estimativa de tokens → filtra candidatos → ordem da classe →   │
│   fallback pré-1º-token → cooldown por tipo de erro              │
│                                                                  │
│  Translator: Anthropic Messages ⇄ OpenAI Chat (req + SSE)        │
│  Providers: um adapter OpenAI-compat + quirks via providers.toml │
│  Ledger: JSONL de toda request (provider, tokens, latência, fim) │
│                                                                  │
│  MCP server (subcomando `openkax mcp`):                          │
│   delegate / delegate_batch / status / quota / report            │
│   → fan-out → worktree por tarefa → verify → resultado           │
└──────────────────────────────────────────────────────────────────┘
        │
        ▼
 Gemini · Mistral · Groq · GitHub Models · DeepSeek · OpenRouter · Anthropic
```

### 1.1 Classes de modelo (o editor e o lead só conhecem classes)

| Classe | Uso | Ordem de providers (exemplo — validar no `providers.toml`) | Limite de pacote |
|---|---|---|---|
| `kax/summarizer` | resumir arquivos, extrair APIs, classificar | Gemini Flash-Lite → Gemini Flash → OpenRouter free | até 200k |
| `kax/coder-fast` | função com assinatura dada, testes, docstrings, rename | Groq (qwen/llama) → Gemini Flash → DeepSeek | ≤ 6k tokens |
| `kax/coder-smart` | módulo isolado contra contrato, adapter, CRUD | Gemini Flash → Mistral (Codestral/Medium) → DeepSeek V3 → OpenRouter → Haiku (pago) | ≤ 60k |
| `kax/reviewer` | segunda opinião de **família diferente** do autor | Mistral → Gemini → DeepSeek | ≤ 60k |
| `kax/cheap-long` | request gorda e rara | Gemini Flash (1M ctx) → Mistral (500k TPM, 2 RPM) | ≤ 500k |
| `claude-*` | lead | passthrough Anthropic | — |

Regra: **nenhuma classe `kax/*` roteia para Anthropic exceto como último degrau explícito** (`Haiku` em `coder-smart`).

### 1.2 Realidade dos limites (nov/2026 — tratar como dado volátil)

| Provider | Limites free relevantes | Papel |
|---|---|---|
| Gemini (AI Studio) | só Flash/Flash-Lite no free; ~5–15 RPM; 250k TPM; 1M ctx | espinha dorsal das requests gordas |
| Mistral La Plateforme | 1B tok/mês, 500k TPM, **2 RPM** | request gorda e rara; Codestral |
| Groq | por modelo, ex. 30 RPM / 1k RPD / **8k TPM** / 200k TPD | só pacotes enxutos (`coder-fast`) |
| GitHub Models | limites diários baixos por tier | backup de `coder-smart` |
| Cerebras | virou trial (cartão, 30 dias) | **fora** até mudar |
| OpenRouter `:free` | 20 RPM; 50 RPD (ou 1k RPD com US$10 lifetime) | pool variado, Tier 3 |
| DeepSeek direto | pago, centavos | degrau pago "nunca trava" |
| Anthropic | plano Max 20x (lead) / API (Haiku como último degrau) | lead |

**Consequência de design:** subagente nativo do Claude Code manda 15–25k tokens por chamada (system + tools) → Groq rejeita 100%. Por isso o **Modo B (MCP com pacote enxuto) é o principal**; o Modo A (rewrite de modelo) é bônus para Explore/grep.

---

## 2. Processo (o que faz ser robusto)

### 2.1 Ciclo por feature

```
PLAN → CONTRACT → FAN-OUT → VERIFY → INTEGRATE → REVIEW → RECORD
 lead     lead     workers    máquina    lead       lead     lead
```

1. **PLAN** (Opus): decompõe em tarefas; marca cada uma como `lead-only` ou `delegável` com classe.
2. **CONTRACT** (Opus/Sonnet): escreve tipos, interfaces, schemas, assinaturas, stubs que **compilam**, e testes de aceitação. Commit do contrato antes de qualquer worker.
3. **FAN-OUT** (MCP `delegate_batch`): um pacote por tarefa, worktree por tarefa.
4. **VERIFY** (máquina): `cargo check/test`, `tsc`, `eslint`, `pytest` conforme `.kax/verify.toml`. Falhou → retry 1x no próximo provider da classe com o erro anexado → falhou de novo → volta pro lead.
5. **INTEGRATE** (Sonnet): aplica diffs aprovados, resolve conflitos, roda suíte completa.
6. **REVIEW** (Opus): lê diff; opcionalmente pede `kax/reviewer` de outra família antes.
7. **RECORD**: ADR se houve decisão; métricas no ledger; tarefa fechada no `tasks.json`.

### 2.2 Política de delegação (vai no CLAUDE.md)

**Delegar quando:** ≥3 itens paralelizáveis; output-heavy (boilerplate, testes, docs, fixtures, adapters); leitura pesada (resumir muitos arquivos); tradução mecânica (migrar N componentes contra um padrão dado).

**Nunca delegar:** arquitetura; auth, pagamentos, migrações destrutivas, criptografia; refactor cross-cutting; qualquer coisa com spec ambígua; edit único.

**Pacote mínimo (schema em 3.3):** objetivo em 1 frase; arquivos exatos (paths + trechos relevantes); tipos/assinaturas a honrar; critério de aceite (comandos de verify + esperado); restrições (não tocar X, não adicionar deps); formato de saída (diff unificado ou arquivo completo).

**Confiança:** saída de worker = PR de desconhecido. Só o lead vê depois do `verify` passar.

### 2.3 Métricas e critérios de corte

| Métrica | Alvo | Ação se falhar |
|---|---|---|
| Aceitação em 1ª passada por classe | ≥ 70% | < 50% por 2 semanas → classe rebaixada para `summarizer`/testes/docs |
| Aceitação por provider dentro da classe | ≥ 60% | provider desce na ordem automaticamente |
| Retrabalho do lead (tokens corrigindo worker ÷ tokens se fizesse direto) | < 0.6 | revisar granularidade dos pacotes |
| Wall-clock por feature vs baseline Claude Code puro | ≤ 1.2× em integração; < 0.6× em output-heavy | reclassificar tipos de tarefa |

Relatório semanal gerado por `openkax report` a partir do ledger.

---

## 3. Especificação técnica

### 3.1 Stack Rust e orçamento de recursos

- `tokio` (rt-multi-thread, 2 workers), `axum`, `reqwest` (rustls, sem openssl), `serde`/`serde_json`, `toml`, `tracing` + `tracing-appender`, `bytes`, `futures`, `rmcp` (MCP stdio), `notify` (hot-reload do TOML).
- Sem SQLite na v1: ledger em JSONL (`%LOCALAPPDATA%\OpenKax\ledger\YYYY-MM-DD.jsonl`), estado dos buckets em snapshot JSON a cada 5s e no shutdown.
- `Cargo.toml` release: `lto = "fat"`, `codegen-units = 1`, `opt-level = "z"`, `panic = "abort"`, `strip = true`. Alvo: binário < 8 MB, RSS ocioso < 10 MB.
- Windows: `--no-console` (subsystem windows), `openkax tray` opcional na Fase 5, autostart via Task Scheduler.

### 3.2 `providers.toml` (provider = dado, não código)

```toml
[defaults]
request_timeout_ms = 60000
first_token_timeout_ms = 8000
estimate_chars_per_token = 3.6     # recalibrado por provider via `usage`

[[provider]]
id = "gemini"
base_url = "https://generativelanguage.googleapis.com/v1beta/openai"
auth = { kind = "bearer", env = "GEMINI_API_KEY" }
models = ["gemini-flash", "gemini-flash-lite"]
limits = { rpm = 10, tpm = 250000, rpd = 250, max_ctx = 1000000 }
quirks = { strip_params = ["logprobs", "frequency_penalty"], tool_choice = "auto_only" }
cooldown = { on_429_s = 60, on_5xx_s = 20, on_timeout_s = 30, on_4xx_other_s = 600 }

[[provider]]
id = "groq"
base_url = "https://api.groq.com/openai/v1"
auth = { kind = "bearer", env = "GROQ_API_KEY" }
models = ["qwen-coder", "llama-70b"]
limits = { rpm = 30, tpm = 8000, rpd = 1000, tpd = 200000, max_ctx = 128000 }

[[provider]]
id = "mistral"
base_url = "https://api.mistral.ai/v1"
auth = { kind = "bearer", env = "MISTRAL_API_KEY" }
models = ["codestral-latest", "mistral-medium-latest"]
limits = { rpm = 2, tpm = 500000, tokens_per_month = 1000000000 }

[[provider]]
id = "openrouter"
base_url = "https://openrouter.ai/api/v1"
auth = { kind = "bearer", env = "OPENROUTER_API_KEY" }
extra_headers = { "HTTP-Referer" = "http://localhost", "X-Title" = "OpenKax" }
models = ["qwen/qwen3-coder:free", "deepseek/deepseek-chat:free"]
limits = { rpm = 20, rpd = 1000 }

[[provider]]
id = "deepseek"
base_url = "https://api.deepseek.com/v1"
auth = { kind = "bearer", env = "DEEPSEEK_API_KEY" }
models = ["deepseek-chat"]
limits = { rpm = 60 }
paid = true

[[provider]]
id = "anthropic"
base_url = "https://api.anthropic.com"
auth = { kind = "passthrough" }        # repassa Authorization/x-api-key do cliente
native = "anthropic"

[[class]]
id = "kax/coder-fast"
max_packet_tokens = 6000
route = [
  { provider = "groq", model = "qwen-coder" },
  { provider = "gemini", model = "gemini-flash" },
  { provider = "deepseek", model = "deepseek-chat" },
]

[[class]]
id = "kax/coder-smart"
max_packet_tokens = 60000
route = [
  { provider = "gemini", model = "gemini-flash" },
  { provider = "mistral", model = "codestral-latest" },
  { provider = "deepseek", model = "deepseek-chat" },
  { provider = "openrouter", model = "qwen/qwen3-coder:free" },
]
```

### 3.3 Scheduler

```
fn pick(class, request) -> Vec<Candidate>:
  est = estimate_tokens(request) * provider.safety_factor
  for step in class.route (ordem):
     p = provider(step)
     skip se: p.in_cooldown() || est > p.limits.tpm_remaining || est > p.max_ctx
              || p.rpm_bucket.empty() || p.rpd_bucket.empty()
     push Candidate{p, step.model, est}
  return candidatos (fallback percorre nesta ordem)
```

- Token buckets: RPM/TPM com refill contínuo; RPD/TPD com reset no horário do provider (configurável, default UTC 00:00).
- Headers `x-ratelimit-remaining-*` quando existirem corrigem o bucket.
- `usage` da resposta recalibra `chars_per_token` por provider (EMA).
- Cooldown por tipo de erro (tabela no TOML). 401/403 → provider desabilitado até reload (chave inválida, não adianta repetir).
- Máquina de estados do provider: `Healthy → Cooldown(until) → Healthy`; `Disabled` para auth; `Degraded` quando aceitação < limiar (vai pro fim da ordem).

### 3.4 Streaming e fallback

- Fallback **somente antes do primeiro delta de conteúdo**. Erro/timeout/429 antes disso → próximo candidato, cliente não percebe.
- Após o 1º token: sem retry. Morreu no meio → encerra o stream com `finish_reason: "error"` (OpenAI) / `stop_reason: "error"` + evento `error` (Anthropic) e loga. A ferramenta reenvia.
- Buffer opcional dos primeiros N ms (default 0; ligar só se houver provider instável) — reduz a janela de dor ao custo de latência percebida.

### 3.5 Translator Anthropic Messages ⇄ OpenAI Chat

Entrada (`/v1/messages`) → OpenAI:
- `system` (string ou blocos) → mensagem `system`; remover `cache_control`.
- blocos `text` → content; `image` → `image_url` (base64 data URI) ou descartar se provider não suporta (flag em quirks).
- `tool_use` → `assistant.tool_calls[{id, function{name, arguments: json-string}}]`.
- `tool_result` → mensagem `tool` com `tool_call_id`; conteúdo em blocos → concatenar texto.
- `tools[{name, description, input_schema}]` → `tools[{type:"function", function{name, description, parameters}}]`.
- `tool_choice` `{type:"auto"|"any"|"tool"}` → `"auto"|"required"|{function}`.
- `thinking` blocos → descartar na ida (provider free não tem); na volta nunca gerar.
- `max_tokens` obrigatório na Anthropic → repassar; `stop_sequences` → `stop`.

Saída OpenAI → eventos Anthropic SSE:
- `message_start` (id, model, usage.input_tokens estimado)
- texto: `content_block_start{text}` → `content_block_delta{text_delta}` → `content_block_stop`
- tool_calls: `content_block_start{tool_use, id, name}` → `content_block_delta{input_json_delta}` (acumular `arguments` em chunks) → `content_block_stop`
- `message_delta{stop_reason, usage.output_tokens}` com mapeamento `stop→end_turn`, `tool_calls→tool_use`, `length→max_tokens`
- `message_stop`
- `/v1/messages/count_tokens` → estimativa heurística `{input_tokens}`.

Passthrough `claude-*`: proxy byte-a-byte, headers do cliente intocados (inclui `anthropic-beta`, `anthropic-version`, auth). Nunca logar o corpo nem auth — só metadados.

### 3.6 MCP `kax-workers` (`openkax mcp`, stdio)

Tools:

```jsonc
// delegate_batch
{
  "repo": "C:/dev/projeto",
  "base_ref": "main",
  "tasks": [
    {
      "id": "T-014",
      "class": "kax/coder-smart",
      "objective": "Implementar PaymentAdapter contra trait em src/payments/mod.rs",
      "files": [
        { "path": "src/payments/mod.rs", "mode": "read" },
        { "path": "src/payments/stripe.rs", "mode": "write", "exists": false },
        { "path": "tests/payments_stripe.rs", "mode": "read" }
      ],
      "contracts": ["trait PaymentAdapter { ... }"],
      "constraints": ["não adicionar dependências", "não tocar em src/payments/mod.rs"],
      "accept": { "verify": ["cargo check", "cargo test payments_stripe"], "expect": "exit 0" },
      "output": "unified_diff",
      "max_attempts": 2
    }
  ],
  "parallelism": 8
}
```

Execução por tarefa:
1. `git worktree add .kax/wt/T-014 base_ref`
2. Monta pacote enxuto (system prompt curto de operário + arquivos `read` + contratos + restrições + formato).
3. Scheduler escolhe provider pela classe e tamanho do pacote.
4. Aplica diff no worktree; roda `accept.verify`.
5. Falhou → attempt 2 no **próximo** provider com stderr anexado (truncado a 4k).
6. Resultado: `{id, status: accepted|rejected|needs_lead, provider, attempts, diff, verify_log, tokens, latency_ms}`.
7. Worktree aceito vira branch `kax/T-014`; lead integra. Rejeitado fica para inspeção ou é limpo com `openkax clean`.

Outras tools: `status(batch_id)`, `quota()` (saldo por provider), `report(period)`.

`.kax/verify.toml` por repo define comandos por linguagem e timeout; nada roda fora do worktree.

### 3.7 Ledger (JSONL)

```json
{"ts":"...","kind":"request","route":"kax/coder-smart","provider":"gemini","model":"gemini-flash","est_in":4120,"in":3987,"out":812,"latency_ms":2210,"ttft_ms":640,"outcome":"ok"}
{"ts":"...","kind":"task","id":"T-014","class":"kax/coder-smart","provider":"gemini","attempts":1,"status":"accepted","verify_ms":9400}
```

`openkax report --week` → aceitação por classe/provider, retrabalho, latência p50/p95, tokens por classe, requests que caíram no degrau pago.

---

## 4. Fases, entregáveis e critérios de aceite

### Fase 0 — Core do gateway (2–3 dias)
- `axum` + `providers.toml` + hot-reload; `/v1/chat/completions` com classes; scheduler com buckets; fallback pré-1º-token; ledger; `/quota`, `/health`, `/v1/models`.
- Testes com providers mockados (`wiremock`): 429 → próximo; timeout de 1º token → próximo; erro pós-1º-token → encerra limpo; TPM insuficiente → provider pulado sem request.
- **Aceite:** Continue/Cline apontados para `kax/coder-fast` e `kax/coder-smart` funcionam com streaming; binário < 8 MB; RSS < 10 MB ocioso.

### Fase 1 — Ponte Anthropic (2–3 dias)
- `/v1/messages` + `count_tokens`; translator completo (3.5); passthrough `claude-*`.
- **Aceite:** Claude Code com `ANTHROPIC_BASE_URL=http://localhost:8000` opera normal em Sonnet/Opus; com `ANTHROPIC_DEFAULT_HAIKU_MODEL=kax/summarizer`, um subagente Explore roda em Gemini Flash com tool-calling funcionando (Read/Grep/Glob) em ≥ 8 de 10 execuções.

### Fase 2 — MCP `kax-workers` (3–4 dias)
- Subcomando `openkax mcp`; `delegate_batch`/`status`/`quota`/`report`; worktree por tarefa; runners de verify; retry no próximo provider.
- **Aceite:** Claude Code despacha 10 tarefas (ex.: 10 testes unitários para 10 funções existentes), recebe resultados verificados em paralelo; ≥ 7 aceitas em 1ª passada.

### Fase 3 — Processo e piloto (1 semana, paralelo às anteriores)
- CLAUDE.md com política de delegação (2.2), ciclo (2.1), formato de ADR, convenções do repo; `.kax/verify.toml`; `tasks.json` persistido; template de pacote.
- Hooks do Claude Code: `PreToolUse` bloqueando edição direta em arquivos marcados `lead-only`? Não — hooks: `PostToolUse` roda verify rápido após edits do lead; `Stop` exige `tasks.json` atualizado.
- **Piloto:** um projeto real pequeno (1 módulo com 8–15 tarefas). Medir as 4 métricas de 2.3 contra uma baseline Claude Code puro no mesmo escopo.
- **Aceite / go-no-go:** aceitação `coder-smart` ≥ 60% e retrabalho < 0.7. Abaixo disso, workers ficam restritos a testes/docs/summaries e o projeto continua valendo como gateway + ponte.

### Fase 4 — Autonomia noturna (2–3 dias)
- `openkax queue run queue.json`: processa fila de pacotes com verify, abre PRs (`gh pr create`) — nunca merge em main; orçamento por noite; para após 3 falhas consecutivas do mesmo tipo; agenda fan-out pesado logo após o reset diário dos providers.
- Lead (Claude Code) antes de dormir: PLAN + CONTRACT + gera `queue.json`. De manhã: REVIEW dos PRs.
- **Aceite:** acordar com ≥ 5 PRs verificados de uma fila de 8, zero quebras na suíte.

### Fase 5 — Dogfooding e hardening (contínuo)
- OpenKax constrói o OpenKax a partir daqui.
- `openkax tray` (ícone, saldo por provider, pausar/retomar), autostart, logs rotacionados, `openkax doctor` (testa cada chave e reporta limites reais).
- Auto-descoberta de modelos via `/v1/models` de cada provider; alerta quando um provider muda limites (429 inesperado com bucket cheio → sugere revisão do TOML).

---

## 5. Riscos e mitigação

| Risco | Mitigação |
|---|---|
| Free tiers mudam/desaparecem (3x em 2026) | provider = TOML, hot-reload, `doctor`, DeepSeek como degrau pago sempre presente |
| Tool-calling de modelo free quebra em loop agêntico | Modo B como principal; Modo A só para Explore; classe `reviewer` de outra família |
| Lead delega demais e revisão vira gargalo | política 2.2 no CLAUDE.md + métrica de retrabalho + relatório semanal |
| Fragmentação de contexto (worker não conhece o repo) | contract-first, pacote com trechos exatos, `CONVENTIONS.md` curto anexado a todo pacote |
| Integração de peças isoladas quebra | stubs que compilam antes do fan-out; verify no worktree; suíte completa no INTEGRATE |
| Passthrough Anthropic com OAuth de assinatura | byte-a-byte, headers intocados; se qualquer atrito, usar API key para o lead |
| Vazamento de chave/prompt no ledger | ledger só com metadados; corpo nunca persistido; chaves só via env |

---

## 6. Primeiros comandos para o Claude Code

```
1. Ler este documento inteiro. Confirmar escopo: Fase 0 primeiro, Modo B é o principal.
2. Criar o esqueleto: cargo new openkax --bin; módulos: config, scheduler, provider, translate, server, mcp, ledger, verify.
3. Escrever providers.toml de exemplo (seção 3.2) e os testes de scheduler com wiremock ANTES da implementação.
4. Implementar Fase 0 até o aceite. Não iniciar Fase 1 sem os 4 testes de fallback passando.
5. Em paralelo, gerar CLAUDE.md do repo-piloto com a política 2.2 e o ciclo 2.1.
```

---

*Documento vivo. Toda mudança de arquitetura vira ADR em `docs/decisions/`.*
