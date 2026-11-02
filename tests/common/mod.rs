//! Utilitários dos testes de integração: sobe o gateway numa porta efêmera,
//! monta corpos SSE de upstream e decodifica o SSE devolvido ao cliente.

#![allow(dead_code)]

use std::time::Duration;

use openkax::config::Config;
use openkax::provider::Secrets;
use openkax::server::{self, AppState, Deps};
use serde_json::{Value, json};
use wiremock::ResponseTemplate;

/// Gateway rodando em background.
pub struct Gateway {
    pub base: String,
    pub http: reqwest::Client,
}

impl Gateway {
    pub async fn spawn(toml: &str, secrets: &[(&str, &str)]) -> Self {
        let config = Config::from_toml_str(toml).expect("config de teste inválida");
        let deps = Deps::new(config, Secrets::from_pairs(secrets.iter().copied()));
        let state = AppState::new(deps).expect("AppState");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(server::serve(listener, state, std::future::pending()));
        server::install_crypto_provider();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        Self { base, http }
    }

    /// `POST /v1/chat/completions` com streaming; devolve status, provider e SSE lido até o fim.
    pub async fn chat_stream(&self, model: &str, user_content: &str) -> Reply {
        let body = json!({
            "model": model,
            "stream": true,
            "messages": [{ "role": "user", "content": user_content }],
        });
        let resp = self
            .http
            .post(format!("{}/v1/chat/completions", self.base))
            .json(&body)
            .send()
            .await
            .expect("gateway não respondeu");
        let status = resp.status().as_u16();
        let header = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let provider = header(server::PROVIDER_HEADER);
        let retry_after = header("retry-after");
        let content_type = header("content-type");
        let text = resp.text().await.expect("corpo do gateway");
        Reply {
            status,
            provider,
            retry_after,
            content_type,
            sse: Sse::parse(&text),
        }
    }
}

#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub provider: Option<String>,
    pub retry_after: Option<String>,
    pub content_type: Option<String>,
    pub sse: Sse,
}

/// SSE decodificado (só eventos `data:`; comentários ignorados).
#[derive(Debug, Default)]
pub struct Sse {
    pub chunks: Vec<Value>,
    pub done: bool,
    /// Algo veio depois de `[DONE]`.
    pub trailing_after_done: bool,
}

impl Sse {
    pub fn parse(text: &str) -> Self {
        let mut out = Self::default();
        for event in text.split("\n\n") {
            for line in event.lines() {
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if out.done {
                    out.trailing_after_done = true;
                }
                if data == "[DONE]" {
                    out.done = true;
                } else {
                    out.chunks
                        .push(serde_json::from_str(data).unwrap_or_else(|e| {
                            panic!("chunk SSE do gateway não é JSON ({e}): {data}")
                        }));
                }
            }
        }
        out
    }

    /// Concatenação de `choices[0].delta.content`.
    pub fn content(&self) -> String {
        self.chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect()
    }

    pub fn finish_reasons(&self) -> Vec<String> {
        self.chunks
            .iter()
            .filter_map(|c| c["choices"][0]["finish_reason"].as_str().map(str::to_owned))
            .collect()
    }
}

// ----------------------------------------------------------- upstream fake

/// Um evento SSE de chunk OpenAI.
pub fn chunk(delta: Value, finish_reason: Option<&str>) -> String {
    let c = json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "upstream-model",
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }],
    });
    format!("data: {c}\n\n")
}

/// Stream upstream completo e bem-formado que responde `text`.
pub fn sse_ok(text: &str) -> ResponseTemplate {
    let body = [
        chunk(json!({ "role": "assistant" }), None),
        chunk(json!({ "content": text }), None),
        chunk(json!({}), Some("stop")),
        "data: [DONE]\n\n".to_owned(),
    ]
    .concat();
    sse_raw(body)
}

pub fn sse_raw(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

pub fn rate_limited(retry_after_s: u64) -> ResponseTemplate {
    ResponseTemplate::new(429)
        .insert_header("retry-after", retry_after_s.to_string().as_str())
        .set_body_json(json!({ "error": { "message": "rate limited", "type": "rate_limit" } }))
}

/// Config com dois providers apontando para mocks: `groq` (TPM 8k) → `gemini`.
pub fn two_provider_config(groq_url: &str, gemini_url: &str) -> String {
    format!(
        r#"
        [defaults]
        request_timeout_ms = 5000
        first_token_timeout_ms = 300
        estimate_chars_per_token = 3.6

        [[provider]]
        id = "groq"
        base_url = "{groq_url}"
        auth = {{ kind = "bearer", env = "GROQ_API_KEY" }}
        models = [{{ id = "qwen-coder", upstream = "qwen-coder", max_ctx = 128000, limits = {{ rpm = 30, tpm = 8000 }} }}]
        cooldown = {{ on_429_s = 60 }}

        [[provider]]
        id = "gemini"
        base_url = "{gemini_url}"
        auth = {{ kind = "bearer", env = "GEMINI_API_KEY" }}
        models = [{{ id = "gemini-flash", upstream = "gemini-flash", max_ctx = 1000000, limits = {{ rpm = 30, tpm = 250000 }} }}]

        [[class]]
        id = "kax/coder-fast"
        max_packet_tokens = 60000
        route = [
          {{ provider = "groq", model = "qwen-coder" }},
          {{ provider = "gemini", model = "gemini-flash" }},
        ]
        "#
    )
}

pub const SECRETS: &[(&str, &str)] = &[("GROQ_API_KEY", "k-groq"), ("GEMINI_API_KEY", "k-gemini")];
