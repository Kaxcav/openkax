//! Aceite do `kax-context` para Rust, índice, resolução e pack (ADR 0009).
//!
//! CONTRACT (P6a): os testes `#[ignore = "P6c"]` descrevem o comportamento
//! que a P6c entrega; com os stubs eles falham (`cargo test -- --ignored`).
//! A fixture é um crate de verdade (`tests/fixtures/context/rust`, confere com
//! `cargo check --manifest-path`); linhas citadas abaixo são as dela.

use std::fs;
use std::path::{Path, PathBuf};

use openkax::context::index::store_dir;
use openkax::context::lang::RustAdapter;
use openkax::context::{
    ContextError, ContextPack, Fidelity, Index, LanguageAdapter, MAX_FILE_BYTES, OmitReason,
    PackRole, RefKind, RefSite, Resolution, SymbolId, SymbolKind, Tier,
};

const CPT: f64 = 3.6;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/context/rust")
}

fn read(rel: &str) -> String {
    fs::read_to_string(fixture().join(rel)).unwrap()
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dst = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            if e.file_name() != "target" {
                copy_dir(&e.path(), &dst);
            }
        } else {
            fs::copy(e.path(), dst).unwrap();
        }
    }
}

/// Cópia da fixture num diretório temporário, fora de qualquer repo git.
fn workdir() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    copy_dir(&fixture(), d.path());
    d
}

fn indexed(root: &Path) -> Index {
    let mut idx = Index::open(root, &store_dir(root)).unwrap();
    idx.refresh().unwrap();
    idx
}

fn id(s: &str) -> SymbolId {
    SymbolId(s.into())
}

/// `(arquivo, linha, qual_name do envolvente)` de um sítio.
fn site(s: &RefSite) -> (String, u32, Option<String>) {
    (
        s.file.clone(),
        s.lines[0],
        s.enclosing.as_ref().map(|e| e.qual_name.clone()),
    )
}

fn sites(v: &[RefSite]) -> Vec<(String, u32, Option<String>)> {
    v.iter().map(site).collect()
}

fn owned(file: &str, line: u32, enclosing: &str) -> (String, u32, Option<String>) {
    (file.into(), line, Some(enclosing.into()))
}

/// Linhas `[a, b]` da fixture, cada uma com `\n` (o texto de um trecho `full`).
fn lines_of(rel: &str, a: u32, b: u32) -> String {
    read(rel)
        .lines()
        .skip(a as usize - 1)
        .take((b - a + 1) as usize)
        .map(|l| format!("{l}\n"))
        .collect()
}

fn tokens(text: &str) -> u32 {
    (text.chars().count() as f64 / CPT).ceil() as u32
}

/// `(qual_name, kind, span, doc, signature, parent)`; ids sem o arquivo.
type Expect<'a> = (
    &'a str,
    SymbolKind,
    [u32; 2],
    Option<[u32; 2]>,
    &'a str,
    Option<&'a str>,
);

/// As gramáticas leem a fixture sem erro: vale desde o CONTRACT.
#[test]
fn rust_fixture_parses_cleanly() {
    for f in [
        "src/lib.rs",
        "src/geometry.rs",
        "src/store.rs",
        "src/render.rs",
    ] {
        let mut p = tree_sitter::Parser::new();
        p.set_language(&RustAdapter.grammar()).unwrap();
        let tree = p.parse(read(f), None).unwrap();
        assert!(!tree.root_node().has_error(), "{f}");
    }
}

// ---------------------------------------------------------------- adapter --

