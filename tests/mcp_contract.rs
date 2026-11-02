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
fn delegate_batch_validates_then_reports_not_implemented() {
    let mut mcp = Mcp::spawn();

    let bad = mcp.call(
        "delegate_batch",
        json!({ "repo": "/r", "base_ref": "main", "tasks": [packet("T-1"), packet("T-1")] }),
    );
    assert_eq!(bad["result"]["isError"], json!(true), "{bad}");
    assert_eq!(text_of(&bad)["error"]["code"], "invalid_input");

    let ok = mcp.call(
        "delegate_batch",
        json!({ "repo": "/r", "base_ref": "main", "tasks": [packet("T-1")] }),
    );
    assert_eq!(ok["result"]["isError"], json!(true), "{ok}");
    let err = text_of(&ok);
    assert_eq!(err["error"]["code"], "not_implemented");
    assert!(
        err["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("não implementado na etapa F2-1"))
    );
}

// --- Aceite das próximas etapas (falham de propósito até serem ligados) ---

/// `delegate` de ponta a ponta pelo MCP: operários em `wiremock` (groq erra, gemini
/// acerta), repo git temporário, resultado `accepted` na 2ª tentativa.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delegate_returns_accepted_result() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    let diff = |v: &str| {
        format!("```diff
--- a/value.txt
+++ b/value.txt
@@ -1 +1 @@
-OLD
+{v}
```")
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
    std::fs::write(repo.path().join("value.txt"), "OLD
").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);

    let cfg_dir = tempfile::tempdir().unwrap();
    let cfg = cfg_dir.path().join("providers.toml");
    std::fs::write(&cfg, common::two_provider_config(&groq.uri(), &gemini.uri())).unwrap();
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
#[ignore = "F2-4: status(batch_id) devolve o lote despachado"]
fn status_returns_known_batch() {
    let mut mcp = Mcp::spawn();
    let resp = mcp.call("status", json!({ "batch_id": "B-1" }));
    assert_ne!(resp["result"]["isError"], json!(true), "{resp}");
    assert_eq!(text_of(&resp)["batch_id"], "B-1");
}

#[test]
#[ignore = "F2-6: report(period) agrega o ledger"]
fn report_returns_acceptance_by_class() {
    let mut mcp = Mcp::spawn();
    let resp = mcp.call("report", json!({ "period": "week" }));
    assert_ne!(resp["result"]["isError"], json!(true), "{resp}");
    assert!(text_of(&resp)["by_class"].is_object());
}
