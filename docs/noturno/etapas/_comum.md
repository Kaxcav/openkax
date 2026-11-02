# Regras comuns das etapas da esteira (Fase 2 + F5) — LEI

Você é o construtor do OpenKax nesta pasta (worktree da branch `Kaxcav/openkax-fase-2`, Windows, PowerShell). Leia antes de cada etapa: `CLAUDE.md`, `PLANO_MESTRE.md` (§3.6 MCP, §3.7 ledger, §4 Fase 2), `docs/decisions/*.md` e o código que a etapa toca.

## Fronteira (outro agente trabalha em paralelo)
- Um agente na nuvem está construindo a **Fase 1** (ponte Anthropic) em outra branch. **Não edite** `src/translate/**` nem crie rotas `/v1/messages*` em `src/server.rs`.
- Seus arquivos: `src/mcp*.rs` / `src/mcp/**`, `src/verify*.rs` / `src/verify/**`, `src/worktree*.rs`, `src/doctor*.rs`, `src/report*.rs`, `tests/mcp_*.rs`, `tests/doctor_*.rs`, `tests/report_*.rs`, `docs/decisions/0007-*.md`, `docs/smoke/fase-2-aceite.md`, `.kax/verify.toml.example`. Em `src/main.rs` e `src/lib.rs` mexa só no mínimo (despacho de subcomando, `mod`).
- Não mude `providers.toml` (rotas, limites, modelos). Não mude o formato do ledger existente (só acrescente `kind:"task"` como o §3.7 define).

## Segurança
- **Nunca** leia, imprima, logue ou commite valor de chave. Não existe `.env` nesta pasta e não deve existir.
- **Nenhuma request a provider real.** Operários respondem via `wiremock` com diffs fixos.
- Nada de `git push`, merge, force-push, apagar branch. A esteira commita localmente; o push é feito depois.

## Qualidade
- TDD: escreva os testes da etapa primeiro, confirme que falham, implemente.
- Verde antes de terminar: `cargo clippy --all-targets -- -D warnings` e `cargo test` (é isso que a esteira roda).
- Sem `todo!()`, sem `unimplemented!()`, sem stub fora da etapa CONTRACT, sem `#[allow(...)]` para calar clippy.
- Testes de worktree/git usam repositório temporário (`tempfile`) — nunca o próprio repo.
- Código cross-platform (Windows e Linux): caminhos com `Path`/`PathBuf`, nada de `/` fixo; processos com `std::process::Command`/`tokio::process`.
- Commits: `tipo(escopo): resumo` (feat, fix, test, refactor, docs). Docs e comentários de alto nível em PT-BR; identificadores em inglês.
- Atualize `tasks.json` (status da etapa, nota curta com números de testes) no fim de cada etapa.

## Fim de cada etapa
Responda com: o que entrou (arquivos), contagem de testes, o que NÃO foi verificado, e qualquer decisão que precise do Kaxcav (marque `needs_user` no `tasks.json` em vez de inventar).
