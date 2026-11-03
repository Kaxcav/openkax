# Roteiro de Aceite Manual — Fase 1 (Ponte Anthropic)

Roteiro de verificação manual no PC de desenvolvimento (`kaxcavmaster`, Windows nativo) com comandos em PowerShell.
Este teste valida o gateway OpenKax operando com o Claude Code real via protocolo Anthropic (`/v1/messages`), cobrindo o passthrough direto (`claude-*`) e o Modo A (`kax/summarizer`).

---

## 1. Subir o Gateway

1. Compile o binário em modo release:
```powershell
cargo build --release
```

2. Suba o gateway utilizando o `.env` local existente (as chaves reais são carregadas em memória pelo processo e **nunca** devem ser impressas no console ou commitadas):
```powershell
.\target\release\openkax.exe
```

3. Em outro terminal, confirme a disponibilidade dos endpoints de status:
```powershell
curl.exe -s http://localhost:8000/v1/models
curl.exe -s http://localhost:8000/quota
```

---

## 2. Claude Code com Passthrough (Sonnet e Opus)

Configure o Claude Code para apontar para o gateway local:
```powershell
$env:ANTHROPIC_BASE_URL = "http://localhost:8000"
```

### Teste de Sessão Longa
1. **Sessão em Sonnet (`/model sonnet`)**:
   Inicie o Claude Code (`claude`) e faça uma requisição regular de código.
2. **Sessão em Opus (`/model opus`) com resposta longa (> 60 s)**:
   Inicie uma tarefa complexa de raciocínio profundo que gere saída contínua por mais de 60 segundos.
   - **Critério**: O stream deve fluir sem interrupção e sem corte de timeout (validando o client dedicado de passthrough com `read_timeout = 300s` e ausência de timeout total de 60 s).

