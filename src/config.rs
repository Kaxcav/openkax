//! Configuração declarativa: `providers.toml`.
//!
//! Provider é **dado**, nunca código especializado: tudo que diferencia um
//! provider (URL, auth, limites, quirks, cooldowns) mora aqui. Este módulo só
//! faz parse + validação; estado de runtime (buckets, cooldown) fica no
//! `scheduler`.
//!
//! Limites têm dois escopos (ADR 0003): `[[provider]].limits` é o escopo
//! compartilhado da conta/workspace; `models[].limits` é o escopo do modelo.
//! Qualquer um pode faltar.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Raiz do `providers.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default, rename = "provider")]
    pub providers: Vec<Provider>,
    #[serde(default, rename = "class")]
    pub classes: Vec<Class>,
}

/// Valores globais; cada campo é opcional no TOML.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Defaults {
    /// Teto total de uma request upstream.
    pub request_timeout_ms: u64,
    /// Teto até o primeiro delta de conteúdo; estourou → próximo candidato.
    pub first_token_timeout_ms: u64,
    /// Teto estendido quando o upstream manda deltas de raciocínio antes do
    /// conteúdo (sinal de vida, não commit) — N4.
    pub first_token_timeout_reasoning_ms: u64,
    /// Chute inicial de chars/token (recalibrado por provider via `usage`).
    pub estimate_chars_per_token: f64,
    /// Multiplicador sobre a estimativa de tokens antes de checar buckets.
    pub safety_factor: f64,
    /// Horário (UTC) de reset dos buckets diários quando o escopo não define.
    pub daily_reset_utc: UtcTime,
}

impl Defaults {
    /// `(prazo do 1º token, prazo estendido por raciocínio)` de `model`: o do
    /// modelo quando houver; o estendido nunca fica abaixo do prazo base.
    pub fn first_token_timeouts(&self, model: Option<&Model>) -> (Duration, Duration) {
        let base = model
            .and_then(|m| m.first_token_timeout_ms)
            .unwrap_or(self.first_token_timeout_ms);
        (
            Duration::from_millis(base),
            Duration::from_millis(self.first_token_timeout_reasoning_ms.max(base)),
        )
    }
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            request_timeout_ms: 60_000,
            first_token_timeout_ms: 8_000,
            first_token_timeout_reasoning_ms: 30_000,
            estimate_chars_per_token: 3.6,
            safety_factor: 1.0,
            daily_reset_utc: UtcTime::MIDNIGHT,
        }
    }
}

/// Um provider upstream (OpenAI-compat, ou nativo Anthropic para passthrough).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub id: String,
    pub base_url: String,
    pub auth: Auth,
    /// `false` → ignorado pelo scheduler (ex.: IDs de modelo ainda não verificados).
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<Model>,
    /// Escopo compartilhado (conta/workspace).
    #[serde(default, skip_serializing_if = "Limits::is_unbounded")]
    pub limits: Limits,
    #[serde(default, skip_serializing_if = "Quirks::is_empty")]
    pub quirks: Quirks,
    /// `None` → [`Cooldown::default`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown: Option<Cooldown>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_headers: BTreeMap<String, String>,
    /// `free` → `credit` → `paid`: ordem obrigatória em qualquer rota (N3).
    #[serde(default, skip_serializing_if = "Tier::is_free")]
    pub tier: Tier,
    /// Protocolo nativo; ausente = OpenAI Chat Completions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<Native>,
    /// Sobrescreve [`Defaults::safety_factor`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safety_factor: Option<f64>,
}

impl Provider {
    /// Cooldowns efetivos (TOML ou default).
    pub fn cooldown(&self) -> Cooldown {
        self.cooldown.clone().unwrap_or_default()
    }

    pub fn model(&self, id: &str) -> Option<&Model> {
        self.models.iter().find(|m| m.id == id)
    }

    /// Variáveis `${VAR}` referenciadas em `base_url` e `extra_headers` (N1).
    pub fn env_refs(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for s in std::iter::once(&self.base_url).chain(self.extra_headers.values()) {
            for r in env_refs(s).unwrap_or_default() {
                if !out.contains(&r) {
                    out.push(r);
                }
            }
        }
        out
    }

