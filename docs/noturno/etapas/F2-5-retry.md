# Etapa F2-5 — tentativa, retry no próximo candidato, resultado

Objetivo: fechar o ciclo de uma tarefa (`delegate`).

Entregáveis:
1. Execução de uma tarefa: worktree → pacote → `pick()` da classe pelo scheduler existente (reserve/settle/ledger normais) → chamada do operário (reuse o despacho do `/v1/chat/completions`, sem duplicar fallback) → parse → apply → verify.
2. Falhou (apply ou verify) → tentativa 2 no **próximo candidato da rota** (não no mesmo provider), com o stderr truncado anexado ao pacote. Respeitar `max_attempts` (default 2). Esgotou → `needs_lead`.
3. Aceita → commit no worktree (`kax(<id>): <objective>`) e a branch `kax/<id>` fica para o lead integrar. Rejeitada → worktree preservado para inspeção (o `clean` remove depois).
4. Ledger: linha `kind:"task"` conforme §3.7 (id, class, provider, attempts, status, verify_ms).
5. Tool MCP `delegate` liga de verdade; o teste de aceite correspondente da F2-1 sai do `#[ignore]`.
6. Testes de integração com wiremock (2 providers): 1ª tentativa devolve diff que quebra o teste do repo temporário → 2ª vai para o outro provider, que devolve diff bom → `accepted`, attempts=2, provider = o segundo; e o caso em que as duas falham → `needs_lead`.
