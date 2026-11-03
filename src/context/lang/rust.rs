//! Adapter Rust (`.rs`). Lógica de extração: P6c.
//!
//! O que a P6c extrai (ADR 0009, testes em `tests/context_rust.rs`):
//! - símbolos: `fn`, métodos de `impl` (qualificados pelo tipo) e de `trait`
//!   (pelo trait), `struct`, `enum`, `trait`, `impl` (`impl Tipo`, `impl Trait
//!   for Tipo`), `type`, `const`/`static`, `mod` inline;
//! - refs: chamadas (com `qual_hint` do caminho e `method` em `recv.f()`,
//!   inclusive turbofish), tipos usados e cada nome de `use`;
//! - imports: um por nome de `use`, com `as` e `*`;
//! - `module`: `src/lib.rs`/`src/main.rs` → `[]`, `src/a/mod.rs` → `["a"]`,
//!   `src/a/b.rs` → `["a", "b"]` (sai o caminho até o último `src/`; fora de
//!   um `src/`, vale o caminho sem extensão);
//! - não são símbolos: `mod x;` (só liga arquivos) e campos de struct.

use std::path::Path;

use super::LanguageAdapter;
use crate::context::FileFacts;

/// Rust via `tree-sitter-rust`.
#[derive(Debug, Clone, Copy, Default)]
pub struct RustAdapter;

impl RustAdapter {
    /// Gramática do tree-sitter usada pelo adapter.
    pub fn grammar(&self) -> tree_sitter::Language {
        tree_sitter_rust::LANGUAGE.into()
    }
}

impl LanguageAdapter for RustAdapter {
    fn id(&self) -> &'static str {
        "rust"
    }

    fn extensions(&self) -> &[&str] {
        &["rs"]
    }

    /// CONTRACT (P6a): ainda sem extração; devolve fatos vazios.
    fn extract(&self, _path: &Path, _src: &str) -> FileFacts {
        FileFacts::default()
    }
}
