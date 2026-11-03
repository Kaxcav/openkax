//! Scheduler de quota (PLANO_MESTRE §3.3, ADR 0003).
//!
//! Responsável por: estado de cada provider e modelo ([`ProviderState`]),
//! buckets RPM/TPM/RPD/TPD em dois escopos (provider e modelo), estimativa de
//! tokens, escolha ordenada de [`Candidate`]s e reservas ([`Reservation`]).
//! Não faz I/O e não tem `async`: o tempo vem de um [`Clock`] injetável e
//! nenhum lock atravessa `.await`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::provider::UpstreamError;
use crate::translate::openai::{ChatCompletionRequest, Usage};

/// Fonte de tempo. `now` (monotônico) para buckets e cooldowns; `utc_now`
/// (relógio de parede) só para o reset diário de RPD/TPD.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;
    fn utc_now(&self) -> SystemTime;
}

/// Relógio real.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn utc_now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Relógio manual para testes: só anda com [`advance`](Self::advance).
#[derive(Debug)]
pub struct ManualClock {
    at: Mutex<(Instant, SystemTime)>,
}

impl ManualClock {
    pub fn new(utc: SystemTime) -> Self {
        Self {
            at: Mutex::new((Instant::now(), utc)),
        }
    }

    /// Avança monotônico e parede juntos.
    pub fn advance(&self, d: Duration) {
        let mut g = lock(&self.at);
        g.0 += d;
        g.1 += d;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        lock(&self.at).0
    }

    fn utc_now(&self) -> SystemTime {
        lock(&self.at).1
    }
}

/// Máquina de estados de um escopo (provider ou modelo):
/// `Healthy → Cooldown(until) → Healthy`; `Disabled` até reload;
/// `Degraded` (aceitação baixa) entra na Fase 3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderState {
    Healthy,
    Cooldown { until: Instant },
    Disabled { reason: DisabledReason },
    Degraded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DisabledReason {
    /// Sem chave no ambiente.
    MissingKey,
    /// 401/403: não adianta repetir até reload.
    AuthRejected,
    /// 404: só o modelo.
    ModelNotFound,
    /// 410: modelo descontinuado pelo provider; só o modelo (F0-21).
    ModelGone,
    /// `enabled = false` no TOML.
    ConfigDisabled,
    /// Protocolo ainda não suportado nesta rota (nativo Anthropic até a Fase 1).
    Unsupported,
    /// `${VAR}` de `base_url`/`extra_headers` ausente no ambiente (N1).
    MissingEnv,
}

/// Um degrau elegível da rota, na ordem de fallback.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// Índice em `Config::providers`.
    pub provider_idx: usize,
    /// Índice em `Provider::models`.
    pub model_idx: usize,
    pub provider_id: Arc<str>,
    pub model_id: Arc<str>,
    /// ID real enviado ao provider.
    pub upstream_model: Arc<str>,
    /// Chars de texto da request (base da recalibração).
    pub input_chars: usize,
    /// Tokens de entrada estimados (com `safety_factor`).
    pub est_tokens: u32,
    /// O que será debitado no TPM/TPD: entrada (+ saída com `tpm_counts_max_tokens`).
    pub reserve_tokens: u32,
    /// `max_tokens` a enviar ao upstream quando há `max_tokens_cap`.
    pub max_tokens_override: Option<u32>,
}

/// Por que um degrau ficou fora da lista de candidatos.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// Nunca vai caber: `est > max_ctx` ou `reserva > capacidade de TPM/TPD`.
    NeverFits,
    /// Cabe, mas não agora: falta saldo. Entra no cálculo do `Retry-After`.
    NotNow {
        ready_in: Duration,
    },
    Cooldown {
        ready_in: Duration,
    },
    Disabled(DisabledReason),
    /// Família excluída por `x-openkax-exclude-family` (N5).
    Excluded,
}

/// Resultado de [`Scheduler::pick`].
#[derive(Debug, Clone, PartialEq)]
pub struct Pick {
    pub candidates: Vec<Candidate>,
    /// `(índice do degrau na rota, motivo)`.
    pub skipped: Vec<(usize, Skip)>,
}

impl Pick {
    /// Menor tempo até algum degrau pulado voltar (`NotNow`/`Cooldown`).
    /// `None` quando nenhum volta sozinho (todos `NeverFits`/`Disabled`).
    pub fn retry_after(&self) -> Option<Duration> {
        self.skipped
            .iter()
            .filter_map(|(_, s)| match s {
                Skip::NotNow { ready_in } | Skip::Cooldown { ready_in } => Some(*ready_in),
                Skip::NeverFits | Skip::Disabled(_) | Skip::Excluded => None,
            })
            .min()
    }

    /// Todos os degraus pulados por "nunca cabe".
    pub fn all_never_fit(&self) -> bool {
        self.candidates.is_empty()
            && !self.skipped.is_empty()
            && self.skipped.iter().all(|(_, s)| *s == Skip::NeverFits)
    }
}

/// Filtros de candidatos por request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PickOptions {
    /// Famílias (`models[].family`) fora da rota; comparação sem caixa.
    pub exclude_families: Vec<String>,
    /// Teto de tier (`x-openkax-max-tier`): degraus acima viram `Excluded`.
    pub max_tier: Option<crate::config::Tier>,
}

/// Falha de `pick_pinned` (header de depuração `x-openkax-pin`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PinError {
    #[error(transparent)]
    Pick(#[from] PickError),
    #[error("pin aponta para provider/modelo inexistente")]
    UnknownTarget,
    #[error("modelo fixado está desabilitado: {0:?}")]
    Disabled(DisabledReason),
}

/// Saldo restante informado pelo upstream em headers (corrige buckets).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Remaining {
    pub rpm: Option<i64>,
    pub tpm: Option<i64>,
    pub rpd: Option<i64>,
    pub tpd: Option<i64>,
    /// Janela IETF (`RateLimit`/`RateLimit-Policy`); sempre do provider.
    pub window: Option<WindowObs>,
}

/// Uma observação IETF: `remaining` requests até `reset_s`, numa cota de
/// `quota` requests a cada `window_s` (quando há `RateLimit-Policy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowObs {
    pub quota: Option<u64>,
    pub window_s: Option<u64>,
    pub remaining: i64,
    pub reset_s: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PickError {
    #[error("classe desconhecida: {0}")]
    UnknownClass(String),
    #[error("pacote de {est} tokens excede max_packet_tokens={max} da classe")]
    PacketTooLarge { est: u32, max: u32 },
}

