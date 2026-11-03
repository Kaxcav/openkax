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
    /// Isolamento dos comandos de verify (ADR 0008). Padrão `bwrap`: sem
    /// bwrap o verify recusa (fail closed). `none` só vale escrito aqui.
    #[serde(default)]
    pub sandbox: Sandbox,
}

/// Como os comandos de verify são isolados.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sandbox {
    /// `bubblewrap` (Linux): sem rede, home vazio, só o worktree gravável.
    #[default]
    Bwrap,
    /// Sem isolamento; precisa estar escrito no `verify.toml`.
    None,
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
    /// Comandos com rede, fora da caixa, no worktree recém-criado e **antes**
    /// de aplicar o diff do operário (ex.: `cargo fetch --locked`).
    #[serde(default)]
    pub prefetch: Vec<String>,
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
            if l.prefetch.iter().any(|c| c.trim().is_empty()) {
                return bad(format!("language {:?}: prefetch com comando vazio", l.name));
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
    #[error("alteração fora do escopo: {0}")]
    OutOfScope(String),
    #[error("git {args}: {stderr}")]
    Git { args: String, stderr: String },
    #[error("{0}")]
    SandboxUnavailable(String),
    #[error("prefetch falhou: {command}\n{log}")]
    PrefetchFailed { command: String, log: String },
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
    /// Avisos da execução (ex.: rodou com `sandbox = "none"`); vão para o log da tarefa.
    pub warnings: Vec<String>,
}

/// Limites e ambiente de execução.
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub timeout: Duration,
    pub max_log_bytes: usize,
    /// Repassadas aos comandos além de [`ENV_ALLOWLIST`] (já validadas).
    pub env_passthrough: Vec<String>,
    /// Isolamento dos comandos (padrão: [`Sandbox::Bwrap`], fail closed).
    pub sandbox: Sandbox,
    /// Comandos de prefetch (rede, sem sandbox, antes do diff); ver [`run_prefetch`].
    pub prefetch: Vec<String>,
    /// `PATH` usado só para achar o `bwrap`; `None` = `PATH` do processo.
    /// Existe para os testes do fail closed.
    pub search_path: Option<std::ffi::OsString>,
}

impl RunOptions {
    pub fn from_config(cfg: &VerifyConfig) -> Self {
        let mut prefetch: Vec<String> = Vec::new();
        for c in cfg.languages.iter().flat_map(|l| &l.prefetch) {
            if !prefetch.contains(c) {
                prefetch.push(c.clone());
            }
        }
        Self {
            timeout: Duration::from_secs(cfg.timeout_s),
            max_log_bytes: cfg.max_log_bytes,
            env_passthrough: cfg.env_passthrough.clone(),
            sandbox: cfg.sandbox,
            prefetch,
            search_path: None,
        }
    }
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_S),
            max_log_bytes: DEFAULT_MAX_LOG_BYTES,
            env_passthrough: Vec::new(),
            sandbox: Sandbox::default(),
            prefetch: Vec::new(),
            search_path: None,
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

/// Modos aceitos no índice depois do apply: arquivo comum ou deleção.
const STAGED_MODES: [&str; 3] = ["100644", "100755", "000000"];

