//! Índice de símbolos de um repo, incremental e persistido (ADR 0009,
//! decisão 4).
//!
//! - **Percurso** (`refresh`): crate `ignore` com `.gitignore`, `.ignore` e
//!   `.git/info/exclude`, `require_git(false)`, sem ocultos, sempre sem `.git/`
//!   e `.kax/`, sem seguir links; só arquivos com adapter
//!   ([`super::adapter_for`]); acima de [`super::MAX_FILE_BYTES`] ou não UTF-8
//!   → `skipped`. Os `.gitignore` acima da raiz não valem: a raiz é um repo ou
//!   um worktree inteiro.
//! - **Invalidação**: [`content_hash`] do conteúdo bruto + id do adapter +
//!   [`super::INDEX_FORMAT_VERSION`]. Igual → reaproveita os fatos sem parse.
//!   O texto vai ao adapter normalizado (CRLF → LF).
//! - **Persistência** (`save`): [`StoredIndex`] em JSON em
//!   `<store>/index.json`, gravado num temporário e trocado por rename; o
//!   store ganha um `.gitignore` com `*`. Último a gravar vence. O `open` é
//!   tolerante: índice ilegível ou de outra versão vira índice vazio, e o
//!   próximo `refresh` reconstrói tudo.
//! - **Queries**: `name` (`validate`), caminho qualificado casado pelo sufixo
//!   (`Rect::new`, `Rect.square`, `geometry::validate`) ou id
//!   (`src/store.rs::validate`, reconhecido pelo 1º segmento terminar numa
//!   extensão com adapter). O texto (assinatura, doc, trechos) é relido do
//!   disco e conferido pelo hash: arquivo alterado depois do `refresh` dá
//!   erro em vez de linhas trocadas.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::{
    ContextError, ContextPack, FileFacts, INDEX_FORMAT_VERSION, MAX_FILE_BYTES, RefSite, Span,
    Symbol, SymbolDef, SymbolSummary, SymbolTable, adapter_for,
};

/// Nome do arquivo do índice dentro do store.
pub const INDEX_FILE: &str = "index.json";

/// Store padrão do repo principal: `<repo>/.kax/context/`.
pub fn store_dir(repo: &Path) -> PathBuf {
    repo.join(".kax").join("context")
}

/// Hash de invalidação: xxh3-128 do conteúdo bruto, em 32 dígitos hex.
pub fn content_hash(bytes: &[u8]) -> String {
    format!("{:032x}", xxhash_rust::xxh3::xxh3_128(bytes))
}

/// CRLF → LF, a forma em que os spans valem.
pub fn normalize_newlines(text: String) -> String {
    if text.contains("\r\n") {
        text.replace("\r\n", "\n")
    } else {
        text
    }
}

/// Formato de `index.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredIndex {
    /// [`super::INDEX_FORMAT_VERSION`]; outra versão → tudo é reparseado.
    pub format: u32,
    /// Por arquivo relativo à raiz, com `/`.
    pub files: BTreeMap<String, StoredFile>,
}

/// Entrada de um arquivo no índice persistido.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredFile {
    /// [`content_hash`] do conteúdo bruto (antes de normalizar CRLF).
    pub hash: String,
    /// [`super::LanguageAdapter::id`] que extraiu os fatos.
    pub adapter: String,
    pub facts: FileFacts,
}

/// Mesma forma de [`StoredIndex`], emprestada da tabela para gravar sem copiar
/// os fatos.
#[derive(Serialize)]
struct StoredIndexRef<'a> {
    format: u32,
    files: BTreeMap<&'a str, StoredFileRef<'a>>,
}

#[derive(Serialize)]
struct StoredFileRef<'a> {
    hash: &'a str,
    adapter: &'a str,
    facts: &'a FileFacts,
}

/// Resultado de um `refresh`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshStats {
    /// Arquivos (re)parseados: novos, alterados ou de outra versão.
    pub parsed: u32,
    /// Arquivos com hash igual, fatos reaproveitados sem parse.
    pub reused: u32,
    /// Arquivos que sumiram (apagados ou passaram a ser ignorados).
    pub removed: u32,
    /// Grandes demais ou não UTF-8.
    pub skipped: u32,
}

