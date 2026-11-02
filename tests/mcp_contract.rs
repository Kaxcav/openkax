//! Contrato do MCP `kax-workers` (F2-1): sobe `openkax mcp` por stdio e confere
//! `initialize` + `tools/list`. Os testes `#[ignore = "F2-x"]` são os de aceite
//! das próximas etapas e falham de propósito até a etapa indicada.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Mcp {
    fn spawn() -> Self {
        Self::spawn_with(concat!(env!("CARGO_MANIFEST_DIR"), "/providers.toml"), &[])
    }

    fn spawn_with(config: &str, envs: &[(&str, &str)]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_openkax"))
            .arg("mcp")
            .env("OPENKAX_CONFIG", config)
            .envs(envs.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("sobe openkax mcp");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut mcp = Mcp {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        let init = mcp.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "mcp-contract-test", "version": "0.0.0" }
            }),
        );
        assert_eq!(
            init["result"]["serverInfo"]["name"], "kax-workers",
            "{init}"
        );
        mcp.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        mcp
    }

    fn send(&mut self, v: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin aberto");
        writeln!(stdin, "{v}").expect("escreve");
        stdin.flush().expect("flush");
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).expect("lê stdout");
            assert!(
                n > 0,
                "servidor fechou o stdout antes de responder {method}"
            );
            let v: Value = serde_json::from_str(line.trim()).expect("linha JSON-RPC");
            if v["id"] == json!(id) {
                return v;
            }
        }
    }

    fn call(&mut self, name: &str, arguments: Value) -> Value {
        self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

fn tool<'a>(tools: &'a [Value], name: &str) -> &'a Value {
    tools
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("tool {name} ausente"))
}

