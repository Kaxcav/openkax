//! Superfície HTTP (`axum`) e despacho com fallback pré-1º-token.
//!
//! Contrato observável (exercitado por `tests/fallback.rs`):
//! - `POST /v1/chat/completions` com `model = "kax/<classe>"` roteia pelo scheduler.
//! - Streaming: os headers só saem depois do 1º token do candidato escolhido;
//!   erros antes disso caem no próximo candidato sem o cliente perceber.
//! - Header `x-openkax-provider` identifica o provider que respondeu.
//! - Morte pós-1º-token: chunk final com `finish_reason: "error"` + `data: [DONE]`.
//! - Nenhum candidato disponível: `429` com `Retry-After` (segundos, arredondado pra cima).
//! - Erro de culpa da request (400/413/422): no máximo 1 candidato extra; dois
//!   seguidos → status do upstream devolvido ao cliente (ADR 0003, A3).
//! - Deltas de raciocínio nunca chegam ao cliente (ADR 0004, N4).
//! - `x-openkax-exclude-family: glm,qwen` tira famílias da rota (N5).
//! - `x-openkax-max-tier: free` tira degraus credit/paid da rota.
//! - `x-openkax-pin: provider/modelo` (depuração) fixa o candidato; continua
//!   passando por reserva/ledger; modelo desabilitado → 409. Com pin, a resposta
//!   traz os headers de limite do upstream como `x-openkax-upstream-<nome>`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::net::TcpListener;

use crate::config::{Auth, Config, Tier};
use crate::ledger::{self, LedgerEntry, LedgerHandle, Outcome, RequestRecord};
use crate::provider::{self, ProviderClient, Secrets, StartedStream, UpstreamError};
use crate::scheduler::{
    Candidate, Clock, NotNow, Pick, PickError, PickOptions, PinError, Reservation, Scheduler,
    SystemClock,
};
use crate::translate::openai::{
    ChatCompletionChunk, ChatCompletionRequest, ChunkChoice, Delta, Usage,
};

pub use crate::provider::install_crypto_provider;

/// Nome do header de resposta com o provider que atendeu.
pub const PROVIDER_HEADER: &str = "x-openkax-provider";
/// Header de request: famílias a excluir (separadas por vírgula).
pub const EXCLUDE_FAMILY_HEADER: &str = "x-openkax-exclude-family";
/// Header de request: teto de tier (`free`, `credit`, `paid`).
pub const MAX_TIER_HEADER: &str = "x-openkax-max-tier";
/// Header de request (depuração): `provider/modelo` fixo.
pub const PIN_HEADER: &str = "x-openkax-pin";

/// Dependências externas do servidor; tudo injetável para teste.
pub struct Deps {
    pub config: Config,
    pub secrets: Secrets,
    pub clock: Arc<dyn Clock>,
    pub ledger: LedgerHandle,
}

