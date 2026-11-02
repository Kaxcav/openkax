# 0006 — Ponte Anthropic: tradução, stream e passthrough

Data: 2026-11-02
Status: proposta (F1-1; passa a aceita na F1-8, ao fim da fase)

## Contexto

A Fase 1 expõe `/v1/messages` para o Claude Code e outros clientes do protocolo Anthropic. Os providers habilitados nas rotas `kax/*` falam OpenAI Chat; `claude-*` tem de chegar à Anthropic como saiu do cliente (PLANO §3.5). O §3.5 dá o mapeamento campo a campo, mas deixa em aberto o que fazer quando os dois protocolos não têm a mesma forma.

Haiku como último degrau de `kax/coder-smart` (PLANO §1.1) fica **fora desta fase**: o runbook não tem tarefa para ele e proíbe mudar rotas. O scheduler segue marcando degrau nativo como `Unsupported`. Decisão pendente do Kaxcav, junto com a calibração das rotas.

## Decisão

### Tradução sem estado e sem I/O
`translate::anthropic` não conhece scheduler nem HTTP. O despacho de `/v1/messages` reusa a lógica de `/v1/chat/completions` (reserva, fallback pré-1º-token, ledger, `x-openkax-*`), com a forma de resposta e de erro decidida pelo protocolo de entrada (F1-4): traduz a request para `ChatCompletionRequest` na entrada e passa os chunks já normalizados pelo `Relay` (raciocínio removido, índices de `tool_calls` estáveis) para `OpenAiToAnthropicStream` na saída. Provider continua sendo dado: nada de Anthropic chega ao `provider`.

### Request
- O que a tradução descarta não entra nos tipos (`cache_control`, `metadata`, `top_k`, `thinking`, `betas`, `context_management`): o serde ignora. Bloco ou fonte de imagem desconhecidos viram `Unknown` e somem, em vez de derrubar a request.
- Blocos de `system` e de texto de usuário viram uma string, unidos por linha em branco. Partes (`image_url`) só quando há imagem.
- `tool_result` vira mensagem `tool` **antes** do texto da mesma mensagem de usuário, porque o OpenAI exige `tool` logo depois de `tool_calls`. Blocos de um resultado são unidos por `\n`. `is_error` vira o prefixo `[tool error] ` no texto.
- Imagem dentro de `tool_result` vai para uma mensagem `user` logo depois dos resultados, porque `tool` só leva texto. Se a mesma mensagem tem texto ou imagem do usuário, fica tudo nessa `user`, com a imagem da tool primeiro.
- Turnos `assistant` seguidos viram um só (a Anthropic também os funde): um `assistant` no meio separaria `tool_calls` dos resultados.
- Imagem para provider sem visão: a tradução sempre gera `image_url`; quem troca cada imagem por um aviso em texto é o quirk novo `strip_images` do provider, aplicado em `build_upstream_body` (default desligado). Trocar em vez de remover evita mensagem com `content: []` e diz ao modelo o que faltou.
- Tool com `type` próprio e sem `input_schema` (`web_search_…`, `bash_…`, `mcp_toolset`) não vira function. Sem nenhuma tool utilizável, `tool_choice` também sai: o OpenAI recusa `tool_choice` sem `tools`. O mesmo vale para `tool_choice` que nomeia uma tool descartada.
- `tool_choice`: `auto→"auto"`, `any→"required"`, `tool→{function}`, `none→"none"`; `disable_parallel_tool_use: true → parallel_tool_calls: false`. O quirk `auto_only` já existente trata quem só aceita `auto`.
- Mensagem `assistant` ou `user` que fica vazia depois dos descartes não é enviada.
- `null` explícito vale o mesmo que campo ausente no parse.

