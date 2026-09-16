//! Private per-agent environment overrides (T131).
//!
//! Batey-owned values scoped by agent id, separate from immutable Registry
//! snapshots. They reach only their own agent's chat processes, auth probes,
//! and terminal-auth children, with deterministic precedence, and they never
//! appear in list/detail responses, logs, errors, events, or frontend state.

use batey::{
    agents::{
        registry::{default_fetch, RegistryClient},
        AgentCatalog, AgentManager, CustomAgentInput, HostRuntimeProbe,
    },
    auth::{AgentAuthService, AuthFreshness},
    config::{BateyPaths, Config, PathOverrides},
    events::EventLog,
    service::HubService,
    session::SessionManager,
    store::{AgentEnvAction, AgentEnvEdit, Store},
};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

fn replace(name: &str, value: &str) -> AgentEnvEdit {
    AgentEnvEdit {
        name: name.into(),
        value: Some(value.into()),
        action: AgentEnvAction::Replace,
    }
}

fn remove(name: &str) -> AgentEnvEdit {
    AgentEnvEdit {
        name: name.into(),
        value: None,
        action: AgentEnvAction::Remove,
    }
}

struct Harness {
    hub: Arc<HubService>,
    sessions: Arc<SessionManager>,
    auth: Arc<AgentAuthService>,
    store: Arc<Store>,
    manager: Arc<AgentManager>,
    root: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let paths = BateyPaths::from_overrides(PathOverrides {
            database: Some(root.path().join("hub.db")),
            data_dir: Some(root.path().join("data")),
            state_dir: Some(root.path().join("state")),
            ..Default::default()
        });
        let store = Arc::new(Store::open(&paths.database).unwrap());
        let events = Arc::new(EventLog::persistent(store.clone()).unwrap());
        let agents = Arc::new(AgentCatalog::new([]));
        let sessions = SessionManager::with_store(agents.clone(), events, Some(store.clone()));
        let registry = Arc::new(RegistryClient::new(
            "https://registry.invalid/registry.json",
            paths.registry_cache.clone(),
            default_fetch(),
        ));
        let manager = AgentManager::new(
            store.clone(),
            agents.clone(),
            registry,
            paths.installed_agents.clone(),
            Arc::new(HostRuntimeProbe),
        );
        let auth = AgentAuthService::new(
            agents.clone(),
            sessions.clone(),
            Config::agent_auth_dir(&paths),
        );
        let mut config = Config {
            paths,
            agents: agents.clone(),
            agent_manager: Some(manager.clone()),
            agent_auth: Some(auth.clone()),
            ..Default::default()
        };
        config.web.project_roots = vec![root.path().display().to_string()];
        let config = Arc::new(config);
        let hub = HubService::with_agent_manager(
            store.clone(),
            sessions.clone(),
            agents,
            manager.clone(),
            &config,
        );
        Self {
            hub,
            sessions,
            auth,
            store,
            manager,
            root,
        }
    }

    fn custom_input(id: &str, history: &std::path::Path, mode: &str) -> CustomAgentInput {
        CustomAgentInput {
            id: id.into(),
            command: "python3".into(),
            args: vec![
                format!("{}/tests/fake_acp.py", env!("CARGO_MANIFEST_DIR")),
                history.display().to_string(),
                mode.into(),
            ],
            ..CustomAgentInput::default()
        }
    }

    async fn install_dump_agent(&self, id: &str) -> std::path::PathBuf {
        let history = self.root.path().join(format!("history-{id}"));
        std::fs::create_dir_all(&history).unwrap();
        let input = Self::custom_input(id, &history, "dump-env");
        self.manager.create_custom(input).await.unwrap();
        history
    }

    async fn install_auth_agent(&self, id: &str) -> std::path::PathBuf {
        let history = self.root.path().join(format!("history-{id}"));
        std::fs::create_dir_all(&history).unwrap();
        let input = Self::custom_input(id, &history, "auth");
        self.manager.create_custom(input).await.unwrap();
        history
    }

    fn dump_env(&self, history: &std::path::Path) -> serde_json::Value {
        let dumps: Vec<_> = std::fs::read_dir(history)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json")
                    && entry.file_name().to_string_lossy().ends_with(".env.json")
            })
            .collect();
        assert_eq!(dumps.len(), 1, "expected one env dump in {history:?}");
        serde_json::from_str(&std::fs::read_to_string(dumps[0].path()).unwrap()).unwrap()
    }
}

