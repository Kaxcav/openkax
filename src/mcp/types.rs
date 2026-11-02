//! Tipos de fio do MCP `kax-workers` (PLANO_MESTRE §3.6).
//!
//! Contrato da Fase 2: os handlers validam estas estruturas; a execução
//! (worktree, provider, verify) chega nas etapas F2-2 em diante.

use std::collections::HashSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Teto de operários simultâneos aceito em `delegate_batch`.
pub const MAX_PARALLELISM: u32 = 16;
/// Paralelismo quando o lead não informa.
pub const DEFAULT_PARALLELISM: u32 = 4;
/// Tentativas quando o lead não informa (1ª + retry no próximo candidato).
pub const DEFAULT_MAX_ATTEMPTS: u32 = 2;
/// Teto de tentativas por tarefa.
pub const MAX_ATTEMPTS: u32 = 3;

fn default_parallelism() -> u32 {
    DEFAULT_PARALLELISM
}

fn default_max_attempts() -> u32 {
    DEFAULT_MAX_ATTEMPTS
}

fn default_true() -> bool {
    true
}

/// Como o operário acessa um arquivo do pacote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileMode {
    /// Vai no pacote como contexto; não pode ser alterado.
    Read,
    /// O operário pode criar/alterar.
    Write,
}

/// Arquivo referenciado pelo pacote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileSpec {
    /// Caminho relativo à raiz do repo.
    pub path: String,
    pub mode: FileMode,
    /// `false` quando o arquivo ainda não existe (operário vai criá-lo).
    #[serde(default = "default_true")]
    pub exists: bool,
}

/// Critério de aceite: comandos do `verify` e o resultado esperado.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Accept {
    /// Comandos rodados no worktree, em ordem.
    pub verify: Vec<String>,
    /// Resultado esperado (ex.: `"exit 0"`).
    pub expect: String,
}

/// Formato que o operário devolve.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    #[default]
    UnifiedDiff,
    FullFile,
}

/// Pacote enxuto de uma tarefa delegada.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskPacket {
    /// Identificador estável (vira a branch `kax/<id>`).
    pub id: String,
    /// Classe de roteamento (`kax/coder-smart`, ...).
    pub class: String,
    /// Objetivo em uma frase.
    pub objective: String,
    pub files: Vec<FileSpec>,
    /// Tipos/traits/assinaturas que o operário deve honrar.
    #[serde(default)]
    pub contracts: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<String>,
    pub accept: Accept,
    #[serde(default)]
    pub output: OutputFormat,
    /// Tentativas (1..=3); a 2ª usa o próximo candidato com o stderr anexado.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
}

/// Veredito de uma tarefa.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Accepted,
    Rejected,
    /// Esgotou tentativas ou a spec é ambígua: o lead faz.
    NeedsLead,
}

/// Tokens consumidos pela tarefa.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TokenUsage {
    #[serde(rename = "in")]
    pub input: u64,
    #[serde(rename = "out")]
    pub output: u64,
}

/// Resultado estruturado de uma tarefa.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskResult {
    pub id: String,
    pub status: TaskStatus,
    pub provider: String,
    pub model: String,
    pub attempts: u32,
    /// Diff unificado aplicado no worktree (vazio se rejeitado sem diff).
    pub diff: String,
    /// Saída do verify, truncada em `max_log_bytes`.
    pub verify_log: String,
    pub tokens: TokenUsage,
    pub latency_ms: u64,
}

/// Estado de um lote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BatchState {
    Running,
    Done,
}

/// Estado de uma tarefa dentro do lote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Running,
    Accepted,
    Rejected,
    NeedsLead,
}

impl From<TaskStatus> for TaskState {
    fn from(s: TaskStatus) -> Self {
        match s {
            TaskStatus::Accepted => Self::Accepted,
            TaskStatus::Rejected => Self::Rejected,
            TaskStatus::NeedsLead => Self::NeedsLead,
        }
    }
}

/// Uma tarefa do lote: estado e, quando pronta, o resultado.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskEntry {
    pub id: String,
    pub state: TaskState,
    pub result: Option<TaskResult>,
}

/// Resposta de `delegate_batch` (tudo em `queued`) e de `status(batch_id)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BatchStatus {
    pub batch_id: String,
    pub state: BatchState,
    pub total: u32,
    /// Tarefas já concluídas (qualquer status).
    pub finished: u32,
    /// Maior número de tarefas rodando ao mesmo tempo até agora.
    pub peak_running: u32,
    /// Todas as tarefas, na ordem de envio, com o estado atual.
    pub tasks: Vec<TaskEntry>,
    /// Resultados já prontos.
    pub results: Vec<TaskResult>,
}

/// Entrada de `delegate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DelegateParams {
    /// Caminho do repo git do projeto.
    pub repo: String,
    /// Ref a partir da qual o worktree é criado.
    pub base_ref: String,
    pub task: TaskPacket,
}