#[test]
#[ignore = "P6c"]
fn rust_outline_of_geometry() {
    let src = read("src/geometry.rs");
    let facts = RustAdapter.extract(Path::new("src/geometry.rs"), &src);
    assert_eq!(facts.module, ["geometry"]);
    assert_eq!(facts.parse_errors, 0);

    let expected: [Expect<'_>; 9] = [
        (
            "Shape",
            SymbolKind::Trait,
            [4, 7],
            Some([3, 3]),
            "pub trait Shape",
            None,
        ),
        (
            "Shape::area",
            SymbolKind::Method,
            [6, 6],
            Some([5, 5]),
            "fn area(&self) -> f64",
            Some("Shape"),
        ),
        (
            "Rect",
            SymbolKind::Struct,
            [10, 14],
            Some([9, 9]),
            "pub struct Rect",
            None,
        ),
        (
            "impl Rect",
            SymbolKind::Impl,
            [16, 26],
            None,
            "impl Rect",
            None,
        ),
        (
            "Rect::new",
            SymbolKind::Method,
            [18, 20],
            Some([17, 17]),
            "pub fn new(w: f64, h: f64) -> Self",
            Some("impl Rect"),
        ),
        (
            "Rect::square",
            SymbolKind::Method,
            [23, 25],
            Some([22, 22]),
            "pub fn square(side: f64) -> Self",
            Some("impl Rect"),
        ),
        (
            "impl Shape for Rect",
            SymbolKind::Impl,
            [28, 32],
            None,
            "impl Shape for Rect",
            None,
        ),
        (
            "Rect::area",
            SymbolKind::Method,
            [29, 31],
            None,
            "fn area(&self) -> f64",
            Some("impl Shape for Rect"),
        ),
        (
            "validate",
            SymbolKind::Fn,
            [35, 37],
            Some([34, 34]),
            "pub fn validate(r: &Rect) -> bool",
            None,
        ),
    ];
    assert_eq!(facts.symbols.len(), expected.len(), "{:#?}", facts.symbols);
    for (s, (qual, kind, span, doc, sig, parent)) in facts.symbols.iter().zip(expected) {
        assert_eq!(s.id, id(&format!("src/geometry.rs::{qual}")));
        assert_eq!(s.qual_name, qual);
        // Um impl se chama pelo cabeçalho inteiro: `symbol("Rect")` não o pega.
        let name = if qual.starts_with("impl ") {
            qual
        } else {
            qual.rsplit("::").next().unwrap()
        };
        assert_eq!(s.name, name, "{qual}");
        assert_eq!(s.kind, kind, "{qual}");
        assert_eq!(s.file, "src/geometry.rs");
        assert_eq!(s.span.lines(), span, "{qual}");
        assert_eq!(s.doc_span.map(|d| d.lines()), doc, "{qual}");
        let sig_text =
            &src[s.signature_span.start_byte as usize..s.signature_span.end_byte as usize];
        assert_eq!(sig_text, sig, "{qual}");
        assert_eq!(
            s.parent,
            parent.map(|p| id(&format!("src/geometry.rs::{p}"))),
            "{qual}"
        );
    }
    // O span do struct começa no atributo; o byte do span bate com a linha.
    let rect = &facts.symbols[2];
    assert!(src[rect.span.start_byte as usize..].starts_with("#[derive(Debug"));
}

#[test]
#[ignore = "P6c"]
fn rust_refs_and_imports_of_store() {
    let src = read("src/store.rs");
    let facts = RustAdapter.extract(Path::new("src/store.rs"), &src);
    assert_eq!(facts.module, ["store"]);

    let imports: Vec<(&str, Option<&str>, Option<&str>, bool)> = facts
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
        imports,
        [
            ("crate::geometry", Some("Rect"), None, false),
            ("crate::geometry", Some("Shape"), None, false)
        ]
    );

    let has = |name: &str,
               hint: Option<&str>,
               method: bool,
               line: u32,
               kind: RefKind,
               enc: Option<&str>| {
        facts.refs.iter().any(|r| {
            r.name == name
                && r.qual_hint.as_deref() == hint
                && r.method == method
                && r.span.start_line == line
                && r.kind == kind
                && r.enclosing == enc.map(|e| id(&format!("src/store.rs::{e}")))
                && r.file == "src/store.rs"
        })
    };
    assert!(
        has(
            "Rect",
            Some("crate::geometry"),
            false,
            3,
            RefKind::Use,
            None
        ),
        "{:#?}",
        facts.refs
    );
    assert!(has(
        "Shape",
        Some("crate::geometry"),
        false,
        3,
        RefKind::Use,
        None
    ));
    assert!(has("Rect", None, false, 18, RefKind::Type, Some("Store")));
    assert!(has(
        "Rect",
        None,
        false,
        27,
        RefKind::Type,
        Some("Store::add")
    ));
    assert!(has(
        "StoreError",
        None,
        false,
        27,
        RefKind::Type,
        Some("Store::add")
    ));
    assert!(has(
        "validate",
        Some("crate::geometry"),
        false,
        31,
        RefKind::Call,
        Some("Store::add")
    ));
    assert!(has(
        "push",
        None,
        true,
        34,
        RefKind::Call,
        Some("Store::add")
    ));
    assert!(has(
        "new",
        Some("Vec"),
        false,
        23,
        RefKind::Call,
        Some("Store::new")
    ));
}

