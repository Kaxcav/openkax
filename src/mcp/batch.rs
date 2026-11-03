//! Lotes de `delegate_batch` (F2-6): cada tarefa roda em background, limitada
//! por um semáforo de `parallelism`; `status` consulta o registro em memória.
//!
//! O registro vive só enquanto o processo `openkax mcp` vive (o ledger guarda o
//! histórico durável). Guarda no máximo `MAX_BATCHES` lotes: ao passar disso,
//! o lote concluído mais antigo sai.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::Semaphore;

use super::{
    BatchState, BatchStatus, TaskEntry, TaskPacket, TaskResult, TaskState, TaskStatus, TokenUsage,
    run_task,
};
use crate::server::AppState;

/// Lotes mantidos em memória.
const MAX_BATCHES: usize = 32;

#[derive(Debug)]
struct Batch {
    entries: Vec<TaskEntry>,
    running: u32,
    peak_running: u32,
}

impl Batch {
    fn snapshot(&self, batch_id: &str) -> BatchStatus {
        let finished = self.entries.iter().filter(|e| e.result.is_some()).count() as u32;
        let total = self.entries.len() as u32;
        BatchStatus {
            batch_id: batch_id.to_owned(),
            state: if finished == total {
                BatchState::Done
            } else {
                BatchState::Running
            },
            total,
            finished,
            peak_running: self.peak_running,
            tasks: self.entries.clone(),
            results: self
                .entries
                .iter()
                .filter_map(|e| e.result.clone())
                .collect(),
        }
    }
}

#[derive(Debug, Default)]
struct Registry {
    order: Vec<String>,
    batches: HashMap<String, Arc<Mutex<Batch>>>,
}

/// Registro de lotes; clonável (compartilha o mesmo estado).
#[derive(Debug, Clone, Default)]
pub struct Batches {
    inner: Arc<Mutex<Registry>>,
    seq: Arc<AtomicU64>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Batches {
    /// Registra o lote com todas as tarefas em `queued`, dispara a execução em
    /// background e devolve na hora. Precisa de runtime tokio.
    pub fn start(
        &self,
        app: &AppState,
        repo: &Path,
        base_ref: &str,
        tasks: Vec<TaskPacket>,
        parallelism: u32,
    ) -> BatchStatus {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let batch_id = format!("B-{millis}-{}", self.seq.fetch_add(1, Ordering::Relaxed));
        let batch = Arc::new(Mutex::new(Batch {
            entries: tasks
                .iter()
                .map(|t| TaskEntry {
                    id: t.id.clone(),
                    state: TaskState::Queued,
                    result: None,
                })
                .collect(),
            running: 0,
            peak_running: 0,
        }));
        let first = lock(&batch).snapshot(&batch_id);
        self.register(&batch_id, batch.clone());

        let permits = Arc::new(Semaphore::new(parallelism.max(1) as usize));
        for (idx, packet) in tasks.into_iter().enumerate() {
            let (app, repo, base) = (app.clone(), repo.to_owned(), base_ref.to_owned());
            let (batch, permits) = (batch.clone(), permits.clone());
            tokio::spawn(async move {
                let Ok(_permit) = permits.acquire_owned().await else {
                    return;
                };
                {
                    let mut b = lock(&batch);
                    b.running += 1;
                    b.peak_running = b.peak_running.max(b.running);
                    b.entries[idx].state = TaskState::Running;
                }
                let id = packet.id.clone();
                let handle =
                    tokio::spawn(async move { run_task(&app, &repo, &base, &packet).await });
                let result = handle.await.unwrap_or_else(|e| panic_result(&id, &e));
                let mut b = lock(&batch);
                b.running -= 1;
                let e = &mut b.entries[idx];
                e.state = result.status.into();
                e.result = Some(result);
            });
        }
        first
    }

    /// Estado atual do lote, ou `None` se o id é desconhecido.
    pub fn status(&self, batch_id: &str) -> Option<BatchStatus> {
        let batch = lock(&self.inner).batches.get(batch_id)?.clone();
        let snap = lock(&batch).snapshot(batch_id);
        Some(snap)
    }

    fn register(&self, batch_id: &str, batch: Arc<Mutex<Batch>>) {
        let mut reg = lock(&self.inner);
        reg.order.push(batch_id.to_owned());
        reg.batches.insert(batch_id.to_owned(), batch);
        while reg.order.len() > MAX_BATCHES {
            let done = reg.order.iter().position(|id| {
                reg.batches
                    .get(id)
                    .is_some_and(|b| lock(b).snapshot(id).state == BatchState::Done)
            });
            let Some(pos) = done else { break };
            let id = reg.order.remove(pos);
            reg.batches.remove(&id);
        }
    }
}

/// A task do operário panicou: o lead assume.
fn panic_result(id: &str, e: &tokio::task::JoinError) -> TaskResult {
    TaskResult {
        id: id.to_owned(),
        status: TaskStatus::NeedsLead,
        provider: String::new(),
        model: String::new(),
        attempts: 0,
        diff: String::new(),
        verify_log: format!("execução interrompida: {e}"),
        tokens: TokenUsage::default(),
        latency_ms: 0,
    }
}
