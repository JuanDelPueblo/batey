//! Stable ACP v1 agent-level authentication: metadata, `authenticate`,
//! capability-gated `logout`, the structured `auth_required` state, and the
//! shared REST contract.
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use batey::{
    agents::{AgentDefinition, AgentRegistry, AgentSource, InstalledAgent},
    auth::TERMINAL_AUTH_SUPPORTED,
    config::{BateyPaths, Config, PathOverrides},
    events::EventLog,
    service::HubService,
    session::SessionManager,
    store::{AgentEnvAction, AgentEnvEdit, Store},
    web::{router, AppState},
};
use serde_json::Value;
use std::{
    path::Path,
    sync::{Arc, Once, OnceLock},
    time::Duration,
};
use tower::ServiceExt;

/// An agent whose ACP process is the fake peer in one of its auth modes.
fn agent(id: &str, history: &Path, mode: &str) -> AgentDefinition {
    AgentDefinition::new(id, "python3").with_args(vec![
        format!("{}/tests/fake_acp.py", env!("CARGO_MANIFEST_DIR")),
        history.display().to_string(),
        mode.into(),
    ])
}

struct Harness {
    app: axum::Router,
    hub: Arc<HubService>,
    sessions: Arc<SessionManager>,
    agents: Arc<AgentRegistry>,
    root: std::path::PathBuf,
    /// Owns the temporary directory for the lifetime of this harness, when
    /// it created one itself. A restart test builds a second harness over
    /// the same path and keeps the directory alive externally instead.
    _tempdir: Option<tempfile::TempDir>,
}

impl Harness {
    /// Builds a hub whose catalog holds one agent per named auth mode, in a
    /// fresh temporary directory this harness owns.
    fn new(modes: &[(&str, &str)]) -> Self {
        let tempdir = tempfile::tempdir().unwrap();
        let mut harness = Self::at(tempdir.path(), modes);
        harness._tempdir = Some(tempdir);
        harness
    }

    /// Builds a hub over an existing directory, so a caller can reopen the
    /// same durable store across two harnesses, as a restart would.
    fn at(root: &Path, modes: &[(&str, &str)]) -> Self {
        let paths = BateyPaths::from_overrides(PathOverrides {
            database: Some(root.join("hub.db")),
            data_dir: Some(root.join("data")),
            state_dir: Some(root.join("state")),
            ..Default::default()
        });
        let store = Arc::new(Store::open(&paths.database).unwrap());
        let events = Arc::new(EventLog::persistent(store.clone()).unwrap());
        let definitions: Vec<AgentDefinition> = modes
            .iter()
            .map(|(id, mode)| {
                let history = root.join(id);
                std::fs::create_dir_all(&history).unwrap();
                agent(id, &history, mode)
            })
            .collect();
        let agents = Arc::new(AgentRegistry::new(definitions));
        let sessions = SessionManager::with_store(agents.clone(), events, Some(store.clone()));
        let mut config = Config {
            paths,
            agents: agents.clone(),
            ..Default::default()
        };
        config.web.project_roots = vec![root.display().to_string()];
        let config = Arc::new(config);
        let hub = HubService::new(store, sessions.clone(), agents.clone(), &config);
        let app = router(AppState::new(sessions.clone(), config, 8765));
        Self {
            app,
            hub,
            sessions,
            agents,
            root: root.to_path_buf(),
            _tempdir: None,
        }
    }

    fn history(&self, agent_id: &str) -> std::path::PathBuf {
        self.root.join(agent_id)
    }

    /// What the fake agent recorded for one request kind, if anything.
    fn recorded(&self, agent_id: &str, file: &str) -> Option<Value> {
        let path = self.history(agent_id).join(file);
        path.exists()
            .then(|| serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap())
    }

    async fn request(&self, method: &str, uri: &str) -> (u16, Value) {
        self.request_with_body(method, uri, None).await
    }

    async fn request_with_body(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "127.0.0.1:8765");
        let builder = if body.is_some() {
            builder.header("content-type", "application/json")
        } else {
            builder
        };
        let bytes = body.map(|v| v.to_string()).unwrap_or_default();
        let response = self
            .app
            .clone()
            .oneshot(builder.body(Body::from(bytes)).unwrap())
            .await
            .unwrap();
        let status = response.status().as_u16();
        let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }
}