#[test]
#[ignore = "P6c"]
fn rust_lib_module_and_mod_declarations() {
    let src = read("src/lib.rs");
    let facts = RustAdapter.extract(Path::new("src/lib.rs"), &src);
    assert!(facts.module.is_empty(), "{:?}", facts.module);
    // `mod x;` só liga arquivos: não é símbolo (o `mod x { }` inline é).
    let names: Vec<&str> = facts.symbols.iter().map(|s| s.qual_name.as_str()).collect();
    assert_eq!(names, ["total_area"]);
    assert_eq!(facts.symbols[0].span.lines(), [13, 15]);
    assert_eq!(facts.symbols[0].doc_span.map(|d| d.lines()), Some([12, 12]));

    assert_eq!(facts.imports.len(), 1);
    assert_eq!(facts.imports[0].source, "geometry");
    assert_eq!(facts.imports[0].name.as_deref(), Some("Shape"));

    let area = facts
        .refs
        .iter()
        .find(|r| r.name == "area")
        .expect("s.area()");
    assert!(area.method);
    assert_eq!(area.qual_hint, None);
    assert_eq!(area.span.start_line, 14);
    assert_eq!(area.enclosing, Some(id("src/lib.rs::total_area")));
}

#[test]
#[ignore = "P6c"]
fn rust_syntax_error_is_counted_not_fatal() {
    let facts = RustAdapter.extract(Path::new("src/broken.rs"), "pub fn ok() {}\npub fn x( {\n");
    assert!(facts.parse_errors > 0);
    assert!(
        facts.symbols.iter().any(|s| s.name == "ok"),
        "fatos parciais"
    );
}

#[test]
#[ignore = "P6c"]
fn rust_inline_module_and_duplicate_ids() {
    let src = "mod inner {\n    pub fn f() {}\n}\n\nstruct S;\nimpl S {\n    fn g() {}\n}\nimpl S {\n    fn g() {}\n}\n";
    let facts = RustAdapter.extract(Path::new("src/dup.rs"), src);
    let ids: Vec<&str> = facts.symbols.iter().map(|s| s.id.as_str()).collect();
    assert!(ids.contains(&"src/dup.rs::inner"), "{ids:?}");
    assert!(ids.contains(&"src/dup.rs::inner::f"), "{ids:?}");
    let inner = facts
        .symbols
        .iter()
        .find(|s| s.qual_name == "inner")
        .unwrap();
    assert_eq!(inner.kind, SymbolKind::Module);
    // Colisão no mesmo arquivo: o 1º fica sem sufixo, os seguintes ganham
    // `#<linha inicial>`.
    let dup: Vec<&str> = ids
        .iter()
        .copied()
        .filter(|i| i.contains("S::g") || i.contains("impl S"))
        .collect();
    assert_eq!(
        dup,
        [
            "src/dup.rs::impl S",
            "src/dup.rs::S::g",
            "src/dup.rs::impl S#9",
            "src/dup.rs::S::g#10"
        ]
    );
}

// ----------------------------------------------------------------- índice --

