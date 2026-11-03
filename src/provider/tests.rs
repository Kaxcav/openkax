use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
use serde_json::{Value, json};

use super::*;
use crate::config::Config;

fn chunk(delta: Value, finish: Option<&str>) -> ChatCompletionChunk {
    serde_json::from_value(json!({
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }]
    }))
    .unwrap()
}

#[test]
fn first_token_is_content_or_tool_calls_only() {
    assert!(!is_first_token(&chunk(
        json!({ "role": "assistant" }),
        None
    )));
    assert!(!is_first_token(&chunk(json!({ "content": "" }), None)));
    assert!(!is_first_token(&chunk(json!({ "tool_calls": [] }), None)));
    assert!(!is_first_token(&chunk(json!({}), Some("stop"))));
    let usage_only: ChatCompletionChunk =
        serde_json::from_value(json!({ "choices": [], "usage": { "prompt_tokens": 3 } })).unwrap();
    assert!(!is_first_token(&usage_only));

    assert!(is_first_token(&chunk(json!({ "content": "a" }), None)));
    assert!(is_first_token(&chunk(
        json!({ "role": "assistant", "content": "oi" }),
        None
    )));
    assert!(is_first_token(&chunk(
        json!({ "tool_calls": [{ "index": 0, "function": { "name": "f" } }] }),
        None
    )));
}

fn utc0() -> SystemTime {
    // 2001-09-09T01:46:40Z (domingo)
    UNIX_EPOCH + Duration::from_secs(1_000_000_000)
}

fn headers(retry_after: Option<&str>) -> HeaderMap {
    let mut h = HeaderMap::new();
    if let Some(v) = retry_after {
        h.insert(RETRY_AFTER, HeaderValue::from_str(v).unwrap());
    }
    h
}

#[test]
fn classify_request_fault_auth_not_found_other() {
    let h = headers(None);
    for s in [400, 413, 422] {
        assert!(
            matches!(classify_status(s, &h, "bad param", utc0()), UpstreamError::RequestFault { status, .. } if status == s),
            "{s}"
        );
    }
    for s in [401, 403] {
        assert_eq!(
            classify_status(s, &h, "", utc0()),
            UpstreamError::Auth { status: s }
        );
    }
    assert_eq!(
        classify_status(404, &h, "", utc0()),
        UpstreamError::NotFound { status: 404 }
    );
    // F0-21: 410 Gone = modelo descontinuado.
    assert_eq!(classify_status(410, &h, "", utc0()), UpstreamError::Gone);
    assert_eq!(
        classify_status(408, &h, "", utc0()),
        UpstreamError::RequestTimeout
    );
    assert_eq!(
        classify_status(418, &h, "", utc0()),
        UpstreamError::OtherClient { status: 418 }
    );
    for s in [500, 502, 503] {
        assert_eq!(
            classify_status(s, &h, "", utc0()),
            UpstreamError::Server { status: s }
        );
    }
}

#[test]
fn request_fault_message_is_truncated() {
    let long = "é".repeat(5000);
    let UpstreamError::RequestFault { message, .. } =
        classify_status(400, &headers(None), &long, utc0())
    else {
        panic!()
    };
    assert!(message.len() <= 2048, "{}", message.len());
}

#[test]
fn classify_429_retry_after_seconds_and_http_date() {
    assert_eq!(
        classify_status(429, &headers(Some("17")), "", utc0()),
        UpstreamError::RateLimited {
            retry_after: Some(Duration::from_secs(17)),
            daily: false
        }
    );
    assert_eq!(
        classify_status(
            429,
            &headers(Some("Sun, 09 Sep 2001 01:47:22 GMT")),
            "",
            utc0()
        ),
        UpstreamError::RateLimited {
            retry_after: Some(Duration::from_secs(42)),
            daily: false
        }
    );
    // Data no passado → zero (o scheduler aplica o piso de 1s).
    assert_eq!(
        classify_status(
            429,
            &headers(Some("Sun, 09 Sep 2001 01:00:00 GMT")),
            "",
            utc0()
        ),
        UpstreamError::RateLimited {
            retry_after: Some(Duration::ZERO),
            daily: false
        }
    );
    assert_eq!(
        classify_status(429, &headers(Some("lixo")), "", utc0()),
        UpstreamError::RateLimited {
            retry_after: None,
            daily: false
        }
    );
}