/// The advertised methods survive with their kinds, and a kind this build
/// cannot run comes back as unsupported rather than as a guessed fallback.
#[tokio::test]
async fn auth_methods_and_capabilities_are_preserved_by_kind() {
    let harness = Harness::new(&[("full", "auth"), ("plain", "auth-no-logout")]);
    let (status, body) = harness
        .request("POST", "/api/agents/full/auth/refresh")
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["agent_id"], "full");
    assert_eq!(body["logout_supported"], true);
    assert_eq!(body["terminal_supported"], TERMINAL_AUTH_SUPPORTED);

    let methods = body["methods"].as_array().unwrap();
    assert_eq!(methods.len(), 4, "{body}");
    // An untyped method is an agent method, per the stable schema.
    assert_eq!(methods[0]["id"], "api-key");
    assert_eq!(methods[0]["type"], "agent");
    assert_eq!(methods[0]["name"], "API key");
    assert_eq!(methods[0]["description"], "Paste an API key");
    assert_eq!(methods[0]["supported"], true);
    assert_eq!(methods[2]["id"], "tui");
    assert_eq!(methods[2]["type"], "terminal");
    assert_eq!(methods[2]["supported"], TERMINAL_AUTH_SUPPORTED);
    // The unknown kind is kept and reported, never turned into `agent`.
    assert_eq!(methods[3]["id"], "future");
    assert_eq!(methods[3]["type"], "browser-popup");
    assert_eq!(methods[3]["supported"], false);

    let (status, body) = harness
        .request("POST", "/api/agents/plain/auth/refresh")
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["logout_supported"], false);
    harness.sessions.shutdown_all().await;
}

/// The client advertises terminal authentication only when it truly runs a
/// PTY. An agent reads that flag before it offers a terminal method.
#[tokio::test]
async fn client_advertises_terminal_auth_capability_truthfully() {
    let harness = Harness::new(&[("full", "auth")]);
    let (status, _) = harness
        .request("POST", "/api/agents/full/auth/refresh")
        .await;
    assert_eq!(status, 200);
    let recorded = harness.recorded("full", "initialize.json").unwrap();
    assert_eq!(
        recorded[0]["auth"]["terminal"],
        Value::Bool(TERMINAL_AUTH_SUPPORTED)
    );
    harness.sessions.shutdown_all().await;
}

/// An `agent` method reaches the stable `authenticate` request with the id
/// the client selected, and the state is read again afterwards.
#[tokio::test]
async fn agent_method_authenticate_succeeds_and_refreshes_state() {
    let harness = Harness::new(&[("full", "auth")]);
    let (status, body) = harness
        .request("POST", "/api/agents/full/auth/api-key")
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["agent_id"], "full");
    let sent = harness.recorded("full", "authenticate.json").unwrap();
    assert_eq!(sent.as_array().unwrap().len(), 1);
    assert_eq!(sent[0]["methodId"], "api-key");
    // One process ran `authenticate`, and a second read the refreshed state.
    let initializes = harness.recorded("full", "initialize.json").unwrap();
    assert_eq!(initializes.as_array().unwrap().len(), 2);
    harness.sessions.shutdown_all().await;
}

/// An agent that rejects the method reports a failure. Batey never
/// reports success it did not get.
#[tokio::test]
async fn agent_method_authenticate_error_is_reported() {
    let harness = Harness::new(&[("full", "auth")]);
    let (status, body) = harness
        .request("POST", "/api/agents/full/auth/api-key-broken")
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("Key rejected"),
        "{body}"
    );
    harness.sessions.shutdown_all().await;
}

/// The stable schema forbids `authenticate` for a terminal method. The
/// request is refused, and no `authenticate` ever reaches the agent.
#[tokio::test]
async fn terminal_methods_never_reach_authenticate() {
    let harness = Harness::new(&[("full", "auth")]);
    let (status, body) = harness.request("POST", "/api/agents/full/auth/tui").await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("terminal"),
        "{body}"
    );
    assert!(harness.recorded("full", "authenticate.json").is_none());
    harness.sessions.shutdown_all().await;
}

/// An unknown method kind is reported as unsupported. Guessing `agent` would
/// send the wrong request.
#[tokio::test]
async fn unknown_method_kinds_are_refused_without_a_fallback() {
    let harness = Harness::new(&[("full", "auth")]);
    let (status, body) = harness
        .request("POST", "/api/agents/full/auth/future")
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("browser-popup"),
        "{body}"
    );
    assert!(harness.recorded("full", "authenticate.json").is_none());

    // A method the agent never advertised is not found.
    let (status, _) = harness
        .request("POST", "/api/agents/full/auth/invented")
        .await;
    assert_eq!(status, 404);
    assert!(harness.recorded("full", "authenticate.json").is_none());
    harness.sessions.shutdown_all().await;
}

