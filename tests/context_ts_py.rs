//! Aceite do `kax-context` para TypeScript/JavaScript e Python (ADR 0009).
//!
//! Os testes `*_index_*` também usam o `Index` da P6c; todos rodam desde que a
//! P6b e a P6c entraram (PRs #12 e #13).
//! Fixtures em `tests/fixtures/context/{ts,py}`; linhas citadas são as delas.

use std::fs;
use std::path::{Path, PathBuf};

use openkax::context::index::store_dir;
use openkax::context::lang::{JavaScriptAdapter, PythonAdapter, TypeScriptAdapter};
use openkax::context::{
    FileFacts, Index, LanguageAdapter, RefKind, RefSite, Resolution, SymbolId, SymbolKind, Tier,
};

fn fixture(lang: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/context")
        .join(lang)
}

fn read(lang: &str, rel: &str) -> String {
    fs::read_to_string(fixture(lang).join(rel)).unwrap()
}

fn extract(adapter: &dyn LanguageAdapter, lang: &str, rel: &str) -> (String, FileFacts) {
    let src = read(lang, rel);
    let facts = adapter.extract(Path::new(rel), &src);
    (src, facts)
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dst = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dst);
        } else {
            fs::copy(e.path(), dst).unwrap();
        }
    }
}

fn indexed(lang: &str) -> (tempfile::TempDir, Index) {
    let d = tempfile::tempdir().unwrap();
    copy_dir(&fixture(lang), d.path());
    let mut idx = Index::open(d.path(), &store_dir(d.path())).unwrap();
    idx.refresh().unwrap();
    (d, idx)
}

fn id(s: &str) -> SymbolId {
    SymbolId(s.into())
}

fn sites(v: &[RefSite]) -> Vec<(String, u32, Option<String>)> {
    v.iter()
        .map(|s| {
            (
                s.file.clone(),
                s.lines[0],
                s.enclosing.as_ref().map(|e| e.qual_name.clone()),
            )
        })
        .collect()
}

fn owned(file: &str, line: u32, enclosing: &str) -> (String, u32, Option<String>) {
    (file.into(), line, Some(enclosing.into()))
}

type Expect<'a> = (
    &'a str,
    SymbolKind,
    [u32; 2],
    Option<[u32; 2]>,
    &'a str,
    Option<&'a str>,
);

