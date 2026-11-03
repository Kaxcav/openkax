//! F2-6: `delegate_batch` + `status` + `report`. Lote de 8 tarefas contra
//! `wiremock` (4 aceitas, 4 que o verify recusa), `parallelism = 4`.

mod common;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use common::*;
use openkax::config::Config;
use openkax::ledger;
use openkax::mcp::{BatchState, Batches, TaskPacket, TaskState, TaskStatus};
use openkax::provider::Secrets;
use openkax::server::{self, AppState, Deps};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer};

fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .status()
        .unwrap();
    assert!(st.success(), "git {args:?}");
}

fn temp_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("value.txt"), "OLD\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

fn packet(id: &str, needle: &str) -> TaskPacket {
    serde_json::from_value(json!({
        "id": id,
        "class": "kax/coder-fast",
        "objective": "Trocar OLD por GOOD",
        "files": [{ "path": "value.txt", "mode": "write" }],
        "accept": { "verify": [format!("git grep -q {needle} -- value.txt")], "expect": "exit 0" },
        "max_attempts": 1
    }))
    .unwrap()
}

async fn app(groq: &MockServer, gemini: &MockServer, ledger_dir: &Path) -> AppState {
    server::install_crypto_provider();
    let config = Config::from_toml_str(&two_provider_config(&groq.uri(), &gemini.uri())).unwrap();
    let mut deps = Deps::new(config, Secrets::from_pairs(SECRETS.iter().copied()));
    deps.ledger = ledger::spawn(ledger_dir.to_owned(), 64);
    AppState::new(deps).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_of_eight_never_exceeds_parallelism_and_reports_all() {
    let diff = "```diff\n--- a/value.txt\n+++ b/value.txt\n@@ -1 +1 @@\n-OLD\n+GOOD\n```";
    let (groq, gemini) = (MockServer::start().await, MockServer::start().await);
    for s in [&groq, &gemini] {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(sse_ok(diff).set_delay(Duration::from_millis(120)))
            .mount(s)
            .await;
    }
    let repo = temp_repo();
    let ledger_dir = tempfile::tempdir().unwrap();
    let app = app(&groq, &gemini, ledger_dir.path()).await;

    // Pares aceitam (`GOOD` existe após o diff); ímpares exigem `NOPE` e são recusadas.
    let tasks: Vec<_> = (0..8)
        .map(|i| packet(&format!("T-{i}"), if i % 2 == 0 { "GOOD" } else { "NOPE" }))
        .collect();
    let batches = Batches::default();
    let first = batches.start(&app, repo.path(), "main", tasks, 4);

    // Volta na hora com tudo em fila.
    assert!(first.batch_id.starts_with("B-"), "{first:?}");
    assert_eq!(first.state, BatchState::Running);
    assert_eq!(first.total, 8);
    assert!(first.tasks.iter().all(|t| t.state == TaskState::Queued));

    let mut last = first;
    for _ in 0..600 {
        last = batches.status(&last.batch_id).expect("lote conhecido");
        let running = last
            .tasks
            .iter()
            .filter(|t| t.state == TaskState::Running)
            .count();
        assert!(running <= 4, "{running} simultâneas: {last:?}");
        if last.state == BatchState::Done {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert_eq!(last.state, BatchState::Done, "{last:?}");
    assert_eq!(last.finished, 8);
    assert_eq!(last.results.len(), 8);
    assert!(last.peak_running <= 4, "pico {}", last.peak_running);
    assert!(
        last.peak_running >= 2,
        "não paralelizou: {}",
        last.peak_running
    );
    let accepted = last
        .tasks
        .iter()
        .filter(|t| t.state == TaskState::Accepted)
        .count();
    let refused = last
        .tasks
        .iter()
        .filter(|t| t.state == TaskState::NeedsLead)
        .count();
    assert_eq!((accepted, refused), (4, 4), "{last:?}");
    for t in &last.tasks {
        let r = t.result.as_ref().expect("resultado pronto");
        let even = t.id.trim_start_matches("T-").parse::<u32>().unwrap() % 2 == 0;
        assert_eq!(r.status == TaskStatus::Accepted, even, "{r:?}");
    }
    assert!(batches.status("B-nao-existe").is_none());
}
