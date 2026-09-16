//! Stable ACP v1 completeness: commands, modes, usage, message IDs,
//! session/delete, cancellation semantics, and metadata preservation.

use ::agent_client_protocol_schema::v1::{ContentBlock, ImageContent, TextContent};
use batey::{
    agents::{AgentDefinition, AgentRegistry},
    config::Config,
    events::{EventLog, EventPayload},
    service::HubService,
    session::SessionManager,
    store::Store,
};
use std::{sync::Arc, time::Duration};

fn hub(root: &std::path::Path) -> (Arc<HubService>, Arc<SessionManager>) {
    hub_with_mode(root, "load")
}

fn hub_with_mode(root: &std::path::Path, mode: &str) -> (Arc<HubService>, Arc<SessionManager>) {
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
        ]);
    let agents = Arc::new(AgentRegistry::new([agent]));
    let sessions = SessionManager::with_store(agents.clone(), log, Some(store.clone()));
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

async fn await_turn(log: &EventLog, chat_id: &str) {
    let mut rx = log.subscribe();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let event = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("turn did not finish in time")
            .unwrap();
        if event.session_id == chat_id {
            match event.payload {
                EventPayload::TurnComplete { .. } => return,
                EventPayload::Error { message } => panic!("turn failed: {message}"),
                _ => {}
            }
        }
    }
}

