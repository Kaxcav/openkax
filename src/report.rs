//! `openkax report` (F5-2): agrega o ledger JSONL (PLANO_MESTRE §3.7) por
//! classe e por provider/modelo. Só leitura; o formato do ledger não muda.
//!
//! Linhas malformadas (JSON inválido, `kind`/`ts` ausentes, campos
//! obrigatórios faltando) são contadas em `malformed` e ignoradas.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::Serialize;
use serde_json::Value;

use crate::ledger::rfc3339;
use crate::mcp::Period;

/// Percentis (nearest-rank) de uma série em ms.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct Percentiles {
    pub p50: Option<u64>,
    pub p95: Option<u64>,
}

fn percentiles(samples: &mut [u64]) -> Percentiles {
    if samples.is_empty() {
        return Percentiles::default();
    }
    samples.sort_unstable();
    let rank = |p: usize| samples[(p * samples.len()).div_ceil(100).max(1) - 1];
    Percentiles {
        p50: Some(rank(50)),
        p95: Some(rank(95)),
    }
}

/// Acumulador de `kind:"request"` de um grupo.
#[derive(Debug, Default, Clone)]
struct ReqAcc {
    requests: u64,
    ok: u64,
    outcomes: BTreeMap<String, u64>,
    latency: Vec<u64>,
    ttft: Vec<u64>,
    tokens_in: u64,
    tokens_out: u64,
    credit: u64,
    paid: u64,
    fallbacks: u64,
}

impl ReqAcc {
    fn finish(mut self) -> RequestStats {
        RequestStats {
            requests: self.requests,
            ok: self.ok,
            ok_rate: ratio(self.ok, self.requests),
            outcomes: self.outcomes,
            latency_ms: percentiles(&mut self.latency),
            ttft_ms: percentiles(&mut self.ttft),
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            tier_credit: self.credit,
            tier_paid: self.paid,
            fallbacks: self.fallbacks,
        }
    }
}

fn ratio(n: u64, d: u64) -> f64 {
    if d == 0 { 0.0 } else { n as f64 / d as f64 }
}

/// Estatísticas de requests de um grupo (classe ou provider/modelo).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RequestStats {
    pub requests: u64,
    pub ok: u64,
    /// `ok / requests` (0 se vazio).
    pub ok_rate: f64,
    pub outcomes: BTreeMap<String, u64>,
    pub latency_ms: Percentiles,
    pub ttft_ms: Percentiles,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Requests atendidas por provider de tier `credit`.
    pub tier_credit: u64,
    /// Requests atendidas por provider de tier `paid`.
    pub tier_paid: u64,
    /// Soma de candidatos que falharam antes do que respondeu.
    pub fallbacks: u64,
}

/// Contagem de veredito de tarefas de um grupo.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct Tally {
    pub total: u32,
    pub accepted: u32,
    pub rejected: u32,
    pub needs_lead: u32,
    /// `accepted / total` (0 se vazio).
    pub acceptance_rate: f64,
    /// Aceitas com `attempts == 1` (métrica de 1ª passada, PLANO §2.3).
    pub first_pass_accepted: u32,
    /// `first_pass_accepted / total` (0 se vazio).
    pub first_pass_rate: f64,
    /// Soma de tentativas (retrabalho = `attempts - total`).
    pub attempts: u32,
}

