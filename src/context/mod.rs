//! `kax-context`: contexto enxuto por símbolo para os pacotes dos operários
//! (ADR 0009, P6).
//!
//! Em vez de mandar arquivos inteiros, o pacote leva só o trecho necessário:
//! o símbolo pedido, os tipos que ele usa, as assinaturas do que ele chama e
//! quem o chama. O desenho (grafo de símbolos via tree-sitter, atualização
//! incremental por arquivo, resolução em camadas e empacotamento com
//! orçamento) é inspirado no gortex (<https://github.com/zzet/gortex>,
//! Apache 2.0, © 2024-2026 Andrey Kumanyaev). Nenhum trecho foi portado.
//!
//! Peças:
//! - [`lang`]: um [`LanguageAdapter`] por linguagem (Rust, TS/JS, Python)
//!   extrai os [`FileFacts`] de um arquivo.
//! - [`index`]: [`Index`] percorre o repo respeitando `.gitignore`, reparseia
//!   só o que mudou (hash) e persiste em `<repo>/.kax/context/`.
//! - [`resolve`]: liga cada [`Ref`] a uma definição pela heurística nome +
//!   hint + proximidade; empate vira [`Resolution::Ambiguous`].
//! - [`pack`]: monta o [`ContextPack`] dentro de um orçamento de tokens.
//!
//! **CONTRACT (P6a).** Tipos e assinaturas valem; a lógica chega na P6b
//! (adapters TS/JS e Python) e na P6c (adapter Rust, índice, resolução, pack).
//! Até lá, os pontos de entrada devolvem [`ContextError::NotImplemented`].

pub mod index;
pub mod lang;
pub mod pack;
pub mod resolve;

use serde::{Deserialize, Serialize};

pub use index::{Index, IndexStats, RefreshStats};
pub use lang::{LanguageAdapter, adapter_for, adapters};
pub use pack::{ContextPack, Excerpt, Fidelity, OmitReason, Omitted, PackRole};
pub use resolve::{Resolution, SymbolTable, Tier};

/// Arquivo acima disso não é indexado (gerado ou minificado, quase sempre).
pub const MAX_FILE_BYTES: u64 = 1 << 20;

/// Versão do formato persistido e da extração. Sobe quando um adapter muda o
/// que extrai: todas as entradas antigas são reparseadas.
pub const INDEX_FORMAT_VERSION: u32 = 1;

/// Erro do `kax-context`.
#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    /// Ponto de entrada do CONTRACT ainda sem lógica; o texto diz a etapa dona.
    #[error("kax-context: {0} ainda não implementado")]
    NotImplemented(&'static str),
    /// O arquivo não está no índice (fora da raiz, ignorado ou sem adapter).
    #[error("arquivo fora do índice: {0}")]
    UnknownFile(String),
    /// Nenhuma definição casa com a query.
    #[error("símbolo não encontrado: {0}")]
    UnknownSymbol(String),
    /// A query de um alvo do `pack` casa com mais de uma definição; o lead
    /// precisa qualificar (`Tipo::nome` ou `<arquivo>::<qual_name>`).
    #[error("alvo ambíguo {query:?}: {} candidatos", candidates.len())]
    AmbiguousTarget {
        query: String,
        candidates: Vec<SymbolId>,
    },
    /// O corpo dos alvos sozinho passa do orçamento.
    #[error("orçamento de {budget} tokens não cabe o corpo dos alvos ({needed} tokens)")]
    BudgetTooSmall { needed: u32, budget: u32 },
    /// Path absoluto, com `..` ou que escapa da raiz por symlink.
    #[error("path fora da raiz do repo: {0}")]
    OutOfRoot(String),
    #[error("E/S em {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// `index.json` ilegível ou de outra versão de formato.
    #[error("índice persistido inválido em {path}: {reason}")]
    Store { path: String, reason: String },
}

