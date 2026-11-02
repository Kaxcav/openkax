//! Testes de aceite de fallback da Fase 0 (PLANO_MESTRE §4, CLAUDE.md).
//!
//! Escritos **antes** do scheduler/dispatcher: no CONTRACT todos falham.
//! Cada provider é um `wiremock::MockServer`; `.expect(n)` prova quantas
//! requests chegaram (ou não) em cada upstream.

mod common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Mock que só casa com a request certa: rota, auth e modelo reescrito.
fn upstream(key: &str, model: &str) -> wiremock::MockBuilder {
    // `stream: true` sempre: o gateway fala streaming com o upstream mesmo
    // quando o cliente não pede.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", format!("Bearer {key}").as_str()))
        .and(body_partial_json(json!({ "model": model, "stream": true })))
}

async fn mocks() -> (MockServer, MockServer) {
    (MockServer::start().await, MockServer::start().await)
}

/// Critério 1: 429 → próximo candidato; e o provider fica em cooldown, então
/// uma 2ª request nem chega nele.
#[tokio::test]
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
    let gw = Gateway::spawn(&two_provider_config(&groq.uri(), &gemini.uri()), SECRETS).await;

    for attempt in 1..=2 {
        let r = gw.chat_stream("kax/coder-fast", "oi").await;
        assert_eq!(r.status, 200, "request {attempt}: {r:?}");
        assert_eq!(r.provider.as_deref(), Some("gemini"), "request {attempt}");
        assert!(
            r.content_type
                .as_deref()
                .unwrap_or("")
                .starts_with("text/event-stream")
        );
        assert_eq!(r.sse.content(), "from-gemini", "request {attempt}");
        assert!(
            r.sse.done && !r.sse.trailing_after_done,
            "request {attempt}: {:?}",
            r.sse
        );
    }

    groq.verify().await;
    gemini.verify().await;
}

/// Critério 2: upstream que não entrega o 1º token dentro de
/// `first_token_timeout_ms` (300 ms aqui) → próximo candidato, sem esperar
/// o upstream lento.
#[tokio::test]
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
    let gw = Gateway::spawn(&two_provider_config(&groq.uri(), &gemini.uri()), SECRETS).await;

    let t0 = Instant::now();
    let r = gw.chat_stream("kax/coder-fast", "oi").await;
    let elapsed = t0.elapsed();

    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.provider.as_deref(), Some("gemini"));
    assert_eq!(r.sse.content(), "from-gemini");
    assert!(r.sse.done);
    assert!(
        elapsed < Duration::from_secs(2),
        "esperou o upstream lento: {elapsed:?}"
    );

    gemini.verify().await;
}

/// Critério 3: stream morre depois do 1º token (EOF sem `finish_reason` nem
/// `[DONE]`) → sem retry; cliente recebe o que já veio, um chunk final com
/// `finish_reason: "error"` e `[DONE]`. O 2º provider não é chamado.
#[tokio::test]
async fn mid_stream_error_ends_cleanly_without_fallback() {
    let (groq, gemini) = mocks().await;
    let truncated = [
        chunk(json!({ "role": "assistant" }), None),
        chunk(json!({ "content": "Hel" }), None),
    ]
    .concat();
    upstream("k-groq", "qwen-coder")
        .respond_with(sse_raw(truncated))
        .expect(1)
        .mount(&groq)
        .await;
    upstream("k-gemini", "gemini-flash")
        .respond_with(sse_ok("from-gemini"))
        .expect(0)
        .mount(&gemini)
        .await;
    let gw = Gateway::spawn(&two_provider_config(&groq.uri(), &gemini.uri()), SECRETS).await;

    let r = gw.chat_stream("kax/coder-fast", "oi").await;

    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.provider.as_deref(), Some("groq"));
    assert_eq!(r.sse.content(), "Hel");
    assert_eq!(
        r.sse.finish_reasons().last().map(String::as_str),
        Some("error"),
        "{:?}",
        r.sse
    );
    assert!(r.sse.done && !r.sse.trailing_after_done, "{:?}", r.sse);

    groq.verify().await;
    gemini.verify().await;
}

/// Critério 4: estimativa de tokens maior que o TPM do provider → provider
/// pulado **sem** request; o próximo atende.
#[tokio::test]
async fn tpm_insufficient_skips_provider_without_request() {
    let (groq, gemini) = mocks().await;
    upstream("k-groq", "qwen-coder")
        .respond_with(sse_ok("from-groq"))
        .expect(0)
        .mount(&groq)
        .await;
    upstream("k-gemini", "gemini-flash")
        .respond_with(sse_ok("from-gemini"))
        .expect(1)
        .mount(&gemini)
        .await;
    let gw = Gateway::spawn(&two_provider_config(&groq.uri(), &gemini.uri()), SECRETS).await;

    // 40k chars / 3.6 ≈ 11.1k tokens > TPM 8k do groq; < 60k da classe.
    let big = "x".repeat(40_000);
    let r = gw.chat_stream("kax/coder-fast", &big).await;

    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.provider.as_deref(), Some("gemini"));
    assert_eq!(r.sse.content(), "from-gemini");

    groq.verify().await;
    gemini.verify().await;
}