impl Tally {
    fn add(&mut self, status: &str, attempts: u32) {
        self.total += 1;
        self.attempts += attempts;
        match status {
            "accepted" => {
                self.accepted += 1;
                if attempts == 1 {
                    self.first_pass_accepted += 1;
                }
            }
            "rejected" => self.rejected += 1,
            _ => self.needs_lead += 1,
        }
        self.acceptance_rate = f64::from(self.accepted) / f64::from(self.total);
        self.first_pass_rate = f64::from(self.first_pass_accepted) / f64::from(self.total);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    /// Início da janela (RFC 3339 UTC ou `AAAA-MM-DD`).
    pub since: String,
    /// Linhas ilegíveis ou sem os campos obrigatórios (ignoradas).
    pub malformed: u64,
    pub requests: u64,
    pub tasks: u32,
    pub requests_by_class: BTreeMap<String, RequestStats>,
    /// Chave `provider/modelo`.
    pub requests_by_provider_model: BTreeMap<String, RequestStats>,
    pub tasks_by_class: BTreeMap<String, Tally>,
    pub tasks_by_provider: BTreeMap<String, Tally>,
}

/// Início da janela de um período, a partir de `now`.
pub fn since_for_period(period: Period, now: SystemTime) -> String {
    let days = match period {
        Period::Day => 1,
        Period::Week => 7,
        Period::Month => 30,
    };
    rfc3339(now - Duration::from_secs(days * 86_400))
}

/// Valida `AAAA-MM-DD` (formato e faixas; não checa dias por mês).
pub fn parse_since_date(s: &str) -> Result<String, String> {
    let b = s.as_bytes();
    let shape = b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
    if shape {
        let (m, d) = (s[5..7].parse::<u32>(), s[8..10].parse::<u32>());
        if matches!(m, Ok(1..=12)) && matches!(d, Ok(1..=31)) {
            return Ok(s.to_owned());
        }
    }
    Err(format!("data inválida: {s:?} (use AAAA-MM-DD)"))
}

/// Agrega o ledger de `dir` (`YYYY-MM-DD.jsonl`) a partir de `since`
/// (prefixo de timestamp RFC 3339, comparado como texto). Diretório ausente
/// → relatório vazio.
pub fn build(dir: &Path, since: &str) -> Report {
    let mut by_class: BTreeMap<String, ReqAcc> = BTreeMap::new();
    let mut by_pm: BTreeMap<String, ReqAcc> = BTreeMap::new();
    let mut report = Report {
        since: since.to_owned(),
        malformed: 0,
        requests: 0,
        tasks: 0,
        requests_by_class: BTreeMap::new(),
        requests_by_provider_model: BTreeMap::new(),
        tasks_by_class: BTreeMap::new(),
        tasks_by_provider: BTreeMap::new(),
    };
    let first_day = since.get(..10).unwrap_or(since);
    let Ok(rd) = std::fs::read_dir(dir) else {
        return report;
    };
    for f in rd.flatten() {
        let name = f.file_name().to_string_lossy().into_owned();
        let Some(day) = name.strip_suffix(".jsonl") else {
            continue;
        };
        if day < first_day {
            continue;
        }
        let Ok(bytes) = std::fs::read(f.path()) else {
            continue;
        };
        for line in String::from_utf8_lossy(&bytes).lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                report.malformed += 1;
                continue;
            };
            let (Some(kind), Some(ts)) = (v["kind"].as_str(), v["ts"].as_str()) else {
                report.malformed += 1;
                continue;
            };
            if ts < since {
                continue;
            }
            match kind {
                "request" => {
                    let Some(route) = v["route"].as_str() else {
                        report.malformed += 1;
                        continue;
                    };
                    report.requests += 1;
                    let pm = format!(
                        "{}/{}",
                        v["provider"].as_str().unwrap_or("(nenhum)"),
                        v["model"].as_str().unwrap_or("(nenhum)")
                    );
                    add_request(by_class.entry(route.to_owned()).or_default(), &v);
                    add_request(by_pm.entry(pm).or_default(), &v);
                }
                "task" => {
                    let (Some(class), Some(status)) = (v["class"].as_str(), v["status"].as_str())
                    else {
                        report.malformed += 1;
                        continue;
                    };
                    let attempts = v["attempts"].as_u64().unwrap_or(0) as u32;
                    let provider = v["provider"].as_str().unwrap_or("(nenhum)");
                    report.tasks += 1;
                    report
                        .tasks_by_class
                        .entry(class.to_owned())
                        .or_default()
                        .add(status, attempts);
                    report
                        .tasks_by_provider
                        .entry(provider.to_owned())
                        .or_default()
                        .add(status, attempts);
                }
                _ => {}
            }
        }
    }
    report.requests_by_class = by_class.into_iter().map(|(k, a)| (k, a.finish())).collect();
    report.requests_by_provider_model = by_pm.into_iter().map(|(k, a)| (k, a.finish())).collect();
    report
}

fn add_request(acc: &mut ReqAcc, v: &Value) {
    acc.requests += 1;
    let outcome = v["outcome"].as_str().unwrap_or("unknown");
    if outcome == "ok" {
        acc.ok += 1;
    }
    *acc.outcomes.entry(outcome.to_owned()).or_default() += 1;
    if let Some(l) = v["latency_ms"].as_u64() {
        acc.latency.push(l);
    }
    if let Some(t) = v["ttft_ms"].as_u64() {
        acc.ttft.push(t);
    }
    acc.tokens_in += v["in"].as_u64().unwrap_or(0);
    acc.tokens_out += v["out"].as_u64().unwrap_or(0);
    match v["tier"].as_str() {
        Some("credit") => acc.credit += 1,
        Some("paid") => acc.paid += 1,
        _ => {}
    }
    acc.fallbacks += v["fallbacks"].as_u64().unwrap_or(0);
}

