//! Cliente upstream genérico OpenAI-compat.
//!
//! Um único adapter para todos os providers; diferenças vêm de
//! [`config::Provider`] (URL, auth, `extra_headers`, `quirks`). Este módulo
//! também é dono da **classificação de erro** que o scheduler usa para decidir
//! cooldown/desabilitação, e da definição de "1º token" (PLANO_MESTRE §3.4).

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use futures::stream::BoxStream;

use crate::config::{Auth, Config};
use crate::translate::openai::{ChatCompletionChunk, ChatCompletionRequest};

/// Chaves resolvidas a partir do ambiente. Nunca aparecem em `Debug`/logs.
#[derive(Clone, Default)]
pub struct Secrets {
    by_env: HashMap<String, String>,
}

impl Secrets {
    /// Lê do ambiente cada `auth.env` declarado na config. Vazio = ausente.
    pub fn from_env(config: &Config) -> Self {
        Self::from_pairs(config.providers.iter().filter_map(|p| match &p.auth {
            Auth::Bearer { env } => std::env::var(env).ok().map(|v| (env.clone(), v)),
            Auth::Passthrough | Auth::None => None,
        }))
    }

    /// Para testes e para fontes que não sejam o ambiente do processo.
    pub fn from_pairs<K: Into<String>, V: Into<String>>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Self {
        let by_env = pairs
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .filter(|(_, v)| !v.trim().is_empty())
            .collect();
        Self { by_env }
    }

    pub fn get(&self, env: &str) -> Option<&str> {
        self.by_env.get(env).map(String::as_str)
    }
}

impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut names: Vec<_> = self.by_env.keys().collect();
        names.sort();
        f.debug_struct("Secrets")
            .field("present", &names)
            .finish_non_exhaustive()
    }
}

/// Falha de uma tentativa upstream, já classificada.
///
/// Mapeamento → ação do scheduler (detalhado no plano do scheduler):
/// - [`RequestFault`](Self::RequestFault): pula o candidato, **sem** cooldown.
/// - [`Auth`](Self::Auth): provider `Disabled` até reload.
/// - [`RateLimited`](Self::RateLimited): cooldown honrando `Retry-After`.
/// - [`Server`](Self::Server) / [`Timeout`](Self::Timeout) / [`Network`](Self::Network):
///   cooldown da tabela.
/// - [`OtherClient`](Self::OtherClient): cooldown `on_4xx_other_s`.
/// - [`MidStream`](Self::MidStream): sem fallback; encerra o stream do cliente.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpstreamError {
    /// 400/413/422 que é culpa da request (contexto, parâmetro).
    #[error("request rejeitada pelo upstream (HTTP {status})")]
    RequestFault { status: u16 },
    #[error("credencial recusada (HTTP {status})")]
    Auth { status: u16 },
    #[error("rate limited (retry_after={retry_after:?})")]
    RateLimited { retry_after: Option<Duration> },
    #[error("erro do servidor upstream (HTTP {status})")]
    Server { status: u16 },
    #[error("timeout antes do 1º token")]
    FirstTokenTimeout,
    #[error("timeout total da request")]
    RequestTimeout,
    #[error("outro erro 4xx (HTTP {status})")]
    OtherClient { status: u16 },
    #[error("erro de rede: {0}")]
    Network(String),
    /// Stream morreu depois do 1º token (EOF sem `finish_reason`/`[DONE]`,
    /// evento de erro, JSON inválido).
    #[error("stream interrompido após o 1º token: {0}")]
    MidStream(String),
}

/// Stream upstream que **já passou** do 1º token.
///
/// `prelude` guarda os chunks vistos até (e incluindo) o primeiro que conta
/// como token, para serem reenviados ao cliente em ordem.
pub struct StartedStream {
    pub prelude: Vec<ChatCompletionChunk>,
    pub rest: BoxStream<'static, Result<ChatCompletionChunk, UpstreamError>>,
}

/// Cliente HTTP compartilhado (um `reqwest::Client` para todos os providers).
#[derive(Clone)]
pub struct ProviderClient {
    http: reqwest::Client,
}

impl ProviderClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Abre um stream em `provider` com `model` e espera até o 1º token,
    /// respeitando `first_token_timeout`. Qualquer falha aqui é elegível
    /// para fallback.
    pub async fn start_stream(
        &self,
        _provider: &crate::config::Provider,
        _api_key: Option<&str>,
        _model: &str,
        _request: &ChatCompletionRequest,
        _first_token_timeout: Duration,
    ) -> Result<StartedStream, UpstreamError> {
        // CONTRACT: implementação junto com o dispatcher (Fase 0).
        Err(UpstreamError::Network(
            "start_stream ainda não implementado".into(),
        ))
    }
}

/// "1º token" = primeiro delta com `content` não vazio **ou** `tool_calls`.
/// Chunk só com `role`, keepalive e comentário SSE não contam.
pub fn is_first_token(_chunk: &ChatCompletionChunk) -> bool {
    // CONTRACT: implementação com testes unitários antes (Fase 0).
    false
}

/// Classifica uma resposta HTTP não-2xx do upstream.
pub fn classify_status(_status: u16, _headers: &reqwest::header::HeaderMap) -> UpstreamError {
    // CONTRACT: implementação com testes unitários antes (Fase 0).
    UpstreamError::Network("classify_status ainda não implementado".into())
}