/// Estado acumulado do índice desde o `open`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexStats {
    pub files: u32,
    pub symbols: u32,
    /// Chamadas a [`super::LanguageAdapter::extract`] desde o `open`.
    pub parses_total: u64,
}

/// Hash e adapter de um arquivo indexado.
#[derive(Debug, Clone)]
struct FileMeta {
    hash: String,
    adapter: &'static str,
}

/// Índice de um repo: a raiz percorrida e o store onde ele é persistido.
#[derive(Debug)]
pub struct Index {
    root: PathBuf,
    store: PathBuf,
    table: SymbolTable,
    stats: IndexStats,
    meta: BTreeMap<String, FileMeta>,
}

impl Index {
    /// Abre o índice de `root` (o worktree da tarefa), com o store em `store`
    /// (normalmente [`store_dir`] do repo principal). Carrega `index.json` se
    /// existir e for desta versão; não percorre a árvore (isso é `refresh`).
    pub fn open(root: &Path, store: &Path) -> Result<Self, ContextError> {
        if !root.is_dir() {
            return Err(ContextError::Io {
                path: root.display().to_string(),
                source: io::Error::new(io::ErrorKind::NotFound, "a raiz não é um diretório"),
            });
        }
        let mut idx = Self {
            root: root.to_path_buf(),
            store: store.to_path_buf(),
            table: SymbolTable::new(),
            stats: IndexStats::default(),
            meta: BTreeMap::new(),
        };
        for (file, f) in load(&store.join(INDEX_FILE)) {
            let Some(adapter) = adapter_for(Path::new(&file)) else {
                continue;
            };
            if f.adapter != adapter.id() || !is_relative_clean(&file) {
                continue;
            }
            idx.table.insert_file(&file, f.facts)?;
            idx.meta.insert(
                file,
                FileMeta {
                    hash: f.hash,
                    adapter: adapter.id(),
                },
            );
        }
        idx.update_counts();
        Ok(idx)
    }

