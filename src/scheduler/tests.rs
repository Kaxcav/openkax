//! Testes do scheduler: relógio manual, nenhum sleep real.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;

use super::*;
use crate::config::Config;
use crate::provider::UpstreamError;
use crate::translate::openai::{ChatCompletionRequest, Usage};

const DAY: u64 = 86_400;

/// UTC em um dia qualquer (dia 20000 da época), `h:m:s`.
fn utc(h: u64, m: u64, s: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(20_000 * DAY + h * 3600 + m * 60 + s)
}

/// chars/token = 1 e 4 tokens de overhead por mensagem: `req(n)` estima `n` tokens.
fn req(tokens: u32) -> ChatCompletionRequest {
    req_with(tokens, json!({}))
}

fn req_with(tokens: u32, extra: serde_json::Value) -> ChatCompletionRequest {
    let content = "x".repeat(tokens.saturating_sub(4) as usize);
    let mut v = json!({ "model": "kax/t", "messages": [{ "role": "user", "content": content }] });
    v.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    serde_json::from_value(v).unwrap()
}

struct Fx {
    clock: Arc<ManualClock>,
    s: Scheduler,
}

/// `body` = providers + classes; defaults fixos (cpt 1.0, safety 1.0).
fn fx_at(body: &str, at: SystemTime) -> Fx {
    let toml = format!(
        "[defaults]\nestimate_chars_per_token = 1.0\nsafety_factor = 1.0\nfirst_token_timeout_ms = 100\nrequest_timeout_ms = 1000\n{body}"
    );
    let config = Config::from_toml_str(&toml).unwrap();
    let clock = Arc::new(ManualClock::new(at));
    let s = Scheduler::new(&config, clock.clone(), &|env| env != "MISSING");
    Fx { clock, s }
}

fn fx(body: &str) -> Fx {
    fx_at(body, utc(12, 0, 0))
}

/// Um provider `p` com modelo `m` e uma classe `kax/t` → p/m.
fn single(provider_extra: &str, model_extra: &str) -> String {
    format!(
        r#"
        [[provider]]
        id = "p"
        base_url = "http://p"
        auth = {{ kind = "none" }}
        models = [{{ id = "m", upstream = "m-up" {model_extra} }}]
        {provider_extra}

        [[class]]
        id = "kax/t"
        max_packet_tokens = 1000000
        route = [{{ provider = "p", model = "m" }}]
        "#
    )
}

impl Fx {
    fn pick(&self, r: &ChatCompletionRequest) -> Pick {
        self.s.pick("kax/t", r).unwrap()
    }

    fn only(&self, r: &ChatCompletionRequest) -> Candidate {
        let p = self.pick(r);
        assert_eq!(p.candidates.len(), 1, "esperava 1 candidato: {p:?}");
        p.candidates[0].clone()
    }

    fn skip(&self, r: &ChatCompletionRequest) -> Skip {
        let p = self.pick(r);
        assert!(p.candidates.is_empty(), "esperava nenhum candidato: {p:?}");
        p.skipped[0].1.clone()
    }

    fn reserve(&self, tokens: u32) -> Reservation {
        let c = self.only(&req(tokens));
        self.s.reserve(&c).expect("reserve")
    }

    fn snap_model(&self, provider: &str, model: &str) -> ModelQuota {
        let q = self.s.snapshot();
        let p = q.providers.into_iter().find(|p| p.id == provider).unwrap();
        p.models.into_iter().find(|m| m.id == model).unwrap()
    }

    fn snap_provider(&self, provider: &str) -> ProviderQuota {
        self.s
            .snapshot()
            .providers
            .into_iter()
            .find(|p| p.id == provider)
            .unwrap()
    }
}

fn approx(d: Duration, secs: f64) -> bool {
    (d.as_secs_f64() - secs).abs() < 0.01
}

fn not_now(s: Skip) -> Duration {
    match s {
        Skip::NotNow { ready_in } => ready_in,
        other => panic!("esperava NotNow, veio {other:?}"),
    }
}

fn cooldown(s: Skip) -> Duration {
    match s {
        Skip::Cooldown { ready_in } => ready_in,
        other => panic!("esperava Cooldown, veio {other:?}"),
    }
}

// ---------------------------------------------------------------- estimativa

