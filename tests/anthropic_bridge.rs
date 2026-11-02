//! Aceite da ponte Anthropic (PLANO_MESTRE §3.5 e §4 Fase 1, ADR 0006).
//!
//! Escritos no CONTRACT (F1-1): `#[ignore = "F1-…"]` até a tarefa de cada um.
//! Upstream sempre é `wiremock`; nenhum provider real.

mod common;

use std::time::{Duration, Instant};

use reqwest::header::HeaderMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use common::*;
use openkax::scheduler::estimate_tokens;
use openkax::translate::anthropic::{MessagesRequest, to_openai_request};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Evento SSE do stream Anthropic: `event:` + `data:` JSON.
#[derive(Debug, Clone)]
struct Event {
    name: String,
    data: Value,
}

/// Estrito: toda entrada tem `event:` e `data:`, iguais ao `type` do JSON, e o
/// stream termina em linha em branco. `[DONE]` não existe neste protocolo.
fn parse_events(text: &str) -> Vec<Event> {
    assert!(text.ends_with("\n\n"), "evento sem terminador: {text:?}");
    assert!(!text.contains("[DONE]"), "[DONE] é do OpenAI: {text:?}");
    text.split("\n\n")
        .filter(|e| !e.is_empty())
        .map(|e| {
            let mut lines = e.lines();
            let name = lines
                .next()
                .and_then(|l| l.strip_prefix("event: "))
                .unwrap_or_else(|| panic!("sem event: {e:?}"));
            let data = lines
                .next()
                .and_then(|l| l.strip_prefix("data: "))
                .unwrap_or_else(|| panic!("sem data: {e:?}"));
            assert_eq!(lines.next(), None, "linha a mais: {e:?}");
            let data: Value = serde_json::from_str(data).expect("data não é JSON");
            assert_eq!(data["type"], name, "{e:?}");
            Event {
                name: name.to_owned(),
                data,
            }
        })
        .collect()
}

fn names(events: &[Event]) -> Vec<&str> {
    events.iter().map(|e| e.name.as_str()).collect()
}

fn text_of(events: &[Event]) -> String {
    events
        .iter()
        .filter_map(|e| e.data["delta"]["text"].as_str())
        .collect()
}

#[derive(Debug)]
struct Reply {
    status: u16,
    provider: Option<String>,
    retry_after: Option<String>,
    content_type: Option<String>,
    headers: HeaderMap,
    text: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text)
            .unwrap_or_else(|e| panic!("corpo não é JSON ({e}): {}", self.text))
    }

    fn events(&self) -> Vec<Event> {
        parse_events(&self.text)
    }

    /// Erro no formato da Anthropic, com o `type` esperado.
    fn assert_error(&self, status: u16, kind: &str) {
        assert_eq!(self.status, status, "{self:?}");
        let v = self.json();
        assert_eq!(v["type"], "error", "{self:?}");
        assert_eq!(v["error"]["type"], kind, "{self:?}");
        assert!(v["error"]["message"].is_string(), "{self:?}");
    }
}

async fn post(gw: &Gateway, route: &str, headers: &[(&str, &str)], body: Vec<u8>) -> Reply {
    let mut req = gw.http.post(format!("{}{route}", gw.base)).body(body);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req.send().await.expect("gateway não respondeu");
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let (provider, retry_after, content_type) = (
        header("x-openkax-provider"),
        header("retry-after"),
        header("content-type"),
    );
    let headers = resp.headers().clone();
    Reply {
        status: resp.status().as_u16(),
        provider,
        retry_after,
        content_type,
        headers,
        text: resp.text().await.expect("corpo do gateway"),
    }
}

async fn messages(gw: &Gateway, body: Value) -> Reply {
    post(
        gw,
        "/v1/messages",
        &[("content-type", "application/json")],
        body.to_string().into_bytes(),
    )
    .await
}

fn req_body(model: &str, stream: bool, text: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": 100,
        "stream": stream,
        "messages": [{ "role": "user", "content": text }],
    })
}

/// Upstream OpenAI do degrau: mesmas amarras do `tests/fallback.rs`.
fn upstream(key: &str, model: &str) -> wiremock::MockBuilder {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", format!("Bearer {key}").as_str()))
        .and(body_partial_json(json!({ "model": model, "stream": true })))
}

async fn mocks() -> (MockServer, MockServer) {
    (MockServer::start().await, MockServer::start().await)
}

async fn gateway(groq: &MockServer, gemini: &MockServer) -> Gateway {
    Gateway::spawn(&two_provider_config(&groq.uri(), &gemini.uri()), SECRETS).await
}