### Conferência do Ledger
Inspecione o arquivo JSONL do dia em `$env:LOCALAPPDATA\OpenKax\ledger\` (ou no diretório configurado de ledger):
```powershell
Get-Content "$env:LOCALAPPDATA\OpenKax\ledger\*.jsonl" -Tail 5
```
- **Esperado**: Linhas registradas para `route: "claude-*"` contendo estritamente metadados:
  `ts`, `route`, `provider: "anthropic"`, `model`, `status`, `latency_ms` (tempo até os headers/1º byte) e `outcome`.
- **Validação de Segurança**: **Zero** corpo de request ou resposta, e **zero** vazamento de headers de autenticação (`x-api-key`, `authorization` ou tokens `sk-ant-*`).

---

## 3. Modo A: Subagente Explore via `kax/summarizer`

Só o `ANTHROPIC_DEFAULT_HAIKU_MODEL` não leva o Explore ao gateway: ele cobre as chamadas "haiku" e de fundo. Para forçar todos os subagentes (inclusive o Explore) em `kax/summarizer`, use as quatro variáveis:
```powershell
$env:ANTHROPIC_BASE_URL = "http://localhost:8000"
$env:ANTHROPIC_DEFAULT_HAIKU_MODEL = "kax/summarizer"
$env:CLAUDE_CODE_SUBAGENT_MODEL = "kax/summarizer"
$env:CLAUDE_CODE_SUBAGENT_MODEL_FORCE = "1"
claude
```
Com um Explore rodando, `/tasks` mostra o modelo do subagente: tem de ser `kax/summarizer`.
*(Alternativa equivalente: configurar `[aliases]` no topo do `providers.toml`: `"claude-haiku-*" = "kax/summarizer"`).*

### Execução de 10 Tarefas do Subagente Explore
Dispare 10 comandos que acionem o subagente Explore pedindo operações de leitura e busca (`Read`, `Grep`, `Glob`) no código deste repositório (ex.: *"Explore os módulos em src/ e liste onde Relay é utilizado"*).

O Claude Code só chama o Explore quando acha a busca grande; pergunta pontual ele responde sozinho (pelo passthrough). Para forçar: *"use o subagente Explore (thoroughness: medium) para mapear ..."*. Cada Explore faz ~25 chamadas em rajada, com 15k–50k tokens cada: a cota diária da Gemini free (250 por modelo) acaba em ~10 Explores.

Ledger em tabela (PowerShell normal, fora do Claude Code):
```powershell
Get-Content "$env:LOCALAPPDATA\OpenKax\ledger\*.jsonl" | Select-String '"route":"kax/summarizer"' | Select-Object -Last 120 | ForEach-Object { $j = $_.Line | ConvertFrom-Json; "{0}  {1,-11} {2,-18} in={3,-6} {4}" -f $j.ts.Substring(11,8), $j.provider, $j.model, $j.in, $j.outcome }
```
As linhas de um Explore têm `in` na casa dos milhares e aparecem em rajada.

- **Critério de Aceite**: $\ge 8 / 10$ execuções completas com tool calling e síntese bem-sucedidas.

### Tabela de Apuração

| # | Provider Acionado | Modelo Upstream | Status | Motivo / Observações |
|---|---|---|---|---|
| 1 | | | [ ] OK  [ ] Falha | |
| 2 | | | [ ] OK  [ ] Falha | |
| 3 | | | [ ] OK  [ ] Falha | |
| 4 | | | [ ] OK  [ ] Falha | |
| 5 | | | [ ] OK  [ ] Falha | |
| 6 | | | [ ] OK  [ ] Falha | |
| 7 | | | [ ] OK  [ ] Falha | |
| 8 | | | [ ] OK  [ ] Falha | |
| 9 | | | [ ] OK  [ ] Falha | |
| 10 | | | [ ] OK  [ ] Falha | |

---

### Execuções

**2026-11-03 (PC, Claude Code com as quatro variáveis):** 4 Explores concluídos em `kax/summarizer` (1 isolado + 3 em paralelo), distribuídos entre `gemini-flash-lite`, `codestral` e `cloudflare/llama-4-scout`. Dois `exhausted` (todos os degraus em cooldown ou sem cota no momento) foram absorvidos pelo retry do próprio Claude Code. A rota original (só Gemini + Cloudflare + OpenRouter) esgotava a Gemini em segundos; o codestral entrou como 2º degrau (PR #8). Cloudflare funcionou com até 47k de entrada (duas mortes no meio do stream na primeira execução foram transitórias). Modo A é bônus: aprovado.

## 4. Checklist de Hipóteses Não Testadas (F1-1..F1-4)

Itens mapeados durante o desenvolvimento autônomo que dependem de observação com providers e clientes reais:

- [ ] **Rodada dupla real com Gemini e thought signature (F1-1, F1-2)**: Gemini 3 e 4 exige a devolução da `thought_signature` na chamada subsequente de tool.
  *Como observar*: No Explore, confirmar se a 2ª rodada de tool com Gemini conclui com sucesso ou se retorna 400 por assinatura ausente/inválida.
- [ ] **`$schema` e `additionalProperties` nas tools do Claude Code (F1-1)**: Schemas JSON das tools do Claude Code contendo `$schema` ou `additionalProperties`.
  *Como observar*: Confirmar se o Gemini aceita as tools do Claude Code ou se recusa com erro 400 de validação de schema.
- [x] **Codestral: mensagem `user` imediatamente após `tool` (F1-2)**: Claude Code envia texto junto com `tool_result`, gerando `tool` seguida de `user`. **Verificado em 2026-11-03: aceita** (7 chamadas `ok` com 27k–49k de entrada no Explore).
  *Como observar*: Quando uma classe acionar Codestral (Mistral), checar se o upstream aceita a sequência ou rejeita com 400.
- [ ] **Codestral: prefill com `assistant` no fim (F1-2)**: Modelos Mistral/Codestral podem rejeitar mensagens terminadas em `assistant`.
  *Como observar*: Verificar se o degrau Codestral aceita a mensagem ou falha pré-1º-token disparando fallback.
- [ ] **Gemini: suporte a `parallel_tool_calls` (F1-2)**: Envio do parâmetro `parallel_tool_calls` para upstreams Gemini.
  *Como observar*: Confirmar se o provider processa normalmente ou retorna 400 (exigindo quirk específico).
- [ ] **Providers sem visão recebendo imagem sem `strip_images` (F1-2)**: Envio de imagens para degraus puramente textuais.
  *Como observar*: Testar envio de imagem pelo cliente e conferir se o aviso de texto substituto evita erro 400.
- [ ] **Gemini: chamadas paralelas com `id` vazio (F1-3)**: Emissão de tools paralelas pelo Gemini sem preencher `id`.
  *Como observar*: Conferir se o tradutor de stream (`src/translate/anthropic/stream.rs`) sintetiza IDs determinísticos (`toolu_<msg>_<n>`) e se o cliente executa ambas sem colisão. O `ToolIndexer` (`src/server.rs`) não cria ids: só corrige o `index` das tool calls e separa chamadas paralelas.
- [ ] **Upstream devolvendo `arguments` como objeto em vez de JSON string (F1-3)**: Provedores que enviam dicionário serializado em vez de string.
  *Como observar*: Validar se a tradução empacota o JSON em string sem corromper argumentos.
- [ ] **`id` de tool fora do regex da Anthropic (`^[a-zA-Z0-9_-]+$`) em migração para `claude-*` (F1-3)**: Sessão iniciada em `kax/*` (IDs com formato `functions.Read:0`) continuada em modelo `claude-*`.
  *Como observar*: Se a API da Anthropic rejeita o histórico com 400 `invalid_request_error`.
- [ ] **Auto-compact do Claude Code dependente do texto `'prompt is too long'` (F1-4)**: Detecção de estouro de contexto pelo Claude Code.
  *Como observar*: Ao estourar o limite de tokens, checar se o Claude Code aciona o auto-compact ou se exibe erro fatal.
- [ ] **Stream sem `ping` mudo durante raciocínio longo após o 1º token (F1-4)**: Período de inatividade prolongada do modelo gerando raciocínio.
  *Como observar*: Verificar se o cliente local encerra a conexão por timeout de inatividade enquanto aguarda o primeiro delta de conteúdo após raciocínio extenso.

---

## 5. Problemas Conhecidos (Fase 1)

Achados de baixa gravidade identificados na revisão da F1-4 e registrados no `docs/noturno/LOG.md`:

1. **Log de até 200 caracteres de `error.message` do upstream**:
   - *Descrição*: Em `Relay::finish_err` e nos avisos de falha pré-1º-token (`src/server.rs`), o gateway registra em nível de log até 200 caracteres da mensagem de erro do upstream.
   - *Impacto*: Embora o contrato de `RequestFault` prometa que corpos de requisição nunca chegam ao log, se um upstream replicar trechos do prompt na sua mensagem de erro, até 200 caracteres podem ser logados.
2. **`Reservation::fail` ignora `usage` já medido antes da falha**:
   - *Descrição*: Se uma requisição receber múltiplos chunks com `usage` parcial (ex.: 5001 tokens) e falhar posteriormente no meio do stream, o rollback da reserva no scheduler debita a estimativa inicial (`est_in`) em vez de reconciliar com os tokens de fato consumidos.
   - *Impacto*: Pequena distorção transitória nos contadores do bucket de TPM até o próximo reset de janela.
