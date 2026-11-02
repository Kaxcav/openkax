# Etapa F2-6 — `delegate_batch`, `status`, `report`

Objetivo: paralelismo real com controle.

Entregáveis:
1. `delegate_batch`: cria `batch_id`, roda as tarefas com semáforo de `parallelism` (default 4, máx 16), cada uma no seu worktree; devolve imediatamente o `batch_id` e as tarefas em `queued` (não bloquear o stdio do MCP até acabar).
2. `status(batch_id)`: estado por tarefa (`queued|running|accepted|rejected|needs_lead`) e resultados prontos.
3. `report(period)`: agregado simples a partir do ledger (`kind:"task"`): aceitação por classe e por provider no período. (A versão completa do relatório é a F5-2; aqui basta o que o MCP precisa — reuse a F5-2 se ela já existir.)
4. Ligar os testes de aceite restantes da F2-1.
5. Teste de integração: lote de 8 tarefas com wiremock (mistura de aceitas e rejeitadas), `parallelism=4`, confirma que nunca passam de 4 simultâneas e que todos os resultados aparecem no `status`.
6. Medição: rode o lote de 8 com o binário release e registre memória privada antes/durante/depois em `docs/decisions/0002-*` (seção de memória) — só números, sem mudar o critério.
