//! `report(period)`: aceitação por classe e por provider, a partir das linhas
//! `kind:"task"` do ledger (F2-6). O relatório completo é o F5-2.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::Serialize;
use serde_json::Value;

use super::Period;
use crate::ledger::rfc3339;

/// Contagem de veredito de um grupo (classe ou provider).
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct Tally {
    pub total: u32,
    pub accepted: u32,
    pub rejected: u32,
    pub needs_lead: u32,
    /// `accepted / total` (0 se vazio).
    pub acceptance_rate: f64,
    /// Soma de tentativas (retrabalho = `attempts - total`).
    pub attempts: u32,
}

impl Tally {
    fn add(&mut self, status: &str, attempts: u32) {
        self.total += 1;
        self.attempts += attempts;
        match status {
            "accepted" => self.accepted += 1,
            "rejected" => self.rejected += 1,
            _ => self.needs_lead += 1,
        }
        self.acceptance_rate = f64::from(self.accepted) / f64::from(self.total);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub period: Period,
    /// Início da janela (RFC 3339 UTC).
    pub since: String,
    pub tasks: u32,
    pub by_class: BTreeMap<String, Tally>,
    pub by_provider: BTreeMap<String, Tally>,
}

fn window(p: Period) -> Duration {
    let days = match p {
        Period::Day => 1,
        Period::Week => 7,
        Period::Month => 30,
    };
    Duration::from_secs(days * 86_400)
}

/// Agrega as tarefas de `dir` (`YYYY-MM-DD.jsonl`) a partir de `now - período`.
/// Diretório ausente ou linhas ilegíveis contam como vazio/ignoradas.
pub fn build(dir: &Path, period: Period, now: SystemTime) -> Report {
    let since = rfc3339(now - window(period));
    let mut report = Report {
        period,
        since: since.clone(),
        tasks: 0,
        by_class: BTreeMap::new(),
        by_provider: BTreeMap::new(),
    };
    let Ok(rd) = std::fs::read_dir(dir) else {
        return report;
    };
    let first_day = &since[..10];
    for f in rd.flatten() {
        let name = f.file_name().to_string_lossy().into_owned();
        let Some(day) = name.strip_suffix(".jsonl") else {
            continue;
        };
        if day < first_day {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(f.path()) else {
            continue;
        };
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let ts = v["ts"].as_str().unwrap_or_default();
            if v["kind"] != "task" || ts < since.as_str() {
                continue;
            }
            let status = v["status"].as_str().unwrap_or("needs_lead");
            let attempts = v["attempts"].as_u64().unwrap_or(0) as u32;
            let class = v["class"].as_str().unwrap_or("?").to_owned();
            let provider = v["provider"].as_str().unwrap_or("(nenhum)").to_owned();
            report.tasks += 1;
            report
                .by_class
                .entry(class)
                .or_default()
                .add(status, attempts);
            report
                .by_provider
                .entry(provider)
                .or_default()
                .add(status, attempts);
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(ts: &str, class: &str, provider: &str, status: &str, attempts: u32) -> String {
        format!(
            r#"{{"ts":"{ts}","kind":"task","id":"x","class":"{class}","provider":"{provider}","attempts":{attempts},"status":"{status}","verify_ms":1}}"#
        )
    }

    #[test]
    fn aggregates_by_class_and_provider_within_period() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let today = rfc3339(now);
        let old = rfc3339(now - Duration::from_secs(40 * 86_400));
        let req = format!(r#"{{"ts":"{today}","kind":"request","route":"kax/coder-fast"}}"#);
        let body = [
            line(&today, "kax/coder-fast", "groq", "accepted", 1),
            line(&today, "kax/coder-fast", "gemini", "accepted", 2),
            line(&today, "kax/coder-fast", "gemini", "needs_lead", 2),
            line(&today, "kax/coder-smart", "gemini", "accepted", 1),
            req,
            "lixo que não é json".to_owned(),
        ]
        .join("\n");
        std::fs::write(dir.path().join(format!("{}.jsonl", &today[..10])), body).unwrap();
        std::fs::write(
            dir.path().join(format!("{}.jsonl", &old[..10])),
            line(&old, "kax/coder-fast", "groq", "accepted", 1),
        )
        .unwrap();

        let r = build(dir.path(), Period::Week, now);
        assert_eq!(r.tasks, 4);
        let fast = &r.by_class["kax/coder-fast"];
        assert_eq!((fast.total, fast.accepted, fast.needs_lead), (3, 2, 1));
        assert!((fast.acceptance_rate - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(fast.attempts, 5);
        assert_eq!(r.by_class["kax/coder-smart"].acceptance_rate, 1.0);
        assert_eq!(r.by_provider["gemini"].total, 3);
        assert_eq!(r.by_provider["groq"].total, 1);
    }

    #[test]
    fn missing_dir_is_empty_report() {
        let r = build(
            Path::new("/nao/existe/ledger"),
            Period::Day,
            SystemTime::now(),
        );
        assert_eq!(r.tasks, 0);
        assert!(r.by_class.is_empty());
    }
}