/// Logout goes out only when the agent advertised the capability.
#[tokio::test]
async fn logout_is_capability_gated() {
    let harness = Harness::new(&[("full", "auth"), ("plain", "auth-no-logout")]);
    let (status, body) = harness.request("POST", "/api/agents/full/logout").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        harness
            .recorded("full", "logout.json")
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let (status, body) = harness.request("POST", "/api/agents/plain/logout").await;
    assert_eq!(status, 409, "{body}");
    assert!(harness.recorded("plain", "logout.json").is_none());
    harness.sessions.shutdown_all().await;
}

#[tokio::test]
async fn unknown_agents_are_not_found() {
    let harness = Harness::new(&[("full", "auth")]);
    for (method, uri) in [
        ("GET", "/api/agents/nobody/auth"),
        ("POST", "/api/agents/nobody/auth/refresh"),
        ("POST", "/api/agents/nobody/auth/api-key"),
        ("POST", "/api/agents/nobody/logout"),
        ("POST", "/api/agents/nobody/auth/terminal/tui"),
        ("GET", "/api/agent-auth/does-not-exist"),
        ("POST", "/api/agent-auth/does-not-exist/cancel"),
    ] {
        let (status, body) = harness.request(method, uri).await;
        assert_eq!(status, 404, "{method} {uri} returned {status}: {body}");
    }
    harness.sessions.shutdown_all().await;
}

/// An agent that answers `auth_required` produces a recoverable structured
/// state. The chat row, its workspace, and its history all survive.
#[tokio::test]
async fn auth_required_is_structured_and_keeps_durable_chat_data() {
    let harness = Harness::new(&[("gated", "auth-required")]);
    let project = harness
        .hub
        .create_project("demo".into(), harness.root.display().to_string())
        .unwrap();
    let chat = harness
        .hub
        .create_chat(&project.id, "gated", None)
        .await
        .unwrap();

    let (status, body) = harness
        .request("POST", &format!("/api/chats/{}/resume", chat.chat.id))
        .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], "auth_required");
    assert_eq!(body["details"]["agent_id"], "gated");

    // The failure is recoverable: nothing durable was destroyed.
    let (status, reread) = harness
        .request("GET", &format!("/api/chats/{}", chat.chat.id))
        .await;
    assert_eq!(status, 200, "{reread}");
    assert_eq!(reread["id"], chat.chat.id);
    let (status, history) = harness
        .request("GET", &format!("/api/chats/{}/history", chat.chat.id))
        .await;
    assert_eq!(status, 200, "{history}");
    // A plain cache-only read already carries the observed evidence, with
    // no process spawned for it.
    let (status, auth) = harness.request("GET", "/api/agents/gated/auth").await;
    assert_eq!(status, 200, "{auth}");
    assert_eq!(auth["observed_state"], "authentication_required");
    // The agent still advertises how to authenticate; an explicit check
    // discovers the methods.
    let (status, refreshed) = harness
        .request("POST", "/api/agents/gated/auth/refresh")
        .await;
    assert_eq!(status, 200, "{refreshed}");
    assert!(!refreshed["methods"].as_array().unwrap().is_empty());
    harness.sessions.shutdown_all().await;
}

/// A shared in-memory sink that captures every tracing event, so a test can
/// prove what Batey logs and what it never logs.
fn log_capture() -> &'static std::sync::Mutex<Vec<u8>> {
    static BUFFER: OnceLock<std::sync::Mutex<Vec<u8>>> = OnceLock::new();
    static SUBSCRIBER: Once = Once::new();
    let buffer = BUFFER.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    SUBSCRIBER.call_once(|| {
        let _ = tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(Capture(buffer))
                .finish(),
        );
    });
    buffer
}

struct Capture(&'static std::sync::Mutex<Vec<u8>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        Capture(self.0)
    }
}

/// The captured log lines that carry the fake agent's stderr marker.
fn stderr_marker_lines() -> Vec<String> {
    let buffer = log_capture();
    String::from_utf8_lossy(&buffer.lock().unwrap())
        .lines()
        .filter(|line| line.contains("BATEY_TEST_STDERR_SECRET"))
        .map(|line| line.to_owned())
        .collect()
}

