//! `openkax doctor` (F5-1): catálogo `/models` e chat mínimo contra mocks.
//! Nunca toca provider real: o `base_url` aponta para um `wiremock`.

mod common;

use common::*;
use openkax::config::Config;
use openkax::doctor::{self, Options, Status};
use openkax::provider::Secrets;
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config(base: &str, extra_provider: &str) -> Config {
    Config::from_toml_str(&format!(
        r#"
[[provider]]
id = "p1"
base_url = "{base}"
auth = {{ kind = "bearer", env = "P1_KEY" }}
models = [
  {{ id = "m1", upstream = "up-1" }},
  {{ id = "m2", upstream = "up-2" }},
]
{extra_provider}

[[class]]
id = "kax/coder-fast"
max_packet_tokens = 1000
route = [
  {{ provider = "p1", model = "m1" }},
  {{ provider = "p1", model = "m2" }},
]
"#
    ))
    .expect("config de teste")
}

fn secrets() -> Secrets {
    Secrets::from_pairs([("P1_KEY", "k-secreta")])
}

fn opts(chat: bool) -> Options {
    Options {
        chat,
        provider: None,
    }
}

async fn run(cfg: &Config, chat: bool) -> doctor::Report {
    doctor::run(cfg, &secrets(), &opts(chat))
        .await
        .expect("doctor")
}

fn catalog(ids: &[&str]) -> ResponseTemplate {
    let data: Vec<_> = ids.iter().map(|i| json!({ "id": i })).collect();
    ResponseTemplate::new(200).set_body_json(json!({ "object": "list", "data": data }))
}

#[tokio::test]
async fn catalog_json_ok_counts_models() {
    let srv = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("authorization", "Bearer k-secreta"))
        .respond_with(catalog(&["up-1", "up-2", "outro"]))
        .expect(1)
        .mount(&srv)
        .await;
    let report = run(&config(&srv.uri(), ""), false).await;
    let p = &report.providers[0];
    assert_eq!(p.status, Status::Ok, "{p:?}");
    assert_eq!(p.catalog_models, Some(3));
    assert!(!report.failed());
}

#[tokio::test]
async fn catalog_2xx_text_plain_is_fail() {
    let srv = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("OK", "text/plain"))
        .mount(&srv)
        .await;
    let report = run(&config(&srv.uri(), ""), false).await;
    assert_eq!(report.providers[0].status, Status::Fail);
    assert!(report.failed());
}

#[tokio::test]
async fn catalog_401_and_403_are_fail() {
    for code in [401, 403] {
        let srv = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(code).set_body_json(json!({"error": "no"})))
            .mount(&srv)
            .await;
        let report = run(&config(&srv.uri(), ""), false).await;
        assert_eq!(report.providers[0].status, Status::Fail, "HTTP {code}");
        assert!(report.failed());
    }
}

#[tokio::test]
async fn upstream_missing_from_catalog_is_warn() {
    let srv = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(catalog(&["up-1"]))
        .mount(&srv)
        .await;
    let report = run(&config(&srv.uri(), ""), false).await;
    let p = &report.providers[0];
    assert_eq!(p.status, Status::Warn, "{p:?}");
    assert!(
        p.checks
            .iter()
            .any(|c| c.status == Status::Warn && c.detail.contains("up-2")),
        "{p:?}"
    );
    assert!(!report.failed(), "WARN não derruba o exit code");
}

#[tokio::test]
async fn cloudflare_style_uses_models_search_endpoint() {
    let srv = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/client/v4/accounts/acc1/ai/models/search"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "result": [{ "name": "up-1" }, { "name": "up-2" }] })),
        )
        .expect(1)
        .mount(&srv)
        .await;
    let base = format!(
        "{}/client/v4/accounts/${{CLOUDFLARE_ACCOUNT_ID}}/ai/v1",
        srv.uri()
    );
    let cfg = config(&base, "");
    let s = Secrets::from_pairs([("P1_KEY", "k"), ("CLOUDFLARE_ACCOUNT_ID", "acc1")]);
    let report = doctor::run(&cfg, &s, &opts(false)).await.unwrap();
    assert_eq!(
        report.providers[0].status,
        Status::Ok,
        "{:?}",
        report.providers[0]
    );
    assert_eq!(report.providers[0].catalog_models, Some(2));
}

#[tokio::test]
async fn chat_reports_status_ttft_and_limit_headers() {
    let srv = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(catalog(&["up-1", "up-2"]))
        .mount(&srv)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(json!({ "max_tokens": 8 })))
        .respond_with(
            sse_ok("pong")
                .insert_header("x-ratelimit-remaining-requests", "59")
                .insert_header("x-ratelimit-remaining-tokens", "1000")
                .insert_header("ratelimit", "\"burst\";r=7;t=30")
                .insert_header("ratelimit-policy", "\"burst\";q=10;w=60"),
        )
        .expect(2)
        .mount(&srv)
        .await;
    let cfg = config(
        &srv.uri(),
        "quirks = { ratelimit_headers = { rpm = \"x-ratelimit-remaining-requests\", tpm = \"x-ratelimit-remaining-tokens\", ietf = true } }",
    );
    let report = run(&cfg, true).await;
    let p = &report.providers[0];
    assert_eq!(p.status, Status::Ok, "{p:?}");
    let chats: Vec<_> = p
        .checks
        .iter()
        .filter(|c| c.name.starts_with("chat:"))
        .collect();
    assert_eq!(chats.len(), 2, "um chat por modelo habilitado: {p:?}");
    let c = chats[0];
    assert!(c.ttft_ms.is_some(), "{c:?}");
    assert!(
        c.headers
            .iter()
            .any(|(k, v)| k == "x-ratelimit-remaining-requests" && v == "59")
    );
    assert!(c.headers.iter().any(|(k, _)| k == "ratelimit-policy"));
    assert!(
        c.detail.contains("rpm=59") && c.detail.contains("tpm=1000"),
        "{}",
        c.detail
    );
    assert!(c.detail.contains("janela r=7"), "{}", c.detail);
}

#[tokio::test]
async fn chat_failure_makes_provider_fail() {
    let srv = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(catalog(&["up-1", "up-2"]))
        .mount(&srv)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(rate_limited(30))
        .mount(&srv)
        .await;
    let report = run(&config(&srv.uri(), ""), true).await;
    assert_eq!(report.providers[0].status, Status::Fail);
    assert!(report.failed());
}

#[tokio::test]
async fn provider_filter_json_and_secret_never_printed() {
    let srv = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(catalog(&["up-1", "up-2"]))
        .mount(&srv)
        .await;
    let cfg = config(&srv.uri(), "");
    let o = Options {
        chat: false,
        provider: Some("p1".into()),
    };
    let report = doctor::run(&cfg, &secrets(), &o).await.unwrap();
    let json = report.to_json();
    let text = report.to_text();
    assert!(!json.contains("k-secreta") && !text.contains("k-secreta"));
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["providers"][0]["id"], "p1");
    assert_eq!(v["providers"][0]["status"], "ok");

    let o = Options {
        chat: false,
        provider: Some("nope".into()),
    };
    assert!(doctor::run(&cfg, &secrets(), &o).await.is_err());
}

#[tokio::test]
async fn missing_key_is_fail_without_request() {
    let srv = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(catalog(&[]))
        .expect(0)
        .mount(&srv)
        .await;
    let report = doctor::run(&config(&srv.uri(), ""), &Secrets::default(), &opts(false))
        .await
        .unwrap();
    assert_eq!(report.providers[0].status, Status::Fail);
    assert!(report.providers[0].checks[0].detail.contains("P1_KEY"));
}