#[test]
fn classify_429_detects_daily_quota_in_body() {
    for body in [
        r#"{"error":{"message":"Rate limit reached on requests per day (RPD): Limit 1000"}}"#,
        r#"{"error":{"details":[{"quotaId":"GenerateRequestsPerDayPerProjectPerModel-FreeTier"}]}}"#,
        r#"{"error":{"message":"Rate limit exceeded: free-models-per-day"}}"#,
        r#"{"error":{"message":"daily quota exhausted"}}"#,
    ] {
        assert!(
            matches!(
                classify_status(429, &headers(None), body, utc0()),
                UpstreamError::RateLimited { daily: true, .. }
            ),
            "{body}"
        );
    }
    assert!(matches!(
        classify_status(
            429,
            &headers(None),
            r#"{"error":"tokens per minute"}"#,
            utc0()
        ),
        UpstreamError::RateLimited { daily: false, .. }
    ));
}

#[test]
fn sse_decoder_handles_split_crlf_comments_and_multiline() {
    let mut d = SseDecoder::default();
    assert_eq!(d.push(b"data: a\n\nda"), vec!["a".to_owned()]);
    assert_eq!(
        d.push(b"ta: b\r\n\r\n: keepalive\n\nevent: x\ndata: c1\ndata:c2\n\n"),
        vec!["b".to_owned(), "c1\nc2".to_owned()]
    );
    assert!(d.push(b": so comentario\n\n").is_empty());
    assert!(d.push(b"data: incompleto").is_empty());
}

fn provider_from(toml_provider: &str) -> crate::config::Provider {
    let c = Config::from_toml_str(&format!(
        "[[provider]]\nid = \"p\"\nbase_url = \"http://p\"\nauth = {{ kind = \"none\" }}\nmodels = [{{ id = \"m\", upstream = \"real\" }}]\n{toml_provider}"
    ))
    .unwrap();
    c.providers[0].clone()
}

fn request(extra: Value) -> ChatCompletionRequest {
    let mut v = json!({ "model": "kax/x", "messages": [{ "role": "user", "content": "oi" }] });
    v.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    serde_json::from_value(v).unwrap()
}

#[test]
fn upstream_body_applies_model_quirks_usage_and_clamp() {
    let p = provider_from(
        "quirks = { strip_params = [\"logprobs\"], tool_choice = \"auto_only\", max_tokens_cap = 2000 }",
    );
    let req = request(json!({
        "logprobs": true, "tool_choice": "required", "temperature": 0.2,
        "max_tokens": 32000, "max_completion_tokens": 32000,
        "tools": [{ "type": "function", "function": { "name": "f" } }],
    }));
    let b = build_upstream_body(&p, &req, "real", Some(2000));
    assert_eq!(b["model"], "real");
    assert_eq!(b["stream"], true);
    assert!(b.get("logprobs").is_none());
    assert_eq!(b["tool_choice"], "auto");
    assert_eq!(b["temperature"], 0.2);
    assert_eq!(b["max_tokens"], 2000);
    assert_eq!(b["max_completion_tokens"], 2000);
    assert_eq!(b["stream_options"]["include_usage"], true);
    assert_eq!(b["messages"][0]["content"], "oi");
}

#[test]
fn upstream_body_auto_only_drops_tools_for_none() {
    let p = provider_from("quirks = { tool_choice = \"auto_only\" }");
    let req = request(json!({ "tool_choice": "none", "tools": [{ "type": "function" }] }));
    let b = build_upstream_body(&p, &req, "real", None);
    assert!(
        b.get("tool_choice").is_none() && b.get("tools").is_none(),
        "{b}"
    );
}

#[test]
fn upstream_body_without_stream_usage_quirk() {
    let p = provider_from("quirks = { no_stream_usage = true }");
    let req = request(json!({ "stream_options": { "include_usage": true }, "max_tokens": 10 }));
    let b = build_upstream_body(&p, &req, "real", None);
    assert!(b.get("stream_options").is_none(), "{b}");
    assert_eq!(b["max_tokens"], 10);
}

/// F0-19: `ascii_json` escapa todo não-ASCII como `\uXXXX` (par substituto fora
/// do BMP); o JSON continua o mesmo valor.
#[test]
fn encode_body_ascii_json_escapes_non_ascii() {
    let v = json!({ "messages": [{ "content": "Você é revisor ção 😀 \"x\"\n" }], "n": 1 });
    let ascii = encode_body(&v, true);
    assert!(ascii.is_ascii(), "{}", String::from_utf8_lossy(&ascii));
    let s = String::from_utf8(ascii.clone()).unwrap();
    assert!(s.contains("Voc\\u00ea"), "{s}");
    assert!(s.contains("\\ud83d\\ude00"), "{s}");
    assert_eq!(serde_json::from_slice::<Value>(&ascii).unwrap(), v);

    let raw = encode_body(&v, false);
    assert!(!raw.is_ascii());
    assert_eq!(serde_json::from_slice::<Value>(&raw).unwrap(), v);
}