    /// Percorre a raiz e reparseia só o que mudou.
    pub fn refresh(&mut self) -> Result<RefreshStats, ContextError> {
        let mut st = RefreshStats::default();
        let mut seen: HashSet<String> = HashSet::new();
        let walker = ignore::WalkBuilder::new(&self.root)
            .hidden(true)
            .parents(false)
            .ignore(true)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(true)
            .require_git(false)
            .follow_links(false)
            .filter_entry(|e| {
                !(e.depth() > 0
                    && e.file_type().is_some_and(|t| t.is_dir())
                    && matches!(e.file_name().to_str(), Some(".git" | ".kax")))
            })
            .build();
        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    tracing::debug!(%err, "kax-context: entrada do percurso ignorada");
                    continue;
                }
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            let Some(adapter) = adapter_for(path) else {
                continue;
            };
            let Some(rel) = relative(&self.root, path) else {
                st.skipped += 1;
                continue;
            };
            let too_big = entry.metadata().map_or(true, |m| m.len() > MAX_FILE_BYTES);
            let bytes = if too_big { None } else { fs::read(path).ok() };
            let Some(bytes) = bytes.filter(|b| b.len() as u64 <= MAX_FILE_BYTES) else {
                st.skipped += 1;
                continue;
            };
            let hash = content_hash(&bytes);
            if self
                .meta
                .get(&rel)
                .is_some_and(|m| m.hash == hash && m.adapter == adapter.id())
            {
                st.reused += 1;
                seen.insert(rel);
                continue;
            }
            let Ok(text) = String::from_utf8(bytes) else {
                st.skipped += 1;
                continue;
            };
            let text = normalize_newlines(text);
            let facts = adapter.extract(Path::new(&rel), &text);
            self.stats.parses_total += 1;
            st.parsed += 1;
            self.table.insert_file(&rel, facts)?;
            self.meta.insert(
                rel.clone(),
                FileMeta {
                    hash,
                    adapter: adapter.id(),
                },
            );
            seen.insert(rel);
        }
        let gone: Vec<String> = self
            .meta
            .keys()
            .filter(|f| !seen.contains(*f))
            .cloned()
            .collect();
        for f in gone {
            self.meta.remove(&f);
            self.table.remove_file(&f)?;
            st.removed += 1;
        }
        self.update_counts();
        Ok(st)
    }

    /// Grava `index.json` no store.
    pub fn save(&self) -> Result<(), ContextError> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        fs::create_dir_all(&self.store).map_err(io_at(&self.store))?;
        let gitignore = self.store.join(".gitignore");
        if !gitignore.exists() {
            fs::write(&gitignore, "*\n").map_err(io_at(&gitignore))?;
        }
        let files = self
            .meta
            .iter()
            .filter_map(|(f, m)| {
                let facts = self.table.file(f)?;
                Some((
                    f.as_str(),
                    StoredFileRef {
                        hash: &m.hash,
                        adapter: m.adapter,
                        facts,
                    },
                ))
            })
            .collect();
        let target = self.store.join(INDEX_FILE);
        let body = serde_json::to_vec(&StoredIndexRef {
            format: INDEX_FORMAT_VERSION,
            files,
        })
        .map_err(|e| ContextError::Store {
            path: target.display().to_string(),
            reason: e.to_string(),
        })?;
        let tmp = self.store.join(format!(
            "{INDEX_FILE}.{}.{}.tmp",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&tmp, &body).map_err(io_at(&tmp))?;
        fs::rename(&tmp, &target).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            io_at(&target)(e)
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn store(&self) -> &Path {
        &self.store
    }

    pub fn stats(&self) -> IndexStats {
        self.stats
    }

    /// Tabela de definições e imports usada pela resolução.
    pub fn table(&self) -> &SymbolTable {
        &self.table
    }

    /// Símbolos de `file` (relativo à raiz, com `/`), em ordem do fonte.
    pub fn outline(&self, file: &str) -> Result<Vec<SymbolSummary>, ContextError> {
        let file = clean_query_path(file)?;
        let facts = self
            .table
            .file(&file)
            .ok_or_else(|| ContextError::UnknownFile(file.clone()))?;
        let src = self.source(&file)?;
        Ok(facts.symbols.iter().map(|s| summary(s, &src)).collect())
    }

    /// Definições que casam com `query`; com mais de uma, todas vêm com
    /// `ambiguous = true`. Nenhuma → [`ContextError::UnknownSymbol`].
    pub fn symbol(&self, query: &str) -> Result<Vec<SymbolDef>, ContextError> {
        let ids = self.table.lookup(query)?;
        if ids.is_empty() {
            return Err(ContextError::UnknownSymbol(query.to_string()));
        }
        let ambiguous = ids.len() > 1;
        let mut texts = Sources::new(self);
        let mut out = Vec::with_capacity(ids.len());
        for id in &ids {
            let Some(s) = self.table.symbol(id) else {
                continue;
            };
            let src = texts.get(&s.file)?;
            out.push(SymbolDef {
                symbol: s.clone(),
                signature: slice(src, s.signature_span).to_string(),
                doc: s
                    .doc_span
                    .map(|d| slice(src, d).trim_end_matches('\n').to_string()),
                ambiguous,
            });
        }
        Ok(out)
    }

    /// Sítios (`call`, `type`, `use`) cuja resolução menciona uma definição
    /// que casa com `query`, em ordem de arquivo e linha.
    pub fn refs(&self, query: &str) -> Result<Vec<RefSite>, ContextError> {
        self.sites(query, false)
    }

    /// Como `refs`, só os sítios `call`; `enclosing` é o chamador.
    pub fn callers(&self, query: &str) -> Result<Vec<RefSite>, ContextError> {
        self.sites(query, true)
    }

    fn sites(&self, query: &str, only_calls: bool) -> Result<Vec<RefSite>, ContextError> {
        let defs = self.table.lookup(query)?;
        if defs.is_empty() {
            return Err(ContextError::UnknownSymbol(query.to_string()));
        }
        let mut texts = Sources::new(self);
        let mut out = Vec::new();
        for (r, resolution) in self.table.sites(&defs, only_calls) {
            let enclosing = match r.enclosing.as_ref().and_then(|id| self.table.symbol(id)) {
                Some(s) => Some(summary(s, texts.get(&s.file)?)),
                None => None,
            };
            out.push(RefSite {
                file: r.file.clone(),
                lines: r.span.lines(),
                kind: r.kind,
                enclosing,
                resolution,
            });
        }
        Ok(out)
    }

    /// Pacote de contexto dos `targets` (queries; cada uma precisa casar com
    /// uma definição só) dentro de `budget_tokens`.
    pub fn pack(
        &self,
        _targets: &[&str],
        _budget_tokens: u32,
        _chars_per_token: f64,
    ) -> Result<ContextPack, ContextError> {
        Err(ContextError::NotImplemented("P6c: Index::pack"))
    }

    /// Texto normalizado de `file`, conferido contra o hash do `refresh`.
    pub fn source(&self, file: &str) -> Result<String, ContextError> {
        let meta = self
            .meta
            .get(file)
            .ok_or_else(|| ContextError::UnknownFile(file.to_string()))?;
        let path = self.root.join(file);
        let bytes = fs::read(&path).map_err(io_at(&path))?;
        if content_hash(&bytes) != meta.hash {
            return Err(io_at(&path)(io::Error::other(
                "o arquivo mudou depois do último refresh do índice",
            )));
        }
        let text = String::from_utf8(bytes)
            .map_err(|e| io_at(&path)(io::Error::new(io::ErrorKind::InvalidData, e)))?;
        Ok(normalize_newlines(text))
    }

    fn update_counts(&mut self) {
        self.stats.files = self.meta.len() as u32;
        self.stats.symbols = self.table.symbol_count() as u32;
    }
}

