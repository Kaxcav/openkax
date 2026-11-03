//! Adapter Rust (`.rs`), ADR 0009 (P6c).
//!
//! O que extrai (testes em `tests/context_rust.rs`):
//! - símbolos: `fn`, métodos de `impl` (qualificados pelo tipo) e de `trait`
//!   (pelo trait), `struct`/`union`, `enum`, `trait`, `impl` (`impl Tipo`,
//!   `impl Trait for Tipo`), `type` (inclusive o associado), `const`/`static`,
//!   `mod` inline;
//! - refs: chamadas (com `qual_hint` do caminho e `method` em `recv.f()`,
//!   inclusive turbofish; `self.f()` leva `qual_hint = "self"`), tipos usados
//!   (anotação, campo, retorno, cabeçalho do `impl`, bound, `Tipo { .. }`) e
//!   cada nome de `use`;
//! - imports: um por nome de `use`, com `as` e `*`; `use a::{self}` vira o
//!   módulo `a` trazido de `a`'s pai;
//! - `module`: `src/lib.rs`/`src/main.rs` → `[]`, `src/a/mod.rs` → `["a"]`,
//!   `src/a/b.rs` → `["a", "b"]` (sai o caminho até o último `src/`; fora de
//!   um `src/`, vale o caminho sem extensão);
//! - não são símbolos: `mod x;` (só liga arquivos), campos de struct,
//!   variantes de enum e `macro_rules!`.
//!
//! O percurso é iterativo (pilha explícita): um arquivo com expressão muito
//! aninhada não estoura a pilha da thread. Macros não são expandidas: o que
//! está dentro de `foo!(...)` não gera ref.

use std::path::Path;

use tree_sitter::{Node, Parser};

use super::LanguageAdapter;
use super::support::{Ids, LineIndex, count_errors, path_str};
use crate::context::{FileFacts, Import, Ref, RefKind, Span, Symbol, SymbolId, SymbolKind};

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

    fn extract(&self, path: &Path, src: &str) -> FileFacts {
        let file = path_str(path);
        let module = module_of(&file);
        let mut parser = Parser::new();
        let tree = match parser.set_language(&self.grammar()) {
            Ok(()) => parser.parse(src, None),
            Err(_) => None,
        };
        let Some(tree) = tree else {
            return FileFacts {
                module,
                ..FileFacts::default()
            };
        };
        let root = tree.root_node();
        let mut x = Extractor {
            src,
            file: &file,
            lines: LineIndex::new(src),
            ids: Ids::default(),
            facts: FileFacts::default(),
            scopes: vec![Scope::default()],
            stack: vec![(root, 0)],
        };
        while let Some((node, scope)) = x.stack.pop() {
            x.visit(node, scope);
        }
        let mut facts = x.facts;
        facts.module = module;
        facts.parse_errors = count_errors(root);
        facts
    }
}

/// Caminho do módulo de um arquivo `.rs` (regra no topo do módulo).
fn module_of(file: &str) -> Vec<String> {
    let stem = match file.rsplit_once('.') {
        Some((s, ext)) if ext.eq_ignore_ascii_case("rs") => s,
        _ => file,
    };
    let parts: Vec<&str> = stem.split('/').filter(|p| !p.is_empty()).collect();
    let dirs = &parts[..parts.len().saturating_sub(1)];
    let from = dirs.iter().rposition(|p| *p == "src").map_or(0, |i| i + 1);
    let mut segs: Vec<String> = parts[from..].iter().map(|s| (*s).to_string()).collect();
    if segs.last().is_some_and(|l| l == "mod") {
        segs.pop();
    }
    if from > 0 && segs.len() == 1 && (segs[0] == "lib" || segs[0] == "main") {
        segs.clear();
    }
    segs
}

/// Contexto léxico de um nó.
#[derive(Debug, Clone, Default)]
struct Scope {
    /// `mod` inline em volta (`inner` em `mod inner { .. }`).
    mods: Vec<String>,
    /// Contêiner direto dos itens daqui (`Symbol::parent`).
    parent: Option<SymbolId>,
    /// Tipo do `impl` ou nome do `trait`: qualifica métodos e associados.
    owner: Option<String>,
    /// Símbolo mais interno em volta (`Ref::enclosing`).
    enclosing: Option<SymbolId>,
}

/// Item cujo `name` é definição, não uso de tipo.
const DEFINES_NAME: [&str; 7] = [
    "struct_item",
    "enum_item",
    "union_item",
    "trait_item",
    "type_item",
    "associated_type",
    "type_parameter",
];