/// Identificador estável: `"<arquivo>::<qual_name>"`. Quando dois símbolos do
/// mesmo arquivo colidiriam (ex.: dois `impl S` com o mesmo método), o 1º fica
/// sem sufixo e os seguintes ganham `"#<linha inicial>"`. `<arquivo>` é
/// relativo à raiz, com `/`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SymbolId(pub String);

impl SymbolId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SymbolId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Trecho do texto normalizado (CRLF → LF). Bytes 0-based com fim exclusivo;
/// linhas 1-based e inclusivas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Span {
    pub start_byte: u32,
    pub end_byte: u32,
    pub start_line: u32,
    pub end_line: u32,
}

impl Span {
    /// `[início, fim]` em linhas, como `files[].lines` e o `ContextPack`.
    pub fn lines(&self) -> [u32; 2] {
        [self.start_line, self.end_line]
    }
}

/// Espécie de símbolo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Fn,
    Method,
    Struct,
    Enum,
    Trait,
    Impl,
    /// Alias de tipo (`type X = …`, `type X = …` em TS).
    Type,
    /// `const`/`static` em Rust.
    Const,
    Class,
    Interface,
    /// `mod x { … }` inline (Rust) e `namespace` (TS).
    Module,
}

/// Definição extraída de um arquivo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    pub id: SymbolId,
    /// Nome curto (`new`). Num `impl`, o cabeçalho inteiro (`impl Shape for
    /// Rect`), para uma query `Rect` não casar com os impls.
    pub name: String,
    /// Contêineres dentro do módulo + nome, com o separador da linguagem:
    /// `Rect::new`, `impl Shape for Rect`, `Rect.square`.
    pub qual_name: String,
    pub kind: SymbolKind,
    /// Relativo à raiz, com `/`.
    pub file: String,
    /// Item inteiro, com atributos/decorators e sem a doc.
    pub span: Span,
    /// Do 1º token do item (sem atributos/decorators, com `export` e
    /// visibilidade) até antes do corpo, sem o delimitador (`{`, o `:` do
    /// Python, o `;` final). Sem corpo (`const`, `type`, método declarado num
    /// trait ou interface), o item todo menos o `;`.
    pub signature_span: Span,
    /// Comentários de doc (`///`, `/** */`, JSDoc) ou docstring Python, que
    /// fica dentro do `span`.
    pub doc_span: Option<Span>,
    /// Contêiner direto: o `impl`/trait de um método, a classe, o `mod` inline.
    pub parent: Option<SymbolId>,
}

/// Espécie de referência.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefKind {
    /// Chamada (função, método, construtor).
    Call,
    /// Uso de tipo (anotação, parâmetro, retorno, `impl X for Y`, herança).
    Type,
    /// `use`/`import` que traz o nome.
    Use,
}

/// Sítio de referência, ainda sem resolver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ref {
    /// Último identificador (`validate` em `crate::geometry::validate(..)`).
    pub name: String,
    /// Qualificador como escrito antes do nome, sem o nome: `Rect` em
    /// `Rect::new`, `crate::geometry`, `self`, `Self`, `this`; num `use` ou
    /// `import`, o módulo de origem (`crate::geometry`, `./shapes`). `None` em
    /// nome solto e em método chamado sobre outra expressão.
    pub qual_hint: Option<String>,
    /// `true` em `recv.nome(...)`: só métodos são candidatos.
    pub method: bool,
    pub file: String,
    /// Span do nome no sítio.
    pub span: Span,
    /// Símbolo mais interno cujo `span` contém o sítio (o método, a fn, o
    /// struct de um campo); `None` no nível do arquivo.
    pub enclosing: Option<SymbolId>,
    pub kind: RefKind,
}

/// Import de um arquivo, um por nome trazido (`use a::{b, c as d}` vira dois).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Import {
    /// Módulo de origem como escrito: `crate::geometry`, `./shapes`, `.shapes`.
    pub source: String,
    /// Item trazido; `None` quando é o módulo inteiro (`import pkg.mod`).
    pub name: Option<String>,
    /// Nome local quando renomeado (`as d`).
    pub alias: Option<String>,
    /// `use a::*`, `from m import *`, `export * from`.
    pub glob: bool,
    pub file: String,
    pub span: Span,
}