/// Starts a turn, retrying while the previous turn's guard is still held.
/// `await_turn` returns on the `TurnComplete` event, which is published just
/// before the turn guard is released, so an immediate follow-up prompt can
/// race with `"Agent busy"`.
async fn prompt_when_idle(hub: &HubService, chat_id: &str, text: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match hub.prompt_chat(chat_id, text.into()).await {
            Ok(()) => return,
            Err(e) if tokio::time::Instant::now() < deadline => {
                assert!(
                    e.to_string().contains("Agent busy"),
                    "unexpected prompt error: {e}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("prompt never admitted: {e}"),
        }
    }
}

/// Stops a chat, retrying while the just-finished turn still holds its
/// guard: `await_turn` returns on the `TurnComplete` event, which is
/// published just before the guard is released.
async fn stop_when_idle(hub: &HubService, chat_id: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match hub.stop_chat(chat_id).await {
            Ok(()) => return,
            Err(e) if tokio::time::Instant::now() < deadline => {
                assert!(
                    e.to_string().contains("Cancel the active turn"),
                    "unexpected stop error: {e}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("stop never admitted: {e}"),
        }
    }
}

/// Reads the current mode id from either wire casing. The backend preserves
/// the agent's camelCase wire object; merged updates also carry snake_case.
fn current_mode_id(modes: &serde_json::Value) -> &str {
    modes
        .get("current_mode_id")
        .or_else(|| modes.get("currentModeId"))
        .and_then(|v| v.as_str())
        .expect("modes snapshot has no current mode id")
}

async fn collect_text(log: &EventLog, chat_id: &str, start_seq: u64) -> String {
    match log.replay_from(start_seq) {
        batey::events::ReplayResult::Complete(events)
        | batey::events::ReplayResult::Partial { events, .. } => events
            .iter()
            .filter(|e| e.session_id == chat_id)
            .filter_map(|e| match &e.payload {
                EventPayload::MessageChunk { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

#[tokio::test]
async fn slash_commands_appear_and_invoke_as_prompt_text() {
    let tmp = tempfile::tempdir().unwrap();
    let (service, sessions) = hub(tmp.path());
    let log = sessions.event_log().clone();
    let project = service
        .create_project("demo".into(), tmp.path().display().to_string())
        .unwrap();
    let chat = service
        .create_chat(&project.id, "codex", None)
        .await
        .unwrap();

    prompt_when_idle(&service, &chat.chat.id, "commands").await;
    await_turn(&log, &chat.chat.id).await;

    let commands = service.chat_commands(&chat.chat.id).await.unwrap();
    let arr = commands.as_array().expect("commands array");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["name"], "plan");
    assert_eq!(arr[0]["input"]["hint"], "goal");

    // Invoking a command is ordinary prompt text, not a command RPC.
    prompt_when_idle(&service, &chat.chat.id, "/plan draft the router").await;
    await_turn(&log, &chat.chat.id).await;

    sessions.shutdown_all().await;
}

#[tokio::test]
async fn boolean_config_and_legacy_modes_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let (service, sessions) = hub(tmp.path());
    let log = sessions.event_log().clone();
    let project = service
        .create_project("demo".into(), tmp.path().display().to_string())
        .unwrap();
    let chat = service
        .create_chat(&project.id, "codex", None)
        .await
        .unwrap();

    // Stable boolean option works and retains generic metadata.
    let options = service.chat_config(&chat.chat.id).await.unwrap();
    assert!(options
        .as_array()
        .unwrap()
        .iter()
        .any(|o| o["id"] == "web_search" && o["type"] == "boolean"));
    service
        .set_chat_config(&chat.chat.id, "web_search", serde_json::json!(true))
        .await
        .unwrap();

    // Legacy modes work as fallback when no `mode` config category covers them.
    let modes = service.chat_modes(&chat.chat.id).await.unwrap();
    assert_eq!(current_mode_id(&modes), "ask");
    service.set_chat_mode(&chat.chat.id, "act").await.unwrap();
    let modes = service.chat_modes(&chat.chat.id).await.unwrap();
    assert_eq!(current_mode_id(&modes), "act");

    // Dynamic mode update during a session is preserved.
    prompt_when_idle(&service, &chat.chat.id, "modes").await;
    await_turn(&log, &chat.chat.id).await;
    let modes = service.chat_modes(&chat.chat.id).await.unwrap();
    assert_eq!(current_mode_id(&modes), "act");

    sessions.shutdown_all().await;
}

#[tokio::test]
async fn usage_message_ids_and_user_chunk_no_duplication() {
    let tmp = tempfile::tempdir().unwrap();
    let (service, sessions) = hub(tmp.path());
    let log = sessions.event_log().clone();
    let project = service
        .create_project("demo".into(), tmp.path().display().to_string())
        .unwrap();
    let chat = service
        .create_chat(&project.id, "codex", None)
        .await
        .unwrap();

    let start = log.next_seq();
    prompt_when_idle(&service, &chat.chat.id, "usage").await;
    await_turn(&log, &chat.chat.id).await;
    let usage = service.chat_usage(&chat.chat.id).await.unwrap();
    assert_eq!(usage["used"], 100);
    assert_eq!(usage["size"], 2000);

    let start2 = log.next_seq();
    prompt_when_idle(&service, &chat.chat.id, "message-id").await;
    await_turn(&log, &chat.chat.id).await;
    let text = collect_text(&log, &chat.chat.id, start2).await;
    assert!(text.contains("part-1"));
    assert!(text.contains("next"));
    // Message IDs survive through the event model.
    let events = match log.replay_from(start2) {
        batey::events::ReplayResult::Complete(e) => e,
        _ => panic!("expected complete"),
    };
    let chunks: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::MessageChunk { message_id, .. } => Some(message_id.clone()),
            _ => None,
        })
        .collect();
    assert!(chunks.iter().any(|id| id.as_deref() == Some("m1")));
    assert!(chunks.iter().any(|id| id.as_deref() == Some("m2")));

    // user_message_chunk reflections never duplicate local history.
    let before: Vec<_> = match log.replay_from(start) {
        batey::events::ReplayResult::Complete(e) => e,
        _ => panic!("expected complete"),
    }
    .into_iter()
    .filter(|e| {
        e.session_id == chat.chat.id && matches!(e.payload, EventPayload::UserMessage { .. })
    })
    .collect();
    let user_count_before = before.len();
    prompt_when_idle(&service, &chat.chat.id, "user-chunk").await;
    await_turn(&log, &chat.chat.id).await;
    let after: Vec<_> = match log.replay_from(start) {
        batey::events::ReplayResult::Complete(e) => e,
        _ => panic!("expected complete"),
    }
    .into_iter()
    .filter(|e| {
        e.session_id == chat.chat.id && matches!(e.payload, EventPayload::UserMessage { .. })
    })
    .collect();
    // One new local user message, no duplicate from the reflected chunk.
    assert_eq!(after.len(), user_count_before + 1);

    sessions.shutdown_all().await;
}

#[tokio::test]
async fn remote_session_delete_is_capability_gated() {
    let tmp = tempfile::tempdir().unwrap();
    let (service, sessions) = hub(tmp.path());
    let project = service
        .create_project("demo".into(), tmp.path().display().to_string())
        .unwrap();
    let chat = service
        .create_chat(&project.id, "codex", None)
        .await
        .unwrap();
    // A second chat gives the agent a second remote session to delete.
    // Agent-side sessions are files in the shared history dir, so both
    // chats see each other's remote sessions here.
    let other = service
        .create_chat(&project.id, "codex", None)
        .await
        .unwrap();
    service.remote_sessions(&other.chat.id, None).await.unwrap();

    let listed = service.remote_sessions(&chat.chat.id, None).await.unwrap();
    let sessions_arr = listed["sessions"].as_array().unwrap().clone();
    let live_id = service
        .get_chat(&chat.chat.id)
        .await
        .unwrap()
        .chat
        .acp_session_id
        .expect("resume links an agent session");
    // Deleting the agent session backing this chat is refused: the next
    // restart would otherwise try to resume a deliberately deleted agent
    // session instead of reporting the saved chat cleanly.
    assert!(sessions_arr.iter().any(|s| s["sessionId"] == live_id));
    assert!(service
        .delete_remote_session(&chat.chat.id, &live_id)
        .await
        .is_err());
    // An unlinked remote session deletes normally; the Batey chat remains.
    let mut target: Option<String> = None;
    for s in &sessions_arr {
        let sid = s["sessionId"].as_str().unwrap_or("<missing>");
        if sid != live_id {
            target = Some(sid.to_string());
            break;
        }
    }
    let target = target.expect("a second remote session");
    service
        .delete_remote_session(&chat.chat.id, &target)
        .await
        .unwrap();
    let listed2 = service.remote_sessions(&chat.chat.id, None).await.unwrap();
    assert!(!listed2["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["sessionId"] == target));
    assert!(service.get_chat(&chat.chat.id).await.is_ok());

    // Stop and restart: the chat still resumes against its linked agent
    // session, which the deletion above never touched.
    sessions.shutdown_all().await;
    let (restarted, restarted_sessions) = hub(tmp.path());
    restarted.resume_chat(&chat.chat.id).await.unwrap();
    assert_eq!(
        restarted
            .get_chat(&chat.chat.id)
            .await
            .unwrap()
            .chat
            .acp_session_id
            .as_deref(),
        Some(live_id.as_str())
    );
    restarted_sessions.shutdown_all().await;
}

#[tokio::test]
async fn rich_prompt_and_agent_chunks_preserve_order_and_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let (service, sessions) = hub(tmp.path());
    let log = sessions.event_log().clone();
    let project = service
        .create_project("demo".into(), tmp.path().display().to_string())
        .unwrap();
    let chat = service
        .create_chat(&project.id, "codex", None)
        .await
        .unwrap();
    let content = vec![
        ContentBlock::Text(TextContent::new("rich-output")),
        ContentBlock::Image(ImageContent::new("iVBORw0KGgo=", "image/png")),
    ];
    service
        .prompt_chat_content(&chat.chat.id, content)
        .await
        .unwrap();
    await_turn(&log, &chat.chat.id).await;
    let events = match log.replay_from(1) {
        batey::events::ReplayResult::Complete(events) => events,
        _ => panic!("expected complete replay"),
    };
    let user = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::UserMessage {
                content,
                message_id,
                ..
            } if event.session_id == chat.chat.id => Some((content, message_id)),
            _ => None,
        })
        .unwrap();
    assert!(matches!(
        user.0.as_slice(),
        [ContentBlock::Text(_), ContentBlock::Image(_)]
    ));
    assert!(user.1.is_some());
    let chunks = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::MessageChunk {
                content,
                message_id,
                ..
            } if event.session_id == chat.chat.id => Some((content, message_id)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        matches!(chunks.as_slice(), [(first, Some(id)), (second, Some(id2))] if matches!(first.as_slice(), [ContentBlock::Text(_)]) && matches!(second.as_slice(), [ContentBlock::ResourceLink(_)]) && id == "rich-1" && id2 == "rich-1")
    );
    assert!(events.iter().any(|event| matches!(&event.payload, EventPayload::ThoughtChunk { content, message_id: Some(id), .. } if matches!(content.as_slice(), [ContentBlock::Resource(_)]) && id == "thought-1")));
    sessions.shutdown_all().await;
}

#[tokio::test]
async fn unsupported_rich_prompt_capabilities_fail_before_user_persistence() {
    for mode in ["no-rich", "null-rich", "object-rich"] {
        let tmp = tempfile::tempdir().unwrap();
        let (service, sessions) = hub_with_mode(tmp.path(), mode);
        let log = sessions.event_log().clone();
        let project = service
            .create_project("demo".into(), tmp.path().display().to_string())
            .unwrap();
        let chat = service
            .create_chat(&project.id, "codex", None)
            .await
            .unwrap();
        let error = service
            .prompt_chat_content(
                &chat.chat.id,
                vec![ContentBlock::Image(ImageContent::new(
                    "iVBORw0KGgo=",
                    "image/png",
                ))],
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("image prompt capability"),
            "{mode}: {error}"
        );
        let events = match log.replay_from(1) {
            batey::events::ReplayResult::Complete(events) => events,
            _ => panic!("expected complete replay"),
        };
        assert!(!events.iter().any(|event| {
            event.session_id == chat.chat.id
                && matches!(event.payload, EventPayload::UserMessage { .. })
        }));
        sessions.shutdown_all().await;
    }
}

#[tokio::test]
async fn initial_tool_call_rich_content_is_preserved() {
    let tmp = tempfile::tempdir().unwrap();
    let (service, sessions) = hub(tmp.path());
    let log = sessions.event_log().clone();
    let project = service
        .create_project("demo".into(), tmp.path().display().to_string())
        .unwrap();
    let chat = service
        .create_chat(&project.id, "codex", None)
        .await
        .unwrap();

    prompt_when_idle(&service, &chat.chat.id, "tool-rich").await;
    await_turn(&log, &chat.chat.id).await;
    let events = match log.replay_from(1) {
        batey::events::ReplayResult::Complete(events) => events,
        _ => panic!("expected complete replay"),
    };
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ToolCall { content: Some(content), .. }
            if matches!(content.as_array().map(Vec::as_slice), Some([first, second, third])
                if first["content"]["type"] == "text"
                    && second["content"]["type"] == "resource_link"
                    && third["content"]["type"] == "image")
    )));
    sessions.shutdown_all().await;
}