struct Extractor<'s, 't> {
    src: &'s str,
    file: &'s str,
    lines: LineIndex,
    ids: Ids,
    facts: FileFacts,
    /// Arena de escopos; a pilha guarda o índice.
    scopes: Vec<Scope>,
    stack: Vec<(Node<'t>, usize)>,
}

impl<'t> Extractor<'_, 't> {
    fn text(&self, n: Node<'_>) -> &str {
        &self.src[n.byte_range()]
    }

    fn visit(&mut self, n: Node<'t>, si: usize) {
        match n.kind() {
            "function_item" | "function_signature_item" => self.function(n, si),
            "struct_item" | "union_item" => self.item(n, si, SymbolKind::Struct, false),
            "enum_item" => self.item(n, si, SymbolKind::Enum, false),
            "type_item" | "associated_type" => self.item(n, si, SymbolKind::Type, false),
            "const_item" | "static_item" => self.item(n, si, SymbolKind::Const, false),
            "trait_item" => self.item(n, si, SymbolKind::Trait, true),
            "impl_item" => self.impl_item(n, si),
            "mod_item" => self.mod_item(n, si),
            "use_declaration" => {
                if let Some(arg) = n.child_by_field_name("argument") {
                    self.use_tree(arg, &[], si);
                }
            }
            "call_expression" => {
                if let Some(f) = n.child_by_field_name("function") {
                    self.callee(f, si);
                }
                self.descend(n, si);
            }
            "type_identifier" => self.type_identifier(n, si),
            "scoped_type_identifier" => {
                if let Some(name) = n.child_by_field_name("name") {
                    let hint = n
                        .child_by_field_name("path")
                        .map(|p| self.path_segs(p).join("::"));
                    let text = self.text(name).to_string();
                    self.add_ref(name, text, hint, false, RefKind::Type, si);
                }
                // Argumentos genéricos dentro do caminho (`a::B<C>::D`).
                if let Some(path) = n.child_by_field_name("path") {
                    self.stack.push((path, si));
                }
            }
            "attribute_item"
            | "inner_attribute_item"
            | "line_comment"
            | "block_comment"
            | "token_tree"
            | "macro_definition" => {}
            _ => self.descend(n, si),
        }
    }

    /// Empilha os filhos para saírem em ordem do fonte.
    fn descend(&mut self, n: Node<'t>, si: usize) {
        let from = self.stack.len();
        let mut c = n.walk();
        self.stack.extend(n.children(&mut c).map(|ch| (ch, si)));
        self.stack[from..].reverse();
    }

    fn push_scope(&mut self, s: Scope) -> usize {
        self.scopes.push(s);
        self.scopes.len() - 1
    }

    fn function(&mut self, n: Node<'t>, si: usize) {
        let Some(name_n) = n.child_by_field_name("name") else {
            return self.descend(n, si);
        };
        let name = self.text(name_n).to_string();
        let sc = self.scopes[si].clone();
        let kind = if sc.owner.is_some() {
            SymbolKind::Method
        } else {
            SymbolKind::Fn
        };
        let body = n.child_by_field_name("body");
        let sig = self.signature(
            n.start_byte(),
            body.map_or(n.end_byte(), |b| b.start_byte()),
        );
        let qual = qual(&sc, &name);
        let id = self.add_symbol(n, &sc, name, qual, kind, sig);
        // Itens aninhados no corpo valem pelo módulo, sem o dono.
        let inner = self.push_scope(Scope {
            mods: sc.mods,
            parent: sc.parent,
            owner: None,
            enclosing: Some(id),
        });
        self.descend(n, inner);
    }

    /// `struct`, `enum`, `trait`, `type`, `const`/`static`. Um `trait` vira
    /// dono dos métodos declarados nele.
    fn item(&mut self, n: Node<'t>, si: usize, kind: SymbolKind, owns: bool) {
        let Some(name_n) = n.child_by_field_name("name") else {
            return self.descend(n, si);
        };
        let name = self.text(name_n).to_string();
        let sc = self.scopes[si].clone();
        let body = n.child_by_field_name("body");
        let sig = self.signature(
            n.start_byte(),
            body.map_or(n.end_byte(), |b| b.start_byte()),
        );
        let qual = qual(&sc, &name);
        let id = self.add_symbol(n, &sc, name.clone(), qual, kind, sig);
        let inner = if owns {
            Scope {
                mods: sc.mods,
                parent: Some(id.clone()),
                owner: Some(name),
                enclosing: Some(id),
            }
        } else {
            Scope {
                enclosing: Some(id),
                ..sc
            }
        };
        let inner = self.push_scope(inner);
        self.descend(n, inner);
    }

