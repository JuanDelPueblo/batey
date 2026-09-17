use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use batey::{
    agents::{AgentDefinition, AgentRegistry},
    config::Config,
    events::EventLog,
    service::{HubService, ServiceError, WorkspaceSelection},
    session::SessionManager,
    store::{Store, WorkspaceMode},
    web::{router, AppState},
};
use std::{path::Path, process::Command, sync::Arc, time::Duration};
use tower::ServiceExt;

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn git_repo(root: &Path) -> std::path::PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Batey tests"]);
    std::fs::write(repo.join("README.md"), "base\n").unwrap();
    std::fs::create_dir_all(repo.join("nested")).unwrap();
    std::fs::write(repo.join("nested/project.txt"), "nested\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);
    git(&repo, &["branch", "feature"]);
    repo
}

/// A repo like `git_repo`, plus a bare remote it pushed `main` to, so
/// `sync_workspace` has a real upstream to fetch and fast-forward from.
fn git_repo_with_remote(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let repo = git_repo(root);
    let remote = root.join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "--bare", "-b", "main"]);
    git(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&repo, &["push", "-u", "origin", "main"]);
    (repo, remote)
}

/// Advances the bare `remote` by one commit through a second clone, so the
/// change reaches the remote without ever touching `repo`.
fn advance_remote(remote: &Path, file: &str, content: &str) {
    let scratch = tempfile::tempdir().unwrap();
    git(scratch.path(), &["clone", remote.to_str().unwrap(), "."]);
    git(scratch.path(), &["config", "user.email", "t@t.t"]);
    git(scratch.path(), &["config", "user.name", "t"]);
    std::fs::write(scratch.path().join(file), content).unwrap();
    git(scratch.path(), &["add", "."]);
    git(scratch.path(), &["commit", "-m", "advance"]);
    git(scratch.path(), &["push", "origin", "main"]);
}

fn git_repo_with_direnv_runtime_cache(root: &Path) -> std::path::PathBuf {
    let repo = git_repo(root);
    // This is the shape that nix-direnv produces for a flake profile. Keep
    // the fixture network- and Nix-independent so the Rust CI job can still
    // exercise the real direnv authorization/export lifecycle.
    let path = std::env::var("PATH").unwrap();
    std::fs::write(
        repo.join(".envrc"),
        format!(
            "export PATH=\"{path}\"\nmkdir -p .direnv\nln -sfn /nix/store/batey-test-profile .direnv/flake-profile-1-link\nln -sfn flake-profile-1-link .direnv/flake-profile\n"
        ),
    )
    .unwrap();
    std::fs::write(repo.join(".gitignore"), ".direnv/\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "add direnv runtime environment"]);
    repo
}

fn hub(root: &Path) -> (Arc<HubService>, Arc<Store>) {
    let (hub, store, _) = hub_with_manager(root);
    (hub, store)
}

fn hub_with_manager(root: &Path) -> (Arc<HubService>, Arc<Store>, Arc<SessionManager>) {
    let store = Arc::new(Store::open(&root.join("hub.db")).unwrap());
    let events = Arc::new(EventLog::persistent(store.clone()).unwrap());
    let history = root.join("history");
    std::fs::create_dir_all(&history).unwrap();
    let agent = AgentDefinition::codex_default()
        .with_command("python3".into())
        .with_args(vec![
            format!("{}/tests/fake_acp.py", env!("CARGO_MANIFEST_DIR")),
            history.display().to_string(),
            "load".into(),
        ]);
    let agents = Arc::new(AgentRegistry::new([agent]));
    let sessions = SessionManager::with_store(agents.clone(), events, Some(store.clone()));
    let mut config = Config {
        agents: agents.clone(),
        ..Default::default()
    };
    config.web.project_roots = vec![root.display().to_string()];
    (
        HubService::new(store.clone(), sessions.clone(), agents, &config),
        store,
        sessions,
    )
}

fn selection(mode: WorkspaceMode, branch: Option<&str>) -> Option<WorkspaceSelection> {
    Some(WorkspaceSelection {
        mode,
        branch: branch.map(str::to_string),
    })
}

#[tokio::test]
async fn workspace_options_enumerate_sorted_local_branches_and_non_git() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    git(&repo, &["branch", "zzz"]);
    git(&repo, &["branch", "aaa"]);
    let (hub, _store) = hub(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();

    let options = hub.workspace_options(&project.id).await.unwrap();
    assert!(options.is_git);
    assert_eq!(options.current_branch.as_deref(), Some("main"));
    assert!(!options.dirty);
    assert_eq!(
        options
            .branches
            .iter()
            .map(|branch| branch.name.as_str())
            .collect::<Vec<_>>(),
        vec!["aaa", "feature", "main", "zzz"]
    );
    assert!(options
        .branches
        .iter()
        .any(|branch| branch.name == "main" && branch.current));

    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let plain_project = hub
        .create_project("plain".into(), plain.display().to_string())
        .unwrap();
    let plain_options = hub.workspace_options(&plain_project.id).await.unwrap();
    assert!(!plain_options.is_git);
    assert!(plain_options.branches.is_empty());
}

#[tokio::test]
async fn sync_workspace_fast_forwards_a_clean_behind_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, remote) = git_repo_with_remote(tmp.path());
    advance_remote(&remote, "new.txt", "new");
    let (hub, _store) = hub(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let before = git(&repo, &["rev-parse", "HEAD"]);

    let result = hub.sync_workspace(&project.id).await.unwrap();

    assert!(result.updated);
    assert_eq!(result.branch, "main");
    assert_ne!(result.head_sha, before);
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), result.head_sha);
    assert!(repo.join("new.txt").exists());
}

