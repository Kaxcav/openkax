//! F0-24: compatibilidade com os clientes reais, sem os clientes.
//!
//! Substitui o aceite manual Continue/Cline como critério bloqueante da Fase 0
//! (ADR 0004). Cada teste reproduz o formato de request do cliente e confere o
//! que ele consome: classe, SSE bem formado até `[DONE]`, `tool_calls`
//! reconstruídos por índice, 413, fallback no meio da sessão, `/quota` e ledger.
//!
//! Procedência dos formatos (conferida em 2026-11-02; o turno na nuvem não
//! conseguiu abrir `openai.ts` dos dois repositórios):
//! - Continue, `stream_options.include_usage` em todo streaming: PR
//!   continuedev/continue#12992.
//! - Continue, chunk com `usage` tratado como final (perde texto e tool_calls):
//!   issue continuedev/continue#13417, relatado com o Gemini.
//! - Continue, mensagem vazia vira `" "`: `core/llm/countTokens.ts`.
//! - Cline, `max_tokens` no provider OpenAI Compatible: issue cline/cline#8912.
//! - Cline, assistente com `tool_calls` sem `content` e mensagem `tool`:
//!   `docs/api/chat-completions.mdx`.
//! - **Sem fonte lida, de memória do código dos clientes:** `parallel_tool_calls`,
//!   `temperature: 0`, partes `[{type:"text"}]` e `prediction`. Os testes só exigem
//!   que esses campos cheguem intactos ao upstream. Formato real que não esteja
//!   modelado aqui segue sem teste: o roteiro de `docs/smoke/fase-0-clientes.md`
//!   confere isso com os clientes de verdade.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;

use common::*;
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer};

const KEYS: &[(&str, &str)] = &[("KA", "ka"), ("KB", "kb")];

/// Duas classes reais (limites de pacote do `providers.toml`) sobre `a` (free) →
/// `b` (paid).
fn config(a: &str, b: &str) -> String {
    format!(
        r#"
        [defaults]
        request_timeout_ms = 5000
        first_token_timeout_ms = 2000
        estimate_chars_per_token = 3.6

        [[provider]]
        id = "a"
        base_url = "{a}"
        auth = {{ kind = "bearer", env = "KA" }}
        models = [{{ id = "ma", upstream = "up-a", max_ctx = 200000, limits = {{ rpm = 100, tpm = 500000, rpd = 100 }} }}]
        cooldown = {{ on_429_s = 60 }}

        [[provider]]
        id = "b"
        base_url = "{b}"
        tier = "paid"
        auth = {{ kind = "bearer", env = "KB" }}
        models = [{{ id = "mb", upstream = "up-b", max_ctx = 200000, limits = {{ rpm = 100, tpm = 500000, rpd = 100, tpd = 1000000 }} }}]

        [[class]]
        id = "kax/coder-fast"
        max_packet_tokens = 6000
        route = [{{ provider = "a", model = "ma" }}, {{ provider = "b", model = "mb" }}]

        [[class]]
        id = "kax/coder-smart"
        max_packet_tokens = 60000
        route = [{{ provider = "a", model = "ma" }}, {{ provider = "b", model = "mb" }}]

        [[class]]
        id = "kax/reviewer"
        max_packet_tokens = 60000
        route = [{{ provider = "b", model = "mb" }}]
        "#
    )
}

fn upstream(key: &str, model: &str) -> wiremock::MockBuilder {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", format!("Bearer {key}").as_str()))
        .and(body_partial_json(json!({ "model": model, "stream": true })))
}

fn ledger_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(
        format!(
            "openkax-compat-{name}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        )
        .replace([':', ' ', '{', '}', ','], "_"),
    );
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Chunk com `usage` junto de `choices` (o Gemini faz isso em todo chunk).
fn chunk_with_usage(delta: Value, finish: Option<&str>, prompt: u32, completion: u32) -> String {
    let c = json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "upstream-model",
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        "usage": { "prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": prompt + completion },
    });
    format!("data: {c}\n\n")
}

/// Chunk final no formato OpenAI: `choices` vazio + `usage`.
fn usage_only(prompt: u32, completion: u32) -> String {
    let c = json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "upstream-model",
        "choices": [],
        "usage": { "prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": prompt + completion },
    });
    format!("data: {c}\n\n")
}

const DONE: &str = "data: [DONE]\n\n";

fn tool_delta(index: u32, id: Option<&str>, name: Option<&str>, args: &str) -> Value {
    let mut f = json!({ "arguments": args });
    if let Some(n) = name {
        f["name"] = n.into();
    }
    let mut t = json!({ "index": index, "function": f });
    if let Some(i) = id {
        t["id"] = i.into();
        t["type"] = "function".into();
    }
    json!({ "tool_calls": [t] })
}