/// Recusa de [`Scheduler::reserve`]: o saldo mudou entre `pick` e `reserve`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("sem saldo para reservar (pronto em {ready_in:?})")]
pub struct NotNow {
    pub ready_in: Duration,
}

/// Saldo atual de um provider (escopo compartilhado) — `GET /quota`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuotaSnapshot {
    pub providers: Vec<ProviderQuota>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderQuota {
    pub id: String,
    pub tier: crate::config::Tier,
    /// Variáveis `${VAR}` ausentes (N1).
    pub missing_env: Vec<String>,
    pub in_flight: u32,
    pub state: StateView,
    pub rpm_remaining: Option<i64>,
    pub tpm_remaining: Option<i64>,
    pub rpd_remaining: Option<i64>,
    pub tpd_remaining: Option<i64>,
    /// Janela aprendida dos headers IETF (F0-21); `None` até a 1ª observação.
    pub window_remaining: Option<i64>,
    /// Buckets mensais (mês UTC), F0-10.
    pub requests_month_remaining: Option<i64>,
    pub tokens_month_remaining: Option<i64>,
    pub models: Vec<ModelQuota>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelQuota {
    pub id: String,
    pub upstream: Option<String>,
    pub state: StateView,
    /// 400/413/422 recebidos (A3): quirk errado não pode ficar invisível.
    pub request_faults: u64,
    pub family: Option<String>,
    pub in_flight: u32,
    /// Recalibrado por EMA a cada `usage` deste modelo (F0-20).
    pub chars_per_token: f64,
    pub rpm_remaining: Option<i64>,
    pub tpm_remaining: Option<i64>,
    pub rpd_remaining: Option<i64>,
    pub tpd_remaining: Option<i64>,
    pub requests_month_remaining: Option<i64>,
    pub tokens_month_remaining: Option<i64>,
}

/// Estado persistível dos buckets (F0-10): vai para `state/buckets.json` e
/// atravessa o hot-reload. Tempos em segundos de parede (UTC), porque
/// `Instant` não sobrevive a restart. `Disabled` nunca entra (vale até reload).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BucketsSnapshot {
    pub version: u32,
    /// Segundos UTC (com fração) do export; base do refill no restore.
    pub saved_at_utc: f64,
    /// `"provider"` ou `"provider/modelo"` → estado do escopo.
    pub scopes: BTreeMap<String, ScopeSnapshot>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScopeSnapshot {
    /// Nível dos buckets de refill contínuo no export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpd: Option<CounterSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpd: Option<CounterSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_month: Option<CounterSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_month: Option<CounterSnapshot>,
    /// Cooldown restante no export, em segundos.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_s: Option<f64>,
    /// Só em modelo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chars_per_token: Option<f64>,
}

/// Janela IETF aprendida (é dado do upstream, não do TOML: vai inteira).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowSnapshot {
    pub cap: f64,
    pub per_sec: f64,
    pub level: f64,
}

/// Contador de janela fixa (dia ou mês UTC): `window` é o índice da janela.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CounterSnapshot {
    pub used: i64,
    pub window: i64,
}

const SNAPSHOT_VERSION: u32 = 1;

/// [`ProviderState`] serializável (sem `Instant`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StateView {
    Healthy,
    Cooldown { ready_in_s: f64 },
    Disabled { reason: DisabledReason },
    Degraded,
}

/// Estado vivo de todos os providers. Barato de clonar.
#[derive(Clone)]
pub struct Scheduler {
    shared: Arc<Shared>,
}

const DAY_SECS: i64 = 86_400;
/// Overhead fixo por mensagem (role, separadores do template de chat).
const TOKENS_PER_MESSAGE: u32 = 4;
/// Parte não-texto (imagem, áudio) conta como este tanto de chars.
const NON_TEXT_PART_CHARS: usize = 1000;
const EMA_ALPHA: f64 = 0.2;
const CPT_MIN: f64 = 1.5;
const CPT_MAX: f64 = 8.0;
const MIN_COOLDOWN: Duration = Duration::from_secs(1);
/// Dica de espera quando o escopo está no teto de concorrência.
const CONCURRENCY_WAIT: Duration = Duration::from_secs(1);

struct Shared {
    clock: Arc<dyn Clock>,
    default_cpt: f64,
    providers: Vec<ProviderRt>,
    classes: Vec<ClassRt>,
}

struct ProviderRt {
    id: Arc<str>,
    missing_env: Vec<String>,
    tier: crate::config::Tier,
    cooldown: crate::config::Cooldown,
    safety: f64,
    tpm_counts_max_tokens: bool,
    max_tokens_cap: Option<u32>,
    /// Lock 1 (sempre antes do lock do modelo).
    state: Mutex<Scope>,
    models: Vec<ModelRt>,
}

/// Escopo do modelo + calibração de chars/token, que é do tokenizer do
/// modelo e não do provider (F0-20).
struct ModelScope {
    scope: Scope,
    chars_per_token: f64,
}

struct ModelRt {
    id: Arc<str>,
    family: Option<String>,
    upstream: Option<Arc<str>>,
    max_ctx: Option<u32>,
    /// Tem `limits` próprios → 429 cai neste escopo.
    own_limits: bool,
    /// Lock 2.
    scope: Mutex<ModelScope>,
}

/// Medidas da request independentes do provider.
struct Estimate {
    chars: usize,
    messages: usize,
    requested_out: Option<u32>,
}

struct ClassRt {
    id: String,
    max_packet_tokens: u32,
    /// `None` = degrau sem modelo OpenAI-compat (nativo Anthropic, Fase 1).
    steps: Vec<Option<(usize, usize)>>,
}

/// Bucket de refill contínuo (RPM/TPM). `level` pode ficar negativo (dívida
/// após `settle` acima da reserva), o que freia o escopo naturalmente.
#[derive(Debug)]
struct Rate {
    cap: f64,
    level: f64,
    per_sec: f64,
    last: Instant,
}

impl Rate {
    fn new(limit: u64, now: Instant) -> Self {
        Self::per_window(limit, 60, now)
    }

    /// `limit` a cada `window_s` segundos, com refill contínuo.
    fn per_window(limit: u64, window_s: u64, now: Instant) -> Self {
        let cap = limit as f64;
        Self {
            cap,
            level: cap,
            per_sec: cap / window_s.max(1) as f64,
            last: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        if now > self.last {
            let dt = (now - self.last).as_secs_f64();
            self.level = (self.level + dt * self.per_sec).min(self.cap);
            self.last = now;
        }
    }

    fn wait_for(&self, amount: f64) -> Duration {
        if self.level >= amount {
            Duration::ZERO
        } else {
            Duration::from_secs_f64((amount - self.level) / self.per_sec)
        }
    }

    fn remaining(&self) -> i64 {
        self.level.floor() as i64
    }
}

/// Contador de janela fixa: diária (RPD/TPD, reset num horário UTC) ou
/// mensal (mês UTC, F0-10). `window` é o índice da janela corrente.
#[derive(Debug)]
struct Daily {
    limit: u64,
    used: i64,
    window: i64,
}

impl Daily {
    fn remaining(&self) -> i64 {
        self.limit as i64 - self.used
    }

