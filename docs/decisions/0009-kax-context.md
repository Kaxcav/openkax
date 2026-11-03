# 0009 — kax-context: contexto enxuto por símbolo nos pacotes dos operários

Data: 2026-11-03
Status: Proposta. P6a (CONTRACT) na `main` pelo PR #10; P6c no PR
`feat/context-core`.

## Contexto

- **Limites free (PLANO §1.2).** A Groq aceita 8k TPM e o `kax/coder-fast`
  limita o pacote a 6k tokens; o `kax/coder-smart`, a 60k. O que cabe no
  pacote decide quais providers servem a tarefa.
- **Modo A gasta demais.** No smoke S1a (commit `16ab2bc`), o Explore do Claude
  Code roteado para `kax/summarizer` fez ~25 chamadas em rajada, com ~30k de
  entrada cada: **~700k tokens por execução**. Isso esgota a Gemini free em
  segundos e mostra quanto custa mandar arquivos inteiros.
- **O pacote enxuto é o gargalo do Modo B.** Hoje `mcp/packet.rs::build_request`
  manda cada `files[]` com `mode = "read"` **inteiro**. Para mostrar uma função
  de 20 linhas de um arquivo de 1.000, o pacote estoura `max_packet_tokens`
  (`PacketError::TooLarge`) ou come a cota do minuto.
- **Objetivo.** Entregar só o trecho necessário: o símbolo pedido, os tipos que
  ele usa, as assinaturas do que ele chama e quem o chama. A meta da P6d é
  pacote **≥ 3× menor** que o arquivo inteiro, sem queda na aceitação em 1ª
  passada (PLANO §2.3).

## Estudo do gortex

Fiz um clone raso do [gortex](https://github.com/zzet/gortex) (Go, Apache 2.0,
commit `9241f04`, 2026-11-03) numa pasta temporária fora do repo, e nada dele
foi executado. Li `docs/architecture.md`, `docs/mcp.md`,
`internal/parser/extractor.go`, os extratores
`internal/parser/languages/{rust,typescript,python}.go`, `internal/mcp/manifest.go`,
`internal/elide/elide.go` e partes de `internal/resolver/`.

Ideias aproveitadas:

| Ideia no gortex | Onde | No kax-context |
|---|---|---|
| `Extractor { Language, Extensions, Extract(path, src) }`, um por linguagem | `parser/extractor.go` | `trait LanguageAdapter { id, extensions, extract }` |
| Uma query tree-sitter só por arquivo, alternando padrões, com despacho pelo nome da captura e o dono (impl/trait/class) achado subindo pelos pais do nó | `languages/rust.go` (`qRustAll`), `python.go` (`qPyAll`) | recomendação para P6b/P6c: um cursor por arquivo |
| ID estável `<arquivo>::<Símbolo>`, desempatado pela linha quando colide | `rust.go` (`disambiguateID`) | `SymbolId = "<arquivo>::<qual_name>"`, com `#<linha>` só em colisão |
| A extração por arquivo deixa referências não resolvidas (`unresolved::nome`, com o caminho qualificado preservado), e um resolvedor separado as liga depois | `rust.go`, `resolver/` | `Ref { name, qual_hint }` por arquivo + `resolve.rs` global |
| Função envolvente pela faixa de linhas | `findEnclosingFunc` | `Ref.enclosing` |
| Precedência por escopo e "ambíguo → não adivinhar" (`pickInScopeBinding`, `cpp_overload`: "never lie"); `get_callers` com o aviso `coverage_incomplete` quando a evidência é só o nome | `resolver/`, `docs/mcp.md` | `Resolution::Ambiguous` com todos os candidatos; nenhuma consulta escolhe entre empatados |
| Reinício incremental: só os arquivos alterados são reparseados (`route=scoped`) | `docs/architecture.md` | hash por arquivo; contador de reparse no teste de aceite |
| Pacote graduado (`smart_context`/`explore`): foco com o fonte inteiro, anel de chamadores e chamados como assinatura, um orçamento só, truncamento relatado | `mcp/manifest.go`, `docs/mcp.md` | `pack`: 4 prioridades, omitidos com motivo |
| Corpo vira stub, assinatura fica | `internal/elide` | `signature_span` |
| `.tsx` com a gramática TSX, `.ts` com a TS (o `<T>x` quebra na TSX) | `typescript.go` (`grammarFor`) | o adapter TS escolhe a gramática pela extensão |
| Teto de extração por arquivo | `parser/extraction_limits.go` | `MAX_FILE_BYTES` |