/// Reconstrói os `tool_calls` como o cliente faz: por `index`, juntando pedaços.
fn rebuild_tool_calls(sse: &Sse) -> BTreeMap<u64, (String, String, String)> {
    let mut out: BTreeMap<u64, (String, String, String)> = BTreeMap::new();
    for c in &sse.chunks {
        for t in c["choices"][0]["delta"]["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let e = out.entry(t["index"].as_u64().expect("index")).or_default();
            if let Some(id) = t["id"].as_str() {
                e.0.push_str(id);
            }
            if let Some(n) = t["function"]["name"].as_str() {
                e.1.push_str(n);
            }
            if let Some(a) = t["function"]["arguments"].as_str() {
                e.2.push_str(a);
            }
        }
    }
    out
}

/// SSE terminado em `[DONE]`, sem nada depois, com `id` estável.
fn assert_well_formed(sse: &Sse) {
    assert!(sse.done, "sem [DONE]: {sse:?}");
    assert!(!sse.trailing_after_done, "algo depois de [DONE]: {sse:?}");
    for c in &sse.chunks {
        assert_eq!(c["object"], "chat.completion.chunk", "{c}");
        assert_eq!(c["id"], "chatcmpl-test", "{c}");
    }
}

/// `usage` só pode aparecer num chunk de `choices` vazio, o último antes de
/// `[DONE]` (contrato OpenAI; o Continue perde o resto se vier antes).
fn assert_usage_only_at_the_end(sse: &Sse) {
    let n = sse.chunks.len();
    for (i, c) in sse.chunks.iter().enumerate() {
        if c.get("usage").is_some() {
            assert_eq!(i, n - 1, "usage antes do fim: {c}");
            assert_eq!(c["choices"], json!([]), "usage com choices: {c}");
        }
    }
}

async fn post_sse(gw: &Gateway, body: &Value) -> Reply {
    let resp = gw
        .http
        .post(format!("{}/v1/chat/completions", gw.base))
        .json(body)
        .send()
        .await
        .expect("gateway não respondeu");
    let status = resp.status().as_u16();
    let h = |n: &str| {
        resp.headers()
            .get(n)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let (provider, retry_after, content_type) =
        (h("x-openkax-provider"), h("retry-after"), h("content-type"));
    let text = resp.text().await.expect("corpo");
    Reply {
        status,
        provider,
        retry_after,
        content_type,
        sse: Sse::parse(&text),
    }
}

/// Corpo recebido pelo upstream na `i`-ésima request.
async fn received_body(server: &MockServer, i: usize) -> Value {
    let reqs = server.received_requests().await.expect("recording ligado");
    reqs[i].body_json().expect("corpo JSON")
}

// ----------------------------------------------------------------- Continue

/// Chat do Continue: `system` + histórico, `stream_options` sempre, `stop`,
/// `max_tokens`, `temperature`. Tudo chega ao upstream e o cliente recebe o
/// `usage` que pediu, no fim.
#[tokio::test]
async fn continue_chat_stream_with_history_and_usage() {
    let (a, b) = (MockServer::start().await, MockServer::start().await);
    let body = [
        chunk(json!({ "role": "assistant" }), None),
        chunk(json!({ "content": "Olá" }), None),
        chunk(json!({ "content": ", mundo" }), None),
        chunk(json!({}), Some("stop")),
        usage_only(30, 4),
        DONE.to_owned(),
    ]
    .concat();
    upstream("ka", "up-a")
        .respond_with(sse_raw(body))
        .expect(1)
        .mount(&a)
        .await;
    let dir = ledger_dir("chat");
    let gw = spawn_with_ledger(&config(&a.uri(), &b.uri()), KEYS, dir.clone()).await;

    let messages = json!([
        { "role": "system", "content": "Você é um assistente de código." },
        { "role": "user", "content": "explique este arquivo" },
        { "role": "assistant", "content": " " },
        { "role": "user", "content": "e agora?" },
        { "role": "assistant", "content": "É um parser." },
        { "role": "user", "content": "e os testes?" },
    ]);
    let r = post_sse(
        &gw,
        &json!({
            "model": "kax/coder-fast",
            "messages": messages,
            "max_tokens": 4096,
            "temperature": 0.7,
            "stop": ["<|im_end|>"],
            "stream": true,
            "stream_options": { "include_usage": true },
        }),
    )
    .await;

    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.provider.as_deref(), Some("a"));
    assert!(
        r.content_type
            .as_deref()
            .unwrap_or("")
            .starts_with("text/event-stream")
    );
    assert_eq!(r.sse.content(), "Olá, mundo");
    assert_eq!(r.sse.finish_reasons(), ["stop"]);
    assert_well_formed(&r.sse);
    assert_usage_only_at_the_end(&r.sse);
    let last = r.sse.chunks.last().unwrap();
    assert_eq!(last["usage"]["prompt_tokens"], 30, "{last}");
    assert_eq!(last["usage"]["completion_tokens"], 4, "{last}");

    let sent = received_body(&a, 0).await;
    assert_eq!(sent["messages"], messages);
    assert_eq!(sent["model"], "up-a");
    assert_eq!(sent["stop"], json!(["<|im_end|>"]));
    assert_eq!(sent["max_tokens"], 4096);
    assert_eq!(sent["temperature"], 0.7);
    assert_eq!(sent["stream_options"]["include_usage"], true);

    let q = gw.quota().await;
    assert_eq!(q["served_by_tier"]["free"], 1, "{q}");
    assert_eq!(q["providers"][0]["models"][0]["in_flight"], 0, "{q}");
    let lines = ledger_lines(&dir, 1).await;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["route"], "kax/coder-fast");
    assert_eq!(lines[0]["provider"], "a");
    assert_eq!(lines[0]["outcome"], "ok");
    assert_eq!((&lines[0]["in"], &lines[0]["out"]), (&json!(30), &json!(4)));
    a.verify().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Cada classe usa a própria rota: `kax/reviewer` só tem `b`.
