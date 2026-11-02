//! Cliente upstream genérico OpenAI-compat.
//!
//! Um único adapter para todos os providers; diferenças vêm de
//! [`config::Provider`] (URL, auth, `extra_headers`, `quirks`). Este módulo
//! também é dono da **classificação de erro** que o scheduler usa para decidir
//! cooldown/desabilitação, e da definição de "1º token" (PLANO_MESTRE §3.4).

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use reqwest::header::{ACCEPT, HeaderMap, RETRY_AFTER};
use serde_json::Value;

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
    /// 400/413/422 que é culpa da request (contexto, parâmetro). `message` é o
    /// corpo do upstream truncado (vai ao cliente, nunca ao log).
    #[error("request rejeitada pelo upstream (HTTP {status})")]
    RequestFault { status: u16, message: String },
    #[error("credencial recusada (HTTP {status})")]
    Auth { status: u16 },
    /// 404: modelo inexistente → só o modelo é desabilitado.
    #[error("modelo não encontrado (HTTP {status})")]
    NotFound { status: u16 },
    /// `daily`: o corpo indica cota diária esgotada.
    #[error("rate limited (retry_after={retry_after:?}, daily={daily})")]
    RateLimited {
        retry_after: Option<Duration>,
        daily: bool,
    },
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
    /// Resposta 2xx inválida antes do 1º token (EOF, evento de erro, JSON inválido).
    #[error("erro de protocolo antes do 1º token: {0}")]
    Protocol(String),
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

/// Teto de bytes lidos de um corpo de erro.
const ERROR_BODY_CAP: usize = 8 * 1024;
/// Teto da mensagem de `RequestFault` repassada ao cliente.
const FAULT_MESSAGE_CAP: usize = 2048;

impl ProviderClient {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Abre um stream em `provider` com o modelo real e espera até o 1º token
    /// (ou um `finish_reason`), respeitando `first_token_timeout`. Qualquer
    /// falha aqui é elegível para fallback.
    pub async fn start_stream(
        &self,
        provider: &crate::config::Provider,
        api_key: Option<&str>,
        upstream_model: &str,
        max_tokens_override: Option<u32>,
        request: &ChatCompletionRequest,
        first_token_timeout: Duration,
    ) -> Result<StartedStream, UpstreamError> {
        let url = format!(
            "{}/chat/completions",
            provider.base_url.trim_end_matches('/')
        );
        let body = build_upstream_body(provider, request, upstream_model, max_tokens_override);
        let mut rb = self
            .http
            .post(url)
            .header(ACCEPT, "text/event-stream")
            .json(&body);
        if let Some(key) = api_key {
            rb = rb.bearer_auth(key);
        }
        for (k, v) in &provider.extra_headers {
            rb = rb.header(k, v);
        }

        let attempt = async move {
            let resp = rb.send().await.map_err(map_reqwest)?;
            let status = resp.status();
            if !status.is_success() {
                let headers = resp.headers().clone();
                let body = read_capped(resp, ERROR_BODY_CAP).await;
                return Err(classify_status(
                    status.as_u16(),
                    &headers,
                    &body,
                    SystemTime::now(),
                ));
            }
            let mut events = SseEvents::new(resp.bytes_stream().boxed());
            let mut prelude = Vec::new();
            loop {
                match events.next().await {
                    None => return Err(UpstreamError::Protocol("EOF antes do 1º token".into())),
                    Some(Err(e)) => return Err(UpstreamError::Protocol(e)),
                    Some(Ok(Event::Done)) => {
                        return Err(UpstreamError::Protocol("[DONE] antes do 1º token".into()));
                    }
                    Some(Ok(Event::Chunk(c))) => {
                        let first = is_first_token(&c);
                        let finished = has_finish(&c);
                        prelude.push(c);
                        if first || finished {
                            return Ok(StartedStream {
                                prelude,
                                rest: events.into_rest(finished),
                            });
                        }
                    }
                }
            }
        };
        match tokio::time::timeout(first_token_timeout, attempt).await {
            Ok(r) => r,
            Err(_) => Err(UpstreamError::FirstTokenTimeout),
        }
    }
}

fn map_reqwest(e: reqwest::Error) -> UpstreamError {
    if e.is_timeout() {
        UpstreamError::RequestTimeout
    } else {
        // Sem URL completa nem headers: só a categoria do erro.
        UpstreamError::Network(e.without_url().to_string())
    }
}