/// Upstream em TCP cru: manda `first`, espera o cliente confirmar que o recebeu
/// e só então manda `rest`. Prova que nada espera o fim do corpo.
async fn live_upstream(
    first: &'static str,
    rest: &'static str,
    content_type: &'static str,
    received: tokio::sync::oneshot::Receiver<()>,
) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = sock.read(&mut buf).await.unwrap();
            seen.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&seen);
            if let Some(end) = text.find("\r\n\r\n") {
                let len = text[..end]
                    .lines()
                    .find_map(|l| {
                        let l = l.to_ascii_lowercase();
                        l.strip_prefix("content-length: ")
                            .map(|v| v.parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if seen.len() >= end + 4 + len {
                    break;
                }
            }
        }
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\nconnection: close\r\ntransfer-encoding: chunked\r\n\r\n"
        );
        sock.write_all(head.as_bytes()).await.unwrap();
        let chunk = |s: &str| format!("{:x}\r\n{s}\r\n", s.len());
        sock.write_all(chunk(first).as_bytes()).await.unwrap();
        sock.flush().await.unwrap();
        let _ = received.await;
        sock.write_all(chunk(rest).as_bytes()).await.unwrap();
        sock.write_all(b"0\r\n\r\n").await.unwrap();
    });
    format!("http://{addr}")
}

/// Lê até `needle` aparecer, confirma ao upstream e lê o resto. Se o gateway
/// segurar o corpo, o `timeout` estoura: o upstream só termina depois da confirmação.
async fn read_live(
    resp: reqwest::Response,
    needle: &str,
    received: tokio::sync::oneshot::Sender<()>,
) -> String {
    let mut resp = resp;
    let mut text = String::new();
    let mut received = Some(received);
    while let Some(c) = tokio::time::timeout(Duration::from_secs(3), resp.chunk())
        .await
        .expect("corpo retido: o gateway esperou o fim do upstream")
        .unwrap()
    {
        text.push_str(std::str::from_utf8(&c).unwrap());
        if text.contains(needle)
            && let Some(tx) = received.take()
        {
            tx.send(()).unwrap();
        }
    }
    text
}

fn tempdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "openkax-bridge-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

// ------------------------------------------------------- endpoint kax/* (F1-4)

/// O corpo que chega ao upstream é a tradução, com o modelo do degrau.
#[tokio::test]
#[ignore = "F1-4"]
async fn stream_translates_request_and_response() {
    let (groq, gemini) = mocks().await;
    upstream("k-groq", "qwen-coder")
        .and(body_partial_json(json!({
            "max_tokens": 100,
            "messages": [
                { "role": "system", "content": "regras" },
                { "role": "user", "content": "oi" },
            ],
        })))
        .respond_with(sse_ok("olá"))
        .expect(1)
        .mount(&groq)
        .await;
    let gw = gateway(&groq, &gemini).await;

    let mut body = req_body("kax/coder-fast", true, "oi");
    body["system"] =
        json!([{ "type": "text", "text": "regras", "cache_control": { "type": "ephemeral" } }]);
    let r = messages(&gw, body).await;

    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.provider.as_deref(), Some("groq"));
    assert!(
        r.content_type
            .as_deref()
            .unwrap_or("")
            .starts_with("text/event-stream")
    );
    let ev = r.events();
    assert_eq!(
        names(&ev),
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );
    let start = &ev[0].data["message"];
    assert!(start["id"].as_str().unwrap().starts_with("msg_"));
    assert_eq!(
        start["model"], "kax/coder-fast",
        "o modelo que o cliente pediu"
    );
    assert!(start["usage"]["input_tokens"].as_u64().unwrap() > 0);
    assert_eq!(text_of(&ev), "olá");
    assert_eq!(ev[4].data["delta"]["stop_reason"], "end_turn");

    groq.verify().await;
}

#[tokio::test]
#[ignore = "F1-4"]
async fn non_stream_returns_a_full_message_with_tool_use() {
    let (groq, gemini) = mocks().await;
    let body = [
        chunk(json!({ "role": "assistant" }), None),
        chunk(json!({ "content": "vou ler" }), None),
        chunk(
            json!({ "tool_calls": [{ "index": 0, "id": "c1", "type": "function", "function": { "name": "Read", "arguments": "{\"p\":" } }] }),
            None,
        ),
        chunk(
            json!({ "tool_calls": [{ "index": 0, "function": { "arguments": "1}" } }] }),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
        "data: [DONE]\n\n".to_owned(),
    ]
    .concat();
    upstream("k-groq", "qwen-coder")
        .respond_with(sse_raw(body))
        .expect(1)
        .mount(&groq)
        .await;
    let gw = gateway(&groq, &gemini).await;

    let r = messages(&gw, req_body("kax/coder-fast", false, "oi")).await;

    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.provider.as_deref(), Some("groq"));
    let v = r.json();
    assert_eq!(v["type"], "message");
    assert_eq!(v["role"], "assistant");
    assert_eq!(v["model"], "kax/coder-fast");
    assert_eq!(v["stop_reason"], "tool_use");
    assert_eq!(
        v["content"],
        json!([
            { "type": "text", "text": "vou ler" },
            { "type": "tool_use", "id": "c1", "name": "Read", "input": { "p": 1 } },
        ])
    );

    groq.verify().await;
}