    fn impl_item(&mut self, n: Node<'t>, si: usize) {
        let Some(ty) = n.child_by_field_name("type") else {
            return self.descend(n, si);
        };
        let ty_text = collapse(self.text(ty));
        let name = match n.child_by_field_name("trait") {
            Some(t) => {
                let mut c = n.walk();
                let neg = if n.children(&mut c).any(|ch| ch.kind() == "!") {
                    "!"
                } else {
                    ""
                };
                format!("impl {neg}{} for {ty_text}", collapse(self.text(t)))
            }
            None => format!("impl {ty_text}"),
        };
        let owner = self.base_type(ty);
        let sc = self.scopes[si].clone();
        let body = n.child_by_field_name("body");
        let sig = self.signature(
            n.start_byte(),
            body.map_or(n.end_byte(), |b| b.start_byte()),
        );
        let qual = qual(&sc, &name);
        let id = self.add_symbol(n, &sc, name, qual, SymbolKind::Impl, sig);
        let inner = self.push_scope(Scope {
            mods: sc.mods,
            parent: Some(id.clone()),
            owner: Some(owner),
            enclosing: Some(id),
        });
        self.descend(n, inner);
    }

    fn mod_item(&mut self, n: Node<'t>, si: usize) {
        // `mod x;` só liga arquivos.
        let (Some(name_n), Some(body)) =
            (n.child_by_field_name("name"), n.child_by_field_name("body"))
        else {
            return;
        };
        let name = self.text(name_n).to_string();
        let sc = self.scopes[si].clone();
        let sig = self.signature(n.start_byte(), body.start_byte());
        let qual = qual(&sc, &name);
        let id = self.add_symbol(n, &sc, name.clone(), qual, SymbolKind::Module, sig);
        let mut mods = sc.mods;
        mods.push(name);
        let inner = self.push_scope(Scope {
            mods,
            parent: Some(id.clone()),
            owner: None,
            enclosing: Some(id),
        });
        self.descend(body, inner);
    }

    fn add_symbol(
        &mut self,
        n: Node<'_>,
        sc: &Scope,
        name: String,
        qual: String,
        kind: SymbolKind,
        signature_span: Span,
    ) -> SymbolId {
        let (start, doc) = leading(n);
        let span = self.lines.span(start, n.end_byte());
        let id = self.ids.make(self.file, &qual, span.start_line);
        self.facts.symbols.push(Symbol {
            id: id.clone(),
            name,
            qual_name: qual,
            kind,
            file: self.file.to_string(),
            span,
            signature_span,
            doc_span: doc.map(|(a, b)| self.lines.span(a, b)),
            parent: sc.parent.clone(),
        });
        id
    }

    /// De `start` até `end`, sem o espaço final e sem um `;` final.
    fn signature(&self, start: usize, end: usize) -> Span {
        let mut t = self.src[start..end].trim_end();
        if let Some(x) = t.strip_suffix(';') {
            t = x.trim_end();
        }
        self.lines.span(start, start + t.len())
    }

    fn add_ref(
        &mut self,
        at: Node<'_>,
        name: String,
        qual_hint: Option<String>,
        method: bool,
        kind: RefKind,
        si: usize,
    ) {
        self.facts.refs.push(Ref {
            name,
            qual_hint,
            method,
            file: self.file.to_string(),
            span: self.lines.span(at.start_byte(), at.end_byte()),
            enclosing: self.scopes[si].enclosing.clone(),
            kind,
        });
    }

    fn callee(&mut self, f: Node<'_>, si: usize) {
        match f.kind() {
            "identifier" => {
                let name = self.text(f).to_string();
                self.add_ref(f, name, None, false, RefKind::Call, si);
            }
            "scoped_identifier" => {
                let Some(name_n) = f.child_by_field_name("name") else {
                    return;
                };
                if name_n.kind() != "identifier" {
                    return;
                }
                // `::x::f()` é de crate externa: sem caminho, sem ref.
                let Some(path) = f.child_by_field_name("path") else {
                    return;
                };
                let hint = self.path_segs(path).join("::");
                let name = self.text(name_n).to_string();
                self.add_ref(name_n, name, Some(hint), false, RefKind::Call, si);
            }
            "field_expression" => {
                let Some(field) = f.child_by_field_name("field") else {
                    return;
                };
                if field.kind() != "field_identifier" {
                    return;
                }
                let hint = f
                    .child_by_field_name("value")
                    .filter(|v| v.kind() == "self")
                    .map(|_| "self".to_string());
                let name = self.text(field).to_string();
                self.add_ref(field, name, hint, true, RefKind::Call, si);
            }
            "generic_function" => {
                if let Some(inner) = f.child_by_field_name("function") {
                    self.callee(inner, si);
                }
            }
            _ => {}
        }
    }