/// A secret line on authentication stderr never reaches the log, while the
/// same line from an ordinary chat agent does. Both processes run the same
/// fake agent, so the difference proves the per-process stderr policy.
#[tokio::test]
async fn authentication_stderr_is_discarded_but_chat_stderr_is_logged() {
    // Installs the capturing subscriber before any child can log.
    log_capture();
    let harness = Harness::new(&[("full", "auth")]);

    // The ordinary chat path. The same helper prints the marker at
    // `initialize`, and chat-agent stderr is diagnostic material.
    let project = harness
        .hub
        .create_project("demo".into(), harness.root.display().to_string())
        .unwrap();
    let chat = harness
        .hub
        .create_chat(&project.id, "full", None)
        .await
        .unwrap();
    let (status, body) = harness
        .request("POST", &format!("/api/chats/{}/resume", chat.chat.id))
        .await;
    assert_eq!(status, 200, "{body}");

    // The chat agent's marker must appear. This proves the capture works
    // before the absence assertion below means anything.
    let mut logged_somewhere = false;
    for _ in 0..200 {
        if !stderr_marker_lines().is_empty() {
            logged_somewhere = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        logged_somewhere,
        "no chat agent stderr reached the log at all"
    );

    // The authentication probe path. The same stderr line must stay out of
    // the log entirely. A plain read never probes, so this exercises the
    // explicit refresh operation instead.
    let (status, body) = harness
        .request("POST", "/api/agents/full/auth/refresh")
        .await;
    assert_eq!(status, 200, "{body}");

    // Re-check for a bounded window, so a hypothetical late drain task that
    // still logged would be caught.
    for _ in 0..40 {
        let leaked: Vec<String> = stderr_marker_lines()
            .into_iter()
            .filter(|line| line.contains("agent-auth"))
            .collect();
        assert!(
            leaked.is_empty(),
            "authentication stderr reached the log: {leaked:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    harness.sessions.shutdown_all().await;
}

/// Every authentication route sits behind the shared web authentication
/// middleware, including the flow socket.
#[tokio::test]
async fn web_authentication_middleware_protects_every_auth_route() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&root.path().join("hub.db")).unwrap());
    let events = Arc::new(EventLog::persistent(store.clone()).unwrap());
    let history = root.path().join("full");
    std::fs::create_dir_all(&history).unwrap();
    let agents = Arc::new(AgentRegistry::new([agent("full", &history, "auth")]));
    let sessions = SessionManager::with_store(agents.clone(), events, Some(store));
    let mut config = Config {
        agents: agents.clone(),
        ..Default::default()
    };
    config.web.auth_token = Some("secret-token".into());
    config.web.project_roots = vec![root.path().display().to_string()];
    let app = router(AppState::new(sessions.clone(), Arc::new(config), 8765));

    let routes = [
        ("GET", "/api/agents/full/auth"),
        ("POST", "/api/agents/full/auth/refresh"),
        ("POST", "/api/agents/full/auth/api-key"),
        ("POST", "/api/agents/full/auth/protocol/api-key"),
        ("POST", "/api/agents/full/logout"),
        ("POST", "/api/agents/full/auth/terminal/tui"),
        ("GET", "/api/agent-auth/any-flow"),
        ("POST", "/api/agent-auth/any-flow/cancel"),
        ("GET", "/api/agent-auth/any-flow/ws"),
        ("GET", "/api/protocol-auth/any-flow"),
        ("POST", "/api/protocol-auth/any-flow/cancel"),
        ("GET", "/api/protocol-auth/any-flow/elicitations"),
        (
            "POST",
            "/api/protocol-auth/any-flow/elicitations/e1/respond",
        ),
    ];
    for (method, uri) in routes {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("host", "127.0.0.1:8765")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            401,
            "{method} {uri} answered without a token"
        );
    }

    // A foreign origin is refused even with the right token.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/agents/full/auth")
                .header("host", "127.0.0.1:8765")
                .header("origin", "https://evil.example")
                .header("authorization", "Bearer secret-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    sessions.shutdown_all().await;
}

/// Observed state starts unknown and never derives from the logout capability.
#[tokio::test]
async fn observed_state_is_unknown_until_evidence() {
    let harness = Harness::new(&[("full", "auth")]);
    // Never probed: a plain read is truthfully unknown, with no methods.
    let (status, body) = harness.request("GET", "/api/agents/full/auth").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["observed_state"], "unknown");
    assert_eq!(body["freshness"], "unknown");
    assert!(body["methods"].as_array().unwrap().is_empty());

    // An explicit check discovers the logout capability, but that alone
    // never implies a login.
    let (status, body) = harness
        .request("POST", "/api/agents/full/auth/refresh")
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["observed_state"], "unknown");
    assert_eq!(body["logout_supported"], true);
    harness.sessions.shutdown_all().await;
}

