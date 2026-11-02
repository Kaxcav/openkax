# Etapa F2-1 — CONTRACT do MCP `kax-workers`

Objetivo: deixar compilando o contrato da Fase 2, com testes de aceite escritos e falhando de propósito (marcados `#[ignore = "F2-x"]` com a etapa que os liga), e o ADR 0007 em status **Proposto**.

Entregáveis:
1. Subcomando `openkax mcp` em `src/main.rs` iniciando um servidor MCP stdio com `rmcp` (já está no Cargo.toml com `server` e `transport-io`). Confira a API real do `rmcp 3.8` no código-fonte em `~/.cargo/registry` antes de usar — não invente macro nem trait.
2. Tools registradas com schema JSON, nomes e campos exatamente como o PLANO §3.6: `delegate` (uma tarefa), `delegate_batch` (`repo`, `base_ref`, `tasks[]`, `parallelism`), `status(batch_id)`, `quota()`, `report(period)`. Nesta etapa os handlers só validam a entrada e devolvem erro estruturado "não implementado na etapa F2-1"; `quota()` já pode devolver o snapshot real do scheduler (reuse o que o `/quota` do servidor usa).
3. Tipos serde: `TaskPacket` (id, class, objective, files[{path, mode: read|write, exists}], contracts[], constraints[], accept{verify[], expect}, output: unified_diff|full_file, max_attempts), `TaskResult` (id, status: accepted|rejected|needs_lead, provider, model, attempts, diff, verify_log, tokens{in,out}, latency_ms), `BatchStatus`.
4. Schema de `.kax/verify.toml` (struct + parser + `.kax/verify.toml.example` na raiz): comandos por linguagem/glob, `timeout_s`, `max_log_bytes` (default 4096).
5. Teste de integração `tests/mcp_contract.rs` que sobe o binário com `openkax mcp`, faz `initialize` + `tools/list` pelo stdio e confere os 5 nomes e os campos obrigatórios dos schemas.
6. `docs/decisions/0007-mcp-kax-workers.md` (Proposto): contexto, decisão (pacote enxuto, worktree por tarefa, verify determinístico, retry 1x no próximo candidato, resultado estruturado), consequências.
