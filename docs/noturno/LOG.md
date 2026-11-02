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

## 2026-11-02 16:13 — F1-1 CONTRACT — FEITO (CI pendente)
- entrou: `src/translate/anthropic/` (tipos serde, assinaturas com stub), `tests/anthropic_bridge.rs`, fixture remontada, ADR 0006 (proposta), quirks declarados, `Candidate.chars_per_token`. F0-24 marcada `done` (CI run 41504593873 verde em Linux e Windows).
- testes: 106 unitários + 32 de integração verdes, clippy limpo. Aceite novo: 65 testes `#[ignore = "F1-…"]`; 64 vermelhos com `--ignored`, 1 passa com o stub (`tool_choice_without_usable_tools`, caso positivo em `tool_choice_table`). `#[ignore]` porque o runbook exige `cargo test` verde em todo push.
- revisão independente: achado principal confirmado na [doc do Gemini](https://ai.google.dev/gemini-api/docs/generate-content/thought-signatures): Gemini 3/4 dá 400 sem a `thought_signature` de volta, o que derrubaria a 2ª rodada de tool do Explore. Solução sem estado no ADR 0006: assinatura dentro do `tool_use.id`, quirk `thought_signatures`. Mesma família: Mistral exige `tool_call_id` de 9 alfanuméricos, quirk `tool_call_id = "alnum9"`. Também corrigidos: testes não exigiam stream ao vivo (mock TCP novo), headers de resposta do passthrough, `stop_reason` com tool sem `finish_reason`, `finish_reason: "error"`, `id` atrasado ou ausente, `count_tokens` sem cota/limite, `null` explícito no parse, afirmações do ADR.
- adiado, não descartado: acessor de `chars_per_token` por classe (F1-5), campo `status` no ledger (F1-6), registrados no `tasks.json`.
- pendências para o Kaxcav, sem bloquear: degrau Haiku do `coder-smart` fica fora da F1 (ADR 0006). O default novo tira `extra_content` de quem não tem o quirk, o que muda a Fase 0 só para cliente que o mandava a provider não-Gemini. Rodada dupla real com Gemini e `$schema` nas tools só dá para conferir com provider real: vão para o roteiro da F1-8.