#[test]
#[ignore = "P6c"]
fn index_outline_lists_symbols_in_source_order() {
    let d = workdir();
    let idx = indexed(d.path());
    let out = idx.outline("src/geometry.rs").unwrap();
    let got: Vec<(&str, SymbolKind, [u32; 2], &str)> = out
        .iter()
        .map(|s| (s.qual_name.as_str(), s.kind, s.lines, s.signature.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("Shape", SymbolKind::Trait, [4, 7], "pub trait Shape"),
            (
                "Shape::area",
                SymbolKind::Method,
                [6, 6],
                "fn area(&self) -> f64"
            ),
            ("Rect", SymbolKind::Struct, [10, 14], "pub struct Rect"),
            ("impl Rect", SymbolKind::Impl, [16, 26], "impl Rect"),
            (
                "Rect::new",
                SymbolKind::Method,
                [18, 20],
                "pub fn new(w: f64, h: f64) -> Self"
            ),
            (
                "Rect::square",
                SymbolKind::Method,
                [23, 25],
                "pub fn square(side: f64) -> Self"
            ),
            (
                "impl Shape for Rect",
                SymbolKind::Impl,
                [28, 32],
                "impl Shape for Rect"
            ),
            (
                "Rect::area",
                SymbolKind::Method,
                [29, 31],
                "fn area(&self) -> f64"
            ),
            (
                "validate",
                SymbolKind::Fn,
                [35, 37],
                "pub fn validate(r: &Rect) -> bool"
            ),
        ]
    );
    assert_eq!(out[4].parent, Some(id("src/geometry.rs::impl Rect")));
    assert!(matches!(
        idx.outline("src/nada.rs"),
        Err(ContextError::UnknownFile(_))
    ));
}

#[test]
#[ignore = "P6c"]
fn symbol_finds_the_definition_with_exact_lines() {
    let d = workdir();
    let idx = indexed(d.path());
    for q in [
        "Rect::new",
        "geometry::Rect::new",
        "src/geometry.rs::Rect::new",
    ] {
        let defs = idx.symbol(q).unwrap();
        assert_eq!(defs.len(), 1, "{q}");
        let def = &defs[0];
        assert!(!def.ambiguous, "{q}");
        assert_eq!(def.symbol.id, id("src/geometry.rs::Rect::new"));
        assert_eq!(def.symbol.file, "src/geometry.rs");
        assert_eq!(def.symbol.span.lines(), [18, 20]);
        assert_eq!(def.symbol.doc_span.map(|s| s.lines()), Some([17, 17]));
        assert_eq!(def.signature, "pub fn new(w: f64, h: f64) -> Self");
        assert_eq!(def.doc.as_deref(), Some("/// Cria um retângulo."));
    }
    let store = idx.symbol("Store").unwrap();
    assert_eq!(store.len(), 1);
    assert_eq!(store[0].symbol.kind, SymbolKind::Struct);
    assert_eq!(store[0].symbol.span.lines(), [16, 19]);
    assert!(matches!(
        idx.symbol("nao_existe"),
        Err(ContextError::UnknownSymbol(_))
    ));
}

#[test]
#[ignore = "P6c"]
fn same_name_in_two_modules_is_flagged_ambiguous() {
    let d = workdir();
    let idx = indexed(d.path());
    let defs = idx.symbol("validate").unwrap();
    let got: Vec<(&str, [u32; 2], bool)> = defs
        .iter()
        .map(|d| (d.symbol.id.as_str(), d.symbol.span.lines(), d.ambiguous))
        .collect();
    assert_eq!(
        got,
        [
            ("src/geometry.rs::validate", [35, 37], true),
            ("src/store.rs::validate", [45, 47], true),
        ]
    );
    for q in ["store::validate", "src/store.rs::validate"] {
        let one = idx.symbol(q).unwrap();
        assert_eq!(one.len(), 1, "{q}");
        assert!(!one[0].ambiguous);
        assert_eq!(one[0].symbol.id, id("src/store.rs::validate"));
    }
}

#[test]
#[ignore = "P6c"]
fn callers_find_cross_module_sites() {
    let d = workdir();
    let idx = indexed(d.path());
    assert_eq!(
        sites(&idx.callers("geometry::validate").unwrap()),
        [
            owned("src/render.rs", 17, "describe"),
            owned("src/store.rs", 31, "Store::add"),
        ]
    );
    assert!(idx.callers("store::validate").unwrap().is_empty());
    assert_eq!(
        sites(&idx.callers("Rect::new").unwrap()),
        [
            owned("src/geometry.rs", 24, "Rect::square"),
            owned("src/render.rs", 26, "sample"),
        ]
    );
    assert_eq!(
        sites(&idx.callers("Store::add").unwrap()),
        [owned("src/render.rs", 10, "build")]
    );
    for s in idx.callers("Rect::new").unwrap() {
        assert_eq!(s.kind, RefKind::Call);
        assert!(s.resolution.mentions(&id("src/geometry.rs::Rect::new")));
    }
}