/// F0-19: com o quirk, o upstream recebe só bytes ASCII e `content-type: application/json`.
#[tokio::test]
async fn start_stream_sends_ascii_body_with_quirk() {
    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    let server = MockServer::start().await;
    let sse = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(header("content-type", "application/json"))
        .and(|r: &Request| r.body.is_ascii())
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let mut p = provider_from("quirks = { ascii_json = true }");
    p.base_url = server.uri();
    let mut req = request(json!({}));
    req.messages[0].content = Some(crate::translate::openai::MessageContent::Text(
        "Você é revisor".into(),
    ));
    let client = ProviderClient::new(build_http_client(Duration::from_secs(5)).unwrap());
    let a = client
        .start_stream(
            &p,
            None,
            "real",
            None,
            &req,
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .await;
    assert!(a.result.is_ok(), "{:?}", a.result.err());
    server.verify().await;
}

/// A5: com `rustls-no-provider`, construir o client sem provider instalado dá
/// panic em runtime. `build_http_client` instala o `ring` antes.
#[test]
fn http_client_builds_with_ring_provider() {
    build_http_client(Duration::from_secs(5)).expect("reqwest::Client com rustls+ring");
}

// ---------------------------------------------------------------- N4: raciocínio

/// Stream de bytes com atrasos virtuais (tokio com tempo pausado).
fn timed(events: Vec<(u64, String)>) -> SseEvents {
    let s = futures::stream::unfold(events.into_iter(), |mut it| async move {
        let (delay_ms, data) = it.next()?;
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        Some((Ok::<_, String>(Bytes::from(data)), it))
    });
    SseEvents::from_bytes(Box::pin(s))
}

fn ev(delta: Value) -> String {
    format!(
        "data: {}\n\n",
        json!({ "choices": [{ "index": 0, "delta": delta, "finish_reason": null }] })
    )
}

const FT: Duration = Duration::from_secs(1);
const FTR: Duration = Duration::from_secs(30);

#[tokio::test(start_paused = true)]
async fn reasoning_extends_first_token_deadline_without_committing() {
    let mut e = timed(vec![
        (
            200,
            ev(json!({ "role": "assistant", "reasoning_content": "pensando" })),
        ),
        (5000, ev(json!({ "reasoning": "mais" }))),
        (5000, ev(json!({ "content": "ok" }))),
    ]);
    let t0 = tokio::time::Instant::now();
    let (prelude, finished) = await_first_token(&mut e, t0, FT, FTR).await.unwrap();
    assert!(!finished);
    assert_eq!(prelude.len(), 3);
    assert!(is_first_token(prelude.last().unwrap()));
    assert!(has_reasoning(&prelude[0]) && has_reasoning(&prelude[1]));
    assert!(!is_first_token(&prelude[0]), "raciocínio não é commit");
}

#[tokio::test(start_paused = true)]
async fn no_reasoning_keeps_short_deadline() {
    let mut e = timed(vec![(5000, ev(json!({ "content": "tarde" })))]);
    let t0 = tokio::time::Instant::now();
    let r = await_first_token(&mut e, t0, FT, FTR).await;
    assert_eq!(r.unwrap_err(), UpstreamError::FirstTokenTimeout);
    assert!(
        t0.elapsed() < Duration::from_millis(1100),
        "{:?}",
        t0.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn reasoning_deadline_is_capped() {
    let mut e = timed(vec![
        (200, ev(json!({ "reasoning_content": "hmm" }))),
        (60_000, ev(json!({ "content": "tarde demais" }))),
    ]);
    let t0 = tokio::time::Instant::now();
    let r = await_first_token(&mut e, t0, FT, FTR).await;
    assert_eq!(r.unwrap_err(), UpstreamError::FirstTokenTimeout);
    let el = t0.elapsed();
    assert!(el >= FTR && el < FTR + Duration::from_millis(100), "{el:?}");
}

#[tokio::test(start_paused = true)]
async fn reasoning_then_eof_is_pre_first_token_failure() {
    let mut e = timed(vec![(100, ev(json!({ "reasoning_content": "hmm" })))]);
    let r = await_first_token(&mut e, tokio::time::Instant::now(), FT, FTR).await;
    assert!(matches!(r, Err(UpstreamError::Protocol(_))), "{r:?}");
}

#[test]
fn strip_reasoning_removes_fields_and_reports_chars() {
    let mut c = chunk(json!({ "content": "a", "reasoning_content": "xyz" }), None);
    assert_eq!(strip_reasoning(&mut c), 3);
    assert!(!has_reasoning(&c));
    assert_eq!(c.choices[0].delta.content.as_deref(), Some("a"));
    assert!(!is_empty_chunk(&c));

    let mut only = chunk(json!({ "reasoning": "pensando" }), None);
    assert_eq!(strip_reasoning(&mut only), 8);
    assert!(
        is_empty_chunk(&only),
        "chunk só de raciocínio não vai ao cliente"
    );

    let role = chunk(json!({ "role": "assistant" }), None);
    assert!(!is_empty_chunk(&role));
    assert!(!is_empty_chunk(&chunk(json!({}), Some("stop"))));
}

// ---------------------------------------------------------------- quirks novos

#[test]
fn upstream_body_merges_disable_thinking() {
    let p = provider_from("quirks = { disable_thinking = { thinking = { type = \"disabled\" } } }");
    let b = build_upstream_body(&p, &request(json!({})), "real", None);
    assert_eq!(b["thinking"]["type"], "disabled");
}

#[test]
fn remaining_from_headers_uses_configured_names() {
    let p = provider_from(
        "quirks = { ratelimit_headers = { rpd = \"x-ratelimit-remaining-requests-day\", rpm = \"x-ratelimit-remaining-requests\" } }",
    );
    let mut h = HeaderMap::new();
    h.insert(
        "x-ratelimit-remaining-requests-day",
        HeaderValue::from_static("17"),
    );
    h.insert(
        "x-ratelimit-remaining-requests",
        HeaderValue::from_static("lixo"),
    );
    let r = remaining_from_headers(&p.quirks.ratelimit_headers, &h);
    assert_eq!(r.rpd, Some(17));
    assert_eq!(r.rpm, None);
    assert_eq!(r.tpd, None);
}

/// F0-21: formato IETF (`RateLimit`/`RateLimit-Policy`, structured fields).
#[test]
fn ietf_ratelimit_headers_parse_into_window() {
    use crate::scheduler::WindowObs;

    let p = provider_from("quirks = { ratelimit_headers = { ietf = true } }");
    let hm = |pairs: &[(&'static str, &'static str)]| {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_static(v));
        }
        h
    };
    let r = remaining_from_headers(
        &p.quirks.ratelimit_headers,
        &hm(&[
            ("ratelimit", "\"default\";r=1199;t=1"),
            ("ratelimit-policy", "\"default\";q=1200;w=300"),
        ]),
    );
    assert_eq!(
        r.window,
        Some(WindowObs {
            quota: Some(1200),
            window_s: Some(300),
            remaining: 1199,
            reset_s: 1
        })
    );

    // Várias políticas: vale a mais apertada (menor `r`), casada pelo nome.
    let r = remaining_from_headers(
        &p.quirks.ratelimit_headers,
        &hm(&[
            ("ratelimit", "\"burst\";r=50;t=10, \"day\";r=3;t=4000"),
            (
                "ratelimit-policy",
                "\"burst\";q=100;w=60, \"day\";q=10;w=86400",
            ),
        ]),
    );
    let w = r.window.unwrap();
    assert_eq!(
        (w.quota, w.window_s, w.remaining, w.reset_s),
        (Some(10), Some(86_400), 3, 4000)
    );

    // Sem policy: ainda serve o restante e o reset.
    let r = remaining_from_headers(
        &p.quirks.ratelimit_headers,
        &hm(&[("ratelimit", "\"default\";r=0;t=30")]),
    );
    assert_eq!(
        r.window,
        Some(WindowObs {
            quota: None,
            window_s: None,
            remaining: 0,
            reset_s: 30
        })
    );

    // Lixo ou sem `r` → nada.
    for bad in ["lixo", "\"default\";t=3", "\"default\";r=abc;t=1"] {
        let h = hm(&[("ratelimit", bad)]);
        assert_eq!(
            remaining_from_headers(&p.quirks.ratelimit_headers, &h).window,
            None,
            "{bad}"
        );
    }

    // Sem o quirk, os headers são ignorados.
    let off = provider_from("");
    let h = hm(&[("ratelimit", "\"default\";r=1;t=1")]);
    assert_eq!(
        remaining_from_headers(&off.quirks.ratelimit_headers, &h).window,
        None
    );
}

#[test]
fn ratelimit_headers_for_debug_only_expose_limit_names() {
    let mut h = HeaderMap::new();
    h.insert("x-ratelimit-limit-requests", HeaderValue::from_static("20"));
    h.insert("retry-after", HeaderValue::from_static("3"));
    h.insert("authorization", HeaderValue::from_static("Bearer segredo"));
    h.insert("set-cookie", HeaderValue::from_static("a=b"));
    let v = limit_headers(&h);
    assert_eq!(
        v,
        vec![
            ("retry-after".to_owned(), "3".to_owned()),
            ("x-ratelimit-limit-requests".to_owned(), "20".to_owned())
        ]
    );
}
