use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Default bound for every `git` invocation.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Branch prefix for managed chat worktrees.
pub const MANAGED_PREFIX: &str = "batey/chat/";

/// Bounded error type. `Conflict` signals a safe refusal, never a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceError {
    /// Safe refusal with a clear reason (dirty checkout, external switch...).
    Conflict(String),
    /// Hard failure (missing repo, missing branch, git error...).
    Failed(String),
}

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict(m) | Self::Failed(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for WorkspaceError {}

/// Repository inspection result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    pub is_git: bool,
    pub root: Option<PathBuf>,
    pub subdir: Option<PathBuf>,
    pub branch: Option<String>,
    pub head_sha: Option<String>,
    pub dirty: bool,
}

/// One local branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchInfo {
    pub name: String,
    pub sha: String,
    pub current: bool,
}

/// Managed worktree location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedPaths {
    pub branch: String,
    pub worktree: PathBuf,
}

/// Outcome of managed recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovered {
    Reused(PathBuf),
    Recreated(PathBuf),
}

struct GitOutput {
    stdout: String,
}

/// Run `git` with captured output, `GIT_TERMINAL_PROMPT=0`, no network, bounded time.
fn run_git_output(
    dir: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<GitOutput, WorkspaceError> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| WorkspaceError::Failed(format!("cannot run git: {e}")))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_reader = thread::spawn(move || {
        let mut output = String::new();
        std::io::Read::read_to_string(&mut std::io::BufReader::new(stdout), &mut output)
            .map(|_| output)
    });
    let stderr_reader = thread::spawn(move || {
        let mut output = String::new();
        std::io::Read::read_to_string(&mut std::io::BufReader::new(stderr), &mut output)
            .map(|_| output)
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = stdout_reader
                    .join()
                    .map_err(|_| WorkspaceError::Failed("git stdout reader panicked".to_string()))?
                    .map_err(|e| WorkspaceError::Failed(format!("cannot read git stdout: {e}")))?;
                let err = stderr_reader
                    .join()
                    .map_err(|_| WorkspaceError::Failed("git stderr reader panicked".to_string()))?
                    .map_err(|e| WorkspaceError::Failed(format!("cannot read git stderr: {e}")))?;
                if status.success() {
                    return Ok(GitOutput { stdout: out });
                }
                let detail = if err.trim().is_empty() {
                    out.trim().to_string()
                } else {
                    err.trim().to_string()
                };
                return Err(WorkspaceError::Failed(format!(
                    "git {} failed: {detail}",
                    args.join(" ")
                )));
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(WorkspaceError::Failed(format!(
                        "git {} timed out",
                        args.join(" ")
                    )));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(WorkspaceError::Failed(format!("git wait failed: {e}")));
            }
        }
    }
}

fn run_git(dir: &Path, args: &[&str], timeout: Duration) -> Result<String, WorkspaceError> {
    Ok(run_git_output(dir, args, timeout)?
        .stdout
        .trim_end()
        .to_string())
}

fn run_git_ok(dir: &Path, args: &[&str]) -> Result<String, WorkspaceError> {
    run_git(dir, args, GIT_TIMEOUT)
}

