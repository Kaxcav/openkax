# Runbook do turno autônomo (nuvem)

> Fonte de verdade para as execuções agendadas que rodam sem o Kaxcav presente. Cada execução é uma sessão **nova, sem memória**: tudo o que ela precisa saber está aqui, no `PLANO_MESTRE.md`, no `CLAUDE.md`, no `tasks.json` e no `docs/noturno/LOG.md`.
> Criado em 2026-11-02 pelo chat de arquitetura. Janela: execuções de hora em hora até **2026-11-03 08:00 (America/Sao_Paulo)**.

## Papel
Você é o lead técnico do OpenKax trabalhando sozinho. Implemente com TDD, verifique pela máquina, revise com um agente independente e deixe rastro. Ninguém vai aprovar nada durante a noite: **na dúvida, pare e registre**.

## Limites invioláveis
1. **Nunca** faça merge em `main`, nunca force-push, nunca reescreva histórico publicado, nunca apague branch.
2. **Nunca** leia, peça, crie ou commite chave de API. Não há `.env` na nuvem e não deve haver. Nenhuma request a provider real — só `wiremock`/mocks.
3. Não reabra decisões dos ADRs 0001–0005 nem do `ESTADO_ATUAL`. Decisão nova de arquitetura → ADR com status **Proposto** e a tarefa vira `needs_user`.
4. Não altere `providers.toml` em rotas/limites/modelos de produção (a calibração vem de dado real, que só existe na máquina do Kaxcav). Pode **acrescentar** campos/seções novas exigidos pela fase, com default que não muda comportamento.
5. Uma tarefa por execução, no máximo ~40 min de trabalho. Commit pequeno, push, log.
6. `cargo test` e `cargo clippy --all-targets -- -D warnings` verdes antes de qualquer push. Sem `todo!()`, sem stub fora de CONTRACT.

## Portão e branch de trabalho (checar primeiro)
O merge em `main` é exclusivo do Kaxcav (o turno autônomo **nunca** faz merge; a tentativa de 2026-11-02 foi bloqueada pela checagem de segurança e isso é o esperado). Por isso as fases são **empilhadas**:

```bash
git fetch origin
git ls-remote --heads origin fase-2 fase-1
```
- Se `origin/fase-2` existe → trabalhe nela.
- Senão, se `origin/fase-1` existe → trabalhe nela.
- Senão → crie `fase-1` a partir de `origin/fase-0` se o PR #1 ainda não foi mergeado (`git merge-base --is-ancestor origin/fase-0 origin/main` falha), ou a partir de `origin/main` se já foi.
- Faça checkout da branch escolhida e `git merge --ff-only origin/<branch>`. Se o `main` tiver commits novos que a branch não tem, **não** faça merge do main por conta própria se der conflito: registre no LOG e siga.
- PRs em draft: `fase-1 → fase-0` enquanto o PR #1 estiver aberto (senão `fase-1 → main`); `fase-2 → fase-1`.

## Trava contra execuções sobrepostas
No início, depois do portão, leia a última entrada de `docs/noturno/LOG.md` na branch de trabalho. Se ela diz `EM ANDAMENTO` com início há menos de 75 min, **encerre sem fazer nada**. Senão, adicione uma entrada `EM ANDAMENTO <hora> — <tarefa>`, commit (`chore(noturno): inicia <tarefa>`) e push. Se o push for rejeitado (outra execução pegou), encerre.

## Ciclo por execução
1. Portão → trava.
2. Ler `PLANO_MESTRE.md` (§3.5 para a Fase 1, §3.6 para a Fase 2, §3.7 para o report, §4), `CLAUDE.md`, ADRs relevantes e `tasks.json`.
3. Escolher a primeira tarefa do backlog, na ordem F0-24 → F1-* → F2-*, com status `todo` cujas dependências estão `done`.
4. TDD: testes primeiro (devem falhar), implementação, `cargo test`, `cargo clippy -D warnings`.
5. **Revisão independente:** suba um subagente sem contexto desta conversa, entregue o diff (`git diff origin/fase-1...HEAD`) e o trecho do PLANO/ADR que ele deve honrar, e peça defeitos concretos. Corrija o que for confirmado; registre no LOG o que foi descartado e por quê.
6. Commit(s) `tipo(escopo): resumo`, push na branch de trabalho.
7. PR em **draft** (bases conforme o Portão) (crie se não existir via `gh api repos/Kaxcav/openkax/pulls -f head=<branch> -f base=<base> -F draft=true ...`; GraphQL não funciona, use REST). Atualize o checklist do corpo.
8. Confira o CI do push (`gh api "repos/Kaxcav/openkax/actions/runs?branch=<branch>&per_page=1"`). Se a execução acabar antes do CI, a próxima confere primeiro.
9. `tasks.json`: status da tarefa, nota com números. `LOG.md`: troque `EM ANDAMENTO` por `FEITO`/`BLOQUEADO` com resumo de 3–6 linhas (o que entrou, testes, CI, pendências).

