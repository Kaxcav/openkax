//! Adapter Python (`.py`). Lógica de extração: P6b.
//!
//! O que a P6b extrai (ADR 0009, testes em `tests/context_ts_py.rs`):
//! - símbolos: `def` livre (`fn`), `class` e métodos (`Rect.area`, com
//!   decorators no `span`); a docstring vira `doc_span` dentro do `span`;
//! - refs: chamadas (`self.f()` com `qual_hint = "self"`, `Rect.square()` com
//!   `qual_hint = "Rect"`, `Rect()` como chamada da classe), tipos de
//!   anotação e herança, e cada nome importado;
//! - imports: `import a.b as c`, `from a.b import x as y`, relativos
//!   (`from .shapes import Rect`, `source = ".shapes"`) e `*`;
//! - `module`: `pkg/shapes.py` → `["pkg", "shapes"]`, `pkg/__init__.py` →
//!   `["pkg"]`.

use std::path::Path;

use super::LanguageAdapter;
use crate::context::FileFacts;

/// Python via `tree-sitter-python`.
#[derive(Debug, Clone, Copy, Default)]
pub struct PythonAdapter;

impl PythonAdapter {
    /// Gramática do tree-sitter usada pelo adapter.
    pub fn grammar(&self) -> tree_sitter::Language {
        tree_sitter_python::LANGUAGE.into()
    }
}

impl LanguageAdapter for PythonAdapter {
    fn id(&self) -> &'static str {
        "python"
    }

    fn extensions(&self) -> &[&str] {
        &["py"]
    }

    /// CONTRACT (P6a): ainda sem extração; devolve fatos vazios.
    fn extract(&self, _path: &Path, _src: &str) -> FileFacts {
        FileFacts::default()
    }
}