/// O evento sai quando o chunk chega, não no fim do upstream.
#[tokio::test]
#[ignore = "F1-4"]
async fn stream_is_delivered_live() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let first =
        "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"primeiro\"}}]}\n\n";
    let rest = "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" segundo\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let up = live_upstream(first, rest, "text/event-stream", rx).await;
    let gw = Gateway::spawn(&two_provider_config(&up, "http://127.0.0.1:9"), SECRETS).await;

    let resp = gw
        .http
        .post(format!("{}/v1/messages", gw.base))
        .body(req_body("kax/coder-fast", true, "oi").to_string())
        .send()
        .await
        .unwrap();
    let text = read_live(resp, "primeiro", tx).await;

    assert_eq!(text_of(&parse_events(&text)), "primeiro segundo");
}

/// Rodada completa com o Gemini: a assinatura volta sozinha, e o `id` que o
/// upstream vê é o dele.
#[tokio::test]
#[ignore = "F1-4"]
async fn thought_signature_survives_the_tool_round_trip() {
    let (groq, gemini) = mocks().await;
    let body = [
        chunk(
            json!({ "tool_calls": [{
                "index": 0,
                "id": "c1",
                "type": "function",
                "function": { "name": "Read", "arguments": "{}" },
                "extra_content": { "google": { "thought_signature": "SIG" } },
            }] }),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
        "data: [DONE]\n\n".to_owned(),
    ]
    .concat();
    upstream("k-gemini", "gemini-flash")
        .respond_with(sse_raw(body))
        .up_to_n_times(1)
        .expect(1)
        .mount(&gemini)
        .await;
    upstream("k-gemini", "gemini-flash")
        .and(body_partial_json(json!({ "messages": [
            { "role": "user", "content": "oi" },
            { "role": "assistant", "tool_calls": [{
                "id": "c1",
                "extra_content": { "google": { "thought_signature": "SIG" } },
            }] },
            { "role": "tool", "tool_call_id": "c1", "content": "conteúdo" },
        ] })))
        .respond_with(sse_ok("pronto"))
        .expect(1)
        .mount(&gemini)
        .await;
    let config = two_provider_config(&groq.uri(), &gemini.uri()).replace(
        r#"id = "gemini""#,
        "id = \"gemini\"\n        quirks = { thought_signatures = true }",
    );
    let gw = Gateway::spawn(&config, SECRETS).await;
    let pin = [("x-openkax-pin", "gemini/gemini-flash")];

    let first = post(
        &gw,
        "/v1/messages",
        &pin,
        req_body("kax/coder-fast", false, "oi")
            .to_string()
            .into_bytes(),
    )
    .await;
    assert_eq!(first.status, 200, "{first:?}");
    let tool_use = first.json()["content"][0].clone();
    let id = tool_use["id"].as_str().unwrap().to_owned();
    assert_ne!(id, "c1", "a assinatura precisa ir junto no id");

    let mut second = req_body("kax/coder-fast", false, "oi");
    second["messages"] = json!([
        { "role": "user", "content": "oi" },
        { "role": "assistant", "content": [tool_use] },
        { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": id, "content": "conteúdo" }] },
    ]);
    let r = post(&gw, "/v1/messages", &pin, second.to_string().into_bytes()).await;

    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.json()["content"][0]["text"], "pronto");
    gemini.verify().await;
}

/// Mesmos cenários do `tests/fallback.rs`, agora por `/v1/messages`.
#[tokio::test]
#[ignore = "F1-4"]
async fn fallback_on_429_and_provider_stays_in_cooldown() {
    let (groq, gemini) = mocks().await;
    upstream("k-groq", "qwen-coder")
        .respond_with(rate_limited(30))
        .expect(1)
        .mount(&groq)
        .await;
    upstream("k-gemini", "gemini-flash")
        .respond_with(sse_ok("from-gemini"))
        .expect(2)
        .mount(&gemini)
        .await;
    let gw = gateway(&groq, &gemini).await;

    for attempt in 1..=2 {
        let r = messages(&gw, req_body("kax/coder-fast", true, "oi")).await;
        assert_eq!(r.status, 200, "request {attempt}: {r:?}");
        assert_eq!(r.provider.as_deref(), Some("gemini"), "request {attempt}");
        assert_eq!(text_of(&r.events()), "from-gemini", "request {attempt}");
    }

    groq.verify().await;
    gemini.verify().await;
}

#[tokio::test]
#[ignore = "F1-4"]
async fn fallback_on_first_token_timeout() {
    let (groq, gemini) = mocks().await;
    upstream("k-groq", "qwen-coder")
        .respond_with(sse_ok("from-groq").set_delay(Duration::from_secs(3)))
        .expect(1)
        .mount(&groq)
        .await;
    upstream("k-gemini", "gemini-flash")
        .respond_with(sse_ok("from-gemini"))
        .expect(1)
        .mount(&gemini)
        .await;
    let gw = gateway(&groq, &gemini).await;

    let t0 = Instant::now();
    let r = messages(&gw, req_body("kax/coder-fast", true, "oi")).await;

    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.provider.as_deref(), Some("gemini"));
    assert_eq!(text_of(&r.events()), "from-gemini");
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "esperou o upstream lento"
    );

    gemini.verify().await;
}

