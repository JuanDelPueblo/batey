use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use base64::Engine as _;
use batey::{
    agents::registry::{HttpFetch, RegistryClient},
    agents::{
        parse_agents, AgentCatalog, AgentDefinition, AgentManager, AgentRegistry, AgentSource,
        HostRuntimeProbe,
    },
    config::{BateyPaths, Config, PathOverrides, RegistryConfig},
    events::{EventLog, EventPayload, ReplayResult},
    session::SessionManager,
    store::Store,
    web::{router, AppState},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::{sync::Arc, time::Duration};
use tower::ServiceExt;

struct OfflineRegistry;

impl HttpFetch for OfflineRegistry {
    fn fetch(
        &self,
        _url: String,
        _max_bytes: u64,
    ) -> batey::agents::registry::client::FetchFuture<'_> {
        Box::pin(async { Err(anyhow::anyhow!("fixture registry is offline")) })
    }
}

struct FixtureRegistry {
    response: Mutex<Result<Vec<u8>, String>>,
    calls: AtomicUsize,
}

impl FixtureRegistry {
    fn new(document: Vec<u8>) -> Self {
        Self {
            response: Mutex::new(Ok(document)),
            calls: AtomicUsize::new(0),
        }
    }

    fn fail(&self, message: &str) {
        *self.response.lock().unwrap() = Err(message.into());
    }

    fn set_document(&self, document: Vec<u8>) {
        *self.response.lock().unwrap() = Ok(document);
    }
}

impl HttpFetch for FixtureRegistry {
    fn fetch(
        &self,
        _url: String,
        _max_bytes: u64,
    ) -> batey::agents::registry::client::FetchFuture<'_> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let response = self.response.lock().unwrap().clone();
        Box::pin(async move { response.map_err(anyhow::Error::msg) })
    }
}

fn managed_app(
    root: &std::path::Path,
) -> (
    axum::Router,
    Arc<SessionManager>,
    Arc<AgentCatalog>,
    Arc<AgentManager>,
) {
    managed_app_with_registry(root, Arc::new(OfflineRegistry))
}

fn managed_app_with_registry(
    root: &std::path::Path,
    http: Arc<dyn HttpFetch>,
) -> (
    axum::Router,
    Arc<SessionManager>,
    Arc<AgentCatalog>,
    Arc<AgentManager>,
) {
    let paths = BateyPaths::from_overrides(PathOverrides {
        database: Some(root.join("hub.db")),
        data_dir: Some(root.join("data")),
        ..Default::default()
    });
    let store = Arc::new(Store::open(&paths.database).unwrap());
    let events = Arc::new(EventLog::persistent(store.clone()).unwrap());
    let agents = Arc::new(AgentCatalog::new([]));
    let sessions = SessionManager::with_store(agents.clone(), events, Some(store.clone()));
    let registry = Arc::new(RegistryClient::new(
        "https://registry.fixture.invalid/registry.json",
        paths.registry_cache.clone(),
        http.clone(),
    ));
    let agent_manager = AgentManager::new(
        store,
        agents.clone(),
        registry,
        paths.installed_agents.clone(),
        Arc::new(HostRuntimeProbe),
    );
    let config = Arc::new(Config {
        paths,
        agents: agents.clone(),
        agent_manager: Some(agent_manager.clone()),
        registry: RegistryConfig {
            url: "https://registry.fixture.invalid/registry.json".into(),
            http,
        },
        web: batey::config::WebConfig {
            project_roots: vec![root.display().to_string()],
            ..Default::default()
        },
        ..Default::default()
    });
    (
        router(AppState::new(sessions.clone(), config, 8765)),
        sessions,
        agents,
        agent_manager,
    )
}

#[tokio::test]
async fn registry_api_fetches_on_first_browse_and_keeps_cache_on_refresh_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let document = serde_json::to_vec(&json!({
        "version": "1.0.0",
        "agents": [{
            "id": "fixture-acp",
            "name": "Fixture ACP",
            "version": "1.0.0",
            "description": "Fixture agent",
            "distribution": {"npx": {"package": "fixture-acp@1.0.0"}}
        }]
    }))
    .unwrap();
    let fixture = Arc::new(FixtureRegistry::new(document));
    let (app, sessions, _, _) = managed_app_with_registry(tmp.path(), fixture.clone());

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/agents/registry")
                .header("host", "127.0.0.1:8765")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap()).unwrap();
    assert_eq!(body["status"], "fresh");
    assert_eq!(body["agents"][0]["id"], "fixture-acp");
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/agents/registry")
                .header("host", "127.0.0.1:8765")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap()).unwrap();
    assert_eq!(body["status"], "cached");
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);

    fixture.fail("container DNS lookup failed");
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/agents/registry/refresh")
                .header("host", "127.0.0.1:8765")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap()).unwrap();
    assert_eq!(body["status"], "cached");
    assert_eq!(body["agents"][0]["id"], "fixture-acp");
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("container DNS lookup failed"));
    sessions.shutdown_all().await;
}