## Parada
- CI vermelho: a próxima execução só corrige o CI. **Duas execuções seguidas sem conseguir deixar o CI verde → BLOQUEADO**, não comece tarefa nova; registre causa e hipótese.
- Ambiguidade de spec que muda contrato público → `needs_user`, pule para a próxima tarefa independente; se não houver, encerre.
- **Decisão do Kaxcav (2026-11-02 15:11): o turno roda sem aceite humano intermediário.** Ordem do backlog: F0-24 → F1-1..F1-8 → F2-1..F2-7 (F5-1/F5-2 são do Gemini, ver abaixo). Ao terminar a Fase 1 (F1-8 feito, CI verde), siga direto para a Fase 2 numa branch `fase-2` criada a partir da `fase-1` (empilhada; PR draft `fase-2 → fase-1`). Aceites manuais (clientes reais, providers reais) ficam documentados em `docs/smoke/` para depois — nunca são pré-requisito do turno.
- Tudo feito e CI verde → registre "backlog do turno concluído" no LOG e encerre as execuções seguintes sem trabalhar.

## Fechamento da Fase 0 (primeira tarefa após o portão)

| ID | Entrega | Aceite |
|---|---|---|
| F0-24 compat de clientes | Substitui o aceite manual Continue/Cline como critério bloqueante (decisão do Kaxcav 2026-11-02; o manual vira opcional). Testes de integração (wiremock) que reproduzem os formatos de request reais dos clientes, montados a partir do código-fonte público deles: **Continue** (chat stream, `stream_options`, mensagens com `system` + histórico, edit/apply) e **Cline** (OpenAI Compatible: system prompt grande > 6k tokens com `tools`/`tool_choice`, várias rodadas de `tool_calls` → mensagens `tool`, stream). Cada teste valida: classe certa, stream SSE bem formado e terminado em `[DONE]`, tool_calls com índices/argumentos reconstruídos, 413 para `kax/coder-fast` com o prompt do Cline, fallback pré-1º-token no meio de uma sessão multi-rodada, `/quota` e ledger coerentes. Registre a mudança de critério no ADR 0004 e no PLANO §4 Fase 0. Esta tarefa roda na branch `fase-1`. | suíte verde no CI (Linux e Windows) |

## Tarefas da Fase 1 (ponte Anthropic) — ver PLANO §3.5
Registre F0-24, F1-* e F2-* no `tasks.json` na primeira execução liberada (owner `lead`, status `todo`).

| ID | Depende | Entrega | Aceite |
|---|---|---|---|
| F1-1 CONTRACT | — | Tipos serde do Anthropic Messages (request, content blocks `text`/`image`/`tool_use`/`tool_result`/`thinking`, tools, tool_choice, response, eventos SSE `message_start`/`content_block_*`/`message_delta`/`message_stop`/`error`/`ping`); assinaturas `translate::anthropic::{to_openai_request, OpenAiToAnthropicStream}`; testes de aceite escritos e falhando; ADR 0006 (Proposto→Aceito ao fim da fase) | compila; testes vermelhos esperados |
| F1-2 request | F1-1 | Anthropic → OpenAI: `system` string/blocos (remove `cache_control`); text; image→`image_url` data URI ou descarte via quirk; `tool_use`→`assistant.tool_calls`; `tool_result`→mensagem `tool` (blocos concatenados, `is_error` preservado no texto); tools→functions; `tool_choice` auto/any/tool/none; `thinking` descartado; `max_tokens`; `stop_sequences`→`stop`; `metadata`/`top_k` descartados | testes de tabela cobrindo cada caso + fixtures reais de Claude Code (system grande + tools) |
| F1-3 stream | F1-1 | OpenAI SSE → eventos Anthropic: `message_start` (usage.input_tokens estimado); texto em `content_block_start/delta(text_delta)/stop`; tool_calls com índices múltiplos e `arguments` em pedaços → `tool_use` + `input_json_delta`; mapeamento `stop→end_turn`, `tool_calls→tool_use`, `length→max_tokens`, `content_filter→refusal`; `message_delta` com `usage.output_tokens`; `message_stop`; erro pós-1º-token → evento `error` + encerramento limpo; agregação não-stream para resposta Anthropic completa | testes de sequência de eventos byte a byte; texto + 2 tools intercalados |
| F1-4 endpoint | F1-2, F1-3 | `POST /v1/messages` para modelos `kax/*` e aliases: reusa scheduler/fallback pré-1º-token/ledger/headers `x-openkax-*`; stream e não-stream; erros no formato Anthropic (`{"type":"error","error":{"type","message"}}`) com status corretos (429 + `retry-after`, 413, 503) | integração wiremock: os mesmos cenários do `tests/fallback.rs` via `/v1/messages` (429→próximo, timeout 1º token→próximo, pós-1º-token→evento error, TPM insuficiente→pulado sem request) |
| F1-5 count_tokens | F1-1 | `POST /v1/messages/count_tokens` → `{"input_tokens": n}` pela mesma estimativa do scheduler (`chars_per_token` do 1º candidato da classe) | teste |
| F1-6 passthrough | F1-1 | modelos `claude-*` → provider `anthropic` (`native = "anthropic"`): proxy byte a byte de request e resposta/stream, **todos** os headers do cliente repassados (inclui `authorization`, `x-api-key`, `anthropic-version`, `anthropic-beta`), hop-by-hop removidos; `base_url` do TOML (permite wiremock); ledger só com metadados (status, latência, modelo) — **nunca** corpo nem auth; sem scheduler/quota | wiremock confere corpo idêntico, headers idênticos e stream repassado sem reempacotar; teste garante que o ledger não contém o corpo |
| F1-7 aliases (Modo A) | F1-4 | seção opcional `[aliases]` no TOML (`"claude-haiku-*" = "kax/summarizer"`, glob simples) aplicada antes do roteamento; sem alias, `claude-*` vai para passthrough; default do TOML **sem** aliases | testes: alias casa → classe; sem alias → passthrough |
| F1-8 docs | F1-4..F1-7 | ADR 0006 Aceito; README/CLAUDE.md com uso (`ANTHROPIC_BASE_URL=http://localhost:8000`, `ANTHROPIC_DEFAULT_HAIKU_MODEL=kax/summarizer`); `docs/smoke/fase-1-aceite.md` com o roteiro manual do PLANO §4 Fase 1 (Claude Code normal em Sonnet/Opus via passthrough; Explore em `kax/summarizer` com Read/Grep/Glob em ≥ 8/10) | revisão independente da doc contra o código |

