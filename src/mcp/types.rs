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
    /// Só com `mode = "read"`: em vez do arquivo inteiro, manda estes
    /// símbolos (`nome`, `Tipo::nome` ou `<arquivo>::<qual_name>`) com os
    /// tipos que usam, as assinaturas do que chamam e os chamadores (ADR 0009).
    /// Ainda não atendido: a validação recusa até a P6d.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbols: Option<Vec<String>>,
    /// Só com `mode = "read"`: `[início, fim]`, 1-based e inclusivo; não
    /// combina com `symbols`. Ainda não atendido: a validação recusa até a P6d.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<[u32; 2]>,
}

impl FileSpec {
    /// Pede recorte (`symbols` ou `lines`) em vez do arquivo inteiro.
    pub fn selects(&self) -> bool {
        self.symbols.is_some() || self.lines.is_some()
    }

    /// Regras de `symbols`/`lines` que valem desde já (ADR 0009, decisão 7).
    fn validate_selectors(&self) -> Result<(), String> {
        let path = &self.path;
        if !self.selects() {
            return Ok(());
        }
        if self.mode != FileMode::Read {
            return Err(format!("{path}: symbols/lines só valem com mode=read"));
        }
        if self.symbols.is_some() && self.lines.is_some() {
            return Err(format!("{path}: use symbols ou lines, não os dois"));
        }
        if let Some(s) = &self.symbols
            && (s.is_empty() || s.iter().any(|q| q.trim().is_empty()))
        {
            return Err(format!("{path}: symbols vazio ou com item vazio"));
        }
        if let Some([start, end]) = self.lines
            && (start == 0 || start > end)
        {
            return Err(format!(
                "{path}: lines deve ser [início, fim] com 1 <= início <= fim"
            ));
        }
        Ok(())
    }
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
            f.validate_selectors().map_err(|e| format!("{id}: {e}"))?;
        }
        if !(1..=MAX_ATTEMPTS).contains(&self.max_attempts) {
            return Err(format!(
                "{id}: max_attempts deve estar em 1..={MAX_ATTEMPTS}"
            ));
        }
        // Recusa explícita em vez de mandar o arquivo inteiro sem avisar: o
        // pacote só passa a atender o recorte na P6d (ADR 0009).
        if let Some(f) = self.files.iter().find(|f| f.selects()) {
            return Err(format!(
                "{id}: {}: files[].symbols/lines ainda não são atendidos pelo pacote (P6d, ADR 0009)",
                f.path
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

    fn with_file(extra: serde_json::Value) -> TaskPacket {
        let mut v = serde_json::to_value(packet()).unwrap();
        let file = v["files"][0].as_object_mut().unwrap();
        for (k, val) in extra.as_object().unwrap() {
            file.insert(k.clone(), val.clone());
        }
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn selectors_are_optional_and_absent_on_the_wire() {
        let t = packet();
        assert!(t.files.iter().all(|f| !f.selects()));
        let v = serde_json::to_value(&t).unwrap();
        for f in v["files"].as_array().unwrap() {
            let keys: Vec<&str> = f.as_object().unwrap().keys().map(String::as_str).collect();
            assert_eq!(keys, ["exists", "mode", "path"], "sem symbols/lines");
        }
        let back: TaskPacket = serde_json::from_value(v).unwrap();
        assert_eq!(back, t);

        let schema = schemars::schema_for!(FileSpec);
        let s = schema.as_value();
        assert!(s["properties"]["symbols"].is_object());
        assert!(s["properties"]["lines"].is_object());
        let mut req: Vec<&str> = s["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r.as_str())
            .collect();
        req.sort_unstable();
        assert_eq!(req, ["mode", "path"]);
    }

    #[test]
    fn selectors_only_with_read() {
        let mut t = with_file(serde_json::json!({ "symbols": ["X"] }));
        t.files[0].mode = FileMode::Write;
        assert!(t.validate().unwrap_err().contains("mode=read"));
        let mut t = with_file(serde_json::json!({ "lines": [1, 3] }));
        t.files[0].mode = FileMode::Write;
        assert!(t.validate().unwrap_err().contains("mode=read"));
    }

    #[test]
    fn selectors_shape_is_checked() {
        for (extra, needle) in [
            (serde_json::json!({ "symbols": [] }), "symbols vazio"),
            (
                serde_json::json!({ "symbols": ["Rect::new", " "] }),
                "symbols vazio",
            ),
            (serde_json::json!({ "lines": [0, 3] }), "1 <= início <= fim"),
            (serde_json::json!({ "lines": [5, 2] }), "1 <= início <= fim"),
            (
                serde_json::json!({ "symbols": ["X"], "lines": [1, 2] }),
                "não os dois",
            ),
        ] {
            let e = with_file(extra.clone()).validate().unwrap_err();
            assert!(e.contains(needle), "{extra}: {e}");
        }
    }

    #[test]
    fn well_formed_selectors_wait_for_p6d() {
        for extra in [
            serde_json::json!({ "symbols": ["PaymentAdapter"] }),
            serde_json::json!({ "lines": [1, 1] }),
            serde_json::json!({ "lines": [3, 40] }),
        ] {
            let e = with_file(extra.clone()).validate().unwrap_err();
            assert!(e.contains("P6d"), "{extra}: {e}");
            assert!(e.contains("src/payments/mod.rs"), "{e}");
        }
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