#[tokio::test]
async fn each_class_uses_its_own_route() {
    let (a, b) = (MockServer::start().await, MockServer::start().await);
    upstream("ka", "up-a")
        .respond_with(sse_ok("de-a"))
        .expect(1)
        .mount(&a)
        .await;
    upstream("kb", "up-b")
        .respond_with(sse_ok("de-b"))
        .expect(1)
        .mount(&b)
        .await;
    let gw = Gateway::spawn(&config(&a.uri(), &b.uri()), KEYS).await;

    for (class, provider, text) in [
        ("kax/coder-smart", "a", "de-a"),
        ("kax/reviewer", "b", "de-b"),
    ] {
        let r = gw.chat_stream(class, "oi").await;
        assert_eq!(r.provider.as_deref(), Some(provider), "{class}: {r:?}");
        assert_eq!(r.sse.content(), text, "{class}");
    }
    a.verify().await;
    b.verify().await;
}

/// Upstream que erra depois do `finish_reason`: a resposta já estava completa.
/// Um final só, sem cooldown, ledger `ok`.
#[tokio::test]
async fn error_after_finish_reason_keeps_the_response_complete() {
    let (a, b) = (MockServer::start().await, MockServer::start().await);
    let body = [
        chunk(json!({ "content": "pronto" }), None),
        chunk(json!({}), Some("stop")),
        "data: {\"error\":{\"message\":\"boom\"}}\n\n".to_owned(),
    ]
    .concat();
    upstream("ka", "up-a")
        .respond_with(sse_raw(body))
        .expect(1)
        .mount(&a)
        .await;
    let dir = ledger_dir("after-finish");
    let gw = spawn_with_ledger(&config(&a.uri(), &b.uri()), KEYS, dir.clone()).await;

    let r = gw.chat_stream("kax/coder-fast", "oi").await;
    assert_eq!(r.sse.content(), "pronto");
    assert_eq!(r.sse.finish_reasons(), ["stop"], "{:?}", r.sse);
    assert_well_formed(&r.sse);

    let q = gw.quota().await;
    assert_eq!(
        q["providers"][0]["models"][0]["state"]["state"], "healthy",
        "{q}"
    );
    let lines = ledger_lines(&dir, 1).await;
    assert_eq!(lines[0]["outcome"], "ok", "{lines:?}");
    a.verify().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Upstream que repete `index` em chamadas paralelas: cada chamada segue inteira,
/// no stream e na agregação.
#[tokio::test]
async fn parallel_calls_with_repeated_index_stay_separate() {
    let (a, b) = (MockServer::start().await, MockServer::start().await);
    let body = [
        chunk(
            tool_delta(0, Some("c1"), Some("read_file"), "{\"path\":\"a\"}"),
            None,
        ),
        chunk(
            tool_delta(0, Some("c2"), Some("search_files"), "{\"regex\":"),
            None,
        ),
        chunk(tool_delta(0, None, None, "\"f\"}"), None),
        chunk(json!({}), Some("tool_calls")),
        DONE.to_owned(),
    ]
    .concat();
    upstream("ka", "up-a")
        .respond_with(sse_raw(body))
        .expect(2)
        .mount(&a)
        .await;
    let gw = Gateway::spawn(&config(&a.uri(), &b.uri()), KEYS).await;

    let mut req = json!({
        "model": "kax/coder-fast",
        "stream": true,
        "messages": [{ "role": "user", "content": "oi" }],
    });
    let r = post_sse(&gw, &req).await;
    let calls = rebuild_tool_calls(&r.sse);
    assert_eq!(
        calls[&0],
        ("c1".into(), "read_file".into(), "{\"path\":\"a\"}".into())
    );
    assert_eq!(
        calls[&1],
        (
            "c2".into(),
            "search_files".into(),
            "{\"regex\":\"f\"}".into()
        )
    );

    req["stream"] = false.into();
    let v: Value = gw
        .http
        .post(format!("{}/v1/chat/completions", gw.base))
        .json(&req)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tc = &v["choices"][0]["message"]["tool_calls"];
    assert_eq!(tc[0]["id"], "c1", "{v}");
    assert_eq!(tc[0]["function"]["name"], "read_file", "{v}");
    assert_eq!(tc[1]["id"], "c2", "{v}");
    assert_eq!(tc[1]["function"]["arguments"], "{\"regex\":\"f\"}", "{v}");
    a.verify().await;
}

/// continuedev/continue#13417: o Gemini manda `usage` em todo chunk quando há
/// `stream_options`, e o Continue trata chunk com `usage` como final, perdendo
/// texto e tool_calls. O gateway normaliza: `usage` só no último chunk.
#[tokio::test]
async fn usage_on_every_upstream_chunk_reaches_client_only_at_the_end() {
    let (a, b) = (MockServer::start().await, MockServer::start().await);
    let body = [
        chunk_with_usage(
            json!({ "role": "assistant", "content": "um " }),
            None,
            10,
            1,
        ),
        chunk_with_usage(json!({ "content": "dois" }), None, 10, 2),
        chunk_with_usage(
            tool_delta(0, Some("c1"), Some("read_file"), "{\"path\":\"a\"}"),
            None,
            10,
            3,
        ),
        chunk_with_usage(json!({}), Some("tool_calls"), 10, 5),
        DONE.to_owned(),
    ]
    .concat();
    upstream("ka", "up-a")
        .respond_with(sse_raw(body))
        .expect(2)
        .mount(&a)
        .await;
    let gw = Gateway::spawn(&config(&a.uri(), &b.uri()), KEYS).await;

    let req = |usage: bool| {
        let mut v = json!({
            "model": "kax/coder-fast",
            "stream": true,
            "messages": [{ "role": "user", "content": "oi" }],
        });
        if usage {
            v["stream_options"] = json!({ "include_usage": true });
        }
        v
    };

    let r = post_sse(&gw, &req(true)).await;
    assert_eq!(r.sse.content(), "um dois", "{r:?}");
    assert_eq!(rebuild_tool_calls(&r.sse)[&0].1, "read_file");
    assert_eq!(r.sse.finish_reasons(), ["tool_calls"]);
    assert_well_formed(&r.sse);
    assert_usage_only_at_the_end(&r.sse);
    let last = r.sse.chunks.last().unwrap();
    assert_eq!(
        last["usage"]["completion_tokens"], 5,
        "vale o último: {last}"
    );

    // Sem pedir usage, não recebe usage nenhum.
    let r = post_sse(&gw, &req(false)).await;
    assert_eq!(r.sse.content(), "um dois", "{r:?}");
    assert_well_formed(&r.sse);
    for c in &r.sse.chunks {
        assert!(c.get("usage").is_none(), "usage não pedido: {c}");
    }
    a.verify().await;
}

/// Stream morre depois de `usage` parcial: o cliente que pediu ainda recebe o
/// último valor, depois do `finish_reason: "error"`.
#[tokio::test]
async fn usage_is_still_delivered_after_mid_stream_death() {
    let (a, b) = (MockServer::start().await, MockServer::start().await);
    let body = chunk_with_usage(json!({ "content": "par" }), None, 10, 1);
    upstream("ka", "up-a")
        .respond_with(sse_raw(body))
        .expect(1)
        .mount(&a)
        .await;
    let gw = Gateway::spawn(&config(&a.uri(), &b.uri()), KEYS).await;

    let r = post_sse(
        &gw,
        &json!({
            "model": "kax/coder-fast",
            "stream": true,
            "stream_options": { "include_usage": true },
            "messages": [{ "role": "user", "content": "oi" }],
        }),
    )
    .await;
    assert_eq!(r.sse.finish_reasons(), ["error"], "{:?}", r.sse);
    assert_usage_only_at_the_end(&r.sse);
    let last = r.sse.chunks.last().unwrap();
    assert_eq!(last["usage"]["completion_tokens"], 1, "{last}");
    a.verify().await;
}

/// Edit/apply do Continue: mensagem única, partes de texto, `prediction` em
/// modelos que suportam. O gateway não interpreta nada disso: chega intacto.
#[tokio::test]
async fn continue_edit_apply_reaches_upstream_intact() {
    let (a, b) = (MockServer::start().await, MockServer::start().await);
    upstream("ka", "up-a")
        .respond_with(sse_ok("fn novo() {}"))
        .expect(1)
        .mount(&a)
        .await;
    let gw = Gateway::spawn(&config(&a.uri(), &b.uri()), KEYS).await;

    let messages = json!([{
        "role": "user",
        "content": [
            { "type": "text", "text": "Original: fn velho() {}\nNew: fn novo() {}\nGere o código final." },
        ],
    }]);
    let prediction = json!({ "type": "content", "content": "fn velho() {}" });
    let r = post_sse(
        &gw,
        &json!({
            "model": "kax/coder-smart",
            "messages": messages,
            "max_tokens": 1024,
            "stream": true,
            "stream_options": { "include_usage": true },
            "prediction": prediction,
        }),
    )
    .await;

    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.sse.content(), "fn novo() {}");
    assert_well_formed(&r.sse);
    let sent = received_body(&a, 0).await;
    assert_eq!(sent["messages"], messages);
    assert_eq!(sent["prediction"], prediction);
    a.verify().await;
}