    fn export(&self) -> CounterSnapshot {
        CounterSnapshot {
            used: self.used,
            window: self.window,
        }
    }
}

impl Rate {
    /// Nível salvo + refill pelo tempo decorrido desde o export.
    fn restore(&mut self, level: f64, elapsed_s: f64, now: Instant) {
        self.level = (level + elapsed_s * self.per_sec).min(self.cap);
        self.last = now;
    }
}

/// Um escopo de limite: buckets + estado.
#[derive(Debug)]
struct Scope {
    rpm: Option<Rate>,
    tpm: Option<Rate>,
    /// Janela IETF aprendida dos headers (só no provider).
    window: Option<Rate>,
    rpd: Option<Daily>,
    tpd: Option<Daily>,
    /// Mensais (mês UTC), F0-10.
    rpmo: Option<Daily>,
    tpmo: Option<Daily>,
    /// Segundos do dia (UTC) em que RPD/TPD resetam.
    reset_offset: i64,
    has_daily: bool,
    cooldown_until: Option<Instant>,
    disabled: Option<DisabledReason>,
    request_faults: u64,
    max_concurrent: Option<u32>,
    in_flight: u32,
}

impl Scope {
    fn new(
        limits: Option<&crate::config::Limits>,
        default_reset: crate::config::UtcTime,
        now: Instant,
        utc: i64,
    ) -> Self {
        let reset = limits.and_then(|l| l.reset_utc).unwrap_or(default_reset);
        let reset_offset = i64::from(reset.seconds_of_day());
        let window = (utc - reset_offset).div_euclid(DAY_SECS);
        let daily = |limit: u64| Daily {
            limit,
            used: 0,
            window,
        };
        let monthly = |limit: u64| Daily {
            limit,
            used: 0,
            window: month_index(utc),
        };
        Self {
            rpm: limits.and_then(|l| l.rpm).map(|v| Rate::new(v.into(), now)),
            tpm: limits.and_then(|l| l.tpm).map(|v| Rate::new(v.into(), now)),
            window: None,
            rpd: limits.and_then(|l| l.rpd).map(|v| daily(v.into())),
            tpd: limits.and_then(|l| l.tpd).map(daily),
            rpmo: limits.and_then(|l| l.requests_per_month).map(monthly),
            tpmo: limits.and_then(|l| l.tokens_per_month).map(monthly),
            reset_offset,
            has_daily: limits.is_some_and(crate::config::Limits::has_daily),
            cooldown_until: None,
            disabled: None,
            request_faults: 0,
            max_concurrent: limits.and_then(|l| l.max_concurrent),
            in_flight: 0,
        }
    }

    /// Refill, virada de janela diária e expiração de cooldown.
    fn tick(&mut self, now: Instant, utc: i64) {
        for r in [&mut self.rpm, &mut self.tpm, &mut self.window]
            .into_iter()
            .flatten()
        {
            r.refill(now);
        }
        let window = (utc - self.reset_offset).div_euclid(DAY_SECS);
        let month = month_index(utc);
        for (d, w) in [
            (&mut self.rpd, window),
            (&mut self.tpd, window),
            (&mut self.rpmo, month),
            (&mut self.tpmo, month),
        ] {
            if let Some(d) = d
                && d.window != w
            {
                d.window = w;
                d.used = 0;
            }
        }
        if self.cooldown_until.is_some_and(|u| u <= now) {
            self.cooldown_until = None;
        }
    }

    fn secs_to_reset(&self, utc: i64) -> Duration {
        let into = (utc - self.reset_offset).rem_euclid(DAY_SECS);
        Duration::from_secs((DAY_SECS - into) as u64)
    }

    fn daily_exhausted(&self) -> bool {
        [&self.rpd, &self.tpd]
            .into_iter()
            .flatten()
            .any(|d| d.remaining() <= 0)
    }

    /// Ordem: desabilitado → nunca cabe → cooldown → sem saldo agora.
    fn gate(&self, tokens: u32, now: Instant, utc: i64) -> Result<(), Skip> {
        if let Some(reason) = self.disabled {
            return Err(Skip::Disabled(reason));
        }
        let t = f64::from(tokens);
        if self.tpm.as_ref().is_some_and(|r| r.cap < t)
            || [&self.tpd, &self.tpmo]
                .into_iter()
                .flatten()
                .any(|d| (d.limit as f64) < t)
        {
            return Err(Skip::NeverFits);
        }
        if let Some(until) = self.cooldown_until.filter(|u| *u > now) {
            return Err(Skip::Cooldown {
                ready_in: until - now,
            });
        }
        let mut wait = Duration::ZERO;
        if self.max_concurrent.is_some_and(|max| self.in_flight >= max) {
            wait = CONCURRENCY_WAIT;
        }
        for r in [&self.rpm, &self.window].into_iter().flatten() {
            wait = wait.max(r.wait_for(1.0));
        }
        if let Some(r) = &self.tpm {
            wait = wait.max(r.wait_for(t));
        }
        let rpd_out = self.rpd.as_ref().is_some_and(|d| d.remaining() < 1);
        let tpd_out = self
            .tpd
            .as_ref()
            .is_some_and(|d| d.remaining() < i64::from(tokens));
        if rpd_out || tpd_out {
            wait = wait.max(self.secs_to_reset(utc));
        }
        let month_out = self.rpmo.as_ref().is_some_and(|d| d.remaining() < 1)
            || self
                .tpmo
                .as_ref()
                .is_some_and(|d| d.remaining() < i64::from(tokens));
        if month_out {
            wait = wait.max(secs_to_next_month(utc));
        }
        if wait > Duration::ZERO {
            Err(Skip::NotNow { ready_in: wait })
        } else {
            Ok(())
        }
    }