/// A successful `agent` method records observed authenticated; a logout
/// records authentication_required. The view carries both halves.
#[tokio::test]
async fn observed_state_follows_authenticate_and_logout() {
    let harness = Harness::new(&[("full", "auth")]);
    let (status, body) = harness
        .request("POST", "/api/agents/full/auth/api-key")
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["observed_state"], "authenticated");

    let (status, body) = harness.request("POST", "/api/agents/full/logout").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["observed_state"], "authentication_required");
    harness.sessions.shutdown_all().await;
}

/// Stable `auth_required` records observed authentication_required while the
/// chat stays intact.
#[tokio::test]
async fn auth_required_records_observed_state() {
    let harness = Harness::new(&[("gated", "auth-required")]);
    let (status, body) = harness.request("GET", "/api/agents/gated/auth").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["observed_state"], "unknown");

    let project = harness
        .hub
        .create_project("demo".into(), harness.root.display().to_string())
        .unwrap();
    let chat = harness
        .hub
        .create_chat(&project.id, "gated", None)
        .await
        .unwrap();
    let (status, _) = harness
        .request("POST", &format!("/api/chats/{}/resume", chat.chat.id))
        .await;
    assert_eq!(status, 409);

    let (status, body) = harness.request("GET", "/api/agents/gated/auth").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["observed_state"], "authentication_required");
    harness.sessions.shutdown_all().await;
}

/// A legacy OpenCode descriptor becomes a terminal flow, never `authenticate`.
#[tokio::test]
async fn legacy_opencode_bridge_runs_in_a_terminal() {
    let harness = Harness::new(&[("legacy", "auth-legacy-opencode")]);
    let (status, body) = harness
        .request("POST", "/api/agents/legacy/auth/refresh")
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["methods"][0]["type"], "terminal");
    assert_eq!(body["methods"][0]["supported"], TERMINAL_AUTH_SUPPORTED);
    assert_eq!(body["observed_state"], "unknown");

    // The legacy method must not fall through to ordinary authenticate.
    let (status, body) = harness
        .request("POST", "/api/agents/legacy/auth/opencode-login")
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(harness.recorded("legacy", "authenticate.json").is_none());

    // The client advertised the legacy bridge capability.
    let recorded = harness.recorded("legacy", "initialize.json").unwrap();
    assert_eq!(recorded[0]["_meta"]["terminal-auth"], Value::Bool(true));
    harness.sessions.shutdown_all().await;
}

/// A legacy Copilot descriptor becomes a terminal flow with its own command.
#[tokio::test]
async fn legacy_copilot_bridge_runs_in_a_terminal() {
    let harness = Harness::new(&[("legacy", "auth-legacy-copilot")]);
    let (status, body) = harness
        .request("POST", "/api/agents/legacy/auth/refresh")
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["methods"][0]["type"], "terminal");

    let (status, body) = harness
        .request("POST", "/api/agents/legacy/auth/copilot-login")
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(harness.recorded("legacy", "authenticate.json").is_none());
    harness.sessions.shutdown_all().await;
}

/// A protocol flow surfaces a request-scoped URL elicitation and completes
/// on accept without leaking secrets to durable chat history.
#[tokio::test]
async fn protocol_flow_serves_a_url_elicitation() {
    let harness = Harness::new(&[("codex", "auth-codex-url")]);
    let (status, flow) = harness
        .request("POST", "/api/agents/codex/auth/protocol/codex-oauth")
        .await;
    assert_eq!(status, 200, "{flow}");
    let flow_id = flow["flow_id"].as_str().unwrap().to_owned();
    assert_eq!(flow_id.len(), 64);

    // The flow waits for explicit user action instead of hanging in running.
    let mut saw_waiting = false;
    for _ in 0..100 {
        let (status, view) = harness
            .request("GET", &format!("/api/protocol-auth/{flow_id}"))
            .await;
        assert_eq!(status, 200, "{view}");
        if view["state"] == "waiting_for_user" {
            saw_waiting = true;
            break;
        }
        if view["state"] == "succeeded" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        saw_waiting,
        "the URL step never surfaced as waiting_for_user"
    );

    let (status, elicitations) = harness
        .request("GET", &format!("/api/protocol-auth/{flow_id}/elicitations"))
        .await;
    assert_eq!(status, 200, "{elicitations}");
    let list = elicitations.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["mode"], "url");
    let url = list[0]["url"].as_str().unwrap();
    assert!(url.contains("example.invalid"), "{url}");
    assert!(url.contains("ABCD-1234"), "the complete URL must be shown");

    // The elicitation id is flow-scoped; resolve it from the list.
    let eid = list[0]["id"].as_str().unwrap().to_owned();
    let (status, body) = harness
        .request_with_body(
            "POST",
            &format!("/api/protocol-auth/{flow_id}/elicitations/{eid}/respond"),
            Some(serde_json::json!({"action": "accept"})),
        )
        .await;
    assert_eq!(status, 200, "{body}");

    let mut succeeded = false;
    for _ in 0..100 {
        let (status, view) = harness
            .request("GET", &format!("/api/protocol-auth/{flow_id}"))
            .await;
        assert_eq!(status, 200, "{view}");
        if view["state"] == "succeeded" {
            succeeded = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(succeeded, "the URL flow never succeeded after accept");

    let (status, body) = harness.request("GET", "/api/agents/codex/auth").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["observed_state"], "authenticated");
    let _ = (status, body);
    harness.sessions.shutdown_all().await;
}

