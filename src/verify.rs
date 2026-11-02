//! Schema de `.kax/verify.toml` — **Fase 2**.
//!
//! Define, por repo, os comandos de verificação por linguagem/glob, o timeout
//! e o teto do log devolvido ao lead. Nada roda fora do worktree da tarefa;
//! o runner (apply + verify) fica no fim deste módulo.

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::config::{Auth, Config};

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
    /// Variáveis extras (além de [`ENV_ALLOWLIST`]) repassadas aos comandos de
    /// verify. Nunca segredo: `*_KEY`, `*_TOKEN`, `*_SECRET` e os `auth.env`
    /// do `providers.toml` são recusados.
    #[serde(default)]
    pub env_passthrough: Vec<String>,
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
        for name in &self.env_passthrough {
            if name.is_empty() || name.contains(['=', '\0']) {
                return bad(format!("env_passthrough: nome inválido {name:?}"));
            }
            let upper = name.to_ascii_uppercase();
            if SECRET_SUFFIXES.iter().any(|s| upper.ends_with(s)) {
                return bad(format!(
                    "env_passthrough: {name:?} parece segredo (*_KEY, *_TOKEN, *_SECRET)"
                ));
            }
        }
        Ok(())
    }

    /// Recusa `env_passthrough` que repasse variável usada pelos providers
    /// (`auth.env` ou `${VAR}` em `base_url`/headers).
    pub fn check_provider_secrets(&self, providers: &Config) -> Result<(), VerifyConfigError> {
        let mut secret: Vec<String> = Vec::new();
        for p in &providers.providers {
            if let Auth::Bearer { env } = &p.auth {
                secret.push(env.clone());
            }
            secret.extend(p.env_refs());
        }
        match self
            .env_passthrough
            .iter()
            .find(|n| secret.iter().any(|s| s.eq_ignore_ascii_case(n)))
        {
            Some(n) => Err(VerifyConfigError::Invalid(format!(
                "env_passthrough: {n:?} é credencial de provider no providers.toml"
            ))),
            None => Ok(()),
        }
    }
}

/// Sufixos de nome que nunca passam por `env_passthrough`.
const SECRET_SUFFIXES: [&str; 3] = ["_KEY", "_TOKEN", "_SECRET"];

/// Únicas variáveis do ambiente do openkax que os processos do verify e o
/// `git` herdam (as que existirem). As chaves de API que o `.env` põe no
/// ambiente ficam de fora: o verify roda código do operário (ADR 0007).
pub const ENV_ALLOWLIST: [&str; 23] = [
    "PATH",
    "HOME",
    "USERPROFILE",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "TEMP",
    "TMP",
    "TMPDIR",
    "SystemRoot",
    "SystemDrive",
    "windir",
    "ComSpec",
    "PATHEXT",
    "LANG",
    "LC_ALL",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "RUST_BACKTRACE",
    "GIT_EXEC_PATH",
];

/// Zera o ambiente de `cmd` e copia do atual só [`ENV_ALLOWLIST`] + `extra`.
pub fn clean_env<'a>(cmd: &'a mut Command, extra: &[String]) -> &'a mut Command {
    cmd.env_clear();
    let names = ENV_ALLOWLIST
        .iter()
        .copied()
        .chain(extra.iter().map(String::as_str));
    for name in names {
        if let Some(v) = std::env::var_os(name) {
            cmd.env(name, v);
        }
    }
    cmd
}

// ---------------------------------------------------------------------------
// Runner (F2-4): aplica o diff e executa o verify sempre dentro do worktree.
// ---------------------------------------------------------------------------

use std::io::Write;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Falhas que impedem o verify de rodar (não confundir com comando que falha).
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("{0} não é um worktree git")]
    NotAWorktree(String),
    #[error("diff rejeitado pelo git apply: {log}")]
    ApplyRejected { log: String },
    #[error("expect inválido {0:?}: use \"exit <N>\"")]
    BadExpect(String),
    #[error("erro de E/S: {0}")]
    Io(#[from] std::io::Error),
}

/// Resultado de um comando de verify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutcome {
    pub command: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub passed: bool,
    /// stdout+stderr, no máximo `max_log_bytes` (o fim é preservado).
    pub log: String,
    pub truncated: bool,
}

/// Resultado do verify inteiro; para no primeiro comando que falha.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub passed: bool,
    pub outcomes: Vec<CommandOutcome>,
}

/// Limites e ambiente de execução.
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub timeout: Duration,
    pub max_log_bytes: usize,
    /// Repassadas aos comandos além de [`ENV_ALLOWLIST`] (já validadas).
    pub env_passthrough: Vec<String>,
}

