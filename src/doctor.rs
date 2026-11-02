//! `openkax doctor`: valida providers habilitados sem subir o gateway.
//!
//! Por provider: catálogo (`GET {base_url}/models`; estilo Cloudflare, cujo
//! `base_url` termina em `/ai/v1`, usa `/ai/models/search`) e, com `--chat`, um
//! chat mínimo por modelo habilitado pelo mesmo cliente de provider do gateway
//! (quirks inclusos). Do ambiente só se usa a presença das variáveis: nenhum
//! valor de chave vai para a saída.

use std::time::Duration;

use reqwest::header::CONTENT_TYPE;
use serde::Serialize;
use serde_json::Value;

use crate::config::{Auth, Config, Provider};
use crate::provider::{
    ProviderClient, Secrets, build_http_client, limit_headers as limit_header_list,
};
use crate::translate::openai::{ChatCompletionRequest, ChatMessage, MessageContent};

/// Prazo do GET de catálogo.
const CATALOG_TIMEOUT: Duration = Duration::from_secs(25);
/// `max_tokens` do chat mínimo.
const CHAT_MAX_TOKENS: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Provider desabilitado: não conta.
    Skip,
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn tag(self) -> &'static str {
        match self {
            Status::Skip => "SKIP",
            Status::Ok => "OK",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Faz 1 chat mínimo por modelo habilitado (gasta cota).
    pub chat: bool,
    /// Restringe a um provider.
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    /// `catalog`, `upstream:<id>` ou `chat:<modelo>`.
    pub name: String,
    pub status: Status,
    pub detail: String,
    /// Tempo até o 1º token (só nos chats).
    pub ttft_ms: Option<u64>,
    /// Headers de limite da resposta (`x-ratelimit-*`, `RateLimit*`, `retry-after`).
    pub headers: Vec<(String, String)>,
}