fn is_hex40(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn not_git() -> RepoInfo {
    RepoInfo {
        is_git: false,
        root: None,
        subdir: None,
        branch: None,
        head_sha: None,
        dirty: false,
    }
}

/// Check for a `.git` marker on the path or one of its parents.
///
/// A marker makes a failed Git probe a broken/inaccessible repository rather
/// than an ordinary non-Git directory. The conservative result is important:
/// a damaged marker must never become a non-Git fallback.
fn has_git_metadata(path: &Path) -> Result<bool, WorkspaceError> {
    let original = path;
    let mut current = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent().unwrap_or(path).to_path_buf()
    };
    loop {
        match std::fs::symlink_metadata(current.join(".git")) {
            Ok(metadata) if metadata.is_dir() && current != original => {
                let mut entries = std::fs::read_dir(current.join(".git")).map_err(|e| {
                    WorkspaceError::Failed(format!(
                        "cannot inspect Git metadata at {}: {e}",
                        current.display()
                    ))
                })?;
                if entries
                    .next()
                    .transpose()
                    .map_err(|e| {
                        WorkspaceError::Failed(format!(
                            "cannot inspect Git metadata at {}: {e}",
                            current.display()
                        ))
                    })?
                    .is_some()
                {
                    return Ok(true);
                }
            }
            Ok(_) => return Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(WorkspaceError::Failed(format!(
                    "cannot inspect Git metadata at {}: {e}",
                    current.display()
                )));
            }
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if current == Path::new("/") || parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    Ok(false)
}

fn is_not_a_repository_error(error: &WorkspaceError) -> bool {
    matches!(
        error,
        WorkspaceError::Failed(message) if message.contains("not a git repository")
    )
}

/// Inspect a path. An ordinary existing non-Git directory is a successful
/// `is_git=false` result; Git failures are returned to the caller.
pub fn inspect(path: &Path) -> Result<RepoInfo, WorkspaceError> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Ok(not_git()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(not_git()),
        Err(e) => {
            return Err(WorkspaceError::Failed(format!(
                "cannot inspect workspace path {}: {e}",
                path.display()
            )));
        }
    }

    let root_output = match run_git_output(path, &["rev-parse", "--show-toplevel"], GIT_TIMEOUT) {
        Ok(output) => output.stdout,
        Err(error) if is_not_a_repository_error(&error) && !has_git_metadata(path)? => {
            return Ok(not_git());
        }
        Err(error) => return Err(error),
    };
    // Canonical root: resolve symlinks where possible.
    let root = PathBuf::from(root_output.trim())
        .canonicalize()
        .map_err(|e| WorkspaceError::Failed(format!("cannot canonicalize Git root: {e}")))?;
    let canonical_path = path
        .canonicalize()
        .map_err(|e| WorkspaceError::Failed(format!("cannot canonicalize workspace path: {e}")))?;
    let subdir = canonical_path
        .strip_prefix(&root)
        .ok()
        .map(Path::to_path_buf)
        .filter(|s| !s.as_os_str().is_empty());
    let head_sha = run_git_ok(path, &["rev-parse", "HEAD"])?.trim().to_string();
    if !is_hex40(&head_sha) {
        return Err(WorkspaceError::Failed(
            "Git HEAD is not a commit SHA".to_string(),
        ));
    }
    let branch = run_git_ok(path, &["branch", "--show-current"])?
        .trim()
        .to_string();
    let branch = (!branch.is_empty()).then_some(branch);
    let dirty = !run_git_ok(path, &["status", "--porcelain"])?
        .trim()
        .is_empty();
    Ok(RepoInfo {
        is_git: true,
        root: Some(root),
        subdir,
        branch,
        head_sha: Some(head_sha),
        dirty,
    })
}

/// List local branches. Never touches the network.
pub fn list_local_branches(repo: &Path) -> Result<Vec<BranchInfo>, WorkspaceError> {
    let out = run_git_ok(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname:short)%00%(objectname)%00%(HEAD)",
            "refs/heads",
        ],
    )?;
    let mut out_v = Vec::new();
    for line in out.lines() {
        let mut parts = line.split('\0');
        let (Some(name), Some(sha), Some(head)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if name.is_empty() || !is_hex40(sha.trim()) {
            continue;
        }
        out_v.push(BranchInfo {
            name: name.to_string(),
            sha: sha.trim().to_string(),
            current: head.trim() == "*",
        });
    }
    out_v.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out_v)
}

