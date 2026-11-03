//! Servidor MCP `kax-workers` (`openkax mcp`, stdio) — **Fase 2**.
//!
//! Tools (PLANO_MESTRE §3.6): `delegate`, `delegate_batch`, `status`, `quota`,
//! `report`. `delegate` executa uma tarefa de ponta a ponta (F2-5, ver `run`);
//! `delegate_batch` despacha um lote em background e `status` o consulta
//! (F2-6, ver `batch`); `report` agrega o ledger; `quota` devolve o snapshot
//! do scheduler.

mod batch;
mod packet;
mod run;
mod types;

use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    tool, tool_handler, tool_router,
};
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::server::AppState;

pub use batch::Batches;
pub use packet::*;
pub use run::run_task;
pub use types::*;

/// Etapa em que os handlers estão (vira o campo `stage` dos erros).
const STAGE: &str = "F2-6";

/// Corpo JSON de erro estruturado devolvido pelas tools (`is_error = true`).
pub fn tool_error(code: &str, message: &str) -> String {
    json!({ "error": { "code": code, "stage": STAGE, "message": message } }).to_string()
}

fn invalid(message: String) -> String {
    tool_error("invalid_input", &message)
}

/// Servidor MCP; segura o estado do gateway para consultar o scheduler.
#[derive(Clone)]
pub struct KaxWorkers {
    app: AppState,
    batches: Batches,
    /// Diretório do ledger lido pelo `report` (`None` → relatório vazio).
    ledger_dir: Option<PathBuf>,
    tool_router: ToolRouter<Self>,
}

impl KaxWorkers {
    pub fn new(app: AppState) -> Self {
        Self {
            app,
            batches: Batches::default(),
            ledger_dir: None,
            tool_router: Self::tool_router(),
        }
    }

    /// Diretório do ledger que o `report` lê.
    pub fn with_ledger_dir(mut self, dir: PathBuf) -> Self {
        self.ledger_dir = Some(dir);
        self
    }

    /// Atende MCP em stdin/stdout até o cliente fechar.
    pub async fn serve_stdio(self) -> Result<(), Box<dyn std::error::Error>> {
        let running = self.serve(rmcp::transport::stdio()).await?;
        running.waiting().await?;
        Ok(())
    }
}

#[tool_router]
impl KaxWorkers {
    #[tool(
        description = "Delega UMA tarefa verificada a um operário (worktree isolado, verify determinístico)."
    )]
    async fn delegate(&self, Parameters(p): Parameters<DelegateParams>) -> Result<String, String> {
        p.validate().map_err(invalid)?;
        let result = run_task(&self.app, Path::new(&p.repo), &p.base_ref, &p.task).await;
        serde_json::to_string(&result).map_err(|e| tool_error("internal", &e.to_string()))
    }

    #[tool(
        description = "Delega um lote de tarefas em paralelo; cada uma roda em worktree próprio e volta com resultado estruturado."
    )]
    async fn delegate_batch(
        &self,
        Parameters(p): Parameters<DelegateBatchParams>,
    ) -> Result<String, String> {
        p.validate().map_err(invalid)?;
        let queued = self.batches.start(
            &self.app,
            Path::new(&p.repo),
            &p.base_ref,
            p.tasks,
            p.parallelism,
        );
        serde_json::to_string(&queued).map_err(|e| tool_error("internal", &e.to_string()))
    }

    #[tool(description = "Estado e resultados de um lote já despachado.")]
    async fn status(&self, Parameters(p): Parameters<StatusParams>) -> Result<String, String> {
        p.validate().map_err(invalid)?;
        let status = self.batches.status(&p.batch_id).ok_or_else(|| {
            tool_error("not_found", &format!("lote desconhecido: {}", p.batch_id))
        })?;
        serde_json::to_string(&status).map_err(|e| tool_error("internal", &e.to_string()))
    }

    #[tool(description = "Saldo de quota por provider (snapshot do scheduler).")]
    async fn quota(&self) -> Result<String, String> {
        serde_json::to_string(&self.app.scheduler().snapshot())
            .map_err(|e| tool_error("internal", &e.to_string()))
    }

    #[tool(
        description = "Relatório do ledger: aceitação por classe e por provider no período (day|week|month)."
    )]
    async fn report(&self, Parameters(p): Parameters<ReportParams>) -> Result<String, String> {
        let dir = self.ledger_dir.clone().unwrap_or_default();
        let period = p.period;
        let report = tokio::task::spawn_blocking(move || {
            let since = crate::report::since_for_period(period, std::time::SystemTime::now());
            crate::report::build(&dir, &since)
        })
        .await
        .map_err(|e| tool_error("internal", &e.to_string()))?;
        serde_json::to_string(&report).map_err(|e| tool_error("internal", &e.to_string()))
    }
}

#[tool_handler(router = self.tool_router, name = "kax-workers", version = "0.1.0")]
impl ServerHandler for KaxWorkers {}