// -------------------------------------------------------------------- Cline

/// Tools no formato do Cline (OpenAI Compatible).
fn cline_tools() -> Value {
    let tool = |name: &str, desc: &str, props: Value, req: Value| {
        json!({ "type": "function", "function": { "name": name, "description": desc, "strict": false,
            "parameters": { "type": "object", "properties": props, "required": req, "additionalProperties": false } } })
    };
    json!([
        tool(
            "execute_command",
            "Run a CLI command on the system.",
            json!({ "command": { "type": "string" }, "requires_approval": { "type": "boolean" } }),
            json!(["command", "requires_approval"])
        ),
        tool(
            "read_file",
            "Read the contents of a file at the specified path.",
            json!({ "path": { "type": "string" } }),
            json!(["path"])
        ),
        tool(
            "write_to_file",
            "Write content to a file at the specified path.",
            json!({ "path": { "type": "string" }, "content": { "type": "string" } }),
            json!(["path", "content"])
        ),
        tool(
            "search_files",
            "Regex search across files in a directory.",
            json!({ "path": { "type": "string" }, "regex": { "type": "string" } }),
            json!(["path", "regex"])
        ),
        tool(
            "attempt_completion",
            "Present the result of the task to the user.",
            json!({ "result": { "type": "string" } }),
            json!(["result"])
        ),
    ])
}

