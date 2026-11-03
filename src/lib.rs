//! OpenKax — gateway local de IA.
//!
//! Mapa dos módulos (PLANO_MESTRE §1):
//! - [`config`]: `providers.toml` (provider = dado).
//! - [`scheduler`]: buckets RPM/TPM/RPD/TPD, estado por provider, escolha de candidatos.
//! - [`provider`]: cliente HTTP OpenAI-compat genérico + classificação de erros.
//! - [`translate`]: tipos de fio OpenAI/Anthropic e conversão entre eles.
//! - [`server`]: rotas HTTP (`axum`) e despacho com fallback pré-1º-token.
//! - [`ledger`]: JSONL de metadados por request (nunca corpo, nunca credencial).
//! - [`mcp`]: servidor MCP `kax-workers` (Fase 2).
//! - [`verify`]: runners de verificação em worktree (Fase 2).
//! - [`platform`]: integração com o SO atrás do trait `Platform` (ADR 0001).

pub mod config;
pub mod ledger;
pub mod mcp;
pub mod platform;
pub mod provider;
pub mod scheduler;
pub mod server;
pub mod translate;
pub mod verify;
