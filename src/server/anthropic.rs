//! `POST /v1/messages` para classes `kax/*` (F1-4, ADR 0006).
//!
//! Traduz a request na entrada, entrega ao [`dispatch`] de sempre e passa os chunks
//! já normalizados pelo [`Relay`] para o tradutor de eventos na saída. Scheduler,
//! fallback pré-1º-token e ledger não sabem que protocolo o cliente fala.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::extract::rejection::BytesRejection;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use super::{AppState, INTERRUPTED, Meta, PROVIDER_HEADER, Relay, Wire, dispatch};
use crate::ledger::Outcome;
use crate::provider::UpstreamError;
use crate::scheduler::estimate_tokens;
use crate::translate::anthropic::{
    MessagesRequest, OpenAiToAnthropicStream, StreamEvent, StreamMeta, collect_response,
    encode_sse, error_body, to_openai_request,
};
use crate::translate::openai::ChatCompletionRequest;

/// `type` de erro da Anthropic para um status HTTP (ADR 0006, "Erros HTTP").
fn error_kind(status: StatusCode) -> &'static str {
    match status.as_u16() {
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        503 => "overloaded_error",
        500.. => "api_error",
        _ => "invalid_request_error",
    }
}

pub(super) fn error_response(status: StatusCode, message: &str) -> Response {
    let body = error_body(error_kind(status), message);
    (status, axum::Json(body)).into_response()
}

/// Mensagem legível de um erro do upstream: `error.message` do OpenAI, se o corpo
/// tiver essa forma. Senão, o corpo como veio (pode estar truncado).
pub(super) fn upstream_message(body: &str) -> String {
    let parsed = serde_json::from_str::<Value>(body).ok();
    let message = parsed.as_ref().and_then(|v| {
        v["error"]["message"]
            .as_str()
            .or(v["error"].as_str())
            .or(v["message"].as_str())
    });
    message.unwrap_or(body).to_owned()
}

#[derive(serde::Deserialize)]
struct ModelOnly {
    model: String,
}

fn is_hop_by_hop(name: &axum::http::HeaderName) -> bool {
    let s = name.as_str();
    s.eq_ignore_ascii_case("connection")
        || s.eq_ignore_ascii_case("keep-alive")
        || s.eq_ignore_ascii_case("proxy-authenticate")
        || s.eq_ignore_ascii_case("proxy-authorization")
        || s.eq_ignore_ascii_case("te")
        || s.eq_ignore_ascii_case("trailer")
        || s.eq_ignore_ascii_case("transfer-encoding")
        || s.eq_ignore_ascii_case("upgrade")
        || s.eq_ignore_ascii_case("host")
        || s.eq_ignore_ascii_case("content-length")
}

async fn passthrough(
    app: AppState,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
    model: &str,
) -> Response {
    let started = Instant::now();
    let st = app.view();
    let provider = st
        .config()
        .providers
        .iter()
        .find(|p| p.native == Some(crate::config::Native::Anthropic));

    let Some(p) = provider else {
        return error_response(
            StatusCode::NOT_FOUND,
            "nenhum provider nativo Anthropic configurado",
        );
    };

    let base = p.base_url.trim_end_matches('/');
    let path = uri.path();
    let url = match uri.query() {
        Some(q) if !q.is_empty() => format!("{base}{path}?{q}"),
        _ => format!("{base}{path}"),
    };

    let client = st.http_client();
    let mut req_builder = client.post(&url);

    for (name, value) in &headers {
        if !is_hop_by_hop(name) {
            req_builder = req_builder.header(name.clone(), value.clone());
        }
    }

    req_builder = req_builder.body(body);

    let res = req_builder.send().await;

    match res {
        Ok(upstream_resp) => {
            let status = upstream_resp.status();
            let latency_ms = started.elapsed().as_millis() as u64;

            st.ledger().record(crate::ledger::LedgerEntry::Request(
                crate::ledger::RequestRecord {
                    ts: crate::ledger::rfc3339(SystemTime::now()),
                    route: model.to_string(),
                    provider: Some(p.id.clone()),
                    model: Some(model.to_string()),
                    est_in: 0,
                    input_tokens: None,
                    output_tokens: None,
                    latency_ms,
                    ttft_ms: None,
                    fallbacks: 0,
                    tier: None,
                    reasoning: false,
                    pinned: false,
                    outcome: if status.is_success() {
                        crate::ledger::Outcome::Ok
                    } else {
                        crate::ledger::Outcome::RequestFault
                    },
                    status: Some(status.as_u16()),
                },
            ));

            let mut resp_headers = HeaderMap::new();
            for (name, val) in upstream_resp.headers() {
                if !is_hop_by_hop(name) {
                    resp_headers.insert(name.clone(), val.clone());
                }
            }
            if let Ok(pv) = HeaderValue::from_str(&p.id) {
                resp_headers.insert(axum::http::HeaderName::from_static(PROVIDER_HEADER), pv);
            }

            let mut response = Response::new(Body::from_stream(upstream_resp.bytes_stream()));
            *response.status_mut() =
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK);
            *response.headers_mut() = resp_headers;
            response
        }
        Err(e) => {
            let latency_ms = started.elapsed().as_millis() as u64;
            st.ledger().record(crate::ledger::LedgerEntry::Request(
                crate::ledger::RequestRecord {
                    ts: crate::ledger::rfc3339(SystemTime::now()),
                    route: model.to_string(),
                    provider: Some(p.id.clone()),
                    model: Some(model.to_string()),
                    est_in: 0,
                    input_tokens: None,
                    output_tokens: None,
                    latency_ms,
                    ttft_ms: None,
                    fallbacks: 0,
                    tier: None,
                    reasoning: false,
                    pinned: false,
                    outcome: crate::ledger::Outcome::RequestFault,
                    status: Some(StatusCode::BAD_GATEWAY.as_u16()),
                },
            ));
            error_response(
                StatusCode::BAD_GATEWAY,
                &format!("upstream inacessível: {e}"),
            )
        }
    }
}