#[test]
#[ignore = "P6c"]
fn refs_include_use_and_type_sites() {
    let d = workdir();
    let idx = indexed(d.path());
    let refs = idx.refs("Rect").unwrap();
    let got: Vec<(&str, u32, RefKind)> = refs
        .iter()
        .map(|s| (s.file.as_str(), s.lines[0], s.kind))
        .collect();
    for want in [
        ("src/store.rs", 3, RefKind::Use),
        ("src/render.rs", 3, RefKind::Use),
        ("src/store.rs", 18, RefKind::Type),
        ("src/store.rs", 27, RefKind::Type),
        ("src/render.rs", 16, RefKind::Type),
        ("src/render.rs", 25, RefKind::Type),
        ("src/geometry.rs", 35, RefKind::Type),
    ] {
        assert!(got.contains(&want), "faltou {want:?} em {got:?}");
    }
    let rect = id("src/geometry.rs::Rect");
    assert!(
        refs.iter().all(|s| s.resolution.mentions(&rect)),
        "{refs:#?}"
    );
    let mut sorted = got.clone();
    sorted.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    assert_eq!(got, sorted, "ordem de arquivo e linha");
}

// -------------------------------------------------------------- resolução --

#[test]
#[ignore = "P6c"]
fn resolution_tiers_and_hints() {
    let d = workdir();
    let idx = indexed(d.path());
    let at = |q: &str, file: &str, line: u32| {
        idx.callers(q)
            .unwrap()
            .into_iter()
            .find(|s| s.file == file && s.lines[0] == line)
            .unwrap_or_else(|| panic!("{q}: sem sítio em {file}:{line}"))
            .resolution
    };
    // `Self::new` → tipo do impl, no mesmo arquivo.
    assert_eq!(
        at("Rect::new", "src/geometry.rs", 24),
        Resolution::Resolved {
            target: id("src/geometry.rs::Rect::new"),
            tier: Tier::SameFile
        }
    );
    // `Store::new` em render.rs: o import filtra e o degrau é o mais perto que
    // se aplica (store.rs fica no mesmo diretório).
    assert_eq!(
        at("Store::new", "src/render.rs", 8),
        Resolution::Resolved {
            target: id("src/store.rs::Store::new"),
            tier: Tier::SameModule
        }
    );
    // O hint `crate::geometry` vence o `validate` do próprio arquivo.
    assert!(matches!(
        at("geometry::validate", "src/store.rs", 31),
        Resolution::Resolved { target, .. } if target == id("src/geometry.rs::validate")
    ));
    // `Vec::new()` é externo: não vira chamador de nenhum `new` do índice.
    for q in ["Store::new", "Rect::new"] {
        assert!(
            !sites(&idx.callers(q).unwrap())
                .iter()
                .any(|s| s.0 == "src/store.rs" && s.1 == 23),
            "{q}"
        );
    }
}

/// O import explícito vence a proximidade de diretório; sem import nem
/// vizinhança, o caminho qualificado acha o alvo no degrau global.
#[test]
#[ignore = "P6c"]
fn explicit_import_beats_a_sibling_with_the_same_name() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    for (f, text) in [
        (
            "src/lib.rs",
            "pub mod a;\npub mod b;\npub mod c;\npub mod deep;\n",
        ),
        ("src/deep/mod.rs", "pub fn helper() -> u8 {\n    1\n}\n"),
        ("src/b.rs", "pub fn helper() -> u8 {\n    2\n}\n"),
        (
            "src/a.rs",
            "use crate::deep::helper;\n\npub fn run() -> u8 {\n    helper()\n}\n",
        ),
        (
            "src/c.rs",
            "pub fn call() -> u8 {\n    crate::deep::helper()\n}\n",
        ),
    ] {
        let p = root.join(f);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }
    let idx = indexed(root);
    let callers = idx.callers("deep::helper").unwrap();
    assert_eq!(
        sites(&callers),
        [owned("src/a.rs", 4, "run"), owned("src/c.rs", 2, "call")]
    );
    let deep = id("src/deep/mod.rs::helper");
    assert_eq!(
        callers[0].resolution,
        Resolution::Resolved {
            target: deep.clone(),
            tier: Tier::Imported
        }
    );
    assert_eq!(
        callers[1].resolution,
        Resolution::Resolved {
            target: deep,
            tier: Tier::Global
        }
    );
    assert!(idx.callers("b::helper").unwrap().is_empty());
}

