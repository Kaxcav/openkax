//! Execução de uma tarefa delegada (F2-5): worktree → pacote → operário →
//! parse → apply → verify, com retry no próximo candidato da rota.
//!
//! A chamada ao operário reaproveita o despacho do `/v1/chat/completions`
//! (`AppState::complete`): pick, reserva, settle e ledger de request são os
//! normais; aqui só se decide qual provider pular na tentativa seguinte.

use std::path::Path;
use std::time::{Instant, SystemTime};

use axum::http::StatusCode;
use serde_json::Value;

use super::{
    TaskPacket, TaskResult, TaskStatus, TokenUsage, WorkerOutput, build_request, parse_response,
};
use crate::ledger::{self, LedgerEntry, TaskRecord};
use crate::server::{AppState, PROVIDER_HEADER};
use crate::translate::openai::MessageContent;
use crate::verify::{self, RunOptions, VerifyConfig};
use crate::worktree;

/// Limite do corpo do gateway lido de volta (a resposta do operário).
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Executa a tarefa e devolve o veredito; falhas de infraestrutura viram
/// `needs_lead` com o motivo em `verify_log` (o lead assume).
pub async fn run_task(
    app: &AppState,
    repo: &Path,
    base_ref: &str,
    packet: &TaskPacket,
) -> TaskResult {
    let started = Instant::now();
    let mut st = Progress::default();
    let status = attempt_loop(app, repo, base_ref, packet, &mut st).await;
    let result = TaskResult {
        id: packet.id.clone(),
        status,
        provider: st.provider.clone().unwrap_or_default(),
        model: st.model.unwrap_or_default(),
        attempts: st.attempts,
        diff: st.diff,
        verify_log: st.log,
        tokens: st.tokens,
        latency_ms: started.elapsed().as_millis() as u64,
    };
    app.ledger().record(LedgerEntry::Task(TaskRecord {
        ts: ledger::rfc3339(SystemTime::now()),
        id: result.id.clone(),
        class: packet.class.clone(),
        provider: st.provider,
        attempts: result.attempts,
        status,
        verify_ms: st.verify_ms,
    }));
    result
}

#[derive(Default)]
struct Progress {
    attempts: u32,
    /// Providers que já responderam; o retry pula todos.
    tried: Vec<String>,
    provider: Option<String>,
    model: Option<String>,
    diff: String,
    log: String,
    tokens: TokenUsage,
    verify_ms: u64,
}

/// Por que uma tentativa não chegou a `accepted`.
enum Failure {
    /// Vale tentar o próximo candidato com este feedback anexado.
    Retry(String),
    /// Outro provider não ajuda (pacote grande, sem candidato, infra).
    Stop(String),
}

/// Resposta do operário já lida do gateway.
struct Reply {
    provider: String,
    model: String,
    content: String,
    tokens: TokenUsage,
}

/// Roda trabalho bloqueante (git, processos) fora dos workers do tokio.
async fn blocking<T, E>(f: impl FnOnce() -> Result<T, E> + Send + 'static) -> Result<T, String>
where
    T: Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r.map_err(|e| e.to_string()),
        Err(e) => Err(format!("tarefa bloqueante falhou: {e}")),
    }
}

async fn attempt_loop(
    app: &AppState,
    repo: &Path,
    base_ref: &str,
    packet: &TaskPacket,
    st: &mut Progress,
) -> TaskStatus {
    let wt = {
        let (repo, base, id) = (repo.to_owned(), base_ref.to_owned(), packet.id.clone());
        match blocking(move || worktree::create(&repo, &base, &id)).await {
            Ok(wt) => wt,
            Err(e) => {
                st.log = format!("worktree: {e}");
                return TaskStatus::NeedsLead;
            }
        }
    };
    let opts = match verify_options(repo) {
        Ok(o) => o,
        Err(e) => {
            st.log = e;
            return TaskStatus::NeedsLead;
        }
    };

    let mut feedback: Option<String> = None;
    while st.attempts < packet.max_attempts {
        if feedback.is_some() {
            let path = wt.path.clone();
            if let Err(e) = blocking(move || worktree::reset(&path)).await {
                st.log = format!("worktree: {e}");
                return TaskStatus::NeedsLead;
            }
        }
        match one_attempt(app, &wt.path, packet, feedback.as_deref(), opts, st).await {
            Ok(()) => {
                let msg = format!("kax({}): {}", packet.id, packet.objective.trim());
                let path = wt.path.clone();
                if let Err(e) = blocking(move || worktree::commit_all(&path, &msg)).await {
                    st.log = format!("commit: {e}");
                    return TaskStatus::NeedsLead;
                }
                return TaskStatus::Accepted;
            }
            Err(Failure::Retry(why)) => {
                let (tail, _) = verify::truncate_tail(why.as_bytes(), opts.max_log_bytes);
                st.log = tail.clone();
                feedback = Some(tail);
            }
            Err(Failure::Stop(why)) => {
                st.log = why;
                break;
            }
        }
    }
    TaskStatus::NeedsLead
}

