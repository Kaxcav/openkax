//! Scheduler de quota (PLANO_MESTRE §3.3).
//!
//! Responsável por: estado de cada provider ([`ProviderState`]), token
//! buckets RPM/TPM/RPD/TPD, estimativa de tokens e escolha ordenada de
//! [`Candidate`]s para uma classe. Não faz I/O: o tempo vem de um [`Clock`]
//! injetável, para que os testes nunca durmam de verdade.
//!
//! Estado contrato-apenas: as assinaturas abaixo são o CONTRACT da Fase 0;
//! os corpos são implementados depois da aprovação do plano.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

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

/// Máquina de estados do provider: `Healthy → Cooldown(until) → Healthy`;
/// `Disabled` para auth/chave ausente; `Degraded` quando a aceitação cai
/// abaixo do limiar (vai pro fim da ordem).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderState {
    Healthy,
    Cooldown { until: Instant },
    Disabled { reason: DisabledReason },
    Degraded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisabledReason {
    /// Sem chave no ambiente.
    MissingKey,
    /// 401/403: não adianta repetir até reload.
    AuthRejected,
}

/// Um degrau elegível da rota, na ordem de fallback.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// Índice em `Config::providers`.
    pub provider_idx: usize,
    pub provider_id: Arc<str>,
    pub model: Arc<str>,
    /// Estimativa de tokens de entrada (já com `safety_factor`) para este provider.
    pub est_tokens: u32,
}

/// Por que um degrau ficou fora da lista de candidatos.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// Nunca vai caber: `est > max_ctx` ou `est > capacidade de TPM`.
    NeverFits,
    /// Cabe, mas não agora: falta saldo em algum bucket. `ready_in` entra no
    /// cálculo do `Retry-After`.
    NotNow {
        ready_in: Duration,
    },
    Cooldown {
        ready_in: Duration,
    },
    Disabled(DisabledReason),
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
                Skip::NeverFits | Skip::Disabled(_) => None,
            })
            .min()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PickError {
    #[error("classe desconhecida: {0}")]
    UnknownClass(String),
    #[error("pacote de {est} tokens excede max_packet_tokens={max} da classe")]
    PacketTooLarge { est: u32, max: u32 },
}

/// Débito provisório nos buckets de um provider, feito antes do envio.
/// Deve ser fechado com [`Scheduler::settle`] ou [`Scheduler::release`].
#[derive(Debug, PartialEq, Eq)]
#[must_use = "reserva não fechada distorce os buckets"]
pub struct Reservation {
    pub provider_idx: usize,
    pub tokens: u32,
}

/// Recusa de [`Scheduler::reserve`]: o saldo mudou entre `pick` e `reserve`
/// (concorrência). O dispatcher passa ao próximo candidato.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("sem saldo para reservar (pronto em {ready_in:?})")]
pub struct NotNow {
    pub ready_in: Duration,
}

/// Estado vivo de todos os providers. Compartilhado via `Arc`.
pub struct Scheduler {
    _clock: Arc<dyn Clock>,
}

impl Scheduler {
    /// `has_key(env)` diz se a chave existe (sem expor o valor).
    pub fn new(_config: &Config, clock: Arc<dyn Clock>, _has_key: &dyn Fn(&str) -> bool) -> Self {
        // CONTRACT
        Self { _clock: clock }
    }

    /// Filtra e ordena os degraus elegíveis de `class_id` para `request`.
    /// Não debita nada.
    pub fn pick(
        &self,
        _class_id: &str,
        _request: &ChatCompletionRequest,
    ) -> Result<Pick, PickError> {
        // CONTRACT
        Ok(Pick {
            candidates: Vec::new(),
            skipped: Vec::new(),
        })
    }

    /// Re-checa e debita atomicamente 1 request + `candidate.est_tokens`.
    pub fn reserve(&self, _candidate: &Candidate) -> Result<Reservation, NotNow> {
        // CONTRACT
        Err(NotNow {
            ready_in: Duration::ZERO,
        })
    }

    /// Sucesso: reconcilia a reserva com o `usage` real (refund ou charge) e
    /// recalibra chars/token do provider.
    pub fn settle(&self, _reservation: Reservation, _usage: Option<Usage>) {
        // CONTRACT
    }

    /// Falha: devolve tokens reservados (a request continua contada no RPM/RPD)
    /// e aplica a transição de estado correspondente ao erro.
    pub fn release(&self, _reservation: Reservation, _error: &UpstreamError) {
        // CONTRACT
    }

    pub fn state(&self, _provider_id: &str) -> Option<ProviderState> {
        // CONTRACT
        None
    }
}

/// Estimativa heurística: chars de texto da request ÷ `chars_per_token`.
pub fn estimate_tokens(_request: &ChatCompletionRequest, _chars_per_token: f64) -> u32 {
    // CONTRACT
    0
}
