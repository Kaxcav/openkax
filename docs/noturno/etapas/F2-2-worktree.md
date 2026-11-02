# Etapa F2-2 — worktree por tarefa

Objetivo: isolamento real por tarefa.

Entregáveis:
1. Módulo `src/worktree.rs`: `create(repo, base_ref, task_id) -> Worktree` executando `git worktree add <repo>/.kax/wt/<task_id> -b kax/<task_id> <base_ref>`; `remove(...)` com `git worktree remove --force` + `git branch -D` só para branches `kax/*` criadas por nós e ainda não aceitas; `list()`.
2. Garantias: `task_id` validado (só `[A-Za-z0-9._-]`, sem `..`), caminho final verificado como descendente de `<repo>/.kax/wt/`; `.kax/` adicionado ao `.git/info/exclude` do repo alvo (não ao `.gitignore` versionado); erro claro se `repo` não é git ou `base_ref` não existe.
3. Subcomando `openkax clean [--repo <path>] [--all]`: remove worktrees de tarefas rejeitadas/abandonadas; nunca toca branch que não seja `kax/*`.
4. Testes com repo temporário (`tempfile` + `git init`): cria, lista, remove; id malicioso (`../x`, `a/b`) recusado; base_ref inexistente → erro; duas tarefas paralelas não colidem.
