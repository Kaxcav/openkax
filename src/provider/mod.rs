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
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, RETRY_AFTER};
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
        let mut names: Vec<String> = Vec::new();
        for p in &config.providers {
            if let Auth::Bearer { env } = &p.auth {
                names.push(env.clone());
            }
            names.extend(p.env_refs());
        }
        Self::from_pairs(
            names
                .into_iter()
                .filter_map(|n| std::env::var(&n).ok().map(|v| (n, v))),
        )
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
/// Mapeamento → ação do scheduler (ADR 0003, F0-17):
/// - [`RequestFault`](Self::RequestFault): pula o candidato, **sem** cooldown.
/// - [`Auth`](Self::Auth): **provider** `Disabled` até reload.
/// - [`NotFound`](Self::NotFound) / [`Gone`](Self::Gone): só o **modelo** `Disabled`.
/// - [`Network`](Self::Network): cooldown `on_timeout_s` no **provider**.
/// - [`RateLimited`](Self::RateLimited): cooldown honrando `Retry-After`, no
///   modelo se ele tem `limits` próprios, senão no provider.
/// - [`Server`](Self::Server) / [`Protocol`](Self::Protocol) /
///   [`MidStream`](Self::MidStream) (`on_5xx_s`), timeouts (`on_timeout_s`),
///   [`OtherClient`](Self::OtherClient) (`on_4xx_other_s`): cooldown só no **modelo**.
/// - [`MidStream`](Self::MidStream): além disso, sem fallback; encerra o stream do cliente.
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
    /// 410: modelo descontinuado → só o modelo é desabilitado (`model_gone`).
    #[error("modelo descontinuado (HTTP 410)")]
    Gone,
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

/// Resultado de uma tentativa, com o que os headers de resposta disseram
/// (mesmo quando a tentativa falhou).
pub struct Attempt {
    pub result: Result<StartedStream, UpstreamError>,
    /// Saldo restante pelos headers mapeados em `quirks.ratelimit_headers`.
    pub remaining: crate::scheduler::Remaining,
    /// Headers de limite (`x-ratelimit-*`, `retry-after`, …) para depuração.
    pub limit_headers: Vec<(String, String)>,
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

    /// Abre um stream em `provider` (já com `${VAR}` resolvidas) e espera até o
    /// 1º token (ou um `finish_reason`). Deltas de raciocínio estendem o prazo
    /// até `first_token_timeout_reasoning`, mas não são commit (N4). Qualquer
    /// falha aqui é elegível para fallback.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_stream(
        &self,
        provider: &crate::config::Provider,
        api_key: Option<&str>,
        upstream_model: &str,
        max_tokens_override: Option<u32>,
        request: &ChatCompletionRequest,
        first_token_timeout: Duration,
        first_token_timeout_reasoning: Duration,
    ) -> Attempt {
        let mut attempt = Attempt {
            result: Err(UpstreamError::FirstTokenTimeout),
            remaining: Default::default(),
            limit_headers: Vec::new(),
        };
        let url = format!(
            "{}/chat/completions",
            provider.base_url.trim_end_matches('/')
        );
        let body = build_upstream_body(provider, request, upstream_model, max_tokens_override);
        let mut rb = self
            .http
            .post(url)
            .header(ACCEPT, "text/event-stream")
            .header(CONTENT_TYPE, "application/json")
            .body(encode_body(&body, provider.quirks.ascii_json));
        if let Some(key) = api_key {
            rb = rb.bearer_auth(key);
        }
        for (k, v) in &provider.extra_headers {
            rb = rb.header(k, v);
        }

        let t0 = tokio::time::Instant::now();
        let deadline = t0 + first_token_timeout;
        let resp = match tokio::time::timeout_at(deadline, rb.send()).await {
            Err(_) => return attempt,
            Ok(Err(e)) => {
                attempt.result = Err(map_reqwest(e));
                return attempt;
            }
            Ok(Ok(r)) => r,
        };
        attempt.remaining =
            remaining_from_headers(&provider.quirks.ratelimit_headers, resp.headers());
        attempt.limit_headers = limit_headers(resp.headers());
        let status = resp.status();
        if !status.is_success() {
            let headers = resp.headers().clone();
            let body = tokio::time::timeout_at(deadline, read_capped(resp, ERROR_BODY_CAP))
                .await
                .unwrap_or_default();
            attempt.result = Err(classify_status(
                status.as_u16(),
                &headers,
                &body,
                SystemTime::now(),
            ));
            return attempt;
        }
        let mut events = SseEvents::new(resp.bytes_stream().boxed());
        attempt.result = await_first_token(
            &mut events,
            t0,
            first_token_timeout,
            first_token_timeout_reasoning,
        )
        .await
        .map(|(prelude, finished)| StartedStream {
            prelude,
            rest: events.into_rest(finished),
        });
        attempt
    }
}

