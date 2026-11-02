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

/// A5: com `rustls-no-provider`, construir o client sem provider instalado dá
/// panic em runtime. `build_http_client` instala o `ring` antes.
#[test]
fn http_client_builds_with_ring_provider() {
    build_http_client(Duration::from_secs(5)).expect("reqwest::Client com rustls+ring");
}