/// Guarda pós-apply (C2): o que o git de fato pôs no índice, comparado com o
/// commit `base` via `git diff --cached --no-renames --raw -z`. Todo path tem
/// de estar em `allowed` e todo modo novo tem de ser 100644, 100755 ou 000000
/// (deleção). Não depende do parser: vale mesmo que ele deixe algo passar.
pub fn check_staged(worktree: &Path, base: &str, allowed: &[String]) -> Result<(), RunError> {
    ensure_worktree(worktree)?;
    let args = [
        "diff",
        "--cached",
        "--no-renames",
        "--raw",
        "-z",
        base,
        "--",
    ];
    let out = clean_env(&mut Command::new("git"), &[])
        .args(args)
        .current_dir(worktree)
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(RunError::Git {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        });
    }
    // Cada entrada: `:<modo antigo> <modo novo> <hash> <hash> <status>\0<path>\0`.
    let mut fields = out.stdout.split(|&b| b == 0).filter(|f| !f.is_empty());
    while let Some(meta) = fields.next() {
        let meta = String::from_utf8_lossy(meta);
        let path = fields
            .next()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .ok_or_else(|| RunError::OutOfScope(format!("saída inesperada do git: {meta}")))?;
        let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        let (new_mode, status) = (
            parts.get(1).copied().unwrap_or(""),
            parts.get(4).copied().unwrap_or(""),
        );
        if status.starts_with(['R', 'C']) {
            return Err(RunError::OutOfScope(format!("{path}: rename/copy")));
        }
        if !allowed.contains(&path) {
            return Err(RunError::OutOfScope(format!("{path}: fora da lista write")));
        }
        if !STAGED_MODES.contains(&new_mode) {
            return Err(RunError::OutOfScope(format!(
                "{path}: modo {new_mode} não permitido (só 100644, 100755 ou deleção)"
            )));
        }
    }
    Ok(())
}

/// Como um comando é executado, já resolvido (e validado) para esta rodada.
enum Mode {
    Plain,
    #[cfg(target_os = "linux")]
    Bwrap(std::path::PathBuf),
}

#[cfg(not(target_os = "linux"))]
const UNAVAILABLE: &str = "sandbox indisponível nesta plataforma; rode o kax-workers no WSL ou declare sandbox = \"none\" no .kax/verify.toml";
#[cfg(target_os = "linux")]
const UNAVAILABLE_LINUX: &str = "sandbox indisponível: bwrap não está no PATH (instale bubblewrap) ou declare sandbox = \"none\" no .kax/verify.toml";
const NONE_WARNING: &str = "AVISO: verify executado com sandbox = \"none\": o código do operário tem rede e acesso ao disco do usuário (ADR 0008)";

/// Resolve o isolamento pedido; recusa (nunca cai para `none` sozinho).
fn resolve_mode(opts: &RunOptions) -> Result<(Mode, Vec<String>), RunError> {
    match opts.sandbox {
        Sandbox::None => Ok((Mode::Plain, vec![NONE_WARNING.to_owned()])),
        Sandbox::Bwrap => {
            #[cfg(target_os = "linux")]
            {
                let bwrap = find_in_path("bwrap", opts.search_path.clone())
                    .ok_or_else(|| RunError::SandboxUnavailable(UNAVAILABLE_LINUX.to_owned()))?;
                Ok((Mode::Bwrap(bwrap), Vec::new()))
            }
            #[cfg(not(target_os = "linux"))]
            Err(RunError::SandboxUnavailable(UNAVAILABLE.to_owned()))
        }
    }
}

/// Confere, sem executar nada, se o isolamento pedido é atendível. O runner
/// chama antes de gastar uma chamada ao operário.
pub fn check_sandbox(opts: &RunOptions) -> Result<(), RunError> {
    resolve_mode(opts).map(|_| ())
}