/// Resolve a local branch or ref to an immutable commit SHA.
pub fn resolve_ref(repo: &Path, name: &str) -> Result<String, WorkspaceError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(WorkspaceError::Failed("empty ref".to_string()));
    }
    let sha = run_git_ok(
        repo,
        &["rev-parse", "--verify", &format!("{name}^{{commit}}")],
    )?;
    let sha = sha.trim().to_string();
    if !is_hex40(&sha) {
        return Err(WorkspaceError::Failed(format!("unresolvable ref: {name}")));
    }
    Ok(sha)
}

fn local_branch_tip(repo: &Path, branch: &str) -> Result<Option<String>, WorkspaceError> {
    Ok(list_local_branches(repo)?
        .into_iter()
        .find(|candidate| candidate.name == branch)
        .map(|candidate| candidate.sha))
}

fn resolve_local_branch(repo: &Path, branch: &str) -> Result<String, WorkspaceError> {
    let branch = branch.trim();
    if branch.is_empty() {
        return Err(WorkspaceError::Failed("empty branch".to_string()));
    }
    local_branch_tip(repo, branch)?
        .ok_or_else(|| WorkspaceError::Failed(format!("branch does not exist locally: {branch}")))
}

fn validate_chat_id(chat_id: &str) -> Result<(), WorkspaceError> {
    if chat_id.trim().is_empty() || chat_id.contains('/') || chat_id.contains('.') {
        return Err(WorkspaceError::Failed("invalid chat id".to_string()));
    }
    Ok(())
}

fn managed_branch(chat_id: &str) -> String {
    format!("{MANAGED_PREFIX}{chat_id}")
}

/// Whether persisted metadata names the canonical managed branch for this chat.
pub fn managed_branch_matches(branch: &str, chat_id: &str) -> bool {
    branch == managed_branch(chat_id)
}

fn existing_managed_branch(repo: &Path, chat_id: &str) -> Result<String, WorkspaceError> {
    let branch = managed_branch(chat_id);
    if local_branch_tip(repo, &branch)?.is_some() {
        return Ok(branch);
    }
    Err(WorkspaceError::Failed(format!(
        "managed branch missing: {branch}"
    )))
}

/// Provision an isolated branch + external worktree from `base_commit`.
///
/// Never modifies the primary checkout. Only ordinary `git` commands run.
pub fn provision_managed(
    repo: &Path,
    workspace_root: &Path,
    chat_id: &str,
    base_commit: &str,
) -> Result<ManagedPaths, WorkspaceError> {
    validate_chat_id(chat_id)?;
    if !is_hex40(base_commit.trim()) {
        return Err(WorkspaceError::Failed(
            "base commit must be a SHA".to_string(),
        ));
    }
    let base = base_commit.trim().to_string();
    // Verify base resolves to a commit in this repo.
    let verified = run_git_ok(
        repo,
        &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
    )?;
    if verified.trim() != base {
        return Err(WorkspaceError::Failed("unknown base commit".to_string()));
    }
    let branch = managed_branch(chat_id);
    if local_branch_tip(repo, &branch)?.is_some() {
        return Err(WorkspaceError::Conflict(format!(
            "managed branch already exists: {branch}"
        )));
    }
    run_git_ok(repo, &["branch", &branch, &base])?;
    if let Err(e) = std::fs::create_dir_all(workspace_root) {
        let _ = rollback_provision(repo, &branch, &base, true);
        return Err(WorkspaceError::Failed(format!(
            "cannot create workspace root: {e}"
        )));
    }
    let worktree = workspace_root.join(chat_id);
    if worktree.exists() {
        let _ = rollback_provision(repo, &branch, &base, true);
        return Err(WorkspaceError::Failed(
            "worktree path already exists".to_string(),
        ));
    }
    let worktree_arg = match worktree_string(&worktree) {
        Ok(value) => value,
        Err(error) => {
            let _ = rollback_provision(repo, &branch, &base, true);
            return Err(error);
        }
    };
    if let Err(e) = run_git_ok(repo, &["worktree", "add", &worktree_arg, &branch]) {
        let _ = rollback_provision(repo, &branch, &base, true);
        return Err(e);
    }
    Ok(ManagedPaths { branch, worktree })
}

