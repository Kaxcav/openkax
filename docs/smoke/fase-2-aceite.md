# Roteiro de aceite — Fase 2 (MCP `kax-workers`)

Manual, feito pelo Kaxcav. Gasta quota real dos providers free; nada aqui é automatizado.
Critério: de **10 tarefas** reais, **≥ 7 `accepted` na 1ª passada** (`attempts = 1`).

## 1. Pré-requisitos
- Binário: `target\release\openkax.exe` (ou o artefato do Actions).
- Chaves dos providers no ambiente do processo (`.env` carregado como no gateway). Nunca cole chaves em comandos.
- `providers.toml` válido (`OPENKAX_CONFIG` aponta para ele; padrão: `providers.toml` no diretório atual).
- Repo alvo git limpo, com 10 funções existentes sem teste e `cargo test` verde.
- Opcional: copie `.kax/verify.toml.example` para `<repo>\.kax\verify.toml` (ajusta `timeout_s` e `max_log_bytes`).

## 2. Registrar no Claude Code (stdio)
```powershell
claude mcp add kax-workers --env OPENKAX_CONFIG=D:\dev\openkax\providers.toml -- D:\dev\openkax\target\release\openkax.exe mcp
claude mcp list        # kax-workers deve aparecer conectado
```
As 5 tools: `delegate`, `delegate_batch`, `status`, `quota`, `report`. O stdout é só protocolo; logs vão para stderr.

## 3. Despachar 10 tarefas
Peça ao Claude Code (lead) algo como:

> Use `delegate_batch` com `repo` = `<caminho do repo alvo>`, `base_ref` = `HEAD`, `parallelism` = 4 e 10 tarefas
> `t01`..`t10`, classe `kax/coder-fast`. Cada uma: objetivo "escrever testes unitários para `<função>`",
> `files` = o arquivo da função (`read`) e o arquivo de testes (`write`, `exists` conforme o caso),
> `accept.verify` = `["cargo test <nome_do_teste>"]`, `accept.expect` = `"exit 0"`.

Depois acompanhe com `status(batch_id)` até `state = done` (o `delegate_batch` devolve na hora, tudo em `queued`).

## 4. Onde olhar
| O quê | Onde |
|---|---|
| Estado do lote | tool `status(batch_id)` (só em memória: reiniciar `openkax mcp` perde o lote) |
| Resultado por tarefa | `results[]`: `status`, `provider`, `model`, `attempts`, `diff`, `verify_log`, `tokens`, `latency_ms` |
| Ledger durável | `%LOCALAPPDATA%\OpenKax\ledger\` (JSONL; linhas `kind:"task"`) |
| Agregado | tool `report(period)` com `day`, `week` ou `month` (por classe e por provider) |
| Saldo de quota | tool `quota` |
| Worktrees | `<repo>\.kax\wt\<id>` (ignorado pelo git do repo) |
| Branch aceita | `kax/<id>` — o lead integra (`git merge kax/t01` ou cherry-pick) |

Tarefa `needs_lead` = esgotou tentativas ou falhou fora do operário (parse, apply, verify, sem candidato). O worktree fica preservado para inspeção. O status `rejected` não é produzido.

## 5. Contagem
Conte tarefas com `status = accepted` **e** `attempts = 1`. Registre abaixo.

| Métrica | Valor |
|---|---|
| accepted em 1ª passada | __ / 10 |
| accepted após retry | __ |
| needs_lead | __ |
| providers usados | |
| observações (verify_log relevante, erros) | |

Aceite: ≥ 7/10 em 1ª passada. Se passar, o Kaxcav atualiza a seção "Fase atual" do `CLAUDE.md`.

## 6. Limpeza
```powershell
openkax clean --repo <repo alvo>          # remove worktrees kax/* sem alterações e sem commits novos
openkax clean --repo <repo alvo> --all    # remove TODOS os worktrees kax/* (inclusive aceitos não integrados!)
```
Integre (merge) as branches aceitas **antes** de `--all`. O comando só toca branches `kax/*`.