#[tokio::test]
async fn tool_locations_and_session_info_preserved() {
    let tmp = tempfile::tempdir().unwrap();
    let (service, sessions) = hub(tmp.path());
    let log = sessions.event_log().clone();
    let project = service
        .create_project("demo".into(), tmp.path().display().to_string())
        .unwrap();
    let chat = service
        .create_chat(&project.id, "codex", None)
        .await
        .unwrap();

    prompt_when_idle(&service, &chat.chat.id, "tool-loc").await;
    await_turn(&log, &chat.chat.id).await;
    let events = match log.replay_from(1) {
        batey::events::ReplayResult::Complete(e) => e,
        _ => panic!("expected complete"),
    };
    assert!(events.iter().any(|e| matches!(
        &e.payload,
        EventPayload::ToolCall {
            locations: Some(_),
            ..
        }
    )));

    prompt_when_idle(&service, &chat.chat.id, "title:Hello World").await;
    await_turn(&log, &chat.chat.id).await;
    assert_eq!(
        service.get_chat(&chat.chat.id).await.unwrap().chat.title,
        "Hello World"
    );

    sessions.shutdown_all().await;
}

fn count_payload(log: &EventLog, chat_id: &str, ty: &str) -> usize {
    match log.replay_from(1) {
        batey::events::ReplayResult::Complete(events) => events
            .iter()
            .filter(|e| e.session_id == chat_id)
            .filter(|e| serde_json::to_value(&e.payload).unwrap()["type"] == ty)
            .count(),
        _ => panic!("expected complete"),
    }
}