    /// Corrige buckets que este escopo possui com o saldo informado pelo upstream.
    /// Devolve os campos que não eram deste escopo.
    fn apply_remaining(&mut self, r: Remaining) -> Remaining {
        let mut left = Remaining::default();
        match (&mut self.rpm, r.rpm) {
            (Some(b), Some(v)) => b.level = (v as f64).min(b.cap),
            (None, v) => left.rpm = v,
            _ => {}
        }
        match (&mut self.tpm, r.tpm) {
            (Some(b), Some(v)) => b.level = (v as f64).min(b.cap),
            (None, v) => left.tpm = v,
            _ => {}
        }
        match (&mut self.rpd, r.rpd) {
            (Some(d), Some(v)) => d.used = (d.limit as i64 - v).max(0),
            (None, v) => left.rpd = v,
            _ => {}
        }
        match (&mut self.tpd, r.tpd) {
            (Some(d), Some(v)) => d.used = (d.limit as i64 - v).max(0),
            (None, v) => left.tpd = v,
            _ => {}
        }
        left
    }

    /// Janela IETF: (re)cria o bucket com `q`/`w` da política e acerta o
    /// nível pelo `r`; esgotada → cooldown até o `t` (F0-21).
    fn apply_window(&mut self, w: WindowObs, now: Instant) {
        if let (Some(q), Some(ws)) = (w.quota.filter(|q| *q > 0), w.window_s) {
            let fresh = Rate::per_window(q, ws, now);
            let same = self
                .window
                .as_ref()
                .is_some_and(|b| b.cap == fresh.cap && b.per_sec == fresh.per_sec);
            if !same {
                self.window = Some(fresh);
            }
        }
        if let Some(b) = &mut self.window {
            b.level = (w.remaining as f64).min(b.cap);
        }
        if w.remaining <= 0 {
            self.cool(now, Duration::from_secs(w.reset_s).max(MIN_COOLDOWN));
        }
    }

    fn charge_request(&mut self) {
        self.in_flight += 1;
        for r in [&mut self.rpm, &mut self.window].into_iter().flatten() {
            r.level -= 1.0;
        }
        for d in [&mut self.rpd, &mut self.rpmo].into_iter().flatten() {
            d.used += 1;
        }
    }

    /// `delta > 0` cobra, `delta < 0` devolve (limitado à capacidade).
    fn adjust_tokens(&mut self, delta: i64) {
        if let Some(r) = &mut self.tpm {
            r.level = (r.level - delta as f64).min(r.cap);
        }
        for d in [&mut self.tpd, &mut self.tpmo].into_iter().flatten() {
            d.used = (d.used + delta).max(0);
        }
    }

    fn drain_rpm(&mut self) {
        if let Some(r) = &mut self.rpm {
            r.level = r.level.min(0.0);
        }
    }

    fn cool(&mut self, now: Instant, d: Duration) {
        let until = now + d;
        self.cooldown_until = Some(self.cooldown_until.map_or(until, |u| u.max(until)));
    }

    fn state(&self, now: Instant) -> ProviderState {
        if let Some(reason) = self.disabled {
            ProviderState::Disabled { reason }
        } else if let Some(until) = self.cooldown_until.filter(|u| *u > now) {
            ProviderState::Cooldown { until }
        } else {
            ProviderState::Healthy
        }
    }

    fn view(&self, now: Instant) -> StateView {
        match self.state(now) {
            ProviderState::Healthy => StateView::Healthy,
            ProviderState::Cooldown { until } => StateView::Cooldown {
                ready_in_s: (until - now).as_secs_f64(),
            },
            ProviderState::Disabled { reason } => StateView::Disabled { reason },
            ProviderState::Degraded => StateView::Degraded,
        }
    }

    fn monthly_remaining(&self) -> [Option<i64>; 2] {
        [
            self.rpmo.as_ref().map(Daily::remaining),
            self.tpmo.as_ref().map(Daily::remaining),
        ]
    }

    /// Estado persistível (F0-10). Chamar depois de `tick`.
    fn export(&self, now: Instant) -> ScopeSnapshot {
        ScopeSnapshot {
            rpm: self.rpm.as_ref().map(|r| r.level),
            tpm: self.tpm.as_ref().map(|r| r.level),
            window: self.window.as_ref().map(|r| WindowSnapshot {
                cap: r.cap,
                per_sec: r.per_sec,
                level: r.level,
            }),
            rpd: self.rpd.as_ref().map(Daily::export),
            tpd: self.tpd.as_ref().map(Daily::export),
            requests_month: self.rpmo.as_ref().map(Daily::export),
            tokens_month: self.tpmo.as_ref().map(Daily::export),
            cooldown_s: self
                .cooldown_until
                .filter(|u| *u > now)
                .map(|u| (u - now).as_secs_f64()),
            chars_per_token: None,
        }
    }

    /// Aplica um export sobre os buckets que ESTA config declara: bucket que
    /// sumiu é ignorado, bucket novo fica cheio, nível acima do teto é cortado.
    /// Contadores de janela já virada são zerados pelo `tick` do fim.
    fn restore(&mut self, s: &ScopeSnapshot, elapsed_s: f64, now: Instant, utc: i64) {
        for (b, level) in [(&mut self.rpm, s.rpm), (&mut self.tpm, s.tpm)] {
            if let (Some(b), Some(level)) = (b, level) {
                b.restore(level, elapsed_s, now);
            }
        }
        if let Some(w) = s.window.filter(|w| w.cap > 0.0 && w.per_sec > 0.0) {
            let mut b = Rate {
                cap: w.cap,
                level: w.level,
                per_sec: w.per_sec,
                last: now,
            };
            b.restore(w.level, elapsed_s, now);
            self.window = Some(b);
        }
        for (d, c) in [
            (&mut self.rpd, s.rpd),
            (&mut self.tpd, s.tpd),
            (&mut self.rpmo, s.requests_month),
            (&mut self.tpmo, s.tokens_month),
        ] {
            if let (Some(d), Some(c)) = (d, c) {
                d.used = c.used.max(0);
                d.window = c.window;
            }
        }
        if let Some(rem) = s.cooldown_s.map(|c| c - elapsed_s).filter(|r| *r > 0.0) {
            self.cool(now, Duration::from_secs_f64(rem));
        }
        self.tick(now, utc);
    }

    fn remaining(&self) -> [Option<i64>; 4] {
        [
            self.rpm.as_ref().map(Rate::remaining),
            self.tpm.as_ref().map(Rate::remaining),
            self.rpd.as_ref().map(Daily::remaining),
            self.tpd.as_ref().map(Daily::remaining),
        ]
    }
}

/// Junta os vereditos dos dois escopos: o mais restritivo vence; entre
/// esperas, a maior (só sai quando os dois liberarem).
fn combine(a: Result<(), Skip>, b: Result<(), Skip>) -> Result<(), Skip> {
    fn rank(s: &Skip) -> u8 {
        match s {
            Skip::Disabled(_) | Skip::Excluded => 3,
            Skip::NeverFits => 2,
            Skip::Cooldown { .. } | Skip::NotNow { .. } => 1,
        }
    }
    fn wait(s: &Skip) -> Duration {
        match s {
            Skip::Cooldown { ready_in } | Skip::NotNow { ready_in } => *ready_in,
            _ => Duration::ZERO,
        }
    }
    match (a, b) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(s), Ok(())) | (Ok(()), Err(s)) => Err(s),
        (Err(x), Err(y)) => {
            let pick_x = match rank(&x).cmp(&rank(&y)) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Equal => wait(&x) >= wait(&y),
            };
            Err(if pick_x { x } else { y })
        }
    }
}