#[tokio::test]
async fn registry_install_starts_operation_and_reports_progress() {
    let tmp = tempfile::tempdir().unwrap();
    let document = serde_json::to_string(&serde_json::json!({
        "version": "1.0.0",
        "agents": [{
            "id": "fixture-acp",
            "name": "Fixture ACP",
            "version": "1.0.0",
            "description": "A package agent for testing.",
            "distribution": {"npx": {"package": "fixture-acp@1.0.0"}}
        }]
    }))
    .unwrap();
    let fixture = Arc::new(FixtureRegistry::new(document.into_bytes()));
    let (app, sessions, _, _) = managed_app_with_registry(tmp.path(), fixture.clone());

    // First browse populates cache
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/agents/registry")
                .header("host", "127.0.0.1:8765")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    // POST /api/agents/registry/install returns operation immediately
    let install_body = serde_json::to_vec(&serde_json::json!({
        "registry_id": "fixture-acp"
    }))
    .unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/agents/registry/install")
                .header("host", "127.0.0.1:8765")
                .header("content-type", "application/json")
                .body(Body::from(install_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let op: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap()).unwrap();
    assert_eq!(op["agent_id"], "fixture-acp");
    assert_eq!(op["registry_id"], "fixture-acp");
    assert_eq!(op["kind"], "install");
    let op_id = op["id"].as_str().unwrap().to_string();

    // GET /api/agents/operations lists active operations
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/agents/operations")
                .header("host", "127.0.0.1:8765")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let list: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap()).unwrap();
    assert!(list
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["id"] == op_id));

    // Poll GET /api/agents/operations/:id until terminal
    let mut finished = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/agents/operations/{op_id}"))
                    .header("host", "127.0.0.1:8765")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let status: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap())
                .unwrap();
        if status["state"] == "succeeded" {
            finished = true;
            assert_eq!(status["stage"], "completed");
            break;
        }
    }
    assert!(finished, "Operation did not finish with succeeded");

    // Agent is now installed in /api/agents
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/agents")
                .header("host", "127.0.0.1:8765")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let agents: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap()).unwrap();
    assert!(agents
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["id"] == "fixture-acp"));

    // Now test update operation
    let v2_doc = serde_json::to_string(&serde_json::json!({
        "version": "1.0.0",
        "agents": [{
            "id": "fixture-acp",
            "name": "Fixture ACP",
            "version": "2.0.0",
            "description": "A package agent for testing v2.",
            "distribution": {"npx": {"package": "fixture-acp@2.0.0"}}
        }]
    }))
    .unwrap();
    fixture.set_document(v2_doc.into_bytes());

    // Refresh registry
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/agents/registry/refresh")
                .header("host", "127.0.0.1:8765")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    // POST /api/agents/fixture-acp/update returns operation immediately
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/agents/fixture-acp/update")
                .header("host", "127.0.0.1:8765")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let op: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap()).unwrap();
    assert_eq!(op["agent_id"], "fixture-acp");
    assert_eq!(op["kind"], "update");
    let update_op_id = op["id"].as_str().unwrap().to_string();

    // Poll GET /api/agents/operations/:id until terminal
    let mut update_finished = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/agents/operations/{update_op_id}"))
                    .header("host", "127.0.0.1:8765")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let status: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap())
                .unwrap();
        if status["state"] == "succeeded" {
            update_finished = true;
            assert_eq!(status["stage"], "completed");
            break;
        }
    }
    assert!(
        update_finished,
        "Update operation did not finish with succeeded"
    );

    // Agent in /api/agents is now version 2.0.0
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/agents")
                .header("host", "127.0.0.1:8765")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let agents: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap()).unwrap();
    let updated_agent = agents
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "fixture-acp")
        .expect("updated agent present");
    assert_eq!(updated_agent["display"]["version"], "2.0.0");

    sessions.shutdown_all().await;
}

fn manager(root: &std::path::Path, can_load: bool) -> Arc<SessionManager> {
    manager_with_mode(root, if can_load { "load" } else { "no-load" })
}

fn manager_with_mode(root: &std::path::Path, mode: &str) -> Arc<SessionManager> {
    let store = Arc::new(Store::open(&root.join("hub.db")).unwrap());
    let log = Arc::new(EventLog::persistent(store.clone()).unwrap());
    let history = root.join("history");
    std::fs::create_dir_all(&history).unwrap();
    let agent = AgentDefinition::codex_default()
        .with_command("python3".into())
        .with_args(vec![
            format!("{}/tests/fake_acp.py", env!("CARGO_MANIFEST_DIR")),
            history.display().to_string(),
            mode.into(),
        ])
        .with_idle_timeout(Duration::ZERO);
    SessionManager::with_store(Arc::new(AgentRegistry::new([agent])), log, Some(store))
}

#[tokio::test]
async fn rich_prompt_http_body_limit_accepts_encoded_media_and_rejects_oversize() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let mgr = manager_with_mode(&root, "load");
    let mut config = Config::default();
    config.web.project_roots = vec![root.display().to_string()];
    let app = router(AppState::new(mgr.clone(), Arc::new(config), 8765));
    let project = mgr
        .store
        .as_ref()
        .unwrap()
        .create_project("uploads".into(), root.display().to_string())
        .unwrap();
    let chat = mgr
        .store
        .as_ref()
        .unwrap()
        .create_chat(project.id, "codex".into(), Some("uploads".into()))
        .unwrap();

    let mut png = vec![0_u8; batey::content::MAX_MEDIA_BYTES];
    png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    let encoded = base64::engine::general_purpose::STANDARD.encode(png);
    let body = serde_json::to_vec(&json!({"content": [
        {"type": "text", "text": "attachment"},
        {"type": "image", "data": encoded, "mimeType": "image/png"}
    ]}))
    .unwrap();
    assert!(body.len() > 2 * 1024 * 1024);
    assert!(body.len() < batey::content::MAX_RICH_PROMPT_HTTP_BYTES);
    let accepted = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/chats/{}/prompt", chat.id))
                .header("host", "127.0.0.1:8765")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(accepted.status(), axum::http::StatusCode::ACCEPTED);

    let rejected = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/chats/{}/prompt", chat.id))
                .header("host", "127.0.0.1:8765")
                .header("content-type", "application/json")
                .body(Body::from(vec![
                    b'x';
                    batey::content::MAX_RICH_PROMPT_HTTP_BYTES
                        + 1
                ]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rejected.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    mgr.shutdown_all().await;
}

#[tokio::test]
async fn agents_api_returns_provider_neutral_catalog_summaries() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&tmp.path().join("hub.db")).unwrap());
    let events = Arc::new(EventLog::persistent(store.clone()).unwrap());
    let agents = Arc::new(AgentRegistry::new([
        AgentDefinition::codex_default()
            .with_display_name("Codex CLI".into())
            .with_source(AgentSource::File)
            .with_usage_provider(Some("quota-service".into()))
            .with_metadata(json!({"package": "codex"})),
        AgentDefinition::new("offline", "missing-acp").with_available(false),
    ]));
    let sessions = SessionManager::with_store(agents.clone(), events, Some(store));
    let config = Arc::new(Config {
        agents,
        ..Default::default()
    });
    let app = router(AppState::new(sessions.clone(), config, 8765));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/agents")
                .header("host", "127.0.0.1:8765")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap()).unwrap();
    assert_eq!(body[0]["id"], "codex");
    assert_eq!(body[0]["display_name"], "Codex CLI");
    assert_eq!(body[0]["source"], "file");
    assert_eq!(body[0]["availability"], "available");
    assert_eq!(body[0]["usage_provider"], "quota-service");
    assert_eq!(body[0]["metadata"]["package"], "codex");
    assert_eq!(body[1]["id"], "offline");
    assert_eq!(body[1]["availability"], "unavailable");
    sessions.shutdown_all().await;
}