Ficou de fora: SQLite (regra do projeto, ADR 0002), daemon multi-cliente, UI,
as ~257 linguagens (aqui só Rust, TS/JS e Python), grafo entre repos, contratos
HTTP/gRPC, review de PR, LSP agora, as 100+ tools, dataflow, clones, embeddings
e telemetria.

**Nada foi portado.** O gortex é Go, e este CONTRACT só tem tipos e stubs em
Rust escritos do zero, por isso não há `NOTICE` na raiz nesta etapa. A regra
para P6b/P6c: se uma query tree-sitter ou heurística for adaptada de forma
reconhecível de `internal/parser/languages/*.go` ou `internal/resolver/*.go`,
cria-se o `NOTICE` na raiz (Apache 2.0, © 2024-2026 Andrey Kumanyaev) e um
comentário no arquivo Rust que recebeu o trecho, citando o arquivo de origem e
o commit `9241f04`.

## Decisão

1. **Módulo `src/context/` no mesmo binário**, sem processo à parte:
   `mod.rs` (tipos e API), `index.rs`, `lang/{mod,rust,typescript,python}.rs`,
   `resolve.rs` e `pack.rs`.
2. **Linguagens:** Rust (`.rs`), TypeScript (`.ts .tsx .mts .cts`, com a
   gramática TSX só no `.tsx`), JavaScript (`.js .jsx .mjs .cjs`) e Python
   (`.py`).
3. **Fatos por arquivo** (`FileFacts`): `module`, `symbols`, `refs`, `imports`
   e `parse_errors`.
   - Antes de tudo, o texto é normalizado de CRLF para LF, como o pacote já faz.
     Os spans são bytes desse texto normalizado; as linhas são 1-based e
     inclusivas. Assim o resultado é o mesmo com e sem `core.autocrlf`.
   - `Symbol.span` cobre atributos e decorators, mas não a doc. A doc fica em
     `doc_span` (a docstring Python cai dentro do `span`). O `signature_span`
     vai do 1º token do item, sem atributos, até antes do corpo.
   - `qual_name` são os contêineres dentro do módulo mais o nome, com o
     separador da linguagem (`::` em Rust, `.` em TS/Python). Um impl se chama
     `impl Tipo` ou `impl Trait for Tipo`, também no `name`, para que a query
     `Rect` não case com os impls. O método de um impl leva o nome do tipo
     (`Rect::area`), e o método declarado num trait, o do trait
     (`Shape::area`). `mod x;` e campos de struct não são símbolos.
   - `SymbolId` é `<arquivo>::<qual_name>`. Se dois símbolos do mesmo arquivo
     colidem, o 1º fica sem sufixo e os seguintes ganham `#<linha inicial>`.
   - `Ref`: `name`, `qual_hint` (o qualificador escrito antes do nome: `Rect` em
     `Rect::new`, `crate::geometry` em `crate::geometry::validate`, `self`,
     `Self`, `this`), `method` (`recv.nome(...)`), `enclosing` e `kind` (`call`,
     `type` ou `use`).
4. **Índice** (`Index`): `open(root, store)`, `refresh`, `save` e as consultas
   `outline(file)`, `symbol(query)`, `refs(query)`, `callers(query)` e
   `pack(targets, budget_tokens, chars_per_token)`. Uma query pode ser um nome
   (`validate`), um caminho qualificado casado pelo sufixo (`Rect::new`,
   `Rect.square`, `geometry::validate`) ou um id (`src/store.rs::validate`).
   - **Persistência em JSON**: `serde_json` já está no binário, e o arquivo dá
     para inspecionar, como o snapshot dos buckets (ADR 0005) e o ledger.
     Bincode seria dependência nova sem ganho medido. O índice fica em
     `<store>/index.json`, gravado num arquivo temporário e trocado por rename.
     O store padrão é `<repo>/.kax/context/`, com um `.gitignore` próprio
     contendo `*`. Se o arquivo crescer demais, divide-se por arquivo de fonte
     num ADR novo.
   - **Invalidação**: xxh3-128 do conteúdo bruto, mais o id do adapter, mais
     `INDEX_FORMAT_VERSION`, que sobe quando um adapter muda a extração. Se o
     arquivo não mudou, os fatos são reaproveitados sem parse, e o
     `RefreshStats` conta quantos foram reparseados.
   - **Percurso**: crate `ignore` (o do ripgrep) com `.gitignore`, `.ignore` e
     `.git/info/exclude`, e `require_git(false)` para valer também fora de um
     repo git. Arquivos ocultos ficam de fora, e `.git/` e `.kax/` (onde ficam
     os worktrees dos operários) ficam sempre de fora. Arquivos acima de 1 MiB
     ou que não são UTF-8 são pulados e contados.
   - **Raiz e store**: a raiz indexada é o worktree da tarefa, isto é, o
     `base_ref`, o mesmo conteúdo que o pacote lê. O store é do repo principal
     e é compartilhado pelos worktrees. Se dois gravarem ao mesmo tempo, vence o
     último. Como toda entrada passa pelo hash, um índice antigo só custa
     reparse.