impl Deps {
    /// Relógio real, ledger desligado.
    pub fn new(config: Config, secrets: Secrets) -> Self {
        Self {
            config,
            secrets,
            clock: Arc::new(SystemClock),
            ledger: LedgerHandle::disabled(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error("falha ao criar cliente HTTP: {0}")]
    Http(#[from] reqwest::Error),
}

/// Estado compartilhado pelas rotas.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    config: Config,
    /// `config.providers` com `${VAR}` resolvidas (N1); ausentes ficam como estão
    /// (o scheduler já os marca `MissingEnv`).
    resolved: Vec<crate::config::Provider>,
    /// Requests atendidas por tier (free, credit, paid) desde o start (N3).
    served: [AtomicU64; 3],
    secrets: Secrets,
    scheduler: Scheduler,
    client: ProviderClient,
    ledger: LedgerHandle,
}

impl AppState {
    pub fn new(deps: Deps) -> Result<Self, StartupError> {
        let http = provider::build_http_client(Duration::from_millis(
            deps.config.defaults.request_timeout_ms,
        ))?;
        let scheduler = Scheduler::new(&deps.config, deps.clock, &|env| {
            deps.secrets.get(env).is_some()
        });
        let lookup = |k: &str| deps.secrets.get(k).map(str::to_owned);
        let resolved = deps
            .config
            .providers
            .iter()
            .map(|p| p.resolve_env(&lookup).unwrap_or_else(|_| p.clone()))
            .collect();
        Ok(Self {
            inner: Arc::new(Inner {
                resolved,
                served: Default::default(),
                config: deps.config,
                secrets: deps.secrets,
                scheduler,
                client: ProviderClient::new(http),
                ledger: deps.ledger,
            }),
        })
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    pub fn scheduler(&self) -> &Scheduler {
        &self.inner.scheduler
    }

    pub fn ledger(&self) -> &LedgerHandle {
        &self.inner.ledger
    }

    fn api_key(&self, provider_idx: usize) -> Option<&str> {
        match &self.inner.config.providers[provider_idx].auth {
            Auth::Bearer { env } => self.inner.secrets.get(env),
            Auth::Passthrough | Auth::None => None,
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/models", get(models))
        .route("/quota", get(quota))
        .with_state(state)
}

/// Serve até `shutdown` resolver.
pub async fn serve(
    listener: TcpListener,
    state: AppState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
}

async fn health() -> &'static str {
    "ok"
}

async fn models(State(st): State<AppState>) -> Response {
    let data: Vec<Value> = st
        .config()
        .classes
        .iter()
        .map(|c| json!({ "id": c.id, "object": "model", "created": 0, "owned_by": "openkax" }))
        .collect();
    axum::Json(json!({ "object": "list", "data": data })).into_response()
}

async fn quota(State(st): State<AppState>) -> Response {
    let mut v = serde_json::to_value(st.scheduler().snapshot()).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert("ledger_dropped".into(), st.ledger().dropped().into());
        let served = &st.inner.served;
        o.insert(
            "served_by_tier".into(),
            json!({
                "free": served[0].load(Ordering::Relaxed),
                "credit": served[1].load(Ordering::Relaxed),
                "paid": served[2].load(Ordering::Relaxed),
            }),
        );
    }
    axum::Json(v).into_response()
}

fn error_response(status: StatusCode, kind: &str, message: &str) -> Response {
    let body = json!({ "error": { "message": message, "type": kind, "code": status.as_u16() } });
    (status, axum::Json(body)).into_response()
}

/// Campos comuns do registro de ledger de uma request.
struct Meta {
    route: String,
    est_in: u32,
    started: Instant,
    fallbacks: u32,
    pinned: bool,
    reasoning: bool,
}

impl Meta {
    fn record(
        &self,
        st: &AppState,
        cand: Option<&Candidate>,
        usage: Option<Usage>,
        ttft: Option<Duration>,
        outcome: Outcome,
    ) {
        let tier = cand.map(|c| st.config().providers[c.provider_idx].tier);
        if let Some(t) = tier
            && matches!(
                outcome,
                Outcome::Ok | Outcome::MidStreamError | Outcome::ClientDisconnected
            )
        {
            let i = match t {
                Tier::Free => 0,
                Tier::Credit => 1,
                Tier::Paid => 2,
            };
            st.inner.served[i].fetch_add(1, Ordering::Relaxed);
        }
        st.ledger().record(LedgerEntry::Request(RequestRecord {
            ts: ledger::rfc3339(SystemTime::now()),
            route: self.route.clone(),
            provider: cand.map(|c| c.provider_id.to_string()),
            model: cand.map(|c| c.model_id.to_string()),
            est_in: self.est_in,
            input_tokens: usage.map(|u| u.prompt_tokens),
            output_tokens: usage.map(|u| u.completion_tokens),
            latency_ms: self.started.elapsed().as_millis() as u64,
            ttft_ms: ttft.map(|d| d.as_millis() as u64),
            fallbacks: self.fallbacks,
            tier,
            reasoning: self.reasoning,
            pinned: self.pinned,
            outcome,
        }));
    }
}

fn header_str<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

async fn chat_completions(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let started = Instant::now();
    let req: ChatCompletionRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("corpo inválido: {e}"),
            );
        }
    };
    let mut meta = Meta {
        route: req.model.clone(),
        est_in: 0,
        started,
        fallbacks: 0,
        pinned: false,
        reasoning: false,
    };
    if !req.model.starts_with("kax/") {
        meta.record(&st, None, None, None, Outcome::Rejected);
        return error_response(
            StatusCode::NOT_FOUND,
            "model_not_found",
            "esta rota aceita apenas classes kax/* (veja /v1/models)",
        );
    }
    let pick = match choose(&st, &headers, &req) {
        Ok(p) => p,
        Err(resp) => {
            meta.record(&st, None, None, None, Outcome::Rejected);
            return resp;
        }
    };
    meta.pinned = header_str(&headers, PIN_HEADER).is_some();
    meta.est_in = pick.candidates.first().map_or(0, |c| c.est_tokens);

