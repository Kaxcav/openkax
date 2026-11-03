//! Superfície HTTP (`axum`) e despacho com fallback pré-1º-token.
//!
//! Contrato observável (exercitado por `tests/fallback.rs`):
//! - `POST /v1/chat/completions` com `model = "kax/<classe>"` roteia pelo scheduler.
//! - Streaming: os headers só saem depois do 1º token do candidato escolhido;
//!   erros antes disso caem no próximo candidato sem o cliente perceber.
//! - Header `x-openkax-provider` identifica o provider que respondeu.
//! - Morte pós-1º-token: chunk final com `finish_reason: "error"` + `data: [DONE]`.
//! - `finish_reason: "error"` antes do 1º token conta como erro do candidato.
//! - Nenhum candidato disponível: `429` com `Retry-After` (segundos, arredondado pra cima).
//! - Erro de culpa da request (400/413/422): no máximo 1 candidato extra; dois
//!   seguidos → status do upstream devolvido ao cliente (ADR 0003, A3).
//! - Deltas de raciocínio nunca chegam ao cliente (ADR 0004, N4).
//! - `usage` só sai se o cliente pediu (`stream_options.include_usage`), uma vez,
//!   num chunk de `choices` vazio logo antes de `[DONE]`, seja qual for o formato
//!   do upstream (F0-24).
//! - Índices de `tool_calls` chegam ao cliente estáveis e sequenciais, mesmo com
//!   upstream que omite ou repete `index` em chamadas paralelas (F0-24).
//! - `x-openkax-exclude-family: glm,qwen` tira famílias da rota (N5).
//! - `x-openkax-max-tier: free` tira degraus credit/paid da rota.
//! - `x-openkax-pin: provider/modelo` (depuração) fixa o candidato; continua
//!   passando por reserva/ledger; modelo desabilitado → 409. Com pin, a resposta
//!   traz os headers de limite do upstream como `x-openkax-upstream-<nome>`.
//!
//! `POST /v1/messages` usa o mesmo despacho; só a forma da resposta e do erro muda
//! ([`Wire`], módulo [`anthropic`], ADR 0006).

mod anthropic;

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
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
    BucketsSnapshot, Candidate, Clock, NotNow, Pick, PickError, PickOptions, PinError, Reservation,
    Scheduler, SystemClock,
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
/// Teto do corpo de `/v1/messages`. O padrão do axum (2 MiB) recusaria imagem em
/// base64, que o Claude Code manda inline.
pub const MESSAGES_BODY_LIMIT: usize = 32 * 1024 * 1024;

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

/// Estado compartilhado pelas rotas. A config é trocada a quente
/// ([`reload`](Self::reload)); cada request enxerga uma geração só ([`View`]).
#[derive(Clone)]
pub struct AppState {
    hub: Arc<Hub>,
}

struct Hub {
    live: RwLock<Arc<Live>>,
    clock: Arc<dyn Clock>,
    /// Requests atendidas por tier (free, credit, paid) desde o start (N3).
    served: [AtomicU64; 3],
    ledger: LedgerHandle,
}

/// Uma geração de config: tudo que muda junto num reload (F0-10).
struct Live {
    config: Config,
    /// `config.providers` com `${VAR}` resolvidas (N1); ausentes ficam como estão
    /// (o scheduler já os marca `MissingEnv`).
    resolved: Vec<crate::config::Provider>,
    secrets: Secrets,
    scheduler: Scheduler,
    client: ProviderClient,
    passthrough_client: reqwest::Client,
}

impl Live {
    fn build(
        config: Config,
        secrets: Secrets,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, StartupError> {
        let http =
            provider::build_http_client(Duration::from_millis(config.defaults.request_timeout_ms))?;
        let passthrough_client = provider::build_passthrough_client()?;
        let scheduler = Scheduler::new(&config, clock, &|env| secrets.get(env).is_some());
        let lookup = |k: &str| secrets.get(k).map(str::to_owned);
        let resolved = config
            .providers
            .iter()
            .map(|p| p.resolve_env(&lookup).unwrap_or_else(|_| p.clone()))
            .collect();
        Ok(Self {
            config,
            resolved,
            secrets,
            scheduler,
            client: ProviderClient::new(http),
            passthrough_client,
        })
    }
}

/// O que uma request usa do começo ao fim: a geração viva quando ela chegou.
/// Reservations em voo fecham no scheduler da própria geração.
#[derive(Clone)]
struct View {
    hub: Arc<Hub>,
    live: Arc<Live>,
}

impl View {
    fn config(&self) -> &Config {
        &self.live.config
    }

