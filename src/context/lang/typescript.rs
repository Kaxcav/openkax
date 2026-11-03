//! Adapters TypeScript e JavaScript. Lógica de extração: P6b.
//!
//! O que a P6b extrai (ADR 0009, testes em `tests/context_ts_py.rs`):
//! - símbolos: `function`, `class` e métodos (`Rect.area`), `interface` e as
//!   assinaturas de método dela, `type`, arrow function atribuída a `const`,
//!   `namespace`;
//! - refs: chamadas (`this.f()` com `qual_hint = "this"`, `new X()` como
//!   chamada de `X`), tipos usados (`implements`, `extends`, anotações) e cada
//!   nome importado;
//! - imports: ESM (`import { a as b } from "./m"`, `import * as m`,
//!   `export … from`) e `require` com destino literal;
//! - `module`: o caminho sem extensão (`src/main.ts` → `["src", "main"]`);
//!   `index.ts` vale pelo diretório.
//!
//! O `.tsx` usa a gramática TSX; os outros `.ts` usam a TS, porque a TSX lê
//! `<T>x` como JSX. JavaScript (com JSX) usa `tree-sitter-javascript`.

use std::path::Path;

use super::LanguageAdapter;
use crate::context::FileFacts;

/// TypeScript via `tree-sitter-typescript`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TypeScriptAdapter;

impl TypeScriptAdapter {
    /// Gramática para `path`: TSX no `.tsx`, TS nos demais.
    pub fn grammar(&self, path: &Path) -> tree_sitter::Language {
        let tsx = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("tsx"));
        if tsx {
            tree_sitter_typescript::LANGUAGE_TSX.into()
        } else {
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
        }
    }
}

impl LanguageAdapter for TypeScriptAdapter {
    fn id(&self) -> &'static str {
        "typescript"
    }

    fn extensions(&self) -> &[&str] {
        &["ts", "tsx", "mts", "cts"]
    }

    /// CONTRACT (P6a): ainda sem extração; devolve fatos vazios.
    fn extract(&self, _path: &Path, _src: &str) -> FileFacts {
        FileFacts::default()
    }
}

/// JavaScript (com JSX) via `tree-sitter-javascript`.
#[derive(Debug, Clone, Copy, Default)]
pub struct JavaScriptAdapter;

impl JavaScriptAdapter {
    /// Gramática do tree-sitter usada pelo adapter.
    pub fn grammar(&self) -> tree_sitter::Language {
        tree_sitter_javascript::LANGUAGE.into()
    }
}

impl LanguageAdapter for JavaScriptAdapter {
    fn id(&self) -> &'static str {
        "javascript"
    }

    fn extensions(&self) -> &[&str] {
        &["js", "jsx", "mjs", "cjs"]
    }

    /// CONTRACT (P6a): ainda sem extração; devolve fatos vazios.
    fn extract(&self, _path: &Path, _src: &str) -> FileFacts {
        FileFacts::default()
    }
}