5. **Resolução heurística** (`resolve.rs`, `SymbolTable` atualizada por arquivo):
   1. Candidatos com o mesmo nome e um tipo compatível: `call` → fn, struct ou
      class (construtor); `call` com `method` → method; `type` → struct, enum,
      trait, type, class ou interface; `use` → qualquer um.
   2. Filtro pelo hint. Se há `qual_hint`, ele é normalizado (`self`, `Self`,
      `crate`, `super` e `this` são resolvidos, e um alias de import é
      expandido) e precisa ser sufixo do caminho completo do candidato
      (`module` mais o `qual_name`). Sem `qual_hint`, um import explícito do
      nome no arquivo filtra pelo caminho desse import. Se o filtro zera os
      candidatos, o resultado é `Unresolved`: é um nome externo (std, crates,
      `node_modules`).
   3. Proximidade: **mesmo arquivo > mesmo módulo** (o mesmo diretório: pacote
      Python, pasta TS, módulos irmãos em Rust) **> importado** (por import
      explícito, glob ou de módulo) **> global**. O degrau de um candidato é o
      mais perto que se aplica a ele. O import explícito age antes, no passo 2:
      um `use crate::deep::helper` vence um `helper` do arquivo vizinho.
   4. No melhor degrau com candidatos, se sobra 1 o resultado é
      `Resolved { target, tier }`; se sobram vários, `Ambiguous { candidates,
      tier }`. Nunca se escolhe entre empatados.
6. **Pack** (`pack.rs`), por ordem de prioridade:
   1. o corpo dos alvos (doc + span);
   2. as definições dos tipos que os alvos usam, inclusive o tipo dono de um
      método alvo;
   3. as assinaturas das funções que os alvos chamam;
   4. as assinaturas dos chamadores dos alvos.

   A escolha é gulosa nessa ordem e, dentro de cada prioridade, na ordem do
   fonte. Um item que não cabe é omitido como `budget`, e o pack segue para o
   próximo, porque um menor pode caber. Uma dependência ambígua é omitida como
   `ambiguous`, com os candidatos. Um trecho que já está dentro de outro não se
   repete. Se o alvo é ambíguo, o erro é `AmbiguousTarget` e o lead qualifica a
   query. Se o corpo dos alvos não cabe no orçamento, o erro é `BudgetTooSmall`
   e o lead manda o arquivo inteiro ou aumenta o orçamento. A conta de tokens é
   `ceil(chars / chars_per_token)`, a mesma do scheduler, com o
   `chars_per_token` do 1º degrau da classe (`Scheduler::class_chars_per_token`).
7. **MCP** (`src/mcp/types.rs`): `files[]` ganha `symbols: Option<Vec<String>>`
   e `lines: Option<[u32; 2]>`, que só valem com `mode = "read"`. `symbols` e
   `lines` juntos são recusados, `lines` exige `1 ≤ início ≤ fim`, e `symbols`
   não pode ter item vazio. **Até a P6d o pacote não os atende**, então a
   validação recusa a tarefa com `invalid_input` em vez de mandar o arquivo
   inteiro sem avisar. Sem esses campos, o pacote sai byte a byte igual ao de
   hoje (teste golden).
8. **CONTRACT**: os stubs devolvem `ContextError::NotImplemented("P6c: …")`,
   sem `todo!()` nem `unimplemented!()`. Os adapters devolvem `FileFacts` vazio:
   o `extract` é infalível por desenho, porque o tree-sitter tolera erro de
   sintaxe, que fica contado em `parse_errors`. Os testes de aceite levam
   `#[ignore = "P6c"]` (Rust, índice, resolução, pack) ou `#[ignore = "P6b"]`
   (TS/JS, Python).

