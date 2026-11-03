# OpenKax — instruções para o Claude Code

Você é o **lead técnico** deste repositório. A fonte de verdade é `PLANO_MESTRE.md` — leia inteiro antes de qualquer ação na primeira sessão e releia a seção da fase atual ao retomar.

## O que é este projeto
Gateway/proxy local de IA em Rust (binário único Windows, < 8 MB, memória privada < 10 MB ociosa após uso; working set só informativo) que:
1. Expõe `/v1/chat/completions` e `/v1/messages` (Anthropic) em `localhost:8000`.
2. Roteia **classes** (`kax/coder-fast`, `kax/coder-smart`, `kax/summarizer`, `kax/reviewer`, `kax/cheap-long`) para um pool de providers free/baratos com scheduler de quota (RPM/TPM/RPD/TPD) e fallback pré-primeiro-token.
3. Faz passthrough byte-a-byte de `claude-*` para a Anthropic.
4. Expõe um MCP server (`openkax mcp`) com `delegate_batch` para o lead despachar tarefas verificadas em paralelo para operários.

Modo B (MCP com pacote enxuto) é o principal. Modo A (rewrite de modelo via `ANTHROPIC_DEFAULT_HAIKU_MODEL`) é bônus.

## Fase atual
**Em andamento (2026-11-03): P6 kax-context** (ADR 0009, `src/context/`): contexto enxuto por símbolo para os pacotes. P6a (CONTRACT) em revisão no PR draft `feat/context-contract`; P6b (adapters TS/JS e Python), P6c (adapter Rust, índice, resolução, pack) e P6d (ligar `files[].symbols`/`lines` ao pacote) a fazer. Aceite: `cargo test -- --ignored` em `tests/context_rust.rs` (P6c) e `tests/context_ts_py.rs` (P6b).

**Fase 1 — Ponte Anthropic** (branch `fase-1`; a Fase 0 já está na `main`). Critério de saída no `PLANO_MESTRE.md` §4 e tarefas F1-* em `docs/noturno/RUNBOOK.md`.
Progresso (2026-11-02): Tarefas F1-1 a F1-7 concluídas; F1-8 (documentação, ADR 0006 aceito e roteiro de aceite manual) em revisão.
- F1-1..F1-4: Tipos e tradução Anthropic ↔ OpenAI, endpoint `POST /v1/messages` para classes `kax/*` (stream/não-stream), fallback pré-1º-token, assinaturas Gemini (`thought_signatures`) e quirks (`alnum9`).
- F1-5: `POST /v1/messages/count_tokens` para `kax/*` usando `chars_per_token` do 1º degrau, sem reservar cota nem checar limite de pacote.
- F1-6: Passthrough byte a byte de `claude-*` para upstream nativo Anthropic (`/v1/messages` e `/v1/messages/count_tokens`) com client dedicado sem timeout total (`connect_timeout = 10s`, `read_timeout = 300s`), filtro de hop-by-hop e preservação de múltiplos headers.
- F1-7: Seção opcional `[aliases]` no TOML (Modo A) preservando ordem de declaração (primeiro match com glob `*` vence), validado no load/reload.
- Testes atuais: 185 unitários + 79 de integração (total 264 testes verdes, 0 ignored, clippy e fmt limpos). Ver `tasks.json`.

## Como usar

Gateway em `localhost:8000`:
```powershell
$env:ANTHROPIC_BASE_URL="http://localhost:8000"
```

Modo A opcional (subagente Explore / haiku roteado para classe local):
```powershell
$env:ANTHROPIC_DEFAULT_HAIKU_MODEL="kax/summarizer"
```
ou via `[aliases]` no `providers.toml`:
```toml
[aliases]
"claude-haiku-*" = "kax/summarizer"
```

## Fase 2 — como usar o MCP `kax-workers`
- Registrar: `claude mcp add kax-workers -- <caminho>\openkax.exe mcp` (stdio; `OPENKAX_CONFIG` aponta o `providers.toml`).
- Tools: `delegate`, `delegate_batch` (background, `parallelism` 4, máx. 16), `status(batch_id)`, `quota`, `report(day|week|month)`.
- Cada tarefa roda num worktree `.kax/wt/<id>`; aceita → branch `kax/<id>` para integrar. Recusa/esgotou → `needs_lead` (você faz).
- Verify: comandos de `accept.verify`; `.kax/verify.toml` ajusta `timeout_s`, `max_log_bytes` e `env_passthrough` (nunca segredo; ver ADR 0007).
- Sandbox (ADR 0008): `sandbox = "bwrap"` é o padrão. O verify roda sem rede, com `$HOME` vazio e só o worktree gravável. Sem bwrap (Windows, macOS, Linux sem bubblewrap) ele **recusa** e a tarefa vai para `needs_lead`. `sandbox = "none"` só vale escrito e deixa aviso no log. Para delegar do Windows, rode o `openkax mcp` no WSL (`docs/sandbox-wsl.md`) e passe `repo` como caminho Linux. `git` não funciona dentro da caixa.
- Prefetch: `prefetch = ["cargo fetch --locked"]` no `[[language]]` roda com rede, fora da caixa, sobre o `base_ref` e antes do diff do operário. Se falhar, vai para `needs_lead`. Dependência nova trazida pelo diff faz o verify offline falhar, de propósito.
- `openkax doctor` mostra a linha `sandbox` (OK/WARN/FAIL).
- Ledger `kind:"task"` em `%LOCALAPPDATA%\OpenKax\ledger`; limpeza: `openkax clean [--repo <path>] [--all]`.
- Aceite manual: `docs/smoke/fase-2-aceite.md`; decisão: ADR 0007.