/// Morte pós-1º-token: evento `error`, sem `message_stop` e sem fallback.
#[tokio::test]
#[ignore = "F1-4"]
async fn post_first_token_failure_becomes_error_event() {
    let (groq, gemini) = mocks().await;
    let body = [
        chunk(json!({ "content": "parcial" }), None),
        "data: {isto não é json\n\n".to_owned(),
    ]
    .concat();
    upstream("k-groq", "qwen-coder")
        .respond_with(sse_raw(body))
        .expect(1)
        .mount(&groq)
        .await;
    upstream("k-gemini", "gemini-flash")
        .respond_with(sse_ok("não devia"))
        .expect(0)
        .mount(&gemini)
        .await;
    let gw = gateway(&groq, &gemini).await;

    let r = messages(&gw, req_body("kax/coder-fast", true, "oi")).await;

    assert_eq!(r.status, 200, "{r:?}");
    let ev = r.events();
    assert_eq!(text_of(&ev), "parcial");
    assert_eq!(
        names(&ev),
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "error"
        ]
    );
    assert_eq!(ev[3].data["error"]["type"], "api_error");

    groq.verify().await;
    gemini.verify().await;
}

/// Não-stream: a mesma morte é um erro HTTP completo.
#[tokio::test]
#[ignore = "F1-4"]
async fn post_first_token_failure_non_stream_is_http_error() {
    let (groq, gemini) = mocks().await;
    let body = [
        chunk(json!({ "content": "parcial" }), None),
        "data: {isto não é json\n\n".to_owned(),
    ]
    .concat();
    upstream("k-groq", "qwen-coder")
        .respond_with(sse_raw(body))
        .mount(&groq)
        .await;
    let gw = gateway(&groq, &gemini).await;

    let r = messages(&gw, req_body("kax/coder-fast", false, "oi")).await;

    r.assert_error(502, "api_error");
}

#[tokio::test]
#[ignore = "F1-4"]
async fn insufficient_tpm_skips_provider_without_a_request() {
    let (groq, gemini) = mocks().await;
    upstream("k-groq", "qwen-coder")
        .respond_with(sse_ok("não devia"))
        .expect(0)
        .mount(&groq)
        .await;
    upstream("k-gemini", "gemini-flash")
        .respond_with(sse_ok("from-gemini"))
        .expect(1)
        .mount(&gemini)
        .await;
    let gw = gateway(&groq, &gemini).await;

    // 40k chars / 3.6 ≈ 11.1k tokens > TPM 8k do groq; < 60k da classe.
    let r = messages(&gw, req_body("kax/coder-fast", true, &"x".repeat(40_000))).await;

    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.provider.as_deref(), Some("gemini"));

    groq.verify().await;
    gemini.verify().await;
}

#[tokio::test]
#[ignore = "F1-4"]
async fn all_exhausted_is_429_with_retry_after_in_anthropic_format() {
    let (groq, gemini) = mocks().await;
    upstream("k-groq", "qwen-coder")
        .respond_with(rate_limited(30))
        .mount(&groq)
        .await;
    upstream("k-gemini", "gemini-flash")
        .respond_with(rate_limited(30))
        .mount(&gemini)
        .await;
    let gw = gateway(&groq, &gemini).await;

    let r = messages(&gw, req_body("kax/coder-fast", true, "oi")).await;

    r.assert_error(429, "rate_limit_error");
    let secs: u64 = r
        .retry_after
        .as_deref()
        .expect("retry-after")
        .parse()
        .unwrap();
    assert!(secs >= 1, "{secs}");
}

/// Pacote maior que `max_packet_tokens` da classe: 413 sem tocar upstream.
#[tokio::test]
#[ignore = "F1-4"]
async fn too_large_is_413_in_anthropic_format() {
    let (groq, gemini) = mocks().await;
    let gw = gateway(&groq, &gemini).await;

    let r = messages(&gw, req_body("kax/coder-fast", true, &"x".repeat(300_000))).await;

    r.assert_error(413, "request_too_large");
    assert!(groq.received_requests().await.unwrap().is_empty());
}

/// Sem chave nenhuma: nenhum provider disponível.
#[tokio::test]
#[ignore = "F1-4"]
async fn no_provider_available_is_503_in_anthropic_format() {
    let (groq, gemini) = mocks().await;
    let gw = Gateway::spawn(&two_provider_config(&groq.uri(), &gemini.uri()), &[]).await;

    let r = messages(&gw, req_body("kax/coder-fast", true, "oi")).await;

    r.assert_error(503, "overloaded_error");
}

/// Erro de culpa da request nos dois degraus volta com o status do upstream,
/// no formato da Anthropic.
#[tokio::test]
#[ignore = "F1-4"]
async fn upstream_request_fault_keeps_status_in_anthropic_format() {
    let (groq, gemini) = mocks().await;
    let fault = || {
        ResponseTemplate::new(400).set_body_json(
            json!({ "error": { "message": "tools inválidas", "type": "invalid_request" } }),
        )
    };
    upstream("k-groq", "qwen-coder")
        .respond_with(fault())
        .mount(&groq)
        .await;
    upstream("k-gemini", "gemini-flash")
        .respond_with(fault())
        .mount(&gemini)
        .await;
    let gw = gateway(&groq, &gemini).await;

    let r = messages(&gw, req_body("kax/coder-fast", true, "oi")).await;

    r.assert_error(400, "invalid_request_error");
    assert!(
        r.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("tools inválidas")
    );
}