/// Segundos UTC com fração (base do refill no restore do snapshot).
fn utc_secs_f64(t: SystemTime) -> f64 {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_secs_f64(),
        Err(e) => -e.duration().as_secs_f64(),
    }
}

/// `(ano, mês 1-12, dia)` do dia `z` desde 1970-01-01 (Howard Hinnant).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Índice do mês UTC (`ano*12 + mês-1`) de um instante em segundos UTC.
fn month_index(utc: i64) -> i64 {
    let (y, m, _) = civil_from_days(utc.div_euclid(DAY_SECS));
    y * 12 + m - 1
}

/// Até 00:00 UTC do dia 1 do mês seguinte.
fn secs_to_next_month(utc: i64) -> Duration {
    let (y, m, _) = civil_from_days(utc.div_euclid(DAY_SECS));
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    let next = crate::provider::days_from_civil(ny, nm, 1) * DAY_SECS;
    Duration::from_secs((next - utc).max(0) as u64)
}

fn utc_secs(t: SystemTime) -> i64 {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

impl Scheduler {
    /// `has_key(env)` diz se a chave existe (sem expor o valor).
    pub fn new(config: &Config, clock: Arc<dyn Clock>, has_key: &dyn Fn(&str) -> bool) -> Self {
        use crate::config::Auth;

        let now = clock.now();
        let utc = utc_secs(clock.utc_now());
        let d = &config.defaults;
        let providers = config
            .providers
            .iter()
            .map(|p| {
                let mut scope = Scope::new(Some(&p.limits), d.daily_reset_utc, now, utc);
                let missing_env: Vec<String> =
                    p.env_refs().into_iter().filter(|v| !has_key(v)).collect();
                scope.disabled = if !p.enabled {
                    Some(DisabledReason::ConfigDisabled)
                } else if p.native.is_some() {
                    Some(DisabledReason::Unsupported)
                } else {
                    match &p.auth {
                        Auth::Bearer { env } if !has_key(env) => Some(DisabledReason::MissingKey),
                        _ if !missing_env.is_empty() => Some(DisabledReason::MissingEnv),
                        _ => None,
                    }
                };
                let models = p
                    .models
                    .iter()
                    .map(|m| ModelRt {
                        id: m.id.as_str().into(),
                        family: m.family.clone(),
                        upstream: m.upstream.as_deref().map(Into::into),
                        max_ctx: m.max_ctx.or(p.limits.max_ctx),
                        own_limits: m.limits.is_some(),
                        scope: Mutex::new(ModelScope {
                            scope: Scope::new(m.limits.as_ref(), d.daily_reset_utc, now, utc),
                            chars_per_token: d.estimate_chars_per_token,
                        }),
                    })
                    .collect();
                ProviderRt {
                    id: p.id.as_str().into(),
                    missing_env,
                    tier: p.tier,
                    cooldown: p.cooldown(),
                    safety: p.safety_factor.unwrap_or(d.safety_factor),
                    tpm_counts_max_tokens: p.quirks.tpm_counts_max_tokens,
                    max_tokens_cap: p.quirks.max_tokens_cap,
                    state: Mutex::new(scope),
                    models,
                }
            })
            .collect::<Vec<_>>();
        let classes = config
            .classes
            .iter()
            .map(|c| ClassRt {
                id: c.id.clone(),
                max_packet_tokens: c.max_packet_tokens,
                steps: c
                    .route
                    .iter()
                    .map(|s| {
                        let pi = config.providers.iter().position(|p| p.id == s.provider)?;
                        let mi = config.providers[pi]
                            .models
                            .iter()
                            .position(|m| m.id == s.model)?;
                        Some((pi, mi))
                    })
                    .collect(),
            })
            .collect();
        Self {
            shared: Arc::new(Shared {
                clock,
                default_cpt: d.estimate_chars_per_token,
                providers,
                classes,
            }),
        }
    }

    /// Filtra e ordena os degraus elegíveis de `class_id`. Não debita nada.
    pub fn pick(&self, class_id: &str, request: &ChatCompletionRequest) -> Result<Pick, PickError> {
        self.pick_with(class_id, request, &PickOptions::default())
    }

    /// [`pick`](Self::pick) com filtros (N5).
    pub fn pick_with(
        &self,
        class_id: &str,
        request: &ChatCompletionRequest,
        opts: &PickOptions,
    ) -> Result<Pick, PickError> {
        let (class, est) = self.prepare(class_id, request)?;
        let mut pick = Pick {
            candidates: Vec::new(),
            skipped: Vec::new(),
        };
        for (i, step) in class.steps.iter().enumerate() {
            let Some((pi, mi)) = *step else {
                pick.skipped
                    .push((i, Skip::Disabled(DisabledReason::Unsupported)));
                continue;
            };
            if opts
                .max_tier
                .is_some_and(|t| self.shared.providers[pi].tier > t)
            {
                pick.skipped.push((i, Skip::Excluded));
                continue;
            }
            let family = self.shared.providers[pi].models[mi].family.as_deref();
            if family.is_some_and(|f| {
                opts.exclude_families
                    .iter()
                    .any(|x| x.eq_ignore_ascii_case(f))
            }) {
                pick.skipped.push((i, Skip::Excluded));
                continue;
            }
            match self.evaluate(pi, mi, &est) {
                Ok(c) => pick.candidates.push(c),
                Err(skip) => pick.skipped.push((i, skip)),
            }
        }
        Ok(pick)
    }

    /// Candidato fixo (`x-openkax-pin: provider/modelo`), mesmo fora da rota da
    /// classe. Continua passando por gate, reserva e ledger.
    pub fn pick_pinned(
        &self,
        class_id: &str,
        provider_id: &str,
        model_id: &str,
        request: &ChatCompletionRequest,
    ) -> Result<Pick, PinError> {
        let (_, est) = self.prepare(class_id, request)?;
        let sh = &*self.shared;
        let pi = sh
            .providers
            .iter()
            .position(|p| &*p.id == provider_id)
            .ok_or(PinError::UnknownTarget)?;
        let mi = sh.providers[pi]
            .models
            .iter()
            .position(|m| &*m.id == model_id)
            .ok_or(PinError::UnknownTarget)?;
        let mut pick = Pick {
            candidates: Vec::new(),
            skipped: Vec::new(),
        };
        match self.evaluate(pi, mi, &est) {
            Ok(c) => pick.candidates.push(c),
            Err(Skip::Disabled(reason)) => return Err(PinError::Disabled(reason)),
            Err(skip) => pick.skipped.push((0, skip)),
        }
        Ok(pick)
    }

    fn prepare(
        &self,
        class_id: &str,
        request: &ChatCompletionRequest,
    ) -> Result<(&ClassRt, Estimate), PickError> {
        let sh = &*self.shared;
        let class = sh
            .classes
            .iter()
            .find(|c| c.id == class_id)
            .ok_or_else(|| PickError::UnknownClass(class_id.to_owned()))?;
        let (chars, messages) = request_text(request);
        let packet = tokens_from(chars, messages, sh.default_cpt);
        if packet > class.max_packet_tokens {
            return Err(PickError::PacketTooLarge {
                est: packet,
                max: class.max_packet_tokens,
            });
        }
        Ok((
            class,
            Estimate {
                chars,
                messages,
                requested_out: requested_max_tokens(request),
            },
        ))
    }

    /// Veredito de um degrau (provider `pi`, modelo `mi`).
    fn evaluate(&self, pi: usize, mi: usize, est: &Estimate) -> Result<Candidate, Skip> {
        let sh = &*self.shared;
        let now = sh.clock.now();
        let utc = utc_secs(sh.clock.utc_now());
        let p = &sh.providers[pi];
        let m = &p.models[mi];
        let mut ps = lock(&p.state);
        let mut ms = lock(&m.scope);
        ps.tick(now, utc);
        ms.scope.tick(now, utc);
        let tokens = (f64::from(tokens_from(est.chars, est.messages, ms.chars_per_token))
            * p.safety)
            .ceil() as u32;
        let (out_reserve, max_tokens_override) = p.output_plan(est.requested_out);
        let reserve_tokens = tokens.saturating_add(out_reserve);
        let mut verdict = combine(
            ps.gate(reserve_tokens, now, utc),
            ms.scope.gate(reserve_tokens, now, utc),
        );
        if verdict.is_ok() && m.max_ctx.is_some_and(|ctx| tokens > ctx) {
            verdict = Err(Skip::NeverFits);
        }
        drop(ms);
        drop(ps);
        verdict?;
        let upstream = m
            .upstream
            .clone()
            .ok_or(Skip::Disabled(DisabledReason::ConfigDisabled))?;
        Ok(Candidate {
            provider_idx: pi,
            model_idx: mi,
            provider_id: p.id.clone(),
            model_id: m.id.clone(),
            upstream_model: upstream,
            input_chars: est.chars,
            est_tokens: tokens,
            reserve_tokens,
            max_tokens_override,
        })
    }

    /// Re-checa e debita atomicamente (lock provider → modelo) 1 request e
    /// `reserve_tokens` nos dois escopos.
    pub fn reserve(&self, candidate: &Candidate) -> Result<Reservation, NotNow> {
        let sh = &*self.shared;
        let p = &sh.providers[candidate.provider_idx];
        let m = &p.models[candidate.model_idx];
        let now = sh.clock.now();
        let utc = utc_secs(sh.clock.utc_now());
        let tokens = candidate.reserve_tokens;
        {
            let mut ps = lock(&p.state);
            let mut ms = lock(&m.scope);
            ps.tick(now, utc);
            ms.scope.tick(now, utc);
            match combine(ps.gate(tokens, now, utc), ms.scope.gate(tokens, now, utc)) {
                Ok(()) => {}
                Err(Skip::NotNow { ready_in } | Skip::Cooldown { ready_in }) => {
                    return Err(NotNow { ready_in });
                }
                Err(Skip::NeverFits | Skip::Disabled(_) | Skip::Excluded) => {
                    return Err(NotNow {
                        ready_in: Duration::ZERO,
                    });
                }
            }
            for s in [&mut ps, &mut ms.scope] {
                s.charge_request();
                s.adjust_tokens(i64::from(tokens));
            }
        }
        Ok(Reservation {
            shared: self.shared.clone(),
            provider_idx: candidate.provider_idx,
            model_idx: candidate.model_idx,
            reserved: tokens,
            input_chars: candidate.input_chars,
            est_in: candidate.est_tokens,
            output_chars: 0,
            started: false,
            closed: false,
        })
    }

    pub fn provider_state(&self, provider_id: &str) -> Option<ProviderState> {
        let sh = &*self.shared;
        let p = sh.providers.iter().find(|p| &*p.id == provider_id)?;
        let now = sh.clock.now();
        let mut ps = lock(&p.state);
        ps.tick(now, utc_secs(sh.clock.utc_now()));
        Some(ps.state(now))
    }

    pub fn model_state(&self, provider_id: &str, model_id: &str) -> Option<ProviderState> {
        let sh = &*self.shared;
        let p = sh.providers.iter().find(|p| &*p.id == provider_id)?;
        let m = p.models.iter().find(|m| &*m.id == model_id)?;
        let now = sh.clock.now();
        let mut ms = lock(&m.scope);
        ms.scope.tick(now, utc_secs(sh.clock.utc_now()));
        Some(ms.scope.state(now))
    }

    /// Estado dos buckets para persistir ou levar a um scheduler novo (F0-10).
    pub fn export_state(&self) -> BucketsSnapshot {
        let sh = &*self.shared;
        let now = sh.clock.now();
        let utc_now = sh.clock.utc_now();
        let utc = utc_secs(utc_now);
        let mut scopes = BTreeMap::new();
        for p in &sh.providers {
            let mut ps = lock(&p.state);
            ps.tick(now, utc);
            scopes.insert(p.id.to_string(), ps.export(now));
            for m in &p.models {
                let mut ms = lock(&m.scope);
                ms.scope.tick(now, utc);
                let mut s = ms.scope.export(now);
                s.chars_per_token = Some(ms.chars_per_token);
                scopes.insert(format!("{}/{}", p.id, m.id), s);
            }
        }
        BucketsSnapshot {
            version: SNAPSHOT_VERSION,
            saved_at_utc: utc_secs_f64(utc_now),
            scopes,
        }
    }

    /// Aplica um [`BucketsSnapshot`] casando por id de provider e
    /// `provider/modelo`; o que não casa é ignorado. Versão desconhecida: nada.
    pub fn restore_state(&self, snap: &BucketsSnapshot) {
        if snap.version != SNAPSHOT_VERSION {
            return;
        }
        let sh = &*self.shared;
        let now = sh.clock.now();
        let utc_now = sh.clock.utc_now();
        let utc = utc_secs(utc_now);
        let elapsed = (utc_secs_f64(utc_now) - snap.saved_at_utc).max(0.0);
        for p in &sh.providers {
            let mut ps = lock(&p.state);
            if let Some(s) = snap.scopes.get(&*p.id) {
                ps.restore(s, elapsed, now, utc);
            }
            for m in &p.models {
                let Some(s) = snap.scopes.get(&format!("{}/{}", p.id, m.id)) else {
                    continue;
                };
                let mut ms = lock(&m.scope);
                ms.scope.restore(s, elapsed, now, utc);
                if let Some(cpt) = s.chars_per_token.filter(|c| c.is_finite() && *c > 0.0) {
                    ms.chars_per_token = cpt;
                }
            }
        }
    }

    pub fn snapshot(&self) -> QuotaSnapshot {
        let sh = &*self.shared;
        let now = sh.clock.now();
        let utc = utc_secs(sh.clock.utc_now());
        let providers = sh
            .providers
            .iter()
            .map(|p| {
                let (state, [rpm, tpm, rpd, tpd], in_flight, window, [rmo, tmo]) = {
                    let mut ps = lock(&p.state);
                    ps.tick(now, utc);
                    (
                        ps.view(now),
                        ps.remaining(),
                        ps.in_flight,
                        ps.window.as_ref().map(Rate::remaining),
                        ps.monthly_remaining(),
                    )
                };
                let models = p
                    .models
                    .iter()
                    .map(|m| {
                        let mut ms = lock(&m.scope);
                        ms.scope.tick(now, utc);
                        let [rpm, tpm, rpd, tpd] = ms.scope.remaining();
                        let [rmo, tmo] = ms.scope.monthly_remaining();
                        ModelQuota {
                            id: m.id.to_string(),
                            family: m.family.clone(),
                            in_flight: ms.scope.in_flight,
                            chars_per_token: ms.chars_per_token,
                            upstream: m.upstream.as_deref().map(str::to_owned),
                            state: ms.scope.view(now),
                            request_faults: ms.scope.request_faults,
                            rpm_remaining: rpm,
                            tpm_remaining: tpm,
                            rpd_remaining: rpd,
                            tpd_remaining: tpd,
                            requests_month_remaining: rmo,
                            tokens_month_remaining: tmo,
                        }
                    })
                    .collect();
                ProviderQuota {
                    id: p.id.to_string(),
                    tier: p.tier,
                    missing_env: p.missing_env.clone(),
                    in_flight,
                    state,
                    rpm_remaining: rpm,
                    tpm_remaining: tpm,
                    rpd_remaining: rpd,
                    tpd_remaining: tpd,
                    window_remaining: window,
                    requests_month_remaining: rmo,
                    tokens_month_remaining: tmo,
                    models,
                }
            })
            .collect();
        QuotaSnapshot { providers }
    }
}

impl ProviderRt {
    /// Decisão 2 (ADR 0003): `(tokens de saída a reservar, max_tokens a enviar)`.
    fn output_plan(&self, requested: Option<u32>) -> (u32, Option<u32>) {
        let send = self
            .max_tokens_cap
            .map(|cap| requested.map_or(cap, |r| r.min(cap)));
        let reserve = if self.tpm_counts_max_tokens {
            send.or(requested).unwrap_or(0)
        } else {
            0
        };
        (reserve, send)
    }
}

/// Débito provisório nos buckets, feito antes do envio (ADR 0003).
///
/// Fechamento explícito: [`settle`](Self::settle) ou [`fail`](Self::fail).
/// `Drop` sem fechamento (cliente desconectou, future cancelado):
/// antes do 1º token → devolve tokens; depois → cobra o estimado pelo texto
/// transmitido. Nunca gera cooldown.
pub struct Reservation {
    shared: Arc<Shared>,
    provider_idx: usize,
    model_idx: usize,
    reserved: u32,
    input_chars: usize,
    est_in: u32,
    output_chars: usize,
    started: bool,
    closed: bool,
}

impl std::fmt::Debug for Reservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reservation")
            .field("provider_idx", &self.provider_idx)
            .field("model_idx", &self.model_idx)
            .field("reserved", &self.reserved)
            .field("started", &self.started)
            .finish_non_exhaustive()
    }
}