/// Confere `(qual_name, kind, span, doc, signature, parent)` em ordem.
fn assert_symbols(file: &str, src: &str, facts: &FileFacts, expected: &[Expect<'_>]) {
    assert_eq!(facts.symbols.len(), expected.len(), "{:#?}", facts.symbols);
    for (s, (qual, kind, span, doc, sig, parent)) in facts.symbols.iter().zip(expected) {
        assert_eq!(s.id, id(&format!("{file}::{qual}")));
        assert_eq!(s.qual_name, *qual);
        assert_eq!(s.name, qual.rsplit('.').next().unwrap(), "{qual}");
        assert_eq!(s.kind, *kind, "{qual}");
        assert_eq!(s.file, file);
        assert_eq!(s.span.lines(), *span, "{qual}");
        assert_eq!(s.doc_span.map(|d| d.lines()), *doc, "{qual}");
        let text = &src[s.signature_span.start_byte as usize..s.signature_span.end_byte as usize];
        assert_eq!(text, *sig, "{qual}");
        assert_eq!(
            s.parent,
            parent.map(|p| id(&format!("{file}::{p}"))),
            "{qual}"
        );
    }
}

fn has_ref(
    facts: &FileFacts,
    file: &str,
    (name, hint, method, line, kind, enc): (&str, Option<&str>, bool, u32, RefKind, Option<&str>),
) -> bool {
    facts.refs.iter().any(|r| {
        r.name == name
            && r.qual_hint.as_deref() == hint
            && r.method == method
            && r.span.start_line == line
            && r.kind == kind
            && r.file == file
            && r.enclosing == enc.map(|e| id(&format!("{file}::{e}")))
    })
}

fn imports(facts: &FileFacts) -> Vec<(&str, Option<&str>)> {
    facts
        .imports
        .iter()
        .map(|i| (i.source.as_str(), i.name.as_deref()))
        .collect()
}

/// As gramáticas leem as fixtures sem erro: vale desde o CONTRACT.
#[test]
fn ts_js_py_fixtures_parse_cleanly() {
    let cases = [
        (
            TypeScriptAdapter.grammar(Path::new("a.ts")),
            "ts",
            "src/shapes.ts",
        ),
        (
            TypeScriptAdapter.grammar(Path::new("a.ts")),
            "ts",
            "src/main.ts",
        ),
        (JavaScriptAdapter.grammar(), "ts", "src/legacy.js"),
        (PythonAdapter.grammar(), "py", "pkg/shapes.py"),
        (PythonAdapter.grammar(), "py", "pkg/main.py"),
        (PythonAdapter.grammar(), "py", "pkg/__init__.py"),
    ];
    for (lang, dir, rel) in cases {
        let mut p = tree_sitter::Parser::new();
        p.set_language(&lang).unwrap();
        let tree = p.parse(read(dir, rel), None).unwrap();
        assert!(!tree.root_node().has_error(), "{rel}");
    }
}

// ------------------------------------------------------------ TypeScript --

#[test]
fn ts_outline_of_shapes() {
    let (src, facts) = extract(&TypeScriptAdapter, "ts", "src/shapes.ts");
    assert_eq!(facts.module, ["src", "shapes"]);
    assert_eq!(facts.parse_errors, 0);
    assert_symbols(
        "src/shapes.ts",
        &src,
        &facts,
        &[
            (
                "Shape",
                SymbolKind::Interface,
                [4, 6],
                Some([3, 3]),
                "export interface Shape",
                None,
            ),
            (
                "Shape.area",
                SymbolKind::Method,
                [5, 5],
                None,
                "area(): number",
                Some("Shape"),
            ),
            (
                "Rect",
                SymbolKind::Class,
                [9, 20],
                Some([8, 8]),
                "export class Rect implements Shape",
                None,
            ),
            (
                "Rect.constructor",
                SymbolKind::Method,
                [10, 10],
                None,
                "constructor(public w: number, public h: number)",
                Some("Rect"),
            ),
            (
                "Rect.area",
                SymbolKind::Method,
                [13, 15],
                Some([12, 12]),
                "area(): number",
                Some("Rect"),
            ),
            (
                "Rect.square",
                SymbolKind::Method,
                [17, 19],
                None,
                "static square(side: number): Rect",
                Some("Rect"),
            ),
            (
                "totalArea",
                SymbolKind::Fn,
                [23, 25],
                Some([22, 22]),
                "export function totalArea(shapes: Shape[]): number",
                None,
            ),
        ],
    );
}

#[test]
fn ts_imports_and_refs() {
    let (_, main) = extract(&TypeScriptAdapter, "ts", "src/main.ts");
    assert_eq!(main.module, ["src", "main"]);
    assert_eq!(
        imports(&main),
        [
            ("./shapes", Some("Rect")),
            ("./shapes", Some("totalArea")),
            ("./legacy.js", Some("format"))
        ]
    );
    let f = "src/main.ts";
    for want in [
        ("Rect", Some("./shapes"), false, 1, RefKind::Use, None),
        ("square", Some("Rect"), false, 6, RefKind::Call, Some("run")),
        ("totalArea", None, false, 7, RefKind::Call, Some("run")),
        ("format", None, false, 7, RefKind::Call, Some("run")),
    ] {
        assert!(
            has_ref(&main, f, want),
            "faltou {want:?} em {:#?}",
            main.refs
        );
    }

    let (_, shapes) = extract(&TypeScriptAdapter, "ts", "src/shapes.ts");
    let f = "src/shapes.ts";
    for want in [
        ("Shape", None, false, 9, RefKind::Type, Some("Rect")),
        ("Rect", None, false, 18, RefKind::Call, Some("Rect.square")),
        ("area", None, true, 24, RefKind::Call, Some("totalArea")),
        ("Shape", None, false, 23, RefKind::Type, Some("totalArea")),
    ] {
        assert!(
            has_ref(&shapes, f, want),
            "faltou {want:?} em {:#?}",
            shapes.refs
        );
    }
}

#[test]
fn tsx_and_type_assertions_use_the_right_grammar() {
    let tsx = TypeScriptAdapter.extract(
        Path::new("src/App.tsx"),
        "export function App(): JSX.Element {\n  return <div>{1}</div>;\n}\n",
    );
    assert_eq!(tsx.parse_errors, 0);
    assert_eq!(tsx.symbols.len(), 1);
    assert_eq!(tsx.symbols[0].name, "App");
    let ts = TypeScriptAdapter.extract(
        Path::new("src/cast.ts"),
        "export function n(x: unknown): number {\n  return <number>x;\n}\n",
    );
    assert_eq!(ts.parse_errors, 0, "<T>x é cast em .ts");
    assert_eq!(ts.symbols[0].name, "n");
}

#[test]
fn js_outline_of_legacy() {
    let (src, facts) = extract(&JavaScriptAdapter, "ts", "src/legacy.js");
    assert_eq!(facts.module, ["src", "legacy"]);
    let names: Vec<(&str, SymbolKind, [u32; 2])> = facts
        .symbols
        .iter()
        .map(|s| (s.qual_name.as_str(), s.kind, s.span.lines()))
        .collect();
    assert_eq!(
        names,
        [
            ("format", SymbolKind::Fn, [4, 6]),
            ("double", SymbolKind::Fn, [8, 8])
        ]
    );
    let format = &facts.symbols[0];
    assert_eq!(format.doc_span.map(|d| d.lines()), Some([3, 3]));
    let sig =
        &src[format.signature_span.start_byte as usize..format.signature_span.end_byte as usize];
    assert_eq!(sig, "export function format(n)");
}

#[test]
fn ts_index_symbol_and_callers_cross_into_js() {
    let (_d, idx) = indexed("ts");
    let defs = idx.symbol("Rect.square").unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].symbol.span.lines(), [17, 19]);
    assert_eq!(defs[0].signature, "static square(side: number): Rect");

    assert_eq!(
        sites(&idx.callers("totalArea").unwrap()),
        [owned("src/main.ts", 7, "run")]
    );
    let format = idx.callers("format").unwrap();
    assert_eq!(sites(&format), [owned("src/main.ts", 7, "run")]);
    assert!(matches!(
        &format[0].resolution,
        Resolution::Resolved { target, .. } if *target == id("src/legacy.js::format")
    ));
    // `new Rect(...)` é chamada do construtor da classe.
    assert_eq!(
        sites(&idx.callers("Rect").unwrap()),
        [owned("src/shapes.ts", 18, "Rect.square")]
    );
}

