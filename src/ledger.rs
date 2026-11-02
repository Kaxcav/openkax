//! Ledger JSONL (PLANO_MESTRE §3.7): uma linha de **metadados** por request.
//! Nunca corpo, nunca credencial.
//!
//! Fica fora do caminho da request: quem produz faz `try_send` num `mpsc`
//! limitado e segue; uma task dedicada grava em
//! `<dir>/YYYY-MM-DD.jsonl` (data UTC). Fila cheia → descarta e conta.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// Uma linha do ledger. `kind` vira o discriminador no JSON.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LedgerEntry {
    Request(RequestRecord),
    // `Task` (delegate_batch) entra na Fase 2.
}

impl LedgerEntry {
    fn ts(&self) -> &str {
        match self {
            Self::Request(r) => &r.ts,
        }
    }
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
    /// Quantos candidatos falharam antes do que respondeu.
    pub fallbacks: u32,
    pub paid: bool,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    /// Morreu após o 1º token; cliente recebeu `finish_reason: "error"`.
    MidStreamError,
    /// Cliente desconectou depois do 1º token.
    ClientDisconnected,
    /// Nenhum candidato disponível; cliente recebeu 429/503/413.
    Exhausted,
    /// Upstream rejeitou a request como malformada (400/413/422) — A3.
    RequestFault,
    /// Rejeitada antes de qualquer provider (classe desconhecida, pacote grande, JSON inválido).
    Rejected,
}

/// Lado produtor. Clonável e barato; `record` nunca bloqueia.
#[derive(Debug, Clone)]
pub struct LedgerHandle {
    tx: Option<mpsc::Sender<LedgerEntry>>,
    dropped: Arc<AtomicU64>,
}

impl LedgerHandle {
    /// Handle que descarta tudo (testes, `--no-ledger`).
    pub fn disabled() -> Self {
        Self {
            tx: None,
            dropped: Arc::default(),
        }
    }

    pub fn record(&self, entry: LedgerEntry) {
        if let Some(tx) = &self.tx
            && tx.try_send(entry).is_err()
        {
            // Perder uma linha de métrica é preferível a atrasar a request.
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Linhas descartadas por fila cheia ou writer morto.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Sobe a task de escrita em `dir` (precisa de runtime tokio).
pub fn spawn(dir: PathBuf, capacity: usize) -> LedgerHandle {
    let (tx, mut rx) = mpsc::channel::<LedgerEntry>(capacity);
    tokio::spawn(async move {
        let mut current: Option<(String, tokio::fs::File)> = None;
        let mut warned = false;
        while let Some(entry) = rx.recv().await {
            let day = entry.ts().get(..10).unwrap_or("unknown").to_owned();
            let Ok(mut line) = serde_json::to_string(&entry) else {
                continue;
            };
            line.push('\n');
            if current.as_ref().is_none_or(|(d, _)| *d != day) {
                current = open_day(&dir, &day).await.map(|f| (day.clone(), f));
            }
            let ok = match &mut current {
                Some((_, f)) => f.write_all(line.as_bytes()).await.is_ok(),
                None => false,
            };
            if !ok && !warned {
                warned = true;
                tracing::warn!(dir = %dir.display(), "ledger: falha ao gravar; seguindo sem ledger");
            }
        }
    });
    LedgerHandle {
        tx: Some(tx),
        dropped: Arc::default(),
    }
}

async fn open_day(dir: &std::path::Path, day: &str) -> Option<tokio::fs::File> {
    tokio::fs::create_dir_all(dir).await.ok()?;
    tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{day}.jsonl")))
        .await
        .ok()
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn rfc3339(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, day) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60,
        d.subsec_millis()
    )
}

/// Inverso de `days_from_civil` (Howard Hinnant).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn rfc3339_formats_utc() {
        let t = UNIX_EPOCH + Duration::from_millis(1_000_000_000_123);
        assert_eq!(rfc3339(t), "2001-09-09T01:46:40.123Z");
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
        assert_eq!(&rfc3339(leap)[..10], "2000-02-29");
    }

    fn record(ts: &str) -> LedgerEntry {
        LedgerEntry::Request(RequestRecord {
            ts: ts.into(),
            route: "kax/coder-fast".into(),
            provider: Some("gemini".into()),
            model: Some("gemini-flash".into()),
            est_in: 10,
            input_tokens: Some(9),
            output_tokens: Some(3),
            latency_ms: 5,
            ttft_ms: Some(2),
            fallbacks: 1,
            paid: false,
            outcome: Outcome::RequestFault,
        })
    }

    #[tokio::test]
    async fn writer_appends_jsonl_per_utc_day() {
        let dir = std::env::temp_dir().join(format!("openkax-ledger-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let h = spawn(dir.clone(), 8);
        h.record(record("2026-11-02T10:00:00.000Z"));
        h.record(record("2026-11-02T11:00:00.000Z"));
        h.record(record("2026-11-03T00:00:01.000Z"));
        let f1 = dir.join("2026-11-02.jsonl");
        let f2 = dir.join("2026-11-03.jsonl");
        for _ in 0..200 {
            if std::fs::read_to_string(&f2).is_ok_and(|s| s.ends_with('\n')) {
                break;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let day1 = std::fs::read_to_string(&f1).unwrap();
        assert_eq!(day1.lines().count(), 2);
        let v: serde_json::Value = serde_json::from_str(day1.lines().next().unwrap()).unwrap();
        assert_eq!(v["kind"], "request");
        assert_eq!(v["outcome"], "request_fault");
        assert_eq!(v["in"], 9);
        assert_eq!(std::fs::read_to_string(&f2).unwrap().lines().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabled_handle_drops_silently() {
        let h = LedgerHandle::disabled();
        h.record(record("2026-11-02T10:00:00.000Z"));
        assert_eq!(h.dropped(), 0);
    }
}
