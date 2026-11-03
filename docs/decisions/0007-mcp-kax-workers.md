# 0007 — MCP `kax-workers`: delegação verificada em worktrees

Data: 2026-11-02
Status: Aceito (2026-11-02; aceite manual do Kaxcav pendente, ver `docs/smoke/fase-2-aceite.md`)

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

## Pacote do operário (F2-3)

- **Idioma:** o system prompt fixo é em **PT-BR** (`SYSTEM_PROMPT`, versão 1 em
  `SYSTEM_PROMPT_VERSION`, em `src/mcp/packet.rs`); mudar o texto exige subir a versão.
- **Montagem:** `build_request` lê os arquivos do worktree e monta, na ordem:
  objetivo, arquivos `read` (com path), arquivos `write` (conteúdo atual ou
  "novo arquivo"), contratos, restrições, critério de aceite, `CONVENTIONS.md`
  da raiz (truncado em 4000 caracteres, com aviso) e o formato de saída exigido.
  Arquivos fora do worktree (symlink) e não-UTF-8 são recusados; CRLF vira LF.
- **Tamanho:** a estimativa usa `scheduler::estimate_tokens`; acima de
  `max_packet_tokens` da classe devolve `packet_too_large` sem chamar provider.
- **Parser:** aceita diff em bloco ```` ```diff ````, `diff --git` ou diff cru; ou
  arquivos completos em `=== FILE: <path> === … === END FILE ===`. Recusa a
  resposta inteira se qualquer path for absoluto, tiver `..` ou não estiver na
  lista `write` (`out_of_scope`). Barras invertidas são normalizadas antes. O
  diff é lido como o `git apply` o lê (máquina de estados, hunks contados pelo
  `@@`) e cabeçalhos que renomeiam, copiam ou mudam modo são recusados; ver
  "Modelo de ameaça".

## Execução e retry (F2-5)

- **Despacho único:** `run_task` chama `AppState::complete`, que usa o mesmo
  `dispatch` do `/v1/chat/completions` (pick, reserva, settle, ledger de request);
  o retry só acrescenta a lista de providers a pular. Não há fallback duplicado.
- **Tentativas:** falha de parse, de `git apply` ou de verify → `git reset --hard`
  + `clean` no worktree e nova tentativa no próximo candidato, com o log truncado
  anexado ao pacote. Esgotou, ou gateway sem candidato/pacote grande → `needs_lead`
  (o status `rejected` não é produzido; worktree sempre preservado).
- **Aceita:** commit `kax(<id>): <objective>` no worktree (identidade `openkax`),
  branch `kax/<id>` para o lead integrar. Ledger `kind:"task"` por tarefa.
- **Lote (F2-6):** `delegate_batch` registra o lote, devolve na hora `batch_id` com
  todas as tarefas em `queued` e roda em background com semáforo de `parallelism`
  (padrão 4, máximo 16). `status(batch_id)` mostra o estado por tarefa
  (`queued|running|accepted|needs_lead`; `rejected` segue sem uso) e `peak_running`.
  O registro é só em memória (32 lotes; o ledger é o histórico durável); lote
  desconhecido → erro `not_found`. `report(period)` (day|week|month) agrega as
  linhas `kind:"task"` do ledger por classe e por provider.

## Modelo de ameaça

Acrescentado em 2026-11-02, depois da revisão do PR #3 (achados C1 e C2).

**Não confiável:** a resposta do operário (modelo free de terceiros) e tudo o que
sai dela: o diff, os arquivos e o código que o verify compila e executa (build
scripts, macros, testes). O verify **executa código não confiável** com os
privilégios do usuário, **com rede** e com **leitura e escrita do disco** dele.
O worktree isola o diff, não esse código.

**C1 — chaves de API no ambiente (corrigido, F2-8).** O `main` carrega o `.env`
no ambiente do processo. Agora os comandos de verify, o `git apply` e todo `git`
do `worktree.rs` rodam com `env_clear()` e herdam só a allowlist fixa
`ENV_ALLOWLIST` (`src/verify.rs`): `PATH`, `HOME`, `USERPROFILE`, `CARGO_HOME`,
`RUSTUP_HOME`, `RUSTUP_TOOLCHAIN`, `TEMP`, `TMP`, `TMPDIR`, `SystemRoot`,
`SystemDrive`, `windir`, `ComSpec`, `PATHEXT`, `LANG`, `LC_ALL`, `APPDATA`,
`LOCALAPPDATA`, `PROGRAMDATA`, `NUMBER_OF_PROCESSORS`, `PROCESSOR_ARCHITECTURE`,
`RUST_BACKTRACE`, `GIT_EXEC_PATH`. O `git` do worktree entra porque hooks e
config do worktree podem ter sido plantados pelo código do verify. Para o que
um verify precisar a mais, `.kax/verify.toml` aceita `env_passthrough = [...]`
(só para os comandos de verify, nunca para o `git`). Nome terminado em `_KEY`,
`_TOKEN` ou `_SECRET`, ou igual (sem diferenciar caixa) a um `auth.env` ou
`${VAR}` do `providers.toml` (`OPENKAX_CONFIG`), é erro de config e a tarefa vai
para `needs_lead`. `cargo build` com link MSVC foi conferido no Windows só com a
allowlist.

**C2 — diff fora da lista `write` (corrigido, F2-9).** Duas camadas:

1. *Parser* (`src/mcp/packet.rs`): máquina de estados com a gramática do
   `git apply`. Entre `diff --git` e o primeiro `---`/`@@` só passam `index`
   (modo opcional 100644/100755) e `new file mode`/`deleted file mode` 100644 ou
   100755. `rename*`, `copy*`, `similarity`, `dissimilarity`, `old mode`,
   `new mode`, modos 120000/160000, `GIT binary patch` e `Binary files` são
   recusados. `diff --git` exige `a/<p> b/<p>` com o mesmo path; `---`/`+++`
   exigem o prefixo `a/` ou `b/` (o `-p1` cortaria o 1º diretório) e o mesmo
   path do bloco. O fim do hunk vem da contagem do `@@`: `--- `/`+++ ` dentro
   de hunk é conteúdo.
2. *Guarda pós-apply* (`verify::check_staged`), a que vale: `git diff --cached
   --no-renames --raw -z <base>` no worktree, com o ambiente limpo, contra o
   commit base da tarefa. Todo path tem de estar na lista `write` e todo modo
   novo tem de ser 100644, 100755 ou 000000 (deleção). Roda depois do apply (ou
   da escrita dos arquivos completos) e **de novo antes do commit**, para pegar
   o que o próprio verify criou ou commitou. Fora do escopo → `worktree::reset`
   e retry no próximo candidato; se for depois do verify → `needs_lead`.

No modo `full_file`, `write_files` recusa escrever se o path ou um diretório pai
dentro do worktree for symlink (link vindo do `base_ref`).

**O que continua aberto.** O `env_clear` fecha só o vazamento das chaves pelo
ambiente. Até existir sandbox, o código do verify ainda pode:

- ler qualquer arquivo do usuário: o próprio `.env` (em geral na raiz do repo,
  dois níveis acima de `.kax/wt/<id>`), o `~/.claude.json` (onde
  `claude mcp add -e` grava variáveis), `~/.ssh`, credenciais de CLIs;
- usar a rede para mandar isso para fora;
- escrever fora do worktree (repo principal, `.git/hooks`, `.git/config`, outras
  branches) e deixar processo rodando em segundo plano;
- commitar no worktree: a guarda antes do commit compara contra o commit base e
  manda a tarefa para `needs_lead`, mas não reescreve o histórico que o verify
  criou na branch `kax/<id>`.

**Atualização (2026-11-03): no Linux isto passou a ser coberto pelo ADR 0008**
(bwrap, fail closed, prefetch fora da caixa); o Windows nativo segue sem sandbox.

**Decisão adiada para a Fase 5: sandbox do verify.** Linux: `bwrap` ou
`unshare -n` (sem rede, só o worktree e o toolchain montados, home vazio).
Windows: Job Object (mata a árvore ao fechar, limites de CPU/memória) + token
restrito ou AppContainer (sem rede, sem acesso ao perfil do usuário).

**Até lá:** rodar o `openkax mcp` sem segredos além do necessário. Só as chaves
dos providers que atendem as classes `kax/*` dos operários, de preferência
dedicadas, free e revogáveis; nada de outras credenciais no `.env` que o MCP
carrega nem em arquivo dentro do repo delegado. O lead revisa o diff de
`kax/<id>` contra o base antes de integrar.

## Consequências

- O lead ganha paralelismo barato, mas toda saída passa por verify antes de
  chegar a ele; custo extra: um worktree e uma rodada de build por tarefa.
- Segurança: ver "Modelo de ameaça". O worktree e as duas camadas do C2 limitam
  o que o diff do operário altera; o código que o verify executa roda com os
  privilégios do usuário, sem as chaves no ambiente, até o sandbox da Fase 5.
- Tarefas de arquitetura, auth/cripto e specs ambíguas continuam com o lead
  (política do CLAUDE.md).
- Estado dos lotes só em memória: reiniciar `openkax mcp` perde `status(batch_id)`.
  O histórico durável é o ledger (`kind:"task"`) + as branches `kax/<id>`.

## O que mudou durante a implementação

- **Status `rejected` não é produzido.** Qualquer recusa (parse, `git apply`, verify,
  esgotar tentativas) vira `needs_lead`; o valor segue no schema por compatibilidade.
- **`verify.toml` fornece só `timeout_s`, `max_log_bytes` e `env_passthrough`.** Os
  comandos rodados são sempre os de `accept.verify` do pacote; a seleção por glob
  (`[[language]]`) é validada no parse mas **ainda não** ligada ao runner.
- **`delegate_batch`:** padrão de `parallelism` 4, máximo 16 (o contrato inicial dizia 8/32);
  devolve o lote em `queued` e roda em background.
- **Falhas de infra** (worktree, gateway sem candidato, pacote grande) → `needs_lead`.
- **Timeout do verify** mata a árvore de processos (`taskkill /T` no Windows, grupo no Unix).
- **Persistência dos lotes:** decisão adiada; ficou só em memória (32 lotes).