    fn scheduler(&self) -> &Scheduler {
        &self.live.scheduler
    }

    fn ledger(&self) -> &LedgerHandle {
        &self.hub.ledger
    }

    fn passthrough_client(&self) -> &reqwest::Client {
        &self.live.passthrough_client
    }

    fn api_key(&self, provider_idx: usize) -> Option<&str> {
        match &self.live.config.providers[provider_idx].auth {
            Auth::Bearer { env } => self.live.secrets.get(env),
            Auth::Passthrough | Auth::None => None,
        }
    }
}

impl AppState {
    pub fn new(deps: Deps) -> Result<Self, StartupError> {
        let live = Live::build(deps.config, deps.secrets, deps.clock.clone())?;
        Ok(Self {
            hub: Arc::new(Hub {
                live: RwLock::new(Arc::new(live)),
                clock: deps.clock,
                served: Default::default(),
                ledger: deps.ledger,
            }),
        })
    }

    fn view(&self) -> View {
        View {
            hub: self.hub.clone(),
            live: self
                .hub
                .live
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        }
    }

    /// Scheduler da geração viva.
    pub fn scheduler(&self) -> Scheduler {
        self.view().live.scheduler.clone()
    }

    pub fn ledger(&self) -> &LedgerHandle {
        &self.hub.ledger
    }

    /// Troca a config a quente (F0-10). Buckets, cooldowns e chars/token passam
    /// para a geração nova casando por id; `Disabled` volta a valer só se
    /// acontecer de novo (é "até reload"). Requests em voo terminam na antiga.
    pub fn reload(&self, config: Config, secrets: Secrets) -> Result<(), StartupError> {
        let next = Live::build(config, secrets, self.hub.clock.clone())?;
        let mut live = self
            .hub
            .live
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        next.scheduler.restore_state(&live.scheduler.export_state());
        *live = Arc::new(next);
        Ok(())
    }

    /// Grava o snapshot dos buckets em `path` (JSON; escrita atômica via
    /// arquivo temporário + rename). Cria o diretório se faltar.
    pub fn save_state(&self, path: &Path) -> std::io::Result<()> {
        let snap = self.scheduler().export_state();
        let json = serde_json::to_vec_pretty(&snap).map_err(std::io::Error::other)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }

    /// Restaura o snapshot de `path`. `Ok(false)` se o arquivo não existe;
    /// JSON inválido → `Err(InvalidData)` e o estado fica como estava.
    pub fn restore_state(&self, path: &Path) -> std::io::Result<bool> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        let snap: BucketsSnapshot = serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.scheduler().restore_state(&snap);
        Ok(true)
    }
}

/// Mantém o watcher do `providers.toml` vivo; dropar para de observar.
pub struct ConfigWatcher {
    _watcher: notify::RecommendedWatcher,
}

/// Espera depois do 1º evento antes de reler: editores gravam em rajadas.
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(300);