impl Check {
    fn new(name: impl Into<String>, status: Status, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status,
            detail: detail.into(),
            ttft_ms: None,
            headers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderReport {
    pub id: String,
    pub status: Status,
    /// Modelos no catálogo, quando ele respondeu JSON válido.
    pub catalog_models: Option<usize>,
    pub checks: Vec<Check>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub providers: Vec<ProviderReport>,
}

impl Report {
    /// Algum provider habilitado deu FAIL (exit code ≠ 0).
    pub fn failed(&self) -> bool {
        self.providers.iter().any(|p| p.status == Status::Fail)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for p in &self.providers {
            let models = p
                .catalog_models
                .map(|n| format!(" ({n} modelos no catálogo)"))
                .unwrap_or_default();
            out.push_str(&format!("[{}] {}{models}\n", p.status.tag(), p.id));
            for c in &p.checks {
                let ttft = c
                    .ttft_ms
                    .map(|t| format!(" ttft={t}ms"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "    [{}] {}{ttft} {}\n",
                    c.status.tag(),
                    c.name,
                    c.detail
                ));
                for (k, v) in &c.headers {
                    out.push_str(&format!("        | {k}: {v}\n"));
                }
            }
        }
        out
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DoctorError {
    #[error("provider desconhecido: {0}")]
    UnknownProvider(String),
    #[error("falha ao criar cliente HTTP: {0}")]
    Http(String),
}

/// Roda o doctor sobre `config`. `secrets` traz as variáveis de ambiente
/// referenciadas (chave Bearer e `${VAR}` do `base_url`).
pub async fn run(
    config: &Config,
    secrets: &Secrets,
    opts: &Options,
) -> Result<Report, DoctorError> {
    if let Some(id) = &opts.provider
        && config.provider(id).is_none()
    {
        return Err(DoctorError::UnknownProvider(id.clone()));
    }
    let http = build_http_client(Duration::from_secs(60))
        .map_err(|e| DoctorError::Http(e.without_url().to_string()))?;
    let client = ProviderClient::new(http);
    let mut providers = Vec::new();
    for p in &config.providers {
        if opts.provider.as_deref().is_some_and(|id| id != p.id) {
            continue;
        }
        providers.push(check_provider(config, secrets, &client, p, opts).await);
    }
    Ok(Report { providers })
}

async fn check_provider(
    config: &Config,
    secrets: &Secrets,
    client: &ProviderClient,
    provider: &Provider,
    opts: &Options,
) -> ProviderReport {
    let mut rep = ProviderReport {
        id: provider.id.clone(),
        status: Status::Ok,
        catalog_models: None,
        checks: Vec::new(),
    };
    let finish = |mut rep: ProviderReport| {
        rep.status = rep
            .checks
            .iter()
            .map(|c| c.status)
            .max()
            .unwrap_or(Status::Ok);
        rep
    };
    if !provider.enabled {
        rep.status = Status::Skip;
        rep.checks
            .push(Check::new("config", Status::Skip, "desabilitado"));
        return rep;
    }
    if matches!(provider.auth, Auth::Passthrough) {
        rep.status = Status::Skip;
        rep.checks.push(Check::new(
            "config",
            Status::Skip,
            "auth passthrough: sem chave própria para validar",
        ));
        return rep;
    }
    let key = match &provider.auth {
        Auth::Bearer { env } => match secrets.get(env) {
            Some(k) => Some(k),
            None => {
                rep.checks.push(Check::new(
                    "config",
                    Status::Fail,
                    format!("variável ausente: {env}"),
                ));
                return finish(rep);
            }
        },
        _ => None,
    };
    let resolved = match provider.resolve_env(&|n| secrets.get(n).map(str::to_owned)) {
        Ok(p) => p,
        Err(missing) => {
            rep.checks.push(Check::new(
                "config",
                Status::Fail,
                format!("variável ausente: {}", missing.join(", ")),
            ));
            return finish(rep);
        }
    };

    let (catalog, ids) = check_catalog(client, &resolved, key).await;
    let catalog_ok = catalog.status != Status::Fail;
    rep.checks.push(catalog);
    if let Some(ids) = ids {
        rep.catalog_models = Some(ids.len());
        for up in routed_upstreams(config, &resolved) {
            if !ids.contains(&up) {
                rep.checks.push(Check::new(
                    format!("upstream:{up}"),
                    Status::Warn,
                    format!("{up} usado nas rotas mas ausente do catálogo"),
                ));
            }
        }
    }
    // Sem credencial válida o chat só gastaria cota para falhar igual.
    if opts.chat && catalog_ok {
        for m in &resolved.models {
            rep.checks
                .push(check_chat(config, client, &resolved, key, &m.id).await);
        }
    }
    finish(rep)
}

/// Upstreams dos modelos de `provider` que alguma classe usa na rota.
fn routed_upstreams(config: &Config, provider: &Provider) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for step in config
        .classes
        .iter()
        .flat_map(|c| &c.route)
        .filter(|s| s.provider == provider.id)
    {
        let Some(m) = provider.model(&step.model) else {
            continue;
        };
        let up = m.upstream.clone().unwrap_or_else(|| m.id.clone());
        if !out.contains(&up) {
            out.push(up);
        }
    }
    out
}

/// URL do catálogo: `/models`, ou `/ai/models/search` quando o `base_url`
/// termina em `/ai/v1` (estilo Cloudflare).
fn catalog_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    match base.strip_suffix("/ai/v1") {
        Some(root) => format!("{root}/ai/models/search"),
        None => format!("{base}/models"),
    }
}

async fn check_catalog(
    client: &ProviderClient,
    provider: &Provider,
    key: Option<&str>,
) -> (Check, Option<Vec<String>>) {
    let mut rb = client
        .http()
        .get(catalog_url(&provider.base_url))
        .timeout(CATALOG_TIMEOUT);
    if let Some(k) = key {
        rb = rb.bearer_auth(k);
    }
    for (k, v) in &provider.extra_headers {
        rb = rb.header(k, v);
    }
    let resp = match rb.send().await {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("erro de rede: {}", e.without_url());
            return (Check::new("catalog", Status::Fail, msg), None);
        }
    };
    let status = resp.status();
    let headers = limit_header_list(resp.headers());
    let ctype = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let body = resp.text().await.unwrap_or_default();
    let check = |st: Status, detail: String| {
        let mut c = Check::new("catalog", st, detail);
        c.headers = headers.clone();
        c
    };
    let code = status.as_u16();
    if matches!(code, 401 | 403) {
        return (
            check(Status::Fail, format!("HTTP {code}: credencial recusada")),
            None,
        );
    }
    if matches!(code, 404 | 405) {
        return (
            check(Status::Warn, format!("HTTP {code}: provider sem catálogo")),
            None,
        );
    }
    if !status.is_success() {
        return (check(Status::Fail, format!("HTTP {code}")), None);
    }
    if !ctype.contains("json") {
        let ctype = if ctype.is_empty() {
            "sem content-type"
        } else {
            &ctype
        };
        return (
            check(
                Status::Fail,
                format!("HTTP {code} mas {ctype} (não é JSON)"),
            ),
            None,
        );
    }
    match serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| catalog_ids(&v))
    {
        Some(ids) => (check(Status::Ok, format!("HTTP {code}")), Some(ids)),
        None => (
            check(
                Status::Fail,
                format!("HTTP {code} com JSON sem lista de modelos"),
            ),
            None,
        ),
    }
}