/// Textos já lidos numa consulta.
struct Sources<'a> {
    index: &'a Index,
    cache: HashMap<String, String>,
}

impl<'a> Sources<'a> {
    fn new(index: &'a Index) -> Self {
        Self {
            index,
            cache: HashMap::new(),
        }
    }

    fn get(&mut self, file: &str) -> Result<&str, ContextError> {
        if !self.cache.contains_key(file) {
            let text = self.index.source(file)?;
            self.cache.insert(file.to_string(), text);
        }
        Ok(self.cache[file].as_str())
    }
}

/// Erro de E/S em `path`.
fn io_at(path: &Path) -> impl Fn(io::Error) -> ContextError + '_ {
    move |source| ContextError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// Texto de um span; vazio se o span não couber no texto.
pub(crate) fn slice(src: &str, span: Span) -> &str {
    src.get(span.start_byte as usize..span.end_byte as usize)
        .unwrap_or("")
}

fn summary(s: &Symbol, src: &str) -> SymbolSummary {
    SymbolSummary {
        id: s.id.clone(),
        name: s.name.clone(),
        qual_name: s.qual_name.clone(),
        kind: s.kind,
        lines: s.span.lines(),
        signature: slice(src, s.signature_span).to_string(),
        parent: s.parent.clone(),
    }
}

/// Entradas de `index.json`, ou nenhuma (ausente, ilegível ou de outra
/// versão: o próximo `refresh` reconstrói).
fn load(path: &Path) -> BTreeMap<String, StoredFile> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return BTreeMap::new(),
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "kax-context: índice ilegível; reconstruindo");
            return BTreeMap::new();
        }
    };
    match serde_json::from_slice::<StoredIndex>(&bytes) {
        Ok(s) if s.format == INDEX_FORMAT_VERSION => s.files,
        Ok(s) => {
            tracing::info!(
                path = %path.display(),
                format = s.format,
                "kax-context: índice de outra versão; reconstruindo"
            );
            BTreeMap::new()
        }
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "kax-context: índice inválido; reconstruindo");
            BTreeMap::new()
        }
    }
}

