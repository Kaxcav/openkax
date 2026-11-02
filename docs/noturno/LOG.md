# Log do turno autônomo

Entradas mais recentes no fim. Formato e regras em `RUNBOOK.md`.

## 2026-11-02 15:20 — preparação (chat de arquitetura) — FEITO
- Conflito do PR #1 resolvido: merge de `main` na `fase-0` (`35e0484`); `providers-setup.md` alinhado ao ADR 0004.
- Runbook criado. Execuções agendadas de hora em hora até 2026-11-03 08:00.
- Portão: Fase 1 só começa depois que o Kaxcav fizer o aceite Continue/Cline e o merge do PR #1.

## 2026-11-02 15:23 — F0-24 compat de clientes — FEITO (CI pendente)
- entrou: `tests/client_compat.rs` (12 testes de Continue e Cline), roteiro opcional `docs/smoke/fase-0-clientes.md`, ADR 0004 e PLANO §4 com o critério novo, F0-24/F1/F2 no `tasks.json`, PR draft #2 `fase-1 → fase-0`.
- defeitos que a suíte achou, corrigidos: `usage` em chunk não final ia ao cliente; `content: null` sumia; chamadas paralelas com `index` repetido ou ausente se fundiam; erro depois do `finish_reason` virava 2º final e cooldown; `stream_options: null` ficava sem `include_usage`.
- testes: 99 unitários + 31 de integração verdes no Linux, clippy e fmt limpos. **CI do commit em andamento ao fechar a entrada; a próxima execução confere primeiro e marca F0-24 `done`.** Windows só pelo CI.
- limite: o turno não abriu o `openai.ts` do Continue nem do Cline (`raw.githubusercontent.com` pede aprovação que ninguém responde à noite; o blob do GitHub falhou). Fontes usadas: PR continuedev/continue#12992, issues continuedev/continue#13417 e cline/cline#8912, docs via Context7. `parallel_tool_calls`, `temperature: 0`, partes de texto e `prediction` vêm de memória, e o cabeçalho do teste diz isso. Mirrors não foram usados.
- revisão independente: lacunas de teste confirmadas por mutação e fechadas (enquadramento SSE, `settle` sem `usage`, morte sem cooldown, tools/tool_calls no tamanho do pacote, rota por classe), defeitos acima corrigidos.
- descartado por ora, sem efeito no cliente: ledger do 413 com `est_in: 0` e `prompt_tokens_details` fora do `usage` repassado (anteriores a este turno).
- pendências: `add_repo` e `register_repo_root` não existem nesta sessão; o repo já vinha clonado e com acesso de push.
- escopo: a branch `Kaxcav/openkax-fase-2` e o `ESTADO_ATUAL` passam F2/F5 para a fábrica, mas o runbook e o prompt agendado ainda mandavam o turno seguir para a F2. Runbook e `tasks.json` alinhados na `fase-1`; o prompt agendado só o Kaxcav pode ajustar. Se ele quiser outra divisão, é só reverter este commit.

## 2026-11-02 16:13 — F1-1 CONTRACT — EM ANDAMENTO