pub(super) async fn count_tokens(
    State(app): State<AppState>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match body {
        Ok(b) => b,
        Err(e) => return error_response(e.status(), &e.body_text()),
    };
    let model_only: ModelOnly = match serde_json::from_slice(&body) {
        Ok(m) => m,
        Err(e) => {
            return error_response(StatusCode::BAD_REQUEST, &format!("corpo inválido: {e}"));
        }
    };
    if !model_only.model.starts_with("kax/") {
        return passthrough(app, uri, headers, body, &model_only.model).await;
    }
    let req: MessagesRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return error_response(StatusCode::BAD_REQUEST, &format!("corpo inválido: {e}"));
        }
    };
    let st = app.view();
    let Some(cpt) = st.scheduler().class_chars_per_token(&req.model) else {
        return error_response(
            StatusCode::NOT_FOUND,
            &format!("classe desconhecida: {}", req.model),
        );
    };
    let translated = to_openai_request(&req);
    let count = estimate_tokens(&translated, cpt);
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "input_tokens": count })),
    )
        .into_response()
}

pub(super) async fn messages(
    State(app): State<AppState>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match body {
        Ok(b) => b,
        Err(e) => return error_response(e.status(), &e.body_text()),
    };
    let model_only: ModelOnly = match serde_json::from_slice(&body) {
        Ok(m) => m,
        Err(e) => {
            return error_response(StatusCode::BAD_REQUEST, &format!("corpo inválido: {e}"));
        }
    };
    if !model_only.model.starts_with("kax/") {
        return passthrough(app, uri, headers, body, &model_only.model).await;
    }
    let st = app.view();
    let started = Instant::now();
    let req: MessagesRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return error_response(StatusCode::BAD_REQUEST, &format!("corpo inválido: {e}"));
        }
    };
    let meta = Meta {
        route: req.model.clone(),
        est_in: 0,
        started,
        fallbacks: 0,
        pinned: false,
        reasoning: false,
    };
    let translated = to_openai_request(&req);
    if let Some(problem) = invalid(&req, &translated) {
        meta.record(&st, None, None, None, Outcome::Rejected);
        return error_response(StatusCode::BAD_REQUEST, problem);
    }
    dispatch(&st, &headers, &translated, Wire::Anthropic, meta).await
}

/// O que recusar sem gastar cota. `count_tokens` não passa por aqui: lá `max_tokens`
/// não vem.
fn invalid(req: &MessagesRequest, translated: &ChatCompletionRequest) -> Option<&'static str> {
    if req.max_tokens.is_none_or(|m| m == 0) {
        return Some("max_tokens é obrigatório e precisa ser maior que zero");
    }
    // Blocos que a tradução descarta (`document`) podem não deixar nada para enviar.
    if translated.messages.iter().all(|m| m.role == "system") {
        return Some(
            "nenhuma mensagem que estas classes saibam enviar (messages vazio ou só blocos sem suporte)",
        );
    }
    None
}