#[cfg(target_os = "linux")]
pub(crate) fn find_in_path(
    name: &str,
    path: Option<std::ffi::OsString>,
) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = path.or_else(|| std::env::var_os("PATH"))?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| {
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// Argumentos do `bwrap` (sem o `--` e o comando), montados como
/// `Vec<OsString>`: nada passa por shell. Nunca monta `CARGO_HOME` inteiro
/// (tem `credentials.toml`), o repo pai nem `/mnt`.
#[cfg(target_os = "linux")]
pub fn bwrap_args(worktree: &Path) -> std::io::Result<Vec<std::ffi::OsString>> {
    use std::ffi::{OsStr, OsString};
    use std::path::PathBuf;
    fn push(a: &mut Vec<OsString>, xs: &[&OsStr]) {
        a.extend(xs.iter().map(|x| (*x).to_owned()));
    }
    let wt = std::fs::canonicalize(worktree)?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| h.is_absolute() && h != Path::new("/"))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let env_dir = |name: &str, default: &str| -> PathBuf {
        let p = std::env::var_os(name)
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(default));
        std::fs::canonicalize(&p).unwrap_or(p)
    };
    let cargo = env_dir("CARGO_HOME", ".cargo");
    let rustup = env_dir("RUSTUP_HOME", ".rustup");
    let o = |t: &'static str| OsStr::new(t);
    let mut a: Vec<OsString> = Vec::new();
    for flag in ["--unshare-all", "--die-with-parent", "--new-session"] {
        push(&mut a, &[o(flag)]);
    }
    for d in ["/usr", "/etc"] {
        push(&mut a, &[o("--ro-bind"), OsStr::new(d), OsStr::new(d)]);
    }
    // /bin, /lib, /lib64: link (merged-usr) ou diretório real.
    for d in ["/bin", "/lib", "/lib64"] {
        match std::fs::read_link(d) {
            Ok(t) => push(&mut a, &[o("--symlink"), t.as_os_str(), OsStr::new(d)]),
            Err(_) if Path::new(d).is_dir() => {
                push(&mut a, &[o("--ro-bind"), OsStr::new(d), OsStr::new(d)]);
            }
            Err(_) => {}
        }
    }
    push(&mut a, &[o("--proc"), o("/proc"), o("--dev"), o("/dev")]);
    push(
        &mut a,
        &[o("--tmpfs"), o("/tmp"), o("--tmpfs"), home.as_os_str()],
    );
    for p in [
        rustup.clone(),
        cargo.join("bin"),
        cargo.join("registry"),
        cargo.join("git"),
    ] {
        push(&mut a, &[o("--ro-bind-try"), p.as_os_str(), p.as_os_str()]);
    }
    let w = wt.as_os_str();
    push(&mut a, &[o("--bind"), w, w, o("--chdir"), w]);
    for (k, v) in [
        ("CARGO_NET_OFFLINE", OsStr::new("true")),
        ("HOME", home.as_os_str()),
        ("CARGO_HOME", cargo.as_os_str()),
        ("RUSTUP_HOME", rustup.as_os_str()),
    ] {
        push(&mut a, &[o("--setenv"), OsStr::new(k), v]);
    }
    Ok(a)
}

