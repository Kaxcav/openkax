//! Superfície HTTP (`axum`) e despacho com fallback pré-1º-token.
//!
//! Contrato observável que os testes de `tests/fallback.rs` exercitam:
//! - `POST /v1/chat/completions` com `model = "kax/<classe>"` roteia pelo scheduler.
//! - Streaming: os headers só saem depois do 1º token do candidato escolhido;
//!   erros antes disso caem no próximo candidato sem o cliente perceber.
//! - Header `x-openkax-provider` identifica o provider que respondeu.
//! - Morte pós-1º-token: chunk final com `finish_reason: "error"` + `data: [DONE]`.
//! - Nenhum candidato disponível: `429` com `Retry-After` (segundos, arredondado pra cima).

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use tokio::net::TcpListener;

use crate::config::Config;
use crate::ledger::LedgerHandle;
use crate::provider::{ProviderClient, Secrets};
use crate::scheduler::{Clock, Scheduler, SystemClock};

/// Nome do header de resposta com o provider que atendeu.
pub const PROVIDER_HEADER: &str = "x-openkax-provider";

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
    secrets: Secrets,
    scheduler: Scheduler,
    client: ProviderClient,
    ledger: LedgerHandle,
}

impl AppState {
    pub fn new(deps: Deps) -> Result<Self, StartupError> {
        install_crypto_provider();
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(
                deps.config.defaults.request_timeout_ms,
            ))
            .build()?;
        let scheduler = Scheduler::new(&deps.config, deps.clock, &|env| {
            deps.secrets.get(env).is_some()
        });
        Ok(Self {
            inner: Arc::new(Inner {
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

    pub fn secrets(&self) -> &Secrets {
        &self.inner.secrets
    }

    pub fn client(&self) -> &ProviderClient {
        &self.inner.client
    }

    pub fn ledger(&self) -> &LedgerHandle {
        &self.inner.ledger
    }
}

/// rustls sem provider padrão (ver ADR 0002): instala `ring` uma vez por processo.
pub fn install_crypto_provider() {
    // Erro = já instalado (outra instância no mesmo processo, ex.: testes).
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/models", get(not_implemented))
        .route("/quota", get(not_implemented))
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

async fn chat_completions() -> impl IntoResponse {
    // CONTRACT: dispatcher com fallback (plano do scheduler, Fase 0).
    not_implemented().await
}

async fn not_implemented() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        "não implementado (CONTRACT da Fase 0)",
    )
}