    let mut waits: Vec<Duration> = pick.retry_after().into_iter().collect();
    let mut fault: Option<(u16, String)> = None;
    let mut last_was_fault = false;
    let mut attempts_after_fault = 0;

    for cand in &pick.candidates {
        if fault.is_some() && attempts_after_fault >= 1 {
            break;
        }
        let mut res = match st.scheduler().reserve(cand) {
            Ok(r) => r,
            Err(NotNow { ready_in }) => {
                if !ready_in.is_zero() {
                    waits.push(ready_in);
                }
                continue;
            }
        };
        let provider_cfg = &st.inner.resolved[cand.provider_idx];
        let (first_token_timeout, first_token_timeout_reasoning) = st
            .config()
            .defaults
            .first_token_timeouts(provider_cfg.models.get(cand.model_idx));
        let attempt = st
            .inner
            .client
            .start_stream(
                provider_cfg,
                st.api_key(cand.provider_idx),
                &cand.upstream_model,
                cand.max_tokens_override,
                &req,
                first_token_timeout,
                first_token_timeout_reasoning,
            )
            .await;
        res.observe_remaining(&attempt.remaining);
        let debug_headers = if meta.pinned {
            attempt.limit_headers
        } else {
            Vec::new()
        };
        match attempt.result {
            Ok(stream) => {
                res.first_token();
                let ttft = started.elapsed();
                let mut resp = if req.is_stream() {
                    stream_response(st.clone(), meta, cand.clone(), res, stream, ttft, &req)
                } else {
                    aggregate_response(&st, meta, cand, res, stream, ttft).await
                };
                add_upstream_headers(&mut resp, &debug_headers);
                return resp;
            }
            Err(e) if meta.pinned => {
                let outcome = if matches!(e, UpstreamError::RequestFault { .. }) {
                    Outcome::RequestFault
                } else {
                    Outcome::Exhausted
                };
                res.fail(&e);
                meta.fallbacks += 1;
                meta.record(&st, Some(cand), None, None, outcome);
                let mut resp = match &e {
                    UpstreamError::RequestFault { status, message } => upstream_fault_response(
                        StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_REQUEST),
                        message,
                    ),
                    UpstreamError::RateLimited { .. } => error_response(
                        StatusCode::TOO_MANY_REQUESTS,
                        "rate_limit_error",
                        &e.to_string(),
                    ),
                    _ => error_response(StatusCode::BAD_GATEWAY, "upstream_error", &e.to_string()),
                };
                add_upstream_headers(&mut resp, &debug_headers);
                return resp;
            }
            Err(e) => {
                tracing::warn!(
                    provider = %cand.provider_id,
                    model = %cand.model_id,
                    error = %e,
                    "candidato falhou antes do 1º token"
                );
                if fault.is_some() {
                    attempts_after_fault += 1;
                }
                last_was_fault = matches!(e, UpstreamError::RequestFault { .. });
                if let Some(d) = res.fail(&e) {
                    waits.push(d);
                }
                if let UpstreamError::RequestFault { status, message } = e {
                    fault = Some((status, message));
                }
                meta.fallbacks += 1;
            }
        }
    }

    if last_was_fault && let Some((status, message)) = fault {
        meta.record(&st, None, None, None, Outcome::RequestFault);
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
        return upstream_fault_response(status, &message);
    }
    meta.record(&st, None, None, None, Outcome::Exhausted);
    if let Some(wait) = waits.into_iter().filter(|d| !d.is_zero()).min() {
        let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
        let mut r = error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            "todos os providers da classe estão sem saldo ou em cooldown",
        );
        r.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(secs.max(1)));
        return r;
    }
    if pick.all_never_fit() {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            "a request não cabe em nenhum provider da classe (max_ctx/TPM)",
        );
    }
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "service_unavailable",
        "nenhum provider disponível para a classe",
    )
}