#[tokio::test]
async fn managed_agents_persist_through_the_real_router_and_store_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let (app, sessions, _, _) = managed_app(tmp.path());
    let request = Request::builder()
        .method("POST")
        .uri("/api/agents")
        .header("host", "127.0.0.1:8765")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"id":"fixture-agent","command":"fixture-acp","display_name":"Fixture"}"#,
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    sessions.shutdown_all().await;

    let (app, sessions, agents, agent_manager) = managed_app(tmp.path());
    // Startup loads durable agent records before accepting routes, just as main does.
    assert_eq!(agent_manager.load_persisted().unwrap(), 1);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/agents")
                .header("host", "127.0.0.1:8765")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap()).unwrap();
    assert_eq!(body[0]["id"], "fixture-agent");
    assert!(agents.contains("fixture-agent"));
    sessions.shutdown_all().await;
}

#[tokio::test]
async fn independent_sessions_resume_config_permission_and_idle_cleanup() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager(tmp.path(), true);
    let db = mgr.store.as_ref().unwrap();
    let project = db
        .create_project("test".into(), tmp.path().display().to_string())
        .unwrap();
    let second_root = tmp.path().join("second-project");
    std::fs::create_dir_all(&second_root).unwrap();
    let second_project = db
        .create_project("second-test".into(), second_root.display().to_string())
        .unwrap();
    let a = db
        .create_chat(project.id.clone(), "codex".into(), Some("one".into()))
        .unwrap();
    let b = db
        .create_chat(second_project.id, "codex".into(), Some("two".into()))
        .unwrap();
    let one = mgr.get_by_id(&a.id).await.unwrap();
    let two = mgr.get_by_id(&b.id).await.unwrap();
    assert!(!Arc::ptr_eq(&one, &two));
    let (r1, r2) = tokio::join!(one.ask("hi".into(), None), two.ask("hi".into(), None));
    assert_ne!(r1.unwrap(), r2.unwrap());
    let sid = db.chat(&a.id).unwrap().acp_session_id.unwrap();
    assert!(one
        .ask("next".into(), None)
        .await
        .unwrap()
        .contains(":2:small"));
    one.set_config("model", json!("large")).await.unwrap();
    assert!(one.set_config("model", json!("invalid")).await.is_err());
    let mut events = mgr.event_log().subscribe();
    let prompt = {
        let one = one.clone();
        tokio::spawn(async move { one.ask("permission".into(), None).await })
    };
    let permission = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let EventPayload::PermissionRequest { id, .. } = events.recv().await.unwrap().payload
            {
                break id;
            }
        }
    })
    .await
    .unwrap();
    one.respond_to_permission(&permission, "yes").await;
    assert_eq!(prompt.await.unwrap().unwrap(), "yes");
    mgr.reap_idle().await;
    assert_eq!(db.chats().unwrap().len(), 2);
    assert_eq!(
        one.process_state().await,
        batey::state::ProcessState::Stopped
    );
    mgr.shutdown_all().await;
    drop(one);
    drop(two);
    drop(mgr);
    let mgr = manager(tmp.path(), true);
    let one = mgr.get_by_id(&a.id).await.unwrap();
    let result = one.ask("after restart".into(), None).await.unwrap();
    assert_eq!(result, format!("{sid}:4:large"));
    let history = match mgr.event_log().replay_from(0) {
        batey::events::ReplayResult::Complete(e)
        | batey::events::ReplayResult::Partial { events: e, .. } => e,
    };
    assert!(!history.iter().any(
        |e| matches!(&e.payload, EventPayload::MessageChunk { text, .. } if text == "REPLAY")
    ));
    mgr.shutdown_all().await;
}

