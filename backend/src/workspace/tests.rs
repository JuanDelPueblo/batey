use super::*;
use std::fs;
use std::io::Write;
use std::process::Command;
use std::time::Duration;

fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git run");
    assert!(
        st.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&st.stderr)
    );
}

fn init_repo() -> tempfile::TempDir {
    let td = tempfile::tempdir().unwrap();
    init_repo_at(td.path());
    td
}

fn init_repo_at(path: &Path) {
    fs::create_dir_all(path).unwrap();
    git(path, &["init", "-b", "main"]);
    git(path, &["config", "user.email", "t@t.t"]);
    git(path, &["config", "user.name", "t"]);
    fs::write(path.join("f.txt"), "a").unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-m", "init"]);
}

fn create_many_refs(repo: &Path, base: &str) {
    let mut child = Command::new("git")
        .args(["update-ref", "--stdin"])
        .current_dir(repo)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for index in 0..3000 {
        writeln!(stdin, "create refs/heads/output/{index} {base}").unwrap();
    }
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

#[test]
fn branches_resolve_without_network() {
    let td = init_repo();
    git(td.path(), &["branch", "feat"]);
    let branches = list_local_branches(td.path()).unwrap();
    assert!(branches.iter().any(|b| b.name == "main" && b.current));
    assert!(branches.iter().any(|b| b.name == "feat"));
    let sha = resolve_ref(td.path(), "feat").unwrap();
    assert!(is_hex40(&sha));
}

#[test]
fn direct_switch_clean_and_dirty_refusal() {
    let td = init_repo();
    git(td.path(), &["branch", "feat"]);
    fs::write(td.path().join("f.txt"), "dirty").unwrap();
    assert!(inspect(td.path()).unwrap().dirty);
    let err = prepare_direct(td.path(), "feat").unwrap_err();
    assert!(matches!(err, WorkspaceError::Conflict(_)));
    // Restore clean, switch works.
    git(td.path(), &["checkout", "--", "."]);
    prepare_direct(td.path(), "feat").unwrap();
    assert_eq!(inspect(td.path()).unwrap().branch.as_deref(), Some("feat"));
}

#[test]
fn direct_same_branch_preserves_dirty() {
    let td = init_repo();
    fs::write(td.path().join("f.txt"), "dirty").unwrap();
    prepare_direct(td.path(), "main").unwrap();
    assert_eq!(
        fs::read_to_string(td.path().join("f.txt")).unwrap(),
        "dirty"
    );
}

#[test]
fn managed_two_isolated_worktrees_and_nested_paths() {
    let td = init_repo();
    git(td.path(), &["branch", "feat"]);
    let base = resolve_ref(td.path(), "feat").unwrap();
    // Nested project path inside repo.
    let nested = td.path().join("sub/dir");
    fs::create_dir_all(&nested).unwrap();
    let info = inspect(&nested).unwrap();
    assert!(info.is_git);
    assert!(info.subdir.unwrap().to_str().unwrap().contains("sub"));
    let ws = tempfile::tempdir().unwrap();
    let a = provision_managed(td.path(), ws.path(), "chat1", &base).unwrap();
    let b = provision_managed(td.path(), ws.path(), "chat2", &base).unwrap();
    assert!(a.worktree.exists() && b.worktree.exists());
    fs::write(a.worktree.join("only-a.txt"), "x").unwrap();
    assert!(!b.worktree.join("only-a.txt").exists());
    // Primary checkout untouched: still on main.
    assert_eq!(inspect(td.path()).unwrap().branch.as_deref(), Some("main"));
}

#[test]
fn managed_recovery_and_removal() {
    let td = init_repo();
    let base = resolve_ref(td.path(), "main").unwrap();
    let ws = tempfile::tempdir().unwrap();
    let m = provision_managed(td.path(), ws.path(), "chat9", &base).unwrap();
    assert!(matches!(
        recover_managed(td.path(), ws.path(), "chat9").unwrap(),
        Recovered::Reused(_)
    ));
    // Simulate missing worktree: remove via git, keep branch, recreate.
    git(
        td.path(),
        &[
            "worktree",
            "remove",
            "--force",
            m.worktree.to_str().unwrap(),
        ],
    );
    assert!(matches!(
        recover_managed(td.path(), ws.path(), "chat9").unwrap(),
        Recovered::Recreated(_)
    ));
    // Dirty removal refused.
    fs::write(ws.path().join("chat9/f.txt"), "dirty").unwrap();
    // Need commit context: dirty check via status.
    let err = remove_managed(td.path(), ws.path(), "chat9").unwrap_err();
    assert!(matches!(err, WorkspaceError::Conflict(_)));
    git(&m.worktree, &["checkout", "--", "."]);
    remove_managed(td.path(), ws.path(), "chat9").unwrap();
    // Branch preserved.
    assert!(is_hex40(
        &resolve_ref(td.path(), "batey/chat/chat9").unwrap()
    ));
}

#[cfg(unix)]
#[test]
fn only_the_generated_nix_direnv_profile_is_disposable() {
    let td = init_repo();
    fs::write(td.path().join(".gitignore"), ".direnv/\n").unwrap();
    git(td.path(), &["add", ".gitignore"]);
    git(td.path(), &["commit", "-m", "ignore direnv cache"]);
    let base = resolve_ref(td.path(), "main").unwrap();
    let ws = tempfile::tempdir().unwrap();
    let managed = provision_managed(td.path(), ws.path(), "runtime", &base).unwrap();
    let cache = managed.worktree.join(".direnv");
    fs::create_dir(&cache).unwrap();
    std::os::unix::fs::symlink(
        "/nix/store/batey-test-profile",
        cache.join("flake-profile-1-link"),
    )
    .unwrap();
    std::os::unix::fs::symlink("flake-profile-1-link", cache.join("flake-profile")).unwrap();

    git(&managed.worktree, &["status", "--porcelain"]);
    assert!(!worktree_dirty(&managed.worktree).unwrap());

    fs::remove_file(cache.join("flake-profile")).unwrap();
    std::os::unix::fs::symlink("flake-profile-2-link", cache.join("flake-profile")).unwrap();
    assert!(worktree_dirty(&managed.worktree).unwrap());
    fs::remove_file(cache.join("flake-profile")).unwrap();
    std::os::unix::fs::symlink("flake-profile-1-link", cache.join("flake-profile")).unwrap();

    fs::remove_file(cache.join("flake-profile-1-link")).unwrap();
    let important = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(important.path(), cache.join("flake-profile-1-link")).unwrap();
    assert!(worktree_dirty(&managed.worktree).unwrap());

    fs::write(cache.join("agent-note.txt"), "keep\n").unwrap();
    assert!(worktree_dirty(&managed.worktree).unwrap());
}

#[test]
fn unrelated_ignored_runtime_like_content_is_not_disposable() {
    let td = init_repo();
    fs::write(td.path().join(".gitignore"), ".agent-cache/\n").unwrap();
    git(td.path(), &["add", ".gitignore"]);
    git(td.path(), &["commit", "-m", "ignore agent cache"]);
    let base = resolve_ref(td.path(), "main").unwrap();
    let ws = tempfile::tempdir().unwrap();
    let managed = provision_managed(td.path(), ws.path(), "ignored", &base).unwrap();
    fs::create_dir_all(managed.worktree.join(".agent-cache")).unwrap();
    fs::write(
        managed.worktree.join(".agent-cache/state.json"),
        "agent state\n",
    )
    .unwrap();

    assert!(worktree_dirty(&managed.worktree).unwrap());
}

#[test]
fn remove_missing_worktree_preserves_branch() {
    let td = init_repo();
    let base = resolve_ref(td.path(), "main").unwrap();
    let ws = tempfile::tempdir().unwrap();
    let managed = provision_managed(td.path(), ws.path(), "missing", &base).unwrap();
    git(
        td.path(),
        &[
            "worktree",
            "remove",
            "--force",
            managed.worktree.to_str().unwrap(),
        ],
    );

    remove_managed(td.path(), ws.path(), "missing").unwrap();
    assert_eq!(resolve_ref(td.path(), &managed.branch).unwrap(), base);
}

#[test]
fn direct_validation_reports_external_switch() {
    let td = init_repo();
    git(td.path(), &["branch", "feat"]);
    let root = inspect(td.path()).unwrap().root.unwrap();
    validate_direct(td.path(), &root, "main").unwrap();
    git(td.path(), &["checkout", "feat"]);
    let err = validate_direct(td.path(), &root, "main").unwrap_err();
    assert!(matches!(err, WorkspaceError::Conflict(_)));
}

#[test]
fn inspect_distinguishes_non_git_from_broken_metadata() {
    let non_git = tempfile::tempdir().unwrap();
    assert!(!inspect(non_git.path()).unwrap().is_git);

    fs::write(non_git.path().join(".git"), "gitdir: /does/not/exist\n").unwrap();
    assert!(matches!(
        inspect(non_git.path()),
        Err(WorkspaceError::Failed(_))
    ));
}

#[test]
fn direct_mode_rejects_tags_shas_and_remote_refs() {
    let td = init_repo();
    let base = resolve_ref(td.path(), "main").unwrap();
    git(td.path(), &["tag", "release"]);
    git(
        td.path(),
        &["update-ref", "refs/remotes/origin/main", &base],
    );

    for target in ["release", &base, "origin/main"] {
        let error = prepare_direct(td.path(), target).unwrap_err();
        assert!(matches!(error, WorkspaceError::Failed(_)));
        assert_eq!(inspect(td.path()).unwrap().branch.as_deref(), Some("main"));
    }
    prepare_direct(td.path(), "main").unwrap();
}

#[test]
fn provision_rejects_existing_managed_branch() {
    let td = init_repo();
    let base = resolve_ref(td.path(), "main").unwrap();
    let branch = "batey/chat/already";
    git(td.path(), &["branch", branch]);
    let ws = tempfile::tempdir().unwrap();

    let error = provision_managed(td.path(), ws.path(), "already", &base).unwrap_err();
    assert!(matches!(error, WorkspaceError::Conflict(_)));
    assert_eq!(resolve_ref(td.path(), branch).unwrap(), base);
    assert!(!ws.path().join("already").exists());
}

#[test]
fn rollback_deletes_only_at_expected_tip() {
    let td = init_repo();
    let base = resolve_ref(td.path(), "main").unwrap();
    let branch = "batey/chat/rollback";
    git(td.path(), &["branch", branch]);

    rollback_provision(td.path(), branch, &base, true).unwrap();
    assert!(resolve_ref(td.path(), branch).is_err());
}

#[test]
fn rollback_refuses_a_moved_branch() {
    let td = init_repo();
    let base = resolve_ref(td.path(), "main").unwrap();
    let branch = "batey/chat/moved";
    git(td.path(), &["branch", branch]);
    fs::write(td.path().join("new.txt"), "new").unwrap();
    git(td.path(), &["add", "new.txt"]);
    git(td.path(), &["commit", "-m", "move branch"]);
    let moved = resolve_ref(td.path(), "main").unwrap();
    git(
        td.path(),
        &["update-ref", &format!("refs/heads/{branch}"), &moved],
    );

    let error = rollback_provision(td.path(), branch, &base, true).unwrap_err();
    assert!(matches!(error, WorkspaceError::Conflict(_)));
    assert_eq!(resolve_ref(td.path(), branch).unwrap(), moved);
}

#[test]
fn rollback_refuses_a_branch_registered_in_a_missing_worktree() {
    let td = init_repo();
    let base = resolve_ref(td.path(), "main").unwrap();
    let ws = tempfile::tempdir().unwrap();
    let managed = provision_managed(td.path(), ws.path(), "checkedout", &base).unwrap();
    fs::remove_dir_all(&managed.worktree).unwrap();

    let error = rollback_provision(td.path(), &managed.branch, &base, true).unwrap_err();
    assert!(matches!(error, WorkspaceError::Conflict(_)));
    assert_eq!(resolve_ref(td.path(), &managed.branch).unwrap(), base);
}

#[test]
fn recovery_rejects_a_foreign_repository_at_the_expected_path() {
    let td = init_repo();
    let base = resolve_ref(td.path(), "main").unwrap();
    let ws = tempfile::tempdir().unwrap();
    let managed = provision_managed(td.path(), ws.path(), "foreign", &base).unwrap();
    fs::remove_dir_all(&managed.worktree).unwrap();
    init_repo_at(&managed.worktree);
    fs::write(managed.worktree.join("foreign.txt"), "keep").unwrap();

    let error = recover_managed(td.path(), ws.path(), "foreign").unwrap_err();
    assert!(matches!(error, WorkspaceError::Failed(_)));
    assert_eq!(
        fs::read_to_string(managed.worktree.join("foreign.txt")).unwrap(),
        "keep"
    );
}

#[test]
fn recovery_repairs_stale_worktree_metadata_without_losing_files() {
    let td = init_repo();
    let base = resolve_ref(td.path(), "main").unwrap();
    let ws = tempfile::tempdir().unwrap();
    let managed = provision_managed(td.path(), ws.path(), "stale", &base).unwrap();
    let marker = fs::read_to_string(managed.worktree.join(".git")).unwrap();
    let admin = PathBuf::from(marker.trim().strip_prefix("gitdir: ").unwrap());
    fs::write(admin.join("gitdir"), "/tmp/old-batey-worktree/.git\n").unwrap();
    fs::write(managed.worktree.join("sentinel.txt"), "preserve").unwrap();

    assert!(matches!(
        recover_managed(td.path(), ws.path(), "stale").unwrap(),
        Recovered::Reused(_)
    ));
    assert_eq!(
        fs::read_to_string(managed.worktree.join("sentinel.txt")).unwrap(),
        "preserve"
    );
    assert_eq!(
        fs::read_to_string(admin.join("gitdir")).unwrap().trim(),
        managed.worktree.join(".git").to_str().unwrap()
    );
}

#[test]
fn remove_rejects_a_foreign_repository_at_the_expected_path() {
    let td = init_repo();
    let base = resolve_ref(td.path(), "main").unwrap();
    let ws = tempfile::tempdir().unwrap();
    let managed = provision_managed(td.path(), ws.path(), "remove_foreign", &base).unwrap();
    fs::remove_file(managed.worktree.join(".git")).unwrap();
    git(&managed.worktree, &["init"]);
    fs::write(managed.worktree.join("foreign.txt"), "keep").unwrap();

    let error = remove_managed(td.path(), ws.path(), "remove_foreign").unwrap_err();
    assert!(matches!(error, WorkspaceError::Failed(_)));
    assert!(managed.worktree.join("foreign.txt").exists());
}

#[test]
fn run_git_drains_large_output_without_timeout() {
    let td = init_repo();
    let base = resolve_ref(td.path(), "main").unwrap();
    create_many_refs(td.path(), &base);
    let output = run_git(
        td.path(),
        &["for-each-ref", "--format=%(refname)", "refs/heads/output"],
        Duration::from_secs(5),
    )
    .unwrap();
    assert!(output.len() > 60_000);
}