#[test]
fn ts_index_method_on_untyped_receiver_is_ambiguous() {
    let (_d, idx) = indexed("ts");
    let site = idx
        .callers("Rect.area")
        .unwrap()
        .into_iter()
        .find(|s| s.lines[0] == 24)
        .expect("s.area() em totalArea");
    assert_eq!(
        site.resolution,
        Resolution::Ambiguous {
            candidates: vec![
                id("src/shapes.ts::Rect.area"),
                id("src/shapes.ts::Shape.area")
            ],
            tier: Tier::SameFile,
        }
    );
}

// ---------------------------------------------------------------- Python --

#[test]
fn py_outline_of_shapes() {
    let (src, facts) = extract(&PythonAdapter, "py", "pkg/shapes.py");
    assert_eq!(facts.module, ["pkg", "shapes"]);
    assert_eq!(facts.parse_errors, 0);
    assert_symbols(
        "pkg/shapes.py",
        &src,
        &facts,
        &[
            (
                "Rect",
                SymbolKind::Class,
                [4, 17],
                Some([5, 5]),
                "class Rect",
                None,
            ),
            (
                "Rect.__init__",
                SymbolKind::Method,
                [7, 9],
                None,
                "def __init__(self, w: float, h: float) -> None",
                Some("Rect"),
            ),
            (
                "Rect.area",
                SymbolKind::Method,
                [11, 13],
                Some([12, 12]),
                "def area(self) -> float",
                Some("Rect"),
            ),
            // O decorator entra no span; a assinatura começa no `def`.
            (
                "Rect.square",
                SymbolKind::Method,
                [15, 17],
                None,
                "def square(side: float) -> \"Rect\"",
                Some("Rect"),
            ),
            (
                "total_area",
                SymbolKind::Fn,
                [20, 22],
                Some([21, 21]),
                "def total_area(shapes: list[Rect]) -> float",
                None,
            ),
        ],
    );
}

