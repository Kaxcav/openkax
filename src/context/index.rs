//! Índice de símbolos de um repo, incremental e persistido (ADR 0009,
//! decisão 4). Lógica: P6c.
//!
//! - **Percurso** (`refresh`): crate `ignore` com `.gitignore`, `.ignore` e
//!   `.git/info/exclude`, `require_git(false)`, sem ocultos, sempre sem `.git/`
//!   e `.kax/`; só arquivos com adapter ([`super::adapter_for`]); acima de
//!   [`super::MAX_FILE_BYTES`] ou não UTF-8 → `skipped`.
//! - **Invalidação**: [`content_hash`] do conteúdo bruto + id do adapter +
//!   [`super::INDEX_FORMAT_VERSION`]. Igual → reaproveita os fatos sem parse.
//!   O texto vai ao adapter normalizado (CRLF → LF).
//! - **Persistência** (`save`): [`StoredIndex`] em JSON em
//!   `<store>/index.json`, gravado num temporário e trocado por rename; o
//!   store ganha um `.gitignore` com `*`. Último a gravar vence.
//! - **Queries**: `name` (`validate`), caminho qualificado casado pelo sufixo
//!   (`Rect::new`, `Rect.square`, `geometry::validate`) ou id
//!   (`src/store.rs::validate`, reconhecido pelo 1º segmento terminar numa
//!   extensão com adapter).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{ContextError, ContextPack, FileFacts, RefSite, SymbolDef, SymbolSummary, SymbolTable};

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

/// Índice de um repo: a raiz percorrida e o store onde ele é persistido.
#[derive(Debug)]
pub struct Index {
    root: PathBuf,
    store: PathBuf,
    table: SymbolTable,
    stats: IndexStats,
}

impl Index {
    /// Abre o índice de `root` (o worktree da tarefa), com o store em `store`
    /// (normalmente [`store_dir`] do repo principal). Carrega `index.json` se
    /// existir e for desta versão; não percorre a árvore (isso é `refresh`).
    pub fn open(_root: &Path, _store: &Path) -> Result<Self, ContextError> {
        Err(ContextError::NotImplemented("P6c: Index::open"))
    }

    /// Percorre a raiz e reparseia só o que mudou.
    pub fn refresh(&mut self) -> Result<RefreshStats, ContextError> {
        Err(ContextError::NotImplemented("P6c: Index::refresh"))
    }

    /// Grava `index.json` no store.
    pub fn save(&self) -> Result<(), ContextError> {
        Err(ContextError::NotImplemented("P6c: Index::save"))
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
    pub fn outline(&self, _file: &str) -> Result<Vec<SymbolSummary>, ContextError> {
        Err(ContextError::NotImplemented("P6c: Index::outline"))
    }

    /// Definições que casam com `query`; com mais de uma, todas vêm com
    /// `ambiguous = true`. Nenhuma → [`ContextError::UnknownSymbol`].
    pub fn symbol(&self, _query: &str) -> Result<Vec<SymbolDef>, ContextError> {
        Err(ContextError::NotImplemented("P6c: Index::symbol"))
    }

    /// Sítios (`call`, `type`, `use`) cuja resolução menciona uma definição
    /// que casa com `query`, em ordem de arquivo e linha.
    pub fn refs(&self, _query: &str) -> Result<Vec<RefSite>, ContextError> {
        Err(ContextError::NotImplemented("P6c: Index::refs"))
    }

    /// Como `refs`, só os sítios `call`; `enclosing` é o chamador.
    pub fn callers(&self, _query: &str) -> Result<Vec<RefSite>, ContextError> {
        Err(ContextError::NotImplemented("P6c: Index::callers"))
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
}
