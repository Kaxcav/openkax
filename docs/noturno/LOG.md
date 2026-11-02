# Log do turno autônomo

Entradas mais recentes no fim. Formato e regras em `RUNBOOK.md`.

## 2026-11-02 15:20 — preparação (chat de arquitetura) — FEITO
- Conflito do PR #1 resolvido: merge de `main` na `fase-0` (`35e0484`); `providers-setup.md` alinhado ao ADR 0004.
- Runbook criado. Execuções agendadas de hora em hora até 2026-11-03 08:00.
- Portão: Fase 1 só começa depois que o Kaxcav fizer o aceite Continue/Cline e o merge do PR #1.

## 2026-11-02 20:47 — F2-8/F2-9 achados C1 e C2 da revisão do PR #3 — FEITO
- entrou: C1 (`fix(verify)`): verify, `git apply` e `git` do worktree rodam com `env_clear()` + allowlist fixa; `env_passthrough` opcional no `.kax/verify.toml`, recusando `*_KEY`/`*_TOKEN`/`*_SECRET` e `auth.env`/`${VAR}` do `providers.toml`. C2 (`fix(mcp)`): parser de diff em máquina de estados (sem rename/copy/mudança de modo/symlink/submódulo/binário; hunks contados pelo `@@`) + guarda pós-apply `git diff --cached --no-renames --raw -z` contra o commit base, depois do apply e de novo antes do commit; `full_file` recusa escrever através de symlink.
- testes: 148 unit + 41 integração (1 ignorado), clippy e fmt limpos no Windows; ataques (a)/(b)/(c) provados no parser e no `git apply` real + guarda; `OPENKAX_FAKE_API_KEY=vazou-123` não aparece no log do verify; junction no Windows (symlink de arquivo só `cfg(unix)`, roda no CI Linux). CI ainda não rodou.
- revisão independente: não feita nesta sessão (sem subagentes, por cota); os achados vêm da revisão do PR #3.
- pendências: sandbox do verify (bwrap/unshare -n no Linux, Job Object + token restrito no Windows) é decisão da Fase 5 (ADR 0007, "Modelo de ameaça"); o verify ainda lê o disco (inclusive o `.env`) e tem rede. `CLAUDE.md` e `.kax/verify.toml.example` ainda não citam `env_passthrough` (fora do escopo desta sessão).