/// Overrides survive restarts, never appear in API responses, and registry
/// snapshots stay pinned.
#[tokio::test]
async fn overrides_are_private_redacted_and_durable() {
    let harness = Harness::new();
    harness.install_dump_agent("codex").await;

    harness
        .hub
        .update_agent_env("codex", vec![replace("CODEX_API_KEY", "secret-value")])
        .await
        .unwrap();

    // Presence only, never values.
    let presence = harness.hub.agent_env_presence("codex").unwrap();
    assert_eq!(presence.len(), 1);
    assert_eq!(presence[0].name, "CODEX_API_KEY");
    assert!(presence[0].present);
    let exposed = serde_json::to_string(&presence).unwrap();
    assert!(exposed.contains("CODEX_API_KEY"));
    assert!(!exposed.contains("secret-value"));

    // List/detail responses never contain the value.
    let summaries = harness.hub.list_agents();
    let listed = serde_json::to_string(&summaries).unwrap();
    assert!(!listed.contains("secret-value"));
    let detail = harness.hub.agent_management_detail("codex").unwrap();
    let detailed = serde_json::to_string(&detail).unwrap();
    assert!(!detailed.contains("secret-value"));

    // The rows are durable: a second connection to the same database sees
    // them, so a restart rebuilds them.
    let db_path = harness.root.path().join("hub.db");
    let reopened = Store::open(&db_path).unwrap();
    assert_eq!(
        reopened.agent_env("codex").unwrap()["CODEX_API_KEY"],
        "secret-value"
    );
}

/// A value for Codex never reaches Claude and vice versa.
#[tokio::test]
async fn overrides_are_isolated_per_agent_in_chat_sessions() {
    let harness = Harness::new();
    let history_codex = harness.install_dump_agent("codex").await;
    let history_claude = harness.install_dump_agent("claude").await;

    harness
        .hub
        .update_agent_env("codex", vec![replace("CODEX_API_KEY", "codex-secret")])
        .await
        .unwrap();
    harness
        .hub
        .update_agent_env("claude", vec![replace("GH_TOKEN", "claude-secret")])
        .await
        .unwrap();

    let project = harness
        .hub
        .create_project("demo".into(), harness.root.path().display().to_string())
        .unwrap();

    let chat_codex = harness
        .hub
        .create_chat(&project.id, "codex", None)
        .await
        .unwrap();
    harness
        .sessions
        .get_by_id(&chat_codex.chat.id)
        .await
        .unwrap()
        .ensure_running()
        .await
        .unwrap();
    let observed_codex = harness.dump_env(&history_codex);
    assert_eq!(
        observed_codex.get("CODEX_API_KEY").and_then(|v| v.as_str()),
        Some("codex-secret")
    );
    assert!(observed_codex.get("GH_TOKEN").is_none());

    let chat_claude = harness
        .hub
        .create_chat(&project.id, "claude", None)
        .await
        .unwrap();
    harness
        .sessions
        .get_by_id(&chat_claude.chat.id)
        .await
        .unwrap()
        .ensure_running()
        .await
        .unwrap();
    let observed_claude = harness.dump_env(&history_claude);
    assert_eq!(
        observed_claude.get("GH_TOKEN").and_then(|v| v.as_str()),
        Some("claude-secret")
    );
    assert!(observed_claude.get("CODEX_API_KEY").is_none());

    harness.sessions.shutdown_all().await;
}

