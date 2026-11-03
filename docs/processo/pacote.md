# Pacote mínimo de tarefa (`delegate` / `delegate_batch`)

O pacote é tudo o que o operário vê: ele não conhece o repo além disso. Pacote ruim = tarefa rejeitada ou retrabalho do lead. Tipos em `src/mcp/types.rs` (`TaskPacket`, `FileSpec`, `Accept`); política de delegação no `CLAUDE.md`.

## Campos

| Campo | Regra |
|---|---|
| `id` | letras, dígitos, `-`, `_`, `.`; vira a branch `kax/<id>` |
| `class` | começa com `kax/`. Pacote ≤ 6k tokens → `kax/coder-fast`; até ~60k → `kax/coder-smart` |
| `objective` | **uma frase**, verificável ("escrever testes de tabela para `X` cobrindo Y e Z") |
| `files` | `read` = contexto (não pode mudar); `write` = o que o operário cria/altera (`exists: false` se ainda não existe). Ao menos um `write`. Só o mínimo: cada arquivo a mais é cota gasta |
| `contracts` | assinaturas, tipos e traits que ele tem de honrar, copiados do código |
| `constraints` | o que não pode: "não alterar src/", "sem deps novas", "sem unwrap" |
| `accept.verify` | comandos rodados no worktree, dentro da sandbox (sem rede, sem git); o primeiro que falhar reprova |
| `accept.expect` | resultado esperado, hoje `"exit 0"` |
| `output` | `unified_diff` (padrão) ou `full_file` (arquivo novo e pequeno) |
| `max_attempts` | 1..=3 (padrão 2). A 2ª tentativa vai para o próximo provider com o erro anexado |

O MCP anexa o `CONVENTIONS.md` da raiz a todo pacote; não repita aquelas regras em `constraints`.

Quando o P6 (`kax-context`, ADR 0009) entrar, `files[]` com `mode: "read"` aceita `symbols` (só os trechos desses símbolos, com tipos usados e assinaturas relacionadas) ou `lines` (`[início, fim]`) em vez do arquivo inteiro. Prefira isso sempre que o operário não precisar do arquivo todo.

## Checklist antes de despachar
1. O contrato (tipos, assinaturas, testes de aceite) já está commitado no `base_ref`?
2. Dá para o operário terminar só com o que está no pacote?
3. O `accept.verify` falharia se o operário entregasse algo errado? (comando que só compila não prova nada)
4. Os `write` são disjuntos entre as tarefas do mesmo lote?
5. Nada de auth, criptografia, migração destrutiva, arquitetura ou spec ambígua (política: essas o lead faz).

## Exemplo 1 — testes de tabela para uma função pura (`kax/coder-fast`)

```json
{
  "id": "t-glob-01",
  "class": "kax/coder-fast",
  "objective": "Escrever testes de tabela para openkax::config::glob_matches cobrindo padrão exato, * no início, meio e fim, vários *, string vazia e padrão só com *.",
  "files": [
    { "path": "src/config.rs", "mode": "read" },
    { "path": "tests/glob_matches.rs", "mode": "write", "exists": false }
  ],
  "contracts": ["pub fn glob_matches(pattern: &str, s: &str) -> bool"],
  "constraints": ["não alterar src/", "um único #[test] com tabela de casos (pattern, input, esperado)"],
  "accept": { "verify": ["cargo test --test glob_matches"], "expect": "exit 0" },
  "output": "unified_diff",
  "max_attempts": 2
}
```

## Exemplo 2 — validação de entrada com casos de erro (`kax/coder-fast`)

```json
{
  "id": "t-since-01",
  "class": "kax/coder-fast",
  "objective": "Escrever testes para openkax::report::parse_since_date cobrindo datas válidas, formato errado, mês e dia fora da faixa e a mensagem de erro.",
  "files": [
    { "path": "src/report.rs", "mode": "read" },
    { "path": "tests/report_since.rs", "mode": "write", "exists": false }
  ],
  "contracts": ["pub fn parse_since_date(s: &str) -> Result<String, String>"],
  "constraints": [
    "não alterar src/",
    "a função aceita 2026-02-31 de propósito (só valida faixa de mês 1-12 e dia 1-31): não escreva teste esperando erro nesse caso"
  ],
  "accept": { "verify": ["cargo test --test report_since"], "expect": "exit 0" },
  "output": "unified_diff",
  "max_attempts": 2
}
```

No exemplo 2, a segunda `constraint` existe porque o comportamento é contra-intuitivo: sem ela, um operário escreveria um teste "correto" que falha, e a tarefa voltaria como `needs_lead`. Comportamento surpreendente do código vai explícito no pacote.