/// Cancelling a protocol flow ends it promptly; it never sticks in running.
#[tokio::test]
async fn protocol_flow_cancel_ends_promptly() {
    let harness = Harness::new(&[("anti", "auth-antigravity")]);
    let (status, flow) = harness
        .request(
            "POST",
            "/api/agents/anti/auth/protocol/antigravity-interactive",
        )
        .await;
    assert_eq!(status, 200, "{flow}");
    let flow_id = flow["flow_id"].as_str().unwrap().to_owned();

    let (status, view) = harness
        .request("POST", &format!("/api/protocol-auth/{flow_id}/cancel"))
        .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["state"], "cancelled");

    let (status, view) = harness
        .request("GET", &format!("/api/protocol-auth/{flow_id}"))
        .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["state"], "cancelled");
    harness.sessions.shutdown_all().await;
}

/// An active protocol flow is discoverable through the auth view, so a page
/// reload can resume polling and cancel it. Discovery never exposes the URL
/// or any other sensitive auth material.
#[tokio::test]
async fn active_protocol_flow_is_discoverable_and_safe() {
    let harness = Harness::new(&[("codex", "auth-codex-url")]);
    let (status, flow) = harness
        .request("POST", "/api/agents/codex/auth/protocol/codex-oauth")
        .await;
    assert_eq!(status, 200, "{flow}");
    let flow_id = flow["flow_id"].as_str().unwrap().to_owned();

    let mut body = Value::Null;
    for _ in 0..100 {
        let (status, view) = harness.request("GET", "/api/agents/codex/auth").await;
        assert_eq!(status, 200, "{view}");
        body = view;
        if body["active_flow"]["state"] == "waiting_for_user" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let active = &body["active_flow"];
    assert_eq!(active["kind"], "protocol", "{body}");
    assert_eq!(active["flow_id"], flow_id, "{body}");
    assert_eq!(active["method_id"], "codex-oauth", "{body}");
    assert!(active["started_at"].is_string(), "{body}");

    // Discovery carries no URL, code, token, or terminal output.
    let serialized = body.to_string();
    assert!(!serialized.contains("example.invalid"), "{serialized}");
    assert!(!serialized.contains("ABCD-1234"), "{serialized}");
    assert!(body.get("output").is_none(), "{body}");

    // Cancel is available throughout the wait and clears discovery.
    let (status, cancelled) = harness
        .request("POST", &format!("/api/protocol-auth/{flow_id}/cancel"))
        .await;
    assert_eq!(status, 200, "{cancelled}");
    let (status, after) = harness.request("GET", "/api/agents/codex/auth").await;
    assert_eq!(status, 200, "{after}");
    assert!(
        after.get("active_flow").is_none() || after["active_flow"].is_null(),
        "a cancelled flow stayed discoverable: {after}"
    );
    harness.sessions.shutdown_all().await;
}

/// An active terminal flow is discoverable and resumable: reopening reads the
/// existing flow instead of starting a second process.
#[tokio::test]
async fn active_terminal_flow_is_discoverable_and_resumable() {
    if !TERMINAL_AUTH_SUPPORTED {
        return;
    }
    let harness = Harness::new(&[("full", "auth")]);
    let (status, flow) = harness
        .request("POST", "/api/agents/full/auth/terminal/tui")
        .await;
    assert_eq!(status, 200, "{flow}");
    let flow_id = flow["flow_id"].as_str().unwrap().to_owned();

    let (status, view) = harness.request("GET", "/api/agents/full/auth").await;
    assert_eq!(status, 200, "{view}");
    let active = &view["active_flow"];
    assert_eq!(active["kind"], "terminal", "{view}");
    assert_eq!(active["flow_id"], flow_id, "{view}");
    assert_eq!(active["method_id"], "tui", "{view}");
    assert_eq!(active["state"], "running", "{view}");

    // Reopening fetches the same running flow.
    let (status, resumed) = harness
        .request("GET", &format!("/api/agent-auth/{flow_id}"))
        .await;
    assert_eq!(status, 200, "{resumed}");
    assert_eq!(resumed["flow_id"], flow_id);
    assert_eq!(resumed["state"], "running");
    // The active flow still blocks a second start.
    let (status, conflict) = harness
        .request("POST", "/api/agents/full/auth/terminal/tui")
        .await;
    assert_eq!(status, 409, "{conflict}");

    let (status, cancelled) = harness
        .request("POST", &format!("/api/agent-auth/{flow_id}/cancel"))
        .await;
    assert_eq!(status, 200, "{cancelled}");
    assert_eq!(cancelled["state"], "cancelled");
    harness.sessions.shutdown_all().await;
}

// -------------------------------------------------- T140 caching/status core

/// A plain cache-only read never starts a process, no matter how many times
/// it is repeated. This agent was never explicitly checked, so it truthfully
/// reports `unknown` with no methods instead of guessing or probing.
#[tokio::test]
async fn plain_reads_never_spawn_a_process() {
    let harness = Harness::new(&[("full", "auth")]);
    for _ in 0..5 {
        let (status, body) = harness.request("GET", "/api/agents/full/auth").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["freshness"], "unknown");
        assert!(body["methods"].as_array().unwrap().is_empty());
    }
    assert!(
        harness.recorded("full", "initialize.json").is_none(),
        "a plain read spawned an agent process"
    );
    harness.sessions.shutdown_all().await;
}