#[test]
fn estimate_counts_text_tools_and_overhead() {
    let r: ChatCompletionRequest = serde_json::from_value(json!({
        "model": "kax/t",
        "messages": [
            { "role": "system", "content": "abcd" },
            { "role": "user", "content": [{ "type": "text", "text": "efgh" }, { "type": "image_url" }] },
            { "role": "assistant", "tool_calls": [{ "function": { "arguments": "{\"a\":1}" } }] },
        ],
    }))
    .unwrap();
    let base = estimate_tokens(&r, 1.0);
    // 4 + 4 chars de texto + JSON dos tool_calls + 3 mensagens × 4 de overhead.
    assert!(base >= 4 + 4 + 12 + 7, "{base}");
    let with_tools: ChatCompletionRequest = serde_json::from_value(json!({
        "model": "kax/t",
        "messages": [{ "role": "system", "content": "abcd" }],
        "tools": [{ "type": "function", "function": { "name": "grep", "parameters": {} } }],
    }))
    .unwrap();
    assert!(estimate_tokens(&with_tools, 1.0) > estimate_tokens(&req(8), 1.0));
    assert_eq!(estimate_tokens(&req(100), 1.0), 100);
    assert_eq!(estimate_tokens(&req(364), 3.6), 104); // ceil(360 / 3.6) + 4
}

#[test]
fn packet_too_large_and_unknown_class() {
    let f = fx(&single("", "").replace("max_packet_tokens = 1000000", "max_packet_tokens = 50"));
    assert_eq!(
        f.s.pick("kax/t", &req(51)),
        Err(PickError::PacketTooLarge { est: 51, max: 50 })
    );
    assert!(f.s.pick("kax/t", &req(50)).is_ok());
    assert_eq!(
        f.s.pick("kax/nope", &req(5)),
        Err(PickError::UnknownClass("kax/nope".into()))
    );
}

// ---------------------------------------------------------------- buckets

#[test]
fn rpm_bucket_refills_continuously() {
    let f = fx(&single("limits = { rpm = 2 }", ""));
    drop(f.reserve(10));
    drop(f.reserve(10));
    let wait = not_now(f.skip(&req(10)));
    assert!(approx(wait, 30.0), "{wait:?}");
    f.clock.advance(Duration::from_secs(29));
    assert!(approx(not_now(f.skip(&req(10))), 1.0));
    f.clock.advance(Duration::from_secs(1));
    f.only(&req(10));
}

#[test]
fn never_fits_versus_not_now() {
    let f = fx(&single("", ", limits = { tpm = 8000 }, max_ctx = 6000"));
    // > max_ctx
    assert_eq!(f.skip(&req(6001)), Skip::NeverFits);
    let f = fx(&single("", ", limits = { tpm = 8000 }"));
    // > capacidade de TPM
    assert_eq!(f.skip(&req(8001)), Skip::NeverFits);
    // cabe agora
    let r = f.reserve(5000);
    // cabe na capacidade, mas não no saldo: 3000 restantes, faltam 2000 a 8000/60 por s = 15 s
    let wait = not_now(f.skip(&req(5000)));
    assert!(approx(wait, 15.0), "{wait:?}");
    drop(r);
}

#[test]
fn tpd_never_fits_and_rpd_waits_for_reset() {
    let f = fx(&single("limits = { tpd = 1000 }", ""));
    assert_eq!(f.skip(&req(1001)), Skip::NeverFits);

    let f = fx_at(
        &single("limits = { rpd = 1, reset_utc = \"07:30\" }", ""),
        utc(7, 29, 0),
    );
    f.reserve(10).settle(None);
    let wait = not_now(f.skip(&req(10)));
    assert!(approx(wait, 60.0), "{wait:?}");
    f.clock.advance(Duration::from_secs(60));
    f.only(&req(10));
}

#[test]
fn daily_reset_defaults_to_global_utc_time() {
    let f = fx_at(&single("limits = { rpd = 1 }", ""), utc(23, 59, 50));
    f.reserve(10).settle(None);
    assert!(approx(not_now(f.skip(&req(10))), 10.0));
    f.clock.advance(Duration::from_secs(10));
    f.only(&req(10));
}

#[test]
fn retry_after_is_min_over_skipped() {
    let p = Pick {
        candidates: vec![],
        skipped: vec![
            (
                0,
                Skip::NotNow {
                    ready_in: Duration::from_secs(9),
                },
            ),
            (
                1,
                Skip::Cooldown {
                    ready_in: Duration::from_secs(4),
                },
            ),
            (2, Skip::NeverFits),
        ],
    };
    assert_eq!(p.retry_after(), Some(Duration::from_secs(4)));
    let p = Pick {
        candidates: vec![],
        skipped: vec![(0, Skip::NeverFits)],
    };
    assert_eq!(p.retry_after(), None);
}

// ---------------------------------------------------------------- reserva (decisão 1/A1/A2)

#[test]
fn reserve_is_atomic_under_concurrency() {
    let f = fx(&single("", ", limits = { tpm = 8000 }"));
    let c = f.only(&req(3000));
    let ok = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..8).map(|_| sc.spawn(|| f.s.reserve(&c).ok())).collect();
        hs.into_iter()
            .filter_map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(ok.len(), 2, "8000 TPM / 3000 por request = 2");
}