Etapas: **P6a** CONTRACT (esta); **P6b** adapters TS/JS e Python; **P6c**
adapter Rust, índice, resolução e pack; **P6d** ligar `files[].symbols`/`lines`
ao `build_request` e medir (≥ 3× menor, com aceitação ≥ a de hoje).

## Alternativas consideradas

- **gortex externo** (daemon + MCP). Seria um processo à parte, em Go, com
  SQLite e uma centena de tools para configurar. O operário não fala MCP, então
  o lead teria de copiar os trechos para o pacote, e o gateway precisa deles
  dentro do pacote, sem ida e volta. O pacote passaria a depender de um daemon:
  nesta sessão, o MCP do gortex não conectou.
- **LSP agora** (rust-analyzer, tsserver, pyright). A precisão é a do
  compilador, mas cada linguagem traz um processo pesado (o rust-analyzer leva
  centenas de MB e minutos de indexação), que precisa da toolchain e do projeto
  configurados no worktree. Fica como camada futura atrás da mesma API, como a
  resolução em camadas do gortex: tree-sitter primeiro, LSP enriquece.
- **ctags ou regex.** Não dão referências nem chamadas confiáveis, nem o span do
  corpo.
- **Só recorte por linhas (`lines`).** É barato e entra junto na P6d, mas exige
  que o lead saiba as linhas e não traz tipos nem assinaturas de outros
  arquivos.

## Consequências

- **Heurística não é compilador.** Macros, traits genéricos, receptor de método
  sem tipo, re-exports e glob imports viram `Unresolved` ou `Ambiguous`. O pack
  lista o que omitiu, e o lead decide. Um erro de resolução manda contexto a
  mais ou a menos, nunca código alterado: o verify continua sendo o juiz.
- **A ambiguidade é explícita.** Nenhuma consulta escolhe entre empatados.
  `x.area()` com três métodos `area` volta como `Ambiguous` com os três.
- **Dependências novas:** `tree-sitter` 0.27 com as gramáticas `tree-sitter-rust`
  0.24, `tree-sitter-typescript` 0.23, `tree-sitter-javascript` 0.25 e
  `tree-sitter-python` 0.25. As gramáticas são C compilado pelo crate `cc`. No
  Windows ele usa o `cl.exe` do MSVC Build Tools, o mesmo pacote cujo `link.exe`
  o rustc MSVC já exige e que o `ring` já usa; no Linux, `cc`/gcc. Por isso não
  é "toolchain C extra" no sentido do ADR 0002. Também entram `ignore` 0.4 e
  `xxhash-rust` 0.8 (`xxh3`, Rust puro). Nesta etapa conferi que tudo compila e
  que as 5 gramáticas carregam no Windows/MSVC (teste
  `context::lang::tests::grammars_load`); no Linux, pelo CI.
- **O binário fica maior.** Isso foi aceito pela decisão do Kaxcav de
  2026-11-03, que tirou os limites de 8 MB de binário e 10 MB de memória
  privada como critério de aceite (passam a ser métrica informativa). A
  decisão está no ADR 0002 (seção "Revisão 2026-11-03") e no PLANO §3.1 e §4.
  - Medido já no CONTRACT (`.exe` release local, Windows/MSVC): **4.180.992 B
    na `main` → 4.564.480 B nesta branch (+383.488 B)**, sem nenhuma gramática
    no binário (nenhuma string delas aparece no `.exe`). O aumento vem da
    unificação de features: o `tree-sitter` depende de `regex` com `unicode`,
    obrigatório, e isso liga o Unicode do `regex-syntax` que o filtro do
    `tracing-subscriber` já usava. Com isso as tabelas Unicode entram no
    binário (`Old_Hungarian`, `Grapheme_Cluster_Break` passam a aparecer).
    Desligar as features default do `tree-sitter` não muda o tamanho.
  - As gramáticas (a TS/TSX é a maior) entram quando o índice for ligado ao
    `main`. A medição fica para a P6c.
- O build fica mais lento uma vez, pelo C das gramáticas.
- `.kax/context/` entra no `.gitignore` do openkax. Nos outros repos, o próprio
  store cria seu `.gitignore`.
- `tests/fixtures/context/**` e `tests/fixtures/mcp/**` ficam com `eol=lf`
  (`.gitattributes`). Com `core.autocrlf=true`, que é o padrão do Git for
  Windows e dos runners `windows-latest`, as fixtures mudariam de bytes no
  checkout.