fn worktree_string(p: &Path) -> Result<String, WorkspaceError> {
    p.to_str()
        .map(str::to_string)
        .ok_or_else(|| WorkspaceError::Failed("non-utf8 path".to_string()))
}

/// Prepare the primary checkout for direct use of `branch`.
///
/// Already checked out: files and index stay untouched, even when dirty.
/// Other branch: switch only when clean. Never stash, reset, clean, or force.
pub fn prepare_direct(repo: &Path, branch: &str) -> Result<(), WorkspaceError> {
    let branch = branch.trim();
    if branch.is_empty() {
        return Err(WorkspaceError::Failed("empty branch".to_string()));
    }
    // Branch must exist locally.
    resolve_local_branch(repo, branch)?;
    let current = run_git_ok(repo, &["branch", "--show-current"])?
        .trim()
        .to_string();
    if current == branch {
        return Ok(());
    }
    let dirty = !run_git_ok(repo, &["status", "--porcelain"])?
        .trim()
        .is_empty();
    if dirty {
        return Err(WorkspaceError::Conflict(format!(
            "checkout is dirty; refuse to switch from {current} to {branch}"
        )));
    }
    run_git_ok(repo, &["checkout", branch])?;
    Ok(())
}

/// Validate a direct workspace: repo matches, branch exists, checkout still on it.
pub fn validate_direct(
    repo: &Path,
    expected_root: &Path,
    expected_branch: &str,
) -> Result<(), WorkspaceError> {
    let info = inspect(repo)?;
    if !info.is_git {
        return Err(WorkspaceError::Failed(
            "repository no longer exists".to_string(),
        ));
    }
    let root = info
        .root
        .ok_or_else(|| WorkspaceError::Failed("Git root is unavailable".to_string()))?;
    let canon_expected = canonical_path(expected_root)?;
    let canon_root = canonical_path(&root)?;
    if canon_root != canon_expected {
        return Err(WorkspaceError::Failed("repository mismatch".to_string()));
    }
    resolve_local_branch(repo, expected_branch)?;
    match info.branch {
        Some(cur) if cur == expected_branch => Ok(()),
        Some(cur) => Err(WorkspaceError::Conflict(format!(
            "checkout moved externally to {cur}; expected {expected_branch}"
        ))),
        None => Err(WorkspaceError::Conflict("checkout is detached".to_string())),
    }
}

