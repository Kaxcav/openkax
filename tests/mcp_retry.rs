//! F2-5: tentativa, retry no próximo candidato e resultado de `delegate`.
//! Dois providers `wiremock` (groq → gemini) e um repo git temporário; o verify
//! é `git grep`, que roda igual em Windows e Linux.

mod common;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use common::*;
use openkax::config::Config;
use openkax::ledger;
use openkax::mcp::{TaskPacket, TaskStatus, run_task};
use openkax::provider::Secrets;
use openkax::server::{self, AppState, Deps};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn temp_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("value.txt"), "OLD\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

fn diff_to(value: &str) -> String {
    format!("```diff\n--- a/value.txt\n+++ b/value.txt\n@@ -1 +1 @@\n-OLD\n+{value}\n```")
}

fn packet(id: &str, max_attempts: u32) -> TaskPacket {
    serde_json::from_value(json!({
        "id": id,
        "class": "kax/coder-fast",
        "objective": "Trocar OLD por GOOD",
        "files": [{ "path": "value.txt", "mode": "write" }],
        "accept": { "verify": ["git grep -q GOOD -- value.txt"], "expect": "exit 0" },
        "max_attempts": max_attempts
    }))
    .unwrap()
}

fn app(groq: &MockServer, gemini: &MockServer, ledger_dir: &Path) -> AppState {
    server::install_crypto_provider();
    let config = Config::from_toml_str(&two_provider_config(&groq.uri(), &gemini.uri())).unwrap();
    let mut deps = Deps::new(config, Secrets::from_pairs(SECRETS.iter().copied()));
    deps.ledger = ledger::spawn(ledger_dir.to_owned(), 16);
    AppState::new(deps).unwrap()
}

async fn mount(server: &MockServer, content: &str, times: u64) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_ok(content))
        .expect(times)
        .mount(server)
        .await;
}