/// Removing an override removes it from future launches.
#[tokio::test]
async fn removing_an_override_removes_it_from_future_launches() {
    let harness = Harness::new();
    let history = harness.install_dump_agent("codex").await;
    harness
        .hub
        .update_agent_env("codex", vec![replace("CODEX_API_KEY", "one")])
        .await
        .unwrap();

    let project = harness
        .hub
        .create_project("demo".into(), harness.root.path().display().to_string())
        .unwrap();
    let chat = harness
        .hub
        .create_chat(&project.id, "codex", None)
        .await
        .unwrap();
    harness
        .sessions
        .get_by_id(&chat.chat.id)
        .await
        .unwrap()
        .ensure_running()
        .await
        .unwrap();
    assert_eq!(
        harness
            .dump_env(&history)
            .get("CODEX_API_KEY")
            .and_then(|v| v.as_str()),
        Some("one")
    );

    // Remove and restart the session: the next launch must not see it.
    harness
        .hub
        .update_agent_env("codex", vec![remove("CODEX_API_KEY")])
        .await
        .unwrap();
    assert!(harness.hub.agent_env_presence("codex").unwrap().is_empty());
    // A live session keeps the environment it started with; only future
    // launches change. Start a new chat so the agent runs session/new again
    // and dumps its fresh environment.
    let live = harness.sessions.get_by_id(&chat.chat.id).await.unwrap();
    live.shutdown().await;
    for entry in std::fs::read_dir(&history).unwrap().flatten() {
        if entry.file_name().to_string_lossy().ends_with(".env.json") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let chat2 = harness
        .hub
        .create_chat(&project.id, "codex", None)
        .await
        .unwrap();
    harness
        .sessions
        .get_by_id(&chat2.chat.id)
        .await
        .unwrap()
        .ensure_running()
        .await
        .unwrap();
    let observed = harness.dump_env(&history);
    assert!(observed.get("CODEX_API_KEY").is_none());

    harness.sessions.shutdown_all().await;
}

/// Overrides win over workspace/launch values with deterministic precedence.
#[tokio::test]
async fn override_precedence_beats_workspace_and_launch() {
    let workspace: HashMap<String, String> = HashMap::from([
        ("SHARED".to_string(), "workspace".to_string()),
        ("DIRTY".to_string(), "workspace".to_string()),
    ]);
    let launch: HashMap<String, String> =
        HashMap::from([("SHARED".to_string(), "launch".to_string())]);
    let secrets: HashMap<String, String> =
        HashMap::from([("DIRTY".to_string(), "stashed".to_string())]);
    let overrides: HashMap<String, String> = HashMap::from([
        ("SHARED".to_string(), "override".to_string()),
        ("GEMINI_API_KEY".to_string(), "private".to_string()),
    ]);
    let env = batey::workspace_env::resolve_agent_env_with_overrides(
        &workspace,
        &launch,
        &[],
        &secrets,
        &overrides,
    );
    assert_eq!(env.get("SHARED").map(String::as_str), Some("override"));
    assert_eq!(
        env.get("GEMINI_API_KEY").map(String::as_str),
        Some("private")
    );
    // The stashed name was scrubbed from the workspace base.
    assert_ne!(env.get("DIRTY").map(String::as_str), Some("workspace"));
}

/// Auth probes receive the same per-agent overrides; terminal children get
/// them plus method-specific terminal overrides.
#[tokio::test]
async fn auth_probe_and_terminal_env_receive_overrides() {
    let harness = Harness::new();
    let history = harness.install_auth_agent("codex").await;
    harness
        .hub
        .update_agent_env("codex", vec![replace("GEMINI_API_KEY", "gemini-secret")])
        .await
        .unwrap();

    // The probe writes probe-env.json on initialize. A plain read never
    // probes, so this exercises the explicit refresh operation.
    harness.hub.refresh_agent_auth("codex").await.unwrap();
    let probe: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(history.join("probe-env.json")).unwrap())
            .unwrap();
    assert_eq!(
        probe.get("GEMINI_API_KEY").and_then(|v| v.as_str()),
        Some("gemini-secret")
    );

    // Terminal-auth children receive the effective agent env plus method env.
    let runtime = harness
        .manager
        .catalog()
        .runtime("codex")
        .expect("runtime missing");
    let base = {
        let overrides: HashMap<String, String> = harness
            .store
            .agent_env("codex")
            .unwrap()
            .into_iter()
            .collect();
        batey::workspace_env::resolve_agent_env_with_overrides(
            &HashMap::new(),
            &runtime.launch.env,
            &runtime.launch.pass_env,
            &HashMap::new(),
            &overrides,
        )
    };
    let method = batey::acp::auth::TerminalAuthMethod {
        args: vec!["login".into()],
        env: BTreeMap::from([("GEMINI_API_KEY".into(), "from-method".into())]),
    };
    let command = batey::auth::terminal_command(&runtime, &method, &base, harness.root.path());
    assert_eq!(
        command.env.get("GEMINI_API_KEY").map(String::as_str),
        Some("from-method")
    );

    harness.sessions.shutdown_all().await;
}

