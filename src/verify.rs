//! Schema de `.kax/verify.toml` — **Fase 2**.
//!
//! Define, por repo, os comandos de verificação por linguagem/glob, o timeout
//! e o teto do log devolvido ao lead. Nada roda fora do worktree da tarefa;
//! o runner entra na etapa F2-3. Aqui só há struct, parser e validação.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Timeout padrão por comando, em segundos.
pub const DEFAULT_TIMEOUT_S: u64 = 300;
/// Teto padrão do log de verify devolvido (bytes).
pub const DEFAULT_MAX_LOG_BYTES: usize = 4096;

fn default_timeout_s() -> u64 {
    DEFAULT_TIMEOUT_S
}

fn default_max_log_bytes() -> usize {
    DEFAULT_MAX_LOG_BYTES
}

/// Conteúdo de `.kax/verify.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyConfig {
    /// Timeout padrão por comando (s).
    #[serde(default = "default_timeout_s")]
    pub timeout_s: u64,
    /// Máximo de bytes do log devolvido (o fim do log é preservado).
    #[serde(default = "default_max_log_bytes")]
    pub max_log_bytes: usize,
    /// Regras por linguagem/glob.
    #[serde(default, rename = "language")]
    pub languages: Vec<LanguageRule>,
}

/// Comandos de verify de uma linguagem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LanguageRule {
    /// Nome livre (`rust`, `node`, ...).
    pub name: String,
    /// Globs dos arquivos alterados que ativam a regra (`**/*.rs`).
    pub globs: Vec<String>,
    /// Comandos, em ordem; o primeiro que falhar encerra o verify.
    pub commands: Vec<String>,
    /// Sobrescreve `timeout_s` para esta linguagem.
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

/// Erro ao ler ou validar `verify.toml`.
#[derive(Debug, thiserror::Error)]
pub enum VerifyConfigError {
    #[error("não foi possível ler {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("verify.toml inválido: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("verify.toml inválido: {0}")]
    Invalid(String),
}

impl VerifyConfig {
    /// Lê e valida `<repo>/.kax/verify.toml`.
    pub fn load(path: &Path) -> Result<Self, VerifyConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| VerifyConfigError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text)
    }

    /// Parseia e valida o texto TOML.
    pub fn parse(text: &str) -> Result<Self, VerifyConfigError> {
        let cfg: Self = toml::from_str(text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), VerifyConfigError> {
        let bad = |m: String| Err(VerifyConfigError::Invalid(m));
        if self.timeout_s == 0 {
            return bad("timeout_s deve ser > 0".into());
        }
        if self.max_log_bytes == 0 {
            return bad("max_log_bytes deve ser > 0".into());
        }
        for l in &self.languages {
            if l.name.trim().is_empty() {
                return bad("language sem name".into());
            }
            if l.globs.is_empty() || l.globs.iter().any(|g| g.trim().is_empty()) {
                return bad(format!("language {:?}: globs vazio", l.name));
            }
            if l.commands.is_empty() || l.commands.iter().any(|c| c.trim().is_empty()) {
                return bad(format!("language {:?}: commands vazio", l.name));
            }
            if l.timeout_s == Some(0) {
                return bad(format!("language {:?}: timeout_s deve ser > 0", l.name));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../.kax/verify.toml.example");

    #[test]
    fn example_parses() {
        let cfg = VerifyConfig::parse(EXAMPLE).expect("exemplo válido");
        assert_eq!(cfg.max_log_bytes, 4096);
        assert!(cfg.languages.iter().any(|l| l.name == "rust"));
    }

    #[test]
    fn defaults_apply() {
        let cfg = VerifyConfig::parse("").unwrap();
        assert_eq!(cfg.timeout_s, DEFAULT_TIMEOUT_S);
        assert_eq!(cfg.max_log_bytes, DEFAULT_MAX_LOG_BYTES);
        assert!(cfg.languages.is_empty());
    }

    #[test]
    fn rejects_unknown_field_and_empty_commands() {
        assert!(VerifyConfig::parse("tempo = 3").is_err());
        let sem_cmd = "[[language]]\nname = \"x\"\nglobs = [\"*.x\"]\ncommands = []\n";
        assert!(matches!(
            VerifyConfig::parse(sem_cmd),
            Err(VerifyConfigError::Invalid(_))
        ));
        assert!(VerifyConfig::parse("timeout_s = 0").is_err());
    }
}