- O `NotImplemented` sai de cena na P6c, e a recusa de `symbols`/`lines`, na P6d.

## P6c — decisões de implementação (2026-11-03)

PR `feat/context-core`. Nada foi adaptado do gortex nesta etapa: o adapter
Rust percorre a árvore com cursor, sem query tree-sitter, e a heurística de
resolução é a do item 5 acima, escrita do zero. Por isso continua sem
`NOTICE`.

- **Adapter Rust.** O percurso é iterativo, com pilha explícita, para que uma
  expressão muito aninhada não estoure a pilha da thread (1 MiB no Windows).
  `self.f()` sai com `qual_hint = "self"` e `method = true`, e o `self` vira o
  tipo do contêiner. O nome do impl é o cabeçalho com os espaços colapsados
  (`impl !Send for X` mantém o `!`). Os métodos levam o tipo-base, sem
  genéricos nem `&`. Itens dentro do corpo de uma fn valem pelo módulo, sem o
  dono. `Self` em posição de tipo não vira ref. `use a::{self}` vira o import
  de `a`, com origem no pai de `a`. Macros não são expandidas: o que está em
  `foo!(...)` não gera ref.
- **Resolução.** Uma `call` com `qual_hint` e sem `method` também aceita
  método (`Rect::new`, `Rect.square`). A ref `use` não expande imports, porque
  o hint dela é a própria origem. Hint absoluto (`crate::`, `self::`,
  `super::`, `Self`, `this`, `self`/`cls` do Python, origem relativa `./m` ou
  `.m`) exige contêiner igual. `crate::` também exige o mesmo crate (a pasta
  antes do último `src/`). Os outros hints casam pelo sufixo.
  **Limite conhecido:** o nome do crate como 1º segmento (`openkax::context::
  Index` em `tests/` e no `main.rs`) não é conhecido do índice e fica
  `Unresolved`. Ler o `[package].name` dos `Cargo.toml` resolveria isso.
- **Índice.** Os `.gitignore` acima da raiz não valem (`parents(false)`), e
  links não são seguidos. Um arquivo antes indexado que passa a ser pulado
  conta em `skipped` e em `removed`. As consultas releem o texto e conferem o
  hash: se o arquivo mudou depois do `refresh`, devolvem `ContextError::Io`
  em vez de linhas trocadas. No `open`, uma entrada persistida com caminho fora
  da raiz é descartada.
- **Pack.** O dono de um método de `impl` é resolvido pela ref de tipo do
  cabeçalho do impl, que preserva o caminho escrito. Uma `call` que resolve
  para um tipo (`new X()`, `X()`) entra como `type_def`. Um chamador cujo
  sítio é ambíguo vai para `omitted` como `ambiguous`, com o id dele. Uma
  definição inteira que conteria um trecho já incluído (a classe ou o trait de
  um método alvo) entra só pela assinatura. Trechos `full` do mesmo arquivo
  que se tocam ou se sobrepõem saem fundidos, com o papel e o símbolo do de
  maior prioridade. Um `chars_per_token` não positivo vale 1.
- **Medição no próprio repo** (`index_the_openkax_repo`, debug): no Windows,
  58 arquivos e 1.646 símbolos; refresh frio em 0,61 s, quente (outro `open`
  sobre o índice salvo) em 0,10 s. No WSL (Ubuntu 24.04, repo em `/mnt/d`),
  frio em 0,76 s e quente em 0,29 s, com um `index.json` do mesmo tamanho (o
  checkout do Windows tem CRLF, e os spans são do texto normalizado).
  O `index.json` tem 4.034.737 B, cerca de
  4,1× os 981.778 B de fonte. O pack de `Index::refresh` dá 1.669 tokens,
  contra 6.195 do arquivo inteiro (3,7× menor). O tamanho do índice está na
  margem do "se crescer demais" da decisão 4. As saídas mais baratas, se a
  P6d pedir, são omitir os campos vazios e não repetir o `file` em cada
  símbolo e ref.
- **Binário.** O `main` ainda não chama o índice, então as gramáticas
  continuam fora do `.exe`. A medição com elas passa para a P6d.

## Atribuição

As ideias da seção "Estudo do gortex" vêm do gortex
(<https://github.com/zzet/gortex>), licenciado sob a Apache License 2.0,
Copyright 2024-2026 Andrey Kumanyaev. Nenhum trecho foi portado nesta etapa.