/// `msg_…` único no processo: não precisa de mais que isso, e o gateway não guarda
/// conversa.
fn message_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("msg_{nanos:x}{n:x}")
}

/// [`Relay`] + tradutor: a mesma sequência de eventos serve aos dois modos.
struct EventPump {
    relay: Relay,
    translator: OpenAiToAnthropicStream,
    done: bool,
}

impl EventPump {
    /// Eventos do próximo chunk; `None` depois do último. Fecha o relay no fim.
    async fn next_events(&mut self) -> Option<Vec<StreamEvent>> {
        if self.done {
            return None;
        }
        Some(match self.relay.next_chunk().await {
            Ok(Some(chunk)) => {
                let events = self.translator.push(&chunk);
                // `finish_reason: "error"`: para o cliente é morte no meio, então
                // ledger e cooldown também contam assim.
                if events
                    .iter()
                    .any(|e| matches!(e, StreamEvent::Error { .. }))
                {
                    self.done = true;
                    self.relay
                        .finish_err(&UpstreamError::MidStream("finish_reason: error".to_owned()));
                }
                events
            }
            Ok(None) => {
                self.done = true;
                self.relay.finish_ok();
                self.translator.finish(self.relay.usage)
            }
            Err(e) => {
                self.done = true;
                self.relay.finish_err(&e);
                self.translator.fail("api_error", INTERRUPTED)
            }
        })
    }

    /// Próximos bytes SSE, pulando chunk que não gerou evento.
    async fn next_bytes(&mut self) -> Option<Bytes> {
        loop {
            let events = self.next_events().await?;
            if !events.is_empty() {
                let framed: Vec<Bytes> = events.iter().map(encode_sse).collect();
                return Some(framed.concat().into());
            }
        }
    }
}

/// Monta a resposta do candidato que passou do 1º token.
pub(super) async fn respond(
    relay: Relay,
    req: &ChatCompletionRequest,
    is_stream: bool,
) -> Response {
    let provider = HeaderValue::from_str(&relay.cand.provider_id).ok();
    let chars_per_token = relay.cand.chars_per_token;
    let mut pump = EventPump {
        relay,
        translator: OpenAiToAnthropicStream::new(StreamMeta {
            message_id: message_id(),
            model: req.model.clone(),
            est_input_tokens: estimate_tokens(req, chars_per_token),
            chars_per_token,
        }),
        done: false,
    };
    if is_stream {
        let body = futures::stream::unfold(pump, |mut p| async move {
            p.next_bytes()
                .await
                .map(|b| (Ok::<_, std::convert::Infallible>(b), p))
        });
        return super::sse_response(Body::from_stream(body), provider);
    }
    let mut events = Vec::new();
    while let Some(more) = pump.next_events().await {
        events.extend(more);
    }
    match collect_response(&events) {
        Ok(message) => {
            let mut resp = axum::Json(message).into_response();
            if let Some(p) = provider {
                resp.headers_mut().insert(PROVIDER_HEADER, p);
            }
            resp
        }
        Err(e) => error_response(StatusCode::BAD_GATEWAY, &e.message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_kind_table() {
        for (status, kind) in [
            (400, "invalid_request_error"),
            (404, "not_found_error"),
            (409, "invalid_request_error"),
            (413, "request_too_large"),
            (422, "invalid_request_error"),
            (429, "rate_limit_error"),
            (500, "api_error"),
            (502, "api_error"),
            (503, "overloaded_error"),
        ] {
            assert_eq!(error_kind(StatusCode::from_u16(status).unwrap()), kind);
        }
    }

    #[test]
    fn upstream_message_reads_the_common_shapes() {
        assert_eq!(upstream_message(r#"{"error":{"message":"m1"}}"#), "m1");
        assert_eq!(upstream_message(r#"{"error":"m2"}"#), "m2");
        assert_eq!(upstream_message(r#"{"message":"m3"}"#), "m3");
        assert_eq!(upstream_message("{truncado"), "{truncado");
        assert_eq!(upstream_message(r#"{"outro":1}"#), r#"{"outro":1}"#);
    }

    #[test]
    fn message_ids_differ_and_keep_the_prefix() {
        let (a, b) = (message_id(), message_id());
        assert_ne!(a, b);
        assert!(a.starts_with("msg_"));
    }
}