impl RunOptions {
    pub fn from_config(cfg: &VerifyConfig) -> Self {
        Self {
            timeout: Duration::from_secs(cfg.timeout_s),
            max_log_bytes: cfg.max_log_bytes,
            env_passthrough: cfg.env_passthrough.clone(),
        }
    }
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_S),
            max_log_bytes: DEFAULT_MAX_LOG_BYTES,
            env_passthrough: Vec::new(),
        }
    }
}

/// Interpreta `expect` (`exit N`; vazio = `exit 0`).
pub fn parse_expect(expect: &str) -> Result<i32, RunError> {
    let e = expect.trim();
    if e.is_empty() {
        return Ok(0);
    }
    e.strip_prefix("exit")
        .map(str::trim)
        .and_then(|n| n.parse::<i32>().ok())
        .ok_or_else(|| RunError::BadExpect(expect.to_string()))
}

/// Mantém só o fim de `bytes` (até `max`), respeitando limites UTF-8.
pub fn truncate_tail(bytes: &[u8], max: usize) -> (String, bool) {
    if bytes.len() <= max {
        return (String::from_utf8_lossy(bytes).into_owned(), false);
    }
    let mut tail = &bytes[bytes.len() - max..];
    // Descarta bytes de continuação do caractere cortado ao meio.
    while let Some((&b, rest)) = tail.split_first() {
        if b & 0xC0 == 0x80 {
            tail = rest;
        } else {
            break;
        }
    }
    (String::from_utf8_lossy(tail).into_owned(), true)
}

fn ensure_worktree(dir: &Path) -> Result<(), RunError> {
    if dir.is_dir() && dir.join(".git").exists() {
        Ok(())
    } else {
        Err(RunError::NotAWorktree(dir.display().to_string()))
    }
}

/// Aplica o diff do operário no índice e na árvore do worktree.
/// O diff vai por stdin; falha de apply vira `ApplyRejected` com o log do git.
pub fn apply_diff(worktree: &Path, diff: &str, max_log_bytes: usize) -> Result<(), RunError> {
    ensure_worktree(worktree)?;
    let mut child = clean_env(&mut Command::new("git"), &[])
        .args(["apply", "--index", "--whitespace=nowarn", "-"])
        .current_dir(worktree)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        // Pipe quebrado aparece no status/stderr do git.
        let _ = stdin.write_all(diff.as_bytes());
    }
    let out = child.wait_with_output()?;
    if out.status.success() {
        return Ok(());
    }
    let mut all = out.stdout;
    all.extend_from_slice(&out.stderr);
    let (log, _) = truncate_tail(&all, max_log_bytes);
    Err(RunError::ApplyRejected { log })
}

/// Executa os comandos em ordem no worktree; para no primeiro que falhar.
/// Os comandos vêm só do pacote/verify.toml, nunca de texto do operário, e
/// rodam com o ambiente limpo ([`clean_env`]).
pub fn run_verify(
    worktree: &Path,
    commands: &[String],
    expect: &str,
    opts: &RunOptions,
) -> Result<VerifyReport, RunError> {
    ensure_worktree(worktree)?;
    let want = parse_expect(expect)?;
    let mut outcomes = Vec::new();
    for cmd in commands {
        let o = run_command(worktree, cmd, want, opts)?;
        let ok = o.passed;
        outcomes.push(o);
        if !ok {
            return Ok(VerifyReport {
                passed: false,
                outcomes,
            });
        }
    }
    Ok(VerifyReport {
        passed: true,
        outcomes,
    })
}

static LOG_SEQ: AtomicU64 = AtomicU64::new(0);

fn shell_command(cmd: &str) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("cmd");
        c.arg("/C").raw_arg(cmd);
        c
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd).process_group(0);
        c
    }
}

/// Mata o processo e todos os descendentes.
fn kill_tree(child: &mut std::process::Child) {
    let pid = child.id().to_string();
    #[cfg(windows)]
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    #[cfg(not(windows))]
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
}

