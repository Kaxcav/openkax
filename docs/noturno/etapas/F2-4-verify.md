# Etapa F2-4 — aplicar diff e verificar no worktree

Objetivo: verificação determinística, sempre dentro do worktree.

Entregáveis:
1. Aplicação do diff com `git apply --index --whitespace=nowarn` no worktree (falha de apply = tentativa rejeitada com log).
2. Runner de verify (`src/verify.rs`): executa cada comando de `accept.verify` (e/ou os do `.kax/verify.toml`) com `current_dir` = worktree, timeout por comando, mata a árvore de processo no timeout (Windows e Unix), captura stdout+stderr truncados em `max_log_bytes` (padrão 4096, mantendo o fim), compara com `expect` (`exit 0` por padrão).
3. Nada roda fora do worktree; comandos vêm só do pacote/verify.toml (sem interpolar texto do operário em linha de comando).
4. Testes com repo temporário: diff que passa no verify; diff que quebra o verify; comando que estoura timeout é morto; log truncado mantém o final; apply que não aplica.