impl Report {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }

    /// Texto legível para terminal.
    pub fn to_text(&self) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        let _ = writeln!(
            s,
            "OpenKax report desde {} — {} request(s), {} tarefa(s), {} linha(s) malformada(s)",
            self.since, self.requests, self.tasks, self.malformed
        );
        let sections = [
            ("Requests por classe", &self.requests_by_class),
            (
                "Requests por provider/modelo",
                &self.requests_by_provider_model,
            ),
        ];
        for (title, map) in sections {
            if map.is_empty() {
                continue;
            }
            let _ = writeln!(s, "\n{title}");
            for (k, r) in map {
                let _ = writeln!(
                    s,
                    "  {k}: {} req, ok {:.0}%, lat p50/p95 {}/{} ms, ttft p50/p95 {}/{} ms, in {} out {}, credit {} paid {}, fallbacks {}",
                    r.requests,
                    r.ok_rate * 100.0,
                    fmt_ms(r.latency_ms.p50),
                    fmt_ms(r.latency_ms.p95),
                    fmt_ms(r.ttft_ms.p50),
                    fmt_ms(r.ttft_ms.p95),
                    r.tokens_in,
                    r.tokens_out,
                    r.tier_credit,
                    r.tier_paid,
                    r.fallbacks
                );
                let outcomes: Vec<String> =
                    r.outcomes.iter().map(|(o, n)| format!("{o}={n}")).collect();
                let _ = writeln!(s, "      outcomes: {}", outcomes.join(" "));
            }
        }
        let task_sections = [
            ("Tarefas por classe", &self.tasks_by_class),
            ("Tarefas por provider", &self.tasks_by_provider),
        ];
        for (title, map) in task_sections {
            if map.is_empty() {
                continue;
            }
            let _ = writeln!(s, "\n{title}");
            for (k, t) in map {
                let _ = writeln!(
                    s,
                    "  {k}: {} tarefa(s), aceitas {} ({:.0}%), 1ª passada {} ({:.0}%), needs_lead {}, rejeitadas {}, tentativas {}",
                    t.total,
                    t.accepted,
                    t.acceptance_rate * 100.0,
                    t.first_pass_accepted,
                    t.first_pass_rate * 100.0,
                    t.needs_lead,
                    t.rejected,
                    t.attempts
                );
            }
        }
        s
    }
}

