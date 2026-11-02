//! Binário `openkax`.
//!
//! Uso: `openkax [serve]` (padrão) | `openkax mcp` (Fase 2).
//! Env: `OPENKAX_CONFIG` (default `./providers.toml`, recarregado ao mudar),
//! `OPENKAX_BIND` (default `127.0.0.1:8000`), `OPENKAX_LOG` (filtro `tracing`,
//! default `info`). Buckets persistem em `<state_dir>/buckets.json`.

use std::path::PathBuf;
use std::process::ExitCode;

use openkax::config::Config;
use openkax::ledger;
use openkax::platform::{self, Platform};
use openkax::provider::Secrets;
use openkax::server::{self, AppState, Deps};

fn main() -> ExitCode {
    // rustls-no-provider: sem isto, o primeiro reqwest::Client dá panic (ADR 0002).
    openkax::provider::install_crypto_provider();
    // .env só no dev; ausência não é erro.
    let _ = dotenvy::dotenv();
    let sub = std::env::args().nth(1);
    let filter = tracing_subscriber::EnvFilter::try_from_env("OPENKAX_LOG")
        .unwrap_or_else(|_| "info".into());
    if sub.as_deref() == Some("mcp") {
        // stdout é o canal do protocolo MCP: logs só em stderr.
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }

    match sub.as_deref() {
        None | Some("serve") => run_serve(),
        Some("mcp") => run_mcp(),
        Some("clean") => run_clean(),
        Some(other) => {
            eprintln!("subcomando desconhecido: {other}\nuso: openkax [serve|mcp|clean]");
            ExitCode::from(2)
        }
    }
}

/// `openkax clean [--repo <path>] [--all]`: remove worktrees de tarefas
/// abandonadas (`--all`: todos). Só toca branches `kax/*`.
fn run_clean() -> ExitCode {
    let mut repo = PathBuf::from(".");
    let mut all = false;
    let mut args = std::env::args().skip(2);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--all" => all = true,
            "--repo" => match args.next() {
                Some(p) => repo = PathBuf::from(p),
                None => {
                    eprintln!("--repo exige um caminho");
                    return ExitCode::from(2);
                }
            },
            other => {
                eprintln!(
                    "argumento desconhecido: {other}
uso: openkax clean [--repo <path>] [--all]"
                );
                return ExitCode::from(2);
            }
        }
    }
    match openkax::worktree::clean(&repo, all) {
        Ok(removed) => {
            println!("{} worktree(s) removido(s)", removed.len());
            for id in removed {
                println!("  kax/{id}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// `openkax mcp`: servidor MCP stdio. stdout é o canal do protocolo — logs vão
/// para stderr.
fn run_mcp() -> ExitCode {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .thread_keep_alive(std::time::Duration::from_secs(10))
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("falha ao criar runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(mcp()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn mcp() -> Result<(), Box<dyn std::error::Error>> {
    let path = PathBuf::from(
        std::env::var_os("OPENKAX_CONFIG").unwrap_or_else(|| "providers.toml".into()),
    );
    let config = Config::load(&path)?;
    let secrets = Secrets::from_env(&config);
    let mut deps = Deps::new(config, secrets);
    let ledger_dir = platform::current().ledger_dir();
    deps.ledger = ledger::spawn(ledger_dir.clone(), 256);
    let state = AppState::new(deps)?;
    openkax::mcp::KaxWorkers::new(state)
        .with_ledger_dir(ledger_dir)
        .serve_stdio()
        .await
}

fn run_serve() -> ExitCode {
    // Threads de blocking (DNS, fs) limitadas e soltas após 10 s ociosas (ADR 0002).
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .thread_keep_alive(std::time::Duration::from_secs(10))
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("falha ao criar runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(serve()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let path = PathBuf::from(
        std::env::var_os("OPENKAX_CONFIG").unwrap_or_else(|| "providers.toml".into()),
    );
    let bind = std::env::var("OPENKAX_BIND").unwrap_or_else(|_| "127.0.0.1:8000".into());

    let config = Config::load(&path)?;
    let secrets = Secrets::from_env(&config);
    tracing::info!(config = %path.display(), ?secrets, "config carregada");

    let platform = platform::current();
    let mut deps = Deps::new(config, secrets);
    deps.ledger = ledger::spawn(platform.ledger_dir(), 1024);
    let state = AppState::new(deps)?;

    // F0-10: buckets sobrevivem a restart; o providers.toml recarrega sozinho.
    let snapshot = platform.state_dir().join("buckets.json");
    match state.restore_state(&snapshot) {
        Ok(true) => tracing::info!(arquivo = %snapshot.display(), "buckets restaurados"),
        Ok(false) => {}
        Err(e) => tracing::warn!(arquivo = %snapshot.display(), "snapshot ignorado: {e}"),
    }
    let _watcher = server::watch_config(path.clone(), state.clone(), Secrets::from_env)
        .inspect_err(|e| tracing::warn!("hot-reload desligado: {e}"))
        .ok();
    let saver = {
        let (state, snapshot) = (state.clone(), snapshot.clone());
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SNAPSHOT_EVERY);
            tick.tick().await;
            loop {
                tick.tick().await;
                save(&state, &snapshot);
            }
        })
    };

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "openkax ouvindo");
    server::serve(listener, state.clone(), async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    saver.abort();
    save(&state, &snapshot);
    Ok(())
}

/// Intervalo do snapshot periódico dos buckets (além do salvamento no shutdown).
const SNAPSHOT_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

fn save(state: &AppState, path: &std::path::Path) {
    if let Err(e) = state.save_state(path) {
        tracing::warn!(arquivo = %path.display(), "falha ao salvar buckets: {e}");
    }
}