fn run_command(
    worktree: &Path,
    cmd: &str,
    want: i32,
    opts: &RunOptions,
) -> Result<CommandOutcome, RunError> {
    // stdout e stderr no mesmo arquivo temporário preservam a ordem de saída.
    let log_path = std::env::temp_dir().join(format!(
        "openkax-verify-{}-{}.log",
        std::process::id(),
        LOG_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let log_file = std::fs::File::create(&log_path)?;
    let spawned = clean_env(&mut shell_command(cmd), &opts.env_passthrough)
        .current_dir(worktree)
        .stdin(Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&log_path);
            return Err(e.into());
        }
    };
    let start = Instant::now();
    let (status, timed_out) = loop {
        if let Some(st) = child.try_wait()? {
            break (Some(st), false);
        }
        if start.elapsed() >= opts.timeout {
            kill_tree(&mut child);
            let _ = child.wait();
            break (None, true);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let bytes = std::fs::read(&log_path).unwrap_or_default();
    let _ = std::fs::remove_file(&log_path);
    let (log, truncated) = truncate_tail(&bytes, opts.max_log_bytes);
    let exit_code = status.and_then(|s| s.code());
    Ok(CommandOutcome {
        command: cmd.to_string(),
        exit_code,
        timed_out,
        passed: !timed_out && exit_code == Some(want),
        log,
        truncated,
    })
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

    #[test]
    fn env_passthrough_refuses_secret_like_names() {
        let ok =
            VerifyConfig::parse("env_passthrough = [\"DATABASE_URL\", \"RUSTFLAGS\"]").unwrap();
        assert_eq!(ok.env_passthrough, ["DATABASE_URL", "RUSTFLAGS"]);
        for name in ["GROQ_API_KEY", "github_token", "AWS_SECRET", "", "A=B"] {
            let text = format!("env_passthrough = [{name:?}]");
            assert!(
                matches!(
                    VerifyConfig::parse(&text),
                    Err(VerifyConfigError::Invalid(_))
                ),
                "{name:?} deveria ser recusado"
            );
        }
    }

    #[test]
    fn env_passthrough_refuses_provider_credentials() {
        let providers = Config::from_toml_str(
            r#"
            [[provider]]
            id = "p"
            base_url = "https://api/${ACCOUNT_ID}/v1"
            auth = { kind = "bearer", env = "PROVIDER_CRED" }
            models = [{ id = "m", upstream = "m" }]
            "#,
        )
        .unwrap();
        for name in ["PROVIDER_CRED", "provider_cred", "ACCOUNT_ID"] {
            let cfg = VerifyConfig::parse(&format!("env_passthrough = [{name:?}]")).unwrap();
            let e = cfg.check_provider_secrets(&providers).unwrap_err();
            assert!(
                e.to_string().contains("credencial de provider"),
                "{name}: {e}"
            );
        }
        let cfg = VerifyConfig::parse("env_passthrough = [\"DATABASE_URL\"]").unwrap();
        cfg.check_provider_secrets(&providers).unwrap();
    }
}

#[cfg(test)]
mod runner_tests {
    use super::*;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let st = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    }

    fn repo() -> TempDir {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        git(p, &["init", "-q"]);
        git(p, &["config", "user.email", "t@t"]);
        git(p, &["config", "user.name", "t"]);
        git(p, &["config", "core.autocrlf", "false"]);
        std::fs::write(p.join("a.txt"), "old\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-qm", "init"]);
        d
    }

    fn diff_to(content: &str) -> String {
        format!(
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n+{content}\n"
        )
    }

    fn cmds(c: &[&str]) -> Vec<String> {
        c.iter().map(|s| s.to_string()).collect()
    }

    #[cfg(windows)]
    const SLEEP: &str = "ping -n 30 127.0.0.1 > nul";
    #[cfg(not(windows))]
    const SLEEP: &str = "sleep 30";

    #[test]
    fn diff_that_passes_verify() {
        let d = repo();
        apply_diff(d.path(), &diff_to("hello"), 4096).unwrap();
        let r = run_verify(
            d.path(),
            &cmds(&["git grep -q hello -- a.txt"]),
            "exit 0",
            &RunOptions::default(),
        )
        .unwrap();
        assert!(r.passed, "{r:?}");
    }

    #[test]
    fn diff_that_breaks_verify() {
        let d = repo();
        apply_diff(d.path(), &diff_to("bad"), 4096).unwrap();
        let r = run_verify(
            d.path(),
            &cmds(&["git grep -q hello -- a.txt", "git --version"]),
            "exit 0",
            &RunOptions::default(),
        )
        .unwrap();
        assert!(!r.passed);
        assert_eq!(r.outcomes.len(), 1, "para no primeiro que falha");
        assert_eq!(r.outcomes[0].exit_code, Some(1));
    }

    #[test]
    fn expect_other_exit_code() {
        let d = repo();
        let r = run_verify(
            d.path(),
            &cmds(&["exit 3"]),
            "exit 3",
            &RunOptions::default(),
        )
        .unwrap();
        assert!(r.passed);
        assert!(matches!(
            run_verify(
                d.path(),
                &cmds(&["exit 0"]),
                "talvez",
                &RunOptions::default()
            ),
            Err(RunError::BadExpect(_))
        ));
    }

    #[test]
    fn timeout_kills_command() {
        let d = repo();
        let opts = RunOptions {
            timeout: Duration::from_millis(500),
            max_log_bytes: 4096,
            ..RunOptions::default()
        };
        let t = Instant::now();
        let r = run_verify(d.path(), &cmds(&[SLEEP]), "exit 0", &opts).unwrap();
        assert!(t.elapsed() < Duration::from_secs(20), "não esperou o sleep");
        assert!(!r.passed);
        assert!(r.outcomes[0].timed_out);
    }

    #[test]
    fn log_truncation_keeps_the_end() {
        let d = repo();
        #[cfg(windows)]
        let cmd = "for /L %i in (1,1,2000) do @echo line%i";
        #[cfg(not(windows))]
        let cmd = "for i in $(seq 1 2000); do echo line$i; done";
        let opts = RunOptions {
            timeout: Duration::from_secs(60),
            max_log_bytes: 100,
            ..RunOptions::default()
        };
        let r = run_verify(d.path(), &cmds(&[cmd]), "exit 0", &opts).unwrap();
        let o = &r.outcomes[0];
        assert!(o.truncated);
        assert!(o.log.len() <= 100);
        assert!(o.log.trim_end().ends_with("line2000"), "{:?}", o.log);
    }

    #[test]
    fn truncate_tail_respects_utf8() {
        let (s, t) = truncate_tail("aéééé".as_bytes(), 4);
        assert!(t);
        assert_eq!(s, "éé");
        assert_eq!(truncate_tail(b"abc", 10), ("abc".to_string(), false));
    }

    #[test]
    fn apply_that_does_not_apply_is_rejected() {
        let d = repo();
        let bad =
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-nao existe\n+x\n";
        match apply_diff(d.path(), bad, 4096) {
            Err(RunError::ApplyRejected { log }) => assert!(!log.is_empty()),
            other => panic!("esperava ApplyRejected, veio {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "old\n"
        );
    }

    #[test]
    fn refuses_non_worktree_dir() {
        let d = tempfile::tempdir().unwrap();
        assert!(matches!(
            run_verify(
                d.path(),
                &cmds(&["git --version"]),
                "exit 0",
                &RunOptions::default()
            ),
            Err(RunError::NotAWorktree(_))
        ));
    }

    #[cfg(windows)]
    fn echo_var(name: &str) -> String {
        format!("echo %{name}%")
    }
    #[cfg(not(windows))]
    fn echo_var(name: &str) -> String {
        format!("echo \"${name}\"")
    }

    /// C1: a chave que o `.env` põe no ambiente do openkax não chega ao verify.
    #[test]
    fn verify_does_not_inherit_api_keys() {
        // SAFETY: variável exclusiva deste teste; std serializa o acesso ao ambiente.
        unsafe { std::env::set_var("OPENKAX_FAKE_API_KEY", "vazou-123") };
        let d = repo();
        let cmd = echo_var("OPENKAX_FAKE_API_KEY");
        let r = run_verify(d.path(), &[cmd], "exit 0", &RunOptions::default()).unwrap();
        assert!(r.passed, "{r:?}");
        assert!(!r.outcomes[0].log.contains("vazou-123"), "{r:?}");
    }

    /// Controle do teste acima: a expansão funciona e `env_passthrough` repassa
    /// só o que foi pedido.
    #[test]
    fn env_passthrough_reaches_verify() {
        // SAFETY: variável exclusiva deste teste; std serializa o acesso ao ambiente.
        unsafe { std::env::set_var("OPENKAX_TEST_PASSTHRU", "ok-456") };
        let d = repo();
        let cmd = vec![echo_var("OPENKAX_TEST_PASSTHRU")];
        let sem = run_verify(d.path(), &cmd, "exit 0", &RunOptions::default()).unwrap();
        assert!(!sem.outcomes[0].log.contains("ok-456"), "{sem:?}");
        let opts = RunOptions {
            env_passthrough: vec!["OPENKAX_TEST_PASSTHRU".into()],
            ..RunOptions::default()
        };
        let com = run_verify(d.path(), &cmd, "exit 0", &opts).unwrap();
        assert!(com.outcomes[0].log.contains("ok-456"), "{com:?}");
    }

    #[test]
    fn path_survives_clean_env() {
        let d = repo();
        let r = run_verify(
            d.path(),
            &cmds(&["git --version"]),
            "exit 0",
            &RunOptions::default(),
        )
        .unwrap();
        assert!(r.passed, "{r:?}");
    }
}