#[test]
#[ignore = "P6c"]
fn method_with_unknown_receiver_is_ambiguous() {
    let d = workdir();
    let idx = indexed(d.path());
    let site = idx
        .callers("Rect::area")
        .unwrap()
        .into_iter()
        .find(|s| s.file == "src/lib.rs")
        .expect("s.area() em lib.rs");
    assert_eq!(site.lines[0], 14);
    assert_eq!(site.enclosing.as_ref().unwrap().qual_name, "total_area");
    assert_eq!(
        site.resolution,
        Resolution::Ambiguous {
            candidates: vec![
                id("src/geometry.rs::Rect::area"),
                id("src/geometry.rs::Shape::area"),
                id("src/store.rs::Store::area"),
            ],
            tier: Tier::SameModule,
        }
    );
}

// ------------------------------------------------------------------- pack --

fn layout(p: &ContextPack) -> Vec<(PackRole, &str, [u32; 2], Fidelity)> {
    p.excerpts
        .iter()
        .map(|e| (e.role, e.symbol.as_str(), e.lines, e.fidelity))
        .collect()
}

#[test]
#[ignore = "P6c"]
fn pack_with_room_has_the_four_priorities() {
    let d = workdir();
    let idx = indexed(d.path());
    let p = idx.pack(&["Store::add"], 4000, CPT).unwrap();
    assert_eq!(
        layout(&p),
        [
            (
                PackRole::Target,
                "src/store.rs::Store::add",
                [26, 36],
                Fidelity::Full
            ),
            (
                PackRole::TypeDef,
                "src/geometry.rs::Rect",
                [9, 14],
                Fidelity::Full
            ),
            (
                PackRole::TypeDef,
                "src/store.rs::StoreError",
                [8, 13],
                Fidelity::Full
            ),
            (
                PackRole::TypeDef,
                "src/store.rs::Store",
                [15, 19],
                Fidelity::Full
            ),
            (
                PackRole::Callee,
                "src/geometry.rs::validate",
                [35, 35],
                Fidelity::Signature
            ),
            (
                PackRole::Caller,
                "src/render.rs::build",
                [7, 7],
                Fidelity::Signature
            ),
        ]
    );
    assert!(p.omitted.is_empty(), "{:#?}", p.omitted);
    let target = &p.excerpts[0];
    assert_eq!(target.text, lines_of("src/store.rs", 26, 36));
    assert_eq!(target.location(), "src/store.rs:26-36");
    assert_eq!(p.excerpts[4].text, "pub fn validate(r: &Rect) -> bool");
    assert_eq!(
        p.excerpts[5].text,
        "pub fn build(n: u32) -> Result<Store, StoreError>"
    );
    for e in &p.excerpts {
        assert_eq!(e.tokens_estimated, tokens(&e.text), "{}", e.location());
    }
    let sum: u32 = p.excerpts.iter().map(|e| e.tokens_estimated).sum();
    assert_eq!(p.tokens_estimated, sum);
    assert_eq!(p.budget_tokens, 4000);
}

#[test]
#[ignore = "P6c"]
fn pack_small_budget_keeps_the_target_and_lists_omitted() {
    let d = workdir();
    let idx = indexed(d.path());
    let t = tokens(&lines_of("src/store.rs", 26, 36));
    let p = idx.pack(&["Store::add"], t + 5, CPT).unwrap();
    assert_eq!(
        layout(&p),
        [(
            PackRole::Target,
            "src/store.rs::Store::add",
            [26, 36],
            Fidelity::Full
        )]
    );
    assert!(p.tokens_estimated <= p.budget_tokens);
    let mut omitted: Vec<(&str, PackRole)> = p
        .omitted
        .iter()
        .map(|o| (o.symbol.as_str(), o.role))
        .collect();
    omitted.sort();
    assert_eq!(
        omitted,
        [
            ("src/geometry.rs::Rect", PackRole::TypeDef),
            ("src/geometry.rs::validate", PackRole::Callee),
            ("src/render.rs::build", PackRole::Caller),
            ("src/store.rs::Store", PackRole::TypeDef),
            ("src/store.rs::StoreError", PackRole::TypeDef),
        ]
    );
    assert!(p.omitted.iter().all(
        |o| matches!(o.reason, OmitReason::Budget { tokens_estimated } if tokens_estimated > 5)
    ));
}

