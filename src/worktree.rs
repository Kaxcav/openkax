//! Worktree git por tarefa — **Fase 2** (F2-2).
//!
//! Cada tarefa roda em `<repo>/.kax/wt/<task_id>` numa branch `kax/<task_id>`.
//! Garantias: `task_id` restrito a `[A-Za-z0-9._-]` (sem `..`), caminho final
//! descendente de `<repo>/.kax/wt/`, `.kax/` fora do versionamento via
//! `.git/info/exclude` e remoção só de branches `kax/*`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Prefixo das branches criadas pela esteira.
pub const BRANCH_PREFIX: &str = "kax/";

/// Um worktree de tarefa.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub task_id: String,
    pub path: PathBuf,
    pub branch: String,
}

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("task_id inválido {0:?}: use só [A-Za-z0-9._-], sem '..' e não vazio")]
    InvalidTaskId(String),
    #[error("{0} não é um repositório git")]
    NotAGitRepo(String),
    #[error("base_ref {0:?} não existe no repositório")]
    BaseRefNotFound(String),
    #[error("worktree {0} já existe")]
    AlreadyExists(String),
    #[error("caminho {0} escapa de .kax/wt")]
    PathEscapes(String),
    #[error("worktree da tarefa {0:?} não encontrado")]
    NotFound(String),
    #[error("git {args}: {stderr}")]
    Git { args: String, stderr: String },
    #[error("erro de E/S: {0}")]
    Io(#[from] std::io::Error),
}

/// Valida o `task_id`: `[A-Za-z0-9._-]+`, sem `..`, sem começar com `.` ou `-`.
pub fn validate_task_id(id: &str) -> Result<(), WorktreeError> {
    let ok = !id.is_empty()
        && !id.contains("..")
        && !id.starts_with(['.', '-'])
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(WorktreeError::InvalidTaskId(id.to_string()))
    }
}

fn git(repo: &Path, args: &[&str]) -> Result<String, WorktreeError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(WorktreeError::Git {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

/// Raiz do worktree principal do repo (erro claro se não for git).
fn repo_root(repo: &Path) -> Result<PathBuf, WorktreeError> {
    let not_git = || WorktreeError::NotAGitRepo(repo.display().to_string());
    if !repo.is_dir() {
        return Err(not_git());
    }
    let top = git(repo, &["rev-parse", "--show-toplevel"]).map_err(|_| not_git())?;
    // Sem canonicalize: no Windows geraria `\?\`, que o git recusa.
    Ok(PathBuf::from(top.trim()))
}

fn wt_dir(root: &Path) -> PathBuf {
    root.join(".kax").join("wt")
}

/// Garante `.kax/` em `.git/info/exclude` (nunca no `.gitignore` versionado).
fn ensure_excluded(root: &Path) -> Result<(), WorktreeError> {
    let rel = git(root, &["rev-parse", "--git-path", "info/exclude"])?;
    let rel = PathBuf::from(rel.trim());
    let path = if rel.is_absolute() {
        rel
    } else {
        root.join(rel)
    };
    let current = fs::read_to_string(&path).unwrap_or_default();
    if current.lines().any(|l| l.trim() == ".kax/") {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let sep = if current.is_empty() || current.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    fs::write(&path, format!("{current}{sep}.kax/\n"))?;
    Ok(())
}

/// Cria `<repo>/.kax/wt/<task_id>` na branch nova `kax/<task_id>` a partir de `base_ref`.
pub fn create(repo: &Path, base_ref: &str, task_id: &str) -> Result<Worktree, WorktreeError> {
    validate_task_id(task_id)?;
    let root = repo_root(repo)?;
    if base_ref.starts_with('-')
        || git(
            &root,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{base_ref}^{{commit}}"),
            ],
        )
        .is_err()
    {
        return Err(WorktreeError::BaseRefNotFound(base_ref.to_string()));
    }
    let base = wt_dir(&root);
    let path = base.join(task_id);
    // Defesa em profundidade: o id já foi validado, mas confirma a descendência.
    if path.parent() != Some(base.as_path()) {
        return Err(WorktreeError::PathEscapes(path.display().to_string()));
    }
    if path.exists() {
        return Err(WorktreeError::AlreadyExists(path.display().to_string()));
    }
    ensure_excluded(&root)?;
    fs::create_dir_all(&base)?;
    let branch = format!("{BRANCH_PREFIX}{task_id}");
    git(
        &root,
        &[
            "worktree",
            "add",
            &path.to_string_lossy(),
            "-b",
            &branch,
            base_ref,
        ],
    )?;
    Ok(Worktree {
        task_id: task_id.to_string(),
        path,
        branch,
    })
}

/// Lista os worktrees de tarefa (`.kax/wt/*` em branch `kax/*`).
pub fn list(repo: &Path) -> Result<Vec<Worktree>, WorktreeError> {
    let root = repo_root(repo)?;
    let base = fs::canonicalize(wt_dir(&root)).unwrap_or_else(|_| wt_dir(&root));
    let raw = git(&root, &["worktree", "list", "--porcelain"])?;
    let mut found = Vec::new();
    for block in raw.split("\n\n") {
        let mut path = None;
        let mut branch = None;
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                branch = Some(b.to_string());
            }
        }
        let (Some(path), Some(branch)) = (path, branch) else {
            continue;
        };
        let Some(task_id) = branch.strip_prefix(BRANCH_PREFIX) else {
            continue;
        };
        let inside = fs::canonicalize(&path)
            .map(|p| p.parent() == Some(base.as_path()))
            .unwrap_or(false);
        if inside {
            found.push(Worktree {
                task_id: task_id.to_string(),
                path,
                branch,
            });
        }
    }
    found.sort_by(|a, b| a.task_id.cmp(&b.task_id));
    Ok(found)
}

