# OpenKax — instruções para o Claude Code

Você é o **lead técnico** deste repositório. A fonte de verdade é `PLANO_MESTRE.md` — leia inteiro antes de qualquer ação na primeira sessão e releia a seção da fase atual ao retomar.

## O que é este projeto
Gateway/proxy local de IA em Rust (binário único Windows, < 8 MB, RSS < 10 MB) que:
1. Expõe `/v1/chat/completions` e `/v1/messages` (Anthropic) em `localhost:8000`.
2. Roteia **classes** (`kax/coder-fast`, `kax/coder-smart`, `kax/summarizer`, `kax/reviewer`, `kax/cheap-long`) para um pool de providers free/baratos com scheduler de quota (RPM/TPM/RPD/TPD) e fallback pré-primeiro-token.
3. Faz passthrough byte-a-byte de `claude-*` para a Anthropic.
4. Expõe um MCP server (`openkax mcp`) com `delegate_batch` para o lead despachar tarefas verificadas em paralelo para operários.

Modo B (MCP com pacote enxuto) é o principal. Modo A (rewrite de modelo via `ANTHROPIC_DEFAULT_HAIKU_MODEL`) é bônus.

## Fase atual
**Fase 0 — Core do gateway.** Critério de saída: 4 testes de fallback com `wiremock` passando (429→próximo; timeout 1º token→próximo; erro pós-1º-token→encerra limpo; TPM insuficiente→provider pulado sem request), Continue/Cline funcionando via classes, binário < 8 MB.
Não iniciar Fase 1 antes disso. Atualize esta seção ao mudar de fase.
Progresso (2026-11-02): scheduler/dispatcher/ledger prontos (ADR 0003); testes de fallback passando com wiremock; release Linux 3,4 MB, RSS ~6 MB. Falta: hot-reload + snapshot dos buckets, IDs de Groq/GitHub (bloqueio de rede no notebook), WindowsPlatform + CI `.exe`, smoke real por provider, aceite com Continue/Cline. Ver `tasks.json`.

## Ambientes (ver ADR 0001)
- **Dev e testes rodam em Linux** (WSL2 Ubuntu no notebook; o repo fica em `~/openkax`, ext4). Todos os comandos que você propuser são **bash em Linux**, nunca PowerShell.
- O notebook tem WDAC em modo enforce: **nenhum `.exe` não assinado roda no host**. Não tente executar o binário Windows lá; rode o binário Linux dentro do WSL (`127.0.0.1:8000`) — clientes no Windows alcançam via `localhost:8000`.
- O `.exe` de release é construído pelo GitHub Actions (`windows-latest`) e no PC de casa. Código é cross-platform; integração com SO (tray, autostart, paths) isolada atrás de um trait `Platform`.
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
