//! Configuração declarativa: `providers.toml`.
//!
//! Provider é **dado**, nunca código especializado: tudo que diferencia um
//! provider (URL, auth, limites, quirks, cooldowns) mora aqui. Este módulo só
//! faz parse + validação; estado de runtime (buckets, cooldown) fica no
//! `scheduler`.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::Path;
use std::str::FromStr;

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
    /// Chute inicial de chars/token (recalibrado por provider via `usage`).
    pub estimate_chars_per_token: f64,
    /// Multiplicador sobre a estimativa de tokens antes de checar buckets.
    pub safety_factor: f64,
    /// Horário (UTC) de reset dos buckets diários quando o provider não define.
    pub daily_reset_utc: UtcTime,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            request_timeout_ms: 60_000,
            first_token_timeout_ms: 8_000,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "Limits::is_unbounded")]
    pub limits: Limits,
    #[serde(default, skip_serializing_if = "Quirks::is_empty")]
    pub quirks: Quirks,
    /// `None` → [`Cooldown::default`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown: Option<Cooldown>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_headers: BTreeMap<String, String>,
    /// Degrau pago (relatórios destacam requests que caíram aqui).
    #[serde(default, skip_serializing_if = "is_false")]
    pub paid: bool,
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

/// Limites declarados. `None` = sem limite conhecido.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ctx: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_month: Option<u64>,
    /// Sobrescreve [`Defaults::daily_reset_utc`] para RPD/TPD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_utc: Option<UtcTime>,
}

impl Limits {
    pub fn is_unbounded(&self) -> bool {
        *self == Self::default()
    }
}

/// Diferenças de comportamento do provider tratadas como dado.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quirks {
    /// Parâmetros removidos do corpo antes de enviar.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub strip_params: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoiceQuirk>,
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
    pub on_5xx_s: u64,
    pub on_timeout_s: u64,
    /// 4xx que não é culpa da request nem auth (ex.: 404 modelo inexistente).
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
        if d.first_token_timeout_ms == 0 || d.request_timeout_ms < d.first_token_timeout_ms {
            errs.push(
                "defaults: exige 0 < first_token_timeout_ms <= request_timeout_ms".to_owned(),
            );
        }

        let mut ids = HashSet::new();
        for p in &self.providers {
            if !ids.insert(p.id.as_str()) {
                errs.push(format!("provider duplicado: {:?}", p.id));
            }
            if !(p.base_url.starts_with("http://") || p.base_url.starts_with("https://")) {
                errs.push(format!("provider {:?}: base_url deve ser http(s)", p.id));
            }
            if let Auth::Bearer { env } = &p.auth
                && env.trim().is_empty()
            {
                errs.push(format!("provider {:?}: auth.env vazio", p.id));
            }
            if p.native.is_none() && p.auth == Auth::Passthrough {
                errs.push(format!(
                    "provider {:?}: auth passthrough exige native",
                    p.id
                ));
            }
            if let Some(sf) = p.safety_factor
                && (sf.is_nan() || sf < 1.0)
            {
                errs.push(format!(
                    "provider {:?}: safety_factor deve ser >= 1.0",
                    p.id
                ));
            }
            for (name, v) in [
                ("rpm", p.limits.rpm),
                ("tpm", p.limits.tpm),
                ("rpd", p.limits.rpd),
            ] {
                if v == Some(0) {
                    errs.push(format!(
                        "provider {:?}: limits.{name} = 0 (remova o provider)",
                        p.id
                    ));
                }
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
            for (i, step) in c.route.iter().enumerate() {
                let Some(p) = self.provider(&step.provider) else {
                    errs.push(format!(
                        "classe {:?}: provider desconhecido {:?}",
                        c.id, step.provider
                    ));
                    continue;
                };
                if !p.models.iter().any(|m| m == &step.model) {
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
            }
        }

        if errs.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(errs))
        }
    }
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

        let groq = c.provider("groq").unwrap();
        assert_eq!(
            groq.auth,
            Auth::Bearer {
                env: "GROQ_API_KEY".into()
            }
        );
        assert_eq!(groq.limits.tpm, Some(8000));
        assert_eq!(groq.limits.tpd, Some(200_000));
        assert_eq!(groq.cooldown(), Cooldown::default());

        let gemini = c.provider("gemini").unwrap();
        assert_eq!(gemini.quirks.tool_choice, Some(ToolChoiceQuirk::AutoOnly));
        assert_eq!(gemini.limits.max_ctx, Some(1_000_000));

        let or = c.provider("openrouter").unwrap();
        assert_eq!(
            or.extra_headers.get("X-Title").map(String::as_str),
            Some("OpenKax")
        );

        assert!(c.provider("deepseek").unwrap().paid);
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
        assert_eq!(
            fast.route[0],
            RouteStep {
                provider: "groq".into(),
                model: "qwen-coder".into()
            }
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
            models = ["m1"]

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
        ] {
            assert!(all.contains(needle), "faltou erro {needle:?} em:\n{all}");
        }
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
            models = ["claude-haiku"]

            [[provider]]
            id = "b"
            base_url = "http://b"
            auth = { kind = "none" }
            models = ["m"]

            [[class]]
            id = "kax/x"
            max_packet_tokens = 10
            route = [{ provider = "anthropic", model = "claude-haiku" }, { provider = "b", model = "m" }]
            "#,
        );
        assert!(errs.iter().any(|e| e.contains("último degrau")), "{errs:?}");
    }
}