#[tokio::test]
async fn sync_workspace_reports_already_up_to_date() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _remote) = git_repo_with_remote(tmp.path());
    let (hub, _store) = hub(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let before = git(&repo, &["rev-parse", "HEAD"]);

    let result = hub.sync_workspace(&project.id).await.unwrap();

    assert!(!result.updated);
    assert_eq!(result.head_sha, before);
}

#[tokio::test]
async fn sync_workspace_refuses_a_dirty_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, remote) = git_repo_with_remote(tmp.path());
    advance_remote(&remote, "new.txt", "new");
    std::fs::write(repo.join("README.md"), "dirty\n").unwrap();
    let (hub, _store) = hub(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let before = git(&repo, &["rev-parse", "HEAD"]);

    let error = hub.sync_workspace(&project.id).await.unwrap_err();

    assert!(matches!(error, ServiceError::Conflict(_)));
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), before);
    assert_eq!(
        std::fs::read_to_string(repo.join("README.md")).unwrap(),
        "dirty\n"
    );
}

#[tokio::test]
async fn sync_workspace_refuses_diverged_history() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, remote) = git_repo_with_remote(tmp.path());
    advance_remote(&remote, "remote-only.txt", "remote");
    std::fs::write(repo.join("local-only.txt"), "local").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "diverge"]);
    let (hub, _store) = hub(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let before = git(&repo, &["rev-parse", "HEAD"]);

    let error = hub.sync_workspace(&project.id).await.unwrap_err();

    assert!(matches!(error, ServiceError::Conflict(_)));
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), before);
}

#[tokio::test]
async fn sync_workspace_refuses_without_an_upstream() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let (hub, _store) = hub(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();

    let error = hub.sync_workspace(&project.id).await.unwrap_err();

    assert!(matches!(error, ServiceError::Conflict(_)));
}

#[tokio::test]
async fn sync_workspace_reports_a_sanitized_fetch_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _remote) = git_repo_with_remote(tmp.path());
    git(
        &repo,
        &[
            "remote",
            "set-url",
            "origin",
            "https://ghp_SECRET@example.invalid/does/not/exist.git",
        ],
    );
    let (hub, _store) = hub(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();

    let error = hub.sync_workspace(&project.id).await.unwrap_err();

    assert!(matches!(error, ServiceError::Invalid(_)));
    assert!(!error.to_string().contains("ghp_SECRET"));
}

#[tokio::test]
async fn sync_workspace_rejects_a_non_git_project() {
    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let (hub, _store) = hub(tmp.path());
    let project = hub
        .create_project("plain".into(), plain.display().to_string())
        .unwrap();

    let error = hub.sync_workspace(&project.id).await.unwrap_err();

    assert!(matches!(error, ServiceError::Invalid(_)));
}

#[tokio::test]
async fn managed_chats_are_isolated_and_can_start_from_selected_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let base = git(&repo, &["rev-parse", "feature"]);
    let (hub, store) = hub(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();

    let first = hub
        .create_chat_with_workspace(
            &project.id,
            "codex",
            None,
            selection(WorkspaceMode::ManagedWorktree, Some("feature")),
        )
        .await
        .unwrap();
    let second = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let first_ws = store.workspace(&first.chat.id).unwrap().unwrap();
    let second_ws = store.workspace(&second.chat.id).unwrap().unwrap();
    assert_eq!(
        first.workspace.as_ref().unwrap().mode,
        WorkspaceMode::ManagedWorktree
    );
    assert_eq!(first.workspace.as_ref().unwrap().branch, first_ws.branch);
    assert_eq!(
        first.workspace.as_ref().unwrap().base_commit,
        first_ws.base_commit
    );
    let serialized = serde_json::to_value(&first).unwrap();
    assert!(serialized["workspace"].get("repository_root").is_none());
    assert!(serialized["workspace"].get("workspace_path").is_none());
    assert!(serialized["workspace"].get("project_subdir").is_none());
    assert_eq!(first_ws.mode, WorkspaceMode::ManagedWorktree);
    assert_eq!(first_ws.base_commit.as_deref(), Some(base.as_str()));
    assert_ne!(first_ws.workspace_path, second_ws.workspace_path);
    assert_ne!(first_ws.branch, second_ws.branch);
    assert!(Path::new(&first_ws.workspace_path).is_dir());
    assert!(Path::new(&second_ws.workspace_path).is_dir());
    assert_eq!(
        git(
            Path::new(&first_ws.workspace_path),
            &["branch", "--show-current"]
        ),
        first_ws.branch.unwrap()
    );
}

#[tokio::test]
async fn explicit_managed_selection_ignores_dirty_primary_checkout_but_legacy_does_not() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    std::fs::write(repo.join("dirty.txt"), "keep in primary\n").unwrap();
    let (hub, store) = hub(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();

    let managed = hub
        .create_chat_with_workspace(
            &project.id,
            "codex",
            None,
            selection(WorkspaceMode::ManagedWorktree, Some("main")),
        )
        .await
        .unwrap();
    let ws = store.workspace(&managed.chat.id).unwrap().unwrap();
    assert!(!Path::new(&ws.workspace_path).join("dirty.txt").exists());

    let error = hub
        .create_chat(&project.id, "codex", None)
        .await
        .unwrap_err();
    assert!(matches!(error, ServiceError::Conflict(_)));
}

#[tokio::test]
async fn direct_current_branch_preserves_dirty_files_and_switching_refuses_them() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    std::fs::write(repo.join("README.md"), "dirty\n").unwrap();
    let (hub, store) = hub(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();

    let current = hub
        .create_chat_with_workspace(
            &project.id,
            "codex",
            None,
            selection(WorkspaceMode::ProjectCheckout, None),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.join("README.md")).unwrap(),
        "dirty\n"
    );
    assert_eq!(
        store.workspace(&current.chat.id).unwrap().unwrap().mode,
        WorkspaceMode::ProjectCheckout
    );
    assert_eq!(
        current.workspace.as_ref().unwrap().mode,
        WorkspaceMode::ProjectCheckout
    );
    assert_eq!(
        current.workspace.as_ref().unwrap().branch.as_deref(),
        Some("main")
    );

    let error = hub
        .create_chat_with_workspace(
            &project.id,
            "codex",
            None,
            selection(WorkspaceMode::ProjectCheckout, Some("feature")),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ServiceError::Conflict(_)));
    assert_eq!(git(&repo, &["branch", "--show-current"]), "main");
}