/// Escolhe os candidatos: pin (depuração) ou rota da classe com filtros.
#[allow(clippy::result_large_err)]
fn choose(
    st: &AppState,
    headers: &HeaderMap,
    req: &ChatCompletionRequest,
) -> Result<Pick, Response> {
    let pick_err = |e: PickError| {
        let status = match e {
            PickError::UnknownClass(_) => StatusCode::NOT_FOUND,
            PickError::PacketTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
        };
        error_response(status, "invalid_request_error", &e.to_string())
    };
    if let Some(pin) = header_str(headers, PIN_HEADER) {
        let Some((provider, model)) = pin.split_once('/') else {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "x-openkax-pin deve ser provider/modelo",
            ));
        };
        return st
            .scheduler()
            .pick_pinned(&req.model, provider, model, req)
            .map_err(|e| match e {
                PinError::Pick(e) => pick_err(e),
                PinError::UnknownTarget => error_response(
                    StatusCode::NOT_FOUND,
                    "model_not_found",
                    &format!("pin {pin:?}: provider/modelo inexistente"),
                ),
                PinError::Disabled(reason) => {
                    let reason = serde_json::to_value(reason)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default();
                    error_response(
                        StatusCode::CONFLICT,
                        "model_disabled",
                        &format!("pin {pin:?}: modelo desabilitado ({reason})"),
                    )
                }
            });
    }
    let max_tier = match header_str(headers, MAX_TIER_HEADER) {
        None => None,
        Some(v) => match serde_json::from_value::<Tier>(Value::String(v.to_ascii_lowercase())) {
            Ok(t) => Some(t),
            Err(_) => {
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "x-openkax-max-tier deve ser free, credit ou paid",
                ));
            }
        },
    };
    let opts = PickOptions {
        exclude_families: header_str(headers, EXCLUDE_FAMILY_HEADER)
            .map(|v| {
                v.split(',')
                    .map(|f| f.trim().to_owned())
                    .filter(|f| !f.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        max_tier,
    };
    st.scheduler()
        .pick_with(&req.model, req, &opts)
        .map_err(pick_err)
}

/// Com pin: headers de limite do upstream como `x-openkax-upstream-<nome>`.
fn add_upstream_headers(resp: &mut Response, limit_headers: &[(String, String)]) {
    for (k, v) in limit_headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(format!("x-openkax-upstream-{k}").as_bytes()),
            HeaderValue::from_str(v),
        ) {
            resp.headers_mut().insert(name, value);
        }
    }
}

/// Repassa o erro de request do upstream (JSON se for JSON; senão embrulha).
fn upstream_fault_response(status: StatusCode, message: &str) -> Response {
    match serde_json::from_str::<Value>(message) {
        Ok(v) if v.get("error").is_some() => (status, axum::Json(v)).into_response(),
        _ => error_response(status, "invalid_request_error", message),
    }
}

fn sse_bytes(chunk: &ChatCompletionChunk) -> Bytes {
    let json = serde_json::to_string(chunk).unwrap_or_default();
    Bytes::from(format!("data: {json}\n\n"))
}

const DONE: &str = "data: [DONE]\n\n";

fn output_chars(chunk: &ChatCompletionChunk) -> usize {
    chunk
        .choices
        .iter()
        .map(|c| {
            let text = c.delta.content.as_deref().map_or(0, |t| t.chars().count());
            let tools = c.delta.tool_calls.as_ref().map_or(0, |tc| {
                tc.iter()
                    .filter_map(|t| t["function"]["arguments"].as_str())
                    .map(|a| a.chars().count())
                    .sum()
            });
            text + tools
        })
        .sum()
}

