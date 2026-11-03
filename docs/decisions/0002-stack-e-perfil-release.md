# 0002 — Stack Rust e perfil de release

Data: 2026-11-02
Status: aceita

## Contexto

O alvo é um binário único Windows < 8 MB com memória privada < 10 MB ociosa após uso (PLANO_MESTRE §3.1; até a F0-22 o texto dizia "RSS ocioso"), desenvolvido e testado em Linux/WSL (ADR 0001) e com `.exe` gerado no CI `windows-latest` (MSVC). A stack precisa de HTTP server, cliente HTTP com TLS sem OpenSSL, streaming SSE, TOML, MCP stdio e hot-reload de arquivo. Também precisa compilar sem toolchain C extra no Windows.

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

### Critério de memória (F0-22, 2026-11-02)

- O critério passa a ser **memória privada** (Private Bytes / commit) **< 10 MB ociosa após uso**. O working set fica só como informação. No Windows ele inclui as páginas de DLLs do sistema (TLS, rede, store de certificados), que são compartilhadas e o processo não controla. Foi o working set que passou de 10 MB na F0-11 (15,2 MiB), com a memória privada em 4,4 MiB.
- Higiene barata, mantida mesmo com efeito pequeno na medição:
  - `reqwest`: `pool_idle_timeout(30 s)` (default 90 s) e `pool_max_idle_per_host(2)` (default sem teto), em `provider::build_http_client`;
  - `tokio`: `max_blocking_threads(4)` (default 512) e `thread_keep_alive(10 s)`, que já é o default e fica explícito, em `main`.

### Por que `ring` e não o default do reqwest 0.13

No reqwest 0.13 a feature `rustls` puxa `aws-lc-rs`, que no MSVC exige CMake/NASM e gera binário maior. Com `rustls-no-provider` + `rustls/ring` não há dependência de C toolchain além do que o `ring` já embute, e o tamanho fica menor. Custo: chamar `install_default()` uma vez por processo, o que o `AppState::new` já faz.

## Consequências

- Medição inicial (contrato da Fase 0, Linux x86_64, rustc 1.99.1): **binário release 3,01 MB**, sem `openssl-sys` nem `aws-lc-rs` na árvore (`cargo tree -i`). O número oficial continua sendo o `.exe` do CI (ADR 0001).
- `panic = "abort"`: um panic derruba o processo. Os caminhos de request não podem dar panic (nada de `unwrap` em dado externo). Os testes rodam no perfil dev (unwind).
- Memória (F0-22, PC de casa, Windows 11, `.exe` release local). Os dois binários rodaram lado a lado, com porta e `LOCALAPPDATA` próprios: "antes" (sem a higiene, 3 086 336 B) e "depois" (3 086 848 B). Foram 5 requests `kax/coder-fast` com streaming, em sequência, todas servidas pela Groq. Valores em MiB, memória privada / working set:

  | Momento | Antes | Depois |
  |---|---|---|
  | logo após o start | 1,77 / 7,99 | 1,77 / 8,01 |
  | depois de 5 requests | 4,07 / 14,80 | 4,07 / 14,78 |
  | +60 s parado | 3,88 / 14,73 | 3,81 / 14,64 |
  | +5 min parado | 3,86 / 14,82 | 3,82 / 14,74 |

  - Memória privada ociosa após uso = **3,8 MiB**: passa no critério com folga.
  - A diferença entre antes e depois fica no ruído (≤ 0,08 MiB). Com requests em sequência há 1 conexão por host, então o teto de 2 conexões ociosas não atua. As threads de blocking já morriam em 10 s (15 → 10 threads em 60 s nos dois binários). Os ~2 MiB que sobram acima do start não vêm do pool nem das threads de blocking.
  - O ganho esperado da higiene está nas rajadas paralelas (várias conexões por host e mais threads de DNS), que esta medição não cobriu.
- Memória numa rajada do MCP (F2-6, PC de casa, Windows 11, `openkax mcp` do perfil release, só números, critério inalterado). Lote de 8 tarefas, `parallelism = 4`, operários em `wiremock` (4 aceitas, 4 recusadas pelo verify), pico de 4 simultâneas. Memória privada: **antes 1,67 MiB**, **durante (pico amostrado) 2,30 MiB**, **depois +12 s 2,18 MiB**. Os `git`/verify rodam em processos filhos e não entram na conta. Medição única, sem repetição.
- `rmcp` e `notify` já estão no manifesto; o tamanho vai ser remedido quando entrarem em uso (Fase 0 hot-reload, Fase 2 MCP).
- Trocar de crate TLS ou de runtime exige um ADR novo.