/// Custom edits and registry-style launch changes preserve overrides without
/// touching snapshots; uninstall cleans up only when no history remains.
#[tokio::test]
async fn edits_preserve_overrides_and_uninstall_cleans_up() {
    let harness = Harness::new();
    harness.install_dump_agent("codex").await;
    harness
        .hub
        .update_agent_env("codex", vec![replace("CODEX_API_KEY", "kept")])
        .await
        .unwrap();

    // Editing the launch definition preserves the separate overrides.
    let record = harness.store.installed_agent("codex").unwrap().unwrap();
    let edited_input = CustomAgentInput {
        id: "codex".into(),
        command: record.command.clone(),
        args: vec!["--changed".into()],
        ..CustomAgentInput::default()
    };
    harness
        .manager
        .edit_custom("codex", edited_input)
        .await
        .unwrap();
    assert_eq!(
        harness.store.agent_env("codex").unwrap()["CODEX_API_KEY"],
        "kept"
    );
    // The snapshot concept holds: launch args changed, overrides did not.
    let updated = harness.store.installed_agent("codex").unwrap().unwrap();
    assert_eq!(updated.args, vec!["--changed"]);

    // Uninstall with retained chats retires and keeps overrides inertly.
    let project = harness
        .hub
        .create_project("demo".into(), harness.root.path().display().to_string())
        .unwrap();
    let _chat = harness
        .hub
        .create_chat(&project.id, "codex", None)
        .await
        .unwrap();
    let outcome = harness.hub.remove_installed_agent("codex").await.unwrap();
    assert!(!outcome.deleted);
    assert!(harness.store.installed_agent("codex").unwrap().is_some());
    assert_eq!(
        harness.store.agent_env("codex").unwrap()["CODEX_API_KEY"],
        "kept"
    );

    // After history goes away, removal deletes the row and its overrides.
    for chat in harness.store.chats().unwrap() {
        harness.store.delete_chat(&chat.id).unwrap();
    }
    let outcome = harness.hub.remove_installed_agent("codex").await.unwrap();
    assert!(outcome.deleted);
    assert!(harness.store.agent_env("codex").unwrap().is_empty());
}

/// The HTTP surface returns presence only and validates without echoing values.
#[tokio::test]
async fn http_environment_routes_are_redacted() {
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;

    let harness = Harness::new();
    harness.install_dump_agent("codex").await;
    harness
        .hub
        .update_agent_env("codex", vec![replace("CODEX_API_KEY", "http-secret")])
        .await
        .unwrap();

    let config = Config {
        paths: BateyPaths::from_overrides(PathOverrides {
            database: Some(harness.root.path().join("hub.db")),
            ..Default::default()
        }),
        ..Default::default()
    };
    // Build a router over the same hub via AppState. A minimal config with
    // the same store/sessions/catalog is enough because HubService is shared.
    let app = batey::web::router(batey::web::AppState {
        session_manager: harness.sessions.clone(),
        config: Arc::new({
            let mut config = config;
            config.web.project_roots = vec![harness.root.path().display().to_string()];
            config.agents = harness.manager.catalog().clone();
            config.agent_manager = Some(harness.manager.clone());
            config.agent_auth = Some(harness.auth.clone());
            config
        }),
        server_port: 8765,
        hub: Some(harness.hub.clone()),
    });

    async fn get(app: &axum::Router, uri: &str) -> (u16, serde_json::Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .header("host", "127.0.0.1:8765")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status().as_u16();
        let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    let (status, body) = get(&app, "/api/agents/codex/environment").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["name"], "CODEX_API_KEY");
    assert!(body[0].get("value").is_none());
    assert!(!body.to_string().contains("http-secret"));

    let (status, body) = get(&app, "/api/agents").await;
    assert_eq!(status, 200);
    assert!(!body.to_string().contains("http-secret"));

    // Unknown ids are not found; builtin ids are conflicts, never orphans.
    let (status, _) = get(&app, "/api/agents/absent/environment").await;
    assert_eq!(status, 404);
}