fn fmt_ms(v: Option<u64>) -> String {
    v.map_or_else(|| "-".into(), |n| n.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(ts: &str, class: &str, provider: &str, status: &str, attempts: u32) -> String {
        format!(
            r#"{{"ts":"{ts}","kind":"task","id":"x","class":"{class}","provider":"{provider}","attempts":{attempts},"status":"{status}","verify_ms":1}}"#
        )
    }

    /// `(ts, route, provider/model, outcome, latência, ttft, tier, fallbacks)`.
    type Req<'a> = (&'a str, &'a str, &'a str, &'a str, u64, u64, &'a str, u32);

    fn req((ts, route, pm, outcome, lat, ttft, tier, fb): Req) -> String {
        let (provider, model) = pm.split_once('/').unwrap();
        format!(
            r#"{{"ts":"{ts}","kind":"request","route":"{route}","provider":"{provider}","model":"{model}","est_in":10,"in":100,"out":20,"latency_ms":{lat},"ttft_ms":{ttft},"fallbacks":{fb},"tier":"{tier}","reasoning":false,"pinned":false,"outcome":"{outcome}"}}"#
        )
    }

    fn write_day(dir: &Path, ts: &str, lines: &[String]) {
        std::fs::write(dir.join(format!("{}.jsonl", &ts[..10])), lines.join("\n")).unwrap();
    }

    #[test]
    fn percentiles_nearest_rank() {
        let mut v: Vec<u64> = (1..=100).collect();
        let p = percentiles(&mut v);
        assert_eq!((p.p50, p.p95), (Some(50), Some(95)));
        assert_eq!(percentiles(&mut [7]).p95, Some(7));
        assert_eq!(percentiles(&mut []), Percentiles::default());
    }

    #[test]
    fn aggregates_requests_and_tasks_ignoring_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let t = rfc3339(now);
        let old = rfc3339(now - Duration::from_secs(40 * 86_400));
        let f = "kax/coder-fast";
        write_day(
            dir.path(),
            &t,
            &[
                req((&t, f, "groq/llama", "ok", 100, 10, "free", 0)),
                req((&t, f, "groq/llama", "ok", 300, 30, "free", 1)),
                req((&t, f, "vercel/gpt", "exhausted", 50, 5, "credit", 2)),
                req((&t, "kax/coder-smart", "deepseek/r1", "ok", 900, 90, "paid", 0)),
                task(&t, f, "groq", "accepted", 1),
                task(&t, f, "gemini", "accepted", 2),
                task(&t, f, "gemini", "needs_lead", 2),
                task(&t, "kax/coder-smart", "gemini", "accepted", 1),
                "lixo que não é json".to_owned(),
                r#"{"sem":"kind"}"#.to_owned(),
                format!(r#"{{"ts":"{t}","kind":"request"}}"#),
                format!(r#"{{"ts":"{t}","kind":"outro"}}"#),
            ],
        );
        write_day(dir.path(), &old, &[task(&old, f, "groq", "accepted", 1)]);

        let r = build(dir.path(), &since_for_period(Period::Week, now));
        assert_eq!((r.requests, r.tasks, r.malformed), (4, 4, 3));

        let fast = &r.requests_by_class[f];
        assert_eq!((fast.requests, fast.ok), (3, 2));
        assert!((fast.ok_rate - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(fast.outcomes["exhausted"], 1);
        assert_eq!(
            (fast.latency_ms.p50, fast.latency_ms.p95),
            (Some(100), Some(300))
        );
        assert_eq!(fast.ttft_ms.p50, Some(10));
        assert_eq!((fast.tokens_in, fast.tokens_out), (300, 60));
        assert_eq!((fast.tier_credit, fast.tier_paid, fast.fallbacks), (1, 0, 3));
        assert_eq!(r.requests_by_class["kax/coder-smart"].tier_paid, 1);
        assert_eq!(r.requests_by_provider_model["groq/llama"].requests, 2);
        assert_eq!(r.requests_by_provider_model["deepseek/r1"].tier_paid, 1);

        let tf = &r.tasks_by_class[f];
        assert_eq!((tf.total, tf.accepted, tf.needs_lead), (3, 2, 1));
        assert_eq!(tf.first_pass_accepted, 1);
        assert!((tf.first_pass_rate - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(tf.attempts, 5);
        assert_eq!(r.tasks_by_class["kax/coder-smart"].first_pass_rate, 1.0);
        assert_eq!(r.tasks_by_provider["gemini"].total, 3);
        assert_eq!(r.tasks_by_provider["groq"].total, 1);

        let json: Value = serde_json::from_str(&r.to_json()).unwrap();
        assert_eq!(json["malformed"], 3);
        assert!(r.to_text().contains(f));
    }

    #[test]
    fn empty_period_and_missing_dir_give_empty_report() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let old = rfc3339(now - Duration::from_secs(40 * 86_400));
        write_day(
            dir.path(),
            &old,
            &[task(&old, "kax/coder-fast", "groq", "accepted", 1)],
        );
        let r = build(dir.path(), &since_for_period(Period::Day, now));
        assert_eq!((r.requests, r.tasks, r.malformed), (0, 0, 0));
        assert!(r.requests_by_class.is_empty() && r.tasks_by_class.is_empty());
        assert!(r.to_text().contains("0 request(s)"));

        let missing = build(Path::new("/nao/existe/ledger"), "2026-01-01");
        assert_eq!(missing.requests + u64::from(missing.tasks), 0);
    }

    #[test]
    fn since_date_filters_by_timestamp_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        let a = "2026-10-30T23:59:59.000Z";
        let b = "2026-11-01T00:00:01.000Z";
        write_day(dir.path(), a, &[task(a, "kax/x", "p", "accepted", 1)]);
        write_day(dir.path(), b, &[task(b, "kax/x", "p", "accepted", 1)]);
        let r = build(dir.path(), &parse_since_date("2026-11-01").unwrap());
        assert_eq!(r.tasks, 1);
        assert!(parse_since_date("2026-13-01").is_err());
        assert!(parse_since_date("01/11/2026").is_err());
        assert!(parse_since_date("2026-11-1x").is_err());
    }
}
