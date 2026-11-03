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

use std::collections::HashMap;
use std::path::Path;

use tree_sitter::StreamingIterator;

use super::LanguageAdapter;
use crate::context::{FileFacts, Import, Ref, RefKind, Span, Symbol, SymbolId, SymbolKind};

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

    fn extract(&self, path: &Path, src: &str) -> FileFacts {
        let grammar = self.grammar();
        let mut parser = tree_sitter::Parser::new();
        if parser.set_language(&grammar).is_err() {
            return FileFacts::default();
        }
        let tree = match parser.parse(src, None) {
            Some(t) => t,
            None => return FileFacts::default(),
        };

        let root = tree.root_node();
        let parse_errors = count_parse_errors(root);
        let module = compute_module(path);
        let file = path.to_str().unwrap_or("").replace('\\', "/");

        let mut facts = FileFacts {
            module,
            symbols: Vec::new(),
            refs: Vec::new(),
            imports: Vec::new(),
            parse_errors,
        };

        extract_imports(root, src, &file, &mut facts);
        extract_symbols(root, src, &file, &mut facts);
        extract_refs(root, src, &file, &mut facts);

        for r in &mut facts.refs {
            let mut best: Option<&Symbol> = None;
            for s in &facts.symbols {
                if s.span.start_byte <= r.span.start_byte && r.span.end_byte <= s.span.end_byte {
                    match best {
                        Some(curr) => {
                            let curr_len = curr.span.end_byte - curr.span.start_byte;
                            let s_len = s.span.end_byte - s.span.start_byte;
                            if s_len < curr_len {
                                best = Some(s);
                            }
                        }
                        None => best = Some(s),
                    }
                }
            }
            r.enclosing = best.map(|s| s.id.clone());
        }

        facts
    }
}

fn compute_module(path: &Path) -> Vec<String> {
    let mut parts = Vec::new();
    if let Some(parent) = path.parent() {
        for comp in parent.components() {
            let s = comp.as_os_str().to_string_lossy();
            if !s.is_empty() && s != "." {
                parts.push(s.into_owned());
            }
        }
    }
    if let Some(stem) = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|&s| s != "__init__")
    {
        parts.push(stem.to_string());
    }
    parts
}

fn count_parse_errors(root: tree_sitter::Node) -> u32 {
    let mut count = 0;
    let mut cursor = root.walk();
    let mut visit = true;
    while visit {
        let node = cursor.node();
        if node.is_error() || node.is_missing() {
            count += 1;
        }
        if cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                visit = false;
                break;
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }
    count
}

fn node_span(node: tree_sitter::Node) -> Span {
    Span {
        start_byte: node.start_byte() as u32,
        end_byte: node.end_byte() as u32,
        start_line: (node.start_position().row + 1) as u32,
        end_line: (node.end_position().row + 1) as u32,
    }
}

fn make_span(src: &str, start_byte: usize, end_byte: usize) -> Span {
    let start_line = src[..start_byte].bytes().filter(|&b| b == b'\n').count() as u32 + 1;
    let end_line = src[..end_byte].bytes().filter(|&b| b == b'\n').count() as u32 + 1;
    Span {
        start_byte: start_byte as u32,
        end_byte: end_byte as u32,
        start_line,
        end_line,
    }
}

fn find_docstring(body_node: tree_sitter::Node) -> Option<Span> {
    if body_node.kind() == "block" {
        let first_stmt = body_node.named_child(0)?;
        if first_stmt.kind() == "expression_statement" {
            let expr = first_stmt.named_child(0)?;
            if expr.kind() == "string" || expr.kind() == "concatenated_string" {
                return Some(node_span(expr));
            }
        }
    }
    None
}