## Ambientes (ver ADR 0001)
Dois ambientes de dev; descubra em qual está (`uname`/plataforma) antes de propor comandos.
- **Notebook (WSL2 Ubuntu)**: repo em `~/openkax` (ext4). Comandos **bash em Linux**, nunca PowerShell. Tem WDAC em modo enforce: **nenhum `.exe` não assinado roda no host**; rode o binário Linux dentro do WSL (`127.0.0.1:8000`) — clientes no Windows alcançam via `localhost:8000`.
- **PC de casa (`kaxcavmaster`, Windows nativo)**: sem WDAC, sem VPN, repo em `D:\dev\openkax`. PowerShell e Git Bash permitidos; `cargo test`/`clippy` rodam no Windows e o `.exe` roda localmente (medir tamanho/memória privada, smoke). Rede sem proxy: é daqui que se confere a Groq.
- O `.exe` de **release** vem do GitHub Actions (`windows-latest`); o build local é só para medir e testar. Código é cross-platform; integração com SO (tray, autostart, paths) isolada atrás do trait `Platform` (`UnixPlatform` XDG, `WindowsPlatform` `%LOCALAPPDATA%\OpenKax`).
- Validação de chaves: `./doctor.sh` (Linux) ou `.\doctor.ps1` (Windows sem WDAC).

## Stack e regras de código
- Rust estável. `tokio` (2 workers), `axum`, `reqwest` (rustls, **sem openssl**), `serde`, `toml`, `tracing`, `bytes`, `futures`, `rmcp`, `notify`.
- Sem SQLite na v1. Ledger JSONL; buckets em snapshot JSON.
- Release: `lto="fat"`, `codegen-units=1`, `opt-level="z"`, `panic="abort"`, `strip=true`.
- Módulos: `config`, `scheduler`, `provider`, `translate`, `server`, `mcp`, `ledger`, `verify`.
- Provider é **dado** (`providers.toml`), nunca código especializado. Quirks vão no TOML.
- Testes antes da implementação para scheduler e translator. `cargo clippy -- -D warnings` limpo.
- Nunca logar corpo de request nem credenciais. Chaves só via env.

## Ciclo de trabalho (por feature)
PLAN → CONTRACT → FAN-OUT → VERIFY → INTEGRATE → REVIEW → RECORD
- CONTRACT: tipos, traits, schemas e stubs que **compilam** + testes de aceite, commitados antes de qualquer delegação.
- VERIFY é da máquina (`cargo check`, `cargo test`, `clippy`); nada entra sem passar.
- REVIEW é sobre diff, não sobre prosa.
- RECORD: decisão arquitetural → `docs/decisions/NNNN-titulo.md` (contexto, decisão, consequências). Tarefas em `tasks.json`.

## Política de delegação (quando o MCP `kax-workers` existir)
**Delegar:** ≥3 itens paralelizáveis; output-heavy (testes, docs, fixtures, adapters, boilerplate); leitura pesada; migração mecânica contra padrão dado.
**Nunca delegar:** arquitetura; auth, pagamentos, migrações destrutivas, criptografia; refactor cross-cutting; spec ambígua; edit único.
**Pacote mínimo:** objetivo em 1 frase; paths exatos + trechos; tipos/assinaturas a honrar; critério de aceite (comandos + esperado); restrições; formato de saída (diff unificado).
Saída de operário = PR de desconhecido: só chega a você depois do `verify` passar. Falhou 2x → você faz.

## Convenções
- Commits: `tipo(escopo): resumo` (feat, fix, test, refactor, docs, chore).
- Uma branch por fase; features em `feat/<nome>`.
- Sem placeholders, sem `todo!()` em código commitado, sem stubs fora da fase CONTRACT.
- PT-BR em docs e comentários de alto nível; identificadores em inglês.

## Ao terminar uma sessão
Atualize `tasks.json`, a seção "Fase atual" acima e registre decisões pendentes em `docs/decisions/`. Deixe o repo compilando.