#[tokio::test]
async fn unsupported_resume_never_creates_another_conversation() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager(tmp.path(), false);
    let db = mgr.store.as_ref().unwrap();
    let p = db
        .create_project("test".into(), tmp.path().display().to_string())
        .unwrap();
    let chat = db
        .create_chat(p.id, "codex".into(), Some("chat".into()))
        .unwrap();
    let s = mgr.get_by_id(&chat.id).await.unwrap();
    s.ask("hi".into(), None).await.unwrap();
    let saved = db.chat(&chat.id).unwrap().acp_session_id;
    mgr.reap_idle().await;
    assert_eq!(
        s.process_state().await,
        batey::state::ProcessState::Running,
        "a non-resumable chat must not be made unusable by idle reaping"
    );
    s.edit_metadata(None, Some(true)).await.unwrap();
    assert_eq!(
        s.process_state().await,
        batey::state::ProcessState::Running,
        "archiving a live chat must not terminate its process"
    );
    s.edit_metadata(None, Some(false)).await.unwrap();
    assert_eq!(
        s.process_state().await,
        batey::state::ProcessState::Running,
        "unarchiving a live chat must preserve its process"
    );
    let error = s.change_connection_config(|_| Ok(())).await.unwrap_err();
    assert!(error.to_string().contains("require a new chat"));
    assert_eq!(
        s.process_state().await,
        batey::state::ProcessState::Running,
        "a rejected connection edit must not strand a non-resumable chat"
    );
    assert_eq!(db.chat(&chat.id).unwrap().acp_session_id, saved);
    s.stop().await.unwrap();
    assert!(s
        .resume()
        .await
        .unwrap_err()
        .to_string()
        .contains("cannot resume"));
    assert_eq!(db.chat(&chat.id).unwrap().acp_session_id, saved);
    assert_eq!(
        std::fs::read_dir(tmp.path().join("history"))
            .unwrap()
            .count(),
        1
    );
    mgr.shutdown_all().await;
}

#[tokio::test]
async fn api_validation_and_chat_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager(tmp.path(), true);
    let mut config = Config::default();
    config.web.project_roots = vec![tmp.path().display().to_string()];
    let app = router(AppState::new(mgr.clone(), Arc::new(config), 8765));
    async fn call(app: &axum::Router, method: &str, path: &str, body: Value) -> (u16, Value) {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("host", "127.0.0.1:8765")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = r.status().as_u16();
        let body = to_bytes(r.into_body(), 1_000_000).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }
    assert_eq!(
        call(
            &app,
            "POST",
            "/api/projects",
            json!({"name":"bad","path":"/"})
        )
        .await
        .0,
        400
    );
    let (status, p) = call(
        &app,
        "POST",
        "/api/projects",
        json!({"name":"ok","path":tmp.path()}),
    )
    .await;
    assert_eq!(status, 200);
    let path = format!("/api/projects/{}/chats", p["id"].as_str().unwrap());
    assert_eq!(
        call(
            &app,
            "POST",
            &path,
            json!({"agent":"/bin/sh","title":"bad"})
        )
        .await
        .0,
        400
    );
    let (_, a) = call(&app, "POST", &path, json!({"agent":"codex","title":"a"})).await;
    let (_, b) = call(&app, "POST", &path, json!({"agent":"codex","title":"b"})).await;
    assert_ne!(a["id"], b["id"]);

    let (list_status, list) = call(&app, "GET", "/api/projects", json!({})).await;
    assert_eq!(list_status, 200);
    let p_item = list
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == p["id"])
        .unwrap();
    assert_eq!(p_item["chat_count"], 2);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/projects")
                .header("host", "127.0.0.1:8765")
                .header("origin", "https://evil.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    mgr.shutdown_all().await;
}

#[tokio::test]
async fn chat_history_is_bounded_chat_scoped_and_survives_a_large_global_log() {
    let tmp = tempfile::tempdir().unwrap();
    let setup = manager(tmp.path(), true);
    let db = setup.store.as_ref().unwrap();
    let project = db
        .create_project("history".into(), tmp.path().display().to_string())
        .unwrap();
    let target = db
        .create_chat(project.id.clone(), "codex".into(), Some("target".into()))
        .unwrap();
    let other = db
        .create_chat(project.id, "codex".into(), Some("other".into()))
        .unwrap();
    let target_id = target.id.clone();
    let other_id = other.id.clone();
    drop(setup);

    // Seed the large durable fixture in one transaction. Normal EventLog
    // writes remain one-event-at-a-time and fail closed; this only keeps the
    // integration fixture from spending a commit on each of 20,050 rows.
    let connection = rusqlite::Connection::open(tmp.path().join("hub.db")).unwrap();
    let transaction = connection.unchecked_transaction().unwrap();
    for index in 0..20_050 {
        let event = batey::events::SessionEvent {
            seq: index + 1,
            timestamp: chrono::Utc::now(),
            session_id: if index % 2 == 0 {
                target_id.clone()
            } else {
                other_id.clone()
            },
            agent: "codex".into(),
            payload: EventPayload::MessageChunk {
                message_id: None,
                text: index.to_string(),
                content: vec![],
            },
        };
        transaction
            .execute(
                "INSERT INTO events (seq, session_id, data) VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    event.seq as i64,
                    event.session_id,
                    serde_json::to_string(&event).unwrap(),
                ],
            )
            .unwrap();
    }
    transaction.commit().unwrap();

    let mgr = manager(tmp.path(), true);

    let mut config = Config::default();
    config.web.project_roots = vec![tmp.path().display().to_string()];
    let app = router(AppState::new(mgr.clone(), Arc::new(config), 8765));
    let mut cursor = None;
    let mut total = 0;
    let mut pages = 0;
    loop {
        let query = cursor
            .map(|value| format!("&before_seq={value}"))
            .unwrap_or_default();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/chats/{}/history?limit=37{}",
                        target_id, query
                    ))
                    .header("host", "127.0.0.1:8765")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap())
                .unwrap();
        let events = body["events"].as_array().unwrap();
        assert!(events.len() <= 37);
        assert!(events.iter().all(|event| event["session_id"] == target_id));
        total += events.len();
        pages += 1;
        if !body["has_older"].as_bool().unwrap() {
            break;
        }
        cursor = body["next_cursor"].as_u64();
        assert!(cursor.is_some());
    }
    assert_eq!(total, 10_025);
    assert!(pages > 250);
    mgr.shutdown_all().await;
}