#[test]
#[ignore = "P6c"]
fn pack_fills_in_priority_order() {
    let d = workdir();
    let idx = indexed(d.path());
    let types = tokens(&lines_of("src/geometry.rs", 9, 14))
        + tokens(&lines_of("src/store.rs", 8, 13))
        + tokens(&lines_of("src/store.rs", 15, 19));
    let budget = tokens(&lines_of("src/store.rs", 26, 36)) + types + 2;
    let p = idx.pack(&["Store::add"], budget, CPT).unwrap();
    let roles: Vec<PackRole> = p.excerpts.iter().map(|e| e.role).collect();
    assert_eq!(
        roles,
        [
            PackRole::Target,
            PackRole::TypeDef,
            PackRole::TypeDef,
            PackRole::TypeDef
        ]
    );
    let mut left: Vec<PackRole> = p.omitted.iter().map(|o| o.role).collect();
    left.sort();
    assert_eq!(left, [PackRole::Callee, PackRole::Caller]);
    assert!(p.tokens_estimated <= budget);
}

#[test]
#[ignore = "P6c"]
fn pack_refuses_what_it_cannot_honour() {
    let d = workdir();
    let idx = indexed(d.path());
    let t = tokens(&lines_of("src/store.rs", 26, 36));
    match idx.pack(&["Store::add"], 10, CPT) {
        Err(ContextError::BudgetTooSmall { needed, budget }) => {
            assert_eq!((needed, budget), (t, 10));
        }
        other => panic!("esperava BudgetTooSmall: {other:?}"),
    }
    match idx.pack(&["validate"], 4000, CPT) {
        Err(ContextError::AmbiguousTarget { query, candidates }) => {
            assert_eq!(query, "validate");
            assert_eq!(
                candidates,
                [
                    id("src/geometry.rs::validate"),
                    id("src/store.rs::validate")
                ]
            );
        }
        other => panic!("esperava AmbiguousTarget: {other:?}"),
    }
    assert!(matches!(
        idx.pack(&["nao_existe"], 4000, CPT),
        Err(ContextError::UnknownSymbol(_))
    ));
}

#[test]
#[ignore = "P6c"]
fn pack_does_not_repeat_targets_or_shared_types() {
    let d = workdir();
    let idx = indexed(d.path());
    let p = idx.pack(&["Rect::new", "Rect::square"], 4000, CPT).unwrap();
    assert_eq!(
        layout(&p),
        [
            (
                PackRole::Target,
                "src/geometry.rs::Rect::new",
                [17, 20],
                Fidelity::Full
            ),
            (
                PackRole::Target,
                "src/geometry.rs::Rect::square",
                [22, 25],
                Fidelity::Full
            ),
            (
                PackRole::TypeDef,
                "src/geometry.rs::Rect",
                [9, 14],
                Fidelity::Full
            ),
            (
                PackRole::Caller,
                "src/render.rs::build",
                [7, 7],
                Fidelity::Signature
            ),
            (
                PackRole::Caller,
                "src/render.rs::sample",
                [25, 25],
                Fidelity::Signature
            ),
        ]
    );
}

#[test]
#[ignore = "P6c"]
fn pack_lists_an_ambiguous_dependency_instead_of_guessing() {
    let d = workdir();
    let idx = indexed(d.path());
    let p = idx.pack(&["total_area"], 4000, CPT).unwrap();
    assert_eq!(
        layout(&p),
        [
            (
                PackRole::Target,
                "src/lib.rs::total_area",
                [12, 15],
                Fidelity::Full
            ),
            (
                PackRole::TypeDef,
                "src/geometry.rs::Shape",
                [3, 7],
                Fidelity::Full
            ),
        ]
    );
    let amb = p
        .omitted
        .iter()
        .find(|o| o.role == PackRole::Callee)
        .expect("s.area() omitido");
    assert_eq!(amb.symbol, "area");
    assert_eq!(
        amb.reason,
        OmitReason::Ambiguous {
            candidates: vec![
                id("src/geometry.rs::Rect::area"),
                id("src/geometry.rs::Shape::area"),
                id("src/store.rs::Store::area"),
            ]
        }
    );
}