fn canonical_path(path: &Path) -> Result<PathBuf, WorkspaceError> {
    path.canonicalize().map_err(|e| {
        WorkspaceError::Failed(format!(
            "cannot canonicalize workspace path {}: {e}",
            path.display()
        ))
    })
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn same_path(left: &Path, right: &Path) -> Result<bool, WorkspaceError> {
    Ok(canonical_path(left)? == canonical_path(right)?)
}

fn git_common_dir(dir: &Path) -> Result<PathBuf, WorkspaceError> {
    let raw = run_git_ok(dir, &["rev-parse", "--git-common-dir"])?;
    let common = Path::new(raw.trim());
    let common = if common.is_absolute() {
        common.to_path_buf()
    } else {
        dir.join(common)
    };
    canonical_path(&common)
}

fn git_top_level(dir: &Path) -> Result<PathBuf, WorkspaceError> {
    let raw = run_git_ok(dir, &["rev-parse", "--show-toplevel"])?;
    let top = Path::new(raw.trim());
    let top = if top.is_absolute() {
        top.to_path_buf()
    } else {
        dir.join(top)
    };
    canonical_path(&top)
}

#[derive(Debug)]
struct RegisteredWorktree {
    path: PathBuf,
    branch: Option<String>,
}

fn registered_worktrees(repo: &Path) -> Result<Vec<RegisteredWorktree>, WorkspaceError> {
    let list = run_git_ok(repo, &["worktree", "list", "--porcelain"])?;
    let mut worktrees = Vec::new();
    let mut path = None;
    let mut branch = None;
    let mut finish = |path: &mut Option<PathBuf>, branch: &mut Option<String>| {
        if let Some(path) = path.take() {
            worktrees.push(RegisteredWorktree {
                path,
                branch: branch.take(),
            });
        } else {
            branch.take();
        }
    };
    for line in list.lines() {
        if line.is_empty() {
            finish(&mut path, &mut branch);
        } else if let Some(value) = line.strip_prefix("worktree ") {
            finish(&mut path, &mut branch);
            path = Some(PathBuf::from(value));
        } else if let Some(value) = line.strip_prefix("branch refs/heads/") {
            branch = Some(value.to_string());
        }
    }
    finish(&mut path, &mut branch);
    Ok(worktrees)
}

fn registered_managed_worktree(
    repo: &Path,
    worktree: &Path,
    branch: &str,
    repair: bool,
) -> Result<(), WorkspaceError> {
    let expected_common = git_common_dir(repo)?;
    let records = registered_worktrees(repo)?;
    let mut registered = None;
    for candidate in &records {
        if candidate.path.exists() && same_path(&candidate.path, worktree)? {
            registered = Some(candidate);
            break;
        }
    }
    let registered_by_branch = records
        .iter()
        .find(|candidate| candidate.branch.as_deref() == Some(branch));
    let registration_path_is_stale = registered.is_none() && registered_by_branch.is_some();
    if registered.is_none() {
        registered = registered_by_branch;
    }
    let Some(registered) = registered else {
        return Err(WorkspaceError::Failed(
            "worktree path exists but is not registered".to_string(),
        ));
    };
    if registered.branch.as_deref() != Some(branch) {
        return Err(WorkspaceError::Failed(format!(
            "worktree is registered on the wrong branch: {}",
            registered.branch.as_deref().unwrap_or("detached")
        )));
    }

    let git_marker = worktree.join(".git");
    if !git_marker.is_file() {
        return Err(WorkspaceError::Failed(
            "managed worktree has no linked Git metadata".to_string(),
        ));
    }

    if registration_path_is_stale {
        prove_stale_linked_worktree(worktree, &expected_common, branch)?;
    }

    let common_matches = match git_common_dir(worktree) {
        Ok(candidate_common) => {
            if candidate_common != expected_common {
                return Err(WorkspaceError::Failed(
                    "managed worktree repository mismatch".to_string(),
                ));
            }
            true
        }
        Err(error) => {
            if !repair {
                return Err(error);
            }
            prove_stale_linked_worktree(worktree, &expected_common, branch)?;
            false
        }
    };

    if repair {
        let worktree_arg = worktree_string(worktree)?;
        run_git_ok(repo, &["worktree", "repair", &worktree_arg])?;
    }
    if !common_matches || repair {
        let candidate_common = git_common_dir(worktree)?;
        if candidate_common != expected_common {
            return Err(WorkspaceError::Failed(
                "managed worktree repository mismatch".to_string(),
            ));
        }
    }
    let candidate_top = git_top_level(worktree)?;
    if candidate_top != canonical_path(worktree)? {
        return Err(WorkspaceError::Failed(
            "managed worktree path mismatch".to_string(),
        ));
    }
    let candidate_branch = run_git_ok(worktree, &["branch", "--show-current"])?
        .trim()
        .to_string();
    if candidate_branch != branch {
        return Err(WorkspaceError::Failed(format!(
            "worktree on wrong branch: {candidate_branch}"
        )));
    }
    Ok(())
}

fn prove_stale_linked_worktree(
    worktree: &Path,
    expected_common: &Path,
    branch: &str,
) -> Result<(), WorkspaceError> {
    let marker = std::fs::read_to_string(worktree.join(".git"))
        .map_err(|e| WorkspaceError::Failed(format!("cannot read linked Git metadata: {e}")))?;
    let admin_raw = marker
        .trim()
        .strip_prefix("gitdir:")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| WorkspaceError::Failed("invalid linked Git metadata".to_string()))?;
    let admin_path = Path::new(admin_raw);
    let admin_path = if admin_path.is_absolute() {
        admin_path.to_path_buf()
    } else {
        worktree.join(admin_path)
    };
    let admin_path = canonical_path(&admin_path)?;
    let worktrees_root = canonical_path(&expected_common.join("worktrees"))?;
    if !admin_path.starts_with(&worktrees_root) {
        return Err(WorkspaceError::Failed(
            "cannot prove managed worktree ownership".to_string(),
        ));
    }
    let commondir = std::fs::read_to_string(admin_path.join("commondir")).map_err(|e| {
        WorkspaceError::Failed(format!("cannot read linked Git common directory: {e}"))
    })?;
    let commondir_path = Path::new(commondir.trim());
    let commondir_path = if commondir_path.is_absolute() {
        commondir_path.to_path_buf()
    } else {
        admin_path.join(commondir_path)
    };
    if canonical_path(&commondir_path)? != expected_common {
        return Err(WorkspaceError::Failed(
            "cannot prove managed worktree ownership".to_string(),
        ));
    }
    let head = std::fs::read_to_string(admin_path.join("HEAD"))
        .map_err(|e| WorkspaceError::Failed(format!("cannot read linked Git HEAD: {e}")))?;
    if head.trim() != format!("ref: refs/heads/{branch}") {
        return Err(WorkspaceError::Failed(
            "cannot prove managed worktree ownership".to_string(),
        ));
    }
    Ok(())
}