## Tarefas da Fase 2 (MCP `kax-workers`) — ver PLANO §3.6 e §4 Fase 2
Mesmas regras (CONTRACT primeiro, TDD, sem provider real: operários respondem via wiremock com diffs fixos).

| ID | Depende | Entrega |
|---|---|---|
| F2-1 CONTRACT | F1-8 | subcomando `openkax mcp` (stdio, `rmcp`); schemas das tools `delegate`, `delegate_batch`, `status`, `quota`, `report` conforme §3.6; tipos `TaskPacket`, `TaskResult`; `.kax/verify.toml` (schema); ADR 0007 |
| F2-2 worktree | F2-1 | `git worktree add .kax/wt/<id> <base_ref>` por tarefa, limpeza (`openkax clean`), isolamento: nada roda fora do worktree; testes com repo git temporário |
| F2-3 pacote | F2-1 | montagem do pacote enxuto (system curto de operário + arquivos `read` + contratos + restrições + formato `unified_diff`), estimativa de tokens contra `max_packet_tokens` da classe |
| F2-4 aplicar+verify | F2-2, F2-3 | aplicar diff no worktree, rodar `accept.verify` com timeout do `verify.toml`, capturar log truncado (4k) |
| F2-5 retry | F2-4 | falhou → attempt 2 no **próximo** candidato da classe com stderr anexado; resultado `{id,status: accepted\|rejected\|needs_lead, provider, attempts, diff, verify_log, tokens, latency_ms}`; aceito vira branch `kax/<id>` |
| F2-6 batch | F2-5 | `delegate_batch` com `parallelism` (semáforo), `status(batch_id)`, `quota()`; ledger `kind:"task"`; medir memória privada numa rajada de 8 tarefas paralelas (mocks) e registrar |
| F2-7 docs | F2-6 | ADR 0007 Aceito; `docs/smoke/fase-2-aceite.md` (roteiro manual: Claude Code despacha 10 tarefas reais, ≥7 aceitas); exemplo de `.kax/verify.toml` |

## Fase 5 parcial — **frente do Gemini (Antigravity, PC de casa), NÃO do turno na nuvem**
Decisão de 2026-11-02 15:22: estas duas tarefas rodam em paralelo pelo agente Gemini do Antigravity, na branch `feat/doctor-report` (criada a partir da `fase-0`), porque tocam arquivos disjuntos (`src/doctor.rs`, `src/report.rs`). O turno na nuvem **não** as executa e **não** toca nessa branch. Ao terminar a F2-7, o turno encerra o backlog.
Única área de contato: o despacho de subcomandos em `src/main.rs` (`mcp` na F2-1; `doctor`/`report` no Gemini) — conflito pequeno, resolvido no merge pela manhã.

| ID | Entrega |
|---|---|
| F5-1 doctor | `openkax doctor` em Rust: lê `providers.toml` + env (só nomes), testa cada provider com `/models` e chat mínimo, imprime headers de limite; testes com wiremock. Substitui gradualmente `doctor.sh`/`doctor.ps1` |
| F5-2 report | `openkax report --week` a partir do ledger JSONL (§3.7): aceitação por classe/provider, latência p50/p95, tokens por classe, quedas em credit/paid; testes com ledger fixture |

## Formato do LOG.md
```
## 2026-11-02 23:00 — F1-2 request — FEITO
- entrou: ...
- testes: 120 unit + 23 integração, clippy limpo; CI run <id> verde
- revisão independente: 2 achados confirmados (corrigidos), 1 descartado (motivo)
- pendências: ...
```