    /// Cópia com `${VAR}` resolvidas. `Err` lista as variáveis ausentes.
    pub fn resolve_env(
        &self,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, Vec<String>> {
        let mut missing: Vec<String> = Vec::new();
        let mut sub = |s: &str| match interpolate(s, lookup) {
            Ok(v) => v,
            Err(m) => {
                for v in m {
                    if !missing.contains(&v) {
                        missing.push(v);
                    }
                }
                String::new()
            }
        };
        let mut p = self.clone();
        p.base_url = sub(&self.base_url);
        for v in p.extra_headers.values_mut() {
            *v = sub(v);
        }
        if missing.is_empty() {
            Ok(p)
        } else {
            Err(missing)
        }
    }
}

/// Nomes em `${VAR}` de `s`. `Err` = sintaxe inválida (`${` sem `}` ou nome inválido).
pub fn env_refs(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        let after = &rest[i + 2..];
        let Some(end) = after.find('}') else {
            return Err(format!("`${{` sem `}}` em {s:?}"));
        };
        let name = &after[..end];
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("nome de variável inválido {name:?} em {s:?}"));
        }
        out.push(name.to_owned());
        rest = &after[end + 1..];
    }
    Ok(out)
}

/// Substitui `${VAR}` por `lookup(VAR)`. `Err` lista as ausentes.
pub fn interpolate(
    s: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<String, Vec<String>> {
    let refs = env_refs(s).map_err(|e| vec![e])?;
    let mut out = s.to_owned();
    let mut missing = Vec::new();
    for name in refs {
        match lookup(&name) {
            Some(v) => out = out.replace(&format!("${{{name}}}"), &v),
            None => missing.push(name),
        }
    }
    if missing.is_empty() {
        Ok(out)
    } else {
        Err(missing)
    }
}

/// Natureza do custo de um provider.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    #[default]
    Free,
    /// Créditos gratuitos que acabam (Vercel AI Gateway).
    Credit,
    /// Cobra dinheiro (DeepSeek).
    Paid,
}

impl Tier {
    pub fn is_free(&self) -> bool {
        *self == Self::Free
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Credit => "credit",
            Self::Paid => "paid",
        }
    }
}

/// Modelo de um provider: `id` é o nome local usado nas rotas; `upstream` é
/// o ID real enviado ao provider (conferido em `/models`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub id: String,
    /// Obrigatório em provider habilitado.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// Escopo do modelo (ex.: Groq e Gemini limitam por modelo).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Limits>,
    /// Sobrescreve `provider.limits.max_ctx`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ctx: Option<u32>,
    /// Família do modelo ("glm", "qwen", …): filtro `x-openkax-exclude-family` (N5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Sobrescreve [`Defaults::first_token_timeout_ms`] para este modelo (F0-18).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_token_timeout_ms: Option<u64>,
}

/// Como autenticar no upstream. A chave em si nunca está no TOML: só o nome
/// da variável de ambiente.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Auth {
    /// `Authorization: Bearer $env`.
    Bearer { env: String },
    /// Repassa `Authorization`/`x-api-key` do cliente, intocados.
    Passthrough,
    /// Sem auth (provider local, mocks de teste).
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Native {
    Anthropic,
}

/// Limites declarados de um escopo. `None` = sem limite conhecido.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpm: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpd: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpd: Option<u64>,
    /// Só no escopo do provider; no modelo use `models[].max_ctx`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ctx: Option<u32>,
    /// Requests simultâneas no escopo (N2): slot tomado no `reserve`, solto ao fechar a reserva.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<u32>,
    /// Informativo até o snapshot da F0-10 (N6); até lá o pacing é via `rpd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_month: Option<u64>,
    /// Informativo até o snapshot da F0-10 (N6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_per_month: Option<u64>,
    /// Sobrescreve o horário de reset de RPD/TPD deste escopo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_utc: Option<UtcTime>,
}

impl Limits {
    pub fn is_unbounded(&self) -> bool {
        *self == Self::default()
    }

    pub fn has_daily(&self) -> bool {
        self.rpd.is_some() || self.tpd.is_some()
    }
}

