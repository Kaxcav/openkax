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

use std::collections::HashMap;
use std::path::Path;

use tree_sitter::StreamingIterator;

use super::LanguageAdapter;
use crate::context::{FileFacts, Import, Ref, RefKind, Span, Symbol, SymbolId, SymbolKind};

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

    fn extract(&self, path: &Path, src: &str) -> FileFacts {
        let grammar = self.grammar(path);
        extract_js_ts(path, src, grammar, true)
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

    fn extract(&self, path: &Path, src: &str) -> FileFacts {
        let grammar = self.grammar();
        extract_js_ts(path, src, grammar, false)
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
        .filter(|&s| s != "index")
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

fn find_doc_comment(node: tree_sitter::Node, src: &str) -> Option<Span> {
    if let Some(p) = node.prev_sibling()
        && p.kind() == "comment"
        && src[p.byte_range()].starts_with("/**")
    {
        return Some(node_span(p));
    }
    None
}

fn signature_span_for(outer: tree_sitter::Node, node: tree_sitter::Node, src: &str) -> Span {
    let start_byte = outer.start_byte();
    let body = node.children(&mut node.walk()).find(|c| {
        c.kind() == "statement_block" || c.kind() == "class_body" || c.kind() == "interface_body"
    });
    let end_byte = if let Some(b) = body {
        let before_body = &src[start_byte..b.start_byte()];
        start_byte + before_body.trim_end().len()
    } else if node.kind() == "method_signature" || node.kind() == "type_alias_declaration" {
        let whole = &src[start_byte..node.end_byte()];
        start_byte + whole.trim_end().trim_end_matches(';').trim_end().len()
    } else if node.kind() == "variable_declarator" {
        let val = node.child_by_field_name("value");
        if let Some(v) = val {
            if v.kind() == "arrow_function" {
                if let Some(b) = v.child_by_field_name("body") {
                    if b.kind() == "statement_block" {
                        let before_body = &src[start_byte..b.start_byte()];
                        start_byte + before_body.trim_end().len()
                    } else {
                        let whole = &src[start_byte..outer.end_byte()];
                        start_byte + whole.trim_end().trim_end_matches(';').trim_end().len()
                    }
                } else {
                    let whole = &src[start_byte..outer.end_byte()];
                    start_byte + whole.trim_end().trim_end_matches(';').trim_end().len()
                }
            } else {
                let whole = &src[start_byte..outer.end_byte()];
                start_byte + whole.trim_end().trim_end_matches(';').trim_end().len()
            }
        } else {
            let whole = &src[start_byte..outer.end_byte()];
            start_byte + whole.trim_end().trim_end_matches(';').trim_end().len()
        }
    } else {
        let whole = &src[start_byte..node.end_byte()];
        start_byte + whole.trim_end().trim_end_matches(';').trim_end().len()
    };

    make_span(src, start_byte, end_byte)
}

fn unquote(s: &str) -> String {
    let trimmed = s.trim();
    if (trimmed.starts_with('\'') && trimmed.ends_with('\''))
        || (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('`') && trimmed.ends_with('`'))
    {
        if trimmed.len() >= 2 {
            trimmed[1..trimmed.len() - 1].to_string()
        } else {
            trimmed.to_string()
        }
    } else {
        trimmed.to_string()
    }
}

fn extract_js_ts(path: &Path, src: &str, lang: tree_sitter::Language, is_ts: bool) -> FileFacts {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&lang).is_err() {
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
    extract_symbols(root, src, &file, is_ts, &mut facts);
    extract_refs(root, src, &file, is_ts, &mut facts);

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

fn extract_imports(root: tree_sitter::Node, src: &str, file: &str, facts: &mut FileFacts) {
    let query_str = "
        (import_statement source: (string) @src) @import
        (export_statement source: (string) @src) @export
        (call_expression
            function: (identifier) @fn_name
            arguments: (arguments (string) @src)) @call_req
    ";
    let lang = root.language();
    let query = match tree_sitter::Query::new(&lang, query_str) {
        Ok(q) => q,
        Err(_) => return,
    };

    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&query, root, src.as_bytes());

    // Os nomes vêm da query acima; `None` só se ela mudar sem acompanhar aqui.
    let (Some(src_idx), Some(import_idx), Some(export_idx), Some(call_req_idx), Some(fn_name_idx)) = (
        query.capture_index_for_name("src"),
        query.capture_index_for_name("import"),
        query.capture_index_for_name("export"),
        query.capture_index_for_name("call_req"),
        query.capture_index_for_name("fn_name"),
    ) else {
        return;
    };

    while let Some(m) = matches.next() {
        let mut node_import = None;
        let mut node_export = None;
        let mut node_call_req = None;
        let mut node_src = None;
        let mut node_fn_name = None;

        for c in m.captures() {
            if c.index == import_idx {
                node_import = Some(c.node);
            } else if c.index == export_idx {
                node_export = Some(c.node);
            } else if c.index == call_req_idx {
                node_call_req = Some(c.node);
            } else if c.index == src_idx {
                node_src = Some(c.node);
            } else if c.index == fn_name_idx {
                node_fn_name = Some(c.node);
            }
        }

        if let (Some(imp_node), Some(src_node)) = (node_import, node_src) {
            let source = unquote(&src[src_node.byte_range()]);
            let span = node_span(imp_node);

            let mut found_any = false;
            let mut cur = imp_node.walk();
            for ch in imp_node.children(&mut cur) {
                if ch.kind() == "import_clause" {
                    let mut clause_cur = ch.walk();
                    for part in ch.children(&mut clause_cur) {
                        if part.kind() == "identifier" {
                            let local = src[part.byte_range()].to_string();
                            facts.imports.push(Import {
                                source: source.clone(),
                                name: Some("default".to_string()),
                                alias: Some(local.clone()),
                                glob: false,
                                file: file.to_string(),
                                span,
                            });
                            facts.refs.push(Ref {
                                name: local,
                                qual_hint: Some(source.clone()),
                                method: false,
                                file: file.to_string(),
                                span: node_span(part),
                                enclosing: None,
                                kind: RefKind::Use,
                            });
                            found_any = true;
                        } else if part.kind() == "namespace_import" {
                            let alias_node = part.child_by_field_name("name").or_else(|| {
                                part.children(&mut part.walk())
                                    .find(|c| c.kind() == "identifier")
                            });
                            let alias = alias_node.map(|a| src[a.byte_range()].to_string());
                            facts.imports.push(Import {
                                source: source.clone(),
                                name: None,
                                alias: alias.clone(),
                                glob: true,
                                file: file.to_string(),
                                span,
                            });
                            if let Some(a_node) = alias_node {
                                facts.refs.push(Ref {
                                    name: src[a_node.byte_range()].to_string(),
                                    qual_hint: Some(source.clone()),
                                    method: false,
                                    file: file.to_string(),
                                    span: node_span(a_node),
                                    enclosing: None,
                                    kind: RefKind::Use,
                                });
                            }
                            found_any = true;
                        } else if part.kind() == "named_imports" {
                            let mut spec_cur = part.walk();
                            for spec in part.children(&mut spec_cur) {
                                if spec.kind() == "import_specifier" {
                                    let name_node =
                                        spec.child_by_field_name("name").unwrap_or(spec);
                                    let alias_node = spec.child_by_field_name("alias");
                                    let name = src[name_node.byte_range()].to_string();
                                    let alias = alias_node.map(|a| src[a.byte_range()].to_string());
                                    facts.imports.push(Import {
                                        source: source.clone(),
                                        name: Some(name.clone()),
                                        alias,
                                        glob: false,
                                        file: file.to_string(),
                                        span,
                                    });
                                    facts.refs.push(Ref {
                                        name,
                                        qual_hint: Some(source.clone()),
                                        method: false,
                                        file: file.to_string(),
                                        span: node_span(name_node),
                                        enclosing: None,
                                        kind: RefKind::Use,
                                    });
                                    found_any = true;
                                }
                            }
                        }
                    }
                }
            }
            if !found_any {
                facts.imports.push(Import {
                    source,
                    name: None,
                    alias: None,
                    glob: false,
                    file: file.to_string(),
                    span,
                });
            }
        } else if let (Some(exp_node), Some(src_node)) = (node_export, node_src) {
            let source = unquote(&src[src_node.byte_range()]);
            let span = node_span(exp_node);
            let is_glob = exp_node
                .children(&mut exp_node.walk())
                .any(|c| c.kind() == "*");
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
                let mut cur = exp_node.walk();
                for ch in exp_node.children(&mut cur) {
                    if ch.kind() == "export_clause" {
                        let mut spec_cur = ch.walk();
                        for spec in ch.children(&mut spec_cur) {
                            if spec.kind() == "export_specifier" {
                                let name_node = spec.child_by_field_name("name").unwrap_or(spec);
                                let alias_node = spec.child_by_field_name("alias");
                                let name = src[name_node.byte_range()].to_string();
                                let alias = alias_node.map(|a| src[a.byte_range()].to_string());
                                facts.imports.push(Import {
                                    source: source.clone(),
                                    name: Some(name.clone()),
                                    alias,
                                    glob: false,
                                    file: file.to_string(),
                                    span,
                                });
                                facts.refs.push(Ref {
                                    name,
                                    qual_hint: Some(source.clone()),
                                    method: false,
                                    file: file.to_string(),
                                    span: node_span(name_node),
                                    enclosing: None,
                                    kind: RefKind::Use,
                                });
                            }
                        }
                    }
                }
            }
        } else if let (Some(call_node), Some(src_node), Some(fn_node)) =
            (node_call_req, node_src, node_fn_name)
            && &src[fn_node.byte_range()] == "require"
        {
            let source = unquote(&src[src_node.byte_range()]);
            let span = node_span(call_node);
            if let Some(p) = call_node
                .parent()
                .filter(|p| p.kind() == "variable_declarator")
                && let Some(name_node) = p.child_by_field_name("name")
            {
                if name_node.kind() == "identifier" {
                    let local = src[name_node.byte_range()].to_string();
                    facts.imports.push(Import {
                        source: source.clone(),
                        name: None,
                        alias: Some(local.clone()),
                        glob: false,
                        file: file.to_string(),
                        span,
                    });
                    facts.refs.push(Ref {
                        name: local,
                        qual_hint: Some(source),
                        method: false,
                        file: file.to_string(),
                        span: node_span(name_node),
                        enclosing: None,
                        kind: RefKind::Use,
                    });
                } else if name_node.kind() == "object_pattern" {
                    let mut cur = name_node.walk();
                    for prop in name_node.children(&mut cur) {
                        if prop.kind() == "shorthand_property_identifier_pattern" {
                            let name = src[prop.byte_range()].to_string();
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
                                span: node_span(prop),
                                enclosing: None,
                                kind: RefKind::Use,
                            });
                        } else if prop.kind() == "pair_pattern" {
                            let key_node = prop.child_by_field_name("key");
                            let val_node = prop.child_by_field_name("value");
                            if let (Some(k), Some(v)) = (key_node, val_node) {
                                let name = src[k.byte_range()].to_string();
                                let alias = src[v.byte_range()].to_string();
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
                                    span: node_span(k),
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

fn extract_symbols(
    root: tree_sitter::Node,
    src: &str,
    file: &str,
    is_ts: bool,
    facts: &mut FileFacts,
) {
    let query_str = if is_ts {
        "
        (function_declaration name: (identifier) @fn.name) @fn
        (class_declaration name: (type_identifier) @class.name) @class
        (interface_declaration name: (type_identifier) @interface.name) @interface
        (type_alias_declaration name: (type_identifier) @type.name) @type
        (internal_module name: (identifier) @module.name) @module
        (method_definition name: (property_identifier) @method.name) @method
        (method_signature name: (property_identifier) @method_sig.name) @method_sig
        (variable_declarator name: (identifier) @var.name) @var
        "
    } else {
        "
        (function_declaration name: (identifier) @fn.name) @fn
        (class_declaration name: (identifier) @class.name) @class
        (method_definition name: (property_identifier) @method.name) @method
        (variable_declarator name: (identifier) @var.name) @var
        "
    };

    let lang = root.language();
    let query = match tree_sitter::Query::new(&lang, query_str) {
        Ok(q) => q,
        Err(_) => return,
    };

    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&query, root, src.as_bytes());

    struct RawSymbol<'a> {
        name: String,
        kind: SymbolKind,
        outer_node: tree_sitter::Node<'a>,
        inner_node: tree_sitter::Node<'a>,
    }

    let mut raw_symbols: Vec<RawSymbol<'_>> = Vec::new();

    while let Some(m) = matches.next() {
        for c in m.captures() {
            let cap_name = &query.capture_names()[c.index as usize];
            let node = c.node;

            let (kind, name_node) = match *cap_name {
                "fn" => (SymbolKind::Fn, node.child_by_field_name("name")),
                "class" => (SymbolKind::Class, node.child_by_field_name("name")),
                "interface" => (SymbolKind::Interface, node.child_by_field_name("name")),
                "type" => (SymbolKind::Type, node.child_by_field_name("name")),
                "module" => (SymbolKind::Module, node.child_by_field_name("name")),
                "method" => (SymbolKind::Method, node.child_by_field_name("name")),
                "method_sig" => (SymbolKind::Method, node.child_by_field_name("name")),
                "var" => {
                    let val = node.child_by_field_name("value");
                    let is_fn = val.is_some_and(|v| {
                        v.kind() == "arrow_function" || v.kind() == "function_expression"
                    });
                    let parent_decl = node.parent();
                    let is_const = parent_decl.is_some_and(|p| {
                        p.kind() == "lexical_declaration"
                            && p.children(&mut p.walk()).any(|k| k.kind() == "const")
                    });

                    if is_fn {
                        (SymbolKind::Fn, node.child_by_field_name("name"))
                    } else if is_const {
                        let grand = parent_decl.and_then(|p| p.parent());
                        let is_module_level = grand.is_some_and(|g| {
                            g.kind() == "program"
                                || (g.kind() == "export_statement"
                                    && g.parent().is_some_and(|gg| gg.kind() == "program"))
                        });
                        if is_module_level {
                            (SymbolKind::Const, node.child_by_field_name("name"))
                        } else {
                            continue;
                        }
                    } else {
                        continue;
                    }
                }
                _ => continue,
            };

            let name = match name_node {
                Some(n) => src[n.byte_range()].to_string(),
                None => continue,
            };

            let outer = if let Some(parent) = node.parent() {
                if parent.kind() == "export_statement" {
                    parent
                } else if parent.kind() == "lexical_declaration" {
                    if let Some(gp) = parent.parent() {
                        if gp.kind() == "export_statement" {
                            gp
                        } else {
                            parent
                        }
                    } else {
                        parent
                    }
                } else {
                    node
                }
            } else {
                node
            };

            raw_symbols.push(RawSymbol {
                name,
                kind,
                outer_node: outer,
                inner_node: node,
            });
        }
    }

    raw_symbols.sort_by_key(|s| s.outer_node.start_byte());

    let mut seen_ids: HashMap<String, u32> = HashMap::new();

    for sym in raw_symbols {
        let mut parent_qual: Option<String> = None;
        let mut parent_id: Option<SymbolId> = None;

        let mut ancestor = sym.inner_node.parent();
        while let Some(anc) = ancestor {
            if (anc.kind() == "class_declaration"
                || anc.kind() == "interface_declaration"
                || anc.kind() == "internal_module"
                || anc.kind() == "function_declaration")
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

        let span = node_span(sym.outer_node);
        let count = seen_ids.entry(qual_name.clone()).or_insert(0);
        *count += 1;
        let id_str = if *count == 1 {
            format!("{file}::{qual_name}")
        } else {
            format!("{file}::{qual_name}#{}", span.start_line)
        };
        let id = SymbolId(id_str);

        let doc_span = find_doc_comment(sym.outer_node, src);
        let signature_span = signature_span_for(sym.outer_node, sym.inner_node, src);

        facts.symbols.push(Symbol {
            id,
            name: sym.name,
            qual_name,
            kind: sym.kind,
            file: file.to_string(),
            span,
            signature_span,
            doc_span,
            parent: parent_id,
        });
    }
}

fn extract_refs(
    root: tree_sitter::Node,
    src: &str,
    file: &str,
    is_ts: bool,
    facts: &mut FileFacts,
) {
    let mut cursor = root.walk();
    let mut visit = true;

    while visit {
        let node = cursor.node();
        let kind = node.kind();

        if kind == "call_expression" {
            if let Some(func) = node.child_by_field_name("function") {
                if func.kind() == "identifier" {
                    let text = &src[func.byte_range()];
                    if text != "require" && text != "import" {
                        facts.refs.push(Ref {
                            name: text.to_string(),
                            qual_hint: None,
                            method: false,
                            file: file.to_string(),
                            span: node_span(func),
                            enclosing: None,
                            kind: RefKind::Call,
                        });
                    }
                } else if func.kind() == "member_expression" {
                    let obj_opt = func.child_by_field_name("object");
                    let prop_opt = func.child_by_field_name("property");
                    if let (Some(obj), Some(prop)) = (obj_opt, prop_opt) {
                        let prop_name = src[prop.byte_range()].to_string();
                        let obj_text = &src[obj.byte_range()];
                        let (qual_hint, method) = if obj_text == "this" {
                            (Some("this".to_string()), true)
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
                            span: node_span(prop),
                            enclosing: None,
                            kind: RefKind::Call,
                        });
                    }
                }
            }
        } else if kind == "new_expression" {
            if let Some(ctor) = node.child_by_field_name("constructor") {
                if ctor.kind() == "identifier" {
                    let text = src[ctor.byte_range()].to_string();
                    facts.refs.push(Ref {
                        name: text,
                        qual_hint: None,
                        method: false,
                        file: file.to_string(),
                        span: node_span(ctor),
                        enclosing: None,
                        kind: RefKind::Call,
                    });
                } else if ctor.kind() == "member_expression" {
                    let obj_opt = ctor.child_by_field_name("object");
                    let prop_opt = ctor.child_by_field_name("property");
                    if let (Some(obj), Some(prop)) = (obj_opt, prop_opt) {
                        let prop_name = src[prop.byte_range()].to_string();
                        let obj_text = src[obj.byte_range()].to_string();
                        facts.refs.push(Ref {
                            name: prop_name,
                            qual_hint: Some(obj_text),
                            method: false,
                            file: file.to_string(),
                            span: node_span(prop),
                            enclosing: None,
                            kind: RefKind::Call,
                        });
                    }
                }
            }
        } else if is_ts && kind == "type_identifier" {
            let is_decl = node.parent().is_some_and(|p| {
                (p.kind() == "class_declaration" && p.child_by_field_name("name") == Some(node))
                    || (p.kind() == "interface_declaration"
                        && p.child_by_field_name("name") == Some(node))
                    || (p.kind() == "type_alias_declaration"
                        && p.child_by_field_name("name") == Some(node))
            });

            if !is_decl {
                let name = src[node.byte_range()].to_string();
                facts.refs.push(Ref {
                    name,
                    qual_hint: None,
                    method: false,
                    file: file.to_string(),
                    span: node_span(node),
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