fn signature_span_for(node: tree_sitter::Node, src: &str) -> Span {
    let start_byte = node.start_byte();
    let body = node.child_by_field_name("body");
    let end_byte = if let Some(b) = body {
        let before_body = &src[start_byte..b.start_byte()];
        let trimmed = before_body.trim_end().trim_end_matches(':').trim_end();
        start_byte + trimmed.len()
    } else {
        let whole = &src[start_byte..node.end_byte()];
        start_byte + whole.trim_end().trim_end_matches(':').trim_end().len()
    };

    make_span(src, start_byte, end_byte)
}

fn extract_imports(root: tree_sitter::Node, src: &str, file: &str, facts: &mut FileFacts) {
    let query_str = "
        (import_statement) @import
        (import_from_statement) @import_from
    ";
    let lang = root.language();
    let query = match tree_sitter::Query::new(&lang, query_str) {
        Ok(q) => q,
        Err(_) => return,
    };

    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&query, root, src.as_bytes());

    while let Some(m) = matches.next() {
        for c in m.captures() {
            let node = c.node;
            let span = node_span(node);

            if node.kind() == "import_statement" {
                let mut cur = node.walk();
                for ch in node.children(&mut cur) {
                    if ch.kind() == "dotted_name" {
                        let name = src[ch.byte_range()].to_string();
                        facts.imports.push(Import {
                            source: name.clone(),
                            name: None,
                            alias: None,
                            glob: false,
                            file: file.to_string(),
                            span,
                        });
                        facts.refs.push(Ref {
                            name,
                            qual_hint: None,
                            method: false,
                            file: file.to_string(),
                            span: node_span(ch),
                            enclosing: None,
                            kind: RefKind::Use,
                        });
                    } else if ch.kind() == "aliased_import" {
                        let name_node = ch.child_by_field_name("name");
                        let alias_node = ch.child_by_field_name("alias");
                        if let (Some(n), Some(a)) = (name_node, alias_node) {
                            let name = src[n.byte_range()].to_string();
                            let alias = src[a.byte_range()].to_string();
                            facts.imports.push(Import {
                                source: name.clone(),
                                name: None,
                                alias: Some(alias),
                                glob: false,
                                file: file.to_string(),
                                span,
                            });
                            facts.refs.push(Ref {
                                name,
                                qual_hint: None,
                                method: false,
                                file: file.to_string(),
                                span: node_span(n),
                                enclosing: None,
                                kind: RefKind::Use,
                            });
                        }
                    }
                }
            } else if node.kind() == "import_from_statement" {
                let module_name_node = node.child_by_field_name("module_name");
                let source = match module_name_node {
                    Some(m) => src[m.byte_range()].to_string(),
                    None => {
                        let prefix = node
                            .children(&mut node.walk())
                            .find(|c| c.kind() == "import_prefix" || c.kind() == "relative_import");
                        prefix
                            .map(|p| src[p.byte_range()].to_string())
                            .unwrap_or_default()
                    }
                };

                let is_glob = node
                    .children(&mut node.walk())
                    .any(|c| c.kind() == "wildcard_import");

                if is_glob {
                    facts.imports.push(Import {
                        source,
                        name: None,
                        alias: None,
                        glob: true,
                        file: file.to_string(),
                        span,
                    });
                } else {
                    let mut cur = node.walk();
                    for ch in node.children(&mut cur) {
                        if ch.kind() == "dotted_name" {
                            if Some(ch) == module_name_node {
                                continue;
                            }
                            let name = src[ch.byte_range()].to_string();
                            facts.imports.push(Import {
                                source: source.clone(),
                                name: Some(name.clone()),
                                alias: None,
                                glob: false,
                                file: file.to_string(),
                                span,
                            });
                            facts.refs.push(Ref {
                                name,
                                qual_hint: Some(source.clone()),
                                method: false,
                                file: file.to_string(),
                                span: node_span(ch),
                                enclosing: None,
                                kind: RefKind::Use,
                            });
                        } else if ch.kind() == "aliased_import" {
                            let name_node = ch.child_by_field_name("name");
                            let alias_node = ch.child_by_field_name("alias");
                            if let (Some(n), Some(a)) = (name_node, alias_node) {
                                let name = src[n.byte_range()].to_string();
                                let alias = src[a.byte_range()].to_string();
                                facts.imports.push(Import {
                                    source: source.clone(),
                                    name: Some(name.clone()),
                                    alias: Some(alias),
                                    glob: false,
                                    file: file.to_string(),
                                    span,
                                });
                                facts.refs.push(Ref {
                                    name,
                                    qual_hint: Some(source.clone()),
                                    method: false,
                                    file: file.to_string(),
                                    span: node_span(n),
                                    enclosing: None,
                                    kind: RefKind::Use,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
}

fn extract_symbols(root: tree_sitter::Node, src: &str, file: &str, facts: &mut FileFacts) {
    let query_str = "
        (function_definition name: (identifier) @fn.name) @fn
        (class_definition name: (identifier) @class.name) @class
    ";
    let lang = root.language();
    let query = match tree_sitter::Query::new(&lang, query_str) {
        Ok(q) => q,
        Err(_) => return,
    };

    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&query, root, src.as_bytes());

    struct RawSymbol<'a> {
        name: String,
        outer_node: tree_sitter::Node<'a>,
        inner_node: tree_sitter::Node<'a>,
        is_class: bool,
    }

    let mut raw_symbols: Vec<RawSymbol<'_>> = Vec::new();

    while let Some(m) = matches.next() {
        for c in m.captures() {
            let cap_name = &query.capture_names()[c.index as usize];
            let node = c.node;

            let (is_class, name_node) = match *cap_name {
                "class" => (true, node.child_by_field_name("name")),
                "fn" => (false, node.child_by_field_name("name")),
                _ => continue,
            };

            let name = match name_node {
                Some(n) => src[n.byte_range()].to_string(),
                None => continue,
            };

            let outer = if let Some(parent) =
                node.parent().filter(|p| p.kind() == "decorated_definition")
            {
                parent
            } else {
                node
            };

            raw_symbols.push(RawSymbol {
                name,
                outer_node: outer,
                inner_node: node,
                is_class,
            });
        }
    }

    raw_symbols.sort_by_key(|s| s.outer_node.start_byte());

    let mut seen_ids: HashMap<String, u32> = HashMap::new();

    for sym in raw_symbols {
        let mut parent_qual: Option<String> = None;
        let mut parent_id: Option<SymbolId> = None;
        let mut is_method = false;

        let mut ancestor = sym.outer_node.parent();
        while let Some(anc) = ancestor {
            if anc.kind() == "class_definition" {
                if let Some(existing) = facts.symbols.iter().find(|s| {
                    s.span.start_byte <= anc.start_byte() as u32
                        && anc.end_byte() as u32 <= s.span.end_byte
                }) {
                    parent_qual = Some(existing.qual_name.clone());
                    parent_id = Some(existing.id.clone());
                    is_method = true;
                    break;
                }
            } else if anc.kind() == "function_definition"
                && let Some(existing) = facts.symbols.iter().find(|s| {
                    s.span.start_byte <= anc.start_byte() as u32
                        && anc.end_byte() as u32 <= s.span.end_byte
                })
            {
                parent_qual = Some(existing.qual_name.clone());
                parent_id = Some(existing.id.clone());
                break;
            }
            ancestor = anc.parent();
        }

        let qual_name = match &parent_qual {
            Some(pq) => format!("{pq}.{}", sym.name),
            None => sym.name.clone(),
        };

        let kind = if sym.is_class {
            SymbolKind::Class
        } else if is_method {
            SymbolKind::Method
        } else {
            SymbolKind::Fn
        };

        let span = node_span(sym.outer_node);
        let count = seen_ids.entry(qual_name.clone()).or_insert(0);
        *count += 1;
        let id_str = if *count == 1 {
            format!("{file}::{qual_name}")
        } else {
            format!("{file}::{qual_name}#{}", span.start_line)
        };
        let id = SymbolId(id_str);

        let doc_span = sym
            .inner_node
            .child_by_field_name("body")
            .and_then(find_docstring);
        let signature_span = signature_span_for(sym.inner_node, src);

        facts.symbols.push(Symbol {
            id,
            name: sym.name,
            qual_name,
            kind,
            file: file.to_string(),
            span,
            signature_span,
            doc_span,
            parent: parent_id,
        });
    }
}

fn extract_refs(root: tree_sitter::Node, src: &str, file: &str, facts: &mut FileFacts) {
    let mut cursor = root.walk();
    let mut visit = true;

    while visit {
        let node = cursor.node();
        let kind = node.kind();

        if kind == "call" {
            if let Some(func) = node.child_by_field_name("function") {
                if func.kind() == "identifier" {
                    let text = src[func.byte_range()].to_string();
                    facts.refs.push(Ref {
                        name: text,
                        qual_hint: None,
                        method: false,
                        file: file.to_string(),
                        span: node_span(func),
                        enclosing: None,
                        kind: RefKind::Call,
                    });
                } else if func.kind() == "attribute" {
                    let obj_opt = func.child_by_field_name("object");
                    let attr_opt = func.child_by_field_name("attribute");
                    if let (Some(obj), Some(attr)) = (obj_opt, attr_opt) {
                        let prop_name = src[attr.byte_range()].to_string();
                        let obj_text = &src[obj.byte_range()];
                        let (qual_hint, method) = if obj_text == "self" {
                            (Some("self".to_string()), true)
                        } else if obj.kind() == "identifier"
                            && obj_text
                                .chars()
                                .next()
                                .is_some_and(|c| c.is_ascii_uppercase())
                        {
                            (Some(obj_text.to_string()), false)
                        } else {
                            (None, true)
                        };

                        facts.refs.push(Ref {
                            name: prop_name,
                            qual_hint,
                            method,
                            file: file.to_string(),
                            span: node_span(attr),
                            enclosing: None,
                            kind: RefKind::Call,
                        });
                    }
                }
            }
        } else if kind == "type" {
            extract_type_refs(node, src, file, facts);
        } else if kind == "class_definition" {
            // Inheritance: superclasses in argument_list
            if let Some(args) = node.child_by_field_name("superclasses") {
                let mut cur = args.walk();
                for arg in args.children(&mut cur) {
                    if arg.kind() == "identifier" {
                        facts.refs.push(Ref {
                            name: src[arg.byte_range()].to_string(),
                            qual_hint: None,
                            method: false,
                            file: file.to_string(),
                            span: node_span(arg),
                            enclosing: None,
                            kind: RefKind::Type,
                        });
                    }
                }
            }
        }

        if cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                visit = false;
                break;
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn extract_type_refs(node: tree_sitter::Node, src: &str, file: &str, facts: &mut FileFacts) {
    let mut cursor = node.walk();
    let mut visit = true;

    while visit {
        let n = cursor.node();
        if n.kind() == "identifier" {
            let name = src[n.byte_range()].to_string();
            facts.refs.push(Ref {
                name,
                qual_hint: None,
                method: false,
                file: file.to_string(),
                span: node_span(n),
                enclosing: None,
                kind: RefKind::Type,
            });
        } else if n.kind() == "string" {
            let raw = &src[n.byte_range()];
            let unq = raw.trim_matches(|c| c == '\'' || c == '"');
            if !unq.is_empty() && unq.chars().all(|c| c.is_alphanumeric() || c == '_') {
                facts.refs.push(Ref {
                    name: unq.to_string(),
                    qual_hint: None,
                    method: false,
                    file: file.to_string(),
                    span: node_span(n),
                    enclosing: None,
                    kind: RefKind::Type,
                });
            }
        }

        if cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                visit = false;
                break;
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }
}
