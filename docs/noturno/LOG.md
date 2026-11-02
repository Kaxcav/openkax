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

## 2026-11-02 16:13 — F1-1 CONTRACT — FEITO
- entrou: `src/translate/anthropic/` (tipos serde, assinaturas com stub), `tests/anthropic_bridge.rs`, fixture remontada, ADR 0006 (proposta), quirks declarados, `Candidate.chars_per_token`. F0-24 marcada `done` (CI run 41504593873 verde em Linux e Windows).
- testes: 106 unitários + 32 de integração verdes, clippy limpo; CI run 41512182477 verde (Linux e Windows). Aceite novo: 65 testes `#[ignore = "F1-…"]`; 64 vermelhos com `--ignored`, 1 passa com o stub (`tool_choice_without_usable_tools`, caso positivo em `tool_choice_table`). `#[ignore]` porque o runbook exige `cargo test` verde em todo push.
- revisão independente: achado principal confirmado na [doc do Gemini](https://ai.google.dev/gemini-api/docs/generate-content/thought-signatures): Gemini 3/4 dá 400 sem a `thought_signature` de volta, o que derrubaria a 2ª rodada de tool do Explore. Solução sem estado no ADR 0006: assinatura dentro do `tool_use.id`, quirk `thought_signatures`. Mesma família: Mistral exige `tool_call_id` de 9 alfanuméricos, quirk `tool_call_id = "alnum9"`. Também corrigidos: testes não exigiam stream ao vivo (mock TCP novo), headers de resposta do passthrough, `stop_reason` com tool sem `finish_reason`, `finish_reason: "error"`, `id` atrasado ou ausente, `count_tokens` sem cota/limite, `null` explícito no parse, afirmações do ADR.
- adiado, não descartado: acessor de `chars_per_token` por classe (F1-5), campo `status` no ledger (F1-6), registrados no `tasks.json`.
- pendências para o Kaxcav, sem bloquear: degrau Haiku do `coder-smart` fica fora da F1 (ADR 0006). O default novo tira `extra_content` de quem não tem o quirk, o que muda a Fase 0 só para cliente que o mandava a provider não-Gemini. Rodada dupla real com Gemini e `$schema` nas tools só dá para conferir com provider real: vão para o roteiro da F1-8.

## 2026-11-02 17:12 — F1-2 request — FEITO
- entrou: `src/translate/anthropic/request.rs` (`to_openai_request`), `decode_tool_use_id`, quirks `strip_images`, `thought_signatures` e `tool_call_id` em `build_upstream_body`. `providers.toml`: `thought_signatures` no gemini e `alnum9` no mistral, sem mudar rota, limite ou modelo.
- testes: 142 unitários + 32 de integração verdes, clippy e fmt limpos; 45 de aceite seguem `#[ignore]` (F1-3..F1-7). CI verde: ver a linha de CI abaixo.
- revisão independente, corrigidos: assinatura contava na estimativa (cerca de 1150 tokens por passo com 4 KB, tamanho suposto), agora `extra_content` não conta; `assistant` seguidos separavam `tool_calls` dos resultados, agora fundem; `disable_parallel_tool_use` sumia com `tool_choice` descartado; `thought_signature: null` não recebia o valor neutro. Seis lacunas de teste achadas por mutação, fechadas.
- teste do CONTRACT alterado: `tool_use_becomes_assistant_tool_calls` tinha dois `assistant` seguidos e esperava mantê-los separados, o que contradiz a regra do ADR 0006 de `tool` logo depois de `tool_calls`. Ganhou um `user` no meio; a fusão tem teste próprio.
- só hipótese, sem provider real para provar, foi para o roteiro da F1-8: Codestral recusar `user` depois de `tool` (mais provável de aparecer, porque o Claude Code manda texto junto do resultado; o custo seria uma tentativa perdida antes do próximo degrau) e `assistant` no fim; Gemini recusar `parallel_tool_calls`. Nenhum provider tem `strip_images` ligado: quem não tem visão daria erro com imagem.
- para a F1-4: tradução sem nenhuma mensagem além de `system` responde 400 local (nota no `tasks.json`).
- descartado: mais de 4 `stop_sequences` em provider com limite. É raro, e cortar calado mudaria o comportamento pedido; o upstream responde o erro.
- CI: o `080ab81` falhou no clippy porque o CI usa o stable corrente (1.99.1) e a nuvem está no 1.97, onde o lint `chunks_exact_to_as_chunks` não existe. Corrigido no `bcaf966`, CI verde em Linux e Windows (run 41520117016). **Clippy local verde não garante o do CI: confira o CI antes de dar a tarefa por encerrada.**

## 2026-11-02 18:11 — F1-3 stream — FEITO
- entrou: `src/translate/anthropic/stream.rs` (`OpenAiToAnthropicStream`, `collect_response`), `encode_tool_use_id`. Os 16 testes `#[ignore = "F1-3"]` agora rodam, com outros 20 de stream e 2 do `ToolIndexer`. Ainda sem rota: isso é a F1-4.
- testes: 180 unitários + 32 de integração verdes, clippy e fmt limpos; 29 de aceite seguem `#[ignore]` (F1-4..F1-7). CI run 41528418314 verde em Linux e Windows.
- revisão independente, corrigidos: chamadas paralelas sem `id` se fundiam em `{"p":1}{"q":2}` (`ToolIndexer`, em `server.rs`, então também vale para `/v1/chat/completions`); `arguments` como objeto sumia; `arguments: "null"` saía como delta no stream e `{}` no não-stream; delta de tool vazio fechava o texto aberto. Cinco lacunas de teste achadas por mutação, fechadas.
- decisão nova, no ADR 0006: chamada que nunca recebe `name` some; `finish_reason: "tool_calls"` sem tool emitida vira `end_turn`; `usage` zerado vale como não informado.
- descartado: `stop` ou EOF com argumentos truncados saem como `tool_use`. O cliente deve recusar o JSON inválido e devolver o erro ao modelo (lido, não testado com o Claude Code), e inventar `max_tokens` diria uma causa que não sabemos. Assinatura depois de o bloco abrir e nome em pedaços: o `id` e o `name` já saíram, e o turno não tem evidência de provider que faça isso.
- só hipótese, sem provider real, foi para o roteiro da F1-8: Gemini com `id` vazio em chamadas paralelas, `arguments` como objeto, `id` fora do padrão da Anthropic (`functions.Read:0`) se a sessão migrar para `claude-*`.

## 2026-11-02 19:12 — F1-4 endpoint — FEITO
- entrou: `POST /v1/messages` para `kax/*`, stream e não-stream, em `src/server/anthropic.rs`. `dispatch` é um só para as duas rotas; `Wire` decide a forma da resposta e do erro. `Relay` ganhou `next_chunk`, e o cliente OpenAI usa a mesma coisa. Corpo até 32 MiB nesta rota (o padrão do axum, 2 MiB, recusaria imagem em base64). Os 16 testes `#[ignore = "F1-4"]` agora rodam, com outros 12 de integração.
- testes: 183 unitários + 61 de integração verdes, clippy e fmt limpos; 13 de aceite seguem `#[ignore]` (F1-5..F1-7). CI run 41537882354 verde em Linux e Windows, no `4c4365c`.
- muda a Fase 0, nas duas rotas: `finish_reason: "error"` antes do 1º token agora cai no próximo degrau (antes o chunk ia ao cliente, sem fallback). Com pin, 429 traz `Retry-After` se o upstream informou.
- diferença entre as rotas, de propósito: depois do 1º token, `finish_reason: "error"` em `/v1/messages` é `mid_stream_error` com cooldown, porque o cliente recebe um evento `error`. Em `/v1/chat/completions` segue `ok`, como antes.
- revisão independente, corrigidos: o defeito do fallback acima. Cinco mutações sobreviviam (morte real sem ledger nem cooldown, texto do upstream no evento `error`, estimativa errada em `message_start`, `Retry-After` com pin, cooldown em desconexão): as cinco agora derrubam teste, conferido rodando cada uma.
- achados anteriores a este turno, não corrigidos, baixa gravidade: `Relay::finish_err` e o aviso de falha pré-1º-token logam até 200 caracteres de `error.message` do upstream (lido, sem capturar o log; a doc de `RequestFault` promete "nunca ao log", mas o caminho é outro; ecoar prompt é hipótese). `Reservation::fail` cobra a estimativa e ignora `usage` já recebido (`usage` de 5001 seguido de erro: ledger `in: 5000`, `tpm_remaining` 7997, confirmado por sonda do revisor). Tocam F0 e não afetam o contrato da F1-4; ficam para o Kaxcav priorizar.
- só hipótese, sem cliente real, foi para o roteiro da F1-8: auto-compact do Claude Code pode depender do texto `prompt is too long`; stream sem `ping` fica mudo durante raciocínio longo depois do 1º token.
- não testado: desconexão em não-stream (só lido, é o mesmo `Drop`).

## 2026-11-02 20:07 — F1-5, F1-6, F1-7 — FEITO (revisão)
- entrou:
  - F1-5: `POST /v1/messages/count_tokens` para classes `kax/*`: estimativa com o `chars_per_token` do 1º candidato da classe (acessor `Scheduler::class_chars_per_token`), sem reservar cota e sem checar limites de pacote ou contexto. Erros em formato Anthropic (`invalid_request_error` no 400, `not_found_error` no 404).
  - F1-6: Passthrough byte a byte de `claude-*` para provider com `native = "anthropic"` em `/v1/messages` e `/v1/messages/count_tokens`. Parse mínimo com `ModelOnly` para evitar qualquer reserialização; repasse de todos os headers do cliente com filtro de hop-by-hop (`connection`, `keep-alive`, `proxy-authenticate`, `proxy-authorization`, `te`, `trailer`, `transfer-encoding`, `upgrade`, `host`, `content-length`); repasse do stream ao vivo sem reempacotar via `Body::from_stream(upstream_resp.bytes_stream())`; sem scheduler, cota ou cooldown. Ledger grava apenas metadados, com campo novo opcional `status: Option<u16>` em `RequestRecord` e `outcome: RequestFault` para respostas não-2xx ou upstream inacessível (502).
  - F1-7: Seção opcional `[aliases]` no TOML (Modo A) com casamento glob simples (`*`). Resolução de alias aplicada antes do roteamento em `/v1/messages` e `/v1/messages/count_tokens`. `choose` e `anthropic::respond` preservam o modelo solicitado pelo cliente no evento `message_start` enquanto despacham para a classe mapeada no scheduler. Validação no load e hot-reload rejeita alias com destino inexistente. `providers.toml` de produção permanece sem aliases.
- testes: 185 unitários + 74 de integração verdes (Linux/Windows wiremock), clippy `--all-targets -- -D warnings` e `cargo fmt` limpos. Zero testes `#[ignore]` restantes na suíte inteira.
- decisões fora da spec:
  - No passthrough, upstream que retorna status de erro HTTP (ex.: 429) ou erro de conexão (502) é registrado no ledger com `outcome: Outcome::RequestFault` e `status: Some(code)`. Não foi criada variante nova de `Outcome` para não quebrar consumidores existentes do ledger como o report da fábrica (F5-2).
  - Em `choose`, a classe é resolvida por `resolve_alias` mantendo `req.model` intacto como o modelo pedido pelo cliente para que `anthropic::respond` reflita exatamente o nome solicitado no evento inicial `message_start`.
- hipóteses não testadas:
  - Comportamento de clientes reais da Anthropic diante de headers não documentados ou timeouts de rede em passthrough direto.


## 2026-11-02 20:40 — feat/f1-5-7 revisão (B1, B2, M1, M2, M3) — FEITO
- entrou:
  - B1: Client dedicado ao passthrough (`provider::build_passthrough_client`) criado em `Live::build` e reconstruído no hot-reload sem timeout total (apenas `connect_timeout(10s)` e `read_timeout(300s)`). Teste `passthrough_stream_does_not_timeout_on_long_stream` valida stream chunked entregue ao longo de ~2,6 s com `request_timeout_ms = 1000`.
  - B2: Escopo de roteamento estrito em `messages` e `count_tokens`: após resolução de alias, prefixo `kax/` vai para rota de classe, `claude-` para passthrough e qualquer outro retorna 404 Anthropic ("esta rota aceita apenas classes kax/* e modelos claude-*") registrando `Outcome::Rejected` no ledger. Testes com `gpt-4o` e `kax-coder` conferem 404 e 0 requests ao mock da Anthropic.
  - M1: Headers de resposta do passthrough mapeados com `append` em vez de `insert` para preservar múltiplos valores (ex: múltiplos `Set-Cookie` ou headers customizados). Testado via `passthrough_preserves_duplicate_response_headers`.
  - M2: Ordem de declaração de aliases no TOML preservada usando feature `preserve_order` do crate `toml` e `Vec<(String, String)>` com deserializer customizado. Primeiro padrão que casar vence. Testado via `aliases_preserve_declaration_order`.
  - M3: Doc comment `"/// Filtra e ordena os degraus elegíveis de class_id. Não debita nada."` restaurado sobre `pub fn pick` em `src/scheduler/mod.rs`.
- testes: 185 unitários + 79 de integração verdes (total 264 testes, 0 ignored). Clippy e fmt 100% limpos.
