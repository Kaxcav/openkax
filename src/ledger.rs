//! Ledger JSONL (PLANO_MESTRE §3.7): uma linha de **metadados** por request.
//! Nunca corpo, nunca credencial.
//!
//! Fica fora do caminho da request: quem produz faz `try_send` num `mpsc`
//! limitado e segue; uma task dedicada grava em
//! `<data_dir>/ledger/YYYY-MM-DD.jsonl`. Fila cheia → descarta e conta.

use std::path::PathBuf;

use serde::Serialize;
use tokio::sync::mpsc;

/// Uma linha do ledger. `kind` vira o discriminador no JSON.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LedgerEntry {
    Request(RequestRecord),
    // `Task` (delegate_batch) entra na Fase 2.
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RequestRecord {
    /// RFC 3339 UTC.
    pub ts: String,
    /// Classe (`kax/coder-fast`) ou `claude-*`.
    pub route: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub est_in: u32,
    #[serde(rename = "in")]
    pub input_tokens: Option<u32>,
    #[serde(rename = "out")]
    pub output_tokens: Option<u32>,
    pub latency_ms: u64,
    pub ttft_ms: Option<u64>,
    /// Quantos candidatos foram tentados antes do que respondeu.
    pub fallbacks: u32,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    /// Morreu após o 1º token; cliente recebeu `finish_reason: "error"`.
    MidStreamError,
    /// Nenhum candidato disponível; cliente recebeu 429.
    Exhausted,
    /// Todos os candidatos tentados falharam antes do 1º token.
    AllFailed,
    /// Rejeitada antes de qualquer provider (classe desconhecida, pacote grande).
    Rejected,
}

/// Lado produtor. Clonável e barato; `record` nunca bloqueia.
#[derive(Debug, Clone)]
pub struct LedgerHandle {
    tx: Option<mpsc::Sender<LedgerEntry>>,
}

impl LedgerHandle {
    /// Handle que descarta tudo (testes, `--no-ledger`).
    pub fn disabled() -> Self {
        Self { tx: None }
    }

    pub fn record(&self, entry: LedgerEntry) {
        if let Some(tx) = &self.tx {
            // Fila cheia ou writer morto: perder uma linha de métrica é
            // preferível a atrasar a request.
            let _ = tx.try_send(entry);
        }
    }
}

/// Sobe a task de escrita em `dir`. Retorna o handle produtor.
pub fn spawn(_dir: PathBuf, capacity: usize) -> LedgerHandle {
    // CONTRACT: writer real (rotação diária, flush) junto com o dispatcher.
    let (tx, mut rx) = mpsc::channel(capacity);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    LedgerHandle { tx: Some(tx) }
}
