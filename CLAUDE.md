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
**Fase 1 — Ponte Anthropic** (branch `fase-1`, empilhada na `fase-0`). Critério de saída no `PLANO_MESTRE.md` §4 e tarefas F1-* em `docs/noturno/RUNBOOK.md`.
Fase 0 fecha com o CI verde da F0-24 (ver `tasks.json`): 4 testes de fallback com `wiremock`, `tests/client_compat.rs` no lugar do aceite manual Continue/Cline (ADR 0004), binário < 8 MB, memória privada < 10 MB ociosa após uso. O PR `fase-0 → main` continua aberto: merge é só do Kaxcav.
Progresso (2026-11-02): scheduler/dispatcher/ledger + providers de cauda (ADR 0003/0004); cooldown por tipo de erro (F0-17); timeout de 1º token por modelo (F0-18); Cohere sem raciocínio + `ascii_json` (F0-19); rotas F0-20; Groq/Cloudflare habilitados, 410 e headers IETF (F0-21); hot-reload + snapshot dos buckets (F0-10, ADR 0005); WindowsPlatform + CI ubuntu/windows com artefato `.exe` (F0-11); GitHub Models removido e Vercel desligada/fora das rotas (F0-22); critério de memória privada + pool/blocking enxutos (F0-23, ADR 0002); compat de clientes (F0-24). Testes: 99 unitários + 31 de integração. Release Windows local 2,94 MiB; memória privada ociosa após 5 requests 3,82 MiB (working set 14,7 MiB, informativo). Ver `tasks.json`.

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