impl Reservation {
    /// Marca que o 1º token chegou: a partir daqui tokens não são devolvidos.
    pub fn first_token(&mut self) {
        self.started = true;
    }

    /// Chars de saída já transmitidos (base da estimativa sem `usage`).
    pub fn add_output_chars(&mut self, n: usize) {
        self.output_chars += n;
    }

    pub fn reserved_tokens(&self) -> u32 {
        self.reserved
    }

    /// Saldo informado pelo upstream (headers `x-ratelimit-*` mapeados no TOML):
    /// corrige o bucket no escopo dono do limite (modelo antes do provider).
    pub fn observe_remaining(&self, r: &Remaining) {
        if *r == Remaining::default() {
            return;
        }
        let now = self.shared.clock.now();
        self.with_scopes(|ps, ms| {
            let left = ms.scope.apply_remaining(*r);
            ps.apply_remaining(left);
            if let Some(w) = r.window {
                ps.apply_window(w, now);
            }
        });
    }

    /// Sucesso: reconcilia com o `usage` real (refund ou charge) e recalibra chars/token.
    pub fn settle(mut self, usage: Option<Usage>) {
        self.close_settle(usage);
    }

    /// Falha vinda do upstream: aplica a transição de estado. Devolve o
    /// cooldown aplicado (entra no `Retry-After`).
    pub fn fail(mut self, error: &UpstreamError) -> Option<Duration> {
        if self.started {
            // Tokens foram gastos: cobra o estimado; o modelo falhou no meio
            // da resposta → on_5xx_s só no escopo do modelo (F0-17).
            self.close_settle(None);
            let p = &self.shared.providers[self.provider_idx];
            let d = Duration::from_secs(p.cooldown.on_5xx_s).max(MIN_COOLDOWN);
            let now = self.shared.clock.now();
            lock(&p.models[self.model_idx].scope).scope.cool(now, d);
            return Some(d);
        }
        self.close_refund();
        self.transition(error)
    }