/// Tudo o que um adapter extrai de um arquivo.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFacts {
    /// Caminho do módulo do arquivo: `["store"]` para `src/store.rs`, `[]` para
    /// `src/lib.rs`, `["pkg", "shapes"]` para `pkg/shapes.py`, `["src",
    /// "main"]` para `src/main.ts`.
    pub module: Vec<String>,
    /// Em ordem do fonte.
    pub symbols: Vec<Symbol>,
    pub refs: Vec<Ref>,
    pub imports: Vec<Import>,
    /// Nós `ERROR`/`MISSING` do tree-sitter: com erro, os fatos são parciais.
    pub parse_errors: u32,
}

/// Linha do `outline`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolSummary {
    pub id: SymbolId,
    pub name: String,
    pub qual_name: String,
    pub kind: SymbolKind,
    /// `[início, fim]` do `span`.
    pub lines: [u32; 2],
    /// Texto do `signature_span`.
    pub signature: String,
    pub parent: Option<SymbolId>,
}

/// Resposta do `symbol(query)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolDef {
    pub symbol: Symbol,
    /// Texto do `signature_span`.
    pub signature: String,
    /// Texto do `doc_span`, sem a quebra de linha final.
    pub doc: Option<String>,
    /// `true` quando a query casou com mais de uma definição.
    pub ambiguous: bool,
}

/// Resposta do `refs(query)` e do `callers(query)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefSite {
    pub file: String,
    /// `[início, fim]` do nome no sítio.
    pub lines: [u32; 2],
    pub kind: RefKind,
    /// Quem contém o sítio (para `callers`, o chamador).
    pub enclosing: Option<SymbolSummary>,
    /// Como o sítio foi ligado à definição pedida. `Ambiguous` aparece quando
    /// a definição pedida está entre os candidatos empatados.
    pub resolution: Resolution,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_use_snake_case_on_the_wire() {
        let v = serde_json::to_value([SymbolKind::Fn, SymbolKind::Impl, SymbolKind::Interface])
            .unwrap();
        assert_eq!(v, serde_json::json!(["fn", "impl", "interface"]));
        let v = serde_json::to_value([RefKind::Call, RefKind::Type, RefKind::Use]).unwrap();
        assert_eq!(v, serde_json::json!(["call", "type", "use"]));
    }

    #[test]
    fn symbol_id_is_a_plain_string() {
        let id = SymbolId("src/geometry.rs::Rect::new".into());
        assert_eq!(
            serde_json::to_value(&id).unwrap(),
            serde_json::json!("src/geometry.rs::Rect::new")
        );
        assert_eq!(id.to_string(), id.as_str());
    }

    #[test]
    fn facts_round_trip() {
        let span = Span {
            start_byte: 0,
            end_byte: 10,
            start_line: 1,
            end_line: 1,
        };
        let facts = FileFacts {
            module: vec!["store".into()],
            symbols: vec![Symbol {
                id: SymbolId("src/store.rs::validate".into()),
                name: "validate".into(),
                qual_name: "validate".into(),
                kind: SymbolKind::Fn,
                file: "src/store.rs".into(),
                span,
                signature_span: span,
                doc_span: None,
                parent: None,
            }],
            refs: vec![Ref {
                name: "validate".into(),
                qual_hint: Some("crate::geometry".into()),
                method: false,
                file: "src/store.rs".into(),
                span,
                enclosing: None,
                kind: RefKind::Call,
            }],
            imports: vec![],
            parse_errors: 0,
        };
        let text = serde_json::to_string(&facts).unwrap();
        assert_eq!(serde_json::from_str::<FileFacts>(&text).unwrap(), facts);
        assert_eq!(span.lines(), [1, 1]);
    }
}
