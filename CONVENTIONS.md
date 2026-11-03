# Convenções do repo (anexadas a todo pacote de operário)

## Saída
- Devolva só o formato pedido no pacote (diff unificado ou arquivos completos). Nada de explicação fora dele.
- Diff unificado com cabeçalhos `diff --git a/<path> b/<path>`, `--- a/<path>`, `+++ b/<path>` (ou `/dev/null` para arquivo novo).
- As linhas de contexto do diff têm de ser IDÊNTICAS ao arquivo atual (espaços e indentação inclusos). Não invente linhas que não estão no pacote.
- Só altere arquivos marcados `write`. Não renomeie, não copie, não apague, não crie symlink nem mude permissão.

## Rust
- Rust estável, edição 2024. Sem dependências novas (o verify roda offline e falha se o Cargo.toml mudar).
- Erros com `thiserror` e `?`. Sem `unwrap()`/`expect()` fora de teste.
- Sem `todo!()`, `unimplemented!()`, `dbg!` ou placeholder. Código entregue compila e funciona.
- Nada de `println!` em código de biblioteca: use `tracing`. Nunca logue corpo de request, resposta ou credencial.
- Honre exatamente os tipos, traits e assinaturas em `contracts`. Não mude API pública que não foi pedida.
- `cargo fmt` e `cargo clippy --all-targets -- -D warnings` têm de passar.

## Testes
- Nomes descritivos (`rejects_empty_input`, não `test1`). Prefira tabela de casos quando houver vários.
- Teste comportamento observável, não detalhes internos. Sem rede real, sem relógio real (use o que o código já oferece para isso), sem depender de ordem de execução.
- Arquivo novo em `tests/` só usa a API pública do crate (`openkax::...`).

## Texto
- Comentários de alto nível e docs em português do Brasil; identificadores em inglês.
- Comentário explica o porquê, não repete o código.
