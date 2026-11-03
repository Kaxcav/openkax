# 0008 — Sandbox do verify (bwrap, fail closed)

Data: 2026-11-03
Status: Proposta (PR `feat/verify-sandbox`, decisão R1 da revisão da Fase 2)

## Contexto

O verify do `kax-workers` (`verify::run_verify`, chamado por `mcp/run.rs`)
compila e executa código escrito por um operário não confiável (build scripts,
macros, testes). O ADR 0007 ("Modelo de ameaça") fechou o vazamento das chaves
pelo ambiente (C1, `ENV_ALLOWLIST`/`clean_env`) e o diff fora da lista `write`
(C2), mas deixou aberto o principal: o processo do verify tem **rede** e lê e
escreve o **disco do usuário**. O `.env` do repo fica dois níveis acima de
`.kax/wt/<id>`; `~/.claude.json`, `~/.ssh`, `~/.cargo/credentials.toml` estão
no alcance. O ADR 0007 adiava o sandbox para a Fase 5; a revisão da Fase 2
(R1) o trouxe para antes da Fase 3.

## Decisão

1. **`sandbox` no `.kax/verify.toml`**: `"bwrap"` (padrão) ou `"none"`. Fail
   closed: onde o padrão não é atendível o verify **recusa** com
   `sandbox indisponível nesta plataforma; rode o kax-workers no WSL ou declare
   sandbox = "none" no .kax/verify.toml` (Windows, macOS) ou `bwrap não está no
   PATH` (Linux sem bubblewrap), e a tarefa vai para `needs_lead`. A checagem
   (`verify::check_sandbox`) roda logo depois de criar o worktree, antes de
   gastar a chamada ao operário. Nunca se cai para `"none"` sozinho. `"none"`
   só vale escrito no arquivo, e cada execução nesse modo põe
   `AVISO: verify executado com sandbox = "none" …` no `verify_log` da tarefa.
2. **Caixa (Linux + bwrap)**: cada comando de verify roda como
   `bwrap --unshare-all --die-with-parent --new-session --ro-bind /usr /usr
   --ro-bind /etc /etc` + `/bin`, `/lib`, `/lib64` (`--symlink` quando são
   links do merged-usr, `--ro-bind` quando são diretórios reais) + `--proc
   /proc --dev /dev --tmpfs /tmp --tmpfs $HOME` + `--ro-bind-try` de
   `RUSTUP_HOME` e de `CARGO_HOME/{bin,registry,git}` + `--bind <worktree>
   <worktree> --chdir <worktree> --setenv CARGO_NET_OFFLINE true -- sh -c
   "<comando>"`. Argumentos em `Vec<OsString>` (sem shell), caminhos
   canônicos. **Nunca** se monta `CARGO_HOME` inteiro (tem `credentials.toml`),
   o repo pai nem `/mnt`. O `bwrap` herda o `clean_env` (allowlist +
   `env_passthrough`); `HOME`, `CARGO_HOME` e `RUSTUP_HOME` vão por `--setenv`
   com os mesmos caminhos montados. O `target/` fica dentro do worktree (padrão
   do cargo) e é gravável. `timeout` e `kill_tree` continuam valendo: o `bwrap`
   lidera o grupo de processos e `--die-with-parent` derruba a caixa junto.
3. **Prefetch com rede, fora da caixa**: `[[language]]` ganha `prefetch =
   [...]` (ex.: `cargo fetch --locked`). Os prefetch de todos os blocos (sem
   repetição; a seleção por glob ainda não existe no runner) rodam no worktree
   recém-criado, **antes** de aplicar o diff do operário — portanto sobre o
   `base_ref`, que é confiável —, com `clean_env`, sem sandbox e com o mesmo
   `timeout_s`. Falhou → `needs_lead`, sem chamar o operário. Um diff que
   traga dependência nova falha no verify offline: é o comportamento desejado
   (dependência nova é decisão do lead).
4. **`openkax doctor`** ganha a checagem `sandbox`: Linux → `OK` se o bwrap
   cria uma caixa sem rede (`bwrap --unshare-all --unshare-net … true`),
   `WARN` se ele falta (o verify vai recusar), `FAIL` se existe mas não cria a
   caixa (userns bloqueado; derruba o exit code). Windows → `WARN` "verify sem
   sandbox no Windows; use o WSL".
5. **CI**: o job ubuntu instala `bubblewrap` e libera userns sem privilégio
   (`kernel.apparmor_restrict_unprivileged_userns=0`, bloqueado por padrão no
   Ubuntu 24.04). Com `CI=true`, bwrap ausente ou quebrado **falha** os testes
   de sandbox; fora do CI eles avisam `PULADO` no stderr.

## Consequências

- **Windows nativo fica sem sandbox.** O default recusa; quem quiser rodar o
  verify no Windows precisa escrever `sandbox = "none"` e aceita o risco do
  ADR 0007. Recomendado: `kax-workers` no WSL (`docs/sandbox-wsl.md`). Job
  Object + AppContainer no Windows continua como trabalho futuro.
- **Testes provam a caixa** (`src/verify.rs`, `sandbox_tests::linux`): sem
  rede (listener em 127.0.0.1 fora da caixa; o controle sem caixa conecta),
  `.env` no pai do worktree e arquivo no `$HOME` real ilegíveis e fora do log,
  escrita no worktree persiste, escrita em `/tmp` some, nada vaza para o pai,
  `/usr` é só leitura, `cargo test` de um crate sem dependências passa
  offline. Mutação conferida à mão: sem `--unshare-all`, com `--ro-bind` do
  `$HOME` ou do pai do worktree, os testes falham. Conferido também, fora dos
  testes, o build do próprio openkax (~200 dependências) na caixa depois de
  `cargo fetch --locked`: o registry só leitura basta.
- **O que a caixa não cobre:**
  - CPU, memória, disco e número de processos **sem limite** (só o timeout por
    comando). Um verify pode encher o disco do worktree ou a RAM.
  - `/etc` e `/usr` inteiros são legíveis (inclusive `/etc/passwd`, configs do
    sistema). Segredo em `/etc` vaza para o log devolvido ao lead (não para a
    rede).
  - Os pontos de montagem que o bwrap cria (pais do worktree dentro do `$HOME`
    ou do `/tmp` em memória) são graváveis, mas somem com a caixa: nada chega
    ao disco real.
  - **`git` não funciona dentro da caixa.** O `.git` de um worktree é um
    arquivo que aponta para `<repo>/.git/worktrees/<id>`, que não é montado
    (e por isso a caixa também não alcança hooks nem config do repo). Verify
    que dependa de `git` precisa de `sandbox = "none"`; é o caso dos testes de
    fluxo do MCP, que usam `git grep`.
  - O prefetch roda sem caixa: comandos de prefetch devem só baixar (como
    `cargo fetch`), nunca executar código do repo. Ele roda sobre o `base_ref`,
    não sobre o diff do operário.
  - macOS: sem sandbox (recusa como no Windows).
- **ADR 0007** continua valendo para C1/C2; o item "Decisão adiada para a Fase
  5" passa a ser coberto por este ADR no Linux.
