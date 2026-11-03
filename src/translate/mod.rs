//! Tipos de fio e tradução entre protocolos.
//!
//! - [`openai`]: Chat Completions (entrada `/v1/chat/completions` e todos os upstreams).
//! - [`anthropic`]: Messages API e tradução ⇄ OpenAI (PLANO_MESTRE §3.5, ADR 0006).

pub mod anthropic;
pub mod openai;
