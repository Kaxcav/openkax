//! Execução de uma tarefa delegada (F2-5): worktree → pacote → operário →
//! parse → apply → verify, com retry no próximo candidato da rota.
//!
//! A chamada ao operário reaproveita o despacho do `/v1/chat/completions`
//! (`AppState::complete`): pick, reserva, settle e ledger de request são os
//! normais; aqui só se decide qual provider pular na tentativa seguinte.

use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use axum::http::StatusCode;
use serde_json::Value;

use super::{
    TaskPacket, TaskResult, TaskStatus, TokenUsage, WorkerOutput, build_request, parse_response,
    write_paths,
};
use crate::config::Config;
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
    // Fail closed antes de gastar uma chamada ao operário.
    if let Err(e) = verify::check_sandbox(&opts) {
        st.log = e.to_string();
        return TaskStatus::NeedsLead;
    }
    // Prefetch (rede, fora da caixa) sobre o base_ref, antes de qualquer diff do operário.
    if !opts.prefetch.is_empty() {
        let (path, o) = (wt.path.clone(), opts.clone());
        if let Err(e) = blocking(move || verify::run_prefetch(&path, &o)).await {
            st.log = e;
            return TaskStatus::NeedsLead;
        }
    }
    let scope = {
        let path = wt.path.clone();
        match blocking(move || worktree::head(&path)).await {
            Ok(base) => Scope {
                base,
                allowed: write_paths(packet),
            },
            Err(e) => {
                st.log = format!("worktree: {e}");
                return TaskStatus::NeedsLead;
            }
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
        let attempt = one_attempt(
            app,
            &wt.path,
            packet,
            feedback.as_deref(),
            &opts,
            &scope,
            st,
        );
        match attempt.await {
            Ok(()) => {
                // O verify rodou código do operário: o que vai virar commit passa
                // de novo pela guarda (inclusive commits feitos durante o verify).
                match stage_and_guard(&wt.path, &scope).await {
                    Ok(diff) => st.diff = diff,
                    Err(Failure::Retry(why) | Failure::Stop(why)) => {
                        st.log = format!("depois do verify: {why}");
                        return TaskStatus::NeedsLead;
                    }
                }
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
/// Um `env_passthrough` não vazio é conferido contra as credenciais do
/// `providers.toml` (o mesmo `OPENKAX_CONFIG` do `openkax mcp`).
fn verify_options(repo: &Path) -> Result<RunOptions, String> {
    let path = repo.join(".kax").join("verify.toml");
    if !path.is_file() {
        return Ok(RunOptions::default());
    }
    let cfg = VerifyConfig::load(&path).map_err(|e| e.to_string())?;
    if !cfg.env_passthrough.is_empty() {
        let providers = PathBuf::from(
            std::env::var_os("OPENKAX_CONFIG").unwrap_or_else(|| "providers.toml".into()),
        );
        let providers = Config::load(&providers)
            .map_err(|e| format!("env_passthrough: não deu para conferir o providers.toml: {e}"))?;
        cfg.check_provider_secrets(&providers)
            .map_err(|e| e.to_string())?;
    }
    Ok(RunOptions::from_config(&cfg))
}

/// O que a tarefa pode alterar: paths `write` a partir do commit base.
struct Scope {
    base: String,
    allowed: Vec<String>,
}

/// `git add -A` e a guarda C2 ([`verify::check_staged`]) contra o commit base.
/// Fora do escopo → reset do worktree e `Retry` com o motivo; erro de git → `Stop`.
async fn stage_and_guard(wt: &Path, scope: &Scope) -> Result<String, Failure> {
    let path = wt.to_owned();
    let diff = blocking(move || worktree::staged_diff(&path))
        .await
        .map_err(|e| Failure::Stop(format!("worktree: {e}")))?;
    let (path, base, allowed) = (wt.to_owned(), scope.base.clone(), scope.allowed.clone());
    let checked = blocking(move || {
        Ok::<_, std::convert::Infallible>(verify::check_staged(&path, &base, &allowed))
    })
    .await;
    match checked {
        Ok(Ok(())) => Ok(diff),
        Ok(Err(verify::RunError::OutOfScope(why))) => {
            let path = wt.to_owned();
            blocking(move || worktree::reset(&path))
                .await
                .map_err(|e| Failure::Stop(format!("{why}; reset do worktree falhou: {e}")))?;
            Err(Failure::Retry(format!("alteração fora do escopo: {why}")))
        }
        Ok(Err(e)) => Err(Failure::Stop(e.to_string())),
        Err(e) => Err(Failure::Stop(e)),
    }
}

async fn one_attempt(
    app: &AppState,
    wt: &Path,
    packet: &TaskPacket,
    feedback: Option<&str>,
    opts: &RunOptions,
    scope: &Scope,
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
            // Paths já validados; falha aqui é do repo (symlink, disco), não do operário.
            let path = wt.to_owned();
            blocking(move || write_files(&path, &files))
                .await
                .map_err(Failure::Stop)?;
        }
    }
    // Guarda pós-apply: vale o que o git pôs no índice, não o que o parser leu.
    st.diff = stage_and_guard(wt, scope).await?;

    let (path, cmds, expect, opts) = (
        wt.to_owned(),
        packet.accept.verify.clone(),
        packet.accept.expect.clone(),
        opts.clone(),
    );
    let t0 = Instant::now();
    let report = blocking(move || verify::run_verify(&path, &cmds, &expect, &opts))
        .await
        .map_err(Failure::Stop)?;
    st.verify_ms += t0.elapsed().as_millis() as u64;
    st.log = report
        .warnings
        .iter()
        .cloned()
        .chain(
            report
                .outcomes
                .iter()
                .map(|o| format!("$ {}\n{}", o.command, o.log)),
        )
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
/// Recusa tudo, antes de gravar qualquer um, se o path ou um diretório pai
/// dentro do worktree for symlink (link vindo do `base_ref`, C2).
fn write_files(root: &Path, files: &[(String, String)]) -> Result<(), String> {
    for (rel, _) in files {
        let mut cur = root.to_path_buf();
        for part in Path::new(rel).components() {
            cur.push(part);
            if std::fs::symlink_metadata(&cur).is_ok_and(|m| m.file_type().is_symlink()) {
                let link = cur.strip_prefix(root).unwrap_or(&cur).display().to_string();
                return Err(format!("{rel}: {link} é symlink; escrita recusada"));
            }
        }
    }
    for (rel, content) in files {
        let full = root.join(rel);
        if let Some(dir) = full.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{rel}: {e}"))?;
        }
        std::fs::write(full, content).map_err(|e| format!("{rel}: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(rel: &str) -> Vec<(String, String)> {
        vec![(rel.to_owned(), "x\n".to_owned())]
    }

    #[cfg(unix)]
    #[test]
    fn write_files_refuses_symlinks() {
        use std::os::unix::fs::symlink;
        let wt = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        // Diretório pai é link para fora do worktree.
        symlink(outside.path(), wt.path().join("src")).unwrap();
        let e = write_files(wt.path(), &one("src/new.rs")).unwrap_err();
        assert!(e.contains("symlink"), "{e}");
        assert!(!outside.path().join("new.rs").exists());
        // O próprio arquivo é link.
        let target = outside.path().join("alvo");
        std::fs::write(&target, "original").unwrap();
        std::fs::create_dir(wt.path().join("lib")).unwrap();
        symlink(&target, wt.path().join("lib/x.rs")).unwrap();
        assert!(write_files(wt.path(), &one("lib/x.rs")).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
        // Sem link no caminho, grava.
        write_files(wt.path(), &one("lib/y.rs")).unwrap();
        assert_eq!(
            std::fs::read_to_string(wt.path().join("lib/y.rs")).unwrap(),
            "x\n"
        );
    }

    /// No Windows, symlink exige admin ou Modo de Desenvolvedor; junction
    /// (`mklink /J`) não, e o std a trata como symlink. Cobre o diretório pai;
    /// o caso do próprio arquivo como link fica com o teste unix (CI Linux).
    #[cfg(windows)]
    #[test]
    fn write_files_refuses_junction_parent() {
        let wt = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let st = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(wt.path().join("src"))
            .arg(outside.path())
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(st.success(), "mklink /J falhou");
        let e = write_files(wt.path(), &one("src/new.rs")).unwrap_err();
        assert!(e.contains("symlink"), "{e}");
        assert!(!outside.path().join("new.rs").exists());
        write_files(wt.path(), &one("lib/y.rs")).unwrap();
    }
}