/// Repassa prelúdio + resto do upstream ao cliente; dono da `Reservation`.
/// Dropado antes do fim (cliente desconectou) → `Reservation::drop` cobra o
/// estimado sem cooldown e o ledger registra `client_disconnected`.
struct Relay {
    st: AppState,
    meta: Meta,
    cand: Candidate,
    res: Option<Reservation>,
    prelude: VecDeque<ChatCompletionChunk>,
    rest: BoxStream<'static, Result<ChatCompletionChunk, UpstreamError>>,
    usage: Option<Usage>,
    ttft: Duration,
    forward_usage: bool,
    last: (String, u64, String),
    finished: bool,
}

impl Relay {
    /// Contabiliza, remove raciocínio (N4) e decide se o chunk vai ao cliente.
    fn observe(&mut self, chunk: &mut ChatCompletionChunk) -> bool {
        let reasoning = provider::strip_reasoning(chunk);
        if reasoning > 0 {
            self.meta.reasoning = true;
        }
        if let Some(r) = &mut self.res {
            r.add_output_chars(output_chars(chunk) + reasoning);
        }
        if chunk.usage.is_some() {
            self.usage = chunk.usage;
        }
        if !chunk.id.is_empty() {
            self.last = (chunk.id.clone(), chunk.created, chunk.model.clone());
        }
        !provider::is_empty_chunk(chunk) && (!chunk.choices.is_empty() || self.forward_usage)
    }

    fn finish_ok(&mut self) {
        self.finished = true;
        if let Some(r) = self.res.take() {
            r.settle(self.usage);
        }
        self.meta.record(
            &self.st,
            Some(&self.cand),
            self.usage,
            Some(self.ttft),
            Outcome::Ok,
        );
    }

    fn finish_err(&mut self, e: &UpstreamError) -> Bytes {
        self.finished = true;
        tracing::warn!(provider = %self.cand.provider_id, model = %self.cand.model_id, error = %e, "stream interrompido após o 1º token");
        if let Some(r) = self.res.take() {
            r.fail(e);
        }
        self.meta.record(
            &self.st,
            Some(&self.cand),
            self.usage,
            Some(self.ttft),
            Outcome::MidStreamError,
        );
        let (id, created, model) = self.last.clone();
        let chunk = ChatCompletionChunk {
            id,
            object: "chat.completion.chunk".into(),
            created,
            model,
            choices: vec![ChunkChoice {
                index: 0,
                delta: Delta::default(),
                finish_reason: Some("error".into()),
            }],
            usage: None,
            extra: Default::default(),
        };
        let mut out = sse_bytes(&chunk).to_vec();
        out.extend_from_slice(DONE.as_bytes());
        Bytes::from(out)
    }

