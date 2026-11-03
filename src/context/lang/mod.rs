//! Adapters de linguagem: cada um transforma o texto de um arquivo em
//! [`FileFacts`] via tree-sitter (ADR 0009).
//!
//! Recomendação para quem implementa (P6b/P6c), tirada do gortex: uma query
//! tree-sitter só por arquivo, alternando os padrões e despachando pelo nome
//! da captura; o dono de um método (impl/trait/class) sai subindo pelos pais
//! do nó; a função envolvente de um sítio, pela faixa de linhas.

mod python;
mod rust;
pub(crate) mod support;
mod typescript;

use std::path::Path;

use super::FileFacts;

pub use python::PythonAdapter;
pub use rust::RustAdapter;
pub use typescript::{JavaScriptAdapter, TypeScriptAdapter};

/// Extrator de uma linguagem. Sem estado: o mesmo adapter atende qualquer
/// arquivo, de qualquer thread.
pub trait LanguageAdapter: Send + Sync {
    /// Identificador estável (`"rust"`); entra na chave de invalidação.
    fn id(&self) -> &'static str;
    /// Extensões atendidas, sem ponto e em minúsculas (`["rs"]`).
    fn extensions(&self) -> &[&str];
    /// Extrai os fatos de `src`.
    ///
    /// `path` é relativo à raiz, com `/`, e vai para os campos `file` e para
    /// o cálculo de `FileFacts::module`. `src` já vem normalizado (CRLF → LF).
    /// Nunca falha: erro de sintaxe vira `parse_errors` e fatos parciais.
    fn extract(&self, path: &Path, src: &str) -> FileFacts;
}

static ADAPTERS: [&dyn LanguageAdapter; 4] = [
    &RustAdapter,
    &TypeScriptAdapter,
    &JavaScriptAdapter,
    &PythonAdapter,
];

/// Todos os adapters registrados.
pub fn adapters() -> &'static [&'static dyn LanguageAdapter] {
    &ADAPTERS
}

/// Adapter da extensão de `path` (sem diferenciar maiúsculas); `None` quando
/// nenhuma linguagem atende o arquivo.
pub fn adapter_for(path: &Path) -> Option<&'static dyn LanguageAdapter> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    ADAPTERS
        .iter()
        .copied()
        .find(|a| a.extensions().contains(&ext.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_pick_the_adapter() {
        for (path, id) in [
            ("src/lib.rs", Some("rust")),
            ("src/main.ts", Some("typescript")),
            ("web/App.TSX", Some("typescript")),
            ("lib/esm.mts", Some("typescript")),
            ("lib/cjs.cts", Some("typescript")),
            ("src/legacy.js", Some("javascript")),
            ("src/view.jsx", Some("javascript")),
            ("src/esm.mjs", Some("javascript")),
            ("src/cjs.cjs", Some("javascript")),
            ("pkg/shapes.py", Some("python")),
            ("README.md", None),
            ("Makefile", None),
            ("src/types.d", None),
        ] {
            assert_eq!(adapter_for(Path::new(path)).map(|a| a.id()), id, "{path}");
        }
    }

    #[test]
    fn ids_and_extensions_are_unique() {
        let mut ids: Vec<&str> = adapters().iter().map(|a| a.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), adapters().len());
        let mut exts: Vec<&str> = adapters()
            .iter()
            .flat_map(|a| a.extensions())
            .copied()
            .collect();
        let n = exts.len();
        exts.sort_unstable();
        exts.dedup();
        assert_eq!(exts.len(), n, "extensão atendida por dois adapters");
        assert!(
            exts.iter()
                .all(|e| !e.starts_with('.') && *e == e.to_ascii_lowercase())
        );
    }

    /// As gramáticas compilam (C via `cc`) e carregam no tree-sitter desta
    /// versão: ABI compatível em Windows/MSVC e Linux (ADR 0009).
    #[test]
    fn grammars_load() {
        let cases: [(tree_sitter::Language, &str); 5] = [
            (RustAdapter.grammar(), "pub fn f(x: u8) -> u8 { x }\n"),
            (
                TypeScriptAdapter.grammar(Path::new("a.ts")),
                "export function f(x: number): number { return <number>x; }\n",
            ),
            (
                TypeScriptAdapter.grammar(Path::new("a.tsx")),
                "export const A = () => <div>{1}</div>;\n",
            ),
            (
                JavaScriptAdapter.grammar(),
                "export const f = (x) => <b>{x}</b>;\n",
            ),
            (
                PythonAdapter.grammar(),
                "def f(x: int) -> int:\n    return x\n",
            ),
        ];
        for (lang, src) in cases {
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(&lang).expect("ABI da gramática");
            let tree = parser.parse(src, None).expect("parse");
            assert!(!tree.root_node().has_error(), "{src}");
        }
    }
}