/// An explicit refresh populates the durable cache, and a later plain read
/// answers from it without spawning another process.
#[tokio::test]
async fn explicit_refresh_populates_the_cache_for_later_plain_reads() {
    let harness = Harness::new(&[("full", "auth")]);
    let (status, refreshed) = harness
        .request("POST", "/api/agents/full/auth/refresh")
        .await;
    assert_eq!(status, 200, "{refreshed}");
    assert_eq!(refreshed["freshness"], "fresh");
    assert!(!refreshed["methods"].as_array().unwrap().is_empty());

    let initializes_after_refresh = harness
        .recorded("full", "initialize.json")
        .unwrap()
        .as_array()
        .unwrap()
        .len();

    let (status, cached) = harness.request("GET", "/api/agents/full/auth").await;
    assert_eq!(status, 200, "{cached}");
    assert_eq!(cached["freshness"], "cached");
    assert_eq!(cached["methods"], refreshed["methods"]);

    let initializes_after_read = harness
        .recorded("full", "initialize.json")
        .unwrap()
        .as_array()
        .unwrap()
        .len();
    assert_eq!(
        initializes_after_refresh, initializes_after_read,
        "a plain read after a refresh spawned another process"
    );
    harness.sessions.shutdown_all().await;
}

/// Concurrent explicit refreshes for one agent coalesce into a single
/// probe: repeated clicks or racing requests never spawn duplicates.
#[tokio::test]
async fn concurrent_refreshes_result_in_one_probe() {
    let harness = Harness::new(&[("full", "auth-slow")]);
    let (a, b, c, d, e) = tokio::join!(
        harness.request("POST", "/api/agents/full/auth/refresh"),
        harness.request("POST", "/api/agents/full/auth/refresh"),
        harness.request("POST", "/api/agents/full/auth/refresh"),
        harness.request("POST", "/api/agents/full/auth/refresh"),
        harness.request("POST", "/api/agents/full/auth/refresh"),
    );
    for (status, body) in [a, b, c, d, e] {
        assert_eq!(status, 200, "{body}");
        assert!(!body["methods"].as_array().unwrap().is_empty(), "{body}");
    }
    let initializes = harness
        .recorded("full", "initialize.json")
        .unwrap()
        .as_array()
        .unwrap()
        .len();
    assert_eq!(
        initializes, 1,
        "concurrent refreshes spawned more than one probe"
    );
    harness.sessions.shutdown_all().await;
}