#[tokio::test]
async fn checkout_reservation_blocks_branch_switch_and_new_direct_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let (hub, store, sessions) = hub_with_manager(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let current = hub
        .create_chat_with_workspace(
            &project.id,
            "codex",
            None,
            selection(WorkspaceMode::ProjectCheckout, None),
        )
        .await
        .unwrap();

    let reservation = sessions.try_acquire_checkout_guard(&repo).unwrap();
    let session = sessions.get_by_id(&current.chat.id).await.unwrap();
    let turn_error = session.start_turn("hello".into(), None).await.unwrap_err();
    assert_eq!(
        turn_error.to_string(),
        "Another chat is already working in this project checkout"
    );

    let switch_error = hub
        .create_chat_with_workspace(
            &project.id,
            "codex",
            None,
            selection(WorkspaceMode::ProjectCheckout, Some("feature")),
        )
        .await
        .unwrap_err();
    assert!(matches!(switch_error, ServiceError::Conflict(_)));
    assert_eq!(git(&repo, &["branch", "--show-current"]), "main");
    assert_eq!(store.chats().unwrap().len(), 1);
    drop(reservation);
    sessions.shutdown_all().await;
}

#[tokio::test]
async fn active_direct_turn_reserves_checkout_before_same_branch_creation() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let (hub, store, sessions) = hub_with_manager(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let current = hub
        .create_chat_with_workspace(
            &project.id,
            "codex",
            None,
            selection(WorkspaceMode::ProjectCheckout, None),
        )
        .await
        .unwrap();
    let current_branch = git(&repo, &["branch", "--show-current"]);
    let current_session = sessions.get_by_id(&current.chat.id).await.unwrap();
    current_session
        .start_turn("wait".into(), Some(std::time::Duration::from_secs(5)))
        .await
        .unwrap();
    for _ in 0..100 {
        if current_session.turn_state().await == batey::state::TurnState::Prompting {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    git(&repo, &["checkout", "feature"]);

    let error = hub
        .create_chat_with_workspace(
            &project.id,
            "codex",
            None,
            selection(WorkspaceMode::ProjectCheckout, Some("feature")),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ServiceError::Conflict(_)));
    assert_eq!(current_branch, "main");
    assert_eq!(git(&repo, &["branch", "--show-current"]), "feature");
    assert_eq!(store.chats().unwrap().len(), 1);

    current_session.cancel().await.unwrap();
    sessions.shutdown_all().await;
}

#[tokio::test]
async fn nested_project_uses_the_effective_subdirectory_in_each_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let nested = repo.join("nested");
    let (hub, store) = hub(tmp.path());
    let project = hub
        .create_project("nested".into(), nested.display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let ws = store.workspace(&chat.chat.id).unwrap().unwrap();
    assert_eq!(ws.project_subdir, "nested");
    assert!(Path::new(&ws.workspace_path)
        .join("nested/project.txt")
        .is_file());
}

#[tokio::test]
async fn http_workspace_options_route_and_chat_workspace_payload_work() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let store = Arc::new(Store::open(&tmp.path().join("hub.db")).unwrap());
    let events = Arc::new(EventLog::persistent(store.clone()).unwrap());
    let agents = Arc::new(AgentRegistry::new([AgentDefinition::codex_default()]));
    let sessions = SessionManager::with_store(agents.clone(), events, Some(store.clone()));
    let mut config = Config {
        agents,
        ..Default::default()
    };
    config.web.project_roots = vec![tmp.path().display().to_string()];
    let project = store
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let app = router(AppState::new(sessions, Arc::new(config), 0));

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/projects/{}/workspace-options", project.id))
                .header("host", "127.0.0.1:0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let options: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(options["is_git"], true);
    assert_eq!(options["current_branch"], "main");
    assert!(options.get("branch").is_none());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/projects/{}/chats", project.id))
                .header("host", "127.0.0.1:0")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "agent": "codex",
                        "workspace": {"mode": "managed_worktree", "branch": "feature"}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(store.chats().unwrap().len(), 1);
    assert_eq!(store.list_workspaces().unwrap().len(), 1);
}

#[tokio::test]
async fn http_workspace_sync_route_updates_and_reports_dirty_refusal() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, remote) = git_repo_with_remote(tmp.path());
    advance_remote(&remote, "new.txt", "new");
    let store = Arc::new(Store::open(&tmp.path().join("hub.db")).unwrap());
    let events = Arc::new(EventLog::persistent(store.clone()).unwrap());
    let agents = Arc::new(AgentRegistry::new([AgentDefinition::codex_default()]));
    let sessions = SessionManager::with_store(agents.clone(), events, Some(store.clone()));
    let mut config = Config {
        agents,
        ..Default::default()
    };
    config.web.project_roots = vec![tmp.path().display().to_string()];
    let project = store
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let app = router(AppState::new(sessions, Arc::new(config), 0));

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/projects/{}/workspace-sync", project.id))
                .header("host", "127.0.0.1:0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(result["updated"], true);
    assert_eq!(result["branch"], "main");
    assert!(repo.join("new.txt").exists());

    // A second sync with nothing new upstream is a no-op success.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/projects/{}/workspace-sync", project.id))
                .header("host", "127.0.0.1:0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(result["updated"], false);

    // A dirty checkout is refused, and reported as a client error.
    advance_remote(&remote, "another.txt", "another");
    std::fs::write(repo.join("README.md"), "dirty\n").unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/projects/{}/workspace-sync", project.id))
                .header("host", "127.0.0.1:0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    assert_eq!(
        std::fs::read_to_string(repo.join("README.md")).unwrap(),
        "dirty\n"
    );
}

#[tokio::test]
async fn deleting_clean_managed_chat_removes_worktree_but_preserves_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let (hub, store) = hub(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let workspace = store.workspace(&chat.chat.id).unwrap().unwrap();
    let branch = workspace.branch.clone().unwrap();

    hub.delete_chat(&chat.chat.id).await.unwrap();

    assert!(store.chat(&chat.chat.id).is_err());
    assert!(store.workspace(&chat.chat.id).unwrap().is_none());
    assert!(!Path::new(&workspace.workspace_path).exists());
    assert_eq!(git(&repo, &["rev-parse", "--verify", &branch]).len(), 40);
}

#[tokio::test]
async fn deleting_initialized_managed_chat_removes_only_the_runtime_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo_with_direnv_runtime_cache(tmp.path());
    let (hub, store, sessions) = hub_with_manager(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let workspace = store.workspace(&chat.chat.id).unwrap().unwrap();
    let branch = workspace.branch.clone().unwrap();
    let worktree = Path::new(&workspace.workspace_path);
    let session = sessions.get_by_id(&chat.chat.id).await.unwrap();

    hub.authorize_chat_environment(&chat.chat.id, false)
        .await
        .unwrap();
    session.ensure_running().await.unwrap();

    assert_eq!(
        git(
            worktree,
            &[
                "status",
                "--porcelain",
                "--untracked-files=all",
                "--ignored=matching",
            ],
        ),
        "!! .direnv/"
    );
    assert!(worktree.join(".direnv/flake-profile").is_symlink());
    assert!(worktree.join(".direnv/flake-profile-1-link").is_symlink());

    hub.delete_chat(&chat.chat.id).await.unwrap();

    assert!(!worktree.exists());
    assert!(store.chat(&chat.chat.id).is_err());
    assert_eq!(git(&repo, &["rev-parse", "--verify", &branch]).len(), 40);
}

#[tokio::test]
async fn deleting_dirty_managed_chat_preserves_everything() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let (hub, store) = hub(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let workspace = store.workspace(&chat.chat.id).unwrap().unwrap();
    let branch = workspace.branch.clone().unwrap();
    let worktree = std::path::PathBuf::from(&workspace.workspace_path);
    std::fs::write(worktree.join("README.md"), "changed\n").unwrap();

    let error = hub.delete_chat(&chat.chat.id).await.unwrap_err();

    assert!(matches!(error, ServiceError::Conflict(_)));
    assert!(store.chat(&chat.chat.id).is_ok());
    assert_eq!(store.workspace(&chat.chat.id).unwrap(), Some(workspace));
    assert_eq!(
        std::fs::read_to_string(worktree.join("README.md")).unwrap(),
        "changed\n"
    );
    assert_eq!(git(&repo, &["rev-parse", "--verify", &branch]).len(), 40);
}

#[tokio::test]
async fn deleting_untracked_managed_chat_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let (hub, store) = hub(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let workspace = store.workspace(&chat.chat.id).unwrap().unwrap();
    let worktree = Path::new(&workspace.workspace_path);
    std::fs::write(worktree.join("untracked.txt"), "keep\n").unwrap();

    let error = hub.delete_chat(&chat.chat.id).await.unwrap_err();

    assert!(matches!(error, ServiceError::Conflict(_)));
    assert!(store.chat(&chat.chat.id).is_ok());
    assert!(worktree.join("untracked.txt").is_file());
    assert!(worktree.is_dir());
}

#[tokio::test]
async fn deleting_managed_chat_with_an_unrelated_ignored_artifact_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    std::fs::write(repo.join(".gitignore"), ".agent-cache/\n").unwrap();
    git(&repo, &["add", ".gitignore"]);
    git(&repo, &["commit", "-m", "ignore agent cache"]);
    let (hub, store) = hub(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let workspace = store.workspace(&chat.chat.id).unwrap().unwrap();
    let worktree = Path::new(&workspace.workspace_path);
    std::fs::create_dir_all(worktree.join(".agent-cache")).unwrap();
    std::fs::write(worktree.join(".agent-cache/state.json"), "agent state\n").unwrap();

    let error = hub.delete_chat(&chat.chat.id).await.unwrap_err();

    assert!(matches!(error, ServiceError::Conflict(_)));
    assert!(store.chat(&chat.chat.id).is_ok());
    assert!(worktree.join(".agent-cache/state.json").is_file());
}

#[tokio::test]
async fn deleting_missing_managed_worktree_prunes_registration_without_recreation() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let (hub, store) = hub(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let workspace = store.workspace(&chat.chat.id).unwrap().unwrap();
    let branch = workspace.branch.clone().unwrap();
    std::fs::remove_dir_all(&workspace.workspace_path).unwrap();

    hub.delete_chat(&chat.chat.id).await.unwrap();

    assert!(!Path::new(&workspace.workspace_path).exists());
    assert!(store.chat(&chat.chat.id).is_err());
    assert_eq!(git(&repo, &["rev-parse", "--verify", &branch]).len(), 40);
    assert!(!git(&repo, &["worktree", "list", "--porcelain"]).contains(&workspace.workspace_path));
}

#[tokio::test]
async fn managed_cleanup_failure_after_worktree_removal_preserves_metadata_and_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let (hub, store) = hub(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let workspace = store.workspace(&chat.chat.id).unwrap().unwrap();
    let branch = workspace.branch.clone().unwrap();
    let raw = rusqlite::Connection::open(tmp.path().join("hub.db")).unwrap();
    raw.execute_batch("DROP TABLE events").unwrap();

    let error = hub.delete_chat(&chat.chat.id).await.unwrap_err();

    assert!(matches!(error, ServiceError::Invalid(_)));
    assert!(store.chat(&chat.chat.id).is_ok());
    assert_eq!(
        store.workspace(&chat.chat.id).unwrap(),
        Some(workspace.clone())
    );
    assert!(!Path::new(&workspace.workspace_path).exists());
    assert_eq!(git(&repo, &["rev-parse", "--verify", &branch]).len(), 40);
}

#[tokio::test]
async fn deleting_corrupt_managed_workspace_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let (hub, store) = hub(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let workspace = store.workspace(&chat.chat.id).unwrap().unwrap();
    let branch = workspace.branch.clone().unwrap();
    let foreign_root = tmp.path().join("foreign");
    std::fs::create_dir_all(&foreign_root).unwrap();
    let raw = rusqlite::Connection::open(tmp.path().join("hub.db")).unwrap();
    raw.execute(
        "UPDATE chat_workspaces SET repository_root=?1 WHERE chat_id=?2",
        rusqlite::params![foreign_root.display().to_string(), chat.chat.id.as_str()],
    )
    .unwrap();

    let error = hub.delete_chat(&chat.chat.id).await.unwrap_err();

    assert!(matches!(error, ServiceError::Invalid(_)));
    assert!(store.chat(&chat.chat.id).is_ok());
    assert!(store.workspace(&chat.chat.id).is_ok());
    assert!(Path::new(&workspace.workspace_path).is_dir());
    assert_eq!(git(&repo, &["rev-parse", "--verify", &branch]).len(), 40);
}

#[tokio::test]
async fn deleting_foreign_worktree_at_managed_path_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let (hub, store) = hub(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let workspace = store.workspace(&chat.chat.id).unwrap().unwrap();
    let branch = workspace.branch.clone().unwrap();
    let worktree = Path::new(&workspace.workspace_path).to_path_buf();
    std::fs::remove_dir_all(&worktree).unwrap();
    std::fs::create_dir_all(&worktree).unwrap();
    git(&worktree, &["init", "-b", "foreign"]);
    std::fs::write(worktree.join("keep.txt"), "keep\n").unwrap();

    let error = hub.delete_chat(&chat.chat.id).await.unwrap_err();

    assert!(matches!(error, ServiceError::Invalid(_)));
    assert!(store.chat(&chat.chat.id).is_ok());
    assert!(worktree.join("keep.txt").is_file());
    assert_eq!(git(&repo, &["rev-parse", "--verify", &branch]).len(), 40);
}

#[tokio::test]
async fn deleting_direct_chat_leaves_project_checkout_and_branch_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let (hub, store) = hub(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let chat = hub
        .create_chat_with_workspace(
            &project.id,
            "codex",
            None,
            selection(WorkspaceMode::ProjectCheckout, None),
        )
        .await
        .unwrap();
    let before = std::fs::read(repo.join("README.md")).unwrap();
    let branch = git(&repo, &["branch", "--show-current"]);

    hub.delete_chat(&chat.chat.id).await.unwrap();

    assert_eq!(std::fs::read(repo.join("README.md")).unwrap(), before);
    assert_eq!(git(&repo, &["branch", "--show-current"]), branch);
    assert!(store.chat(&chat.chat.id).is_err());
    assert!(store.workspace(&chat.chat.id).unwrap().is_none());
}

#[tokio::test]
async fn deleting_legacy_chat_leaves_git_checkout_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let (hub, store) = hub(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let chat = store
        .create_chat(project.id.clone(), "codex".into(), None)
        .unwrap();
    let before = std::fs::read(repo.join("README.md")).unwrap();
    let branch = git(&repo, &["branch", "--show-current"]);

    hub.delete_chat(&chat.id).await.unwrap();

    assert_eq!(std::fs::read(repo.join("README.md")).unwrap(), before);
    assert_eq!(git(&repo, &["branch", "--show-current"]), branch);
    assert!(store.chat(&chat.id).is_err());
}

#[tokio::test]
async fn active_turn_prevents_chat_deletion() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let (hub, store, sessions) = hub_with_manager(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let workspace = store.workspace(&chat.chat.id).unwrap().unwrap();
    let session = sessions.get_by_id(&chat.chat.id).await.unwrap();
    session
        .start_turn("wait".into(), Some(Duration::from_secs(5)))
        .await
        .unwrap();
    for _ in 0..100 {
        if session.turn_state().await == batey::state::TurnState::Prompting {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let error = hub.delete_chat(&chat.chat.id).await.unwrap_err();

    assert!(matches!(error, ServiceError::Invalid(message) if message.contains("active turn")));
    assert!(store.chat(&chat.chat.id).is_ok());
    assert!(Path::new(&workspace.workspace_path).is_dir());
    session.cancel().await.unwrap();
    sessions.shutdown_all().await;
}

// ---------------------------------------------------------- T127: deletion independent of configured agents

/// Rebuilds a Hub over the same durable store, but with a different agent
/// catalog. Used to simulate "restart with the chat's agent no longer
/// configured" without touching any other Hub state.
fn hub_with_agents(
    root: &Path,
    store: Arc<Store>,
    definitions: Vec<AgentDefinition>,
) -> (Arc<HubService>, Arc<SessionManager>) {
    let events = Arc::new(EventLog::persistent(store.clone()).unwrap());
    let agents = Arc::new(AgentRegistry::new(definitions));
    let sessions = SessionManager::with_store(agents.clone(), events, Some(store.clone()));
    let mut config = Config {
        agents: agents.clone(),
        ..Default::default()
    };
    config.web.project_roots = vec![root.display().to_string()];
    (
        HubService::new(store, sessions.clone(), agents, &config),
        sessions,
    )
}

#[tokio::test]
async fn missing_agent_chat_stays_readable_and_deletes_with_normal_managed_cleanup() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let (hub, store, sessions) = hub_with_manager(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let workspace = store.workspace(&chat.chat.id).unwrap().unwrap();
    let branch = workspace.branch.clone().unwrap();
    assert!(Path::new(&workspace.workspace_path).is_dir());
    sessions.shutdown_all().await;

    // Restart with "codex" no longer in the agent catalog: the durable chat
    // survives, but no `AcpSession` can be constructed for it.
    let (restarted, restarted_sessions) = hub_with_agents(tmp.path(), store.clone(), Vec::new());

    let view = restarted.get_chat(&chat.chat.id).await.unwrap();
    assert_eq!(view.process_state, "STOPPED");
    assert!(restarted
        .chat_history(&chat.chat.id, None, None, 100)
        .is_ok());

    restarted.delete_chat(&chat.chat.id).await.unwrap();

    assert!(store.chat(&chat.chat.id).is_err());
    assert!(store.workspace(&chat.chat.id).unwrap().is_none());
    assert!(!Path::new(&workspace.workspace_path).exists());
    assert_eq!(git(&repo, &["rev-parse", "--verify", &branch]).len(), 40);
    restarted_sessions.shutdown_all().await;
}

#[tokio::test]
async fn missing_agent_dirty_managed_chat_deletion_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let (hub, store, sessions) = hub_with_manager(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let workspace = store.workspace(&chat.chat.id).unwrap().unwrap();
    std::fs::write(
        Path::new(&workspace.workspace_path).join("README.md"),
        "changed\n",
    )
    .unwrap();
    sessions.shutdown_all().await;

    let (restarted, restarted_sessions) = hub_with_agents(tmp.path(), store.clone(), Vec::new());

    let error = restarted.delete_chat(&chat.chat.id).await.unwrap_err();

    assert!(matches!(error, ServiceError::Conflict(_)));
    assert!(store.chat(&chat.chat.id).is_ok());
    assert!(Path::new(&workspace.workspace_path).is_dir());
    restarted_sessions.shutdown_all().await;
}

#[tokio::test]
async fn deleting_project_cascades_many_managed_chats_and_their_worktrees() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let (hub, store) = hub(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let mut worktrees = Vec::new();
    for _ in 0..3 {
        let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
        let workspace = store.workspace(&chat.chat.id).unwrap().unwrap();
        worktrees.push((chat.chat.id, workspace));
    }

    hub.delete_project(&project.id).await.unwrap();

    assert!(hub.get_project(&project.id).is_err());
    for (chat_id, workspace) in worktrees {
        assert!(store.chat(&chat_id).is_err());
        assert!(!Path::new(&workspace.workspace_path).exists());
    }
    assert!(repo.is_dir(), "project files must never be deleted");
}

#[tokio::test]
async fn project_deletion_preflight_allows_the_same_runtime_cache_as_chat_deletion() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo_with_direnv_runtime_cache(tmp.path());
    let (hub, store, sessions) = hub_with_manager(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let workspace = store.workspace(&chat.chat.id).unwrap().unwrap();
    let worktree = Path::new(&workspace.workspace_path);
    let session = sessions.get_by_id(&chat.chat.id).await.unwrap();
    hub.authorize_chat_environment(&chat.chat.id, false)
        .await
        .unwrap();
    session.ensure_running().await.unwrap();
    assert_eq!(
        git(worktree, &["status", "--porcelain", "--ignored=matching"]),
        "!! .direnv/"
    );

    hub.delete_project(&project.id).await.unwrap();

    assert!(hub.get_project(&project.id).is_err());
    assert!(store.chat(&chat.chat.id).is_err());
    assert!(!worktree.exists());
}

#[tokio::test]
async fn deleting_project_cascades_a_chat_whose_agent_is_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let (hub, store, sessions) = hub_with_manager(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let live_chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let orphaned_chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let orphaned_workspace = store.workspace(&orphaned_chat.chat.id).unwrap().unwrap();
    sessions.shutdown_all().await;

    let (restarted, restarted_sessions) = hub_with_agents(
        tmp.path(),
        store.clone(),
        vec![AgentDefinition::codex_default()
            .with_command("python3".into())
            .with_args(vec![
                format!("{}/tests/fake_acp.py", env!("CARGO_MANIFEST_DIR")),
                tmp.path().join("history").display().to_string(),
                "load".into(),
            ])],
    );
    // Simulate the orphaned chat's agent having been removed by pointing it
    // at an agent id the restarted catalog does not define.
    {
        let raw = rusqlite::Connection::open(tmp.path().join("hub.db")).unwrap();
        raw.execute(
            "UPDATE chats SET data = json_set(data, '$.agent', 'gemini') WHERE id = ?1",
            rusqlite::params![orphaned_chat.chat.id.as_str()],
        )
        .unwrap();
    }

    restarted.delete_project(&project.id).await.unwrap();

    assert!(restarted.get_project(&project.id).is_err());
    assert!(store.chat(&live_chat.chat.id).is_err());
    assert!(store.chat(&orphaned_chat.chat.id).is_err());
    assert!(!Path::new(&orphaned_workspace.workspace_path).exists());
    restarted_sessions.shutdown_all().await;
}

#[tokio::test]
async fn dirty_managed_worktree_blocks_project_deletion_before_any_chat_is_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let (hub, store) = hub(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let clean_chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let dirty_chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let dirty_workspace = store.workspace(&dirty_chat.chat.id).unwrap().unwrap();
    std::fs::write(
        Path::new(&dirty_workspace.workspace_path).join("README.md"),
        "changed\n",
    )
    .unwrap();

    let error = hub.delete_project(&project.id).await.unwrap_err();

    assert!(matches!(error, ServiceError::Conflict(_)));
    // Neither chat was deleted: the preflight found the conflict before any
    // destructive cleanup began.
    assert!(store.chat(&clean_chat.chat.id).is_ok());
    assert!(store.chat(&dirty_chat.chat.id).is_ok());
    assert!(hub.get_project(&project.id).is_ok());
}

#[tokio::test]
async fn active_turn_blocks_project_deletion() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    let (hub, store, sessions) = hub_with_manager(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let session = sessions.get_by_id(&chat.chat.id).await.unwrap();
    session
        .start_turn("wait".into(), Some(Duration::from_secs(5)))
        .await
        .unwrap();
    for _ in 0..100 {
        if session.turn_state().await == batey::state::TurnState::Prompting {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let error = hub.delete_project(&project.id).await.unwrap_err();

    assert!(matches!(error, ServiceError::Conflict(_)));
    assert!(store.chat(&chat.chat.id).is_ok());
    session.cancel().await.unwrap();
    sessions.shutdown_all().await;
}

// ---------------------------------------------------------- T125: project-level direnv authorization

/// Writes and commits a file inside `repo`, creating parent directories as
/// needed. `rel_path` is relative to `repo`.
fn write_and_commit(repo: &Path, rel_path: &str, content: &str) {
    let path = repo.join(rel_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, content).unwrap();
    git(repo, &["add", rel_path]);
    git(repo, &["commit", "-m", &format!("add {rel_path}")]);
}

/// Writes and commits a real `.envrc` (or a nested one) for these tests.
/// Real `direnv` recomputes `PATH` from scratch when it loads a boundary
/// unrelated to this test binary's own dev-shell `.envrc`, which can drop
/// this process's own `python3`/`cargo` from `PATH`. Pinning `PATH` to this
/// process's current value keeps the fake ACP agent (`python3
/// tests/fake_acp.py`) spawnable after the workspace environment resolves.
fn write_and_commit_envrc(repo: &Path, rel_path: &str, extra: &str) {
    let content = format!(
        "export PATH=\"{}\"\n{extra}",
        std::env::var("PATH").unwrap()
    );
    write_and_commit(repo, rel_path, &content);
}

#[tokio::test]
async fn one_workspace_approval_stays_workspace_specific() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    write_and_commit_envrc(&repo, ".envrc", "export ENVRC_SHARED=1\n");
    let (hub, store, sessions) = hub_with_manager(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();

    let chat_a = hub
        .create_chat_with_workspace(
            &project.id,
            "codex",
            None,
            selection(WorkspaceMode::ManagedWorktree, Some("main")),
        )
        .await
        .unwrap();
    hub.authorize_chat_environment(&chat_a.chat.id, false)
        .await
        .unwrap();
    assert!(
        store.project_envrc_grant(&project.id).unwrap().is_none(),
        "a plain workspace allow must never remember the project"
    );

    let chat_b = hub
        .create_chat_with_workspace(
            &project.id,
            "codex",
            None,
            selection(WorkspaceMode::ManagedWorktree, Some("main")),
        )
        .await
        .unwrap();
    let err = hub.resume_chat(&chat_b.chat.id).await.unwrap_err();
    assert!(matches!(err, ServiceError::EnvrcBlocked { .. }));

    sessions.shutdown_all().await;
}

#[tokio::test]
async fn remembered_project_authorization_applies_to_a_second_matching_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    write_and_commit_envrc(&repo, ".envrc", "export ENVRC_SHARED=1\n");
    let (hub, store, sessions) = hub_with_manager(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();

    let chat_a = hub
        .create_chat_with_workspace(
            &project.id,
            "codex",
            None,
            selection(WorkspaceMode::ManagedWorktree, Some("main")),
        )
        .await
        .unwrap();
    hub.authorize_chat_environment(&chat_a.chat.id, true)
        .await
        .unwrap();
    let grant = store
        .project_envrc_grant(&project.id)
        .unwrap()
        .expect("remember must record a grant");
    assert_eq!(grant.relative_path, ".envrc");

    let chat_b = hub
        .create_chat_with_workspace(
            &project.id,
            "codex",
            None,
            selection(WorkspaceMode::ManagedWorktree, Some("main")),
        )
        .await
        .unwrap();
    let resumed = hub
        .resume_chat(&chat_b.chat.id)
        .await
        .expect("second worktree must auto-allow from the remembered grant");
    assert_eq!(resumed.chat.id, chat_b.chat.id);

    sessions.shutdown_all().await;
}

#[tokio::test]
async fn changed_envrc_requires_approval_again() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    write_and_commit_envrc(&repo, ".envrc", "export ENVRC_VALUE=original\n");
    let (hub, store, sessions) = hub_with_manager(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();

    let chat_a = hub
        .create_chat_with_workspace(
            &project.id,
            "codex",
            None,
            selection(WorkspaceMode::ManagedWorktree, Some("main")),
        )
        .await
        .unwrap();
    hub.authorize_chat_environment(&chat_a.chat.id, true)
        .await
        .unwrap();
    assert!(store.project_envrc_grant(&project.id).unwrap().is_some());

    write_and_commit_envrc(&repo, ".envrc", "export ENVRC_VALUE=changed\n");

    let chat_c = hub
        .create_chat_with_workspace(
            &project.id,
            "codex",
            None,
            selection(WorkspaceMode::ManagedWorktree, Some("main")),
        )
        .await
        .unwrap();
    let err = hub.resume_chat(&chat_c.chat.id).await.unwrap_err();
    assert!(matches!(err, ServiceError::EnvrcBlocked { .. }));

    sessions.shutdown_all().await;
}

#[tokio::test]
async fn same_content_in_a_different_project_is_not_approved() {
    let tmp = tempfile::tempdir().unwrap();
    let repo1 = git_repo(&tmp.path().join("a"));
    let repo2 = git_repo(&tmp.path().join("b"));
    write_and_commit_envrc(&repo1, ".envrc", "export SAME_CONTENT=1\n");
    write_and_commit_envrc(&repo2, ".envrc", "export SAME_CONTENT=1\n");

    let (hub, store, sessions) = hub_with_manager(tmp.path());
    let project1 = hub
        .create_project("git1".into(), repo1.display().to_string())
        .unwrap();
    let project2 = hub
        .create_project("git2".into(), repo2.display().to_string())
        .unwrap();

    let chat_a = hub
        .create_chat_with_workspace(
            &project1.id,
            "codex",
            None,
            selection(WorkspaceMode::ManagedWorktree, Some("main")),
        )
        .await
        .unwrap();
    hub.authorize_chat_environment(&chat_a.chat.id, true)
        .await
        .unwrap();
    assert!(store.project_envrc_grant(&project1.id).unwrap().is_some());
    assert!(
        store.project_envrc_grant(&project2.id).unwrap().is_none(),
        "identical .envrc content must never authorize an unrelated project"
    );

    let chat_b = hub
        .create_chat_with_workspace(
            &project2.id,
            "codex",
            None,
            selection(WorkspaceMode::ManagedWorktree, Some("main")),
        )
        .await
        .unwrap();
    let err = hub.resume_chat(&chat_b.chat.id).await.unwrap_err();
    assert!(matches!(err, ServiceError::EnvrcBlocked { .. }));

    sessions.shutdown_all().await;
}

#[tokio::test]
async fn relative_path_mismatch_is_not_approved() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    git(&repo, &["checkout", "-b", "root-envrc"]);
    write_and_commit_envrc(&repo, ".envrc", "export SAME_CONTENT=1\n");
    git(&repo, &["checkout", "main"]);
    git(&repo, &["checkout", "-b", "nested-envrc"]);
    write_and_commit_envrc(&repo, "nested/.envrc", "export SAME_CONTENT=1\n");
    git(&repo, &["checkout", "main"]);

    let (hub, store, sessions) = hub_with_manager(tmp.path());
    let project = hub
        .create_project("git".into(), repo.join("nested").display().to_string())
        .unwrap();

    let chat_a = hub
        .create_chat_with_workspace(
            &project.id,
            "codex",
            None,
            selection(WorkspaceMode::ManagedWorktree, Some("root-envrc")),
        )
        .await
        .unwrap();
    hub.authorize_chat_environment(&chat_a.chat.id, true)
        .await
        .unwrap();
    let grant = store
        .project_envrc_grant(&project.id)
        .unwrap()
        .expect("remember must record a grant");
    assert_eq!(grant.relative_path, ".envrc");

    let chat_d = hub
        .create_chat_with_workspace(
            &project.id,
            "codex",
            None,
            selection(WorkspaceMode::ManagedWorktree, Some("nested-envrc")),
        )
        .await
        .unwrap();
    let err = hub.resume_chat(&chat_d.chat.id).await.unwrap_err();
    assert!(
        matches!(err, ServiceError::EnvrcBlocked { .. }),
        "identical bytes at a different relative .envrc location must not auto-allow"
    );

    sessions.shutdown_all().await;
}

#[tokio::test]
async fn remembering_a_symlink_escaping_envrc_fails_closed_without_a_grant() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside.envrc");
    std::fs::write(&outside, "export SHOULD_NEVER_BE_REMEMBERED=1\n").unwrap();
    let project_dir = tmp.path().join("plain_project");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::os::unix::fs::symlink(&outside, project_dir.join(".envrc")).unwrap();

    let (hub, store, sessions) = hub_with_manager(tmp.path());
    let project = hub
        .create_project("plain".into(), project_dir.display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();

    let err = hub
        .authorize_chat_environment(&chat.chat.id, true)
        .await
        .unwrap_err();
    assert!(matches!(err, ServiceError::Invalid(_)));
    assert!(
        store.project_envrc_grant(&project.id).unwrap().is_none(),
        "a symlink escaping the boundary must never be remembered"
    );

    sessions.shutdown_all().await;
}

#[tokio::test]
async fn revoking_the_remembered_grant_causes_future_worktrees_to_prompt_again() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git_repo(tmp.path());
    write_and_commit_envrc(&repo, ".envrc", "export ENVRC_SHARED=1\n");
    let (hub, store, sessions) = hub_with_manager(tmp.path());
    let project = hub
        .create_project("git".into(), repo.display().to_string())
        .unwrap();

    let chat_a = hub
        .create_chat_with_workspace(
            &project.id,
            "codex",
            None,
            selection(WorkspaceMode::ManagedWorktree, Some("main")),
        )
        .await
        .unwrap();
    hub.authorize_chat_environment(&chat_a.chat.id, true)
        .await
        .unwrap();
    assert!(store.project_envrc_grant(&project.id).unwrap().is_some());

    let chat_b = hub
        .create_chat_with_workspace(
            &project.id,
            "codex",
            None,
            selection(WorkspaceMode::ManagedWorktree, Some("main")),
        )
        .await
        .unwrap();
    hub.resume_chat(&chat_b.chat.id)
        .await
        .expect("second worktree auto-allowed while the grant matches");

    hub.forget_project_envrc_grant(&project.id).await.unwrap();
    assert!(store.project_envrc_grant(&project.id).unwrap().is_none());

    let chat_c = hub
        .create_chat_with_workspace(
            &project.id,
            "codex",
            None,
            selection(WorkspaceMode::ManagedWorktree, Some("main")),
        )
        .await
        .unwrap();
    let err = hub.resume_chat(&chat_c.chat.id).await.unwrap_err();
    assert!(
        matches!(err, ServiceError::EnvrcBlocked { .. }),
        "revoking the grant must cause future worktrees to prompt again"
    );

    sessions.shutdown_all().await;
}