### Assinatura de raciocínio e `id` de tool (Gemini, Mistral)
A [doc do Gemini](https://ai.google.dev/gemini-api/docs/generate-content/thought-signatures) exige, em Gemini 3 e 4, que a `thought_signature` da 1ª chamada de um passo volte no histórico (via OpenAI: `tool_calls[].extra_content.google.thought_signature`); sem ela, a resposta é 400. Os dois primeiros degraus de `kax/summarizer` são Gemini, então sem isto a 2ª rodada de tool do Explore falha. O protocolo Anthropic não tem campo para a assinatura, e o gateway não guarda estado de conversa.
- A assinatura viaja **dentro do `tool_use.id`**: `<id>__kaxsig_<hex>`. O cliente trata o `id` como opaco e o devolve igual em `tool_use` e `tool_result`; `to_openai_request` separa os dois. Hexadecimal cabe no padrão de `tool_use.id` e não depende do alfabeto da assinatura. O sufixo fica só entre o gateway e o cliente. A assinatura segue ao upstream em `extra_content`, mas não é prompt: o scheduler não conta `extra_content` na estimativa, senão uma sessão longa daria `PacketTooLarge`.
- Quirk novo `thought_signatures` no provider. Ligado: mantém `extra_content` e, na 1ª chamada de cada mensagem sem assinatura (histórico de outro modelo, depois de um fallback), põe `skip_thought_signature_validator`, valor que a doc aceita para esse caso. Desligado, que é o default: `extra_content` não vai ao upstream, porque há provider que recusa campo extra. **Isso muda a Fase 0 só para quem mandava `extra_content` a um provider sem o quirk.**
- [Mistral recusa `tool_call_id` que não seja 9 letras ou dígitos](https://github.com/anomalyco/opencode/issues/1680) (`call_…`, `toolu_…`), e o Codestral está nas rotas: um fallback no meio da sessão daria 400. Quirk novo `tool_call_id = "alnum9"`: `id` que já cumpre passa igual, o resto vira hash determinístico, igual em `tool_calls` e `tool_call_id`.
- `providers.toml` liga `thought_signatures` no `gemini` e `alnum9` no `mistral` na F1-2. Não muda rota, limite nem modelo.

### Stream
- Blocos saem em sequência, **um aberto por vez**. A 1ª tool fecha o texto aberto. Tool aberta segue ao vivo até o fim do stream; `tool_calls` de outro índice, ou texto que chega com uma tool aberta, ficam retidos e saem em ordem de aparição quando ela fecha.
- Bloco de tool só abre com `id` e `name`; os argumentos esperam. Sem `id` até o fim, o gateway gera `toolu_<msg>_<n>`. Custo: a 2ª tool de uma resposta paralela só aparece no fim, o que não pesa, porque o cliente só executa tool com o JSON completo.
- `message_start.model` é o que o cliente pediu (classe ou alias). O modelo real vai no ledger e em `x-openkax-provider`.
- `message_start.usage.input_tokens` é estimado. Se o upstream informar `usage`, `message_delta.usage` traz também `input_tokens` para corrigir a estimativa. Sem `usage`, `output_tokens` é estimado por `chars_per_token`.
- `stop→end_turn`, `tool_calls→tool_use`, `length→max_tokens`, `content_filter→refusal`; outro valor ou ausência → `end_turn`. Com tool emitida, tudo que não é `length`/`content_filter` vira `tool_use`: há provider que manda `stop` ou nada, e o cliente precisa saber que deve executar.
- Chamada que chega ao fim sem `name` some, e não conta para `tool_use`: o cliente não teria o que executar. Pelo mesmo motivo, `finish_reason: "tool_calls"` sem nenhuma tool emitida vira `end_turn`.
- `usage` zerado vale como não informado: é upstream que não contou, não resposta de custo zero.
- Morte pós-1º-token, ou `finish_reason: "error"`: evento `error` (`api_error`) e fim. Não há `message_stop` depois dele.
- A resposta não-stream é a dobra dos mesmos eventos (`collect_response`), então os dois modos não divergem. Argumentos de tool que não são objeto JSON (truncados, `null`, lista) viram `input: {}`, com o `stop_reason` contando o motivo. No stream, `null`, lista ou número nem saem como delta, e `arguments` que vem como objeto vira a string dele.

### Erros HTTP
`{"type":"error","error":{"type","message"}}`: 400 `invalid_request_error`, 404 `not_found_error`, 413 `request_too_large`, 409 `invalid_request_error` (pin em modelo desabilitado), 429 `rate_limit_error` com `retry-after`, 502 `api_error`, 503 `overloaded_error`. Vale também para os erros de `x-openkax-*`.

### Passthrough
`claude-*` sem alias vai direto ao provider `native = "anthropic"`, em caminho próprio: sem scheduler, sem quota, sem cooldown (o limite é da conta do cliente). Só o campo `model` é lido; bytes de request e resposta seguem intactos, query string incluída (`?beta=true`), e todos os headers do cliente passam, exceto hop-by-hop. `count_tokens` de `claude-*` segue o mesmo caminho. Ledger só com metadados, com um campo opcional novo `status`; resposta não-2xx da Anthropic não fica registrada como `ok`.

### Aliases (Modo A)
Seção opcional `[aliases]` no TOML, com glob simples, aplicada antes do roteamento, inclusive em `count_tokens`: quem aliasou um modelo não quer o prompt indo à Anthropic. O default de produção vem sem nenhuma.

## Consequências

- O despacho passa a ter dois formatos de saída (F1-4 define o ponto de troca); a política de fallback continua em um lugar só.
- O scheduler estima sobre o `ChatCompletionRequest` traduzido. `count_tokens` usa a mesma fórmula sem `safety_factor` e não depende de cota, chave nem limite de pacote: é estimativa, não reserva.
- Blocos `document` e tools de `type` próprio somem em silêncio nas classes `kax/*`. Quem precisa deles usa `claude-*`.
- `tool_use.id` cresce com a assinatura (alguns KB), só entre o gateway e o cliente local. Se uma sessão com esses `id` for para `claude-*`, a Anthropic recebe o `id` longo como está.
- `$schema` e `additionalProperties` nas tools do Claude Code podem ser recusados pelo Gemini: não dá para saber sem provider real. Entra no roteiro manual (F1-8), e vira quirk se o smoke confirmar. A rodada dupla real com assinatura e a captura de uma request real do Claude Code, que a fixture hoje só remonta do formato público, também vão para esse roteiro.
- A F1-2 não confere com provider real: Codestral com `user` depois de `tool` ou com `assistant` no fim, Gemini com `parallel_tool_calls`, e quais providers precisam de `strip_images` (nenhum ligado). Vão para o roteiro da F1-8; viram quirk se o smoke confirmar.
- Sem `stop_sequence` real: o OpenAI não diz qual sequência parou a resposta, então `stop_reason` nunca é `stop_sequence` em `kax/*`.
- Testes de aceite entram no CONTRACT com `#[ignore = "F1-…"]`: o runbook exige `cargo test` verde em todo push. Cada tarefa tira o `ignore` dos seus; `cargo test -- --ignored` mostra o que falta.