async fn read_capped(resp: reqwest::Response, cap: usize) -> String {
    let mut out = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(Ok(chunk)) = stream.next().await {
        out.extend_from_slice(&chunk);
        if out.len() >= cap {
            out.truncate(cap);
            break;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn has_finish(c: &ChatCompletionChunk) -> bool {
    c.choices.iter().any(|ch| ch.finish_reason.is_some())
}

enum Event {
    Chunk(ChatCompletionChunk),
    Done,
}

/// Eventos SSE de um corpo de resposta, já decodificados.
struct SseEvents {
    body: BoxStream<'static, reqwest::Result<Bytes>>,
    decoder: SseDecoder,
    pending: VecDeque<String>,
    eof: bool,
}

impl SseEvents {
    fn new(body: BoxStream<'static, reqwest::Result<Bytes>>) -> Self {
        Self {
            body,
            decoder: SseDecoder::default(),
            pending: VecDeque::new(),
            eof: false,
        }
    }

    async fn next(&mut self) -> Option<Result<Event, String>> {
        loop {
            if let Some(data) = self.pending.pop_front() {
                return Some(parse_event(&data));
            }
            if self.eof {
                return None;
            }
            match self.body.next().await {
                Some(Ok(bytes)) => self.pending.extend(self.decoder.push(&bytes)),
                Some(Err(e)) => {
                    self.eof = true;
                    return Some(Err(map_reqwest(e).to_string()));
                }
                None => self.eof = true,
            }
        }
    }

    /// Resto do stream após o 1º token. EOF sem `[DONE]` e sem
    /// `finish_reason` = morte no meio.
    fn into_rest(
        self,
        saw_finish: bool,
    ) -> BoxStream<'static, Result<ChatCompletionChunk, UpstreamError>> {
        futures::stream::unfold(
            (self, saw_finish, false),
            |(mut ev, mut finished, ended)| async move {
                if ended {
                    return None;
                }
                match ev.next().await {
                    None if finished => None,
                    None => Some((
                        Err(UpstreamError::MidStream(
                            "EOF sem [DONE] nem finish_reason".into(),
                        )),
                        (ev, finished, true),
                    )),
                    Some(Ok(Event::Done)) => None,
                    Some(Ok(Event::Chunk(c))) => {
                        finished |= has_finish(&c);
                        Some((Ok(c), (ev, finished, false)))
                    }
                    Some(Err(e)) => Some((Err(UpstreamError::MidStream(e)), (ev, finished, true))),
                }
            },
        )
        .boxed()
    }
}

fn parse_event(data: &str) -> Result<Event, String> {
    if data.trim() == "[DONE]" {
        return Ok(Event::Done);
    }
    let v: Value =
        serde_json::from_str(data).map_err(|_| "evento SSE com JSON inválido".to_owned())?;
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("erro sem mensagem");
        return Err(format!(
            "evento de erro do upstream: {}",
            truncate(msg, 200)
        ));
    }
    serde_json::from_value(v)
        .map(Event::Chunk)
        .map_err(|_| "chunk SSE fora do formato".to_owned())
}

/// "1º token" = primeiro delta com `content` não vazio **ou** `tool_calls`.
/// Chunk só com `role`, keepalive e comentário SSE não contam.
pub fn is_first_token(chunk: &ChatCompletionChunk) -> bool {
    chunk.choices.iter().any(|c| {
        c.delta.content.as_deref().is_some_and(|t| !t.is_empty())
            || c.delta.tool_calls.as_ref().is_some_and(|t| !t.is_empty())
    })
}

/// Classifica uma resposta HTTP não-2xx do upstream.
pub fn classify_status(
    status: u16,
    headers: &HeaderMap,
    body: &str,
    utc_now: SystemTime,
) -> UpstreamError {
    match status {
        400 | 413 | 422 => UpstreamError::RequestFault {
            status,
            message: truncate(body, FAULT_MESSAGE_CAP).to_owned(),
        },
        401 | 403 => UpstreamError::Auth { status },
        404 => UpstreamError::NotFound { status },
        408 => UpstreamError::RequestTimeout,
        429 => UpstreamError::RateLimited {
            retry_after: headers
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| parse_retry_after(v, utc_now)),
            daily: mentions_daily_quota(body),
        },
        500..=599 => UpstreamError::Server { status },
        400..=499 => UpstreamError::OtherClient { status },
        _ => UpstreamError::Protocol(format!("status inesperado HTTP {status}")),
    }
}