    fn type_identifier(&mut self, n: Node<'_>, si: usize) {
        let text = self.text(n);
        if text == "Self" {
            return;
        }
        if let Some(p) = n.parent()
            && DEFINES_NAME.contains(&p.kind())
            && p.child_by_field_name("name") == Some(n)
        {
            return;
        }
        let name = text.to_string();
        self.add_ref(n, name, None, false, RefKind::Type, si);
    }

    /// Um `use` vira um `Import` e uma ref `use` por nome trazido.
    fn use_tree(&mut self, n: Node<'_>, prefix: &[String], si: usize) {
        match n.kind() {
            "use_list" => {
                let mut c = n.walk();
                let items: Vec<Node<'_>> = n.named_children(&mut c).collect();
                for item in items {
                    self.use_tree(item, prefix, si);
                }
            }
            "scoped_use_list" => {
                let mut p = prefix.to_vec();
                if let Some(path) = n.child_by_field_name("path") {
                    p.extend(self.path_segs(path));
                }
                if let Some(list) = n.child_by_field_name("list") {
                    self.use_tree(list, &p, si);
                }
            }
            "use_wildcard" => {
                let mut p = prefix.to_vec();
                if let Some(path) = n.named_child(0) {
                    p.extend(self.path_segs(path));
                }
                self.facts.imports.push(Import {
                    source: p.join("::"),
                    name: None,
                    alias: None,
                    glob: true,
                    file: self.file.to_string(),
                    span: self.lines.span(n.start_byte(), n.end_byte()),
                });
            }
            "use_as_clause" => {
                if let Some(path) = n.child_by_field_name("path") {
                    let alias = n
                        .child_by_field_name("alias")
                        .map(|a| self.text(a).to_string());
                    self.use_item(n, path, alias, prefix, si);
                }
            }
            _ => self.use_item(n, n, None, prefix, si),
        }
    }

    fn use_item(
        &mut self,
        whole: Node<'_>,
        path: Node<'_>,
        alias: Option<String>,
        prefix: &[String],
        si: usize,
    ) {
        let mut segs = prefix.to_vec();
        segs.extend(self.path_segs(path));
        let Some(last) = segs.pop() else {
            return;
        };
        // `use a::b::{self}` traz o módulo `b`, que vem de `a`.
        let name = if last == "self" {
            match segs.pop() {
                Some(m) => m,
                None => return,
            }
        } else {
            last
        };
        let source = segs.join("::");
        let at = if path.kind() == "scoped_identifier" {
            path.child_by_field_name("name").unwrap_or(path)
        } else {
            path
        };
        self.facts.imports.push(Import {
            source: source.clone(),
            name: Some(name.clone()),
            alias,
            glob: false,
            file: self.file.to_string(),
            span: self.lines.span(whole.start_byte(), whole.end_byte()),
        });
        let hint = (!source.is_empty()).then_some(source);
        self.add_ref(at, name, hint, false, RefKind::Use, si);
    }

    /// Segmentos de um caminho, sem argumentos genéricos.
    fn path_segs(&self, n: Node<'_>) -> Vec<String> {
        match n.kind() {
            "scoped_identifier" | "scoped_type_identifier" => {
                let mut v = n
                    .child_by_field_name("path")
                    .map(|p| self.path_segs(p))
                    .unwrap_or_default();
                if let Some(name) = n.child_by_field_name("name") {
                    v.push(self.text(name).to_string());
                }
                v
            }
            "generic_type" => n
                .child_by_field_name("type")
                .map(|t| self.path_segs(t))
                .unwrap_or_default(),
            _ => vec![collapse(self.text(n))],
        }
    }

    /// Nome do tipo de um `impl` para qualificar os métodos: `Rect` em
    /// `impl<T> a::Rect<T>` e em `impl Trait for &Rect`.
    fn base_type(&self, ty: Node<'_>) -> String {
        match ty.kind() {
            "type_identifier" => self.text(ty).to_string(),
            "scoped_type_identifier" => ty
                .child_by_field_name("name")
                .map_or_else(|| collapse(self.text(ty)), |n| self.text(n).to_string()),
            "generic_type" | "reference_type" | "pointer_type" => ty
                .child_by_field_name("type")
                .map_or_else(|| collapse(self.text(ty)), |t| self.base_type(t)),
            _ => collapse(self.text(ty)),
        }
    }
}

