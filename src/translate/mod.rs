//! Tipos de fio e tradução entre protocolos.
//!
//! - [`openai`]: Chat Completions (entrada `/v1/chat/completions` e todos os upstreams).
//! - `anthropic` (Fase 1): Messages API e tradução ⇄ OpenAI (PLANO_MESTRE §3.5).

pub mod openai;