#[tokio::test]
#[ignore = "F1-4"]
async fn bad_requests_answer_in_anthropic_format() {
    let (groq, gemini) = mocks().await;
    let gw = gateway(&groq, &gemini).await;

    let r = post(&gw, "/v1/messages", &[], b"{nao json".to_vec()).await;
    r.assert_error(400, "invalid_request_error");

    let r = messages(&gw, req_body("kax/nao-existe", true, "oi")).await;
    r.assert_error(404, "not_found_error");

    let r = messages(&gw, req_body("gpt-qualquer", true, "oi")).await;
    r.assert_error(404, "not_found_error");

    // Erro de `x-openkax-*` também sai neste formato.
    let body = req_body("kax/coder-fast", true, "oi")
        .to_string()
        .into_bytes();
    for (name, value, status, kind) in [
        ("x-openkax-pin", "sem-barra", 400, "invalid_request_error"),
        ("x-openkax-pin", "groq/nao-existe", 404, "not_found_error"),
        ("x-openkax-max-tier", "ouro", 400, "invalid_request_error"),
    ] {
        let r = post(&gw, "/v1/messages", &[(name, value)], body.clone()).await;
        r.assert_error(status, kind);
    }

    // `max_tokens` é obrigatório neste protocolo.
    let r = messages(
        &gw,
        json!({ "model": "kax/coder-fast", "messages": [{ "role": "user", "content": "oi" }] }),
    )
    .await;
    r.assert_error(400, "invalid_request_error");
}

/// Os headers `x-openkax-*` valem aqui também.
#[tokio::test]
#[ignore = "F1-4"]
async fn openkax_headers_apply() {
    let (groq, gemini) = mocks().await;
    upstream("k-gemini", "gemini-flash")
        .respond_with(sse_ok("fixado"))
        .expect(1)
        .mount(&gemini)
        .await;
    let gw = gateway(&groq, &gemini).await;

    let r = post(
        &gw,
        "/v1/messages",
        &[("x-openkax-pin", "gemini/gemini-flash")],
        req_body("kax/coder-fast", true, "oi")
            .to_string()
            .into_bytes(),
    )
    .await;

    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.provider.as_deref(), Some("gemini"));
    gemini.verify().await;
}

#[tokio::test]
#[ignore = "F1-4"]
async fn ledger_and_quota_are_coherent() {
    let (groq, gemini) = mocks().await;
    let dir = tempdir("ledger-kax");
    upstream("k-groq", "qwen-coder")
        .respond_with(sse_ok("segredo-da-resposta"))
        .mount(&groq)
        .await;
    let gw = spawn_with_ledger(
        &two_provider_config(&groq.uri(), &gemini.uri()),
        SECRETS,
        dir.clone(),
    )
    .await;

    let r = messages(&gw, req_body("kax/coder-fast", true, "segredo-do-prompt")).await;
    assert_eq!(r.status, 200, "{r:?}");

    let lines = ledger_lines(&dir, 1).await;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["route"], "kax/coder-fast");
    assert_eq!(lines[0]["provider"], "groq");
    assert_eq!(lines[0]["outcome"], "ok");
    let text = lines[0].to_string();
    assert!(!text.contains("segredo"), "{text}");
    assert_eq!(gw.quota().await["served_by_tier"]["free"], 1);
}

// ------------------------------------------------------------ count_tokens (F1-5)

/// Mesma estimativa do scheduler, com o `chars_per_token` do 1º candidato.
#[tokio::test]
#[ignore = "F1-5"]
async fn count_tokens_uses_the_scheduler_estimate() {
    let (groq, gemini) = mocks().await;
    let gw = gateway(&groq, &gemini).await;
    let body = json!({
        "model": "kax/coder-fast",
        "system": "regras do sistema",
        "messages": [{ "role": "user", "content": "x".repeat(3_600) }],
        "tools": [{ "name": "Read", "input_schema": { "type": "object" } }],
    });
    let parsed: MessagesRequest = serde_json::from_value(body.clone()).unwrap();
    let want = estimate_tokens(&to_openai_request(&parsed), 3.6);

    let r = post(
        &gw,
        "/v1/messages/count_tokens",
        &[],
        body.to_string().into_bytes(),
    )
    .await;

    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.json(), json!({ "input_tokens": want }));
    assert!(want >= 1000, "{want}");
    assert!(groq.received_requests().await.unwrap().is_empty());
    assert!(gemini.received_requests().await.unwrap().is_empty());
}

/// Estimar não depende de cota nem de caber: sem chave e acima do limite do
/// pacote, a resposta é o número.
#[tokio::test]
#[ignore = "F1-5"]
async fn count_tokens_ignores_quota_and_packet_limit() {
    let (groq, gemini) = mocks().await;
    let gw = Gateway::spawn(&two_provider_config(&groq.uri(), &gemini.uri()), &[]).await;
    let body = json!({
        "model": "kax/coder-fast",
        "messages": [{ "role": "user", "content": "x".repeat(300_000) }],
    });

    let r = post(
        &gw,
        "/v1/messages/count_tokens",
        &[],
        body.to_string().into_bytes(),
    )
    .await;

    assert_eq!(r.status, 200, "{r:?}");
    assert!(
        r.json()["input_tokens"].as_u64().unwrap() >= 80_000,
        "{r:?}"
    );
}

