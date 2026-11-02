# 0007 — MCP `kax-workers`: delegação verificada em worktrees

Data: 2026-11-02
Status: proposta

## Contexto

O lead (Claude) gasta tokens caros em trabalho mecânico e paralelizável
(testes, adapters, boilerplate). O gateway já roteia classes `kax/*` para um
pool de providers free/baratos com scheduler de quota (Fase 0). A Fase 2
expõe isso como MCP stdio (`openkax mcp`, `rmcp`) para o lead despachar lotes
de tarefas a operários. Saída de operário é código de desconhecido: precisa
ser isolada, verificada por máquina e devolvida de forma estruturada.

## Decisão

1. **Pacote enxuto.** O operário recebe só: objetivo em 1 frase, arquivos
   `read` (conteúdo), caminhos `write`, contratos (tipos/assinaturas),
   restrições, critério de aceite e formato de saída. Sem histórico de
   conversa nem contexto do projeto além disso. O tamanho do pacote
   participa da escolha do provider (`max_packet_tokens` da classe).
2. **Worktree por tarefa.** Cada tarefa roda em `git worktree add
   .kax/wt/<id> <base_ref>`; nada é lido ou executado fora dele. Aceita →
   branch `kax/<id>` para o lead integrar; rejeitada fica para inspeção ou é
   removida por `openkax clean`.
3. **Verify determinístico.** O diff do operário é aplicado no worktree e
   `accept.verify` roda os comandos de `.kax/verify.toml` (por
   linguagem/glob, `timeout_s`, `max_log_bytes` = 4096 por padrão, preservando
   o fim do log). Quem decide aceite é o exit code, nunca o modelo.
4. **Retry 1x no próximo candidato.** Falhou → 2ª tentativa no **próximo**
   provider da rota, com o stderr truncado anexado (`max_attempts` padrão 2,
   teto 3). Esgotou → `needs_lead` e o lead faz.
5. **Resultado estruturado.** `TaskResult {id, status: accepted|rejected|
   needs_lead, provider, model, attempts, diff, verify_log, tokens{in,out},
   latency_ms}`; erros de tool são JSON `{error:{code, stage, message}}` com
   `is_error = true`. Cada tarefa grava `kind:"task"` no ledger (§3.7).
6. **Tools:** `delegate`, `delegate_batch`, `status`, `quota`, `report`
   (PLANO §3.6). `quota` reaproveita o snapshot do scheduler. Em stdio, o
   stdout é exclusivo do protocolo: logs vão para stderr.

Escopo da etapa F2-1 (CONTRACT): tipos serde, schemas das tools, parser de
`verify.toml`, `openkax mcp` subindo e listando tools; handlers validam e
respondem `not_implemented`. Execução entra em F2-2 em diante.

## Consequências

- O lead ganha paralelismo barato, mas toda saída passa por verify antes de
  chegar a ele; custo extra: um worktree e uma rodada de build por tarefa.
- Segurança depende do isolamento no worktree e de paths relativos sem `..`
  (validados já no contrato); comandos de `verify.toml` são do repo do
  usuário e rodam com os privilégios dele.
- Tarefas de arquitetura, auth/cripto e specs ambíguas continuam com o lead
  (política do CLAUDE.md).
- Pendente (precisa de decisão do usuário antes de F2-3): onde persistir o
  estado dos lotes para `status` sobreviver a restart do processo MCP.