fn mentions_daily_quota(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    ["per day", "perday", "per-day", "daily", "(rpd)", "(tpd)"]
        .iter()
        .any(|k| b.contains(k))
}

/// `Retry-After`: segundos ou HTTP-date (IMF-fixdate). Data no passado → 0.
fn parse_retry_after(v: &str, utc_now: SystemTime) -> Option<Duration> {
    let v = v.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = parse_http_date(v)?;
    Some(at.duration_since(utc_now).unwrap_or(Duration::ZERO))
}

/// `Sun, 06 Nov 1994 08:49:37 GMT`.
fn parse_http_date(v: &str) -> Option<SystemTime> {
    let parts: Vec<&str> = v.split_whitespace().collect();
    let [_, day, mon, year, hms, "GMT"] = parts.as_slice() else {
        return None;
    };
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| m == mon)? as i64
        + 1;
    let day: i64 = day.parse().ok()?;
    let year: i64 = year.parse().ok()?;
    let mut t = hms.split(':').map(|x| x.parse::<i64>().ok());
    let (h, m, s) = (t.next()??, t.next()??, t.next()??);
    if !(1..=31).contains(&day) || h > 23 || m > 59 || s > 60 {
        return None;
    }
    let secs = days_from_civil(year, month, day) * 86_400 + h * 3600 + m * 60 + s;
    u64::try_from(secs)
        .ok()
        .map(|s| UNIX_EPOCH + Duration::from_secs(s))
}

/// Dias desde 1970-01-01 (algoritmo de Howard Hinnant).
pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Corta em `max` bytes respeitando fronteira de char.
fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Decoder SSE incremental: devolve o `data` de cada evento completo.
/// Comentários (`:`), `event:`/`id:` e eventos sem `data` são ignorados.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
}

impl SseDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf
            .extend(bytes.iter().copied().filter(|b| *b != b'\r'));
        let mut out = Vec::new();
        while let Some(pos) = self.buf.windows(2).position(|w| w == b"\n\n") {
            let event: Vec<u8> = self.buf.drain(..pos + 2).collect();
            let text = String::from_utf8_lossy(&event[..pos]);
            let data: Vec<&str> = text
                .lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(|d| d.strip_prefix(' ').unwrap_or(d))
                .collect();
            if !data.is_empty() {
                out.push(data.join("\n"));
            }
        }
        out
    }
}

/// Corpo enviado ao upstream: modelo real, `stream: true`, usage, clamp e
/// quirks (`strip_params` por último, para poder remover qualquer coisa).
pub fn build_upstream_body(
    provider: &crate::config::Provider,
    request: &ChatCompletionRequest,
    upstream_model: &str,
    max_tokens_override: Option<u32>,
) -> Value {
    use crate::config::ToolChoiceQuirk;

    let mut v = serde_json::to_value(request).unwrap_or(Value::Null);
    let Some(obj) = v.as_object_mut() else {
        return v;
    };
    let q = &provider.quirks;
    obj.insert("model".into(), upstream_model.into());
    obj.insert("stream".into(), true.into());

    if q.no_stream_usage {
        obj.remove("stream_options");
    } else {
        let so = obj
            .entry("stream_options")
            .or_insert_with(|| Value::Object(Default::default()));
        if let Some(so) = so.as_object_mut() {
            so.insert("include_usage".into(), true.into());
        }
    }

    if let Some(n) = max_tokens_override {
        let has_mct = obj.contains_key("max_completion_tokens");
        if has_mct {
            obj.insert("max_completion_tokens".into(), n.into());
        }
        if obj.contains_key("max_tokens") || !has_mct {
            obj.insert("max_tokens".into(), n.into());
        }
    }

    if q.tool_choice == Some(ToolChoiceQuirk::AutoOnly)
        && let Some(tc) = obj.get("tool_choice").cloned()
    {
        if tc == "none" {
            obj.remove("tool_choice");
            obj.remove("tools");
        } else if tc != "auto" {
            obj.insert("tool_choice".into(), "auto".into());
        }
    }

    for p in &q.strip_params {
        obj.remove(p);
    }
    v
}

/// rustls sem provider padrão (ADR 0002): instala `ring` uma vez por processo.
pub fn install_crypto_provider() {
    // Erro = já instalado.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// `reqwest::Client` com rustls+ring (instala o crypto provider antes).
pub fn build_http_client(timeout: Duration) -> reqwest::Result<reqwest::Client> {
    install_crypto_provider();
    reqwest::Client::builder().timeout(timeout).build()
}

#[cfg(test)]
mod tests;