/// System prompt do Cline: bem acima dos 6k tokens do `coder-fast`.
fn cline_system_prompt() -> String {
    "You are Cline, a highly skilled software engineer. TOOL USE: you have access to a set of tools that are executed upon the user's approval. RULES: your current working directory is fixed. ".repeat(160)
}

/// Rodada 1 da sessão do Cline: o que ele manda antes de qualquer tool.
fn cline_body(class: &str, extra: &[Value]) -> Value {
    let mut messages = vec![
        json!({ "role": "system", "content": cline_system_prompt() }),
        json!({ "role": "user", "content": [
            { "type": "text", "text": "<task>corrija o teste</task>" },
            { "type": "text", "text": "<environment_details># CWD: /repo</environment_details>" },
        ] }),
    ];
    messages.extend_from_slice(extra);
    json!({
        "model": class,
        "messages": messages,
        "temperature": 0,
        "max_tokens": 8192,
        "stream": true,
        "stream_options": { "include_usage": true },
        "tools": cline_tools(),
        "tool_choice": "auto",
        "parallel_tool_calls": false,
    })
}

/// O prompt do Cline não cabe no `kax/coder-fast`: 413 sem tocar upstream, e o
/// ledger registra a recusa sem corpo.
#[tokio::test]
async fn cline_prompt_is_413_on_coder_fast_without_upstream_request() {
    let (a, b) = (MockServer::start().await, MockServer::start().await);
    upstream("ka", "up-a")
        .respond_with(sse_ok("não"))
        .expect(0)
        .mount(&a)
        .await;
    upstream("kb", "up-b")
        .respond_with(sse_ok("não"))
        .expect(0)
        .mount(&b)
        .await;
    let dir = ledger_dir("413");
    let gw = spawn_with_ledger(&config(&a.uri(), &b.uri()), KEYS, dir.clone()).await;

    let resp = gw
        .http
        .post(format!("{}/v1/chat/completions", gw.base))
        .json(&cline_body("kax/coder-fast", &[]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);
    let v: Value = resp.json().await.unwrap();
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("max_packet_tokens"),
        "{v}"
    );

    let lines = ledger_lines(&dir, 1).await;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["route"], "kax/coder-fast");
    assert_eq!(lines[0]["outcome"], "rejected");
    let raw = lines[0].to_string();
    assert!(!raw.contains("You are Cline"), "corpo no ledger: {raw}");

    a.verify().await;
    b.verify().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// `tools` e `tool_calls` do histórico contam no tamanho do pacote, não só o texto.