#[tokio::test]
async fn stop_reconnect_retains_dynamic_state_without_duplicating_history() {
    let tmp = tempfile::tempdir().unwrap();
    let (service, sessions) = hub(tmp.path());
    let log = sessions.event_log().clone();
    let project = service
        .create_project("demo".into(), tmp.path().display().to_string())
        .unwrap();
    let chat = service
        .create_chat(&project.id, "codex", None)
        .await
        .unwrap();

    // Populate live dynamic state, then stop the agent process.
    prompt_when_idle(&service, &chat.chat.id, "commands").await;
    await_turn(&log, &chat.chat.id).await;
    prompt_when_idle(&service, &chat.chat.id, "usage").await;
    await_turn(&log, &chat.chat.id).await;
    let config_before = count_payload(&log, &chat.chat.id, "config_options");
    let commands_before = count_payload(&log, &chat.chat.id, "available_commands");
    let usage_before = count_payload(&log, &chat.chat.id, "usage_update");
    assert!(commands_before > 0 && usage_before > 0);
    stop_when_idle(&service, &chat.chat.id).await;

    // Resume replays the agent history: snapshots sent during replay must
    // be retained in queryable state...
    service.resume_chat(&chat.chat.id).await.unwrap();
    let commands = service.chat_commands(&chat.chat.id).await.unwrap();
    assert_eq!(commands.as_array().unwrap().len(), 2);
    let usage = service.chat_usage(&chat.chat.id).await.unwrap();
    assert_eq!(usage["used"], 100);
    let modes = service.chat_modes(&chat.chat.id).await.unwrap();
    assert_eq!(
        modes.get("current_mode_id").or(modes.get("currentModeId")),
        Some(&serde_json::json!("ask"))
    );
    let options = service.chat_config(&chat.chat.id).await.unwrap();
    assert!(options
        .as_array()
        .unwrap()
        .iter()
        .any(|o| o["id"] == "web_search"));

    // ...without duplicating durable history: replayed snapshots add no
    // events of their own. Only the single ensure_running config snapshot
    // may follow the resume, never a replayed duplicate.
    assert_eq!(
        count_payload(&log, &chat.chat.id, "available_commands"),
        commands_before + 1
    );
    assert_eq!(
        count_payload(&log, &chat.chat.id, "usage_update"),
        usage_before
    );
    assert_eq!(
        count_payload(&log, &chat.chat.id, "config_options"),
        config_before + 1
    );
    let history = match log.replay_from(1) {
        batey::events::ReplayResult::Complete(e) => e,
        _ => panic!("expected complete"),
    };
    assert!(!history.iter().any(|e| matches!(
        &e.payload,
        EventPayload::MessageChunk { text, .. } if text == "REPLAY"
    )));

    sessions.shutdown_all().await;
}

