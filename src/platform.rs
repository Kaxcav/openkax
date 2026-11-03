//! Integração com o SO atrás do trait [`Platform`] (ADR 0001).
//!
//! Único lugar onde `#[cfg(windows)]` é permitido. Linux usa XDG
//! (`$XDG_DATA_HOME/openkax`, `$XDG_CONFIG_HOME/openkax`); Windows usa
//! `%LOCALAPPDATA%\OpenKax` para tudo (config, ledger, estado, logs).

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

    /// Logs em arquivo (`tracing-appender`).
    fn logs_dir(&self) -> PathBuf {
        self.data_dir().join("logs")
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

/// Windows: tudo sob `%LOCALAPPDATA%\OpenKax`. A resolução é pura e compila em
/// qualquer SO, para os testes rodarem também no Linux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsPlatform {
    root: PathBuf,
}

impl WindowsPlatform {
    pub fn from_env() -> Self {
        Self::from_vars(
            std::env::var_os("LOCALAPPDATA"),
            std::env::var_os("USERPROFILE"),
        )
    }

    /// `LOCALAPPDATA` vazio ou relativo é ignorado; cai em `%USERPROFILE%\AppData\Local`.
    pub fn from_vars(local_app_data: Option<OsString>, user_profile: Option<OsString>) -> Self {
        let base = local_app_data
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| {
                PathBuf::from(user_profile.unwrap_or_else(|| ".".into()))
                    .join("AppData")
                    .join("Local")
            });
        Self {
            root: base.join("OpenKax"),
        }
    }
}

impl Platform for WindowsPlatform {
    fn data_dir(&self) -> PathBuf {
        self.root.clone()
    }

    fn config_dir(&self) -> PathBuf {
        self.root.clone()
    }
}

/// Plataforma do processo atual.
#[cfg(unix)]
pub fn current() -> impl Platform {
    UnixPlatform::from_env()
}

/// Plataforma do processo atual.
#[cfg(windows)]
pub fn current() -> impl Platform {
    WindowsPlatform::from_env()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(s.into())
    }

    /// Caminho absoluto em qualquer SO (`/x` não é absoluto no Windows).
    fn abs(rel: &str) -> PathBuf {
        std::env::current_dir().unwrap().join(rel)
    }

    fn some(p: &std::path::Path) -> Option<OsString> {
        Some(p.as_os_str().to_owned())
    }

    /// Mesma interface nos dois SOs: ledger/estado/logs debaixo de `data_dir`.
    fn assert_layout(p: &impl Platform, data: PathBuf, config: PathBuf) {
        assert_eq!(p.data_dir(), data);
        assert_eq!(p.config_dir(), config);
        assert_eq!(p.ledger_dir(), data.join("ledger"));
        assert_eq!(p.state_dir(), data.join("state"));
        assert_eq!(p.logs_dir(), data.join("logs"));
    }

    #[test]
    fn xdg_wins_when_absolute() {
        let (data, cfg) = (abs("x-data"), abs("x-cfg"));
        let p = UnixPlatform::from_vars(some(&data), some(&cfg), os("home-u"));
        assert_layout(&p, data.join("openkax"), cfg.join("openkax"));
    }

    #[test]
    fn falls_back_to_home_when_xdg_missing_or_relative() {
        let home = abs("home-u");
        let p = UnixPlatform::from_vars(None, os("rel/cfg"), some(&home));
        assert_layout(
            &p,
            home.join(".local").join("share").join("openkax"),
            home.join(".config").join("openkax"),
        );
    }

    #[test]
    fn windows_uses_localappdata_for_everything() {
        let base = abs("lad");
        let p = WindowsPlatform::from_vars(some(&base), os("ignored"));
        let root = base.join("OpenKax");
        assert_layout(&p, root.clone(), root);
    }

    #[test]
    fn windows_falls_back_to_userprofile_when_localappdata_missing_or_relative() {
        let profile = abs("profile-u");
        let root = profile.join("AppData").join("Local").join("OpenKax");
        for lad in [None, os("rel")] {
            let p = WindowsPlatform::from_vars(lad, some(&profile));
            assert_layout(&p, root.clone(), root.clone());
        }
    }

    #[cfg(windows)]
    #[test]
    fn current_is_windows_platform() {
        assert_eq!(current().data_dir(), WindowsPlatform::from_env().data_dir());
    }

    #[cfg(unix)]
    #[test]
    fn current_is_unix_platform() {
        assert_eq!(current().data_dir(), UnixPlatform::from_env().data_dir());
    }
}