/// The discovery cache survives a restart. Durable evidence comes back, but
/// it is never shown as a fresh check that never happened this session: a
/// plain read in the new process still spawns nothing.
#[tokio::test]
async fn restart_preserves_the_cache_as_historical_evidence() {
    let tempdir = tempfile::tempdir().unwrap();
    let harness = Harness::at(tempdir.path(), &[("full", "auth")]);
    let (status, body) = harness
        .request("POST", "/api/agents/full/auth/api-key")
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["observed_state"], "authenticated");
    let initializes_before_restart = harness
        .recorded("full", "initialize.json")
        .unwrap()
        .as_array()
        .unwrap()
        .len();
    harness.sessions.shutdown_all().await;
    drop(harness);

    let harness = Harness::at(tempdir.path(), &[("full", "auth")]);
    let (status, body) = harness.request("GET", "/api/agents/full/auth").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["observed_state"], "authenticated");
    assert_ne!(body["freshness"], "fresh");

    let initializes_after_read = harness
        .recorded("full", "initialize.json")
        .unwrap()
        .as_array()
        .unwrap()
        .len();
    assert_eq!(
        initializes_before_restart, initializes_after_read,
        "the restarted process spawned an agent just to answer a plain read"
    );
    harness.sessions.shutdown_all().await;
}

/// A mutation that can change initialization or authentication methods
/// marks the cache stale without erasing it: the last known methods stay
/// visible as historical evidence, and staleness alone never spawns a probe.
#[tokio::test]
async fn environment_changes_mark_the_cache_stale_without_erasing_it() {
    let harness = Harness::new(&[("full", "auth")]);
    let store = harness.sessions.store.as_ref().unwrap();
    let record = InstalledAgent::new("full".into(), AgentSource::Registry, "python3".into());
    store.insert_agent(&record).unwrap();

    let (status, refreshed) = harness
        .request("POST", "/api/agents/full/auth/refresh")
        .await;
    assert_eq!(status, 200, "{refreshed}");
    assert_eq!(refreshed["freshness"], "fresh");
    let methods_before = refreshed["methods"].clone();

    harness
        .hub
        .update_agent_env(
            "full",
            vec![AgentEnvEdit {
                name: "SOME_VALUE".into(),
                value: Some("1".into()),
                action: AgentEnvAction::Replace,
            }],
        )
        .await
        .unwrap();

    let initializes_before_read = harness
        .recorded("full", "initialize.json")
        .unwrap()
        .as_array()
        .unwrap()
        .len();

    let (status, stale) = harness.request("GET", "/api/agents/full/auth").await;
    assert_eq!(status, 200, "{stale}");
    assert_eq!(stale["freshness"], "stale");
    // The last known methods stay visible as historical evidence.
    assert_eq!(stale["methods"], methods_before);

    let initializes_after_read = harness
        .recorded("full", "initialize.json")
        .unwrap()
        .as_array()
        .unwrap()
        .len();
    assert_eq!(
        initializes_before_read, initializes_after_read,
        "staleness alone spawned a probe"
    );
    harness.sessions.shutdown_all().await;
}

/// A failed explicit probe never erases a useful cache and never makes the
/// agent look unusable: the last known methods come back, alongside the
/// refresh error.
#[tokio::test]
async fn a_failed_probe_preserves_the_existing_cache() {
    let harness = Harness::new(&[("full", "auth")]);
    let (status, refreshed) = harness
        .request("POST", "/api/agents/full/auth/refresh")
        .await;
    assert_eq!(status, 200, "{refreshed}");
    assert!(!refreshed["methods"].as_array().unwrap().is_empty());
    let methods_before = refreshed["methods"].clone();

    // Swap in a command that cannot start, simulating an agent that has
    // gone unavailable since it was last checked.
    let broken = AgentDefinition::new("full", "/no/such/binary-does-not-exist");
    harness.agents.replace(broken).unwrap();

    let (status, failed) = harness
        .request("POST", "/api/agents/full/auth/refresh")
        .await;
    assert_eq!(status, 200, "{failed}");
    assert!(failed["refresh_error"].is_string(), "{failed}");
    // The last known methods stay useful, not erased by the failure.
    assert_eq!(failed["methods"], methods_before);

    // A plain read afterward is just as unaffected: the agent is not
    // reported as unusable.
    let (status, after) = harness.request("GET", "/api/agents/full/auth").await;
    assert_eq!(status, 200, "{after}");
    assert_eq!(after["methods"], methods_before);
    harness.sessions.shutdown_all().await;
}