/// `path` relativo a `root`, com `/`; `None` se escapar ou não for UTF-8.
fn relative(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(p) => parts.push(p.to_str()?),
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Caminho relativo sem `..`, sem raiz e sem prefixo de drive.
fn is_relative_clean(file: &str) -> bool {
    !file.is_empty()
        && Path::new(file)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

/// Normaliza o `file` de uma consulta (`\` → `/`, sem `./`) e recusa o que
/// sai da raiz.
fn clean_query_path(file: &str) -> Result<String, ContextError> {
    let f = file.replace('\\', "/");
    let f = f.trim_start_matches("./");
    if !is_relative_clean(f) {
        return Err(ContextError::OutOfRoot(file.to_string()));
    }
    Ok(f.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_lives_under_kax() {
        let s = store_dir(Path::new("repo"));
        assert_eq!(s, Path::new("repo").join(".kax").join("context"));
    }

    /// O hash entra no formato persistido: trocar de função invalida todos os
    /// índices, então o valor fica preso por teste (vetor do xxh3-128).
    #[test]
    fn content_hash_is_xxh3_128_hex() {
        assert_eq!(content_hash(b""), "99aa06d3014798d86001c324468d497f");
        assert_eq!(content_hash(b"fn a() {}\n").len(), 32);
        assert_ne!(content_hash(b"a\r\n"), content_hash(b"a\n"));
    }

    #[test]
    fn query_paths_stay_inside_the_root() {
        assert_eq!(clean_query_path("./src\\a.rs").unwrap(), "src/a.rs");
        for bad in ["../x.rs", "/etc/x.rs", "src/../../x.rs", ""] {
            assert!(
                matches!(clean_query_path(bad), Err(ContextError::OutOfRoot(_))),
                "{bad}"
            );
        }
    }

    fn tiny_repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join("src")).unwrap();
        fs::write(d.path().join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        d
    }

    /// Índice ilegível ou de outra versão não é erro: reconstrói do zero.
    #[test]
    fn unreadable_or_old_store_rebuilds() {
        let d = tiny_repo();
        let store = store_dir(d.path());
        fs::create_dir_all(&store).unwrap();
        for body in [
            "isto não é json".to_string(),
            format!(r#"{{"format":{},"files":{{}}}}"#, INDEX_FORMAT_VERSION + 1),
            r#"{"format":1,"files":{"src/lib.rs":{"hash":"x"}}}"#.to_string(),
        ] {
            fs::write(store.join(INDEX_FILE), body).unwrap();
            let mut idx = Index::open(d.path(), &store).unwrap();
            assert_eq!(idx.stats().files, 0);
            let st = idx.refresh().unwrap();
            assert_eq!((st.parsed, st.reused), (1, 0));
        }
    }

    /// Entrada persistida com caminho que sai da raiz é descartada no `open`.
    #[test]
    fn stored_paths_outside_the_root_are_dropped() {
        let d = tiny_repo();
        let store = store_dir(d.path());
        let mut idx = Index::open(d.path(), &store).unwrap();
        idx.refresh().unwrap();
        idx.save().unwrap();
        let text = fs::read_to_string(store.join(INDEX_FILE)).unwrap();
        fs::write(
            store.join(INDEX_FILE),
            text.replace("\"src/lib.rs\"", "\"../lib.rs\""),
        )
        .unwrap();
        let idx = Index::open(d.path(), &store).unwrap();
        assert_eq!(idx.stats().files, 0);
    }

    /// Arquivo alterado depois do `refresh`: as consultas recusam em vez de
    /// devolver linhas trocadas.
    #[test]
    fn stale_source_is_an_error() {
        let d = tiny_repo();
        let mut idx = Index::open(d.path(), &store_dir(d.path())).unwrap();
        idx.refresh().unwrap();
        assert_eq!(idx.outline("src/lib.rs").unwrap().len(), 1);
        fs::write(d.path().join("src/lib.rs"), "\n\npub fn a() {}\n").unwrap();
        assert!(matches!(
            idx.outline("src/lib.rs"),
            Err(ContextError::Io { .. })
        ));
        idx.refresh().unwrap();
        assert_eq!(idx.outline("src/lib.rs").unwrap()[0].lines, [3, 3]);
    }
}