/// Entrada de `delegate_batch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DelegateBatchParams {
    pub repo: String,
    pub base_ref: String,
    pub tasks: Vec<TaskPacket>,
    /// Operários simultâneos (1..=16, padrão 4).
    #[serde(default = "default_parallelism")]
    pub parallelism: u32,
}

/// Entrada de `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatusParams {
    pub batch_id: String,
}

/// Janela do `report`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Period {
    Day,
    Week,
    Month,
}

/// Entrada de `report`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReportParams {
    pub period: Period,
}

impl TaskPacket {
    /// Valida o pacote sem tocar em disco nem rede.
    pub fn validate(&self) -> Result<(), String> {
        let id = &self.id;
        let id_ok = !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !id_ok {
            return Err(format!(
                "id inválido: {id:?} (use letras, dígitos, '-', '_', '.')"
            ));
        }
        if !self.class.starts_with("kax/") {
            return Err(format!("{id}: class deve começar com \"kax/\""));
        }
        if self.objective.trim().is_empty() {
            return Err(format!("{id}: objective vazio"));
        }
        if self.accept.verify.is_empty() || self.accept.verify.iter().any(|c| c.trim().is_empty()) {
            return Err(format!(
                "{id}: accept.verify precisa de ao menos um comando"
            ));
        }
        if !self.files.iter().any(|f| f.mode == FileMode::Write) {
            return Err(format!("{id}: ao menos um arquivo com mode=write"));
        }
        for f in &self.files {
            let p = std::path::Path::new(&f.path);
            let escapes = p
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir));
            if f.path.is_empty() || p.has_root() || p.is_absolute() || escapes {
                return Err(format!("{id}: path fora do repo: {:?}", f.path));
            }
        }
        if !(1..=MAX_ATTEMPTS).contains(&self.max_attempts) {
            return Err(format!(
                "{id}: max_attempts deve estar em 1..={MAX_ATTEMPTS}"
            ));
        }
        Ok(())
    }
}

fn validate_target(repo: &str, base_ref: &str) -> Result<(), String> {
    if repo.trim().is_empty() {
        return Err("repo vazio".into());
    }
    if base_ref.trim().is_empty() {
        return Err("base_ref vazio".into());
    }
    Ok(())
}

impl DelegateParams {
    pub fn validate(&self) -> Result<(), String> {
        validate_target(&self.repo, &self.base_ref)?;
        self.task.validate()
    }
}

impl DelegateBatchParams {
    pub fn validate(&self) -> Result<(), String> {
        validate_target(&self.repo, &self.base_ref)?;
        if self.tasks.is_empty() {
            return Err("tasks vazio".into());
        }
        if !(1..=MAX_PARALLELISM).contains(&self.parallelism) {
            return Err(format!("parallelism deve estar em 1..={MAX_PARALLELISM}"));
        }
        let mut seen = HashSet::new();
        for t in &self.tasks {
            t.validate()?;
            if !seen.insert(t.id.as_str()) {
                return Err(format!("id duplicado: {}", t.id));
            }
        }
        Ok(())
    }
}

impl StatusParams {
    pub fn validate(&self) -> Result<(), String> {
        if self.batch_id.trim().is_empty() {
            return Err("batch_id vazio".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet() -> TaskPacket {
        serde_json::from_value(serde_json::json!({
            "id": "T-014",
            "class": "kax/coder-smart",
            "objective": "Implementar PaymentAdapter",
            "files": [
                { "path": "src/payments/mod.rs", "mode": "read" },
                { "path": "src/payments/stripe.rs", "mode": "write", "exists": false }
            ],
            "accept": { "verify": ["cargo check"], "expect": "exit 0" }
        }))
        .unwrap()
    }

    #[test]
    fn defaults_follow_plan() {
        let t = packet();
        assert_eq!(t.max_attempts, 2);
        assert_eq!(t.output, OutputFormat::UnifiedDiff);
        assert!(t.files[0].exists);
        assert!(!t.files[1].exists);
        t.validate().unwrap();
    }

    #[test]
    fn rejects_bad_packets() {
        let mut t = packet();
        t.files[1].path = "../fora.rs".into();
        assert!(t.validate().is_err());
        let mut t = packet();
        t.class = "gpt-4".into();
        assert!(t.validate().is_err());
        let mut t = packet();
        t.max_attempts = 9;
        assert!(t.validate().is_err());
    }

    #[test]
    fn batch_rejects_duplicate_ids_and_bad_parallelism() {
        let mut b = DelegateBatchParams {
            repo: "/r".into(),
            base_ref: "main".into(),
            tasks: vec![packet(), packet()],
            parallelism: 8,
        };
        assert!(b.validate().unwrap_err().contains("duplicado"));
        b.tasks.pop();
        b.parallelism = 0;
        assert!(b.validate().is_err());
    }

    #[test]
    fn tokens_use_in_out_keys() {
        let v = serde_json::to_value(TokenUsage {
            input: 1,
            output: 2,
        })
        .unwrap();
        assert_eq!(v, serde_json::json!({ "in": 1, "out": 2 }));
    }
}