/// Complemento do critério 2 (definição de 1º token): chunk só com `role`,
/// comentário SSE e `content` vazio não contam; EOF depois disso ainda é
/// "antes do 1º token" → fallback.
#[tokio::test]
async fn role_only_and_keepalive_do_not_count_as_first_token() {
    let (groq, gemini) = mocks().await;
    let no_token = [
        chunk(json!({ "role": "assistant" }), None),
        ": keepalive\n\n".to_owned(),
        chunk(json!({ "content": "" }), None),
    ]
    .concat();
    upstream("k-groq", "qwen-coder")
        .respond_with(sse_raw(no_token))
        .expect(1)
        .mount(&groq)
        .await;
    upstream("k-gemini", "gemini-flash")
        .respond_with(sse_ok("from-gemini"))
        .expect(1)
        .mount(&gemini)
        .await;
    let gw = Gateway::spawn(&two_provider_config(&groq.uri(), &gemini.uri()), SECRETS).await;

    let r = gw.chat_stream("kax/coder-fast", "oi").await;

    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.provider.as_deref(), Some("gemini"));
    assert_eq!(r.sse.content(), "from-gemini");

    groq.verify().await;
    gemini.verify().await;
}

/// Todos esgotados → 429 com `Retry-After` = menor tempo até algum candidato
/// voltar. Na 2ª request ambos estão em cooldown: 429 sem tocar upstream.
#[tokio::test]
async fn all_exhausted_returns_429_with_min_retry_after() {
    let (groq, gemini) = mocks().await;
    upstream("k-groq", "qwen-coder")
        .respond_with(rate_limited(30))
        .expect(1)
        .mount(&groq)
        .await;
    upstream("k-gemini", "gemini-flash")
        .respond_with(rate_limited(5))
        .expect(1)
        .mount(&gemini)
        .await;
    let gw = Gateway::spawn(&two_provider_config(&groq.uri(), &gemini.uri()), SECRETS).await;

    for attempt in 1..=2 {
        let r = gw.chat_stream("kax/coder-fast", "oi").await;
        assert_eq!(r.status, 429, "request {attempt}: {r:?}");
        let secs: u64 = r
            .retry_after
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("request {attempt}: Retry-After ausente/inválido: {r:?}"));
        assert!(
            (1..=5).contains(&secs),
            "request {attempt}: Retry-After={secs}, esperado ≤ 5"
        );
    }

    groq.verify().await;
    gemini.verify().await;
}

/// Sanidade do harness: o gateway sobe e `/health` responde mesmo no CONTRACT.
#[tokio::test]
async fn harness_health() {
    let gw = Gateway::spawn("", &[]).await;
    let r = gw
        .http
        .get(format!("{}/health", gw.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

/// A3 (ADR 0003): erro de culpa da request → no máximo 1 candidato extra;
/// dois seguidos → o erro do upstream volta ao cliente e o 3º degrau não é
/// chamado (não queima RPM da rota inteira).
#[tokio::test]
async fn request_fault_tries_one_extra_candidate_then_returns_to_client() {
    let (a, b) = mocks().await;
    let c = MockServer::start().await;
    let bad = ResponseTemplate::new(400).set_body_json(
        json!({ "error": { "message": "unsupported param", "type": "invalid_request_error" } }),
    );
    upstream("k", "ma")
        .respond_with(bad.clone())
        .expect(1)
        .mount(&a)
        .await;
    upstream("k", "mb")
        .respond_with(bad)
        .expect(1)
        .mount(&b)
        .await;
    upstream("k", "mc")
        .respond_with(sse_ok("from-c"))
        .expect(0)
        .mount(&c)
        .await;
    let mut toml =
        String::from("[defaults]\nfirst_token_timeout_ms = 300\nrequest_timeout_ms = 5000\n");
    for (id, uri) in [("a", a.uri()), ("b", b.uri()), ("c", c.uri())] {
        toml.push_str(&format!(
            "[[provider]]\nid = \"{id}\"\nbase_url = \"{uri}\"\nauth = {{ kind = \"bearer\", env = \"K\" }}\nmodels = [{{ id = \"m{id}\", upstream = \"m{id}\" }}]\n"
        ));
    }
    toml.push_str(
        "[[class]]\nid = \"kax/coder-fast\"\nmax_packet_tokens = 60000\nroute = [{ provider = \"a\", model = \"ma\" }, { provider = \"b\", model = \"mb\" }, { provider = \"c\", model = \"mc\" }]\n",
    );
    let gw = Gateway::spawn(&toml, &[("K", "k")]).await;

    let resp = gw
        .http
        .post(format!("{}/v1/chat/completions", gw.base))
        .json(&json!({ "model": "kax/coder-fast", "stream": true, "messages": [{ "role": "user", "content": "oi" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["message"], "unsupported param");

    // contador visível em /quota
    let q: serde_json::Value = gw
        .http
        .get(format!("{}/quota", gw.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let faults = |p: usize| q["providers"][p]["models"][0]["request_faults"].as_u64();
    assert_eq!(
        (faults(0), faults(1), faults(2)),
        (Some(1), Some(1), Some(0)),
        "{q}"
    );

    a.verify().await;
    b.verify().await;
    c.verify().await;
}