/// Recover a managed worktree without destroying user work.
pub fn recover_managed(
    repo: &Path,
    workspace_root: &Path,
    chat_id: &str,
) -> Result<Recovered, WorkspaceError> {
    validate_chat_id(chat_id)?;
    let branch = existing_managed_branch(repo, chat_id)?;
    recover_managed_on_branch(repo, workspace_root, chat_id, &branch)
}

/// Recover a managed worktree using the branch recorded by persisted chat
/// metadata, verified against the canonical name before any Git command runs.
pub fn recover_managed_on_branch(
    repo: &Path,
    workspace_root: &Path,
    chat_id: &str,
    branch: &str,
) -> Result<Recovered, WorkspaceError> {
    validate_chat_id(chat_id)?;
    if !managed_branch_matches(branch, chat_id) {
        return Err(WorkspaceError::Failed(
            "invalid managed branch for chat".to_string(),
        ));
    }
    let worktree = workspace_root.join(chat_id);
    if local_branch_tip(repo, branch)?.is_none() {
        run_git_ok(repo, &["worktree", "prune"])?;
        return Err(WorkspaceError::Failed(format!(
            "managed branch missing: {branch}"
        )));
    }
    if worktree.exists() {
        registered_managed_worktree(repo, &worktree, branch, true)?;
        return Ok(Recovered::Reused(worktree));
    }
    // Missing worktree + surviving branch: prune stale metadata, then recreate.
    run_git_ok(repo, &["worktree", "prune"])?;
    if worktree.exists() {
        return Err(WorkspaceError::Failed("worktree path blocked".to_string()));
    }
    let worktree_arg = worktree_string(&worktree)?;
    run_git_ok(repo, &["worktree", "add", &worktree_arg, branch])?;
    Ok(Recovered::Recreated(worktree))
}

/// Remove a managed worktree. Dirty worktrees are refused. Branch is preserved.
pub fn remove_managed(
    repo: &Path,
    workspace_root: &Path,
    chat_id: &str,
) -> Result<(), WorkspaceError> {
    validate_chat_id(chat_id)?;
    let branch = match existing_managed_branch(repo, chat_id) {
        Ok(branch) => branch,
        Err(WorkspaceError::Failed(message)) if message.starts_with("managed branch missing:") => {
            managed_branch(chat_id)
        }
        Err(error) => return Err(error),
    };
    remove_managed_on_branch(repo, workspace_root, chat_id, &branch)
}