/// Minimal peer: answers `initialize`, records every `$/cancel_request`
/// id it observes, and hangs on anything else.
const HANG_PEER: &str = r#"
import json
import sys

cancel_log = sys.argv[1]

def send(obj):
    print(json.dumps({"jsonrpc": "2.0", **obj}), flush=True)

for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    if method == "$/cancel_request":
        with open(cancel_log, "a") as handle:
            handle.write(str(msg["params"]["requestId"]) + "\n")
    elif method == "initialize":
        send({"id": msg["id"], "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    # Anything else hangs: no reply is ever sent.
"#;

fn observed_cancels(log: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

#[tokio::test]
async fn timed_out_requests_emit_cancel_request() {
    use batey::acp::{AcpClient, RequestTimedOut, StderrPolicy};

    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("hang_peer.py");
    std::fs::write(&script, HANG_PEER).unwrap();
    let cancel_log = tmp.path().join("cancels.log");
    let tracker = Arc::new(batey::tasks::TerminalTaskTracker::default());
    // The child needs a real environment (notably PATH) to exec python3.
    let env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let client = AcpClient::spawn(
        "python3",
        &[
            script.display().to_string(),
            cancel_log.display().to_string(),
        ],
        &env,
        tmp.path(),
        "sess".into(),
        "test-agent".into(),
        Arc::new(EventLog::new(100)),
        None,
        tracker,
        vec![tmp.path().canonicalize().unwrap()],
        // An ordinary protocol peer: its stderr stays diagnostic material.
        StderrPolicy::Log,
    )
    .await
    .unwrap();
    client.initialize(tmp.path()).await.unwrap();

    // An answered request is never protocol-cancelled.
    client.initialize(tmp.path()).await.unwrap();
    assert!(observed_cancels(&cancel_log).is_empty());

    // An abandoned request is protocol-cancelled with its own id, which
    // stays distinct from whole-turn `session/cancel` semantics.
    let error = client
        .send_request_with_timeout(
            "test/hang",
            serde_json::json!({}),
            Duration::from_millis(300),
        )
        .await
        .unwrap_err();
    assert!(error.is::<RequestTimedOut>());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let cancels = observed_cancels(&cancel_log);
        if cancels.len() == 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "peer never observed $/cancel_request, got {cancels:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    client.shutdown().await;
}

#[tokio::test]
async fn user_message_identity_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let (service, sessions) = hub(tmp.path());
    let log = sessions.event_log().clone();
    let project = service
        .create_project("demo".into(), tmp.path().display().to_string())
        .unwrap();
    let chat = service
        .create_chat(&project.id, "codex", None)
        .await
        .unwrap();

    // The fake peer echoes the `_meta` identity it observed in the prompt
    // request back in its reply text and response `_meta`.
    let start = log.next_seq();
    prompt_when_idle(&service, &chat.chat.id, "identity: who are you").await;
    await_turn(&log, &chat.chat.id).await;

    // The durable user message persists the sent identity.
    let durable_id = match log.replay_from(start) {
        batey::events::ReplayResult::Complete(events) => events
            .into_iter()
            .filter(|e| e.session_id == chat.chat.id)
            .find_map(|e| match e.payload {
                EventPayload::UserMessage { message_id, .. } => message_id,
                _ => None,
            })
            .expect("durable user message carries its identity"),
        _ => panic!("expected complete"),
    };
    // The agent observed and returned that same identity: send, persist,
    // and correlate all agree end to end.
    let text = collect_text(&log, &chat.chat.id, start).await;
    assert_eq!(text, format!("identity:{durable_id}"));

    sessions.shutdown_all().await;
}