    fn transition(&self, error: &UpstreamError) -> Option<Duration> {
        let sh = &*self.shared;
        let p = &sh.providers[self.provider_idx];
        let m = &p.models[self.model_idx];
        let now = sh.clock.now();
        let utc = utc_secs(sh.clock.utc_now());
        let cd = &p.cooldown;
        let mut ps = lock(&p.state);
        let mut ms = lock(&m.scope);
        ps.tick(now, utc);
        ms.scope.tick(now, utc);

        // F0-17: o escopo do cooldown segue o tipo do erro. Conta/conexão
        // (auth, rede) → provider; o modelo que falhou (5xx, protocolo,
        // timeout, 4xx genérico) → só o modelo, para não derrubar os irmãos.
        let cool = |secs: u64, scope: &mut Scope| {
            let d = Duration::from_secs(secs).max(MIN_COOLDOWN);
            scope.cool(now, d);
            Some(d)
        };
        match error {
            UpstreamError::RequestFault { .. } => {
                ms.scope.request_faults += 1;
                None
            }
            UpstreamError::Auth { .. } => {
                ps.disabled = Some(DisabledReason::AuthRejected);
                None
            }
            UpstreamError::NotFound { .. } => {
                ms.scope.disabled = Some(DisabledReason::ModelNotFound);
                None
            }
            UpstreamError::Gone => {
                ms.scope.disabled = Some(DisabledReason::ModelGone);
                None
            }
            UpstreamError::RateLimited { retry_after, daily } => {
                let target: &mut Scope = if m.own_limits { &mut ms.scope } else { &mut ps };
                target.drain_rpm();
                let to_reset = target.secs_to_reset(utc);
                let d = if *daily || target.daily_exhausted() {
                    to_reset
                } else {
                    let ceiling = if target.has_daily {
                        to_reset
                    } else {
                        Duration::from_secs(DAY_SECS as u64)
                    };
                    retry_after
                        .unwrap_or(Duration::from_secs(cd.on_429_s))
                        .min(ceiling)
                }
                .max(MIN_COOLDOWN);
                target.cool(now, d);
                Some(d)
            }
            UpstreamError::Network(_) => cool(cd.on_timeout_s, &mut ps),
            UpstreamError::Server { .. }
            | UpstreamError::Protocol(_)
            | UpstreamError::MidStream(_) => cool(cd.on_5xx_s, &mut ms.scope),
            UpstreamError::FirstTokenTimeout | UpstreamError::RequestTimeout => {
                cool(cd.on_timeout_s, &mut ms.scope)
            }
            UpstreamError::OtherClient { .. } => cool(cd.on_4xx_other_s, &mut ms.scope),
        }
    }