/// Diferenças de comportamento do provider tratadas como dado.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quirks {
    /// Parâmetros removidos do corpo antes de enviar.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub strip_params: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoiceQuirk>,
    /// Provider não aceita `stream_options.include_usage`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub no_stream_usage: bool,
    /// Provider conta `max_tokens` no TPM (Groq): reserva entrada + saída.
    #[serde(default, skip_serializing_if = "is_false")]
    pub tpm_counts_max_tokens: bool,
    /// Teto de `max_tokens` enviado ao upstream (clamp).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_cap: Option<u32>,
    /// Fragmento mesclado no corpo para desligar raciocínio — só parâmetro
    /// documentado pelo provider (N4). Ex.: `{ thinking = { type = "disabled" } }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_thinking: Option<toml::Table>,
    /// Headers de resposta com o saldo restante de cada bucket; corrigem o bucket.
    #[serde(default, skip_serializing_if = "RateLimitHeaders::is_empty")]
    pub ratelimit_headers: RateLimitHeaders,
    /// Corpo enviado com todo não-ASCII escapado (`\uXXXX`): mesmo JSON, outra
    /// codificação. Cohere degenera com UTF-8 cru (F0-19).
    #[serde(default, skip_serializing_if = "is_false")]
    pub ascii_json: bool,
}

/// Nome do header com o saldo **restante** de cada bucket.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitHeaders {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpd: Option<String>,
}

impl RateLimitHeaders {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl Quirks {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoiceQuirk {
    /// Provider só aceita `tool_choice: "auto"`; outros valores são reescritos.
    AutoOnly,
}

/// Segundos de cooldown por tipo de erro.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Cooldown {
    /// 429 sem `Retry-After` utilizável.
    pub on_429_s: u64,
    /// 5xx, erro de protocolo, morte pós-1º-token vinda do upstream.
    pub on_5xx_s: u64,
    /// Timeout de 1º token / total, erro de rede.
    pub on_timeout_s: u64,
    /// 4xx que não é culpa da request, nem auth, nem 404.
    pub on_4xx_other_s: u64,
}

impl Default for Cooldown {
    fn default() -> Self {
        Self {
            on_429_s: 60,
            on_5xx_s: 20,
            on_timeout_s: 30,
            on_4xx_other_s: 600,
        }
    }
}

/// Classe de modelo (`kax/*`): o que clientes e o lead enxergam.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Class {
    pub id: String,
    /// Pacote maior que isso é rejeitado antes de qualquer provider.
    pub max_packet_tokens: u32,
    /// Ordem de fallback.
    pub route: Vec<RouteStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteStep {
    pub provider: String,
    /// `models[].id` do provider.
    pub model: String,
}

/// Horário do dia em UTC, serializado como `"HH:MM"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtcTime {
    minute_of_day: u16,
}

impl UtcTime {
    pub const MIDNIGHT: Self = Self { minute_of_day: 0 };

    pub fn new(hour: u8, minute: u8) -> Option<Self> {
        (hour < 24 && minute < 60).then(|| Self {
            minute_of_day: u16::from(hour) * 60 + u16::from(minute),
        })
    }

    /// Segundos desde 00:00 UTC.
    pub fn seconds_of_day(self) -> u32 {
        u32::from(self.minute_of_day) * 60
    }
}

impl fmt::Display for UtcTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02}:{:02}",
            self.minute_of_day / 60,
            self.minute_of_day % 60
        )
    }
}

impl FromStr for UtcTime {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || format!("horário UTC inválido {s:?} (esperado \"HH:MM\")");
        let (h, m) = s.split_once(':').ok_or_else(bad)?;
        if h.len() != 2 || m.len() != 2 {
            return Err(bad());
        }
        let h: u8 = h.parse().map_err(|_| bad())?;
        let m: u8 = m.parse().map_err(|_| bad())?;
        Self::new(h, m).ok_or_else(bad)
    }
}

impl Serialize for UtcTime {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for UtcTime {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("não consegui ler {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("TOML inválido: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("falha ao serializar TOML: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("config inválida:\n  - {}", .0.join("\n  - "))]
    Invalid(Vec<String>),
}

impl Config {
    /// Parse + validação.
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        let cfg: Self = toml::from_str(s)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let s = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_toml_str(&s)
    }

    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string(self)?)
    }

