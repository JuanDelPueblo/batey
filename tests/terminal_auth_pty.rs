//! Terminal authentication over a real PTY.
//!
//! These tests start real processes. They prove the exact invocation, the
//! interactive behavior, the bounds, and the cleanup that the stable terminal
//! authentication method requires.
#![cfg(unix)]

use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use batey::{
    agents::{
        which, AgentDefinition, AgentRegistry, AgentSource, InstalledAgent, InstalledDistribution,
        RegistrySnapshot,
    },
    auth::{AgentAuthService, MAX_SCROLLBACK_BYTES},
    config::{BateyPaths, Config, PathOverrides},
    events::EventLog,
    service::HubService,
    session::SessionManager,
    store::Store,
    web::{router, AppState},
    workspace_env::take_secret_env,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{path::Path, path::PathBuf, sync::Arc, time::Duration};
use tokio::net::TcpListener;
use tower::ServiceExt;

const SHARED_ENV: &str = "BATEY_TEST_SHARED_ENV";

fn agent(id: &str, history: &Path, pass_env: &[&str]) -> AgentDefinition {
    AgentDefinition::new(id, "python3")
        .with_args(vec![
            format!("{}/tests/fake_acp.py", env!("CARGO_MANIFEST_DIR")),
            history.display().to_string(),
            "auth".into(),
        ])
        .with_pass_env(pass_env.iter().map(|name| name.to_string()).collect())
}

fn legacy_agent(id: &str, history: &Path, mode: &str) -> AgentDefinition {
    AgentDefinition::new(id, "python3").with_args(vec![
        format!("{}/tests/fake_acp.py", env!("CARGO_MANIFEST_DIR")),
        history.display().to_string(),
        mode.into(),
    ])
}

/// Writes an executable stub that records its invocation and behaves like a
/// login TUI: `ok` succeeds, `fail` fails, and a descendant proves tree kill.
fn write_legacy_stub(path: &Path) {
    let script = r#"#!/usr/bin/env python3
import json, os, pathlib, signal, subprocess, sys
out = pathlib.Path(sys.argv[0]).parent
# The stub path is <history>/<name>; record beside it like the fake ACP does.
history = out
(history / "invocation.json").write_text(json.dumps({
    "argv": [sys.argv[0]] + sys.argv[1:],
    "cwd": os.getcwd(),
    "env": dict(os.environ),
    "isatty": sys.stdin.isatty(),
}))
def report_size(*_):
    try:
        size = os.get_terminal_size(sys.stdin.fileno())
        print(f"size:{size.columns}x{size.lines}", flush=True)
    except Exception:
        pass
signal.signal(signal.SIGWINCH, report_size)
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(600)"])
(history / "child.pid").write_text(str(child.pid))
print("ready", flush=True)
while True:
    line = sys.stdin.readline()
    if not line:
        sys.exit(7)
    command = line.strip()
    if command == "ok":
        print("login-complete", flush=True)
        sys.exit(0)
    if command == "fail":
        sys.exit(3)
    print(f"echo:{command}", flush=True)
"#;
    std::fs::write(path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }
}

struct Harness {
    app: axum::Router,
    hub: Arc<HubService>,
    sessions: Arc<SessionManager>,
    auth: Arc<AgentAuthService>,
    port: u16,
    root: PathBuf,
    _server: tokio::task::JoinHandle<()>,
}

impl Harness {
    /// A running server, so a real WebSocket client can attach to a flow.
    ///
    /// Every agent records what it observed under `<root>/<agent id>`, so a
    /// test can read the exact invocation each process received.
    async fn start(root: &Path, agents: Vec<AgentDefinition>) -> Self {
        let paths = BateyPaths::from_overrides(PathOverrides {
            database: Some(root.join("hub.db")),
            data_dir: Some(root.join("data")),
            state_dir: Some(root.join("state")),
            ..Default::default()
        });
        let store = Arc::new(Store::open(&paths.database).unwrap());
        let events = Arc::new(EventLog::persistent(store.clone()).unwrap());
        let catalog = Arc::new(AgentRegistry::new(agents));
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
        config.web.project_roots = vec![root.display().to_string()];
        let config = Arc::new(config);
        let hub = HubService::new(store, sessions.clone(), catalog, &config);
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = router(AppState::new(sessions.clone(), config, port));
        let served = app.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, served).await.unwrap();
        });
        Self {
            app,
            hub,
            sessions,
            auth,
            port,
            root: root.to_path_buf(),
            _server: server,
        }
    }

    fn history(&self, agent_id: &str) -> PathBuf {
        self.root.join(agent_id)
    }

    async fn request(&self, method: &str, uri: &str) -> (u16, Value) {
        let response = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("host", format!("127.0.0.1:{}", self.port))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status().as_u16();
        let bytes = to_bytes(response.into_body(), 4_000_000).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn start_flow(&self, agent_id: &str) -> String {
        self.start_flow_for(agent_id, "tui").await
    }

    async fn start_flow_for(&self, agent_id: &str, method_id: &str) -> String {
        let (status, body) = self
            .request(
                "POST",
                &format!("/api/agents/{agent_id}/auth/terminal/{method_id}"),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["state"], "running");
        body["flow_id"].as_str().unwrap().to_owned()
    }

    async fn connect(
        &self,
        flow_id: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let url = format!("ws://127.0.0.1:{}/api/agent-auth/{flow_id}/ws", self.port);
        tokio_tungstenite::connect_async(url).await.unwrap().0
    }

    /// The invocation the PTY process recorded for one agent.
    async fn invocation(&self, agent_id: &str) -> Value {
        let path = self.history(agent_id).join("invocation.json");
        for _ in 0..200 {
            if path.exists() {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if let Ok(value) = serde_json::from_str(&text) {
                        return value;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the terminal authentication process never recorded its invocation");
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn send(socket: &mut Socket, payload: Value) {
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            payload.to_string(),
        ))
        .await
        .unwrap();
}

/// Reads flow messages until one matches, or the deadline passes.
async fn wait_for(socket: &mut Socket, mut matches: impl FnMut(&Value) -> bool) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let message = tokio::time::timeout_at(deadline, socket.next())
            .await
            .expect("flow socket went quiet")
            .expect("flow socket closed")
            .unwrap();
        let Ok(text) = message.into_text() else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if matches(&value) {
            return value;
        }
    }
}

async fn wait_for_state(socket: &mut Socket, state: &str) -> Value {
    wait_for(socket, |value| {
        value["type"] == "state" && value["state"] == state
    })
    .await
}

async fn wait_for_output(socket: &mut Socket, needle: &str) -> String {
    let mut seen = String::new();
    wait_for(socket, |value| {
        if value["type"] == "output" {
            seen.push_str(value["data"].as_str().unwrap_or_default());
        }
        seen.contains(needle)
    })
    .await;
    seen
}

fn process_is_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only probes for the process; it delivers nothing.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// The invocation reproduces the installed runtime plus the advertised
/// method, exactly: same executable, same base arguments, method arguments
/// appended, and method environment overriding the base environment.
#[tokio::test]
async fn terminal_auth_reproduces_the_exact_advertised_invocation() {
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path();
    let history = root.join("demo");
    std::fs::create_dir_all(&history).unwrap();
    unsafe {
        std::env::set_var(SHARED_ENV, "from-base");
    }
    let harness = Harness::start(
        root,
        vec![
            agent("demo", &history, &[]).with_env(std::collections::HashMap::from([(
                "BATEY_TEST_LAUNCH_ENV".to_string(),
                "from-launch".to_string(),
            )])),
        ],
    )
    .await;
    let flow_id = harness.start_flow("demo").await;
    let invocation = harness.invocation("demo").await;

    let argv: Vec<String> = invocation["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_owned())
        .collect();
    // argv[0] is the script the base arguments named; the base executable is
    // `python3` and stays unchanged.
    assert!(argv[0].ends_with("tests/fake_acp.py"), "{argv:?}");
    assert_eq!(argv[2], "auth", "base arguments changed: {argv:?}");
    // The advertised method arguments follow the base arguments, in order.
    assert_eq!(argv[3], "terminal-auth", "{argv:?}");
    assert_eq!(argv.len(), 5, "{argv:?}");

    let env = &invocation["env"];
    // The sanitized base environment reached the process.
    assert!(env.get("PATH").is_some(), "the base environment is missing");
    // The base launch environment survives.
    assert_eq!(env["BATEY_TEST_LAUNCH_ENV"], "from-launch");
    // The method environment overrides the same name in the base.
    assert_eq!(env[SHARED_ENV], "from-method");
    assert_eq!(env["BATEY_TEST_METHOD_ENV"], "from-method");
    // Batey owns the working directory; it is neither a project path nor
    // anything a client chose.
    let cwd = invocation["cwd"].as_str().unwrap();
    assert!(
        cwd.ends_with("agent-auth"),
        "the flow ran outside the Batey working directory: {cwd}"
    );
    // The program really got a terminal.
    assert_eq!(invocation["isatty"], true);

    harness.auth.cancel_terminal_flow(&flow_id).unwrap();
    harness.auth.shutdown();
    harness.sessions.shutdown_all().await;
    unsafe {
        std::env::remove_var(SHARED_ENV);
    }
}

/// One agent's terminal authentication process never observes another
/// agent's stashed secret.
#[tokio::test]
async fn terminal_auth_keeps_per_agent_secret_isolation() {
    const SECRET_A: &str = "BATEY_TEST_TERMINAL_SECRET_A";
    const SECRET_B: &str = "BATEY_TEST_TERMINAL_SECRET_B";
    unsafe {
        std::env::set_var(SECRET_A, "value-a");
        std::env::set_var(SECRET_B, "value-b");
    }
    let stash = take_secret_env(&[SECRET_A.to_string(), SECRET_B.to_string()]);

    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path();
    let history_a = root.join("agent-a");
    let history_b = root.join("agent-b");
    std::fs::create_dir_all(&history_a).unwrap();
    std::fs::create_dir_all(&history_b).unwrap();
    let harness = Harness::start(
        root,
        vec![
            agent("agent-a", &history_a, &[SECRET_A]),
            agent("agent-b", &history_b, &[SECRET_B]),
        ],
    )
    .await;
    harness.sessions.set_secret_env(stash);

    let flow_a = harness.start_flow("agent-a").await;
    // The probe process that read the advertised methods obeys the same rule.
    let probe: Value =
        serde_json::from_str(&std::fs::read_to_string(history_a.join("probe-env.json")).unwrap())
            .unwrap();
    assert_eq!(probe[SECRET_A], "value-a");
    assert!(
        probe.get(SECRET_B).is_none(),
        "the probe process observed the other agent's secret"
    );

    let invocation = harness.invocation("agent-a").await;
    assert_eq!(invocation["env"][SECRET_A], "value-a");
    assert!(
        invocation["env"].get(SECRET_B).is_none(),
        "agent-a observed the other agent's secret"
    );
    harness.auth.cancel_terminal_flow(&flow_a).unwrap();

    let flow_b = harness.start_flow("agent-b").await;
    let path = history_b.join("invocation.json");
    for _ in 0..200 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let invocation: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(invocation["env"][SECRET_B], "value-b");
    assert!(
        invocation["env"].get(SECRET_A).is_none(),
        "agent-b observed the other agent's secret"
    );
    harness.auth.cancel_terminal_flow(&flow_b).unwrap();
    harness.auth.shutdown();
    harness.sessions.shutdown_all().await;
}

/// Input reaches the terminal, output comes back, and a resize reaches the
/// program as a real window change.
#[tokio::test]
async fn flow_socket_carries_input_output_and_resize() {
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path();
    let history = root.join("demo");
    std::fs::create_dir_all(&history).unwrap();
    let harness = Harness::start(root, vec![agent("demo", &history, &[])]).await;
    let flow_id = harness.start_flow("demo").await;
    let mut socket = harness.connect(&flow_id).await;

    wait_for_output(&mut socket, "ready").await;
    send(&mut socket, json!({"type": "input", "data": "hello\n"})).await;
    wait_for_output(&mut socket, "echo:hello").await;

    send(
        &mut socket,
        json!({"type": "resize", "cols": 132, "rows": 43}),
    )
    .await;
    // The program reports the kernel's window size on SIGWINCH.
    wait_for_output(&mut socket, "size:132x43").await;

    harness.auth.cancel_terminal_flow(&flow_id).unwrap();
    harness.auth.shutdown();
    harness.sessions.shutdown_all().await;
}

/// A zero exit status means success, and Batey reads the agent's
/// authentication state again without sending `authenticate`.
#[tokio::test]
async fn zero_exit_succeeds_and_reinitializes_without_authenticate() {
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path();
    let history = root.join("demo");
    std::fs::create_dir_all(&history).unwrap();
    let harness = Harness::start(root, vec![agent("demo", &history, &[])]).await;
    let flow_id = harness.start_flow("demo").await;
    let initializes_before = std::fs::read_to_string(history.join("initialize.json"))
        .map(|text| {
            serde_json::from_str::<Value>(&text)
                .unwrap()
                .as_array()
                .unwrap()
                .len()
        })
        .unwrap_or(0);

    let mut socket = harness.connect(&flow_id).await;
    wait_for_output(&mut socket, "ready").await;
    send(&mut socket, json!({"type": "input", "data": "ok\n"})).await;
    // The last line the program wrote still reaches the client.
    wait_for_output(&mut socket, "login-complete").await;
    let state = wait_for_state(&mut socket, "succeeded").await;
    assert_eq!(state["exit_code"], 0);

    let (status, view) = harness
        .request("GET", &format!("/api/agent-auth/{flow_id}"))
        .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["state"], "succeeded");

    // The refresh starts the agent again and reads `initialize`. It must not
    // send `authenticate`: the stable schema forbids that for this method.
    let mut refreshed = false;
    for _ in 0..200 {
        let count = std::fs::read_to_string(history.join("initialize.json"))
            .map(|text| {
                serde_json::from_str::<Value>(&text)
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .len()
            })
            .unwrap_or(0);
        if count > initializes_before {
            refreshed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(refreshed, "the agent state was never read again");
    assert!(
        !history.join("authenticate.json").exists(),
        "terminal authentication sent the `authenticate` request"
    );
    harness.auth.shutdown();
    harness.sessions.shutdown_all().await;
}

/// Observing `succeeded` guarantees durable evidence at once, even before
/// the background re-probe finishes.
///
/// A plain cache-only read never spawns a process, so the pre-login view
/// answers `unknown` with no methods until something actually probes this
/// agent. The success hook records observed `authenticated` synchronously,
/// inside the transition, so a browser that reacts to the success
/// immediately sees that evidence without waiting for the slow background
/// re-probe. The explicit refresh endpoint, once that re-probe lands,
/// reports the post-login methods.
#[tokio::test]
async fn observing_succeeded_guarantees_durable_evidence_at_once() {
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path();
    let history = root.join("demo");
    std::fs::create_dir_all(&history).unwrap();
    let agent = AgentDefinition::new("demo", "python3").with_args(vec![
        format!("{}/tests/fake_acp.py", env!("CARGO_MANIFEST_DIR")),
        history.display().to_string(),
        "auth-slow".into(),
    ]);
    let harness = Harness::start(root, vec![agent]).await;
    let advertises_terminal = |methods: &Value| {
        methods
            .as_array()
            .unwrap()
            .iter()
            .any(|method| method["id"] == "tui")
    };

    // A plain read never probes: this agent was never checked yet.
    let (status, view) = harness.request("GET", "/api/agents/demo/auth").await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["freshness"], "unknown");
    assert!(view["methods"].as_array().unwrap().is_empty(), "{view}");

    let flow_id = harness.start_flow("demo").await;
    let mut socket = harness.connect(&flow_id).await;
    wait_for_output(&mut socket, "ready").await;
    send(&mut socket, json!({"type": "input", "data": "ok\n"})).await;
    wait_for_state(&mut socket, "succeeded").await;

    // The browser reacts to the success immediately: durable evidence is
    // available at once, from a plain cache read, with no process spawned.
    let (status, view) = harness.request("GET", "/api/agents/demo/auth").await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["observed_state"], "authenticated");

    // The stored credentials changed, so an explicit refresh (each probe is
    // slow, so a few tries are enough) no longer advertises the terminal
    // method.
    let mut refreshed = None;
    for _ in 0..10 {
        let (status, view) = harness
            .request("POST", "/api/agents/demo/auth/refresh")
            .await;
        assert_eq!(status, 200, "{view}");
        if !advertises_terminal(&view["methods"]) {
            refreshed = Some(view);
            break;
        }
    }
    let view = refreshed.expect("the post-login methods never arrived");
    assert!(!advertises_terminal(&view["methods"]), "{view}");
    harness.auth.shutdown();
    harness.sessions.shutdown_all().await;
}

/// A non-zero exit status is a failure, and it is reported as one.
#[tokio::test]
async fn non_zero_exit_fails() {
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path();
    let history = root.join("demo");
    std::fs::create_dir_all(&history).unwrap();
    let harness = Harness::start(root, vec![agent("demo", &history, &[])]).await;
    let flow_id = harness.start_flow("demo").await;
    let mut socket = harness.connect(&flow_id).await;
    wait_for_output(&mut socket, "ready").await;
    send(&mut socket, json!({"type": "input", "data": "fail\n"})).await;
    let state = wait_for_state(&mut socket, "failed").await;
    assert_eq!(state["exit_code"], 3);
    assert!(state["reason"].as_str().unwrap().contains("status 3"));
    harness.auth.shutdown();
    harness.sessions.shutdown_all().await;
}

/// Cancelling a flow kills the program and every process it started.
#[tokio::test]
async fn cancel_kills_the_whole_process_tree() {
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path();
    let history = root.join("demo");
    std::fs::create_dir_all(&history).unwrap();
    let harness = Harness::start(root, vec![agent("demo", &history, &[])]).await;
    let flow_id = harness.start_flow("demo").await;
    let mut socket = harness.connect(&flow_id).await;
    wait_for_output(&mut socket, "ready").await;

    let child_pid: i32 = std::fs::read_to_string(history.join("child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(process_is_alive(child_pid), "the descendant never started");

    let (status, body) = harness
        .request("POST", &format!("/api/agent-auth/{flow_id}/cancel"))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["state"], "cancelled");

    let mut gone = false;
    for _ in 0..200 {
        if !process_is_alive(child_pid) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(gone, "the descendant of the terminal process survived");
    harness.auth.shutdown();
    harness.sessions.shutdown_all().await;
}

/// Server shutdown ends every flow and kills every process tree.
#[tokio::test]
async fn shutdown_ends_every_flow() {
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path();
    let history = root.join("demo");
    std::fs::create_dir_all(&history).unwrap();
    let harness = Harness::start(root, vec![agent("demo", &history, &[])]).await;
    let flow_id = harness.start_flow("demo").await;
    let mut socket = harness.connect(&flow_id).await;
    wait_for_output(&mut socket, "ready").await;
    let child_pid: i32 = std::fs::read_to_string(history.join("child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    harness.auth.shutdown();
    let mut gone = false;
    for _ in 0..200 {
        if !process_is_alive(child_pid) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(gone, "shutdown left a terminal process tree running");
    assert!(harness.auth.terminal_flow_view(&flow_id).is_err());
    harness.sessions.shutdown_all().await;
}

/// Output is bounded in memory, and nothing about the flow reaches durable
/// chat events.
#[tokio::test]
async fn output_is_bounded_and_never_durable() {
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path();
    let history = root.join("demo");
    std::fs::create_dir_all(&history).unwrap();
    let harness = Harness::start(root, vec![agent("demo", &history, &[])]).await;
    let project = harness
        .hub
        .create_project("demo".into(), harness.root.display().to_string())
        .unwrap();
    let chat = harness
        .hub
        .create_chat(&project.id, "demo", None)
        .await
        .unwrap();

    let flow_id = harness.start_flow("demo").await;
    let mut socket = harness.connect(&flow_id).await;
    wait_for_output(&mut socket, "ready").await;
    send(&mut socket, json!({"type": "input", "data": "flood\n"})).await;
    wait_for_output(&mut socket, "flood-003999").await;

    // A second client sees only the retained tail, never the whole stream.
    let mut late = harness.connect(&flow_id).await;
    let first = tokio::time::timeout(Duration::from_secs(10), late.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let first: Value = serde_json::from_str(first.to_text().unwrap()).unwrap();
    assert_eq!(first["type"], "output");
    let replayed = first["data"].as_str().unwrap();
    assert!(
        replayed.len() <= MAX_SCROLLBACK_BYTES,
        "the retained scrollback grew past its bound: {} bytes",
        replayed.len()
    );
    assert!(
        !replayed.contains("flood-000000"),
        "the oldest output was never dropped"
    );

    // The flow summary reports lifecycle state, not a transcript.
    let (status, view) = harness
        .request("GET", &format!("/api/agent-auth/{flow_id}"))
        .await;
    assert_eq!(status, 200);
    assert!(view.get("output").is_none(), "{view}");
    assert!(view.get("scrollback").is_none(), "{view}");

    // No durable chat event mentions the flow or its output.
    let (status, history_page) = harness
        .request("GET", &format!("/api/chats/{}/history", chat.chat.id))
        .await;
    assert_eq!(status, 200);
    let text = history_page.to_string();
    assert!(!text.contains("flood-"), "terminal output reached history");
    assert!(!text.contains(&flow_id), "a flow id reached chat history");

    harness.auth.shutdown();
    harness.sessions.shutdown_all().await;
}

/// Concurrent flows are bounded per agent.
#[tokio::test]
async fn concurrent_flows_are_bounded_per_agent() {
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path();
    let history = root.join("demo");
    std::fs::create_dir_all(&history).unwrap();
    let harness = Harness::start(root, vec![agent("demo", &history, &[])]).await;
    let flow_id = harness.start_flow("demo").await;

    let (status, body) = harness
        .request("POST", "/api/agents/demo/auth/terminal/tui")
        .await;
    assert_eq!(status, 409, "{body}");

    // Cancelling frees the slot again.
    harness.auth.cancel_terminal_flow(&flow_id).unwrap();
    let (status, body) = harness
        .request("POST", "/api/agents/demo/auth/terminal/tui")
        .await;
    assert_eq!(status, 200, "{body}");
    harness.auth.shutdown();
    harness.sessions.shutdown_all().await;
}

/// A request body cannot change the invocation. The routes read an agent id,
/// a method id, and a flow id, so they cannot become a shell.
#[tokio::test]
async fn a_request_body_cannot_inject_a_command_path_or_environment() {
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path();
    let history = root.join("demo");
    std::fs::create_dir_all(&history).unwrap();
    let harness = Harness::start(root, vec![agent("demo", &history, &[])]).await;
    let marker = root.join("escaped");

    let response = harness
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/agents/demo/auth/terminal/tui")
                .header("host", format!("127.0.0.1:{}", harness.port))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "command": "sh",
                        "args": ["-c", format!("touch {}", marker.display())],
                        "cwd": "/etc",
                        "env": {"BATEY_TEST_INJECTED": "yes"},
                        "program": "sh",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let invocation = harness.invocation("demo").await;

    // The executable, the arguments, the working directory, and the
    // environment all came from the runtime and the advertised method.
    let argv: Vec<String> = invocation["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_owned())
        .collect();
    assert!(argv[0].ends_with("tests/fake_acp.py"), "{argv:?}");
    assert!(!argv.iter().any(|arg| arg.contains("touch")), "{argv:?}");
    assert_eq!(argv[3], "terminal-auth", "{argv:?}");
    assert_eq!(argv.len(), 5, "{argv:?}");
    assert!(invocation["cwd"].as_str().unwrap().ends_with("agent-auth"));
    assert!(invocation["env"].get("BATEY_TEST_INJECTED").is_none());
    assert!(!marker.exists(), "the request body ran a command");

    harness.auth.shutdown();
    harness.sessions.shutdown_all().await;
}

/// A non-terminal method never starts a PTY, and the flow routes accept only
/// an agent id, a method id, and a flow id.
#[tokio::test]
async fn terminal_route_refuses_non_terminal_methods() {
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path();
    let history = root.join("demo");
    std::fs::create_dir_all(&history).unwrap();
    let harness = Harness::start(root, vec![agent("demo", &history, &[])]).await;
    for (method_id, expected) in [("api-key", 400), ("future", 400), ("invented", 404)] {
        let (status, body) = harness
            .request(
                "POST",
                &format!("/api/agents/demo/auth/terminal/{method_id}"),
            )
            .await;
        assert_eq!(status, expected, "{method_id}: {body}");
    }
    assert!(
        !history.join("invocation.json").exists(),
        "a non-terminal method started a process"
    );
    harness.auth.shutdown();
    harness.sessions.shutdown_all().await;
}

/// An authentication change never destroys a chat process that is running a
/// turn, and it never destroys durable chat data.
#[tokio::test]
async fn authentication_changes_never_interrupt_an_active_turn() {
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path();
    let history = root.join("demo");
    std::fs::create_dir_all(&history).unwrap();
    let harness = Harness::start(root, vec![agent("demo", &history, &[])]).await;
    let project = harness
        .hub
        .create_project("demo".into(), harness.root.display().to_string())
        .unwrap();
    let chat = harness
        .hub
        .create_chat(&project.id, "demo", None)
        .await
        .unwrap();
    let chat_id = chat.chat.id.clone();

    // `wait` keeps the turn open until the test cancels it.
    harness
        .hub
        .prompt_chat(&chat_id, "wait".into())
        .await
        .unwrap();
    let live = harness.sessions.get_by_id(&chat_id).await.unwrap();
    for _ in 0..200 {
        if live.turn_state().await.to_string() == "PROMPTING" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(live.turn_state().await.to_string(), "PROMPTING");

    // Both authentication changes run while the turn is open.
    let (status, body) = harness.request("POST", "/api/agents/demo/logout").await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = harness
        .request("POST", "/api/agents/demo/auth/api-key")
        .await;
    assert_eq!(status, 200, "{body}");

    // The turn survived: the same process still holds it.
    assert_eq!(live.process_state().await.to_string(), "RUNNING");
    assert_eq!(live.turn_state().await.to_string(), "PROMPTING");
    harness.hub.cancel_chat(&chat_id).await.unwrap();

    // The durable chat is intact.
    let (status, view) = harness
        .request("GET", &format!("/api/chats/{chat_id}"))
        .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["id"], chat_id);

    // A stopped session is retired instead, so its next start observes the
    // new credentials. Wait for the cancelled turn to release its guard.
    for _ in 0..200 {
        if harness.hub.stop_chat(&chat_id).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let stopped = harness.sessions.get_by_id(&chat_id).await.unwrap();
    let (status, body) = harness.request("POST", "/api/agents/demo/logout").await;
    assert_eq!(status, 200, "{body}");
    let after = harness.sessions.get_by_id(&chat_id).await.unwrap();
    assert!(
        !Arc::ptr_eq(&stopped, &after),
        "the stopped session kept its stale runtime handle"
    );
    let (status, view) = harness
        .request("GET", &format!("/api/chats/{chat_id}"))
        .await;
    assert_eq!(status, 200, "{view}");

    harness.auth.shutdown();
    harness.sessions.shutdown_all().await;
}

/// Legacy OpenCode: the `_meta["terminal-auth"]` descriptor becomes an
/// interactive terminal that runs the advertised command, never `authenticate`.
#[tokio::test]
async fn legacy_opencode_runs_the_advertised_login_command() {
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path();
    let history = root.join("legacy");
    std::fs::create_dir_all(&history).unwrap();
    write_legacy_stub(&history.join("opencode-stub"));
    let harness = Harness::start(
        root,
        vec![legacy_agent("legacy", &history, "auth-legacy-opencode")],
    )
    .await;

    // Never probed yet: a plain cache-only read is truthfully unknown.
    let (status, view) = harness.request("GET", "/api/agents/legacy/auth").await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["freshness"], "unknown");
    assert!(view["methods"].as_array().unwrap().is_empty(), "{view}");
    assert_eq!(view["observed_state"], "unknown");

    let flow_id = harness.start_flow_for("legacy", "opencode-login").await;
    // The flow runs the stub from `_meta`, not the base ACP program.
    let invocation = harness.invocation("legacy").await;
    let argv: Vec<String> = invocation["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_owned())
        .collect();
    assert!(argv[0].ends_with("opencode-stub"), "{argv:?}");
    assert_eq!(argv[1..], vec!["auth".to_string(), "login".to_string()]);
    assert_eq!(invocation["isatty"], true);

    // Starting the flow force-probed the agent to validate the method live,
    // so the cache now carries the discovered legacy bridge. No false
    // success before login completes and no `authenticate` call.
    let (status, view) = harness.request("GET", "/api/agents/legacy/auth").await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["methods"][0]["type"], "terminal");
    assert_eq!(view["observed_state"], "unknown");
    assert!(
        !history.join("authenticate.json").exists(),
        "legacy terminal-auth sent `authenticate`"
    );

    let mut socket = harness.connect(&flow_id).await;
    wait_for_output(&mut socket, "ready").await;
    send(&mut socket, json!({"type": "input", "data": "ok\n"})).await;
    wait_for_output(&mut socket, "login-complete").await;
    let state = wait_for_state(&mut socket, "succeeded").await;
    assert_eq!(state["exit_code"], 0);

    // After success the observed state is authenticated and the probe ran again.
    let mut observed = false;
    for _ in 0..200 {
        let (status, view) = harness.request("GET", "/api/agents/legacy/auth").await;
        assert_eq!(status, 200, "{view}");
        if view["observed_state"] == "authenticated" {
            observed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(observed, "legacy success never recorded authenticated");
    assert!(
        !history.join("authenticate.json").exists(),
        "legacy terminal-auth sent `authenticate`"
    );
    harness.auth.shutdown();
    harness.sessions.shutdown_all().await;
}

/// Legacy Copilot: `copilot login` runs in a terminal and a fresh probe
/// follows success.
#[tokio::test]
async fn legacy_copilot_runs_copilot_login() {
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path();
    let history = root.join("legacy");
    std::fs::create_dir_all(&history).unwrap();
    write_legacy_stub(&history.join("copilot-stub"));
    let harness = Harness::start(
        root,
        vec![legacy_agent("legacy", &history, "auth-legacy-copilot")],
    )
    .await;

    let flow_id = harness.start_flow_for("legacy", "copilot-login").await;
    let invocation = harness.invocation("legacy").await;
    let argv: Vec<String> = invocation["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_owned())
        .collect();
    assert!(argv[0].ends_with("copilot-stub"), "{argv:?}");
    assert_eq!(argv[1..], vec!["login".to_string()]);

    let mut socket = harness.connect(&flow_id).await;
    wait_for_output(&mut socket, "ready").await;
    send(&mut socket, json!({"type": "input", "data": "ok\n"})).await;
    wait_for_state(&mut socket, "succeeded").await;

    let mut observed = false;
    for _ in 0..200 {
        let (status, view) = harness.request("GET", "/api/agents/legacy/auth").await;
        assert_eq!(status, 200, "{view}");
        if view["observed_state"] == "authenticated" {
            observed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        observed,
        "copilot legacy success never recorded authenticated"
    );
    assert!(
        !history.join("authenticate.json").exists(),
        "copilot legacy sent `authenticate` instead of `copilot login`"
    );
    harness.auth.shutdown();
    harness.sessions.shutdown_all().await;
}

/// A Registry-installed agent that advertises a bare relative login command
/// resolves it inside that same agent's own validated install directory when
/// the sanitized PATH cannot. The advertised args stay exact and no shell runs.
#[tokio::test]
async fn registry_installed_relative_legacy_command_resolves_in_its_install_dir() {
    // If `opencode` were on this host's PATH, resolution would (correctly)
    // use the PATH entry instead, so this test only proves the install-dir
    // fallback when PATH cannot answer.
    if which("opencode").is_some() {
        return;
    }

    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path();
    let history = root.join("legacy");
    std::fs::create_dir_all(&history).unwrap();

    // The agent's own install directory holds the advertised executable.
    let install_dir = root.join("data/agents/opencode/1.0.0");
    std::fs::create_dir_all(&install_dir).unwrap();
    write_legacy_stub(&install_dir.join("opencode"));

    let harness = Harness::start(
        root,
        vec![legacy_agent(
            "legacy",
            &history,
            "auth-legacy-opencode-relative",
        )],
    )
    .await;

    // The durable record names the Registry agent and its install directory.
    let store = harness
        .sessions
        .store
        .as_ref()
        .expect("the harness has a store");
    let mut record = InstalledAgent::new("legacy".into(), AgentSource::Registry, "python3".into());
    record.registry = Some(RegistrySnapshot {
        registry_id: "opencode".into(),
        registry_version: "1.0.0".into(),
        distribution: InstalledDistribution::Npx {
            package: "opencode@1.0.0".into(),
            args: Vec::new(),
            env: Default::default(),
        },
        install_dir: Some(install_dir.display().to_string()),
        installed_at: chrono::Utc::now().to_rfc3339(),
    });
    store.insert_agent(&record).unwrap();

    let flow_id = harness.start_flow_for("legacy", "opencode-login").await;

    // The legacy stub records its invocation beside itself, inside the
    // install directory.
    let invocation_path = install_dir.join("invocation.json");
    for _ in 0..200 {
        if invocation_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let invocation: Value =
        serde_json::from_str(&std::fs::read_to_string(&invocation_path).unwrap()).unwrap();
    let argv: Vec<String> = invocation["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_owned())
        .collect();
    // The resolved program is the install-dir file, not a bare name.
    let resolved = std::fs::canonicalize(install_dir.join("opencode")).unwrap();
    assert_eq!(
        std::fs::canonicalize(&argv[0]).unwrap(),
        resolved,
        "{argv:?}"
    );
    // The advertised arguments are preserved exactly.
    assert_eq!(argv[1..], vec!["auth".to_string(), "login".to_string()]);

    harness.auth.cancel_terminal_flow(&flow_id).unwrap();
    harness.auth.shutdown();
    harness.sessions.shutdown_all().await;
}
