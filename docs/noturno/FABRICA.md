# Frente da fábrica (Gemini via esteira) — Fase 2 + F5

- Branch: `Kaxcav/openkax-fase-2` (a partir de `fase-0`). Worktree no PC: `D:\dev\wt-openkax-fase-2`.
- Executor: `C:\fabrica\scripts\esteira.ps1` (Gemini `agy` constrói; reserva Claude quando a cota da Gemini acaba). Uma etapa por vez, testes, commit local.
- Fila: `docs/noturno/etapas/F2-1..F2-7`, `F5-1`, `F5-2`. Regras: `docs/noturno/etapas/_comum.md`.
- Teste da esteira: `cargo clippy --all-targets -- -D warnings && cargo test`.
- Log: `%USERPROFILE%\.fabrica\esteira\openkax-fase-2.log` (+ `-feitos.txt`, `-<etapa>-falha.txt`).
- Em paralelo, o turno da nuvem (Claude) faz F0-24 + Fase 1 na branch `fase-1`. Fronteira de arquivos em `_comum.md`.
- Sem push automático: de manhã a branch é publicada e revisada pelo chat de arquitetura antes de qualquer merge (sempre do Kaxcav).