#[tokio::test]
#[ignore = "F1-5"]
async fn count_tokens_errors_in_anthropic_format() {
    let (groq, gemini) = mocks().await;
    let gw = gateway(&groq, &gemini).await;

    let r = post(&gw, "/v1/messages/count_tokens", &[], b"{".to_vec()).await;
    r.assert_error(400, "invalid_request_error");

    let body = json!({ "model": "kax/nao-existe", "messages": [] });
    let r = post(
        &gw,
        "/v1/messages/count_tokens",
        &[],
        body.to_string().into_bytes(),
    )
    .await;
    r.assert_error(404, "not_found_error");
}

// ------------------------------------------------------------- passthrough (F1-6)

/// Config de dois providers + `anthropic` nativo apontando para o mock.
fn passthrough_config(anthropic: &str, extra: &str) -> String {
    let base = two_provider_config("http://127.0.0.1:9", "http://127.0.0.1:9");
    format!(
        r#"{base}
        [[provider]]
        id = "anthropic"
        base_url = "{anthropic}"
        auth = {{ kind = "passthrough" }}
        native = "anthropic"
        {extra}
        "#
    )
}

const CLIENT_HEADERS: &[(&str, &str)] = &[
    ("authorization", "Bearer sk-ant-oat-do-cliente"),
    ("x-api-key", "sk-ant-api-do-cliente"),
    ("anthropic-version", "2023-06-01"),
    (
        "anthropic-beta",
        "oauth-2025-04-20,prompt-caching-2024-07-31",
    ),
    ("user-agent", "claude-cli/9.9"),
    ("x-app", "cli"),
    ("content-type", "application/json"),
];

/// Corpo com espaços, ordem de chaves e formas que `MessagesRequest` recusaria: se
/// o gateway reserializar, muda; se parsear além de `model`, quebra.
const ODD_BODY: &str = "{ \"max_tokens\":1,  \"zeta\" : [1, 2.50],\n\"model\":\"claude-sonnet-9\",\"stream\":\"talvez\",\"messages\":[{\"role\":\"inventado\",\"content\":7,\"x\":\"ol\\u00e1 é\"}], \"tools\":[{\"novo\":true}], \"alfa\":{} }";

/// Bytes de stream que um tradutor não deixaria iguais (comentário, espaço, ordem).
const ODD_STREAM: &str = ": ping cru\n\nevent: message_start\ndata:   {\"type\":\"message_start\",\"zzz\":1,\"aaa\":2}\n\nevent: vendor_event\ndata: {\"x\": 1}\n\n";

fn header_of(req: &Request, name: &str) -> Option<String> {
    req.headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

#[tokio::test]
#[ignore = "F1-6"]
async fn passthrough_keeps_body_and_headers_identical() {
    let anth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("request-id", "req_123")
                .insert_header("anthropic-ratelimit-requests-remaining", "49")
                .insert_header("content-encoding", "gzip")
                .insert_header("x-vendor-qualquer", "fica")
                .set_body_raw(ODD_STREAM, "text/event-stream"),
        )
        .expect(1)
        .mount(&anth)
        .await;
    let gw = Gateway::spawn(&passthrough_config(&anth.uri(), ""), SECRETS).await;

    let mut headers = CLIENT_HEADERS.to_vec();
    headers.extend([("te", "trailers"), ("proxy-authorization", "Basic hop")]);
    // Claude Code usa `?beta=true`.
    let resp = post(
        &gw,
        "/v1/messages?beta=true",
        &headers,
        ODD_BODY.as_bytes().to_vec(),
    )
    .await;

    assert_eq!(resp.status, 200, "{resp:?}");
    assert_eq!(resp.text, ODD_STREAM, "stream reempacotado");
    assert_eq!(resp.provider.as_deref(), Some("anthropic"));
    for (name, want) in [
        ("request-id", "req_123"),
        ("anthropic-ratelimit-requests-remaining", "49"),
        ("content-encoding", "gzip"),
        ("x-vendor-qualquer", "fica"),
    ] {
        assert_eq!(
            resp.headers.get(name).and_then(|v| v.to_str().ok()),
            Some(want),
            "header de resposta {name}"
        );
    }

    let got = anth.received_requests().await.unwrap();
    assert_eq!(got.len(), 1);
    let up = &got[0];
    assert_eq!(up.body, ODD_BODY.as_bytes(), "corpo alterado");
    assert_eq!(up.url.query(), Some("beta=true"));
    for (k, v) in CLIENT_HEADERS {
        assert_eq!(header_of(up, k).as_deref(), Some(*v), "header {k}");
    }
    for hop in ["te", "proxy-authorization"] {
        assert_eq!(header_of(up, hop), None, "hop-by-hop {hop} repassado");
    }
    let host = header_of(up, "host").unwrap();
    assert!(
        anth.uri().ends_with(&host),
        "host {host} não é o do upstream"
    );

    anth.verify().await;
}