#[test]
fn py_imports_and_refs() {
    let (_, main) = extract(&PythonAdapter, "py", "pkg/main.py");
    assert_eq!(main.module, ["pkg", "main"]);
    assert_eq!(
        imports(&main),
        [(".shapes", Some("Rect")), (".shapes", Some("total_area"))]
    );
    let f = "pkg/main.py";
    for want in [
        ("Rect", Some(".shapes"), false, 3, RefKind::Use, None),
        ("square", Some("Rect"), false, 7, RefKind::Call, Some("run")),
        ("total_area", None, false, 8, RefKind::Call, Some("run")),
    ] {
        assert!(
            has_ref(&main, f, want),
            "faltou {want:?} em {:#?}",
            main.refs
        );
    }

    let (_, init) = extract(&PythonAdapter, "py", "pkg/__init__.py");
    assert_eq!(init.module, ["pkg"]);
    assert!(init.symbols.is_empty());

    let (_, shapes) = extract(&PythonAdapter, "py", "pkg/shapes.py");
    let f = "pkg/shapes.py";
    for want in [
        ("Rect", None, false, 17, RefKind::Call, Some("Rect.square")),
        ("area", None, true, 22, RefKind::Call, Some("total_area")),
        ("Rect", None, false, 20, RefKind::Type, Some("total_area")),
    ] {
        assert!(
            has_ref(&shapes, f, want),
            "faltou {want:?} em {:#?}",
            shapes.refs
        );
    }
}

#[test]
fn py_index_symbol_and_callers() {
    let (_d, idx) = indexed("py");
    let defs = idx.symbol("Rect.square").unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].symbol.span.lines(), [15, 17]);
    assert_eq!(defs[0].doc, None);

    assert_eq!(
        sites(&idx.callers("total_area").unwrap()),
        [owned("pkg/main.py", 8, "run")]
    );
    let square = idx.callers("Rect.square").unwrap();
    assert_eq!(sites(&square), [owned("pkg/main.py", 7, "run")]);
    assert_eq!(
        square[0].resolution,
        Resolution::Resolved {
            target: id("pkg/shapes.py::Rect.square"),
            tier: Tier::SameModule
        }
    );
    assert_eq!(
        sites(&idx.callers("Rect").unwrap()),
        [owned("pkg/shapes.py", 17, "Rect.square")]
    );
    // Um só método `area` no índice: resolve mesmo sem o tipo do receptor.
    let area = idx.callers("Rect.area").unwrap();
    assert_eq!(sites(&area), [owned("pkg/shapes.py", 22, "total_area")]);
    assert!(matches!(area[0].resolution, Resolution::Resolved { .. }));
}

#[test]
fn ts_added_language_cases() {
    let src = r#"import { original as renamed } from "./relative";

export default function defaultFn(): number {
  return 1;
}

export const arrowFn = (x: number): number => x * 2;

export class Service {
  static create(): Service {
    return new Service();
  }
}

export function outer(): number {
  function inner(): number {
    return renamed(arrowFn(2));
  }
  return inner();
}
"#;

    let facts = TypeScriptAdapter.extract(Path::new("src/features.ts"), src);
    assert_eq!(facts.parse_errors, 0);
    assert_eq!(facts.module, ["src", "features"]);

    // Imports
    assert_eq!(facts.imports.len(), 1);
    assert_eq!(facts.imports[0].source, "./relative");
    assert_eq!(facts.imports[0].name.as_deref(), Some("original"));
    assert_eq!(facts.imports[0].alias.as_deref(), Some("renamed"));
    assert!(!facts.imports[0].glob);

    // Symbols
    let sym_names: Vec<(&str, SymbolKind, Option<&str>)> = facts
        .symbols
        .iter()
        .map(|s| {
            (
                s.qual_name.as_str(),
                s.kind,
                s.parent.as_ref().map(|p| p.as_str()),
            )
        })
        .collect();

    assert_eq!(
        sym_names,
        [
            ("defaultFn", SymbolKind::Fn, None),
            ("arrowFn", SymbolKind::Fn, None),
            ("Service", SymbolKind::Class, None),
            (
                "Service.create",
                SymbolKind::Method,
                Some("src/features.ts::Service")
            ),
            ("outer", SymbolKind::Fn, None),
            (
                "outer.inner",
                SymbolKind::Fn,
                Some("src/features.ts::outer")
            ),
        ]
    );

    // Static method signature
    let create_sym = facts
        .symbols
        .iter()
        .find(|s| s.qual_name == "Service.create")
        .unwrap();
    let create_sig = &src[create_sym.signature_span.start_byte as usize
        ..create_sym.signature_span.end_byte as usize];
    assert_eq!(create_sig, "static create(): Service");

    // Default export signature
    let def_sym = facts
        .symbols
        .iter()
        .find(|s| s.qual_name == "defaultFn")
        .unwrap();
    let def_sig =
        &src[def_sym.signature_span.start_byte as usize..def_sym.signature_span.end_byte as usize];
    assert_eq!(def_sig, "export default function defaultFn(): number");

    // Nested call ref inside outer.inner
    let f = "src/features.ts";
    assert!(has_ref(
        &facts,
        f,
        (
            "renamed",
            None,
            false,
            17,
            RefKind::Call,
            Some("outer.inner")
        )
    ));
    assert!(has_ref(
        &facts,
        f,
        (
            "arrowFn",
            None,
            false,
            17,
            RefKind::Call,
            Some("outer.inner")
        )
    ));
    assert!(has_ref(
        &facts,
        f,
        ("inner", None, false, 19, RefKind::Call, Some("outer"))
    ));
}