    async fn next_bytes(&mut self) -> Option<Bytes> {
        loop {
            if self.finished {
                return None;
            }
            if let Some(mut c) = self.prelude.pop_front() {
                if self.observe(&mut c) {
                    return Some(sse_bytes(&c));
                }
                continue;
            }
            match self.rest.next().await {
                Some(Ok(mut c)) => {
                    if self.observe(&mut c) {
                        return Some(sse_bytes(&c));
                    }
                }
                None => {
                    self.finish_ok();
                    return Some(Bytes::from_static(DONE.as_bytes()));
                }
                Some(Err(e)) => return Some(self.finish_err(&e)),
            }
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        if !self.finished {
            // `self.res` cai junto: Reservation::drop pós-1º-token, sem cooldown.
            self.meta.record(
                &self.st,
                Some(&self.cand),
                self.usage,
                Some(self.ttft),
                Outcome::ClientDisconnected,
            );
        }
    }
}

fn new_relay(
    st: AppState,
    meta: Meta,
    cand: Candidate,
    res: Reservation,
    stream: StartedStream,
    ttft: Duration,
    forward_usage: bool,
) -> Relay {
    Relay {
        st,
        meta,
        cand,
        res: Some(res),
        prelude: stream.prelude.into(),
        rest: stream.rest,
        usage: None,
        ttft,
        forward_usage,
        last: Default::default(),
        finished: false,
    }
}

fn stream_response(
    st: AppState,
    meta: Meta,
    cand: Candidate,
    res: Reservation,
    stream: StartedStream,
    ttft: Duration,
    req: &ChatCompletionRequest,
) -> Response {
    let forward_usage = req
        .extra
        .get("stream_options")
        .and_then(|o| o.get("include_usage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let provider = HeaderValue::from_str(&cand.provider_id).ok();
    let relay = new_relay(st, meta, cand, res, stream, ttft, forward_usage);
    let body = futures::stream::unfold(relay, |mut r| async move {
        r.next_bytes()
            .await
            .map(|b| (Ok::<_, std::convert::Infallible>(b), r))
    });
    let mut resp = Response::new(Body::from_stream(body));
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Some(p) = provider {
        h.insert(PROVIDER_HEADER, p);
    }
    resp
}

/// Cliente sem streaming: consome o stream upstream e monta `chat.completion`.
async fn aggregate_response(
    st: &AppState,
    meta: Meta,
    cand: &Candidate,
    res: Reservation,
    stream: StartedStream,
    ttft: Duration,
) -> Response {
    let mut relay = new_relay(st.clone(), meta, cand.clone(), res, stream, ttft, true);
    let mut content = String::new();
    let mut tool_calls: BTreeMap<u64, Value> = BTreeMap::new();
    let mut finish_reason: Option<String> = None;
    let mut chunks = Vec::new();
    loop {
        if let Some(mut c) = relay.prelude.pop_front() {
            relay.observe(&mut c);
            chunks.push(c);
            continue;
        }
        match relay.rest.next().await {
            Some(Ok(mut c)) => {
                relay.observe(&mut c);
                chunks.push(c);
            }
            None => {
                relay.finish_ok();
                break;
            }
            Some(Err(e)) => {
                relay.finish_err(&e);
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    "upstream_error",
                    "o provider interrompeu a resposta no meio; reenvie a request",
                );
            }
        }
    }
    for c in &chunks {
        for ch in &c.choices {
            if let Some(t) = &ch.delta.content {
                content.push_str(t);
            }
            if let Some(fr) = &ch.finish_reason {
                finish_reason = Some(fr.clone());
            }
            for tc in ch.delta.tool_calls.iter().flatten() {
                merge_tool_call(&mut tool_calls, tc);
            }
        }
    }
    let (id, created, model) = relay.last.clone();
    let mut message = json!({ "role": "assistant", "content": content });
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls.into_values().collect());
        if content.is_empty() {
            message["content"] = Value::Null;
        }
    }
    let mut body = json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason }],
    });
    if let Some(u) = relay.usage {
        body["usage"] = serde_json::to_value(u).unwrap_or(Value::Null);
    }
    let mut resp = axum::Json(body).into_response();
    if let Ok(p) = HeaderValue::from_str(&cand.provider_id) {
        resp.headers_mut().insert(PROVIDER_HEADER, p);
    }
    resp
}

/// Junta deltas de `tool_calls` por `index` (arguments chegam em pedaços).
fn merge_tool_call(acc: &mut BTreeMap<u64, Value>, delta: &Value) {
    let idx = delta["index"].as_u64().unwrap_or(0);
    let entry = acc.entry(idx).or_insert_with(
        || json!({ "id": "", "type": "function", "function": { "name": "", "arguments": "" } }),
    );
    if let Some(id) = delta["id"].as_str() {
        entry["id"] = id.into();
    }
    for field in ["name", "arguments"] {
        if let Some(part) = delta["function"][field].as_str() {
            let cur = entry["function"][field].as_str().unwrap_or("").to_owned();
            entry["function"][field] = (cur + part).into();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_deltas_merge_by_index() {
        let mut acc = BTreeMap::new();
        merge_tool_call(
            &mut acc,
            &json!({ "index": 0, "id": "c1", "function": { "name": "grep", "arguments": "{\"q\":" } }),
        );
        merge_tool_call(
            &mut acc,
            &json!({ "index": 0, "function": { "arguments": "\"x\"}" } }),
        );
        merge_tool_call(
            &mut acc,
            &json!({ "index": 1, "id": "c2", "function": { "name": "ls" } }),
        );
        assert_eq!(acc[&0]["function"]["arguments"], "{\"q\":\"x\"}");
        assert_eq!(acc[&0]["id"], "c1");
        assert_eq!(acc[&1]["function"]["name"], "ls");
    }
}
