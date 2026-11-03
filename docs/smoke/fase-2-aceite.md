# Roteiro de aceite — Fase 2 (MCP `kax-workers`)

Manual, feito pelo Kaxcav. Gasta quota real dos providers free; nada aqui é automatizado.
Critério: de **10 tarefas** reais, **≥ 7 `accepted` na 1ª passada** (`attempts = 1`).

O `openkax mcp` roda **no WSL**: no Windows nativo o verify não tem sandbox e recusa a tarefa (`needs_lead`, ADR 0008). Preparação da distro, build Linux e detalhes do registro: `docs/sandbox-wsl.md`. Este roteiro só resume.

## 1. Pré-requisitos (no WSL, distro `Ubuntu-24.04`)
```bash
# repo privado: autentique o git antes (ex.: sudo apt-get install -y gh && gh auth login)
git clone https://github.com/Kaxcav/openkax ~/openkax && cd ~/openkax
cp /mnt/d/DEV/openkax/.env .env          # chaves só no .env; nunca em comando nem em chat
source ~/.cargo/env && cargo build --release
./target/release/openkax doctor          # tem de mostrar [OK] sandbox
```
- Repo alvo no ext4 do WSL (não em `/mnt/c` ou `/mnt/d`), git limpo, `cargo test` verde, com 10 funções existentes sem teste. Pode ser o próprio `~/openkax` (dogfooding: o `.env` na raiz fica invisível para o verify, que roda na caixa).
- `<repo>/.kax/verify.toml` com prefetch (a caixa não tem rede):
  ```toml
  timeout_s = 900
  sandbox = "bwrap"

  [[language]]
  name = "rust"
  globs = ["**/*.rs", "Cargo.toml", "Cargo.lock"]
  commands = ["cargo test"]
  prefetch = ["cargo fetch --locked"]
  ```
- Comandos de verify **não podem usar git** (não funciona dentro da caixa).

## 2. Registrar no Claude Code (PowerShell, no Windows)
```powershell
claude mcp add kax-workers -- wsl -d Ubuntu-24.04 -- bash -c '. ~/.cargo/env && cd ~/openkax && OPENKAX_CONFIG=$HOME/openkax/providers.toml exec ./target/release/openkax mcp'
claude mcp list        # kax-workers deve aparecer conectado
```
Sempre `-d Ubuntu-24.04`: a distro padrão pode ser a `docker-desktop`, que nem tem bash. Use `bash -c`, não `bash -lc` (um `.profile` que imprima algo quebra o protocolo, que usa o stdout).
As 5 tools: `delegate`, `delegate_batch`, `status`, `quota`, `report`.

## 3. Despachar 10 tarefas
Peça ao Claude Code (lead) algo como:

> Use `delegate_batch` com `repo` = `/home/<user>/openkax`, `base_ref` = `HEAD`, `parallelism` = 4 e 10 tarefas
> `t01`..`t10`, classe `kax/coder-fast`. Cada uma: objetivo "escrever testes unitários para `<função>`",
> `files` = o arquivo da função (`read`) e um arquivo novo em `tests/` (`write`, `exists: false`),
> `accept.verify` = `["cargo test --test <arquivo>"]`, `accept.expect` = `"exit 0"`.

`repo` é **caminho Linux**, nunca `D:\...`. Acompanhe com `status(batch_id)` até `state = done` (o `delegate_batch` devolve na hora, tudo em `queued`). Formato do pacote: `docs/processo/pacote.md`.

## 4. Onde olhar
| O quê | Onde |
|---|---|
| Estado do lote | tool `status(batch_id)` (só em memória: reiniciar `openkax mcp` perde o lote) |
| Resultado por tarefa | `results[]`: `status`, `provider`, `model`, `attempts`, `diff`, `verify_log`, `tokens`, `latency_ms` |
| Ledger durável | no WSL, `~/.local/share/openkax/ledger/` (XDG; JSONL, linhas `kind:"task"`) |
| Agregado | tool `report(period)` com `day`, `week` ou `month` (por classe e por provider) |
| Saldo de quota | tool `quota` |
| Worktrees | `<repo>/.kax/wt/<id>` (ignorado pelo git do repo) |
| Branch aceita | `kax/<id>` — o lead integra (`git merge kax/t01` ou cherry-pick) |

Tarefa `needs_lead` = esgotou tentativas ou falhou fora do operário (sandbox indisponível, prefetch, parse, apply, guarda de escopo, verify, sem candidato). O worktree fica preservado para inspeção. O status `rejected` não é produzido.

## 5. Contagem
Conte tarefas com `status = accepted` **e** `attempts = 1`. Registre abaixo.

| Métrica | Valor |
|---|---|
| accepted em 1ª passada | __ / 10 |
| accepted após retry | __ |
| needs_lead | __ |
| providers usados | |
| observações (verify_log relevante, erros) | |

Aceite: ≥ 7/10 em 1ª passada.

## 6. Limpeza (no WSL)
```bash
./target/release/openkax clean --repo <repo alvo>          # remove worktrees kax/* sem alterações e sem commits novos
./target/release/openkax clean --repo <repo alvo> --all    # remove TODOS os worktrees kax/* (inclusive aceitos não integrados!)
```
Integre (merge) as branches aceitas **antes** de `--all`. O comando só toca branches `kax/*`.