#[test]
fn reserve_rechecks_and_returns_not_now() {
    let f = fx(&single("", ", limits = { tpm = 8000 }"));
    let c = f.only(&req(5000));
    let _a = f.s.reserve(&c).unwrap();
    let err = f.s.reserve(&c).unwrap_err();
    assert!(approx(err.ready_in, 15.0), "{err:?}");
}

/// A1: reserva debita o escopo do provider E o do modelo.
#[test]
fn reserve_debits_provider_and_model_scopes() {
    let f = fx(r#"
        [[provider]]
        id = "p"
        base_url = "http://p"
        auth = { kind = "none" }
        limits = { tpm = 10000, rpm = 100 }
        models = [
          { id = "a", upstream = "a", limits = { tpm = 6000, rpm = 50 } },
          { id = "b", upstream = "b" },
        ]

        [[class]]
        id = "kax/a"
        max_packet_tokens = 100000
        route = [{ provider = "p", model = "a" }]

        [[class]]
        id = "kax/b"
        max_packet_tokens = 100000
        route = [{ provider = "p", model = "b" }]
    "#);
    let ca = f.s.pick("kax/a", &req(5000)).unwrap().candidates.remove(0);
    let _ra = f.s.reserve(&ca).unwrap();

    let p = f.snap_provider("p");
    assert_eq!(p.tpm_remaining, Some(5000));
    assert_eq!(p.rpm_remaining, Some(99));
    let a = f.snap_model("p", "a");
    assert_eq!(a.tpm_remaining, Some(1000));
    assert_eq!(a.rpm_remaining, Some(49));
    assert_eq!(f.snap_model("p", "b").tpm_remaining, None);

    // b não tem limite próprio, mas o escopo compartilhado só tem 5000.
    let pb = f.s.pick("kax/b", &req(6000)).unwrap();
    assert!(matches!(pb.skipped[0].1, Skip::NotNow { .. }), "{pb:?}");
    // a: escopo do modelo só tem 1000.
    let pa = f.s.pick("kax/a", &req(2000)).unwrap();
    assert!(matches!(pa.skipped[0].1, Skip::NotNow { .. }), "{pa:?}");
    assert!(!f.s.pick("kax/b", &req(4000)).unwrap().candidates.is_empty());
}

#[test]
fn settle_refunds_and_charges_against_usage() {
    let f = fx(&single("", ", limits = { tpm = 8000 }"));
    let r = f.reserve(3000);
    r.settle(Some(Usage {
        prompt_tokens: 1000,
        completion_tokens: 500,
        total_tokens: 1500,
    }));
    assert_eq!(f.snap_model("p", "m").tpm_remaining, Some(6500));

    let r = f.reserve(1000);
    r.settle(Some(Usage {
        prompt_tokens: 2000,
        completion_tokens: 2500,
        total_tokens: 4500,
    }));
    // 6500 - 4500 = 2000 (cobra a diferença acima da reserva)
    assert_eq!(f.snap_model("p", "m").tpm_remaining, Some(2000));
}

#[test]
fn settle_recalibrates_chars_per_token_ema() {
    let f = fx(&single("", ""));
    let r = f.reserve(1004); // 1000 chars
    r.settle(Some(Usage {
        prompt_tokens: 250,
        completion_tokens: 1,
        total_tokens: 251,
    }));
    // observado 4.0; EMA 0.8·1.0 + 0.2·4.0 = 1.6
    let cpt = f.snap_model("p", "m").chars_per_token;
    assert!((cpt - 1.6).abs() < 1e-9, "{cpt}");
}

/// F0-20: chars/token é do MODELO (tokenizers diferentes no mesmo provider):
/// recalibrar um não muda a estimativa do irmão.
#[test]
fn chars_per_token_is_calibrated_per_model() {
    let f = fx(SIBLINGS);
    let p = f.pick(&req(1004));
    assert_eq!(p.candidates[0].est_tokens, 1004);
    f.s.reserve(&p.candidates[0]).unwrap().settle(Some(Usage {
        prompt_tokens: 250,
        completion_tokens: 1,
        total_tokens: 251,
    }));
    let flash = f.snap_model("gemini", "gemini-flash").chars_per_token;
    let lite = f.snap_model("gemini", "gemini-flash-lite").chars_per_token;
    assert!((flash - 1.6).abs() < 1e-9, "{flash}");
    assert!((lite - 1.0).abs() < 1e-9, "{lite}");

    let p = f.pick(&req(1004));
    // 1000 chars / 1.6 = 625 + 4 de overhead
    assert_eq!(p.candidates[0].est_tokens, 629);
    assert_eq!(p.candidates[1].est_tokens, 1004);
}

/// A2: drop antes do 1º token devolve tokens; a request continua contada.
#[test]
fn drop_before_first_token_refunds_tokens_keeps_request() {
    let f = fx(&single("", ", limits = { tpm = 8000, rpm = 10 }"));
    drop(f.reserve(3000));
    let m = f.snap_model("p", "m");
    assert_eq!(m.tpm_remaining, Some(8000));
    assert_eq!(m.rpm_remaining, Some(9));
    assert_eq!(f.s.model_state("p", "m"), Some(ProviderState::Healthy));
}

/// A2: drop depois do 1º token (cliente desconectou) cobra o estimado e não
/// gera cooldown.
#[test]
fn drop_after_first_token_settles_estimate_without_cooldown() {
    let f = fx(&single("", ", limits = { tpm = 8000 }"));
    let mut r = f.reserve(3000);
    r.first_token();
    r.add_output_chars(400);
    drop(r);
    // entrada estimada 3000 + 400 chars / 1.0
    assert_eq!(f.snap_model("p", "m").tpm_remaining, Some(8000 - 3400));
    assert_eq!(f.s.provider_state("p"), Some(ProviderState::Healthy));
    assert_eq!(f.s.model_state("p", "m"), Some(ProviderState::Healthy));
}

/// Decisão 3: morte pós-1º-token vinda do upstream → settle estimado + on_5xx_s.
#[test]
fn mid_stream_upstream_error_settles_and_cools_down() {
    let f = fx(&single(
        "cooldown = { on_5xx_s = 20 }",
        ", limits = { tpm = 8000 }",
    ));
    let mut r = f.reserve(1000);
    r.first_token();
    r.add_output_chars(100);
    let d = r.fail(&UpstreamError::MidStream("eof".into()));
    assert_eq!(d, Some(Duration::from_secs(20)));
    assert_eq!(f.snap_model("p", "m").tpm_remaining, Some(8000 - 1100));
    assert!(approx(cooldown(f.skip(&req(10))), 20.0));
}

// ---------------------------------------------------------------- max_tokens (decisão 2)

#[test]
fn tpm_counts_max_tokens_reserves_output_and_clamps() {
    let f = fx(&single(
        "quirks = { tpm_counts_max_tokens = true, max_tokens_cap = 2000 }",
        ", limits = { tpm = 8000 }",
    ));
    let c = f.only(&req_with(5000, json!({ "max_tokens": 32000 })));
    assert_eq!(c.est_tokens, 5000);
    assert_eq!(c.reserve_tokens, 7000);
    assert_eq!(c.max_tokens_override, Some(2000));
    // sem max_tokens do cliente: reserva o cap e envia o cap.
    let c = f.only(&req(5000));
    assert_eq!(
        (c.reserve_tokens, c.max_tokens_override),
        (7000, Some(2000))
    );
    // entrada 6500 + 2000 > 8000 → nunca cabe
    assert_eq!(
        f.skip(&req_with(6500, json!({ "max_tokens": 32000 }))),
        Skip::NeverFits
    );
}

#[test]
fn without_quirk_reserves_input_only() {
    let f = fx(&single("", ", limits = { tpm = 8000 }"));
    let c = f.only(&req_with(5000, json!({ "max_tokens": 32000 })));
    assert_eq!((c.reserve_tokens, c.max_tokens_override), (5000, None));
}

// ---------------------------------------------------------------- erros → estado

#[test]
fn request_fault_skips_without_cooldown_and_counts() {
    let f = fx(&single("", ""));
    let d = f.reserve(10).fail(&UpstreamError::RequestFault {
        status: 400,
        message: String::new(),
    });
    assert_eq!(d, None);
    assert_eq!(f.s.provider_state("p"), Some(ProviderState::Healthy));
    assert_eq!(f.snap_model("p", "m").request_faults, 1);
    f.only(&req(10));
}

#[test]
fn auth_error_disables_provider() {
    let f = fx(&single("", ""));
    f.reserve(10).fail(&UpstreamError::Auth { status: 401 });
    assert_eq!(
        f.s.provider_state("p"),
        Some(ProviderState::Disabled {
            reason: DisabledReason::AuthRejected
        })
    );
    assert_eq!(
        f.skip(&req(10)),
        Skip::Disabled(DisabledReason::AuthRejected)
    );
    f.clock.advance(Duration::from_secs(10 * DAY));
    assert!(matches!(f.skip(&req(10)), Skip::Disabled(_)));
}

/// A1: 404 desabilita só o modelo; outro modelo do mesmo provider segue.
#[test]
fn not_found_disables_only_the_model() {
    let f = fx(r#"
        [[provider]]
        id = "p"
        base_url = "http://p"
        auth = { kind = "none" }
        models = [{ id = "a", upstream = "a" }, { id = "b", upstream = "b" }]

        [[class]]
        id = "kax/t"
        max_packet_tokens = 100000
        route = [{ provider = "p", model = "a" }, { provider = "p", model = "b" }]
    "#);
    let p = f.pick(&req(10));
    assert_eq!(p.candidates.len(), 2);
    f.s.reserve(&p.candidates[0])
        .unwrap()
        .fail(&UpstreamError::NotFound { status: 404 });

    assert_eq!(f.s.provider_state("p"), Some(ProviderState::Healthy));
    assert_eq!(
        f.s.model_state("p", "a"),
        Some(ProviderState::Disabled {
            reason: DisabledReason::ModelNotFound
        })
    );
    let p = f.pick(&req(10));
    assert_eq!(p.candidates.len(), 1);
    assert_eq!(&*p.candidates[0].model_id, "b");
    assert_eq!(
        p.skipped,
        vec![(0, Skip::Disabled(DisabledReason::ModelNotFound))]
    );
}

/// A1: 429 vai para o escopo do modelo quando ele tem limites próprios.
#[test]
fn rate_limit_cools_model_scope_when_model_has_limits() {
    let f = fx(r#"
        [[provider]]
        id = "p"
        base_url = "http://p"
        auth = { kind = "none" }
        models = [{ id = "a", upstream = "a", limits = { rpm = 10 } }, { id = "b", upstream = "b", limits = { rpm = 10 } }]

        [[class]]
        id = "kax/t"
        max_packet_tokens = 100000
        route = [{ provider = "p", model = "a" }, { provider = "p", model = "b" }]
    "#);
    let p = f.pick(&req(10));
    let d =
        f.s.reserve(&p.candidates[0])
            .unwrap()
            .fail(&UpstreamError::RateLimited {
                retry_after: Some(Duration::from_secs(30)),
                daily: false,
            });
    assert_eq!(d, Some(Duration::from_secs(30)));
    assert_eq!(f.s.provider_state("p"), Some(ProviderState::Healthy));
    assert!(matches!(
        f.s.model_state("p", "a"),
        Some(ProviderState::Cooldown { .. })
    ));
    assert_eq!(f.s.model_state("p", "b"), Some(ProviderState::Healthy));
    // bucket RPM do escopo que estourou é zerado
    assert_eq!(f.snap_model("p", "a").rpm_remaining, Some(0));
    let p = f.pick(&req(10));
    assert_eq!(p.candidates.len(), 1);
    assert!(approx(cooldown(p.skipped[0].1.clone()), 30.0));
}

#[test]
fn rate_limit_cools_provider_scope_when_model_has_no_limits() {
    let f = fx(&single("limits = { rpm = 10 }", ""));
    let d = f.reserve(10).fail(&UpstreamError::RateLimited {
        retry_after: None,
        daily: false,
    });
    assert_eq!(d, Some(Duration::from_secs(60)), "on_429_s default");
    assert!(matches!(
        f.s.provider_state("p"),
        Some(ProviderState::Cooldown { .. })
    ));
    assert_eq!(f.s.model_state("p", "m"), Some(ProviderState::Healthy));
    f.clock.advance(Duration::from_secs(60));
    assert_eq!(f.s.provider_state("p"), Some(ProviderState::Healthy));
}

/// A4: teto do Retry-After = próximo reset diário; cota diária → até o reset.
#[test]
fn rate_limit_ceiling_is_next_daily_reset() {
    let f = fx_at(&single("limits = { rpd = 100 }", ""), utc(23, 59, 50));
    let d = f.reserve(10).fail(&UpstreamError::RateLimited {
        retry_after: Some(Duration::from_secs(3600)),
        daily: false,
    });
    assert_eq!(d, Some(Duration::from_secs(10)));

    let f = fx_at(&single("limits = { rpd = 100 }", ""), utc(22, 0, 0));
    let d = f.reserve(10).fail(&UpstreamError::RateLimited {
        retry_after: Some(Duration::from_secs(5)),
        daily: true,
    });
    assert_eq!(d, Some(Duration::from_secs(2 * 3600)));

    // Retry-After zero/ausente nunca vira cooldown de 0 s.
    let f = fx(&single("", ""));
    let d = f.reserve(10).fail(&UpstreamError::RateLimited {
        retry_after: Some(Duration::ZERO),
        daily: false,
    });
    assert_eq!(d, Some(Duration::from_secs(1)));
}

#[test]
fn rpd_exhausted_rate_limit_waits_for_reset() {
    let f = fx_at(&single("limits = { rpd = 1 }", ""), utc(20, 0, 0));
    let d = f.reserve(10).fail(&UpstreamError::RateLimited {
        retry_after: Some(Duration::from_secs(5)),
        daily: false,
    });
    // bucket RPD do escopo zerado → cooldown até o reset (4 h)
    assert_eq!(d, Some(Duration::from_secs(4 * 3600)));
}

/// Dois modelos irmãos com limites próprios (o caso Gemini flash / flash-lite).
const SIBLINGS: &str = r#"
    [[provider]]
    id = "gemini"
    base_url = "http://g"
    auth = { kind = "none" }
    models = [
      { id = "gemini-flash", upstream = "f", limits = { rpm = 10 } },
      { id = "gemini-flash-lite", upstream = "fl", limits = { rpm = 10 } },
    ]
    cooldown = { on_429_s = 1, on_5xx_s = 20, on_timeout_s = 30, on_4xx_other_s = 600 }

    [[class]]
    id = "kax/t"
    max_packet_tokens = 100000
    route = [{ provider = "gemini", model = "gemini-flash" }, { provider = "gemini", model = "gemini-flash-lite" }]
"#;

fn fail_flash(f: &Fx, err: &UpstreamError) -> Option<Duration> {
    let p = f.pick(&req(10));
    assert_eq!(&*p.candidates[0].model_id, "gemini-flash");
    f.s.reserve(&p.candidates[0]).unwrap().fail(err)
}

fn is_cooldown(s: Option<ProviderState>) -> bool {
    matches!(s, Some(ProviderState::Cooldown { .. }))
}

/// F0-17: erro do modelo (5xx, protocolo, timeout, 4xx genérico) esfria só o
/// modelo; o irmão do mesmo provider continua elegível.
#[test]
fn model_errors_cool_only_the_model() {
    for (err, secs) in [
        (UpstreamError::Server { status: 503 }, 20),
        (UpstreamError::Protocol("eof".into()), 20),
        (UpstreamError::MidStream("eof".into()), 20),
        (UpstreamError::FirstTokenTimeout, 30),
        (UpstreamError::RequestTimeout, 30),
        (UpstreamError::OtherClient { status: 418 }, 600),
    ] {
        let f = fx(SIBLINGS);
        assert_eq!(
            fail_flash(&f, &err),
            Some(Duration::from_secs(secs)),
            "{err:?}"
        );
        assert_eq!(
            f.s.provider_state("gemini"),
            Some(ProviderState::Healthy),
            "{err:?}"
        );
        assert!(
            is_cooldown(f.s.model_state("gemini", "gemini-flash")),
            "{err:?}"
        );
        assert_eq!(
            f.s.model_state("gemini", "gemini-flash-lite"),
            Some(ProviderState::Healthy),
            "{err:?}"
        );
    }
}

/// F0-17: timeout no gemini-flash não bloqueia o gemini-flash-lite.
#[test]
fn timeout_on_flash_does_not_block_flash_lite() {
    let f = fx(SIBLINGS);
    fail_flash(&f, &UpstreamError::FirstTokenTimeout);
    let p = f.pick(&req(10));
    assert_eq!(p.candidates.len(), 1, "{p:?}");
    assert_eq!(&*p.candidates[0].model_id, "gemini-flash-lite");
    assert!(approx(cooldown(p.skipped[0].1.clone()), 30.0));
}

/// F0-17: erro de conexão (rede/DNS/TLS) é do provider: bloqueia os dois modelos.
#[test]
fn network_error_blocks_every_model_of_the_provider() {
    let f = fx(SIBLINGS);
    let d = fail_flash(&f, &UpstreamError::Network("connect refused".into()));
    assert_eq!(d, Some(Duration::from_secs(30)));
    assert!(is_cooldown(f.s.provider_state("gemini")));
    let p = f.pick(&req(10));
    assert!(p.candidates.is_empty(), "{p:?}");
    assert_eq!(p.skipped.len(), 2);
    for (_, s) in p.skipped {
        assert!(approx(cooldown(s), 30.0));
    }
}

/// F0-17: auth continua no escopo do provider (desabilita os dois).
#[test]
fn auth_error_blocks_every_model_of_the_provider() {
    let f = fx(SIBLINGS);
    fail_flash(&f, &UpstreamError::Auth { status: 403 });
    let p = f.pick(&req(10));
    assert!(p.candidates.is_empty(), "{p:?}");
    assert!(
        p.skipped
            .iter()
            .all(|(_, s)| *s == Skip::Disabled(DisabledReason::AuthRejected))
    );
}

/// F0-17: falha depois do 1º token (`fail` com `started`) esfria só o modelo.
#[test]
fn failure_after_first_token_cools_only_the_model() {
    let f = fx(SIBLINGS);
    let p = f.pick(&req(10));
    let mut r = f.s.reserve(&p.candidates[0]).unwrap();
    r.first_token();
    let d = r.fail(&UpstreamError::Network("reset".into()));
    assert_eq!(d, Some(Duration::from_secs(20)));
    assert_eq!(f.s.provider_state("gemini"), Some(ProviderState::Healthy));
    assert!(is_cooldown(f.s.model_state("gemini", "gemini-flash")));
    assert_eq!(
        f.s.model_state("gemini", "gemini-flash-lite"),
        Some(ProviderState::Healthy)
    );
}

#[test]
fn missing_key_and_disabled_provider_are_skipped() {
    let f = fx(r#"
        [[provider]]
        id = "nokey"
        base_url = "http://a"
        auth = { kind = "bearer", env = "MISSING" }
        models = [{ id = "m", upstream = "m" }]

        [[provider]]
        id = "off"
        base_url = "http://b"
        auth = { kind = "none" }
        enabled = false
        models = [{ id = "m" }]

        [[provider]]
        id = "ok"
        base_url = "http://c"
        auth = { kind = "bearer", env = "PRESENT" }
        models = [{ id = "m", upstream = "m" }]

        [[class]]
        id = "kax/t"
        max_packet_tokens = 100000
        route = [{ provider = "nokey", model = "m" }, { provider = "off", model = "m" }, { provider = "ok", model = "m" }]
    "#);
    let p = f.pick(&req(10));
    assert_eq!(
        p.skipped,
        vec![
            (0, Skip::Disabled(DisabledReason::MissingKey)),
            (1, Skip::Disabled(DisabledReason::ConfigDisabled)),
        ]
    );
    assert_eq!(&*p.candidates[0].provider_id, "ok");
    assert_eq!(&*p.candidates[0].upstream_model, "m");
}

// ---------------------------------------------------------------- N2: max_concurrent

#[test]
fn max_concurrent_blocks_until_reservation_closes() {
    let f = fx(&single("", ", limits = { max_concurrent = 1 }"));
    let r1 = f.reserve(10);
    assert_eq!(f.snap_model("p", "m").in_flight, 1);
    // slot ocupado: não cabe agora (sem request ao provider)
    assert!(matches!(f.skip(&req(10)), Skip::NotNow { .. }));
    let c = f.only_after_drop(r1);
    // settle solta o slot
    let r2 = f.s.reserve(&c).unwrap();
    assert!(matches!(f.skip(&req(10)), Skip::NotNow { .. }));
    r2.settle(None);
    // fail solta o slot
    let r3 = f.reserve(10);
    r3.fail(&UpstreamError::RequestFault {
        status: 400,
        message: String::new(),
    });
    // drop pós-1º-token solta o slot
    let mut r4 = f.reserve(10);
    r4.first_token();
    drop(r4);
    assert_eq!(f.snap_model("p", "m").in_flight, 0);
    f.only(&req(10));
}

#[test]
fn max_concurrent_at_provider_scope_is_shared_by_models() {
    let f = fx(r#"
        [[provider]]
        id = "p"
        base_url = "http://p"
        auth = { kind = "none" }
        limits = { max_concurrent = 1 }
        models = [{ id = "a", upstream = "a" }, { id = "b", upstream = "b" }]

        [[class]]
        id = "kax/t"
        max_packet_tokens = 100000
        route = [{ provider = "p", model = "a" }, { provider = "p", model = "b" }]
    "#);
    let p = f.pick(&req(10));
    let _held = f.s.reserve(&p.candidates[0]).unwrap();
    let p = f.pick(&req(10));
    assert!(p.candidates.is_empty(), "{p:?}");
    assert_eq!(f.snap_provider("p").in_flight, 1);
}

// ---------------------------------------------------------------- N1: env ausente

#[test]
fn missing_env_in_base_url_disables_provider_with_names() {
    let f = fx(r#"
        [[provider]]
        id = "cf"
        base_url = "https://api/${MISSING}/v1"
        auth = { kind = "none" }
        models = [{ id = "m", upstream = "m" }]

        [[class]]
        id = "kax/t"
        max_packet_tokens = 100000
        route = [{ provider = "cf", model = "m" }]
    "#);
    assert_eq!(f.skip(&req(10)), Skip::Disabled(DisabledReason::MissingEnv));
    let q = f.snap_provider("cf");
    assert_eq!(q.missing_env, vec!["MISSING".to_owned()]);
    assert_eq!(
        q.state,
        StateView::Disabled {
            reason: DisabledReason::MissingEnv
        }
    );
}

// ---------------------------------------------------------------- N3/N5: tier e família

#[test]
fn quota_exposes_tier_and_family() {
    let f = fx(&single("tier = \"credit\"", ", family = \"glm\""));
    assert_eq!(f.snap_provider("p").tier, crate::config::Tier::Credit);
    assert_eq!(f.snap_model("p", "m").family.as_deref(), Some("glm"));
}

#[test]
fn exclude_family_filters_candidates_without_retry_hint() {
    let f = fx(r#"
        [[provider]]
        id = "p"
        base_url = "http://p"
        auth = { kind = "none" }
        models = [{ id = "a", upstream = "a", family = "glm" }, { id = "b", upstream = "b", family = "qwen" }, { id = "c", upstream = "c" }]

        [[class]]
        id = "kax/t"
        max_packet_tokens = 100000
        route = [{ provider = "p", model = "a" }, { provider = "p", model = "b" }, { provider = "p", model = "c" }]
    "#);
    let opts = PickOptions {
        exclude_families: vec!["GLM".into(), "qwen".into()],
        max_tier: None,
    };
    let p = f.s.pick_with("kax/t", &req(10), &opts).unwrap();
    assert_eq!(p.skipped, vec![(0, Skip::Excluded), (1, Skip::Excluded)]);
    assert_eq!(&*p.candidates[0].model_id, "c");
    assert_eq!(p.retry_after(), None);
}

// ---------------------------------------------------------------- pin (F0-14)

#[test]
fn pin_targets_any_model_and_refuses_disabled() {
    let f = fx(r#"
        [[provider]]
        id = "p"
        base_url = "http://p"
        auth = { kind = "none" }
        models = [{ id = "a", upstream = "a" }, { id = "fora", upstream = "f" }]

        [[provider]]
        id = "off"
        base_url = "http://o"
        auth = { kind = "none" }
        enabled = false
        models = [{ id = "m" }]

        [[class]]
        id = "kax/t"
        max_packet_tokens = 100000
        route = [{ provider = "p", model = "a" }]
    "#);
    // modelo fora da rota da classe
    let p = f.s.pick_pinned("kax/t", "p", "fora", &req(10)).unwrap();
    assert_eq!(&*p.candidates[0].upstream_model, "f");
    assert_eq!(
        f.s.pick_pinned("kax/t", "off", "m", &req(10)),
        Err(PinError::Disabled(DisabledReason::ConfigDisabled))
    );
    assert_eq!(
        f.s.pick_pinned("kax/t", "p", "nope", &req(10)),
        Err(PinError::UnknownTarget)
    );
    assert_eq!(
        f.s.pick_pinned("kax/nope", "p", "a", &req(10)),
        Err(PinError::Pick(PickError::UnknownClass("kax/nope".into())))
    );
}

// ---------------------------------------------------------------- headers x-ratelimit-*

#[test]
fn observed_remaining_corrects_owning_scope() {
    let f = fx(&single(
        "limits = { rpm = 100 }",
        ", limits = { rpd = 20, tpd = 200000 }",
    ));
    let r = f.reserve(10);
    r.observe_remaining(&Remaining {
        rpm: Some(7),
        tpm: None,
        rpd: Some(3),
        tpd: Some(150_000),
    });
    r.settle(None);
    // rpm é do provider; rpd/tpd são do modelo.
    assert_eq!(f.snap_provider("p").rpm_remaining, Some(7));
    let m = f.snap_model("p", "m");
    assert_eq!(m.rpd_remaining, Some(3));
    // settle depois da observação: o estimado (10) já está contido no header.
    assert_eq!(m.tpd_remaining, Some(150_000));
}

impl Fx {
    fn only_after_drop(&self, r: Reservation) -> Candidate {
        drop(r);
        self.only(&req(10))
    }
}

#[test]
fn max_tier_excludes_credit_and_paid_steps() {
    let f = fx(r#"
        [[provider]]
        id = "free"
        base_url = "http://f"
        auth = { kind = "none" }
        models = [{ id = "m", upstream = "m" }]

        [[provider]]
        id = "cred"
        base_url = "http://c"
        auth = { kind = "none" }
        tier = "credit"
        models = [{ id = "m", upstream = "m" }]

        [[provider]]
        id = "pago"
        base_url = "http://p"
        auth = { kind = "none" }
        tier = "paid"
        models = [{ id = "m", upstream = "m" }]

        [[class]]
        id = "kax/t"
        max_packet_tokens = 100000
        route = [{ provider = "free", model = "m" }, { provider = "cred", model = "m" }, { provider = "pago", model = "m" }]
    "#);
    let only_free = PickOptions {
        max_tier: Some(crate::config::Tier::Free),
        ..Default::default()
    };
    let p = f.s.pick_with("kax/t", &req(10), &only_free).unwrap();
    assert_eq!(p.candidates.len(), 1);
    assert_eq!(p.skipped, vec![(1, Skip::Excluded), (2, Skip::Excluded)]);
    let upto_credit = PickOptions {
        max_tier: Some(crate::config::Tier::Credit),
        ..Default::default()
    };
    assert_eq!(
        f.s.pick_with("kax/t", &req(10), &upto_credit)
            .unwrap()
            .candidates
            .len(),
        2
    );
}