async fn task_ledger_line(dir: &Path) -> Value {
    for _ in 0..200 {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for f in rd.flatten() {
                let text = std::fs::read_to_string(f.path()).unwrap_or_default();
                for line in text.lines() {
                    let v: Value = serde_json::from_str(line).unwrap();
                    if v["kind"] == "task" {
                        return v;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("linha kind:task não apareceu no ledger");
}

/// 1ª tentativa quebra o verify → 2ª vai para o OUTRO provider e é aceita.
#[tokio::test]
async fn retry_goes_to_next_provider_and_is_accepted() {
    let (groq, gemini) = (MockServer::start().await, MockServer::start().await);
    mount(&groq, &diff_to("BAD"), 1).await;
    mount(&gemini, &diff_to("GOOD"), 1).await;
    let repo = temp_repo();
    let ledger_dir = tempfile::tempdir().unwrap();
    let app = app(&groq, &gemini, ledger_dir.path());

    let r = run_task(&app, repo.path(), "main", &packet("T-1", 2)).await;

    assert_eq!(r.status, TaskStatus::Accepted, "{r:?}");
    assert_eq!(r.attempts, 2);
    assert_eq!(r.provider, "gemini");
    assert!(r.diff.contains("+GOOD"), "{}", r.diff);
    groq.verify().await;
    gemini.verify().await;

    // A branch kax/T-1 tem o commit da tarefa para o lead integrar.
    let subject = git(repo.path(), &["log", "kax/T-1", "-1", "--format=%s"]);
    assert_eq!(subject.trim(), "kax(T-1): Trocar OLD por GOOD");
    let blob = git(repo.path(), &["show", "kax/T-1:value.txt"]);
    assert_eq!(blob.trim(), "GOOD");

    let line = task_ledger_line(ledger_dir.path()).await;
    assert_eq!(line["id"], "T-1");
    assert_eq!(line["class"], "kax/coder-fast");
    assert_eq!(line["provider"], "gemini");
    assert_eq!(line["attempts"], 2);
    assert_eq!(line["status"], "accepted");
    assert!(line["verify_ms"].is_u64(), "{line}");
}

/// As duas falham → `needs_lead`, worktree preservado e cada provider chamado 1x.
#[tokio::test]
async fn both_fail_needs_lead_and_keeps_worktree() {
    let (groq, gemini) = (MockServer::start().await, MockServer::start().await);
    mount(&groq, &diff_to("BAD1"), 1).await;
    mount(&gemini, &diff_to("BAD2"), 1).await;
    let repo = temp_repo();
    let ledger_dir = tempfile::tempdir().unwrap();
    let app = app(&groq, &gemini, ledger_dir.path());

    let r = run_task(&app, repo.path(), "main", &packet("T-2", 2)).await;

    assert_eq!(r.status, TaskStatus::NeedsLead, "{r:?}");
    assert_eq!(r.attempts, 2);
    assert_eq!(r.provider, "gemini");
    assert!(!r.verify_log.is_empty());
    groq.verify().await;
    gemini.verify().await;
    assert!(repo.path().join(".kax/wt/T-2/value.txt").exists());

    let line = task_ledger_line(ledger_dir.path()).await;
    assert_eq!(line["status"], "needs_lead");
    assert_eq!(line["attempts"], 2);
}

/// `max_attempts = 1`: sem retry, o 2º provider nem é chamado.
#[tokio::test]
async fn max_attempts_one_does_not_retry() {
    let (groq, gemini) = (MockServer::start().await, MockServer::start().await);
    mount(&groq, &diff_to("BAD"), 1).await;
    mount(&gemini, &diff_to("GOOD"), 0).await;
    let repo = temp_repo();
    let app = app(&groq, &gemini, tempfile::tempdir().unwrap().path());

    let r = run_task(&app, repo.path(), "main", &packet("T-3", 1)).await;

    assert_eq!(r.status, TaskStatus::NeedsLead, "{r:?}");
    assert_eq!(r.attempts, 1);
    groq.verify().await;
    gemini.verify().await;
}

/// C2: o verify roda código do operário; o que ele deixar fora da lista `write`
/// não vira commit. A tarefa vai para `needs_lead` e a branch fica no base.
#[tokio::test]
async fn verify_side_effect_outside_write_list_is_not_committed() {
    let (groq, gemini) = (MockServer::start().await, MockServer::start().await);
    mount(&groq, &diff_to("GOOD"), 1).await;
    mount(&gemini, &diff_to("GOOD"), 0).await;
    let repo = temp_repo();
    let app = app(&groq, &gemini, tempfile::tempdir().unwrap().path());
    let packet: TaskPacket = serde_json::from_value(json!({
        "id": "T-5",
        "class": "kax/coder-fast",
        "objective": "Trocar OLD por GOOD",
        "files": [{ "path": "value.txt", "mode": "write" }],
        "accept": {
            "verify": ["git grep -q GOOD -- value.txt", "echo x> evil.txt"],
            "expect": "exit 0"
        },
        "max_attempts": 2
    }))
    .unwrap();

    let r = run_task(&app, repo.path(), "main", &packet).await;

    assert_eq!(r.status, TaskStatus::NeedsLead, "{r:?}");
    assert_eq!(r.attempts, 1, "não adianta tentar outro provider");
    assert!(
        r.verify_log.contains("depois do verify"),
        "{}",
        r.verify_log
    );
    assert!(r.verify_log.contains("evil.txt"), "{}", r.verify_log);
    groq.verify().await;
    gemini.verify().await;
    let ahead = git(repo.path(), &["rev-list", "--count", "main..kax/T-5"]);
    assert_eq!(ahead.trim(), "0", "nada foi commitado");
    assert!(!repo.path().join(".kax/wt/T-5/evil.txt").exists());
}

/// O stderr da tentativa que falhou vai anexado ao pacote da seguinte.
#[tokio::test]
async fn failed_attempt_log_is_appended_to_next_packet() {
    let (groq, gemini) = (MockServer::start().await, MockServer::start().await);
    mount(&groq, &diff_to("BAD"), 1).await;
    mount(&gemini, &diff_to("GOOD"), 1).await;
    let repo = temp_repo();
    let app = app(&groq, &gemini, tempfile::tempdir().unwrap().path());

    let r = run_task(&app, repo.path(), "main", &packet("T-4", 2)).await;
    assert_eq!(r.status, TaskStatus::Accepted, "{r:?}");

    let first = &groq.received_requests().await.unwrap()[0];
    let second = &gemini.received_requests().await.unwrap()[0];
    let first = String::from_utf8_lossy(&first.body).into_owned();
    let second = String::from_utf8_lossy(&second.body).into_owned();
    assert!(!first.contains("Tentativa anterior falhou"));
    assert!(second.contains("Tentativa anterior falhou"), "{second}");
    assert!(second.contains("git grep"), "{second}");
}
