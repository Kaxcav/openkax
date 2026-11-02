//! Binário `openkax`.
//!
//! Uso: `openkax [serve]` (padrão) | `openkax mcp` (Fase 2).
//! Env: `OPENKAX_CONFIG` (default `./providers.toml`), `OPENKAX_BIND`
//! (default `127.0.0.1:8000`), `OPENKAX_LOG` (filtro `tracing`, default `info`).

use std::path::PathBuf;
use std::process::ExitCode;

use openkax::config::Config;
use openkax::ledger;
use openkax::platform::{self, Platform};
use openkax::provider::Secrets;
use openkax::server::{self, AppState, Deps};

fn main() -> ExitCode {
    // .env só no dev; ausência não é erro.
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("OPENKAX_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    match std::env::args().nth(1).as_deref() {
        None | Some("serve") => run_serve(),
        Some("mcp") => {
            eprintln!("openkax mcp: Fase 2");
            ExitCode::from(2)
        }
        Some(other) => {
            eprintln!("subcomando desconhecido: {other}\nuso: openkax [serve|mcp]");
            ExitCode::from(2)
        }
    }
}

fn run_serve() -> ExitCode {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
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

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "openkax ouvindo");
    server::serve(listener, state, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}