/// Lê eventos até o 1º token (ou `finish_reason`). Prazo: `t0 + ft`;
/// estendido para `t0 + ft_reasoning` quando chega raciocínio (sinal de vida).
pub(crate) async fn await_first_token(
    events: &mut SseEvents,
    t0: tokio::time::Instant,
    ft: Duration,
    ft_reasoning: Duration,
) -> Result<(Vec<ChatCompletionChunk>, bool), UpstreamError> {
    let mut deadline = t0 + ft;
    let mut prelude = Vec::new();
    loop {
        let Ok(next) = tokio::time::timeout_at(deadline, events.next()).await else {
            return Err(UpstreamError::FirstTokenTimeout);
        };
        match next {
            None => return Err(UpstreamError::Protocol("EOF antes do 1º token".into())),
            Some(Err(e)) => return Err(UpstreamError::Protocol(e)),
            Some(Ok(Event::Done)) => {
                return Err(UpstreamError::Protocol("[DONE] antes do 1º token".into()));
            }
            Some(Ok(Event::Chunk(c))) => {
                if has_reasoning(&c) {
                    deadline = deadline.max(t0 + ft_reasoning);
                }
                let first = is_first_token(&c);
                let finished = has_finish(&c);
                // Nada chegou ao cliente ainda: é falha do degrau, cabe fallback.
                if !first && has_error_finish(&c) {
                    return Err(UpstreamError::Protocol(
                        "finish_reason: error antes do 1º token".into(),
                    ));
                }
                prelude.push(c);
                if first || finished {
                    return Ok((prelude, finished));
                }
            }
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

fn has_error_finish(c: &ChatCompletionChunk) -> bool {
    c.choices
        .iter()
        .any(|ch| ch.finish_reason.as_deref() == Some("error"))
}

enum Event {
    Chunk(ChatCompletionChunk),
    Done,
}

/// Eventos SSE de um corpo de resposta, já decodificados.
pub(crate) struct SseEvents {
    body: BoxStream<'static, Result<Bytes, String>>,
    decoder: SseDecoder,
    pending: VecDeque<String>,
    eof: bool,
}

impl SseEvents {
    fn new(body: BoxStream<'static, reqwest::Result<Bytes>>) -> Self {
        Self::from_bytes(
            body.map(|r| r.map_err(|e| map_reqwest(e).to_string()))
                .boxed(),
        )
    }

    pub(crate) fn from_bytes(body: BoxStream<'static, Result<Bytes, String>>) -> Self {
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
                    return Some(Err(e));
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
                    // Depois do `finish_reason` a resposta já está completa: erro
                    // aqui não é morte no meio (nem gera um 2º final ao cliente).
                    Some(Err(_)) if finished => None,
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

/// Campos de raciocínio conhecidos nos deltas (DeepSeek/Z.ai: `reasoning_content`;
/// OpenRouter/gpt-oss: `reasoning`).
const REASONING_FIELDS: [&str; 3] = ["reasoning_content", "reasoning", "reasoning_details"];

/// O chunk traz raciocínio (sinal de vida, não commit) — N4.
pub fn has_reasoning(chunk: &ChatCompletionChunk) -> bool {
    chunk.choices.iter().any(|c| {
        REASONING_FIELDS
            .iter()
            .any(|f| match c.delta.extra.get(*f) {
                Some(Value::String(s)) => !s.is_empty(),
                Some(Value::Array(a)) => !a.is_empty(),
                Some(Value::Null) | None => false,
                Some(_) => true,
            })
    })
}

/// Remove o raciocínio dos deltas (não vai ao cliente). Devolve os chars removidos.
pub fn strip_reasoning(chunk: &mut ChatCompletionChunk) -> usize {
    let mut n = 0;
    for c in &mut chunk.choices {
        for f in REASONING_FIELDS {
            if let Some(v) = c.delta.extra.remove(f) {
                n += match v {
                    Value::String(s) => s.chars().count(),
                    other => other.to_string().chars().count(),
                };
            }
        }
    }
    n
}

/// Nada para o cliente: sem role, conteúdo, tool_calls, finish, usage nem outro campo.
pub fn is_empty_chunk(chunk: &ChatCompletionChunk) -> bool {
    chunk.usage.is_none()
        && chunk.choices.iter().all(|c| {
            c.finish_reason.is_none()
                && c.delta.role.is_none()
                && c.delta.content.as_deref().is_none_or(str::is_empty)
                && c.delta.tool_calls.as_ref().is_none_or(Vec::is_empty)
                && c.delta.extra.is_empty()
        })
}

/// Saldo restante pelos headers configurados (valor não numérico é ignorado).
pub fn remaining_from_headers(
    names: &crate::config::RateLimitHeaders,
    headers: &HeaderMap,
) -> crate::scheduler::Remaining {
    let get = |n: &Option<String>| {
        n.as_deref()
            .and_then(|n| headers.get(n))
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<f64>().ok())
            .map(|v| v.floor() as i64)
    };
    crate::scheduler::Remaining {
        rpm: get(&names.rpm),
        tpm: get(&names.tpm),
        rpd: get(&names.rpd),
        tpd: get(&names.tpd),
        window: if names.ietf {
            ietf_window(headers)
        } else {
            None
        },
    }
}

/// Itens de um header IETF (`"nome";k=v;k=v, "outro";…`) → `(nome, params)`.
fn ietf_items(headers: &HeaderMap, name: &str) -> Vec<(String, HashMap<String, String>)> {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|item| {
            let mut parts = item.split(';').map(str::trim);
            let policy = parts.next()?.trim_matches('"').to_owned();
            let params = parts
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
                .collect();
            Some((policy, params))
        })
        .collect()
}

/// `RateLimit` (`r` restante, `t` reset em s) + `RateLimit-Policy` (`q` cota,
/// `w` janela em s), casados pelo nome. Com várias políticas, vale a de menor `r`.
fn ietf_window(headers: &HeaderMap) -> Option<crate::scheduler::WindowObs> {
    let policies = ietf_items(headers, "ratelimit-policy");
    let num = |p: &HashMap<String, String>, k: &str| p.get(k)?.parse::<u64>().ok();
    ietf_items(headers, "ratelimit")
        .into_iter()
        .filter_map(|(name, p)| {
            let remaining = p.get("r")?.parse::<i64>().ok()?;
            let policy = policies.iter().find(|(n, _)| *n == name).map(|(_, q)| q);
            Some(crate::scheduler::WindowObs {
                quota: policy.and_then(|q| num(q, "q")),
                window_s: policy.and_then(|q| num(q, "w")),
                remaining,
                reset_s: num(&p, "t").unwrap_or(0),
            })
        })
        .min_by_key(|w| w.remaining)
}

/// Headers de limite para depuração/calibração. Lista branca por prefixo:
/// nunca auth, cookies ou qualquer outro header.
pub fn limit_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = headers
        .iter()
        .filter(|(k, _)| {
            let k = k.as_str();
            k.starts_with("x-ratelimit-")
                || k.starts_with("ratelimit")
                || k == "retry-after"
                || k.starts_with("x-request-limit")
        })
        .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
        .collect();
    v.sort();
    v
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
        410 => UpstreamError::Gone,
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
        if !so.is_object() {
            *so = Value::Object(Default::default());
        }
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

    if let Some(frag) = &q.disable_thinking
        && let Ok(Value::Object(frag)) = serde_json::to_value(frag)
    {
        for (k, v) in frag {
            obj.insert(k, v);
        }
    }

    if let Some(Value::Array(messages)) = obj.get_mut("messages") {
        for message in messages {
            apply_message_quirks(q, message);
        }
    }

    for p in &q.strip_params {
        obj.remove(p);
    }
    v
}

/// Aviso que substitui a imagem com `strip_images` (ADR 0006).
const IMAGE_OMITTED: &str = "[image omitted: provider has no vision]";
/// Valor que a doc do Gemini aceita em chamada sem assinatura real (ADR 0006).
const SKIP_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

/// Quirks que mexem em cada mensagem do histórico. Forma inesperada passa como
/// veio: o upstream é quem recusa.
fn apply_message_quirks(q: &crate::config::Quirks, message: &mut Value) {
    if q.strip_images
        && let Some(Value::Array(parts)) = message.get_mut("content")
    {
        for part in parts {
            if part.get("type").and_then(Value::as_str) == Some("image_url") {
                *part = serde_json::json!({ "type": "text", "text": IMAGE_OMITTED });
            }
        }
    }

    if let Some(Value::Array(calls)) = message.get_mut("tool_calls") {
        for (i, call) in calls.iter_mut().enumerate() {
            let Some(call) = call.as_object_mut() else {
                continue;
            };
            if !q.thought_signatures {
                call.remove("extra_content");
            } else if i == 0 {
                ensure_thought_signature(call);
            }
            if q.tool_call_id.is_some()
                && let Some(Value::String(id)) = call.get_mut("id")
            {
                *id = alnum9(id);
            }
        }
    }
    if q.tool_call_id.is_some()
        && let Some(Value::String(id)) = message.get_mut("tool_call_id")
    {
        *id = alnum9(id);
    }
}

/// Só a 1ª chamada de um passo precisa da assinatura (doc do Gemini). O que mais
/// houver em `extra_content` fica.
fn ensure_thought_signature(call: &mut serde_json::Map<String, Value>) {
    let extra = call
        .entry("extra_content")
        .or_insert_with(|| serde_json::json!({}));
    if !extra.is_object() {
        *extra = serde_json::json!({});
    }
    let google = &mut extra["google"];
    if !google.is_object() {
        *google = serde_json::json!({});
    }
    if google.get("thought_signature").is_none_or(Value::is_null) {
        google["thought_signature"] = SKIP_THOUGHT_SIGNATURE.into();
    }
}

/// `id` que já é 9 letras ou dígitos passa igual; o resto vira um hash determinístico
/// (FNV-1a, independente da versão do Rust): cada request é reescrita sozinha, e o
/// `id` de uma sessão em andamento tem de dar sempre o mesmo valor.
fn alnum9(id: &str) -> String {
    const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    if id.len() == 9 && id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return id.to_owned();
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in id.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (0..9)
        .map(|_| {
            let digit = ALPHABET[(hash % 62) as usize];
            hash /= 62;
            char::from(digit)
        })
        .collect()
}

/// Serializa o corpo upstream. `ascii` (quirk `ascii_json`): todo char não-ASCII
/// vira `\uXXXX` (par substituto fora do BMP). Fora de strings o JSON do serde
/// é ASCII puro, então escapar char a char preserva o valor.
pub fn encode_body(body: &Value, ascii: bool) -> Vec<u8> {
    let s = serde_json::to_string(body).unwrap_or_default();
    if !ascii || s.is_ascii() {
        return s.into_bytes();
    }
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for u in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out.into_bytes()
}

/// rustls sem provider padrão (ADR 0002): instala `ring` uma vez por processo.
pub fn install_crypto_provider() {
    // Erro = já instalado.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// `reqwest::Client` com rustls+ring (instala o crypto provider antes).
/// Pool enxuto (ADR 0002): conexão ociosa fecha em 30 s e no máximo 2 por host,
/// para a memória privada voltar ao patamar ocioso depois de uma rajada.
pub fn build_passthrough_client() -> reqwest::Result<reqwest::Client> {
    install_crypto_provider();
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(300))
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(2)
        .build()
}

pub fn build_http_client(timeout: Duration) -> reqwest::Result<reqwest::Client> {
    install_crypto_provider();
    reqwest::Client::builder()
        .timeout(timeout)
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(2)
        .build()
}

#[cfg(test)]
mod tests;