/// Ids de um catálogo: lista na raiz ou em `data`/`models`/`result`; item é
/// string ou objeto com `id`/`name`. O prefixo `models/` (Gemini) é removido.
fn catalog_ids(v: &Value) -> Option<Vec<String>> {
    let list = match v {
        Value::Array(a) => a,
        Value::Object(o) => ["data", "models", "result"]
            .iter()
            .find_map(|k| o.get(*k)?.as_array())?,
        _ => return None,
    };
    Some(
        list.iter()
            .filter_map(|it| {
                let s = match it {
                    Value::String(s) => s.as_str(),
                    Value::Object(o) => o.get("id").or_else(|| o.get("name"))?.as_str()?,
                    _ => return None,
                };
                Some(s.strip_prefix("models/").unwrap_or(s).to_owned())
            })
            .collect(),
    )
}

async fn check_chat(
    config: &Config,
    client: &ProviderClient,
    provider: &Provider,
    key: Option<&str>,
    model_id: &str,
) -> Check {
    let name = format!("chat:{model_id}");
    let Some(model) = provider.model(model_id) else {
        return Check::new(name, Status::Fail, "modelo não encontrado");
    };
    let upstream = model.upstream.as_deref().unwrap_or(&model.id);
    let request = ChatCompletionRequest {
        model: upstream.to_owned(),
        messages: vec![ChatMessage {
            role: "user".into(),
            content: Some(MessageContent::Text("ping".into())),
            extra: Default::default(),
        }],
        stream: Some(true),
        max_tokens: Some(CHAT_MAX_TOKENS),
        extra: Default::default(),
    };
    let (ft, ft_reasoning) = config.defaults.first_token_timeouts(Some(model));
    let t0 = tokio::time::Instant::now();
    let attempt = client
        .start_stream(
            provider,
            key,
            upstream,
            Some(CHAT_MAX_TOKENS),
            &request,
            ft,
            ft_reasoning,
        )
        .await;
    let ttft = t0.elapsed().as_millis() as u64;
    let (status, mut detail) = match &attempt.result {
        Ok(_) => (Status::Ok, "ok".to_owned()),
        Err(e) => (Status::Fail, e.to_string()),
    };
    let r = &attempt.remaining;
    let num = |v: Option<i64>| v.map_or_else(|| "-".to_owned(), |n| n.to_string());
    if r.rpm.is_some() || r.tpm.is_some() || r.rpd.is_some() || r.tpd.is_some() {
        detail.push_str(&format!(
            "; restante rpm={} tpm={} rpd={} tpd={}",
            num(r.rpm),
            num(r.tpm),
            num(r.rpd),
            num(r.tpd)
        ));
    }
    if let Some(w) = &r.window {
        detail.push_str(&format!("; janela r={} reset={}s", w.remaining, w.reset_s));
        if let (Some(q), Some(ws)) = (w.quota, w.window_s) {
            detail.push_str(&format!(" ({q} req/{ws}s)"));
        }
    }
    let mut check = Check::new(name, status, detail);
    check.ttft_ms = attempt.result.is_ok().then_some(ttft);
    check.headers = attempt.limit_headers;
    check
}