    fn with_scopes(&self, f: impl FnOnce(&mut Scope, &mut ModelScope)) {
        let sh = &*self.shared;
        let p = &sh.providers[self.provider_idx];
        let m = &p.models[self.model_idx];
        let now = sh.clock.now();
        let utc = utc_secs(sh.clock.utc_now());
        let mut ps = lock(&p.state);
        let mut ms = lock(&m.scope);
        ps.tick(now, utc);
        ms.scope.tick(now, utc);
        f(&mut ps, &mut ms);
    }

    fn close_refund(&mut self) {
        self.closed = true;
        let delta = -i64::from(self.reserved);
        self.with_scopes(|ps, ms| {
            ps.in_flight = ps.in_flight.saturating_sub(1);
            ms.scope.in_flight = ms.scope.in_flight.saturating_sub(1);
            ps.adjust_tokens(delta);
            ms.scope.adjust_tokens(delta);
        });
    }

    fn close_settle(&mut self, usage: Option<Usage>) {
        self.closed = true;
        let (reserved, est_in, out_chars, in_chars) = (
            self.reserved,
            self.est_in,
            self.output_chars,
            self.input_chars,
        );
        self.with_scopes(|ps, ms| {
            ps.in_flight = ps.in_flight.saturating_sub(1);
            ms.scope.in_flight = ms.scope.in_flight.saturating_sub(1);
            let actual = match usage {
                Some(u) => i64::from(u.prompt_tokens) + i64::from(u.completion_tokens),
                None => i64::from(est_in) + (out_chars as f64 / ms.chars_per_token).ceil() as i64,
            };
            let delta = actual - i64::from(reserved);
            ps.adjust_tokens(delta);
            ms.scope.adjust_tokens(delta);
            if let Some(u) = usage
                && u.prompt_tokens > 0
                && in_chars > 0
            {
                let observed = in_chars as f64 / f64::from(u.prompt_tokens);
                let ema = (1.0 - EMA_ALPHA) * ms.chars_per_token + EMA_ALPHA * observed;
                ms.chars_per_token = ema.clamp(CPT_MIN, CPT_MAX);
            }
        });
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        if self.started {
            self.close_settle(None);
        } else {
            self.close_refund();
        }
    }
}

/// `max_tokens` (ou `max_completion_tokens`) pedido pelo cliente.
pub fn requested_max_tokens(request: &ChatCompletionRequest) -> Option<u32> {
    request.max_tokens.or_else(|| {
        request
            .extra
            .get("max_completion_tokens")
            .and_then(serde_json::Value::as_u64)
            .map(|v| u32::try_from(v).unwrap_or(u32::MAX))
    })
}

/// `(chars de texto, nº de mensagens)`.
fn request_text(request: &ChatCompletionRequest) -> (usize, usize) {
    use crate::translate::openai::MessageContent;

    let json_len =
        |v: &serde_json::Value| serde_json::to_string(v).map_or(0, |s| s.chars().count());
    let mut chars = 0;
    for m in &request.messages {
        match &m.content {
            Some(MessageContent::Text(t)) => chars += t.chars().count(),
            Some(MessageContent::Parts(parts)) => {
                for part in parts {
                    chars += part
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .map_or(NON_TEXT_PART_CHARS, |t| t.chars().count());
                }
            }
            None => {}
        }
        if let Some(tc) = m.extra.get("tool_calls") {
            chars += json_len(tc);
        }
    }
    if let Some(tools) = request.extra.get("tools") {
        chars += json_len(tools);
    }
    (chars, request.messages.len())
}

fn tokens_from(chars: usize, messages: usize, chars_per_token: f64) -> u32 {
    let text = (chars as f64 / chars_per_token).ceil() as u64;
    let overhead = messages as u64 * u64::from(TOKENS_PER_MESSAGE);
    u32::try_from(text + overhead).unwrap_or(u32::MAX)
}

/// Estimativa heurística de tokens de entrada (sem `safety_factor`).
pub fn estimate_tokens(request: &ChatCompletionRequest, chars_per_token: f64) -> u32 {
    let (chars, messages) = request_text(request);
    tokens_from(chars, messages, chars_per_token)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