/// Remove o worktree da tarefa e sua branch `kax/<task_id>` (nunca outra).
/// Quem chama não deve remover tarefas já aceitas (a branch é a entrega).
pub fn remove(repo: &Path, task_id: &str) -> Result<(), WorktreeError> {
    validate_task_id(task_id)?;
    let root = repo_root(repo)?;
    let wt = list(&root)?
        .into_iter()
        .find(|w| w.task_id == task_id)
        .ok_or_else(|| WorktreeError::NotFound(task_id.to_string()))?;
    git(
        &root,
        &["worktree", "remove", "--force", &wt.path.to_string_lossy()],
    )?;
    if wt.branch.starts_with(BRANCH_PREFIX) {
        git(&root, &["branch", "-D", &wt.branch])?;
    }
    Ok(())
}

/// Descarta alterações não commitadas (tracked e novas) do worktree, para a
/// próxima tentativa partir do `base_ref`.
pub fn reset(path: &Path) -> Result<(), WorktreeError> {
    git(path, &["reset", "--hard", "-q", "HEAD"])?;
    git(path, &["clean", "-fdq"])?;
    Ok(())
}

/// Diff do que está no índice contra o HEAD do worktree (após `git add -A`).
pub fn staged_diff(path: &Path) -> Result<String, WorktreeError> {
    git(path, &["add", "-A"])?;
    git(path, &["diff", "--cached", "HEAD"])
}

/// Commita tudo que está no worktree como `message` (identidade fixa do
/// openkax, sem depender da config git do usuário nem de assinatura).
pub fn commit_all(path: &Path, message: &str) -> Result<(), WorktreeError> {
    git(path, &["add", "-A"])?;
    git(
        path,
        &[
            "-c",
            "user.name=openkax",
            "-c",
            "user.email=openkax@localhost",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--no-verify",
            "-m",
            message,
        ],
    )?;
    Ok(())
}

/// Tarefa abandonada: worktree sem alterações e sem commits além do HEAD do repo.
fn is_abandoned(root: &Path, wt: &Worktree) -> bool {
    let clean = git(&wt.path, &["status", "--porcelain"]).map(|s| s.trim().is_empty());
    let ahead = git(
        root,
        &["rev-list", "--count", &format!("HEAD..{}", wt.branch)],
    )
    .map(|s| s.trim() == "0");
    matches!((clean, ahead), (Ok(true), Ok(true)))
}