/// `mods::dono::nome`.
fn qual(sc: &Scope, name: &str) -> String {
    let mut q = String::new();
    for m in &sc.mods {
        q.push_str(m);
        q.push_str("::");
    }
    if let Some(o) = &sc.owner {
        q.push_str(o);
        q.push_str("::");
    }
    q.push_str(name);
    q
}

/// Início do item com os atributos que o precedem e o trecho da doc (`///`,
/// `/** */`) logo acima. Comentário comum interrompe a busca.
fn leading(n: Node<'_>) -> (usize, Option<(usize, usize)>) {
    let mut start = n.start_byte();
    let mut doc: Option<(usize, usize)> = None;
    let mut cur = n.prev_sibling();
    while let Some(s) = cur {
        match s.kind() {
            "attribute_item" => start = s.start_byte(),
            "line_comment" | "block_comment" if s.child_by_field_name("outer").is_some() => {
                doc = Some((s.start_byte(), doc.map_or(s.end_byte(), |d| d.1)));
            }
            _ => break,
        }
        cur = s.prev_sibling();
    }
    (start, doc)
}

/// Espaços em sequência viram um só.
fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_paths() {
        for (file, want) in [
            ("src/lib.rs", vec![]),
            ("src/main.rs", vec![]),
            ("src/store.rs", vec!["store"]),
            ("src/a/mod.rs", vec!["a"]),
            ("src/a/b.rs", vec!["a", "b"]),
            ("crates/x/src/lib.rs", vec![]),
            ("crates/x/src/y.rs", vec!["y"]),
            ("tests/context_rust.rs", vec!["tests", "context_rust"]),
            ("build.rs", vec!["build"]),
            ("a/src.rs", vec!["a", "src"]),
            ("src/bin/tool.rs", vec!["bin", "tool"]),
        ] {
            assert_eq!(module_of(file), want, "{file}");
        }
    }

    #[test]
    fn use_trees_become_one_import_per_name() {
        let src = "use a::b::{self, c as d, e::*};\nuse f;\nuse g::h::i;\n";
        let facts = RustAdapter.extract(Path::new("src/x.rs"), src);
        let got: Vec<(&str, Option<&str>, Option<&str>, bool)> = facts
            .imports
            .iter()
            .map(|i| {
                (
                    i.source.as_str(),
                    i.name.as_deref(),
                    i.alias.as_deref(),
                    i.glob,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("a", Some("b"), None, false),
                ("a::b", Some("c"), Some("d"), false),
                ("a::b::e", None, None, true),
                ("", Some("f"), None, false),
                ("g::h", Some("i"), None, false),
            ]
        );
        let uses: Vec<(&str, Option<&str>)> = facts
            .refs
            .iter()
            .map(|r| (r.name.as_str(), r.qual_hint.as_deref()))
            .collect();
        assert_eq!(
            uses,
            [
                ("b", Some("a")),
                ("c", Some("a::b")),
                ("f", None),
                ("i", Some("g::h"))
            ]
        );
    }

    #[test]
    fn calls_types_and_generic_impls() {
        let src = "impl<T: Shape> Wrap<T> {\n    pub const N: u8 = 1;\n    fn go(&self) -> Vec<T> {\n        self.run();\n        a::b::<u8>(Foo { x: 1 });\n        x.collect::<Vec<_>>()\n    }\n}\n";
        let facts = RustAdapter.extract(Path::new("src/w.rs"), src);
        let quals: Vec<&str> = facts.symbols.iter().map(|s| s.qual_name.as_str()).collect();
        assert_eq!(quals, ["impl Wrap<T>", "Wrap::N", "Wrap::go"]);
        assert_eq!(facts.symbols[1].kind, SymbolKind::Const);
        let refs: Vec<(&str, Option<&str>, bool, RefKind)> = facts
            .refs
            .iter()
            .map(|r| (r.name.as_str(), r.qual_hint.as_deref(), r.method, r.kind))
            .collect();
        for want in [
            ("Shape", None, false, RefKind::Type),
            ("Wrap", None, false, RefKind::Type),
            ("run", Some("self"), true, RefKind::Call),
            ("b", Some("a"), false, RefKind::Call),
            ("Foo", None, false, RefKind::Type),
            ("collect", None, true, RefKind::Call),
        ] {
            assert!(refs.contains(&want), "faltou {want:?} em {refs:?}");
        }
        assert!(
            !refs
                .iter()
                .any(|r| r.0 == "T" && r.3 == RefKind::Type && r.1.is_some())
        );
    }
}