/// Observa o diretório de `path` (editores trocam o arquivo por rename) e
/// recarrega a config quando ele muda. TOML inválido é logado e ignorado: a
/// config viva continua. `secrets` resolve as chaves da config nova.
pub fn watch_config(
    path: PathBuf,
    state: AppState,
    secrets: impl Fn(&Config) -> Secrets + Send + 'static,
) -> notify::Result<ConfigWatcher> {
    use notify::Watcher;

    let file_name = path.file_name().map(ToOwned::to_owned);
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let mut watcher = notify::recommended_watcher(move |ev: notify::Result<notify::Event>| {
        if let Ok(ev) = ev
            && !ev.kind.is_access()
            && ev
                .paths
                .iter()
                .any(|p| p.file_name() == file_name.as_deref())
        {
            let _ = tx.send(());
        }
    })?;
    watcher.watch(&dir, notify::RecursiveMode::NonRecursive)?;
    std::thread::Builder::new()
        .name("openkax-config-watch".into())
        .spawn(move || {
            while rx.recv().is_ok() {
                std::thread::sleep(RELOAD_DEBOUNCE);
                while rx.try_recv().is_ok() {}
                match Config::load(&path) {
                    Ok(cfg) => {
                        let s = secrets(&cfg);
                        match state.reload(cfg, s) {
                            Ok(()) => {
                                tracing::info!(config = %path.display(), "config recarregada")
                            }
                            Err(e) => tracing::warn!("reload falhou, config anterior mantida: {e}"),
                        }
                    }
                    Err(e) => {
                        tracing::warn!("providers.toml inválido, config anterior mantida: {e}")
                    }
                }
            }
        })
        .map_err(notify::Error::io)?;
    Ok(ConfigWatcher { _watcher: watcher })
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/chat/completions", post(chat_completions))
        .route(
            "/v1/messages",
            post(anthropic::messages).layer(DefaultBodyLimit::max(MESSAGES_BODY_LIMIT)),
        )
        .route(
            "/v1/messages/count_tokens",
            post(anthropic::count_tokens).layer(DefaultBodyLimit::max(MESSAGES_BODY_LIMIT)),
        )
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

async fn models(State(app): State<AppState>) -> Response {
    let st = app.view();
    let data: Vec<Value> = st
        .config()
        .classes
        .iter()
        .map(|c| json!({ "id": c.id, "object": "model", "created": 0, "owned_by": "openkax" }))
        .collect();
    axum::Json(json!({ "object": "list", "data": data })).into_response()
}

async fn quota(State(app): State<AppState>) -> Response {
    let st = app.view();
    let mut v = serde_json::to_value(st.scheduler().snapshot()).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert("ledger_dropped".into(), st.ledger().dropped().into());
        let served = &st.hub.served;
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

/// Protocolo de entrada: decide a forma da resposta e do erro. A política de
/// fallback é uma só, em [`dispatch`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wire {
    OpenAi,
    Anthropic,
}

impl Wire {
    /// `kind` é o `type` do OpenAI; a Anthropic deriva o dela do status.
    fn error(self, status: StatusCode, kind: &str, message: &str) -> Response {
        match self {
            Self::OpenAi => error_response(status, kind, message),
            Self::Anthropic => anthropic::error_response(status, message),
        }
    }

    /// Erro de request do upstream, com o status dele.
    fn upstream_fault(self, status: StatusCode, message: &str) -> Response {
        match self {
            Self::OpenAi => upstream_fault_response(status, message),
            Self::Anthropic => {
                anthropic::error_response(status, &anthropic::upstream_message(message))
            }
        }
    }

    /// 429 de rate limit, com `Retry-After` se houver prazo.
    fn rate_limited(self, wait: Option<Duration>, message: &str) -> Response {
        let mut r = self.error(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", message);
        if let Some(wait) = wait {
            let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
            r.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(secs.max(1)));
        }
        r
    }
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
        st: &View,
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
            st.hub.served[i].fetch_add(1, Ordering::Relaxed);
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
            status: None,
        }));
    }
}

fn header_str<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