#[test]
fn py_added_language_cases() {
    let src = "from .utils import helper as h\nfrom ..shared import config\n\n@register\n@validate\nclass Model:\n    @property\n    def value(self) -> int:\n        \"\"\"Valor calculado.\"\"\"\n        return 42\n\ndef outer(x: int) -> int:\n    def inner(y: int) -> int:\n        return h(y * 2)\n    return inner(x)\n";

    let facts = PythonAdapter.extract(Path::new("pkg/models.py"), src);
    assert_eq!(facts.parse_errors, 0);
    assert_eq!(facts.module, ["pkg", "models"]);

    // Imports
    assert_eq!(facts.imports.len(), 2);
    assert_eq!(facts.imports[0].source, ".utils");
    assert_eq!(facts.imports[0].name.as_deref(), Some("helper"));
    assert_eq!(facts.imports[0].alias.as_deref(), Some("h"));
    assert_eq!(facts.imports[1].source, "..shared");
    assert_eq!(facts.imports[1].name.as_deref(), Some("config"));
    assert_eq!(facts.imports[1].alias, None);

    // Symbols
    let sym_names: Vec<(&str, SymbolKind, Option<&str>)> = facts
        .symbols
        .iter()
        .map(|s| {
            (
                s.qual_name.as_str(),
                s.kind,
                s.parent.as_ref().map(|p| p.as_str()),
            )
        })
        .collect();

    assert_eq!(
        sym_names,
        [
            ("Model", SymbolKind::Class, None),
            (
                "Model.value",
                SymbolKind::Method,
                Some("pkg/models.py::Model")
            ),
            ("outer", SymbolKind::Fn, None),
            ("outer.inner", SymbolKind::Fn, Some("pkg/models.py::outer")),
        ]
    );

    // Decorator on class and method: decorators included in span, signature starts at class / def
    let model_sym = facts
        .symbols
        .iter()
        .find(|s| s.qual_name == "Model")
        .unwrap();
    assert_eq!(model_sym.span.lines(), [4, 10]);
    let model_sig = &src
        [model_sym.signature_span.start_byte as usize..model_sym.signature_span.end_byte as usize];
    assert_eq!(model_sig, "class Model");

    let val_sym = facts
        .symbols
        .iter()
        .find(|s| s.qual_name == "Model.value")
        .unwrap();
    assert_eq!(val_sym.span.lines(), [7, 10]);
    assert_eq!(val_sym.doc_span.map(|d| d.lines()), Some([9, 9]));
    let val_sig =
        &src[val_sym.signature_span.start_byte as usize..val_sym.signature_span.end_byte as usize];
    assert_eq!(val_sig, "def value(self) -> int");

    // Nested call refs
    let f = "pkg/models.py";
    assert!(has_ref(
        &facts,
        f,
        ("h", None, false, 14, RefKind::Call, Some("outer.inner"))
    ));
    assert!(has_ref(
        &facts,
        f,
        ("inner", None, false, 15, RefKind::Call, Some("outer"))
    ));
}