    pub fn provider(&self, id: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.id == id)
    }

    pub fn class(&self, id: &str) -> Option<&Class> {
        self.classes.iter().find(|c| c.id == id)
    }

    /// Checagens semânticas; acumula todos os erros em vez de parar no primeiro.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut errs = Vec::new();
        let d = &self.defaults;
        if d.estimate_chars_per_token.is_nan() || d.estimate_chars_per_token <= 0.0 {
            errs.push("defaults.estimate_chars_per_token deve ser > 0".to_owned());
        }
        if d.safety_factor.is_nan() || d.safety_factor < 1.0 {
            errs.push("defaults.safety_factor deve ser >= 1.0".to_owned());
        }
        if d.first_token_timeout_reasoning_ms < d.first_token_timeout_ms {
            errs.push(
                "defaults: first_token_timeout_reasoning_ms deve ser >= first_token_timeout_ms"
                    .to_owned(),
            );
        }
        if d.first_token_timeout_ms == 0 || d.request_timeout_ms < d.first_token_timeout_ms {
            errs.push(
                "defaults: exige 0 < first_token_timeout_ms <= request_timeout_ms".to_owned(),
            );
        }

        let mut ids = HashSet::new();
        for p in &self.providers {
            validate_provider(p, &mut errs);
            for m in &p.models {
                if let Some(ft) = m.first_token_timeout_ms
                    && (ft == 0 || ft > d.request_timeout_ms)
                {
                    errs.push(format!(
                        "provider {:?}, modelo {:?}: first_token_timeout_ms exige 0 < {ft} <= request_timeout_ms ({})",
                        p.id, m.id, d.request_timeout_ms
                    ));
                }
            }
            if !ids.insert(p.id.as_str()) {
                errs.push(format!("provider duplicado: {:?}", p.id));
            }
        }

        let mut class_ids = HashSet::new();
        for c in &self.classes {
            if !class_ids.insert(c.id.as_str()) {
                errs.push(format!("classe duplicada: {:?}", c.id));
            }
            if !c.id.starts_with("kax/") {
                errs.push(format!("classe {:?}: id deve começar com \"kax/\"", c.id));
            }
            if c.route.is_empty() {
                errs.push(format!("classe {:?}: route vazia", c.id));
            }
            let mut max_tier = Tier::Free;
            for (i, step) in c.route.iter().enumerate() {
                let Some(p) = self.provider(&step.provider) else {
                    errs.push(format!(
                        "classe {:?}: provider desconhecido {:?}",
                        c.id, step.provider
                    ));
                    continue;
                };
                if p.native.is_none() && p.model(&step.model).is_none() {
                    errs.push(format!(
                        "classe {:?}: modelo {:?} não listado em provider {:?}",
                        c.id, step.model, p.id
                    ));
                }
                // Regra 1.1: kax/* só cai na Anthropic como último degrau explícito.
                if p.native == Some(Native::Anthropic) && i + 1 != c.route.len() {
                    errs.push(format!(
                        "classe {:?}: provider nativo Anthropic só pode ser o último degrau",
                        c.id
                    ));
                }
                if p.tier < max_tier {
                    errs.push(format!(
                        "classe {:?}: degrau {:?} ({}) depois de um {} (pago/crédito sempre no fim)",
                        c.id,
                        p.id,
                        p.tier.as_str(),
                        max_tier.as_str()
                    ));
                }
                max_tier = max_tier.max(p.tier);
            }
        }

        if errs.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(errs))
        }
    }
}