// ----------------------------------------------------------- incremental --

#[test]
#[ignore = "P6c"]
fn refresh_reparses_only_the_changed_file() {
    let d = workdir();
    let root = d.path();
    let mut idx = Index::open(root, &store_dir(root)).unwrap();
    let first = idx.refresh().unwrap();
    assert_eq!((first.parsed, first.reused, first.removed), (4, 0, 0));
    assert_eq!(idx.stats().parses_total, 4);
    assert_eq!(idx.stats().files, 4);

    let again = idx.refresh().unwrap();
    assert_eq!((again.parsed, again.reused), (0, 4));

    let store_rs = root.join("src/store.rs");
    let mut text = fs::read_to_string(&store_rs).unwrap();
    text.push_str("\n/// Nova.\npub fn extra() -> u8 {\n    1\n}\n");
    fs::write(&store_rs, text).unwrap();
    let changed = idx.refresh().unwrap();
    assert_eq!((changed.parsed, changed.reused), (1, 3));
    assert_eq!(idx.stats().parses_total, 5);
    assert_eq!(
        idx.symbol("extra").unwrap()[0].symbol.span.lines(),
        [50, 52]
    );

    // Persistido: outro processo abre e não reparseia nada.
    idx.save().unwrap();
    assert!(store_dir(root).join("index.json").is_file());
    let gi = fs::read_to_string(store_dir(root).join(".gitignore")).unwrap();
    assert_eq!(gi.trim(), "*");
    let mut reopened = Index::open(root, &store_dir(root)).unwrap();
    let warm = reopened.refresh().unwrap();
    assert_eq!((warm.parsed, warm.reused), (0, 4));
    assert_eq!(reopened.stats().parses_total, 0);

    fs::remove_file(root.join("src/render.rs")).unwrap();
    let gone = reopened.refresh().unwrap();
    assert_eq!((gone.parsed, gone.reused, gone.removed), (0, 3, 1));
    assert!(reopened.callers("Store::add").unwrap().is_empty());
}

#[test]
#[ignore = "P6c"]
fn walk_respects_gitignore_hidden_and_kax() {
    let d = workdir();
    let root = d.path();
    for f in [
        "ignored/skip.rs",
        "target/debug/build.rs",
        ".kax/wt/T-1/src/lib.rs",
        ".hidden/h.rs",
        "README.md",
    ] {
        let p = root.join(f);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, "pub fn fantasma() {}\n").unwrap();
    }
    let idx = indexed(root);
    assert_eq!(idx.stats().files, 4);
    assert!(matches!(
        idx.symbol("fantasma"),
        Err(ContextError::UnknownSymbol(_))
    ));
    for f in [
        "ignored/skip.rs",
        "target/debug/build.rs",
        ".kax/wt/T-1/src/lib.rs",
    ] {
        assert!(
            matches!(idx.outline(f), Err(ContextError::UnknownFile(_))),
            "{f}"
        );
    }
}

#[test]
#[ignore = "P6c"]
fn crlf_is_normalized_and_limits_skip_files() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/crlf.rs"),
        "/// Doc.\r\npub fn crlf_fn() -> u8 {\r\n    1\r\n}\r\n",
    )
    .unwrap();
    let big = format!("// {}\n", "x".repeat(MAX_FILE_BYTES as usize));
    fs::write(root.join("src/big.rs"), big).unwrap();
    fs::write(root.join("src/latin1.rs"), b"// \xe9\xff\n").unwrap();

    let mut idx = Index::open(root, &store_dir(root)).unwrap();
    let stats = idx.refresh().unwrap();
    assert_eq!((stats.parsed, stats.skipped), (1, 2));
    let def = &idx.symbol("crlf_fn").unwrap()[0];
    assert_eq!(def.symbol.span.lines(), [2, 4]);
    assert_eq!(def.symbol.doc_span.map(|s| s.lines()), Some([1, 1]));
    assert_eq!(def.signature, "pub fn crlf_fn() -> u8");
    for f in ["src/big.rs", "src/latin1.rs"] {
        assert!(
            matches!(idx.outline(f), Err(ContextError::UnknownFile(_))),
            "{f}"
        );
    }
}
