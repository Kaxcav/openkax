# 0002 — Stack Rust e perfil de release

Data: 2026-11-02
Status: aceita

## Contexto

O alvo é um binário único Windows < 8 MB com RSS ocioso < 10 MB (PLANO_MESTRE §3.1), desenvolvido e testado em Linux/WSL (ADR 0001) e com `.exe` gerado no CI `windows-latest` (MSVC). A stack precisa de HTTP server, cliente HTTP com TLS sem OpenSSL, streaming SSE, TOML, MCP stdio e hot-reload de arquivo. Também precisa compilar sem toolchain C extra no Windows.

## Decisão

Dependências (versões travadas no `Cargo.lock`):

| Papel | Crate | Notas |
|---|---|---|
| Runtime | `tokio` 1.x | `rt-multi-thread`, **2 workers** fixos em `main` |
| HTTP server | `axum` 0.8 | features default (http1, json, tokio) |
| HTTP client | `reqwest` 0.13 | `default-features = false`, `rustls-no-provider`, `http2`, `json`, `stream` |
| TLS | `rustls` 0.23 + **`ring`** | provider instalado em `server::install_crypto_provider()`; raízes via `rustls-platform-verifier` (store do SO) |
| Serde | `serde`, `serde_json`, `toml` 1.x | `deny_unknown_fields` na config |
| Logs | `tracing`, `tracing-subscriber` (fmt, env-filter, ansi), `tracing-appender` | filtro via `OPENKAX_LOG` |
| Utilitários | `bytes`, `futures`, `thiserror`, `dotenvy` | `.env` só no dev |
| MCP | `rmcp` 3.x (`server`, `transport-io`) | usado na Fase 2; morto no binário até lá |
| Hot-reload | `notify` 8.x | |
| Testes | `wiremock` 0.6 | providers mockados nos testes de fallback |

Perfil release:

```toml
[profile.release]
lto = "fat"
codegen-units = 1
opt-level = "z"
panic = "abort"
strip = true
```

Fora da v1: SQLite (o ledger é JSONL e os buckets ficam em snapshot JSON), OpenSSL/`native-tls`, `aws-lc-rs`, `chrono` (timestamps RFC 3339 formatados à mão), `clap` (só 2 subcomandos).

### Por que `ring` e não o default do reqwest 0.13

No reqwest 0.13 a feature `rustls` puxa `aws-lc-rs`, que no MSVC exige CMake/NASM e gera binário maior. Com `rustls-no-provider` + `rustls/ring` não há dependência de C toolchain além do que o `ring` já embute, e o tamanho fica menor. Custo: chamar `install_default()` uma vez por processo, o que o `AppState::new` já faz.

## Consequências

- Medição inicial (contrato da Fase 0, Linux x86_64, rustc 1.99.1): **binário release 3,01 MB**, sem `openssl-sys` nem `aws-lc-rs` na árvore (`cargo tree -i`). O número oficial continua sendo o `.exe` do CI (ADR 0001).
- `panic = "abort"`: um panic derruba o processo. Os caminhos de request não podem dar panic (nada de `unwrap` em dado externo). Os testes rodam no perfil dev (unwind).
- `rmcp` e `notify` já estão no manifesto; o tamanho vai ser remedido quando entrarem em uso (Fase 0 hot-reload, Fase 2 MCP).
- Trocar de crate TLS ou de runtime exige um ADR novo.