fn required(schema: &Value) -> Vec<&str> {
    let mut r: Vec<&str> = schema["required"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    r.sort_unstable();
    r
}

fn packet(id: &str) -> Value {
    json!({
        "id": id,
        "class": "kax/coder-smart",
        "objective": "Implementar PaymentAdapter contra o trait",
        "files": [
            { "path": "src/payments/mod.rs", "mode": "read" },
            { "path": "src/payments/stripe.rs", "mode": "write", "exists": false }
        ],
        "contracts": ["trait PaymentAdapter { }"],
        "constraints": ["não adicionar dependências"],
        "accept": { "verify": ["cargo check"], "expect": "exit 0" },
        "output": "unified_diff",
        "max_attempts": 2
    })
}

fn text_of(resp: &Value) -> Value {
    let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
    serde_json::from_str(text).unwrap_or(Value::Null)
}

#[test]
fn tools_list_has_the_five_tools_with_required_fields() {
    let mut mcp = Mcp::spawn();
    let list = mcp.request("tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().expect("tools[]").clone();

    let mut names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["delegate", "delegate_batch", "quota", "report", "status"]
    );

    let batch = &tool(&tools, "delegate_batch")["inputSchema"];
    assert_eq!(required(batch), ["base_ref", "repo", "tasks"]);
    for f in ["repo", "base_ref", "tasks", "parallelism"] {
        assert!(batch["properties"][f].is_object(), "delegate_batch.{f}");
    }

    let one = &tool(&tools, "delegate")["inputSchema"];
    assert_eq!(required(one), ["base_ref", "repo", "task"]);

    assert_eq!(
        required(&tool(&tools, "status")["inputSchema"]),
        ["batch_id"]
    );
    assert_eq!(required(&tool(&tools, "report")["inputSchema"]), ["period"]);
    assert!(required(&tool(&tools, "quota")["inputSchema"]).is_empty());

    // Campos do TaskPacket (PLANO §3.6) presentes em $defs.
    let task = &batch["$defs"]["TaskPacket"];
    assert_eq!(
        required(task),
        ["accept", "class", "files", "id", "objective"]
    );
    for f in [
        "id",
        "class",
        "objective",
        "files",
        "contracts",
        "constraints",
        "accept",
        "output",
        "max_attempts",
    ] {
        assert!(task["properties"][f].is_object(), "TaskPacket.{f}");
    }
}

#[test]
fn quota_returns_real_scheduler_snapshot() {
    let mut mcp = Mcp::spawn();
    let resp = mcp.call("quota", json!({}));
    assert_ne!(resp["result"]["isError"], json!(true), "{resp}");
    let snap = text_of(&resp);
    assert!(snap.is_object(), "{resp}");
}

#[test]
fn delegate_batch_validates_input() {
    let mut mcp = Mcp::spawn();
    let bad = mcp.call(
        "delegate_batch",
        json!({ "repo": "/r", "base_ref": "main", "tasks": [packet("T-1"), packet("T-1")] }),
    );
    assert_eq!(bad["result"]["isError"], json!(true), "{bad}");
    assert_eq!(text_of(&bad)["error"]["code"], "invalid_input");

    let too_wide = mcp.call(
        "delegate_batch",
        json!({ "repo": "/r", "base_ref": "main", "tasks": [packet("T-1")], "parallelism": 17 }),
    );
    assert_eq!(text_of(&too_wide)["error"]["code"], "invalid_input");
}

/// Devolve na hora com tudo `queued`; o `status` acaba mostrando o resultado
/// (repo inexistente → `needs_lead`, sem tocar em provider).
#[test]
fn delegate_batch_returns_queued_then_status_shows_results() {
    let mut mcp = Mcp::spawn();
    let ok = mcp.call(
        "delegate_batch",
        json!({ "repo": "/nao/existe/repo", "base_ref": "main", "tasks": [packet("T-1"), packet("T-2")] }),
    );
    assert_ne!(ok["result"]["isError"], json!(true), "{ok}");
    let first = text_of(&ok);
    let id = first["batch_id"].as_str().expect("batch_id").to_owned();
    assert_eq!(first["total"], 2);
    assert!(
        first["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["state"] == "queued")
    );

    let mut last = Value::Null;
    for _ in 0..200 {
        last = text_of(&mcp.call("status", json!({ "batch_id": id })));
        if last["state"] == "done" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(last["state"], "done", "{last}");
    assert_eq!(last["results"].as_array().unwrap().len(), 2);
    assert!(
        last["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["state"] == "needs_lead")
    );
}

#[test]
fn status_of_unknown_batch_is_not_found() {
    let mut mcp = Mcp::spawn();
    let resp = mcp.call("status", json!({ "batch_id": "B-nada" }));
    assert_eq!(resp["result"]["isError"], json!(true), "{resp}");
    assert_eq!(text_of(&resp)["error"]["code"], "not_found");
}

// --- Aceite das próximas etapas (falham de propósito até serem ligados) ---

/// `delegate` de ponta a ponta pelo MCP: operários em `wiremock` (groq erra, gemini
/// acerta), repo git temporário, resultado `accepted` na 2ª tentativa.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delegate_returns_accepted_result() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    let diff = |v: &str| {
        format!(
            "```diff
--- a/value.txt
+++ b/value.txt
@@ -1 +1 @@
-OLD
+{v}
```"
        )
    };
    let (groq, gemini) = (MockServer::start().await, MockServer::start().await);
    for (server, text) in [(&groq, diff("BAD")), (&gemini, diff("GOOD"))] {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(common::sse_ok(&text))
            .expect(1)
            .mount(server)
            .await;
    }

    let repo = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let st = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(
        repo.path().join("value.txt"),
        "OLD
",
    )
    .unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);

    let cfg_dir = tempfile::tempdir().unwrap();
    let cfg = cfg_dir.path().join("providers.toml");
    std::fs::write(
        &cfg,
        common::two_provider_config(&groq.uri(), &gemini.uri()),
    )
    .unwrap();
    let mut mcp = Mcp::spawn_with(
        cfg.to_str().unwrap(),
        &[("GROQ_API_KEY", "k-groq"), ("GEMINI_API_KEY", "k-gemini")],
    );

    let resp = mcp.call(
        "delegate",
        json!({
            "repo": repo.path().to_str().unwrap(),
            "base_ref": "main",
            "task": {
                "id": "T-1",
                "class": "kax/coder-fast",
                "objective": "Trocar OLD por GOOD",
                "files": [{ "path": "value.txt", "mode": "write" }],
                "accept": { "verify": ["git grep -q GOOD -- value.txt"], "expect": "exit 0" }
            }
        }),
    );
    assert_ne!(resp["result"]["isError"], json!(true), "{resp}");
    let out = text_of(&resp);
    assert_eq!(out["id"], "T-1", "{resp}");
    assert_eq!(out["status"], "accepted", "{resp}");
    assert_eq!(out["attempts"], 2);
    assert_eq!(out["provider"], "gemini");
    groq.verify().await;
    gemini.verify().await;
}

#[test]
fn report_returns_acceptance_by_class() {
    let mut mcp = Mcp::spawn();
    let resp = mcp.call("report", json!({ "period": "week" }));
    assert_ne!(resp["result"]["isError"], json!(true), "{resp}");
    let out = text_of(&resp);
    assert!(out["tasks_by_class"].is_object(), "{out}");
    assert!(out["requests_by_class"].is_object(), "{out}");
}

/// Memória privada (bytes) do processo no Windows; `None` fora dele.
fn private_bytes(pid: u32) -> Option<u64> {
    if !cfg!(windows) {
        return None;
    }
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command"])
        .arg(format!("(Get-Process -Id {pid}).PrivateMemorySize64"))
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Medição da F2-6 (não é critério): lote de 8 tarefas, `parallelism = 4`, mocks.
/// Rode com `cargo test --release --test mcp_contract -- --ignored --nocapture batch_memory`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "medição manual de memória (F2-6); usa o binário do perfil de teste"]
async fn batch_memory_probe() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    let diff = "```diff\n--- a/value.txt\n+++ b/value.txt\n@@ -1 +1 @@\n-OLD\n+GOOD\n```";
    let (groq, gemini) = (MockServer::start().await, MockServer::start().await);
    for s in [&groq, &gemini] {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(common::sse_ok(diff).set_delay(std::time::Duration::from_millis(150)))
            .mount(s)
            .await;
    }
    let repo = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let st = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(repo.path().join("value.txt"), "OLD\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
    let cfg_dir = tempfile::tempdir().unwrap();
    let cfg = cfg_dir.path().join("providers.toml");
    std::fs::write(
        &cfg,
        common::two_provider_config(&groq.uri(), &gemini.uri()),
    )
    .unwrap();

    let mut mcp = Mcp::spawn_with(
        cfg.to_str().unwrap(),
        &[("GROQ_API_KEY", "k-groq"), ("GEMINI_API_KEY", "k-gemini")],
    );
    let pid = mcp.child.id();
    let mib = |b: Option<u64>| {
        b.map_or("n/d".to_owned(), |b| {
            format!("{:.2} MiB", b as f64 / 1_048_576.0)
        })
    };
    let before = private_bytes(pid);

    let tasks: Vec<Value> = (0..8)
        .map(|i| {
            let mut t = packet(&format!("M-{i}"));
            t["class"] = json!("kax/coder-fast");
            t["files"] = json!([{ "path": "value.txt", "mode": "write" }]);
            let needle = if i % 2 == 0 { "GOOD" } else { "NOPE" };
            t["accept"] = json!({ "verify": [format!("git grep -q {needle} -- value.txt")], "expect": "exit 0" });
            t["max_attempts"] = json!(1);
            t
        })
        .collect();
    let first = text_of(&mcp.call(
        "delegate_batch",
        json!({ "repo": repo.path().to_str().unwrap(), "base_ref": "main", "tasks": tasks, "parallelism": 4 }),
    ));
    let id = first["batch_id"].as_str().unwrap().to_owned();
    let mut during = 0u64;
    for _ in 0..400 {
        during = during.max(private_bytes(pid).unwrap_or(0));
        let s = text_of(&mcp.call("status", json!({ "batch_id": id })));
        if s["state"] == "done" {
            println!("pico de simultâneas: {}", s["peak_running"]);
            break;
        }
    }
    std::thread::sleep(std::time::Duration::from_secs(12));
    let after = private_bytes(pid);
    println!(
        "memória privada: antes {} | durante (pico) {} | depois (12 s) {}",
        mib(before),
        mib((during > 0).then_some(during)),
        mib(after)
    );
}