async fn chat_completions(
    State(app): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let st = app.view();
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
    let meta = Meta {
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
    dispatch(&st, &headers, &req, Wire::OpenAi, meta).await
}

/// Roteia pelo scheduler com fallback pré-1º-token. `req` é sempre OpenAI, com a
/// classe em `model`; `wire` diz em que forma o cliente recebe a resposta.
async fn dispatch(
    st: &View,
    headers: &HeaderMap,
    req: &ChatCompletionRequest,
    wire: Wire,
    mut meta: Meta,
) -> Response {
    let started = meta.started;
    let pick = match choose(st, headers, req, wire) {
        Ok(p) => p,
        Err(resp) => {
            meta.record(st, None, None, None, Outcome::Rejected);
            return resp;
        }
    };
    meta.pinned = header_str(headers, PIN_HEADER).is_some();
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
        let provider_cfg = &st.live.resolved[cand.provider_idx];
        let (first_token_timeout, first_token_timeout_reasoning) = st
            .config()
            .defaults
            .first_token_timeouts(provider_cfg.models.get(cand.model_idx));
        let attempt = st
            .live
            .client
            .start_stream(
                provider_cfg,
                st.api_key(cand.provider_idx),
                &cand.upstream_model,
                cand.max_tokens_override,
                req,
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
                let mut resp = match (wire, req.is_stream()) {
                    (Wire::OpenAi, true) => {
                        stream_response(st.clone(), meta, cand.clone(), res, stream, ttft, req)
                    }
                    (Wire::OpenAi, false) => {
                        aggregate_response(st, meta, cand, res, stream, ttft).await
                    }
                    (Wire::Anthropic, is_stream) => {
                        let relay =
                            new_relay(st.clone(), meta, cand.clone(), res, stream, ttft, false);
                        anthropic::respond(relay, req, is_stream).await
                    }
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
                meta.record(st, Some(cand), None, None, outcome);
                let mut resp = match &e {
                    UpstreamError::RequestFault { status, message } => wire.upstream_fault(
                        StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_REQUEST),
                        message,
                    ),
                    UpstreamError::RateLimited { retry_after, .. } => {
                        wire.rate_limited(*retry_after, &e.to_string())
                    }
                    _ => wire.error(StatusCode::BAD_GATEWAY, "upstream_error", &e.to_string()),
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
        meta.record(st, None, None, None, Outcome::RequestFault);
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
        return wire.upstream_fault(status, &message);
    }
    meta.record(st, None, None, None, Outcome::Exhausted);
    if let Some(wait) = waits.into_iter().filter(|d| !d.is_zero()).min() {
        return wire.rate_limited(
            Some(wait),
            "todos os providers da classe estão sem saldo ou em cooldown",
        );
    }
    if pick.all_never_fit() {
        return wire.error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            "a request não cabe em nenhum provider da classe (max_ctx/TPM)",
        );
    }
    wire.error(
        StatusCode::SERVICE_UNAVAILABLE,
        "service_unavailable",
        "nenhum provider disponível para a classe",
    )
}

/// Escolhe os candidatos: pin (depuração) ou rota da classe com filtros.
#[allow(clippy::result_large_err)]
fn choose(
    st: &View,
    headers: &HeaderMap,
    req: &ChatCompletionRequest,
    wire: Wire,
) -> Result<Pick, Response> {
    let pick_err = |e: PickError| {
        let status = match e {
            PickError::UnknownClass(_) => StatusCode::NOT_FOUND,
            PickError::PacketTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
        };
        wire.error(status, "invalid_request_error", &e.to_string())
    };
    let class = st.config().resolve_alias(&req.model).unwrap_or(&req.model);
    if let Some(pin) = header_str(headers, PIN_HEADER) {
        let Some((provider, model)) = pin.split_once('/') else {
            return Err(wire.error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "x-openkax-pin deve ser provider/modelo",
            ));
        };
        return st
            .scheduler()
            .pick_pinned(class, provider, model, req)
            .map_err(|e| match e {
                PinError::Pick(e) => pick_err(e),
                PinError::UnknownTarget => wire.error(
                    StatusCode::NOT_FOUND,
                    "model_not_found",
                    &format!("pin {pin:?}: provider/modelo inexistente"),
                ),
                PinError::Disabled(reason) => {
                    let reason = serde_json::to_value(reason)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default();
                    wire.error(
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
                return Err(wire.error(
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
        .pick_with(class, req, &opts)
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

/// O provider morreu depois do 1º token. O detalhe fica no log: pode trazer trecho
/// do corpo do upstream.
const INTERRUPTED: &str = "o provider interrompeu a resposta no meio; reenvie a request";

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

/// Dá a cada chamada paralela o seu índice. Há upstream que omite `index` ou
/// repete o mesmo em chamadas diferentes; o `id` novo é o que marca a chamada. Sem
/// `id` nenhum, marca a chamada o nome novo, ou o nome com argumentos depois de um
/// JSON já completo; nome repetido em chunk de continuação não conta.
#[derive(Default)]
struct ToolIndexer {
    /// Por índice devolvido ao cliente.
    slots: Vec<ToolSlot>,
}

#[derive(Default)]
struct ToolSlot {
    upstream_index: Option<u64>,
    id: String,
    name: String,
    args: String,
}

impl ToolSlot {
    fn starts_new_call(&self, name: &str, args: &str) -> bool {
        !name.is_empty()
            && !self.name.is_empty()
            && (name != self.name
                || (!args.is_empty()
                    && serde_json::from_str::<serde::de::IgnoredAny>(&self.args).is_ok()))
    }
}

impl ToolIndexer {
    fn normalize(&mut self, delta: &mut Value) {
        let up = delta["index"].as_u64();
        let id = delta["id"].as_str().unwrap_or("").to_owned();
        let name = delta["function"]["name"].as_str().unwrap_or("").to_owned();
        let args = delta["function"]["arguments"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        let by_index =
            up.and_then(|u| self.slots.iter().rposition(|s| s.upstream_index == Some(u)));
        let found = if id.is_empty() {
            // Continuação: a chamada daquele índice ou, sem índice, a última.
            let target = if up.is_some() {
                by_index
            } else {
                self.slots.len().checked_sub(1)
            };
            target.filter(|&i| !self.slots[i].starts_new_call(&name, &args))
        } else {
            self.slots
                .iter()
                .position(|s| s.id == id)
                .or(by_index.filter(|&i| self.slots[i].id.is_empty()))
        };
        let slot = found.unwrap_or_else(|| {
            self.slots.push(ToolSlot {
                upstream_index: up,
                ..ToolSlot::default()
            });
            self.slots.len() - 1
        });
        let entry = &mut self.slots[slot];
        if !id.is_empty() {
            entry.id = id;
        }
        if entry.name.is_empty() {
            entry.name = name;
        }
        entry.args.push_str(&args);
        if let Some(o) = delta.as_object_mut() {
            o.insert("index".into(), slot.into());
        }
    }
}

/// Repassa prelúdio + resto do upstream ao cliente; dono da `Reservation`.
/// Dropado antes do fim (cliente desconectou) → `Reservation::drop` cobra o
/// estimado sem cooldown e o ledger registra `client_disconnected`.
struct Relay {
    st: View,
    meta: Meta,
    cand: Candidate,
    res: Option<Reservation>,
    prelude: VecDeque<ChatCompletionChunk>,
    rest: BoxStream<'static, Result<ChatCompletionChunk, UpstreamError>>,
    usage: Option<Usage>,
    ttft: Duration,
    forward_usage: bool,
    tools: ToolIndexer,
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
        for c in chunk.choices.iter_mut().filter(|c| c.index == 0) {
            for t in c.delta.tool_calls.iter_mut().flatten() {
                self.tools.normalize(t);
            }
        }
        // O último vale; quem repassa é `usage_chunk`, no fim (F0-24): há upstream
        // que manda `usage` em todo chunk, e há cliente que trata isso como o fim.
        if let Some(u) = chunk.usage.take() {
            self.usage = Some(u);
        }
        if !chunk.id.is_empty() {
            self.last = (chunk.id.clone(), chunk.created, chunk.model.clone());
        }
        !provider::is_empty_chunk(chunk)
    }

    /// Chunk final com `usage`, se o cliente pediu e o upstream informou.
    fn usage_chunk(&self) -> Option<ChatCompletionChunk> {
        if !self.forward_usage {
            return None;
        }
        let (id, created, model) = self.last.clone();
        Some(ChatCompletionChunk {
            id,
            object: "chat.completion.chunk".into(),
            created,
            model,
            choices: Vec::new(),
            usage: Some(self.usage?),
            extra: Default::default(),
        })
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

    fn finish_err(&mut self, e: &UpstreamError) {
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
    }

    /// O que o cliente OpenAI recebe depois de uma morte no meio.
    fn error_tail(&self) -> Bytes {
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
        if let Some(u) = self.usage_chunk() {
            out.extend_from_slice(&sse_bytes(&u));
        }
        out.extend_from_slice(DONE.as_bytes());
        Bytes::from(out)
    }

    /// Próximo chunk para o cliente, já normalizado. `Ok(None)`: o upstream acabou.
    /// Quem chama fecha com [`finish_ok`](Self::finish_ok) ou
    /// [`finish_err`](Self::finish_err).
    async fn next_chunk(&mut self) -> Result<Option<ChatCompletionChunk>, UpstreamError> {
        loop {
            let mut chunk = match self.prelude.pop_front() {
                Some(c) => c,
                None => match self.rest.next().await {
                    Some(Ok(c)) => c,
                    Some(Err(e)) => return Err(e),
                    None => return Ok(None),
                },
            };
            if self.observe(&mut chunk) {
                return Ok(Some(chunk));
            }
        }
    }

    async fn next_bytes(&mut self) -> Option<Bytes> {
        if self.finished {
            return None;
        }
        match self.next_chunk().await {
            Ok(Some(c)) => Some(sse_bytes(&c)),
            Ok(None) => {
                self.finish_ok();
                let mut out = self
                    .usage_chunk()
                    .map_or_else(Vec::new, |c| sse_bytes(&c).to_vec());
                out.extend_from_slice(DONE.as_bytes());
                Some(Bytes::from(out))
            }
            Err(e) => {
                self.finish_err(&e);
                Some(self.error_tail())
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
    st: View,
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
        tools: ToolIndexer::default(),
        last: Default::default(),
        finished: false,
    }
}

fn stream_response(
    st: View,
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
    sse_response(Body::from_stream(body), provider)
}

fn sse_response(body: Body, provider: Option<HeaderValue>) -> Response {
    let mut resp = Response::new(body);
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
    st: &View,
    meta: Meta,
    cand: &Candidate,
    res: Reservation,
    stream: StartedStream,
    ttft: Duration,
) -> Response {
    let mut relay = new_relay(st.clone(), meta, cand.clone(), res, stream, ttft, false);
    let mut content = String::new();
    let mut tool_calls: BTreeMap<u64, Value> = BTreeMap::new();
    let mut finish_reason: Option<String> = None;
    let mut chunks = Vec::new();
    loop {
        match relay.next_chunk().await {
            Ok(Some(c)) => chunks.push(c),
            Ok(None) => {
                relay.finish_ok();
                break;
            }
            Err(e) => {
                relay.finish_err(&e);
                return error_response(StatusCode::BAD_GATEWAY, "upstream_error", INTERRUPTED);
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

    fn indexes(deltas: &[Value]) -> Vec<Value> {
        let mut ix = ToolIndexer::default();
        deltas
            .iter()
            .map(|d| {
                let mut d = d.clone();
                ix.normalize(&mut d);
                d["index"].clone()
            })
            .collect()
    }

    #[test]
    fn tool_indexer_keeps_well_formed_interleaved_indexes() {
        let d = [
            json!({ "index": 0, "id": "c1" }),
            json!({ "index": 1, "id": "c2" }),
            json!({ "index": 0 }),
            json!({ "index": 1 }),
        ];
        assert_eq!(indexes(&d), [json!(0), json!(1), json!(0), json!(1)]);
    }

    #[test]
    fn tool_indexer_splits_parallel_calls_with_repeated_or_missing_index() {
        let repeated = [
            json!({ "index": 0, "id": "c1" }),
            json!({ "index": 0 }),
            json!({ "index": 0, "id": "c2" }),
            json!({ "index": 0 }),
        ];
        assert_eq!(indexes(&repeated), [json!(0), json!(0), json!(1), json!(1)]);
        let missing = [
            json!({ "id": "c1" }),
            json!({ "function": { "arguments": "{}" } }),
            json!({ "id": "c2" }),
        ];
        assert_eq!(indexes(&missing), [json!(0), json!(0), json!(1)]);
    }

    /// Sem `id`, com `index` repetido ou ausente: duas chamadas não viram uma com
    /// `{"p":1}{"q":2}`.
    #[test]
    fn tool_indexer_splits_parallel_calls_without_any_id() {
        let call = |index: Value, name: &str, args: &str| json!({ "index": index, "id": "", "function": { "name": name, "arguments": args } });
        for index in [json!(0), Value::Null] {
            let got = indexes(&[
                call(index.clone(), "Read", "{\"p\":1}"),
                call(index.clone(), "Grep", "{\"q\":2}"),
                call(index.clone(), "Grep", "{\"q\":3}"),
            ]);
            assert_eq!(got, [json!(0), json!(1), json!(2)], "index {index}");
        }
    }

    /// Nome repetido em chunk de continuação é a mesma chamada, mesmo com o JSON
    /// completo seguido de chunk sem argumentos.
    #[test]
    fn tool_indexer_keeps_repeated_name_as_continuation() {
        let call = |name: &str, args: &str| json!({ "index": 0, "function": { "name": name, "arguments": args } });
        let got = indexes(&[
            call("Read", "{\"p\""),
            call("Read", ":1}"),
            call("Read", ""),
            call("", ""),
        ]);
        assert_eq!(got, [json!(0), json!(0), json!(0), json!(0)]);
    }

    #[test]
    fn tool_indexer_tolerates_repeated_id_and_late_id() {
        let repeated_id = [
            json!({ "index": 0, "id": "c1" }),
            json!({ "index": 0, "id": "c1" }),
        ];
        assert_eq!(indexes(&repeated_id), [json!(0), json!(0)]);
        let late_id = [
            json!({ "index": 0, "function": { "name": "f" } }),
            json!({ "index": 0, "id": "c1" }),
            json!({ "index": 1, "id": "" }),
        ];
        assert_eq!(indexes(&late_id), [json!(0), json!(0), json!(1)]);
    }

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