fn validate_provider(p: &Provider, errs: &mut Vec<String>) {
    let id = &p.id;
    if !(p.base_url.starts_with("http://") || p.base_url.starts_with("https://")) {
        errs.push(format!("provider {id:?}: base_url deve ser http(s)"));
    }
    if let Auth::Bearer { env } = &p.auth
        && env.trim().is_empty()
    {
        errs.push(format!("provider {id:?}: auth.env vazio"));
    }
    if p.native.is_none() && p.auth == Auth::Passthrough {
        errs.push(format!("provider {id:?}: auth passthrough exige native"));
    }
    if let Some(sf) = p.safety_factor
        && (sf.is_nan() || sf < 1.0)
    {
        errs.push(format!("provider {id:?}: safety_factor deve ser >= 1.0"));
    }
    if p.quirks.max_tokens_cap == Some(0) {
        errs.push(format!("provider {id:?}: quirks.max_tokens_cap = 0"));
    }
    validate_limits(&p.limits, &format!("provider {id:?}: limits"), errs);
    for (what, v) in std::iter::once(("base_url", &p.base_url))
        .chain(p.extra_headers.iter().map(|(k, v)| (k.as_str(), v)))
    {
        if let Err(e) = env_refs(v) {
            errs.push(format!("provider {id:?}: {what}: {e}"));
        }
    }

    let mut model_ids = HashSet::new();
    for m in &p.models {
        let at = format!("provider {id:?}, modelo {:?}", m.id);
        if !model_ids.insert(m.id.as_str()) {
            errs.push(format!("{at}: id duplicado"));
        }
        let upstream_ok = m.upstream.as_deref().is_some_and(|u| !u.trim().is_empty());
        if p.enabled && !upstream_ok {
            errs.push(format!(
                "{at}: `upstream` obrigatório em provider habilitado (confira em /models)"
            ));
        }
        if let Some(l) = &m.limits {
            if l.max_ctx.is_some() {
                errs.push(format!(
                    "{at}: use `max_ctx` do modelo, não `limits.max_ctx`"
                ));
            }
            validate_limits(l, &format!("{at}: limits"), errs);
        }
    }
}

fn validate_limits(l: &Limits, at: &str, errs: &mut Vec<String>) {
    let zero = [
        ("rpm", l.rpm.map(u64::from)),
        ("tpm", l.tpm.map(u64::from)),
        ("rpd", l.rpd.map(u64::from)),
        ("tpd", l.tpd),
        ("max_ctx", l.max_ctx.map(u64::from)),
        ("max_concurrent", l.max_concurrent.map(u64::from)),
    ];
    for (name, v) in zero {
        if v == Some(0) {
            errs.push(format!("{at}.{name} = 0 (desabilite com enabled = false)"));
        }
    }
}