#[tokio::test]
#[ignore = "F1-6"]
async fn passthrough_returns_response_headers_and_errors_untouched() {
    let anth = MockServer::start().await;
    let err = r#"{"type":"error","error":{"type":"rate_limit_error","message":"cheio"},"request_id":"req_9"}"#;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "17")
                .insert_header("request-id", "req_9")
                .set_body_raw(err, "application/json"),
        )
        .expect(2)
        .mount(&anth)
        .await;
    let gw = Gateway::spawn(&passthrough_config(&anth.uri(), ""), SECRETS).await;

    // Sem scheduler: um 429 da Anthropic não põe nada em cooldown.
    for _ in 0..2 {
        let r = post(
            &gw,
            "/v1/messages",
            CLIENT_HEADERS,
            ODD_BODY.as_bytes().to_vec(),
        )
        .await;
        assert_eq!(r.status, 429, "{r:?}");
        assert_eq!(r.text, err);
        assert_eq!(r.retry_after.as_deref(), Some("17"));
        assert_eq!(r.content_type.as_deref(), Some("application/json"));
    }

    anth.verify().await;
}

/// O stream passa enquanto chega: nada de `.bytes().await` no meio.
#[tokio::test]
#[ignore = "F1-6"]
async fn passthrough_stream_is_delivered_live() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let up = live_upstream(
        "event: a\ndata: {\"x\": 1}\n\n",
        "event: b\ndata: {\"y\": 2}\n\n",
        "text/event-stream",
        rx,
    )
    .await;
    let gw = Gateway::spawn(&passthrough_config(&up, ""), SECRETS).await;

    let mut req = gw
        .http
        .post(format!("{}/v1/messages", gw.base))
        .body(ODD_BODY);
    for (k, v) in CLIENT_HEADERS {
        req = req.header(*k, *v);
    }
    let text = read_live(req.send().await.unwrap(), "event: a", tx).await;

    assert_eq!(
        text,
        "event: a\ndata: {\"x\": 1}\n\nevent: b\ndata: {\"y\": 2}\n\n"
    );
}

/// Claude Code conta tokens em `claude-*` também: vai para a Anthropic.
#[tokio::test]
#[ignore = "F1-6"]
async fn passthrough_count_tokens() {
    let anth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("{\"input_tokens\": 42}", "application/json"),
        )
        .expect(1)
        .mount(&anth)
        .await;
    let gw = Gateway::spawn(&passthrough_config(&anth.uri(), ""), SECRETS).await;

    let r = post(
        &gw,
        "/v1/messages/count_tokens",
        CLIENT_HEADERS,
        ODD_BODY.as_bytes().to_vec(),
    )
    .await;

    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.text, "{\"input_tokens\": 42}");
    assert_eq!(
        anth.received_requests().await.unwrap()[0].body,
        ODD_BODY.as_bytes()
    );
}

/// Ledger só com metadados: nem corpo nem auth, na ida ou na volta.
#[tokio::test]
#[ignore = "F1-6"]
async fn passthrough_ledger_has_metadata_only() {
    let anth = MockServer::start().await;
    let dir = tempdir("ledger-pass");
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw("data: segredo-da-resposta\n\n", "text/event-stream"),
        )
        .mount(&anth)
        .await;
    let gw = spawn_with_ledger(&passthrough_config(&anth.uri(), ""), SECRETS, dir.clone()).await;

    let body = req_body("claude-sonnet-9", true, "segredo-do-prompt");
    let r = post(
        &gw,
        "/v1/messages",
        CLIENT_HEADERS,
        body.to_string().into_bytes(),
    )
    .await;
    assert_eq!(r.status, 200, "{r:?}");

    let lines = ledger_lines(&dir, 1).await;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["route"], "claude-sonnet-9");
    assert_eq!(lines[0]["provider"], "anthropic");
    assert_eq!(lines[0]["outcome"], "ok");
    assert_eq!(lines[0]["status"], 200);
    assert!(lines[0]["latency_ms"].is_u64());
    let text = lines[0].to_string();
    for leak in ["segredo", "sk-ant", "oauth-2025"] {
        assert!(!text.contains(leak), "{leak} no ledger: {text}");
    }
    // Sem scheduler: nada de cota na Anthropic.
    assert_eq!(gw.quota().await["served_by_tier"]["free"], 0);
}

/// Erro da Anthropic aparece no ledger com o status, sem passar por sucesso.
#[tokio::test]
#[ignore = "F1-6"]
async fn passthrough_ledger_records_upstream_errors() {
    let anth = MockServer::start().await;
    let dir = tempdir("ledger-pass-429");
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(429).set_body_string("{}"))
        .mount(&anth)
        .await;
    let gw = spawn_with_ledger(&passthrough_config(&anth.uri(), ""), SECRETS, dir.clone()).await;

    let r = post(
        &gw,
        "/v1/messages",
        CLIENT_HEADERS,
        ODD_BODY.as_bytes().to_vec(),
    )
    .await;
    assert_eq!(r.status, 429, "{r:?}");

    let lines = ledger_lines(&dir, 1).await;
    assert_eq!(lines[0]["status"], 429, "{lines:?}");
    assert_ne!(lines[0]["outcome"], "ok", "{lines:?}");
}

