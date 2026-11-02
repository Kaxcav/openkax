//! Integração com o SO atrás do trait [`Platform`] (ADR 0001).
//!
//! Único lugar onde `#[cfg(windows)]` é permitido. Linux usa XDG
//! (`$XDG_DATA_HOME/openkax`, `$XDG_CONFIG_HOME/openkax`); Windows usará
//! `%LOCALAPPDATA%\OpenKax` (implementação junto com o workflow de CI).

use std::ffi::OsString;
use std::path::PathBuf;

pub trait Platform: Send + Sync {
    /// Dados persistentes (ledger, snapshots).
    fn data_dir(&self) -> PathBuf;
    /// Config do usuário (`providers.toml` quando fora do repo).
    fn config_dir(&self) -> PathBuf;

    fn ledger_dir(&self) -> PathBuf {
        self.data_dir().join("ledger")
    }

    /// Snapshots JSON dos buckets.
    fn state_dir(&self) -> PathBuf {
        self.data_dir().join("state")
    }
}

/// Linux/Unix: diretórios XDG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnixPlatform {
    data_dir: PathBuf,
    config_dir: PathBuf,
}

impl UnixPlatform {
    pub fn from_env() -> Self {
        Self::from_vars(
            std::env::var_os("XDG_DATA_HOME"),
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("HOME"),
        )
    }

    /// Resolução pura (testável). XDG vazio ou relativo é ignorado, como manda a spec.
    pub fn from_vars(
        xdg_data: Option<OsString>,
        xdg_config: Option<OsString>,
        home: Option<OsString>,
    ) -> Self {
        let home = home
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let pick = |xdg: Option<OsString>, fallback: &[&str]| {
            xdg.map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| fallback.iter().fold(home.clone(), |p, s| p.join(s)))
                .join("openkax")
        };
        Self {
            data_dir: pick(xdg_data, &[".local", "share"]),
            config_dir: pick(xdg_config, &[".config"]),
        }
    }
}

impl Platform for UnixPlatform {
    fn data_dir(&self) -> PathBuf {
        self.data_dir.clone()
    }

    fn config_dir(&self) -> PathBuf {
        self.config_dir.clone()
    }
}

/// Plataforma do processo atual.
#[cfg(unix)]
pub fn current() -> impl Platform {
    UnixPlatform::from_env()
}

#[cfg(windows)]
compile_error!(
    "WindowsPlatform ainda não existe: entra com o workflow de CI no fechamento da Fase 0"
);

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(s.into())
    }

    #[test]
    fn xdg_wins_when_absolute() {
        let p = UnixPlatform::from_vars(os("/x/data"), os("/x/cfg"), os("/home/u"));
        assert_eq!(p.data_dir(), PathBuf::from("/x/data/openkax"));
        assert_eq!(p.config_dir(), PathBuf::from("/x/cfg/openkax"));
        assert_eq!(p.ledger_dir(), PathBuf::from("/x/data/openkax/ledger"));
    }

    #[test]
    fn falls_back_to_home_when_xdg_missing_or_relative() {
        let p = UnixPlatform::from_vars(None, os("rel/cfg"), os("/home/u"));
        assert_eq!(p.data_dir(), PathBuf::from("/home/u/.local/share/openkax"));
        assert_eq!(p.config_dir(), PathBuf::from("/home/u/.config/openkax"));
        assert_eq!(
            p.state_dir(),
            PathBuf::from("/home/u/.local/share/openkax/state")
        );
    }
}