fn yes() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO_TOML: &str = include_str!("../providers.toml");

    fn invalid(toml: &str) -> Vec<String> {
        match Config::from_toml_str(toml) {
            Err(ConfigError::Invalid(e)) => e,
            other => panic!("esperava ConfigError::Invalid, veio {other:?}"),
        }
    }

    #[test]
    fn repo_providers_toml_round_trips() {
        let a = Config::from_toml_str(REPO_TOML).expect("providers.toml do repo deve ser válido");
        let s = a.to_toml_string().unwrap();
        let b = Config::from_toml_str(&s).expect("TOML re-serializado deve ser válido");
        assert_eq!(a, b);
        // Idempotente na segunda volta também.
        assert_eq!(s, b.to_toml_string().unwrap());
    }

    #[test]
    fn repo_providers_toml_content() {
        let c = Config::from_toml_str(REPO_TOML).unwrap();
        assert_eq!(c.defaults.first_token_timeout_ms, 8000);
        assert_eq!(c.defaults.estimate_chars_per_token, 3.6);

        // Gemini limita por modelo: escopo de provider vazio, limites no modelo.
        let gemini = c.provider("gemini").unwrap();
        assert!(gemini.limits.rpm.is_none());
        let flash = gemini.model("gemini-flash").unwrap();
        assert!(flash.upstream.is_some());
        assert_eq!(flash.limits.as_ref().unwrap().tpm, Some(250_000));
        assert_eq!(flash.max_ctx, Some(1_000_000));
        assert_eq!(gemini.quirks.tool_choice, Some(ToolChoiceQuirk::AutoOnly));

        // Mistral: headers reais do smoke mostraram limite por modelo.
        let mistral = c.provider("mistral").unwrap();
        assert!(mistral.limits.rpm.is_none());
        let codestral = mistral.model("codestral").unwrap();
        assert_eq!(codestral.limits.as_ref().unwrap().rpm, Some(125));
        assert!(mistral.quirks.ratelimit_headers.rpm.is_some());

        let groq = c.provider("groq").unwrap();
        assert!(groq.quirks.tpm_counts_max_tokens);
        assert!(groq.quirks.max_tokens_cap.is_some());
        let qwen = groq.model("qwen-coder").unwrap();
        assert_eq!(qwen.limits.as_ref().unwrap().tpm, Some(8000));

        let or = c.provider("openrouter").unwrap();
        assert_eq!(
            or.extra_headers.get("X-Title").map(String::as_str),
            Some("OpenKax")
        );
        assert!(c.provider("github").is_some());
        assert_eq!(c.provider("deepseek").unwrap().tier, Tier::Paid);
        assert_eq!(c.provider("vercel").unwrap().tier, Tier::Credit);
        let cf = c.provider("cloudflare").unwrap();
        assert_eq!(cf.env_refs(), ["CLOUDFLARE_ACCOUNT_ID"]);
        for p in c.providers.iter().filter(|p| p.native.is_none()) {
            for m in &p.models {
                assert!(m.family.is_some(), "{}/{} sem family", p.id, m.id);
            }
        }
        // F0-19: Cohere com modelo SEM raciocínio (sem features=reasoning em /models),
        // logo sem quirk para desligá-lo.
        let cohere = c.provider("cohere").unwrap();
        assert_eq!(
            cohere.model("command-a").unwrap().upstream.as_deref(),
            Some("command-a-03-2025")
        );
        assert!(cohere.model("command-a-plus").is_none());
        assert!(cohere.quirks.disable_thinking.is_none());
        assert!(cohere.quirks.ascii_json);

        // F0-18: NVIDIA e gemini-flash demoram mais até o 1º token.
        for m in &c.provider("nvidia").unwrap().models {
            assert_eq!(m.first_token_timeout_ms, Some(20_000), "nvidia/{}", m.id);
        }
        assert_eq!(flash.first_token_timeout_ms, Some(20_000));
        assert_eq!(
            gemini
                .model("gemini-flash-lite")
                .unwrap()
                .first_token_timeout_ms,
            None
        );
        let anth = c.provider("anthropic").unwrap();
        assert_eq!(
            (anth.auth.clone(), anth.native),
            (Auth::Passthrough, Some(Native::Anthropic))
        );

        for id in [
            "kax/coder-fast",
            "kax/coder-smart",
            "kax/summarizer",
            "kax/reviewer",
            "kax/cheap-long",
        ] {
            assert!(c.class(id).is_some(), "classe {id} ausente");
        }
        let fast = c.class("kax/coder-fast").unwrap();
        assert_eq!(fast.max_packet_tokens, 6000);
        assert_eq!(fast.route[0].provider, "groq");

        let smart: Vec<_> = c
            .class("kax/coder-smart")
            .unwrap()
            .route
            .iter()
            .map(|s| s.provider.as_str())
            .collect();
        assert_eq!(
            smart,
            [
                "gemini",
                "nvidia",
                "mistral",
                "zai",
                "github",
                "sambanova",
                "openrouter",
                "vercel",
                "deepseek"
            ]
        );
    }

    /// F0-18: timeout de 1º token por modelo.
    #[test]
    fn model_first_token_timeout_overrides_defaults() {
        let c = Config::from_toml_str(
            r#"
            [defaults]
            first_token_timeout_ms = 8000
            first_token_timeout_reasoning_ms = 30000

            [[provider]]
            id = "a"
            base_url = "http://a"
            auth = { kind = "none" }
            models = [
              { id = "slow", upstream = "s", first_token_timeout_ms = 20000 },
              { id = "huge", upstream = "h", first_token_timeout_ms = 45000 },
              { id = "plain", upstream = "p" },
            ]
            "#,
        )
        .unwrap();
        let ms = Duration::from_millis;
        let t = |id: &str| c.defaults.first_token_timeouts(c.providers[0].model(id));
        assert_eq!(t("slow"), (ms(20_000), ms(30_000)));
        // raciocínio nunca encurta o prazo do modelo
        assert_eq!(t("huge"), (ms(45_000), ms(45_000)));
        assert_eq!(t("plain"), (ms(8_000), ms(30_000)));
        assert_eq!(
            c.defaults.first_token_timeouts(None),
            (ms(8_000), ms(30_000))
        );
    }

    #[test]
    fn model_first_token_timeout_must_fit_request_timeout() {
        let errs = invalid(
            r#"
            [defaults]
            request_timeout_ms = 60000

            [[provider]]
            id = "a"
            base_url = "http://a"
            auth = { kind = "none" }
            models = [
              { id = "zero", upstream = "z", first_token_timeout_ms = 0 },
              { id = "big", upstream = "b", first_token_timeout_ms = 60001 },
            ]
            "#,
        );
        let all = errs.join("\n");
        assert!(
            all.contains("modelo \"zero\": first_token_timeout_ms"),
            "{all}"
        );
        assert!(
            all.contains("modelo \"big\": first_token_timeout_ms"),
            "{all}"
        );
    }

    #[test]
    fn defaults_apply_when_section_missing() {
        let c = Config::from_toml_str("").unwrap();
        assert_eq!(c.defaults, Defaults::default());
        let c = Config::from_toml_str("[defaults]\nfirst_token_timeout_ms = 500\n").unwrap();
        assert_eq!(c.defaults.first_token_timeout_ms, 500);
        assert_eq!(c.defaults.request_timeout_ms, 60_000);
    }

    #[test]
    fn utc_time_parse_and_display() {
        assert_eq!("07:30".parse::<UtcTime>().unwrap().seconds_of_day(), 27_000);
        assert_eq!("07:30".parse::<UtcTime>().unwrap().to_string(), "07:30");
        for bad in ["24:00", "7:30", "07:60", "0730", "aa:bb", ""] {
            assert!(bad.parse::<UtcTime>().is_err(), "{bad:?} deveria falhar");
        }
    }

    #[test]
    fn rejects_unknown_fields() {
        let toml = r#"
            [[provider]]
            id = "x"
            base_url = "http://x"
            auth = { kind = "none" }
            limtis = { rpm = 1 }
        "#;
        assert!(matches!(
            Config::from_toml_str(toml),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn rejects_bad_routes_and_duplicates() {
        let errs = invalid(
            r#"
            [[provider]]
            id = "a"
            base_url = "http://a"
            auth = { kind = "none" }
            models = [{ id = "m1", upstream = "u1" }, { id = "m2" }]

            [[provider]]
            id = "a"
            base_url = "ftp://a"
            auth = { kind = "bearer", env = "" }

            [[class]]
            id = "coder"
            max_packet_tokens = 10
            route = [
              { provider = "a", model = "nope" },
              { provider = "ghost", model = "m1" },
            ]
            "#,
        );
        let all = errs.join("\n");
        for needle in [
            "provider duplicado",
            "base_url",
            "auth.env vazio",
            "deve começar com \"kax/\"",
            "modelo \"nope\" não listado",
            "provider desconhecido \"ghost\"",
            "modelo \"m2\": `upstream` obrigatório",
        ] {
            assert!(all.contains(needle), "faltou erro {needle:?} em:\n{all}");
        }
    }

    #[test]
    fn disabled_provider_may_omit_upstream() {
        let c = Config::from_toml_str(
            r#"
            [[provider]]
            id = "a"
            base_url = "http://a"
            auth = { kind = "none" }
            enabled = false
            models = [{ id = "m", limits = { rpm = 1 }, max_ctx = 10 }]
            "#,
        )
        .unwrap();
        assert!(!c.providers[0].enabled);
        assert!(c.to_toml_string().unwrap().contains("enabled = false"));
    }

    #[test]
    fn model_limits_reject_max_ctx_and_zero() {
        let errs = invalid(
            r#"
            [[provider]]
            id = "a"
            base_url = "http://a"
            auth = { kind = "none" }
            models = [{ id = "m", upstream = "u", limits = { max_ctx = 5, tpm = 0 } }]
            "#,
        );
        let all = errs.join("\n");
        assert!(all.contains("use `max_ctx` do modelo"), "{all}");
        assert!(all.contains("limits.tpm = 0"), "{all}");
    }

    #[test]
    fn paid_must_come_last() {
        let errs = invalid(
            r#"
            [[provider]]
            id = "paid"
            base_url = "http://p"
            auth = { kind = "none" }
            tier = "credit"
            models = [{ id = "m", upstream = "u" }]

            [[provider]]
            id = "free"
            base_url = "http://f"
            auth = { kind = "none" }
            models = [{ id = "m", upstream = "u" }]

            [[class]]
            id = "kax/x"
            max_packet_tokens = 10
            route = [{ provider = "paid", model = "m" }, { provider = "free", model = "m" }]
            "#,
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("pago/crédito sempre no fim")),
            "{errs:?}"
        );
    }

    /// N1: interpolação de `${VAR}`.
    #[test]
    fn interpolates_env_refs_and_reports_missing() {
        let look = |k: &str| (k == "ACC").then(|| "abc123".to_owned());
        assert_eq!(
            interpolate("https://x/accounts/${ACC}/ai/v1", &look).unwrap(),
            "https://x/accounts/abc123/ai/v1"
        );
        assert_eq!(
            interpolate("sem vars $ e {}", &look).unwrap(),
            "sem vars $ e {}"
        );
        assert_eq!(
            interpolate("${ACC}-${NOPE}-${NOPE2}", &look),
            Err(vec!["NOPE".to_owned(), "NOPE2".to_owned()])
        );
        assert!(env_refs("https://x/${ABERTO").is_err());
        assert!(env_refs("https://x/${}").is_err());
        assert!(env_refs("https://x/${A-B}").is_err());
    }

    #[test]
    fn provider_resolve_env_covers_base_url_and_headers() {
        let c = Config::from_toml_str(
            r#"
            [[provider]]
            id = "cf"
            base_url = "https://api/${ACC}/v1"
            auth = { kind = "none" }
            extra_headers = { "X-Org" = "${ORG}", "X-Fixo" = "a" }
            models = [{ id = "m", upstream = "m" }]
            "#,
        )
        .unwrap();
        let p = &c.providers[0];
        assert_eq!(p.env_refs(), ["ACC", "ORG"]);
        let all = |k: &str| Some(format!("<{k}>"));
        let r = p.resolve_env(&all).unwrap();
        assert_eq!(r.base_url, "https://api/<ACC>/v1");
        assert_eq!(r.extra_headers["X-Org"], "<ORG>");
        assert_eq!(r.extra_headers["X-Fixo"], "a");
        let only_acc = |k: &str| (k == "ACC").then(|| "1".to_owned());
        assert_eq!(p.resolve_env(&only_acc), Err(vec!["ORG".to_owned()]));
    }

    #[test]
    fn rejects_bad_env_ref_syntax() {
        let errs = invalid(
            r#"
            [[provider]]
            id = "cf"
            base_url = "https://api/${ACC/v1"
            auth = { kind = "none" }
            "#,
        );
        assert!(errs.iter().any(|e| e.contains("sem `}`")), "{errs:?}");
    }

    #[test]
    fn tier_order_free_credit_paid() {
        let errs = invalid(
            r#"
            [[provider]]
            id = "pago"
            base_url = "http://p"
            auth = { kind = "none" }
            tier = "paid"
            models = [{ id = "m", upstream = "u" }]

            [[provider]]
            id = "cred"
            base_url = "http://c"
            auth = { kind = "none" }
            tier = "credit"
            models = [{ id = "m", upstream = "u" }]

            [[class]]
            id = "kax/x"
            max_packet_tokens = 10
            route = [{ provider = "pago", model = "m" }, { provider = "cred", model = "m" }]
            "#,
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("\"cred\" (credit) depois de um paid")),
            "{errs:?}"
        );
    }

    #[test]
    fn anthropic_only_as_last_step() {
        let errs = invalid(
            r#"
            [[provider]]
            id = "anthropic"
            base_url = "https://api.anthropic.com"
            auth = { kind = "passthrough" }
            native = "anthropic"

            [[provider]]
            id = "b"
            base_url = "http://b"
            auth = { kind = "none" }
            models = [{ id = "m", upstream = "m" }]

            [[class]]
            id = "kax/x"
            max_packet_tokens = 10
            route = [{ provider = "anthropic", model = "claude-haiku" }, { provider = "b", model = "m" }]
            "#,
        );
        assert!(errs.iter().any(|e| e.contains("último degrau")), "{errs:?}");
    }
}