/// Opções do verify: `<repo>/.kax/verify.toml` se existir, senão os padrões.
fn verify_options(repo: &Path) -> Result<RunOptions, String> {
    let path = repo.join(".kax").join("verify.toml");
    if !path.is_file() {
        return Ok(RunOptions::default());
    }
    let cfg = VerifyConfig::load(&path).map_err(|e| e.to_string())?;
    Ok(RunOptions::from_config(&cfg))
}

async fn one_attempt(
    app: &AppState,
    wt: &Path,
    packet: &TaskPacket,
    feedback: Option<&str>,
    opts: RunOptions,
    st: &mut Progress,
) -> Result<(), Failure> {
    let (max_packet, cpt) = app
        .packet_limits(&packet.class)
        .ok_or_else(|| Failure::Stop(format!("classe desconhecida: {}", packet.class)))?;
    let mut request =
        build_request(packet, wt, max_packet, cpt).map_err(|e| Failure::Stop(e.to_string()))?;
    if let Some(prev) = feedback
        && let Some(MessageContent::Text(body)) =
            request.messages.last_mut().and_then(|m| m.content.as_mut())
    {
        body.push_str(&format!(
            "\n# Tentativa anterior falhou\nA saída anterior foi recusada com:\n```\n{prev}\n```\nCorrija e devolva o resultado completo no mesmo formato.\n"
        ));
    }

    let response = app.complete(request, &st.tried).await;
    let reply = read_reply(response).await?;
    st.attempts += 1;
    st.tried.push(reply.provider.clone());
    st.provider = Some(reply.provider.clone());
    st.model = Some(reply.model.clone());
    st.tokens.input += reply.tokens.input;
    st.tokens.output += reply.tokens.output;

    let output =
        parse_response(&reply.content, packet).map_err(|e| Failure::Retry(e.to_string()))?;
    match output {
        WorkerOutput::Diff { diff, .. } => {
            let (path, max) = (wt.to_owned(), opts.max_log_bytes);
            blocking(move || verify::apply_diff(&path, &diff, max))
                .await
                .map_err(Failure::Retry)?;
        }
        WorkerOutput::Files(files) => {
            let path = wt.to_owned();
            blocking(move || write_files(&path, &files))
                .await
                .map_err(Failure::Retry)?;
        }
    }
    let path = wt.to_owned();
    st.diff = blocking(move || worktree::staged_diff(&path))
        .await
        .unwrap_or_default();

    let (path, cmds, expect) = (
        wt.to_owned(),
        packet.accept.verify.clone(),
        packet.accept.expect.clone(),
    );
    let t0 = Instant::now();
    let report = blocking(move || verify::run_verify(&path, &cmds, &expect, opts))
        .await
        .map_err(Failure::Stop)?;
    st.verify_ms += t0.elapsed().as_millis() as u64;
    st.log = report
        .outcomes
        .iter()
        .map(|o| format!("$ {}\n{}", o.command, o.log))
        .collect::<Vec<_>>()
        .join("\n");
    if report.passed {
        Ok(())
    } else {
        Err(Failure::Retry(st.log.clone()))
    }
}

/// Lê a resposta (não-streaming) do despacho; qualquer não-2xx encerra.
async fn read_reply(response: axum::response::Response) -> Result<Reply, Failure> {
    let status = response.status();
    let provider = response
        .headers()
        .get(PROVIDER_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = axum::body::to_bytes(response.into_body(), MAX_BODY_BYTES)
        .await
        .map_err(|e| Failure::Stop(format!("gateway: corpo ilegível: {e}")))?;
    if status != StatusCode::OK {
        let text = String::from_utf8_lossy(&bytes);
        return Err(Failure::Stop(
            format!(
                "gateway: nenhum operário respondeu (status {})",
                status.as_u16()
            ) + &format!(": {}", text.chars().take(300).collect::<String>()),
        ));
    }
    let v: Value = serde_json::from_slice(&bytes)
        .map_err(|e| Failure::Stop(format!("gateway: resposta inválida: {e}")))?;
    let tok = |k: &str| v["usage"][k].as_u64().unwrap_or(0);
    Ok(Reply {
        provider,
        model: v["model"].as_str().unwrap_or_default().to_owned(),
        content: v["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        tokens: TokenUsage {
            input: tok("prompt_tokens"),
            output: tok("completion_tokens"),
        },
    })
}

/// Grava os arquivos completos do operário (paths já validados pelo parser).
fn write_files(root: &Path, files: &[(String, String)]) -> std::io::Result<()> {
    for (rel, content) in files {
        let full = root.join(rel);
        if let Some(dir) = full.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(full, content)?;
    }
    Ok(())
}