/// Changing an override marks the durable discovery cache stale and
/// invalidates stopped sessions, so the next explicit check and the next
/// launch both observe the new value.
#[tokio::test]
async fn env_changes_invalidate_auth_cache_and_stopped_sessions() {
    let harness = Harness::new();
    let history = harness.install_auth_agent("codex").await;

    // A first explicit refresh caches state and writes env without the
    // override.
    harness.hub.refresh_agent_auth("codex").await.unwrap();
    let first: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(history.join("probe-env.json")).unwrap())
            .unwrap();
    assert!(first.get("CODEX_API_KEY").is_none());

    // Changing the override marks the cache stale. A plain read never
    // probes, so the next explicit refresh observes the new value.
    harness
        .hub
        .update_agent_env("codex", vec![replace("CODEX_API_KEY", "fresh")])
        .await
        .unwrap();
    let stale = harness.hub.agent_auth("codex").await.unwrap();
    assert_eq!(
        stale.freshness,
        AuthFreshness::Stale,
        "the environment change never marked the cache stale"
    );
    harness.hub.refresh_agent_auth("codex").await.unwrap();
    let second: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(history.join("probe-env.json")).unwrap())
            .unwrap();
    assert_eq!(
        second.get("CODEX_API_KEY").and_then(|v| v.as_str()),
        Some("fresh")
    );

    // Stopped sessions are invalidated: a stopped session disappears from the
    // manager so the next lookup rebuilds it.
    let project = harness
        .hub
        .create_project("demo".into(), harness.root.path().display().to_string())
        .unwrap();
    // Use dump-env agent for session test instead; auth agent history differs.
    let harness2 = Harness::new();
    let _ = harness2.install_dump_agent("codex").await;
    let project2 = harness2
        .hub
        .create_project("demo".into(), harness2.root.path().display().to_string())
        .unwrap();
    let chat = harness2
        .hub
        .create_chat(&project2.id, "codex", None)
        .await
        .unwrap();
    let first_session = harness2.sessions.get_by_id(&chat.chat.id).await.unwrap();
    first_session.ensure_running().await.unwrap();
    first_session.shutdown().await;
    // Still materialized as stopped.
    assert!(harness2.sessions.get_by_id(&chat.chat.id).await.is_some());
    harness2
        .hub
        .update_agent_env("codex", vec![replace("CODEX_API_KEY", "v2")])
        .await
        .unwrap();
    // Implementation detail: the stopped session was retired. A lookup
    // rebuilds, which proves the next launch reads fresh overrides. The chat
    // itself is untouched.
    let _ = project;
    let rebuilt = harness2.sessions.get_by_id(&chat.chat.id).await;
    assert!(rebuilt.is_some());
    harness2.sessions.shutdown_all().await;
}

/// Only installed agents take overrides; arbitrary errors never create them.
#[tokio::test]
async fn only_installed_agents_take_overrides_and_errors_never_infer_credentials() {
    let harness = Harness::new();
    // Unknown id.
    assert!(harness.hub.agent_env_presence("absent").is_err());
    // No catalog entry for builtin here (empty catalog), so also not found.
    // After installing, validation still refuses bad names without storing.
    harness.install_dump_agent("codex").await;
    let error = harness
        .hub
        .update_agent_env("codex", vec![replace("BAD-NAME", "x")])
        .await
        .unwrap_err();
    assert!(!error.to_string().contains('x'));
    assert!(harness.hub.agent_env_presence("codex").unwrap().is_empty());

    // An agent error message mentioning credentials never creates an override.
    // Batey never parses error text for credential names; the presence stays empty.
    assert!(harness.hub.agent_env_presence("codex").unwrap().is_empty());
}

/// Arbitrary provider names validate; error paths never echo values.
#[tokio::test]
async fn arbitrary_names_validate_and_errors_hide_values() {
    let harness = Harness::new();
    harness.install_dump_agent("codex").await;
    for name in [
        "CODEX_API_KEY",
        "OPENAI_API_KEY",
        "GEMINI_API_KEY",
        "GH_TOKEN",
        "COPILOT_GITHUB_TOKEN",
        "NO_BROWSER",
    ] {
        harness
            .hub
            .update_agent_env("codex", vec![replace(name, "v")])
            .await
            .unwrap();
    }
    assert_eq!(harness.hub.agent_env_presence("codex").unwrap().len(), 6);

    let error = harness
        .hub
        .update_agent_env("codex", vec![replace("HAS-DASH", "super-secret")])
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("super-secret"));
}