/// `openkax clean`: com `all`, remove todo worktree de tarefa; sem, só os
/// abandonados (sem alterações nem commits novos). Devolve os ids removidos.
pub fn clean(repo: &Path, all: bool) -> Result<Vec<String>, WorktreeError> {
    let root = repo_root(repo)?;
    let mut removed = Vec::new();
    for wt in list(&root)? {
        if all || is_abandoned(&root, &wt) {
            remove(&root, &wt.task_id)?;
            removed.push(wt.task_id);
        }
    }
    let _ = git(&root, &["worktree", "prune"]);
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let st = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    }

    fn temp_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-q", "-b", "main"]);
        fs::write(dir.path().join("a.txt"), "a").unwrap();
        run(dir.path(), &["add", "."]);
        run(dir.path(), &["commit", "-q", "-m", "init"]);
        dir
    }

    #[test]
    fn create_list_remove() {
        let repo = temp_repo();
        let wt = create(repo.path(), "main", "T-1").unwrap();
        assert!(wt.path.join("a.txt").exists());
        assert_eq!(wt.branch, "kax/T-1");
        let all = list(repo.path()).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].task_id, "T-1");
        remove(repo.path(), "T-1").unwrap();
        assert!(list(repo.path()).unwrap().is_empty());
        assert!(!wt.path.exists());
        let branches = git(repo.path(), &["branch", "--list", "kax/*"]).unwrap();
        assert!(branches.trim().is_empty());
    }

    #[test]
    fn kax_is_excluded_not_gitignored() {
        let repo = temp_repo();
        create(repo.path(), "main", "T-1").unwrap();
        let exclude = fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap();
        assert!(exclude.lines().any(|l| l == ".kax/"));
        assert!(!repo.path().join(".gitignore").exists());
        assert!(
            git(repo.path(), &["status", "--porcelain"])
                .unwrap()
                .trim()
                .is_empty()
        );
        create(repo.path(), "main", "T-2").unwrap();
        let exclude = fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.lines().filter(|l| *l == ".kax/").count(), 1);
    }

    #[test]
    fn malicious_ids_refused() {
        let repo = temp_repo();
        for id in [
            "../x", "a/b", "a\\b", "..", "", ".hidden", "-x", "a b", "x..y",
        ] {
            assert!(
                matches!(
                    create(repo.path(), "main", id),
                    Err(WorktreeError::InvalidTaskId(_))
                ),
                "id {id:?}"
            );
        }
        assert!(!repo.path().join(".kax").exists());
    }

    #[test]
    fn unknown_base_ref_errors() {
        let repo = temp_repo();
        assert!(matches!(
            create(repo.path(), "nao-existe", "T-1"),
            Err(WorktreeError::BaseRefNotFound(_))
        ));
        assert!(list(repo.path()).unwrap().is_empty());
    }

    #[test]
    fn not_a_git_repo_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            create(dir.path(), "main", "T-1"),
            Err(WorktreeError::NotAGitRepo(_))
        ));
    }

    #[test]
    fn duplicate_id_errors() {
        let repo = temp_repo();
        create(repo.path(), "main", "T-1").unwrap();
        assert!(create(repo.path(), "main", "T-1").is_err());
    }

    #[test]
    fn parallel_tasks_do_not_collide() {
        let repo = temp_repo();
        let a = create(repo.path(), "main", "A").unwrap();
        let b = create(repo.path(), "main", "B").unwrap();
        fs::write(a.path.join("only-a.txt"), "x").unwrap();
        assert!(!b.path.join("only-a.txt").exists());
        assert_eq!(list(repo.path()).unwrap().len(), 2);
    }

    #[test]
    fn remove_never_touches_foreign_branches() {
        let repo = temp_repo();
        run(repo.path(), &["branch", "feature"]);
        assert!(matches!(
            remove(repo.path(), "feature"),
            Err(WorktreeError::NotFound(_))
        ));
        let branches = git(repo.path(), &["branch", "--list", "feature"]).unwrap();
        assert!(branches.contains("feature"));
    }

    #[test]
    fn clean_removes_only_abandoned_unless_all() {
        let repo = temp_repo();
        create(repo.path(), "main", "idle").unwrap();
        let busy = create(repo.path(), "main", "busy").unwrap();
        fs::write(busy.path.join("new.txt"), "x").unwrap();
        assert_eq!(clean(repo.path(), false).unwrap(), vec!["idle".to_string()]);
        assert_eq!(list(repo.path()).unwrap().len(), 1);
        assert_eq!(clean(repo.path(), true).unwrap(), vec!["busy".to_string()]);
        assert!(list(repo.path()).unwrap().is_empty());
    }
}