/// Anthropic fora do ar: erro no formato dela, sem tentar outro provider.
#[tokio::test]
#[ignore = "F1-6"]
async fn passthrough_unreachable_upstream_is_an_anthropic_error() {
    let gw = Gateway::spawn(&passthrough_config("http://127.0.0.1:9", ""), SECRETS).await;

    let r = post(
        &gw,
        "/v1/messages",
        CLIENT_HEADERS,
        ODD_BODY.as_bytes().to_vec(),
    )
    .await;

    r.assert_error(502, "api_error");
}

// ---------------------------------------------------------------- aliases (F1-7)

const ALIASES: &str = r#"
        [aliases]
        "claude-haiku-*" = "kax/coder-fast"
        "claude-exato" = "kax/coder-fast"
"#;

/// Config de `passthrough_config` com os upstreams kax/* de verdade.
fn alias_config(groq: &str, gemini: &str, anthropic: &str) -> String {
    let base = two_provider_config(groq, gemini);
    format!(
        r#"{base}
        [[provider]]
        id = "anthropic"
        base_url = "{anthropic}"
        auth = {{ kind = "passthrough" }}
        native = "anthropic"
        {ALIASES}
        "#
    )
}

#[tokio::test]
#[ignore = "F1-7"]
async fn alias_routes_to_the_class_and_the_rest_to_passthrough() {
    let (groq, gemini) = mocks().await;
    let anth = MockServer::start().await;
    upstream("k-groq", "qwen-coder")
        .respond_with(sse_ok("via-classe"))
        .expect(2)
        .mount(&groq)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(ODD_STREAM, "text/event-stream"))
        .expect(1)
        .mount(&anth)
        .await;
    let gw = Gateway::spawn(
        &alias_config(&groq.uri(), &gemini.uri(), &anth.uri()),
        SECRETS,
    )
    .await;

    for model in ["claude-haiku-4-5-20251001", "claude-exato"] {
        let r = post(
            &gw,
            "/v1/messages",
            CLIENT_HEADERS,
            req_body(model, true, "oi").to_string().into_bytes(),
        )
        .await;
        assert_eq!(r.status, 200, "{model}: {r:?}");
        assert_eq!(r.provider.as_deref(), Some("groq"), "{model}");
        let ev = r.events();
        assert_eq!(text_of(&ev), "via-classe", "{model}");
        assert_eq!(
            ev[0].data["message"]["model"], model,
            "o cliente vê o que pediu"
        );
    }

    let r = post(
        &gw,
        "/v1/messages",
        CLIENT_HEADERS,
        req_body("claude-sonnet-9", true, "oi")
            .to_string()
            .into_bytes(),
    )
    .await;
    assert_eq!(r.provider.as_deref(), Some("anthropic"), "{r:?}");
    assert_eq!(r.text, ODD_STREAM);

    groq.verify().await;
    anth.verify().await;
}

/// Com alias o usuário tirou o modelo da Anthropic: `count_tokens` também fica aqui.
#[tokio::test]
#[ignore = "F1-7"]
async fn alias_applies_to_count_tokens() {
    let (groq, gemini) = mocks().await;
    let anth = MockServer::start().await;
    let gw = Gateway::spawn(
        &alias_config(&groq.uri(), &gemini.uri(), &anth.uri()),
        SECRETS,
    )
    .await;

    let r = post(
        &gw,
        "/v1/messages/count_tokens",
        CLIENT_HEADERS,
        req_body("claude-haiku-4-5", false, "oi")
            .to_string()
            .into_bytes(),
    )
    .await;

    assert_eq!(r.status, 200, "{r:?}");
    assert!(r.json()["input_tokens"].as_u64().unwrap() > 0, "{r:?}");
    assert!(
        anth.received_requests().await.unwrap().is_empty(),
        "foi à Anthropic"
    );
}

/// Sem `[aliases]`, `claude-haiku-*` é passthrough como outro `claude-*`.
#[tokio::test]
#[ignore = "F1-7"]
async fn no_aliases_means_passthrough() {
    let anth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(ODD_STREAM, "text/event-stream"))
        .expect(1)
        .mount(&anth)
        .await;
    let gw = Gateway::spawn(&passthrough_config(&anth.uri(), ""), SECRETS).await;

    let r = post(
        &gw,
        "/v1/messages",
        CLIENT_HEADERS,
        req_body("claude-haiku-4-5", true, "oi")
            .to_string()
            .into_bytes(),
    )
    .await;

    assert_eq!(r.provider.as_deref(), Some("anthropic"), "{r:?}");
    anth.verify().await;
}

/// `providers.toml` de produção vem sem aliases (Modo A é opt-in).
#[test]
fn production_toml_has_no_aliases() {
    let text = std::fs::read_to_string("providers.toml").unwrap();
    let table: toml::Table = text.parse().unwrap();
    assert!(!table.contains_key("aliases"), "{:?}", table.get("aliases"));
}