/// Confere que o `bwrap` do `PATH` cria uma caixa sem rede (usado pelo doctor).
#[cfg(target_os = "linux")]
pub fn sandbox_probe() -> Result<(), String> {
    let bwrap = find_in_path("bwrap", None).ok_or("bwrap não está no PATH")?;
    let out = Command::new(bwrap)
        .args([
            "--unshare-all",
            "--unshare-net",
            "--die-with-parent",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "true",
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

/// Executa os comandos em ordem no worktree; para no primeiro que falhar.
/// Os comandos vêm só do pacote/verify.toml, nunca de texto do operário, e
/// rodam com o ambiente limpo ([`clean_env`]) dentro da caixa pedida em
/// `opts.sandbox` (ADR 0008). Sem como honrá-la → `SandboxUnavailable` e
/// nada é executado.
pub fn run_verify(
    worktree: &Path,
    commands: &[String],
    expect: &str,
    opts: &RunOptions,
) -> Result<VerifyReport, RunError> {
    ensure_worktree(worktree)?;
    let want = parse_expect(expect)?;
    let (mode, warnings) = resolve_mode(opts)?;
    let mut outcomes = Vec::new();
    for cmd in commands {
        let o = run_command(worktree, cmd, want, opts, &mode)?;
        let ok = o.passed;
        outcomes.push(o);
        if !ok {
            return Ok(VerifyReport {
                passed: false,
                outcomes,
                warnings,
            });
        }
    }
    Ok(VerifyReport {
        passed: true,
        outcomes,
        warnings,
    })
}

/// Prefetch (`opts.prefetch`): roda com rede, **fora** da caixa, no worktree
/// recém-criado e antes de aplicar o diff do operário (portanto sobre o
/// `base_ref`, confiável). Mesmo timeout e ambiente limpo; o primeiro que
/// falhar encerra com [`RunError::PrefetchFailed`].
pub fn run_prefetch(worktree: &Path, opts: &RunOptions) -> Result<(), RunError> {
    ensure_worktree(worktree)?;
    for cmd in &opts.prefetch {
        let o = run_command(worktree, cmd, 0, opts, &Mode::Plain)?;
        if !o.passed {
            let why = if o.timed_out {
                "timeout".to_owned()
            } else {
                format!("exit {:?}", o.exit_code)
            };
            return Err(RunError::PrefetchFailed {
                command: cmd.clone(),
                log: format!("{why}\n{}", o.log),
            });
        }
    }
    Ok(())
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
    mode: &Mode,
) -> Result<CommandOutcome, RunError> {
    // stdout e stderr no mesmo arquivo temporário preservam a ordem de saída.
    let log_path = std::env::temp_dir().join(format!(
        "openkax-verify-{}-{}.log",
        std::process::id(),
        LOG_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let log_file = std::fs::File::create(&log_path)?;
    let mut command = match mode {
        Mode::Plain => shell_command(cmd),
        #[cfg(target_os = "linux")]
        Mode::Bwrap(bwrap) => {
            use std::os::unix::process::CommandExt;
            let args = match bwrap_args(worktree) {
                Ok(a) => a,
                Err(e) => {
                    let _ = std::fs::remove_file(&log_path);
                    return Err(e.into());
                }
            };
            let mut c = Command::new(bwrap);
            c.args(args).args(["--", "sh", "-c"]).arg(cmd);
            c.process_group(0);
            c
        }
    };
    let spawned = clean_env(&mut command, &opts.env_passthrough)
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

    /// Os testes do runner exercitam apply/timeout/log, não a caixa (ver `sandbox_tests`).
    fn test_opts() -> RunOptions {
        RunOptions {
            sandbox: Sandbox::None,
            ..RunOptions::default()
        }
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
            &test_opts(),
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
            &test_opts(),
        )
        .unwrap();
        assert!(!r.passed);
        assert_eq!(r.outcomes.len(), 1, "para no primeiro que falha");
        assert_eq!(r.outcomes[0].exit_code, Some(1));
    }

    #[test]
    fn expect_other_exit_code() {
        let d = repo();
        let r = run_verify(d.path(), &cmds(&["exit 3"]), "exit 3", &test_opts()).unwrap();
        assert!(r.passed);
        assert!(matches!(
            run_verify(d.path(), &cmds(&["exit 0"]), "talvez", &test_opts()),
            Err(RunError::BadExpect(_))
        ));
    }

    #[test]
    fn timeout_kills_command() {
        let d = repo();
        let opts = RunOptions {
            timeout: Duration::from_millis(500),
            max_log_bytes: 4096,
            ..test_opts()
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
            ..test_opts()
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
            run_verify(d.path(), &cmds(&["git --version"]), "exit 0", &test_opts()),
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
        let r = run_verify(d.path(), &[cmd], "exit 0", &test_opts()).unwrap();
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
        let sem = run_verify(d.path(), &cmd, "exit 0", &test_opts()).unwrap();
        assert!(!sem.outcomes[0].log.contains("ok-456"), "{sem:?}");
        let opts = RunOptions {
            env_passthrough: vec!["OPENKAX_TEST_PASSTHRU".into()],
            ..test_opts()
        };
        let com = run_verify(d.path(), &cmd, "exit 0", &opts).unwrap();
        assert!(com.outcomes[0].log.contains("ok-456"), "{com:?}");
    }

    /// Repo com `src/a.rs` e `src/keep.rs`; a lista `write` é `src/a.rs` + `src/new.rs`.
    fn src_repo() -> (TempDir, String, Vec<String>) {
        let d = repo();
        let p = d.path();
        std::fs::create_dir_all(p.join("src")).unwrap();
        std::fs::write(p.join("src/a.rs"), "a\n").unwrap();
        std::fs::write(p.join("src/keep.rs"), "keep\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-qm", "src"]);
        let out = Command::new("git")
            .arg("-C")
            .arg(p)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let base = String::from_utf8(out.stdout).unwrap().trim().to_owned();
        (d, base, vec!["src/a.rs".into(), "src/new.rs".into()])
    }

    const HUNK_A: &str =
        "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-a\n+b\n";
    const NEW_FILE: &str = "diff --git a/src/new.rs b/src/new.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/new.rs\n@@ -0,0 +1 @@\n+fn n() {}\n";

    /// Aplica com o git direto (sem o parser) e exige que a guarda recuse.
    fn guard_refuses(diff: &str, needle: &str) {
        let (d, base, allowed) = src_repo();
        apply_diff(d.path(), diff, 4096).expect("o git apply aceita este diff");
        match check_staged(d.path(), &base, &allowed) {
            Err(RunError::OutOfScope(why)) => assert!(why.contains(needle), "{why}"),
            other => panic!("esperava OutOfScope com {needle:?}, veio {other:?}"),
        }
    }

    /// C2 (a): hunk legítimo + bloco que renomeia arquivo fora da lista.
    #[test]
    fn guard_refuses_rename() {
        let diff = format!(
            "{HUNK_A}diff --git a/src/a.rs b/src/a.rs\nrename from src/keep.rs\nrename to src/evil.rs\n"
        );
        guard_refuses(&diff, "src/evil.rs");
    }

    /// C2 (b): copy para fora da lista antes de um arquivo novo legítimo.
    #[test]
    fn guard_refuses_copy() {
        let diff = format!(
            "diff --git a/src/new.rs b/src/new.rs\nsimilarity index 100%\ncopy from src/keep.rs\ncopy to src/evil2.rs\n{NEW_FILE}"
        );
        guard_refuses(&diff, "src/evil2.rs");
    }

    /// C2 (c): arquivo novo em path permitido, mas como symlink.
    #[test]
    fn guard_refuses_symlink() {
        let diff = "diff --git a/src/new.rs b/src/new.rs\nnew file mode 120000\n--- /dev/null\n+++ b/src/new.rs\n@@ -0,0 +1 @@\n+../../../../../etc/passwd\n\\ No newline at end of file\n";
        guard_refuses(diff, "120000");
    }

    #[test]
    fn guard_accepts_edit_new_file_and_delete() {
        let (d, base, allowed) = src_repo();
        apply_diff(d.path(), &format!("{HUNK_A}{NEW_FILE}"), 4096).unwrap();
        check_staged(d.path(), &base, &allowed).unwrap();
        let del = "diff --git a/src/a.rs b/src/a.rs\ndeleted file mode 100644\n--- a/src/a.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-b\n";
        apply_diff(d.path(), del, 4096).unwrap();
        check_staged(d.path(), &base, &allowed).unwrap();
    }

    #[test]
    fn path_survives_clean_env() {
        let d = repo();
        let r = run_verify(d.path(), &cmds(&["git --version"]), "exit 0", &test_opts()).unwrap();
        assert!(r.passed, "{r:?}");
    }
}

#[cfg(test)]
mod sandbox_tests {
    use super::*;
    use tempfile::TempDir;

    fn opts(sandbox: Sandbox) -> RunOptions {
        RunOptions {
            sandbox,
            timeout: Duration::from_secs(120),
            ..RunOptions::default()
        }
    }

    fn cmds(c: &[&str]) -> Vec<String> {
        c.iter().map(|s| s.to_string()).collect()
    }

    /// `<tmp>/pai/wt`: o worktree tem `.git`; o pai guarda o "repo do usuário".
    fn layout() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let parent = d.path().canonicalize().unwrap().join("pai");
        let wt = parent.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let st = Command::new("git")
            .args(["init", "-q"])
            .arg(&wt)
            .status()
            .unwrap();
        assert!(st.success());
        (d, parent, wt)
    }

    fn passes(wt: &Path, cmd: &str, sandbox: Sandbox) -> VerifyReport {
        run_verify(wt, &cmds(&[cmd]), "exit 0", &opts(sandbox)).unwrap()
    }

    #[test]
    fn none_runs_and_leaves_a_warning() {
        let (_d, _p, wt) = layout();
        let r = passes(&wt, "echo oi", Sandbox::None);
        assert!(r.passed, "{r:?}");
        assert!(
            r.warnings.iter().any(|w| w.contains("sandbox = \"none\"")),
            "{r:?}"
        );
    }

    #[test]
    fn sandbox_config_parses_and_defaults_to_bwrap() {
        assert_eq!(VerifyConfig::parse("").unwrap().sandbox, Sandbox::Bwrap);
        let c = VerifyConfig::parse("sandbox = \"none\"").unwrap();
        assert_eq!(c.sandbox, Sandbox::None);
        assert!(VerifyConfig::parse("sandbox = \"docker\"").is_err());
        let lang = "[[language]]\nname=\"r\"\nglobs=[\"*.rs\"]\ncommands=[\"x\"]\n";
        let p =
            VerifyConfig::parse(&format!("{lang}prefetch=[\"cargo fetch --locked\"]\n")).unwrap();
        assert_eq!(
            RunOptions::from_config(&p).prefetch,
            ["cargo fetch --locked"]
        );
        assert!(VerifyConfig::parse(&format!("{lang}prefetch=[\" \"]\n")).is_err());
    }

    #[test]
    fn prefetch_runs_outside_and_failure_stops() {
        let (_d, _p, wt) = layout();
        let ok = RunOptions {
            prefetch: cmds(&["echo marcador> marker.txt"]),
            ..opts(Sandbox::Bwrap)
        };
        // Roda sem caixa mesmo com sandbox = bwrap (inclusive onde bwrap não existe).
        run_prefetch(&wt, &ok).unwrap();
        assert!(wt.join("marker.txt").exists());
        let bad = RunOptions {
            prefetch: cmds(&["exit 3", "echo nunca> depois.txt"]),
            ..ok
        };
        match run_prefetch(&wt, &bad) {
            Err(RunError::PrefetchFailed { command, log }) => {
                assert_eq!(command, "exit 3");
                assert!(log.contains("exit Some(3)"), "{log}");
            }
            other => panic!("esperava PrefetchFailed, veio {other:?}"),
        }
        assert!(!wt.join("depois.txt").exists());
    }

    /// Windows e macOS: o padrão não é atendível; recusa sem executar nada.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn default_sandbox_is_refused_without_bwrap_platform() {
        let (_d, _p, wt) = layout();
        let r = run_verify(
            &wt,
            &cmds(&["echo x> executou.txt"]),
            "exit 0",
            &RunOptions::default(),
        );
        match r {
            Err(RunError::SandboxUnavailable(m)) => {
                assert!(m.contains("sandbox indisponível nesta plataforma"), "{m}");
                assert!(m.contains("WSL") && m.contains("sandbox = \"none\""), "{m}");
            }
            other => panic!("esperava SandboxUnavailable, veio {other:?}"),
        }
        assert!(!wt.join("executou.txt").exists());
    }

    #[cfg(target_os = "linux")]
    mod linux {
        use super::*;
        use std::net::TcpListener;
        use std::path::PathBuf;

        /// `false` = pular (só fora do CI). No CI a falta de bwrap, ou de
        /// userns, é falha: o teste de sandbox nunca some em silêncio.
        fn have_bwrap() -> bool {
            let ci = std::env::var_os("CI").is_some_and(|v| v == "true");
            match sandbox_probe() {
                Ok(()) => true,
                Err(e) if ci => panic!("CI=true e o bwrap não funciona: {e}"),
                Err(e) => {
                    eprintln!("PULADO: bwrap indisponível ({e})");
                    false
                }
            }
        }

        #[test]
        fn fails_closed_without_bwrap_in_path() {
            let (d, _p, wt) = layout();
            let vazio = d.path().join("vazio");
            std::fs::create_dir(&vazio).unwrap();
            let o = RunOptions {
                search_path: Some(vazio.into_os_string()),
                ..opts(Sandbox::Bwrap)
            };
            match run_verify(&wt, &cmds(&["touch executou.txt"]), "exit 0", &o) {
                Err(RunError::SandboxUnavailable(m)) => {
                    assert!(m.contains("bwrap não está no PATH"), "{m}");
                    assert!(m.contains("sandbox = \"none\""), "{m}");
                }
                other => panic!("esperava SandboxUnavailable, veio {other:?}"),
            }
            assert!(!wt.join("executou.txt").exists());
        }

        #[test]
        fn no_network_from_inside() {
            if !have_bwrap() {
                return;
            }
            let (_d, _p, wt) = layout();
            // Nunca rede real: só um listener em loopback, fora da caixa.
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.set_nonblocking(true).unwrap();
            let port = l.local_addr().unwrap().port();
            let cmd = format!("bash -c 'exec 3<>/dev/tcp/127.0.0.1/{port}'");
            // Controle: sem caixa o comando conecta.
            assert!(passes(&wt, &cmd, Sandbox::None).passed);
            assert!(l.accept().is_ok(), "controle não conectou");
            // Dentro da caixa: sem rede.
            let r = passes(&wt, &cmd, Sandbox::Bwrap);
            assert!(!r.passed, "{r:?}");
            assert!(l.accept().is_err(), "a caixa alcançou o listener");
        }

        #[test]
        fn cannot_read_user_disk() {
            if !have_bwrap() {
                return;
            }
            let (_d, parent, wt) = layout();
            std::fs::write(parent.join(".env"), "KEY=SEGREDO-123\n").unwrap();
            let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
            let home_file = home.join(format!(".openkax-sandbox-test-{}", std::process::id()));
            std::fs::write(&home_file, "SEGREDO-123\n").unwrap();
            // Controle: sem caixa os dois são legíveis.
            let ctl_env = passes(&wt, "cat ../.env", Sandbox::None);
            let ctl_home = passes(&wt, &format!("cat {}", home_file.display()), Sandbox::None);
            let mut leaks = Vec::new();
            for cmd in [
                "cat ../.env".to_owned(),
                format!("cat {}", home_file.display()),
            ] {
                let r = passes(&wt, &cmd, Sandbox::Bwrap);
                if r.passed || r.outcomes[0].log.contains("SEGREDO-123") {
                    leaks.push(format!("{cmd}: {r:?}"));
                }
            }
            let _ = std::fs::remove_file(&home_file);
            assert!(ctl_env.passed && ctl_home.passed, "controles falharam");
            assert!(leaks.is_empty(), "{leaks:?}");
        }

        #[test]
        fn writes_only_inside_worktree_and_tmp() {
            if !have_bwrap() {
                return;
            }
            let (_d, parent, wt) = layout();
            let tmp = format!("/tmp/openkax-sbx-{}", std::process::id());
            assert!(passes(&wt, "echo x > dentro.txt", Sandbox::Bwrap).passed);
            assert!(wt.join("dentro.txt").exists());
            let r = passes(&wt, &format!("echo x > {tmp} && cat {tmp}"), Sandbox::Bwrap);
            assert!(r.passed, "{r:?}");
            assert!(!Path::new(&tmp).exists(), "/tmp da caixa vazou para o host");
            // O pai do worktree só existe na caixa como ponto de montagem em memória.
            let _ = passes(&wt, "echo x > ../fora.txt", Sandbox::Bwrap);
            assert!(!parent.join("fora.txt").exists());
            assert!(!passes(&wt, "echo x > /usr/fora.txt", Sandbox::Bwrap).passed);
        }

        #[test]
        fn cargo_test_runs_offline_inside() {
            if !have_bwrap() {
                return;
            }
            let (_d, _p, wt) = layout();
            std::fs::create_dir(wt.join("src")).unwrap();
            std::fs::write(
                wt.join("Cargo.toml"),
                "[package]\nname = \"mini\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            )
            .unwrap();
            std::fs::write(
                wt.join("src/lib.rs"),
                "pub fn dois() -> u8 { 2 }\n#[cfg(test)]\nmod t { #[test] fn ok() { assert_eq!(super::dois(), 2); } }\n",
            )
            .unwrap();
            let r = passes(&wt, "cargo test", Sandbox::Bwrap);
            assert!(r.passed, "{}", r.outcomes[0].log);
            assert!(r.outcomes[0].log.contains("1 passed"), "{r:?}");
            assert!(wt.join("target").exists());
        }
    }
}