/// Remove a managed worktree using persisted branch metadata, verified
/// against the canonical name before any Git command runs.
pub fn remove_managed_on_branch(
    repo: &Path,
    workspace_root: &Path,
    chat_id: &str,
    branch: &str,
) -> Result<(), WorkspaceError> {
    validate_chat_id(chat_id)?;
    if !managed_branch_matches(branch, chat_id) {
        return Err(WorkspaceError::Failed(
            "invalid managed branch for chat".to_string(),
        ));
    }
    let worktree = workspace_root.join(chat_id);
    let worktree_exists = match std::fs::symlink_metadata(&worktree) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(WorkspaceError::Failed(format!(
                "cannot inspect managed worktree: {error}"
            )));
        }
    };
    if !worktree_exists {
        let expected_path = absolute_path(&worktree);
        let records = registered_worktrees(repo)?;
        let registered_by_path = records
            .iter()
            .find(|candidate| absolute_path(&candidate.path) == expected_path);
        let registered_by_branch = records
            .iter()
            .find(|candidate| candidate.branch.as_deref() == Some(branch));

        match (registered_by_path, registered_by_branch) {
            (None, None) => {}
            (Some(candidate), Some(by_branch))
                if candidate.path == by_branch.path
                    && candidate.branch.as_deref() == Some(branch) =>
            {
                run_git_ok(repo, &["worktree", "prune"])?;
            }
            (Some(_), _) => {
                return Err(WorkspaceError::Failed(
                    "managed worktree is registered on the wrong branch".to_string(),
                ));
            }
            (None, Some(_)) => {
                return Err(WorkspaceError::Failed(
                    "managed branch is registered to a different worktree".to_string(),
                ));
            }
        }
        return Ok(());
    }
    registered_managed_worktree(repo, &worktree, branch, true)?;
    if worktree_dirty(&worktree)? {
        return Err(WorkspaceError::Conflict(
            "managed worktree is dirty; refuse to remove".to_string(),
        ));
    }
    let worktree_arg = worktree_string(&worktree)?;
    run_git_ok(repo, &["worktree", "remove", &worktree_arg])?;
    Ok(())
}

fn worktree_dirty(worktree: &Path) -> Result<bool, WorkspaceError> {
    let status = run_git_ok(
        worktree,
        &[
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignored=matching",
        ],
    )?;

    status
        .lines()
        .filter(|line| !line.trim().is_empty())
        .try_fold(false, |dirty, line| {
            if dirty {
                return Ok(true);
            }
            Ok(!is_disposable_runtime_status(worktree, line)?)
        })
}

/// The first `direnv export` in a Nix-backed workspace creates this cache in
/// the worktree. Git reports the whole ignored directory as one entry, so
/// inspect its contents before allowing it to be disposable. An unexpected
/// file, directory, or link keeps the worktree safety-sensitive.
fn is_disposable_runtime_status(worktree: &Path, status: &str) -> Result<bool, WorkspaceError> {
    let Some(path) = status.strip_prefix("!! ") else {
        return Ok(false);
    };
    let path = path.strip_suffix('/').unwrap_or(path);
    if path != ".direnv" {
        return Ok(false);
    }

    let cache = worktree.join(".direnv");
    let metadata = std::fs::symlink_metadata(&cache).map_err(|error| {
        WorkspaceError::Failed(format!(
            "cannot inspect disposable direnv cache {}: {error}",
            cache.display()
        ))
    })?;
    if !metadata.file_type().is_dir() {
        return Ok(false);
    }

    let entries = std::fs::read_dir(&cache).map_err(|error| {
        WorkspaceError::Failed(format!(
            "cannot inspect disposable direnv cache {}: {error}",
            cache.display()
        ))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            WorkspaceError::Failed(format!(
                "cannot inspect disposable direnv cache {}: {error}",
                cache.display()
            ))
        })?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !is_nix_direnv_profile_name(&name) {
            return Ok(false);
        }
        let entry_metadata = std::fs::symlink_metadata(entry.path()).map_err(|error| {
            WorkspaceError::Failed(format!(
                "cannot inspect disposable direnv cache entry {}: {error}",
                entry.path().display()
            ))
        })?;
        let target = match entry.path().canonicalize() {
            Ok(target) => target,
            Err(_) => return Ok(false),
        };
        if !entry_metadata.file_type().is_symlink() || !target.starts_with(Path::new("/nix/store"))
        {
            return Ok(false);
        }
    }

    Ok(true)
}