#[tokio::test]
async fn fresh_websocket_subscribes_at_the_live_baseline_without_global_history() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager(tmp.path(), true);
    let db = mgr.store.as_ref().unwrap();
    let project = db
        .create_project("socket".into(), tmp.path().display().to_string())
        .unwrap();
    let chat = db
        .create_chat(project.id, "codex".into(), Some("socket".into()))
        .unwrap();
    mgr.event_log()
        .append(
            &chat.id,
            "codex",
            EventPayload::MessageChunk {
                message_id: None,
                text: "old".into(),
                content: vec![],
            },
        )
        .unwrap();

    // This event lands while the chat is opening, before the fresh subscribe
    // captures its baseline. The bounded history request must include it.
    mgr.event_log()
        .append(
            &chat.id,
            "codex",
            EventPayload::MessageChunk {
                message_id: None,
                text: "between startup and baseline".into(),
                content: vec![],
            },
        )
        .unwrap();

    let mut config = Config::default();
    config.web.project_roots = vec![tmp.path().display().to_string()];
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let app = router(AppState::new(mgr.clone(), Arc::new(config), address.port()));
    let server_app = app.clone();
    let server = tokio::spawn(async move { axum::serve(listener, server_app).await.unwrap() });
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .unwrap();
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            json!({ "type": "subscribe", "from_seq": 0 }).to_string(),
        ))
        .await
        .unwrap();

    let subscribed = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let subscribed: Value = serde_json::from_str(subscribed.to_text().unwrap()).unwrap();
    assert_eq!(subscribed["type"], "subscribed");
    assert_eq!(subscribed["through_seq"], 2);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), socket.next())
            .await
            .is_err()
    );

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/chats/{}/history?through_seq=2", chat.id))
                .header("host", format!("127.0.0.1:{}", address.port()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap()).unwrap();
    assert_eq!(
        body["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["seq"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![1, 2]
    );

    mgr.event_log()
        .append(
            &chat.id,
            "codex",
            EventPayload::MessageChunk {
                message_id: None,
                text: "live".into(),
                content: vec![],
            },
        )
        .unwrap();
    let live = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let live: Value = serde_json::from_str(live.to_text().unwrap()).unwrap();
    assert_eq!(live["seq"], 3);
    socket.close(None).await.unwrap();

    mgr.event_log()
        .append(
            &chat.id,
            "codex",
            EventPayload::MessageChunk {
                message_id: None,
                text: "missed while disconnected".into(),
                content: vec![],
            },
        )
        .unwrap();
    let (mut reconnect, _) = tokio_tungstenite::connect_async(format!("ws://{address}/ws"))
        .await
        .unwrap();
    reconnect
        .send(tokio_tungstenite::tungstenite::Message::Text(
            json!({ "type": "subscribe", "from_seq": 4 }).to_string(),
        ))
        .await
        .unwrap();
    let missed = tokio::time::timeout(Duration::from_secs(2), reconnect.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let missed: Value = serde_json::from_str(missed.to_text().unwrap()).unwrap();
    assert_eq!(missed["seq"], 4);
    reconnect.close(None).await.unwrap();
    server.abort();
    mgr.shutdown_all().await;
}

#[test]
fn generic_configuration_is_validated() {
    let agents =
        parse_agents(r#"{"custom":{"command":"/nix/store/example/bin/acp","args":["--stdio"]}}"#)
            .unwrap();
    assert_eq!(
        agents.definition("custom").unwrap().launch.args,
        vec!["--stdio"]
    );
    assert!(parse_agents(r#"{"bad":{"command":""}}"#).is_err());
    assert!(parse_agents(r#"{"bad":{"command":"acp","unknown":true}}"#).is_err());
    let options = json!([{"id":"model","type":"select","options":[{"group":"provider","options":[{"value":"x"}]}]},{"id":"fast","type":"boolean"}]);
    assert!(batey::acp::validate_config_value(&options, "model", &json!("x")).is_ok());
    assert!(batey::acp::validate_config_value(&options, "fast", &json!(true)).is_ok());
    assert!(batey::acp::validate_config_value(&options, "fast", &json!("true")).is_err());
}

#[tokio::test]
async fn folder_browsing_and_security() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let child1 = root.join("child1");
    let child2 = root.join("child2");
    let subchild = child1.join("subchild");
    let hidden = root.join(".hidden");
    std::fs::create_dir_all(&subchild).unwrap();
    std::fs::create_dir_all(&child2).unwrap();
    std::fs::create_dir_all(&hidden).unwrap();

    // Symlink escaping root
    let outside = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    let _ = std::os::unix::fs::symlink(outside.path(), root.join("escaping_symlink"));

    let mgr = manager(&root, false);
    let mut config = Config::default();
    config.web.project_roots = vec![root.display().to_string()];
    let app = router(AppState::new(mgr.clone(), Arc::new(config), 8765));

    async fn get(app: &axum::Router, uri: &str) -> (u16, Value) {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("host", "127.0.0.1:8765")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = r.status().as_u16();
        let body = to_bytes(r.into_body(), 1_000_000).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    // 1. Browsing at root lists allowed children and excludes hidden/escaping
    let (status, list) = get(
        &app,
        &format!("/api/filesystem/directories?path={}", root.display()),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(list["current"], root.display().to_string());
    assert_eq!(list["parent"], Value::Null);
    let dir_names: Vec<&str> = list["directories"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["name"].as_str())
        .collect();
    assert!(dir_names.contains(&"child1"));
    assert!(dir_names.contains(&"child2"));
    assert!(!dir_names.contains(&".hidden"));
    assert!(!dir_names.contains(&"escaping_symlink"));

    // 2. Browsing subfolder shows parent as root and breadcrumbs
    let (status, sub_list) = get(
        &app,
        &format!("/api/filesystem/directories?path={}", child1.display()),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(sub_list["parent"], root.display().to_string());
    let crumbs: Vec<&str> = sub_list["breadcrumbs"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|b| b["name"].as_str())
        .collect();
    assert!(crumbs.contains(&"child1"));

    // 3. Rejects path outside root
    let (status, _) = get(&app, "/api/filesystem/directories?path=/").await;
    assert_ne!(status, 200);

    // 4. Rejects relative path
    let (status, _) = get(&app, "/api/filesystem/directories?path=relative").await;
    assert_eq!(status, 400);

    mgr.shutdown_all().await;
}

#[tokio::test]
async fn git_clone_validation_and_behavior() {
    use batey::web::{derive_repo_name, sanitize_credentials, validate_git_url};

    // Validation unit checks
    assert!(validate_git_url("https://github.com/JuanDelPueblo/batey.git").is_ok());
    assert!(validate_git_url("git@github.com:JuanDelPueblo/batey.git").is_ok());
    assert!(validate_git_url("ssh://git@github.com/JuanDelPueblo/batey.git").is_ok());
    assert!(validate_git_url("file:///etc/passwd").is_err());
    assert!(validate_git_url("/tmp/repo").is_err());
    assert!(validate_git_url("./local-repo").is_err());
    assert!(validate_git_url("ext::sh -c evil").is_err());
    assert!(validate_git_url("").is_err());

    // Plain HTTP sends credentials without encryption. Reject it.
    assert!(validate_git_url("http://github.com/JuanDelPueblo/batey.git").is_err());
    assert!(validate_git_url("HTTP://github.com/JuanDelPueblo/batey.git").is_err());
    assert!(validate_git_url("http://user:pw@github.com/org/repo.git").is_err());
    assert!(validate_git_url("git://github.com/JuanDelPueblo/batey.git").is_err());

    // Credential sanitization must remove the complete userinfo.
    assert_eq!(
        sanitize_credentials("fatal: could not read https://ghp_SECRET@github.com/org/repo.git"),
        "fatal: could not read https://***@github.com/org/repo.git"
    );
    assert_eq!(
        sanitize_credentials("remote: https://user:ghp_SECRET@github.com/org/repo.git denied"),
        "remote: https://***@github.com/org/repo.git denied"
    );
    assert_eq!(
        sanitize_credentials("ssh://git@github.com/org/repo.git"),
        "ssh://***@github.com/org/repo.git"
    );
    // A URL without userinfo stays unchanged.
    assert_eq!(
        sanitize_credentials("https://github.com/org/repo.git"),
        "https://github.com/org/repo.git"
    );
    // An '@' in the path is not userinfo.
    assert_eq!(
        sanitize_credentials("https://github.com/org/repo.git@v1"),
        "https://github.com/org/repo.git@v1"
    );
    // Two URLs in one message are both sanitized.
    assert_eq!(
        sanitize_credentials("https://A@host/a.git and https://B@host/b.git"),
        "https://***@host/a.git and https://***@host/b.git"
    );
    // The token must not survive anywhere in the output.
    assert!(!sanitize_credentials("https://ghp_SECRET@github.com/o/r.git").contains("ghp_SECRET"));

    // Name derivation
    assert_eq!(
        derive_repo_name("https://github.com/JuanDelPueblo/batey.git").as_deref(),
        Some("batey")
    );
    assert_eq!(
        derive_repo_name("git@github.com:JuanDelPueblo/batey.git").as_deref(),
        Some("batey")
    );
    assert_eq!(
        derive_repo_name("https://github.com/JuanDelPueblo/my-project/").as_deref(),
        Some("my-project")
    );

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let mgr = manager(&root, false);
    let mut config = Config::default();
    config.web.project_roots = vec![root.display().to_string()];
    let app = router(AppState::new(mgr.clone(), Arc::new(config), 8765));

    async fn post(app: &axum::Router, uri: &str, body: Value) -> (u16, Value) {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("host", "127.0.0.1:8765")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = r.status().as_u16();
        let body = to_bytes(r.into_body(), 1_000_000).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    // 1. Rejects unsafe file:// URL
    let (status, _) = post(
        &app,
        "/api/projects/clone",
        json!({
            "url": "file:///tmp/repo",
            "parent_path": root.display().to_string(),
        }),
    )
    .await;
    assert_eq!(status, 400);

    // 2. Rejects parent path outside root
    let (status, _) = post(
        &app,
        "/api/projects/clone",
        json!({
            "url": "https://github.com/example/repo.git",
            "parent_path": "/tmp",
        }),
    )
    .await;
    assert_eq!(status, 400);

    // 3. Rejects existing destination folder
    let existing = root.join("existing-dir");
    std::fs::create_dir_all(&existing).unwrap();
    let (status, _) = post(
        &app,
        "/api/projects/clone",
        json!({
            "url": "https://github.com/example/existing-dir.git",
            "parent_path": root.display().to_string(),
        }),
    )
    .await;
    assert_eq!(status, 409);

    // 4. Failed clone cleans up and does not create Project record
    let count_before = mgr.store.as_ref().unwrap().projects().unwrap().len();
    let (status, _) = post(
        &app,
        "/api/projects/clone",
        json!({
            "url": "https://invalid.unresolvable.example/doesnotexist.git",
            "parent_path": root.display().to_string(),
            "name": "failed-clone",
        }),
    )
    .await;
    assert_eq!(status, 400);
    assert!(!root.join("failed-clone").exists());
    let count_after = mgr.store.as_ref().unwrap().projects().unwrap().len();
    assert_eq!(count_before, count_after);

    // 5. Destination name traversal validation unit checks
    use batey::web::validate_clone_destination_name;
    assert!(validate_clone_destination_name("valid-name").is_ok());
    assert!(validate_clone_destination_name("my_repo_123").is_ok());
    assert!(validate_clone_destination_name("../outside").is_err());
    assert!(validate_clone_destination_name("../../foo").is_err());
    assert!(validate_clone_destination_name("/foo").is_err());
    assert!(validate_clone_destination_name("foo/bar").is_err());
    assert!(validate_clone_destination_name("foo\\bar").is_err());
    assert!(validate_clone_destination_name(".").is_err());
    assert!(validate_clone_destination_name("..").is_err());
    assert!(validate_clone_destination_name("").is_err());
    assert!(validate_clone_destination_name("   ").is_err());

    // 6. Path traversal and absolute path attempts via clone endpoint rejected
    let (status, _) = post(
        &app,
        "/api/projects/clone",
        json!({
            "url": "https://github.com/example/repo.git",
            "parent_path": root.display().to_string(),
            "name": "../outside",
        }),
    )
    .await;
    assert_eq!(status, 400);

    let (status, _) = post(
        &app,
        "/api/projects/clone",
        json!({
            "url": "https://github.com/example/repo.git",
            "parent_path": root.display().to_string(),
            "name": "/foo",
        }),
    )
    .await;
    assert_eq!(status, 400);

    let (status, _) = post(
        &app,
        "/api/projects/clone",
        json!({
            "url": "https://github.com/example/repo.git",
            "parent_path": root.display().to_string(),
            "name": "foo/bar",
        }),
    )
    .await;
    assert_eq!(status, 400);

    let (status, _) = post(
        &app,
        "/api/projects/clone",
        json!({
            "url": "https://github.com/example/repo.git",
            "parent_path": root.display().to_string(),
            "name": "..",
        }),
    )
    .await;
    assert_eq!(status, 400);

    // 7. Process timeout kills, waits/reaps, cleans up destination, and does not create Project
    let mut sleep_cmd = tokio::process::Command::new("python3");
    sleep_cmd
        .arg("-c")
        .arg("import time; time.sleep(10)")
        .kill_on_drop(true);
    let child = sleep_cmd.spawn().unwrap();
    let dest_dir = root.join("timed_out_destination");
    std::fs::create_dir_all(&dest_dir).unwrap();
    assert!(dest_dir.exists());

    let res =
        batey::web::run_command_with_timeout(child, Duration::from_millis(50), Some(&dest_dir))
            .await;
    assert!(res.is_err());
    assert!(!dest_dir.exists());
    assert_eq!(
        mgr.store.as_ref().unwrap().projects().unwrap().len(),
        count_before
    );

    mgr.shutdown_all().await;
}

#[tokio::test]
async fn acp_titles_and_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let mgr = manager(&root, false);
    let mut config = Config::default();
    config.web.project_roots = vec![root.display().to_string()];
    let app = router(AppState::new(mgr.clone(), Arc::new(config), 8765));

    async fn post(app: &axum::Router, uri: &str, body: Value) -> (u16, Value) {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("host", "127.0.0.1:8765")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = r.status().as_u16();
        let body = to_bytes(r.into_body(), 1_000_000).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    async fn patch(app: &axum::Router, uri: &str, body: Value) -> (u16, Value) {
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(uri)
                    .header("host", "127.0.0.1:8765")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = r.status().as_u16();
        let body = to_bytes(r.into_body(), 1_000_000).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    // 1. Create project
    let (_, p) = post(
        &app,
        "/api/projects",
        json!({"name":"p1","path":root.display().to_string()}),
    )
    .await;
    let pid = p["id"].as_str().unwrap();

    // 2. Create chat requiring only agent (no title supplied)
    let (status, c) = post(
        &app,
        &format!("/api/projects/{}/chats", pid),
        json!({"agent":"codex"}),
    )
    .await;
    assert_eq!(status, 200);
    let cid = c["id"].as_str().unwrap();
    assert_eq!(c["title"], "New chat 1");
    assert_eq!(c["title_overridden"], false);

    // 3. Connect/Prompt chat with ACP providing title
    let session = mgr.get_by_id(cid).await.unwrap();
    let reply = session
        .ask("title: Great Project Discussion".into(), None)
        .await
        .unwrap();
    assert_eq!(reply, "title-sent");

    // Title updated and persisted in store
    let updated_chat = mgr.store.as_ref().unwrap().chat(cid).unwrap();
    assert_eq!(updated_chat.title, "Great Project Discussion");
    assert!(!updated_chat.title_overridden);

    // 4. Empty title is ignored
    session.ask("title-empty".into(), None).await.unwrap();
    let chat_after_empty = mgr.store.as_ref().unwrap().chat(cid).unwrap();
    assert_eq!(chat_after_empty.title, "Great Project Discussion");

    // 5. Oversized title (>200 chars) is safely ignored
    session.ask("title-oversized".into(), None).await.unwrap();
    let chat_after_oversized = mgr.store.as_ref().unwrap().chat(cid).unwrap();
    assert_eq!(chat_after_oversized.title, "Great Project Discussion");

    // 6. Manual user rename marks title_overridden = true
    let (patch_status, patched) = patch(
        &app,
        &format!("/api/chats/{}", cid),
        json!({"title": "My Custom Title"}),
    )
    .await;
    assert_eq!(patch_status, 200);
    assert_eq!(patched["title"], "My Custom Title");
    assert_eq!(patched["title_overridden"], true);

    // 7. Subsequent ACP title update does NOT overwrite manually overridden title
    session
        .ask("title: Attempted ACP Overwrite".into(), None)
        .await
        .unwrap();
    let chat_after_manual = mgr.store.as_ref().unwrap().chat(cid).unwrap();
    assert_eq!(chat_after_manual.title, "My Custom Title");

    // 8. Verify MetadataChanged was emitted
    let history = match mgr.event_log().replay_from(0) {
        batey::events::ReplayResult::Complete(e)
        | batey::events::ReplayResult::Partial { events: e, .. } => e,
    };
    let meta_events: Vec<_> = history
        .iter()
        .filter(|e| matches!(e.payload, EventPayload::MetadataChanged {}))
        .collect();
    assert!(!meta_events.is_empty());

    mgr.shutdown_all().await;
}

#[tokio::test]
async fn http_permission_endpoint_accepts_frontend_payload() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let mgr = manager(&root, false);
    let mut config = Config::default();
    config.web.project_roots = vec![root.display().to_string()];
    let app = router(AppState::new(mgr.clone(), Arc::new(config), 8765));

    let db = mgr.store.as_ref().unwrap();
    let project = db
        .create_project("perm-test".into(), root.display().to_string())
        .unwrap();
    let chat = db
        .create_chat(project.id, "codex".into(), Some("perm-chat".into()))
        .unwrap();
    let session = mgr.get_by_id(&chat.id).await.unwrap();

    let mut events = mgr.event_log().subscribe();
    let prompt_task = {
        let session = session.clone();
        tokio::spawn(async move { session.ask("permission".into(), None).await })
    };

    let (perm_id, options) = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let EventPayload::PermissionRequest { id, options, .. } =
                events.recv().await.unwrap().payload
            {
                break (id, options);
            }
        }
    })
    .await
    .unwrap();
    let option_ids: Vec<_> = options
        .as_array()
        .unwrap()
        .iter()
        .map(|option| option["optionId"].as_str().unwrap())
        .collect();
    assert_eq!(option_ids, ["yes", "always", "no", "never"]);

    // Select the exact option ID supplied by the ACP agent.
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/chats/{}/permission", chat.id))
        .header("host", "127.0.0.1:8765")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "id": perm_id,
                "option_id": "always"
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body_bytes = to_bytes(resp.into_body(), 100_000).await.unwrap();
    let resp_json: Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(resp_json["success"], true);

    // Prompt finishes successfully because permission was granted
    assert_eq!(prompt_task.await.unwrap().unwrap(), "always");
    let response = match mgr.event_log().replay_from(0) {
        ReplayResult::Complete(events) | ReplayResult::Partial { events, .. } => {
            events.into_iter().find_map(|event| match event.payload {
                EventPayload::PermissionResponse { id, option_id, .. } if id == perm_id => {
                    option_id
                }
                _ => None,
            })
        }
    };
    assert_eq!(response.as_deref(), Some("always"));

    // Second response to already handled perm_id returns 409 CONFLICT
    let req_stale = Request::builder()
        .method("POST")
        .uri(format!("/api/chats/{}/permission", chat.id))
        .header("host", "127.0.0.1:8765")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "id": perm_id,
                "option_id": "always"
            })
            .to_string(),
        ))
        .unwrap();
    let resp_stale = app.oneshot(req_stale).await.unwrap();
    assert_eq!(resp_stale.status(), axum::http::StatusCode::CONFLICT);

    mgr.shutdown_all().await;
}

/// Deleting a chat must take its events with it and leave every other chat's
/// history intact, including after a restart rebuilds the log from SQLite.
#[tokio::test]
async fn deleting_a_chat_removes_only_its_events_across_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = manager(tmp.path(), true);
    let db = mgr.store.as_ref().unwrap().clone();
    let project = db
        .create_project("test".into(), tmp.path().display().to_string())
        .unwrap();
    let doomed = db
        .create_chat(project.id.clone(), "codex".into(), Some("doomed".into()))
        .unwrap();
    let kept = db
        .create_chat(project.id, "codex".into(), Some("kept".into()))
        .unwrap();

    let doomed_session = mgr.get_by_id(&doomed.id).await.unwrap();
    let kept_session = mgr.get_by_id(&kept.id).await.unwrap();
    doomed_session.ask("hi".into(), None).await.unwrap();
    kept_session.ask("hi".into(), None).await.unwrap();
    assert!(db
        .events()
        .unwrap()
        .iter()
        .any(|e| e.session_id == doomed.id));

    doomed_session.delete_metadata().await.unwrap();
    mgr.event_log().forget_chat(&doomed.id);
    mgr.remove_session(&doomed.id).await;
    mgr.shutdown_all().await;

    // A new process rebuilds the in-memory log from the rows that survived.
    let reopened = Arc::new(Store::open(&tmp.path().join("hub.db")).unwrap());
    let log = EventLog::persistent(reopened.clone()).unwrap();
    let replayed = match log.replay_from(0) {
        batey::events::ReplayResult::Complete(e)
        | batey::events::ReplayResult::Partial { events: e, .. } => e,
    };
    assert!(
        replayed.iter().all(|e| e.session_id != doomed.id),
        "deleted chat's events came back after restart"
    );
    assert!(
        replayed.iter().any(|e| e.session_id == kept.id),
        "surviving chat lost its history"
    );
    assert!(reopened.chat(&doomed.id).is_err());
    assert!(reopened.chat(&kept.id).is_ok());
}