#[tokio::test]
async fn tools_and_tool_call_history_count_towards_packet_limit() {
    let (a, b) = (MockServer::start().await, MockServer::start().await);
    upstream("ka", "up-a")
        .respond_with(sse_ok("não"))
        .expect(0)
        .mount(&a)
        .await;
    let gw = Gateway::spawn(&config(&a.uri(), &b.uri()), KEYS).await;
    let big = "x".repeat(30_000);
    let user = json!({ "role": "user", "content": "oi" });
    let bodies = [
        json!({ "model": "kax/coder-fast", "stream": true, "messages": [user],
            "tools": [{ "type": "function", "function": { "name": "f", "description": big } }] }),
        json!({ "model": "kax/coder-fast", "stream": true, "messages": [user,
            { "role": "assistant", "tool_calls": [{ "id": "c", "type": "function",
                "function": { "name": "f", "arguments": big } }] }] }),
    ];
    for body in &bodies {
        let r = post_sse(&gw, body).await;
        assert_eq!(r.status, 413, "{r:?}");
    }
    a.verify().await;
}

/// Sessão do Cline em três rodadas no `kax/coder-smart`:
/// 1. `a` responde 2 tool_calls com índices e `arguments` intercalados;
/// 2. `a` dá 429 → `b` atende (fallback pré-1º-token, cliente não percebe);
/// 3. `a` em cooldown → `b` direto, com o texto final.
/// Histórico com `tool_calls` + mensagens `tool` chega intacto, inclusive
/// `content: null` e assistente sem `content`; `/quota` e ledger batem.
#[tokio::test]
async fn cline_multi_round_tool_session_with_fallback() {
    let (a, b) = (MockServer::start().await, MockServer::start().await);
    let round1 = [
        chunk(json!({ "role": "assistant" }), None),
        chunk(tool_delta(0, Some("call_1"), Some("read_file"), ""), None),
        chunk(
            tool_delta(1, Some("call_2"), Some("search_files"), ""),
            None,
        ),
        chunk(tool_delta(0, None, None, "{\"path\":"), None),
        chunk(tool_delta(1, None, None, "{\"path\":\"src\","), None),
        chunk(tool_delta(0, None, None, "\"a.rs\"}"), None),
        chunk(tool_delta(1, None, None, "\"regex\":\"fn\"}"), None),
        chunk(json!({}), Some("tool_calls")),
        usage_only(9000, 40),
        DONE.to_owned(),
    ]
    .concat();
    upstream("ka", "up-a")
        .respond_with(sse_raw(round1))
        .up_to_n_times(1)
        .expect(1)
        .mount(&a)
        .await;
    upstream("ka", "up-a")
        .respond_with(rate_limited(60))
        .expect(1)
        .mount(&a)
        .await;
    let round2 = [
        chunk(json!({ "role": "assistant", "content": "" }), None),
        chunk(
            tool_delta(
                0,
                Some("call_3"),
                Some("write_to_file"),
                "{\"path\":\"a.rs\",",
            ),
            None,
        ),
        chunk(tool_delta(0, None, None, "\"content\":\"ok\"}"), None),
        chunk(json!({}), Some("tool_calls")),
        usage_only(9100, 20),
        DONE.to_owned(),
    ]
    .concat();
    upstream("kb", "up-b")
        .respond_with(sse_raw(round2))
        .up_to_n_times(1)
        .expect(1)
        .mount(&b)
        .await;
    let round3 = [
        chunk(json!({ "role": "assistant" }), None),
        chunk(json!({ "content": "pronto" }), None),
        chunk(json!({}), Some("stop")),
        usage_only(9200, 2),
        DONE.to_owned(),
    ]
    .concat();
    upstream("kb", "up-b")
        .respond_with(sse_raw(round3))
        .expect(1)
        .mount(&b)
        .await;
    let dir = ledger_dir("session");
    let gw = spawn_with_ledger(&config(&a.uri(), &b.uri()), KEYS, dir.clone()).await;

    // Rodada 1.
    let r1 = post_sse(&gw, &cline_body("kax/coder-smart", &[])).await;
    assert_eq!(r1.status, 200, "{r1:?}");
    assert_eq!(r1.provider.as_deref(), Some("a"));
    assert_well_formed(&r1.sse);
    assert_usage_only_at_the_end(&r1.sse);
    assert_eq!(r1.sse.finish_reasons(), ["tool_calls"]);
    let calls = rebuild_tool_calls(&r1.sse);
    assert_eq!(
        calls[&0],
        (
            "call_1".into(),
            "read_file".into(),
            "{\"path\":\"a.rs\"}".into()
        )
    );
    assert_eq!(
        calls[&1],
        (
            "call_2".into(),
            "search_files".into(),
            "{\"path\":\"src\",\"regex\":\"fn\"}".into()
        )
    );
    let sent = received_body(&a, 0).await;
    assert_eq!(sent["tools"], cline_tools());
    assert_eq!(sent["tool_choice"], "auto");
    assert_eq!(sent["parallel_tool_calls"], false);
    assert_eq!(sent["temperature"], 0);
    assert_eq!(sent["max_tokens"], 8192);

    // Rodada 2: histórico com tool_calls e resultados; `a` dá 429.
    let history = vec![
        json!({ "role": "assistant", "content": null, "tool_calls": [
            { "id": "call_1", "type": "function", "function": { "name": "read_file", "arguments": "{\"path\":\"a.rs\"}" } },
            { "id": "call_2", "type": "function", "function": { "name": "search_files", "arguments": "{\"path\":\"src\",\"regex\":\"fn\"}" } },
        ] }),
        json!({ "role": "tool", "tool_call_id": "call_1", "content": "fn main() {}" }),
        json!({ "role": "tool", "tool_call_id": "call_2", "content": [{ "type": "text", "text": "src/a.rs:1" }] }),
    ];
    let body2 = cline_body("kax/coder-smart", &history);
    let r2 = post_sse(&gw, &body2).await;
    assert_eq!(r2.status, 200, "{r2:?}");
    assert_eq!(
        r2.provider.as_deref(),
        Some("b"),
        "fallback no meio da sessão"
    );
    assert_well_formed(&r2.sse);
    assert_eq!(
        rebuild_tool_calls(&r2.sse)[&0],
        (
            "call_3".into(),
            "write_to_file".into(),
            "{\"path\":\"a.rs\",\"content\":\"ok\"}".into()
        )
    );
    let sent = received_body(&b, 0).await;
    assert_eq!(sent["model"], "up-b");
    assert_eq!(
        sent["messages"], body2["messages"],
        "histórico alterado a caminho do upstream"
    );

    // Rodada 3: assistente sem `content` (forma da doc do Cline); `a` em cooldown.
    let mut history3 = history.clone();
    history3.push(json!({ "role": "assistant", "tool_calls": [
        { "id": "call_3", "type": "function", "function": { "name": "write_to_file", "arguments": "{\"path\":\"a.rs\",\"content\":\"ok\"}" } },
    ] }));
    history3.push(json!({ "role": "tool", "tool_call_id": "call_3", "content": "ok" }));
    let body3 = cline_body("kax/coder-smart", &history3);
    let r3 = post_sse(&gw, &body3).await;
    assert_eq!(r3.provider.as_deref(), Some("b"));
    assert_eq!(r3.sse.content(), "pronto");
    assert_eq!(r3.sse.finish_reasons(), ["stop"]);
    assert_well_formed(&r3.sse);
    assert_usage_only_at_the_end(&r3.sse);
    assert_eq!(received_body(&b, 1).await["messages"], body3["messages"]);

    // /quota: `a` esfriando, tiers contados, `b` debitado duas vezes.
    let q = gw.quota().await;
    // O 429 esfria o escopo dono do limite: aqui, o modelo.
    assert_eq!(
        q["providers"][0]["models"][0]["state"]["state"], "cooldown",
        "{q}"
    );
    assert_eq!(
        q["providers"][1]["models"][0]["state"]["state"], "healthy",
        "{q}"
    );
    assert_eq!(q["served_by_tier"]["free"], 1, "{q}");
    assert_eq!(q["served_by_tier"]["paid"], 2, "{q}");
    assert_eq!(q["providers"][1]["models"][0]["rpd_remaining"], 98, "{q}");
    // TPD cobra o `usage` real das 2 rodadas (9100+20 e 9200+2), não a estimativa.
    assert_eq!(
        q["providers"][1]["models"][0]["tpd_remaining"],
        1_000_000 - 18_322,
        "{q}"
    );
    assert_eq!(q["providers"][0]["models"][0]["in_flight"], 0, "{q}");
    assert_eq!(q["providers"][1]["models"][0]["in_flight"], 0, "{q}");

    // Ledger: uma linha por rodada, metadados coerentes, nunca corpo.
    let lines = ledger_lines(&dir, 3).await;
    assert_eq!(lines.len(), 3, "{lines:?}");
    let col = |k: &str| lines.iter().map(|l| l[k].clone()).collect::<Vec<_>>();
    assert_eq!(col("provider"), [json!("a"), json!("b"), json!("b")]);
    assert_eq!(col("fallbacks"), [json!(0), json!(1), json!(0)]);
    assert_eq!(col("tier"), [json!("free"), json!("paid"), json!("paid")]);
    assert_eq!(col("in"), [json!(9000), json!(9100), json!(9200)]);
    assert_eq!(col("out"), [json!(40), json!(20), json!(2)]);
    for l in &lines {
        assert_eq!(l["route"], "kax/coder-smart", "{l}");
        assert_eq!(l["outcome"], "ok", "{l}");
        assert!(l["est_in"].as_u64().unwrap() > 6000, "{l}");
        let raw = l.to_string();
        for leak in ["You are Cline", "write_to_file", "fn main"] {
            assert!(!raw.contains(leak), "{leak} no ledger: {raw}");
        }
    }

    a.verify().await;
    b.verify().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Tool call já começou e o stream morre: sem fallback (o cliente já viu o 1º
/// token), `finish_reason: "error"` + `[DONE]`, ledger `mid_stream_error`.
#[tokio::test]
async fn cline_stream_dies_mid_tool_call_ends_cleanly() {
    let (a, b) = (MockServer::start().await, MockServer::start().await);
    let truncated = [
        chunk(json!({ "role": "assistant" }), None),
        chunk(
            tool_delta(0, Some("call_1"), Some("read_file"), "{\"pa"),
            None,
        ),
    ]
    .concat();
    upstream("ka", "up-a")
        .respond_with(sse_raw(truncated))
        .expect(1)
        .mount(&a)
        .await;
    upstream("kb", "up-b")
        .respond_with(sse_ok("não"))
        .expect(0)
        .mount(&b)
        .await;
    let dir = ledger_dir("mid");
    let gw = spawn_with_ledger(&config(&a.uri(), &b.uri()), KEYS, dir.clone()).await;

    let r = post_sse(&gw, &cline_body("kax/coder-smart", &[])).await;
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.sse.finish_reasons(), ["error"]);
    assert!(r.sse.done && !r.sse.trailing_after_done, "{:?}", r.sse);
    assert_eq!(rebuild_tool_calls(&r.sse)[&0].2, "{\"pa");

    // Morte no meio esfria só o modelo (F0-17) e o tier conta como atendido.
    let q = gw.quota().await;
    assert_eq!(
        q["providers"][0]["models"][0]["state"]["state"], "cooldown",
        "{q}"
    );
    assert_eq!(q["providers"][0]["state"]["state"], "healthy", "{q}");
    assert_eq!(q["served_by_tier"]["free"], 1, "{q}");
    let lines = ledger_lines(&dir, 1).await;
    assert_eq!(lines[0]["outcome"], "mid_stream_error", "{lines:?}");
    a.verify().await;
    b.verify().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Cliente sem streaming: `tool_calls` agregados, `content: null`, `usage` real.
#[tokio::test]
async fn non_stream_client_gets_aggregated_tool_calls_and_usage() {
    let (a, b) = (MockServer::start().await, MockServer::start().await);
    let body = [
        chunk_with_usage(
            tool_delta(0, Some("c1"), Some("read_file"), "{\"path\":"),
            None,
            11,
            1,
        ),
        chunk_with_usage(tool_delta(0, None, None, "\"a\"}"), None, 11, 2),
        chunk_with_usage(json!({}), Some("tool_calls"), 11, 3),
        DONE.to_owned(),
    ]
    .concat();
    upstream("ka", "up-a")
        .respond_with(sse_raw(body))
        .expect(1)
        .mount(&a)
        .await;
    let gw = Gateway::spawn(&config(&a.uri(), &b.uri()), KEYS).await;

    let resp = gw
        .http
        .post(format!("{}/v1/chat/completions", gw.base))
        .json(&json!({
            "model": "kax/coder-fast",
            "messages": [{ "role": "user", "content": "oi" }],
            "tools": cline_tools(),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    let choice = &v["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls", "{v}");
    assert_eq!(choice["message"]["content"], Value::Null, "{v}");
    assert_eq!(
        choice["message"]["tool_calls"][0]["function"]["arguments"], "{\"path\":\"a\"}",
        "{v}"
    );
    assert_eq!(v["usage"]["completion_tokens"], 3, "{v}");
    a.verify().await;
}