fn is_nix_direnv_profile_name(name: &str) -> bool {
    if name == "flake-profile" {
        return true;
    }
    let Some(generation) = name
        .strip_prefix("flake-profile-")
        .and_then(|name| name.strip_suffix("-link"))
    else {
        return false;
    };
    !generation.is_empty() && generation.bytes().all(|byte| byte.is_ascii_digit())
}

/// Whether a managed worktree currently holds uncommitted or untracked
/// changes. Read-only toward the working tree: it performs the same
/// worktree-registration repair `remove_managed_on_branch` does before
/// removing anything, but never removes the worktree or touches its files.
///
/// A missing worktree reads as not dirty; there is nothing to lose.
///
/// Used by a project-deletion preflight so a dirty chat is reported before
/// any sibling chat in the same project is deleted, rather than after.
pub fn managed_worktree_dirty(
    repo: &Path,
    workspace_root: &Path,
    chat_id: &str,
    branch: &str,
) -> Result<bool, WorkspaceError> {
    validate_chat_id(chat_id)?;
    if !managed_branch_matches(branch, chat_id) {
        return Err(WorkspaceError::Failed(
            "invalid managed branch for chat".to_string(),
        ));
    }
    let worktree = workspace_root.join(chat_id);
    match std::fs::symlink_metadata(&worktree) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(WorkspaceError::Failed(format!(
                "cannot inspect managed worktree: {error}"
            )));
        }
    }
    registered_managed_worktree(repo, &worktree, branch, true)?;
    worktree_dirty(&worktree)
}

/// Roll back provisioning only while the newly-created branch is still at the
/// exact base commit captured by the Hub.
pub fn rollback_provision(
    repo: &Path,
    branch: &str,
    expected_tip: &str,
    created_branch: bool,
) -> Result<(), WorkspaceError> {
    if !created_branch {
        return Ok(());
    }
    let expected_tip = expected_tip.trim();
    if !is_hex40(expected_tip) {
        return Err(WorkspaceError::Failed(
            "rollback expected tip must be a SHA".to_string(),
        ));
    }
    let actual_tip = local_branch_tip(repo, branch)?
        .ok_or_else(|| WorkspaceError::Failed(format!("rollback branch is missing: {branch}")))?;
    if actual_tip != expected_tip {
        return Err(WorkspaceError::Conflict(
            "refuse rollback: branch tip moved".to_string(),
        ));
    }
    for worktree in registered_worktrees(repo)? {
        if worktree.branch.as_deref() == Some(branch) {
            return Err(WorkspaceError::Conflict(
                "refuse rollback: branch is checked out".to_string(),
            ));
        }
    }
    let refname = format!("refs/heads/{branch}");
    match run_git_ok(repo, &["update-ref", "-d", &refname, expected_tip]) {
        Ok(_) => Ok(()),
        Err(error) => match local_branch_tip(repo, branch)? {
            Some(actual_tip) if actual_tip != expected_tip => Err(WorkspaceError::Conflict(
                "refuse rollback: branch tip moved".to_string(),
            )),
            Some(_) => Err(error),
            None => Err(WorkspaceError::Failed(format!(
                "rollback branch disappeared: {branch}"
            ))),
        },
    }
}

#[cfg(test)]
mod tests;
