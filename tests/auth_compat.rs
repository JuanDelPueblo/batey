//! T133: headless authentication compatibility for known Registry agents.
//!
//! Compatibility environment defaults are centralized in one layer and match
//! on the official Registry id from the installed snapshot, never on a Batey
//! catalog id or an agent name. They apply to authentication probe and
//! protocol-auth processes only, never to ordinary chat sessions, and a T131
//! user override always wins.
use batey::{
    agents::{
        AgentCatalog, AgentDefinition, AgentSource, InstalledAgent, InstalledDistribution,
        RegistrySnapshot,
    },
    auth::AgentAuthService,
    config::{BateyPaths, Config, PathOverrides},
    events::EventLog,
    service::HubService,
    session::SessionManager,
    store::Store,
};
use std::sync::Arc;
use url::Url;

struct Harness {
    hub: Arc<HubService>,
    sessions: Arc<SessionManager>,
    auth: Arc<AgentAuthService>,
    store: Arc<Store>,
    catalog: Arc<AgentCatalog>,
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
        let catalog = Arc::new(AgentCatalog::default());
        let sessions = SessionManager::with_store(catalog.clone(), events, Some(store.clone()));
        let auth = AgentAuthService::new(
            catalog.clone(),
            sessions.clone(),
            Config::agent_auth_dir(&paths),
        );
        let mut config = Config {
            paths,
            agents: catalog.clone(),
            agent_auth: Some(auth.clone()),
            ..Default::default()
        };
        config.web.project_roots = vec![root.path().display().to_string()];
        let config = Arc::new(config);
        let hub = HubService::new(store.clone(), sessions.clone(), catalog.clone(), &config);
        Self {
            hub,
            sessions,
            auth,
            store,
            catalog,
            root,
        }
    }

    /// Installs one Registry-style agent whose snapshot names `registry_id`,
    /// with the fake ACP process in the requested mode. The catalog entry is
    /// always available so the test never depends on the host `PATH`.
    async fn install_registry_agent(
        &self,
        id: &str,
        registry_id: &str,
        mode: &str,
    ) -> std::path::PathBuf {
        let history = self.root.path().join(format!("history-{id}"));
        std::fs::create_dir_all(&history).unwrap();
        let args = vec![
            format!("{}/tests/fake_acp.py", env!("CARGO_MANIFEST_DIR")),
            history.display().to_string(),
            mode.into(),
        ];
        let definition = AgentDefinition::new(id, "python3")
            .with_args(args.clone())
            .with_source(AgentSource::Registry);
        self.catalog.insert(definition).unwrap();

        let mut record = InstalledAgent::new(id.into(), AgentSource::Registry, "python3".into());
        record.args = args;
        record.registry = Some(RegistrySnapshot {
            registry_id: registry_id.into(),
            registry_version: "1.0.0".into(),
            distribution: InstalledDistribution::Npx {
                package: format!("{registry_id}@1.0.0"),
                args: Vec::new(),
                env: Default::default(),
            },
            install_dir: None,
            installed_at: chrono::Utc::now().to_rfc3339(),
        });
        self.store.insert_agent(&record).unwrap();
        history
    }

    fn probe_env(&self, history: &std::path::Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(history.join("probe-env.json")).unwrap())
            .unwrap()
    }

    fn dump_env(&self, history: &std::path::Path) -> serde_json::Value {
        let entry = std::fs::read_dir(history)
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().ends_with(".env.json"))
            .expect("the chat process never dumped its environment");
        serde_json::from_str(&std::fs::read_to_string(entry.path()).unwrap()).unwrap()
    }

    /// Waits for the terminal-auth child to record its invocation.
    async fn invocation(&self, history: &std::path::Path) -> serde_json::Value {
        let path = history.join("invocation.json");
        for _ in 0..200 {
            if path.exists() {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if let Ok(value) = serde_json::from_str(&text) {
                        return value;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("the terminal-auth child never recorded its invocation");
    }
}

async fn wait_for_interaction(
    harness: &Harness,
    flow_id: &str,
) -> batey::auth::ProtocolAuthInteractionView {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let flow = harness.auth.protocol_flow(flow_id).unwrap();
            if flow.state().is_finished() {
                panic!("protocol flow became terminal before exposing its interaction");
            }
            if let Some(interaction) = harness.auth.protocol_interaction(flow_id).unwrap() {
                return interaction;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("protocol auth interaction did not appear before the test deadline")
}

/// Codex's default Batey auth environment uses `NO_BROWSER=1` so upstream
/// advertises headless-suitable methods.
#[tokio::test]
async fn codex_probe_uses_no_browser_by_default() {
    let harness = Harness::new();
    let history = harness
        .install_registry_agent("codex", "codex-acp", "auth")
        .await;
    harness.hub.refresh_agent_auth("codex").await.unwrap();
    let probe = harness.probe_env(&history);
    assert_eq!(probe.get("NO_BROWSER").and_then(|v| v.as_str()), Some("1"));
    harness.sessions.shutdown_all().await;
}

/// A user-provided T131 `NO_BROWSER` override wins over the compat default.
#[tokio::test]
async fn t131_no_browser_override_wins() {
    let harness = Harness::new();
    let history = harness
        .install_registry_agent("codex", "codex-acp", "auth")
        .await;
    harness.hub.refresh_agent_auth("codex").await.unwrap();
    assert_eq!(
        harness.probe_env(&history)["NO_BROWSER"],
        serde_json::json!("1")
    );

    harness
        .hub
        .update_agent_env(
            "codex",
            vec![batey::store::AgentEnvEdit {
                name: "NO_BROWSER".into(),
                value: Some("0".into()),
                action: batey::store::AgentEnvAction::Replace,
            }],
        )
        .await
        .unwrap();
    harness.hub.refresh_agent_auth("codex").await.unwrap();
    assert_eq!(
        harness.probe_env(&history)["NO_BROWSER"],
        serde_json::json!("0")
    );
    harness.sessions.shutdown_all().await;
}

/// Copilot's auth environment uses `CI=true` for its headless path, and a
/// T131 override wins.
#[tokio::test]
async fn copilot_probe_uses_ci_and_an_override_wins() {
    let harness = Harness::new();
    let history = harness
        .install_registry_agent("copilot", "github-copilot-cli", "auth")
        .await;
    harness.hub.refresh_agent_auth("copilot").await.unwrap();
    assert_eq!(harness.probe_env(&history)["CI"], serde_json::json!("true"));

    harness
        .hub
        .update_agent_env(
            "copilot",
            vec![batey::store::AgentEnvEdit {
                name: "CI".into(),
                value: Some("false".into()),
                action: batey::store::AgentEnvAction::Replace,
            }],
        )
        .await
        .unwrap();
    harness.hub.refresh_agent_auth("copilot").await.unwrap();
    assert_eq!(
        harness.probe_env(&history)["CI"],
        serde_json::json!("false")
    );
    harness.sessions.shutdown_all().await;
}

/// A normal Codex chat session does not receive the compatibility default.
#[tokio::test]
async fn normal_chat_sessions_do_not_get_compat_env() {
    let harness = Harness::new();
    let history = harness
        .install_registry_agent("codex", "codex-acp", "dump-env")
        .await;
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
    let dump = harness.dump_env(&history);
    // Only assert absence when the host does not set the name itself.
    if std::env::var_os("NO_BROWSER").is_none() {
        assert!(
            dump.get("NO_BROWSER").is_none(),
            "the compat default leaked into a chat session: {dump}"
        );
    }
    harness.sessions.shutdown_all().await;
}

/// Antigravity compatibility is activated by the official Registry id, not by
/// a catalog alias or display name.
#[tokio::test]
async fn antigravity_compatibility_is_registry_scoped() {
    let harness = Harness::new();
    harness
        .install_registry_agent("anti", "antigravity-acp", "auth")
        .await;
    assert!(
        batey::agents::protocol_auth_compatibility(Some("antigravity-acp"))
            .unwrap()
            .intercept_browser
    );

    // A non-antigravity agent exposes no compatibility interaction.
    harness
        .install_registry_agent("codex", "codex-acp", "auth")
        .await;
    let _codex = harness.hub.refresh_agent_auth("codex").await.unwrap().auth;
    assert!(batey::agents::protocol_auth_compatibility(Some("codex-acp")).is_none());

    // A custom agent with an Antigravity-looking catalog id gets no special
    // behavior because it has no official Registry identity.
    assert!(batey::agents::protocol_auth_compatibility(Some("Antigravity")).is_none());
    harness.sessions.shutdown_all().await;
}

#[tokio::test]
async fn antigravity_remote_browser_callback_is_relayed() {
    let harness = Harness::new();
    harness
        .install_registry_agent("anti", "antigravity-acp", "auth-antigravity-browser")
        .await;
    let flow = harness
        .hub
        .start_protocol_auth("anti", "oauth-personal")
        .await
        .unwrap();

    let interaction = wait_for_interaction(&harness, &flow.flow_id).await;
    assert_eq!(interaction.kind, "browser");
    let authorization = Url::parse(&interaction.url).unwrap();
    let redirect = authorization
        .query_pairs()
        .find(|(key, _)| key == "redirect_uri")
        .unwrap()
        .1
        .into_owned();
    let state = authorization
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .into_owned();
    let mut callback = Url::parse(&redirect).unwrap();
    callback
        .query_pairs_mut()
        .append_pair("code", "fake-code")
        .append_pair("state", &state);
    let mut wrong_port = callback.clone();
    wrong_port.set_port(Some(1)).unwrap();
    assert!(harness
        .hub
        .relay_protocol_auth_callback(&flow.flow_id, wrong_port.as_str())
        .await
        .is_err());
    harness
        .hub
        .relay_protocol_auth_callback(&flow.flow_id, callback.as_str())
        .await
        .unwrap();

    for _ in 0..100 {
        if harness.auth.protocol_flow(&flow.flow_id).unwrap().state()
            == batey::auth::ProtocolFlowState::Succeeded
        {
            harness.sessions.shutdown_all().await;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("the relayed callback did not complete authentication");
}

#[tokio::test]
async fn antigravity_oauth_denial_is_relayed_without_exposing_callback() {
    let harness = Harness::new();
    harness
        .install_registry_agent("anti", "antigravity-acp", "auth-antigravity-deny")
        .await;
    let flow = harness
        .hub
        .start_protocol_auth("anti", "oauth-personal")
        .await
        .unwrap();
    let interaction = wait_for_interaction(&harness, &flow.flow_id).await;
    let authorization = Url::parse(&interaction.url).unwrap();
    let redirect = authorization
        .query_pairs()
        .find(|(key, _)| key == "redirect_uri")
        .unwrap()
        .1
        .into_owned();
    let state = authorization
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .into_owned();
    let mut callback = Url::parse(&redirect).unwrap();
    callback
        .query_pairs_mut()
        .append_pair("error", "access_denied")
        .append_pair("state", &state);
    harness
        .hub
        .relay_protocol_auth_callback(&flow.flow_id, callback.as_str())
        .await
        .unwrap();
    for _ in 0..100 {
        if harness.auth.protocol_flow(&flow.flow_id).unwrap().state()
            == batey::auth::ProtocolFlowState::Failed
        {
            assert!(harness
                .auth
                .protocol_interaction(&flow.flow_id)
                .unwrap()
                .is_none());
            harness.sessions.shutdown_all().await;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("the denied callback did not complete authentication");
}

#[tokio::test]
async fn malicious_antigravity_authorization_url_fails_closed() {
    let harness = Harness::new();
    harness
        .install_registry_agent("anti", "antigravity-acp", "auth-antigravity-malicious")
        .await;
    let flow = harness
        .hub
        .start_protocol_auth("anti", "oauth-personal")
        .await
        .unwrap();
    for _ in 0..100 {
        let current = harness.auth.protocol_flow(&flow.flow_id).unwrap();
        if current.state() == batey::auth::ProtocolFlowState::Failed {
            let view = current.view();
            assert_eq!(
                view.reason.as_deref(),
                Some("The agent provided an authentication URL that Batey could not validate.")
            );
            assert!(harness
                .auth
                .protocol_interaction(&flow.flow_id)
                .unwrap()
                .is_none());
            harness.sessions.shutdown_all().await;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("malicious authorization URL did not fail closed");
}

/// Active-flow discovery is safe: it exposes lifecycle only, never terminal
/// output, codes, tokens, or URLs.
#[tokio::test]
async fn active_flow_discovery_is_safe() {
    let harness = Harness::new();
    harness
        .install_registry_agent("codex", "codex-acp", "auth")
        .await;
    let flow = harness
        .hub
        .start_terminal_auth("codex", "tui")
        .await
        .unwrap();

    let view = harness.hub.agent_auth("codex").await.unwrap();
    let active = view
        .active_flow
        .clone()
        .expect("no active flow was discovered");
    assert_eq!(active.kind, "terminal");
    assert_eq!(active.method_id, "tui");
    assert_eq!(active.state, "running");
    assert_eq!(active.flow_id, flow.flow_id);

    // The serialized view carries none of the flow's private material.
    let serialized = serde_json::to_string(&view).unwrap();
    assert!(!serialized.contains("output"));
    assert!(!serialized.contains("scrollback"));
    assert!(!serialized.contains("http://"));
    assert!(!serialized.contains("https://"));

    harness.auth.cancel_terminal_flow(&flow.flow_id).unwrap();
    let after = harness.hub.agent_auth("codex").await.unwrap();
    assert!(
        after.active_flow.is_none(),
        "a cancelled flow stayed active"
    );
    harness.sessions.shutdown_all().await;
}

/// A protocol flow is discoverable too, and one active flow per agent holds
/// across both kinds so a user always has a resumable path.
#[tokio::test]
async fn protocol_flow_is_discoverable_and_blocks_a_second_flow() {
    let harness = Harness::new();
    harness
        .install_registry_agent("codex", "codex-acp", "auth-codex-url")
        .await;
    let flow = harness
        .hub
        .start_protocol_auth("codex", "codex-oauth")
        .await
        .unwrap();

    let view = harness.hub.agent_auth("codex").await.unwrap();
    let active = view.active_flow.expect("no protocol flow was discovered");
    assert_eq!(active.kind, "protocol");
    assert_eq!(active.method_id, "codex-oauth");
    assert_eq!(active.flow_id, flow.flow_id);

    // A terminal start for the same agent is refused while the protocol flow
    // runs, and the refusal names the conflicting flow so recovery is possible.
    let error = harness
        .hub
        .start_terminal_auth("codex", "tui")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("already has an authentication flow running"),
        "{error}"
    );

    harness
        .hub
        .cancel_protocol_auth(&flow.flow_id)
        .await
        .unwrap();
    harness.sessions.shutdown_all().await;
}

/// Codex's `NO_BROWSER` default is scoped to auth processes: a terminal-auth
/// child never receives it, so the advertised terminal method is unchanged.
#[tokio::test]
async fn codex_terminal_auth_does_not_force_no_browser() {
    if !batey::auth::TERMINAL_AUTH_SUPPORTED {
        return;
    }
    let harness = Harness::new();
    let history = harness
        .install_registry_agent("codex", "codex-acp", "auth")
        .await;
    let flow = harness
        .hub
        .start_terminal_auth("codex", "tui")
        .await
        .unwrap();
    let invocation = harness.invocation(&history).await;
    if std::env::var_os("NO_BROWSER").is_none() {
        assert!(
            invocation["env"].get("NO_BROWSER").is_none(),
            "Codex terminal auth received the auth-only default: {invocation}"
        );
    }
    harness.auth.cancel_terminal_flow(&flow.flow_id).unwrap();
    harness.auth.shutdown();
    harness.sessions.shutdown_all().await;
}

/// Copilot's `CI=true` default reaches its terminal-auth child.
#[tokio::test]
async fn copilot_terminal_auth_uses_ci() {
    if !batey::auth::TERMINAL_AUTH_SUPPORTED {
        return;
    }
    let harness = Harness::new();
    let history = harness
        .install_registry_agent("copilot", "github-copilot-cli", "auth")
        .await;
    let flow = harness
        .hub
        .start_terminal_auth("copilot", "tui")
        .await
        .unwrap();
    let invocation = harness.invocation(&history).await;
    assert_eq!(invocation["env"]["CI"], serde_json::json!("true"));
    harness.auth.cancel_terminal_flow(&flow.flow_id).unwrap();
    harness.auth.shutdown();
    harness.sessions.shutdown_all().await;
}
