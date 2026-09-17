//! The Hub service drives a real ACP agent with no HTTP in the picture.
//!
//! This is the proof that a later MCP or federation surface can reuse these
//! operations instead of reimplementing chat and session behavior.
use batey::{
    agents::{AgentDefinition, AgentRegistry},
    config::Config,
    events::{EventLog, EventPayload},
    service::{ChatEdit, HubService, ServiceError},
    session::SessionManager,
    store::{McpServerInput, McpTransport, SecretEdit, SecretInput, Store},
};
use std::{sync::Arc, time::Duration};

fn hub(root: &std::path::Path) -> (Arc<HubService>, Arc<SessionManager>) {
    let store = Arc::new(Store::open(&root.join("hub.db")).unwrap());
    let log = Arc::new(EventLog::persistent(store.clone()).unwrap());
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

/// Waits for the turn the prompt started to finish.
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

/// Applies a configuration change once the turn lock is free.
async fn set_config_when_idle(
    hub: &HubService,
    chat_id: &str,
    option_id: &str,
    value: serde_json::Value,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match hub.set_chat_config(chat_id, option_id, value.clone()).await {
            Ok(_) => return,
            Err(e) if tokio::time::Instant::now() < deadline => {
                assert!(
                    e.to_string().contains("Wait for the active turn"),
                    "unexpected error: {e}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("configuration never applied: {e}"),
        }
    }
}

#[tokio::test]
async fn full_chat_lifecycle_without_http() {
    let tmp = tempfile::tempdir().unwrap();
    let (service, sessions) = hub(tmp.path());
    let log = sessions.event_log().clone();

    let project = service
        .create_project("demo".into(), tmp.path().display().to_string())
        .unwrap();
    assert_eq!(service.list_projects().unwrap().len(), 1);

    let chat = service
        .create_chat(&project.id, "codex", None)
        .await
        .unwrap();
    assert_eq!(chat.process_state, "STOPPED");
    assert_eq!(chat.turn_state, "IDLE");
    assert!(chat.workspace.is_none());
    assert_eq!(service.list_chats(&project.id).await.unwrap().len(), 1);

    service
        .prompt_chat(&chat.chat.id, "hello".into())
        .await
        .unwrap();
    await_turn(&log, &chat.chat.id).await;
    assert_eq!(
        service.get_chat(&chat.chat.id).await.unwrap().process_state,
        "RUNNING"
    );

    let options = service.chat_config(&chat.chat.id).await.unwrap();
    let option_id = options[0]["id"].as_str().unwrap().to_string();
    // `TurnComplete` is appended before `ask` returns, so the turn lock can
    // still be held for a moment after the event arrives.
    set_config_when_idle(
        &service,
        &chat.chat.id,
        &option_id,
        serde_json::json!("large"),
    )
    .await;
    assert_eq!(
        service
            .get_chat(&chat.chat.id)
            .await
            .unwrap()
            .chat
            .config_values[&option_id],
        serde_json::json!("large")
    );

    let renamed = service
        .edit_chat(
            &chat.chat.id,
            ChatEdit {
                title: Some("renamed".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(renamed.chat.title, "renamed");
    assert!(renamed.chat.title_overridden);

    service.stop_chat(&chat.chat.id).await.unwrap();
    assert_eq!(
        service.get_chat(&chat.chat.id).await.unwrap().process_state,
        "STOPPED"
    );

    sessions.shutdown_all().await;
    let (restarted_hub, restarted_sessions) = hub(tmp.path());
    assert_eq!(
        restarted_hub
            .get_chat(&chat.chat.id)
            .await
            .unwrap()
            .process_state,
        "STOPPED"
    );
    restarted_hub.delete_chat(&chat.chat.id).await.unwrap();
    assert!(restarted_hub
        .list_chats(&project.id)
        .await
        .unwrap()
        .is_empty());
    restarted_hub.delete_project(&project.id).await.unwrap();
    assert!(restarted_hub.list_projects().unwrap().is_empty());

    restarted_sessions.shutdown_all().await;
}

#[tokio::test]
async fn prompt_admission_updates_chat_activity_at_user_event_time() {
    let tmp = tempfile::tempdir().unwrap();
    let (service, sessions) = hub(tmp.path());
    let project = service
        .create_project("demo".into(), tmp.path().display().to_string())
        .unwrap();
    let chat = service
        .create_chat(&project.id, "codex", None)
        .await
        .unwrap();
    let before = chat.chat.updated_at.clone();
    let mut events = sessions.event_log().subscribe();

    service
        .prompt_chat(&chat.chat.id, "activity timestamp".into())
        .await
        .unwrap();

    let user_event = loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        if event.session_id == chat.chat.id
            && matches!(event.payload, EventPayload::UserMessage { .. })
        {
            break event;
        }
    };
    let updated = service.get_chat(&chat.chat.id).await.unwrap();
    assert_ne!(updated.chat.updated_at, before);
    assert_eq!(updated.chat.updated_at, user_event.timestamp.to_rfc3339());
    assert_eq!(
        updated.turn_started_at,
        Some(user_event.timestamp),
        "the active turn start must come from durable events, not the bounded history page"
    );
    sessions.shutdown_all().await;
}

#[tokio::test]
async fn title_rename_is_live_but_guarded_compound_edits_are_atomic() {
    let tmp = tempfile::tempdir().unwrap();
    let (hub, sessions) = hub(tmp.path());
    let project = hub
        .create_project("demo".into(), tmp.path().display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    hub.prompt_chat(&chat.chat.id, "wait".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let current = sessions.get_by_id(&chat.chat.id).await.unwrap();
        if current.turn_state().await == batey::state::TurnState::Prompting {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "turn never became active"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let renamed = hub
        .edit_chat(
            &chat.chat.id,
            ChatEdit {
                title: Some("Live manual title".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(renamed.chat.title, "Live manual title");
    assert!(renamed.chat.title_overridden);
    assert_eq!(renamed.turn_state, "PROMPTING");

    let rejected = hub
        .edit_chat(
            &chat.chat.id,
            ChatEdit {
                title: Some("Must not apply".into()),
                archived: Some(true),
            },
        )
        .await;
    assert!(rejected.is_err());
    let unchanged = hub.get_chat(&chat.chat.id).await.unwrap();
    assert_eq!(unchanged.chat.title, "Live manual title");
    assert!(!unchanged.chat.archived);

    sessions.shutdown_all().await;
}

#[tokio::test]
async fn generated_title_before_manual_rename_and_after_it() {
    let tmp = tempfile::tempdir().unwrap();
    let (hub, sessions) = hub(tmp.path());
    let project = hub
        .create_project("demo".into(), tmp.path().display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let session = sessions.get_by_id(&chat.chat.id).await.unwrap();

    session
        .ask("title: Generated before rename".into(), None)
        .await
        .unwrap();
    assert_eq!(
        hub.get_chat(&chat.chat.id).await.unwrap().chat.title,
        "Generated before rename"
    );
    assert!(
        !hub.get_chat(&chat.chat.id)
            .await
            .unwrap()
            .chat
            .title_overridden
    );

    hub.edit_chat(
        &chat.chat.id,
        ChatEdit {
            title: Some("Manual winner".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    session
        .ask("title: Generated after rename".into(), None)
        .await
        .unwrap();
    let final_chat = hub.get_chat(&chat.chat.id).await.unwrap().chat;
    assert_eq!(final_chat.title, "Manual winner");
    assert!(final_chat.title_overridden);
    sessions.shutdown_all().await;
}

#[tokio::test]
async fn failed_turn_emits_error_then_exactly_one_completion() {
    let tmp = tempfile::tempdir().unwrap();
    let (hub, sessions) = hub(tmp.path());
    let log = sessions.event_log().clone();
    let project = hub
        .create_project("demo".into(), tmp.path().display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let mut events = log.subscribe();

    hub.prompt_chat(&chat.chat.id, "rpc-error".into())
        .await
        .unwrap();

    let mut terminal_events = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while terminal_events.len() < 2 {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("failed turn did not finish in time")
            .unwrap();
        if event.session_id == chat.chat.id
            && matches!(
                &event.payload,
                EventPayload::Error { .. } | EventPayload::TurnComplete { .. }
            )
        {
            terminal_events.push(event.payload);
        }
    }

    assert!(matches!(terminal_events[0], EventPayload::Error { .. }));
    assert!(matches!(
        terminal_events[1],
        EventPayload::TurnComplete { ref stop_reason } if stop_reason == "error"
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;
    let completions = log
        .replay_page(1, log.high_watermark().unwrap(), 10_000)
        .unwrap()
        .into_iter()
        .filter(|event| {
            event.session_id == chat.chat.id
                && matches!(event.payload, EventPayload::TurnComplete { .. })
        })
        .count();
    assert_eq!(completions, 1);

    sessions.shutdown_all().await;
}

#[tokio::test]
async fn errors_carry_the_kind_the_caller_needs() {
    let tmp = tempfile::tempdir().unwrap();
    let (hub, sessions) = hub(tmp.path());
    let project = hub
        .create_project("demo".into(), tmp.path().display().to_string())
        .unwrap();

    // An unknown chat is not found, whichever operation asks for it.
    assert!(matches!(
        hub.cancel_chat("nope").await,
        Err(ServiceError::NotFound(_))
    ));
    assert!(matches!(
        hub.edit_chat("nope", ChatEdit::default()).await,
        Err(ServiceError::NotFound(_))
    ));

    // An agent that is not configured is a bad request, not a missing chat.
    assert!(matches!(
        hub.create_chat(&project.id, "gemini", None).await,
        Err(ServiceError::Invalid(m)) if m == "Unknown agent"
    ));

    // A project that still holds chats cannot move, but deleting it cascades
    // to delete its chats too.
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let other = tmp.path().join("elsewhere");
    std::fs::create_dir_all(&other).unwrap();
    assert!(matches!(
        hub.edit_project(&project.id, "demo".into(), other.display().to_string()),
        Err(ServiceError::Conflict(_))
    ));
    hub.delete_project(&project.id).await.unwrap();
    assert!(hub.get_chat(&chat.chat.id).await.is_err());
    assert!(hub.get_project(&project.id).is_err());

    // A path outside the configured roots is refused.
    assert!(matches!(
        hub.create_project("escape".into(), "/".into()),
        Err(ServiceError::Invalid(_))
    ));

    // Missing get_chat returns NotFound
    assert!(matches!(
        hub.get_chat("nope").await,
        Err(ServiceError::NotFound(_))
    ));

    // Missing get_project returns NotFound
    assert!(matches!(
        hub.get_project("nope"),
        Err(ServiceError::NotFound(_))
    ));

    // Missing-project list_chats returns NotFound
    assert!(matches!(
        hub.list_chats("nope").await,
        Err(ServiceError::NotFound(_))
    ));

    // Missing-project create_chat returns NotFound
    assert!(matches!(
        hub.create_chat("nope", "codex", None).await,
        Err(ServiceError::NotFound(_))
    ));

    // An empty prompt never reaches the agent.
    assert!(matches!(
        hub.prompt_chat(&chat.chat.id, "   ".into()).await,
        Err(ServiceError::Invalid(_))
    ));

    // An internal SQLite failure returns ServiceError::Internal.
    {
        let raw = rusqlite::Connection::open(tmp.path().join("hub.db")).unwrap();
        raw.execute_batch("DROP TABLE projects;").unwrap();
    }
    assert!(matches!(
        hub.list_projects(),
        Err(ServiceError::Internal(_))
    ));

    sessions.shutdown_all().await;
}

#[tokio::test]
async fn rejected_saved_config_blocks_until_only_that_option_is_reset() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&root.path().join("hub.db")).unwrap());
    let log = Arc::new(EventLog::persistent(store.clone()).unwrap());
    let history = root.path().join("history");
    std::fs::create_dir_all(&history).unwrap();
    let agent = AgentDefinition::codex_default()
        .with_command("python3".into())
        .with_args(vec![
            format!("{}/tests/fake_acp.py", env!("CARGO_MANIFEST_DIR")),
            history.display().to_string(),
            "reject-config".into(),
        ]);
    let agents = Arc::new(AgentRegistry::new([agent]));
    let sessions = SessionManager::with_store(agents.clone(), log, Some(store.clone()));
    let mut config = Config {
        agents: agents.clone(),
        ..Default::default()
    };
    config.web.project_roots = vec![root.path().display().to_string()];
    let hub = HubService::new(store.clone(), sessions.clone(), agents, &config);
    let project = hub
        .create_project("demo".into(), root.path().display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    hub.resume_chat(&chat.chat.id).await.unwrap();
    hub.stop_chat(&chat.chat.id).await.unwrap();
    store
        .update_chat(&chat.chat.id, |chat| {
            chat.config_values["model"] = serde_json::json!("large");
        })
        .unwrap();

    assert!(matches!(
        hub.resume_chat(&chat.chat.id).await,
        Err(ServiceError::SavedConfigRejected { option_id, .. }) if option_id == "model"
    ));
    hub.clear_saved_config(&chat.chat.id, "model")
        .await
        .unwrap();
    hub.resume_chat(&chat.chat.id).await.unwrap();
    assert!(store
        .chat(&chat.chat.id)
        .unwrap()
        .config_values
        .get("model")
        .is_none());
    sessions.shutdown_all().await;
}

#[tokio::test]
async fn transient_saved_config_failure_preserves_values_for_retry() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&root.path().join("hub.db")).unwrap());
    let log = Arc::new(EventLog::persistent(store.clone()).unwrap());
    let history = root.path().join("history");
    std::fs::create_dir_all(&history).unwrap();
    let agent = AgentDefinition::codex_default()
        .with_command("python3".into())
        .with_args(vec![
            format!("{}/tests/fake_acp.py", env!("CARGO_MANIFEST_DIR")),
            history.display().to_string(),
            "transient-config".into(),
        ]);
    let agents = Arc::new(AgentRegistry::new([agent]));
    let sessions = SessionManager::with_store(agents.clone(), log, Some(store.clone()));
    let mut config = Config {
        agents: agents.clone(),
        ..Default::default()
    };
    config.web.project_roots = vec![root.path().display().to_string()];
    let hub = HubService::new(store.clone(), sessions.clone(), agents, &config);
    let project = hub
        .create_project("demo".into(), root.path().display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    hub.resume_chat(&chat.chat.id).await.unwrap();
    hub.stop_chat(&chat.chat.id).await.unwrap();
    store
        .update_chat(&chat.chat.id, |chat| {
            chat.config_values["model"] = serde_json::json!("large");
        })
        .unwrap();

    // A transient reapply failure must NOT become a saved-config rejection,
    // so the frontend shows Retry rather than Reset.
    let first = hub.resume_chat(&chat.chat.id).await;
    assert!(
        !matches!(&first, Err(ServiceError::SavedConfigRejected { .. })),
        "transient failure must not map to SavedConfigRejected: {first:?}"
    );
    assert!(first.is_err(), "transient failure must still fail resume");
    // Retry retains the saved option.
    assert_eq!(
        store.chat(&chat.chat.id).unwrap().config_values["model"],
        serde_json::json!("large")
    );
    let second = hub.resume_chat(&chat.chat.id).await;
    assert!(
        !matches!(&second, Err(ServiceError::SavedConfigRejected { .. })),
        "retry must not map to SavedConfigRejected: {second:?}"
    );
    assert_eq!(
        store.chat(&chat.chat.id).unwrap().config_values["model"],
        serde_json::json!("large")
    );
    sessions.shutdown_all().await;
}

#[tokio::test]
async fn locally_invalid_stale_saved_config_is_rejection() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&root.path().join("hub.db")).unwrap());
    let log = Arc::new(EventLog::persistent(store.clone()).unwrap());
    let history = root.path().join("history");
    std::fs::create_dir_all(&history).unwrap();
    let agent = AgentDefinition::codex_default()
        .with_command("python3".into())
        .with_args(vec![
            format!("{}/tests/fake_acp.py", env!("CARGO_MANIFEST_DIR")),
            history.display().to_string(),
            "load".into(),
        ]);
    let agents = Arc::new(AgentRegistry::new([agent]));
    let sessions = SessionManager::with_store(agents.clone(), log, Some(store.clone()));
    let mut config = Config {
        agents: agents.clone(),
        ..Default::default()
    };
    config.web.project_roots = vec![root.path().display().to_string()];
    let hub = HubService::new(store.clone(), sessions.clone(), agents, &config);
    let project = hub
        .create_project("demo".into(), root.path().display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    hub.resume_chat(&chat.chat.id).await.unwrap();
    hub.stop_chat(&chat.chat.id).await.unwrap();
    store
        .update_chat(&chat.chat.id, |chat| {
            chat.config_values["model"] = serde_json::json!("bogus-model");
        })
        .unwrap();

    assert!(matches!(
        hub.resume_chat(&chat.chat.id).await,
        Err(ServiceError::SavedConfigRejected { option_id, .. }) if option_id == "model"
    ));
    // The stale value is preserved until the user explicitly resets it.
    assert_eq!(
        store.chat(&chat.chat.id).unwrap().config_values["model"],
        serde_json::json!("bogus-model")
    );
    hub.clear_saved_config(&chat.chat.id, "model")
        .await
        .unwrap();
    hub.resume_chat(&chat.chat.id).await.unwrap();
    sessions.shutdown_all().await;
}

#[tokio::test]
async fn metadata_mutation_succeeds_when_invalidation_publish_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let (hub, sessions) = hub(tmp.path());
    let project = hub
        .create_project("demo".into(), tmp.path().display().to_string())
        .unwrap();
    // Break post-commit invalidation publishing without touching the
    // authoritative project/chat tables. The first failed append poisons the
    // in-memory EventLog (fail-closed for turns), so later publishes keep
    // failing even after the table is restored.
    {
        let raw = rusqlite::Connection::open(tmp.path().join("hub.db")).unwrap();
        raw.execute_batch("DROP TABLE events;").unwrap();
    }
    // Every metadata mutation below must still succeed: the SQLite rows are
    // authoritative and the failed publish is only logged.
    let second = hub
        .create_project("second".into(), tmp.path().display().to_string())
        .unwrap();
    assert_eq!(second.name, "second");
    let edited = hub
        .edit_project(&second.id, "second-renamed".into(), second.path.clone())
        .unwrap();
    assert_eq!(edited.name, "second-renamed");
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let renamed = hub
        .edit_chat(
            &chat.chat.id,
            ChatEdit {
                title: Some("renamed".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(renamed.chat.title, "renamed");
    // Restore the events table so `delete_chat` (which deletes that chat's
    // event rows in the same transaction) can run. The EventLog stays
    // poisoned, so the invalidation publish still fails and must stay
    // best-effort.
    {
        let raw = rusqlite::Connection::open(tmp.path().join("hub.db")).unwrap();
        raw.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (seq INTEGER PRIMARY KEY, data TEXT NOT NULL, session_id TEXT NOT NULL DEFAULT ''); \
             CREATE INDEX IF NOT EXISTS idx_events_session_id ON events(session_id);",
        )
        .unwrap();
    }
    hub.delete_chat(&chat.chat.id).await.unwrap();
    hub.delete_project(&second.id).await.unwrap();
    sessions.shutdown_all().await;
}

#[tokio::test]
async fn concurrent_wait_admission_and_rejected_second_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let (hub, sessions) = hub(tmp.path());
    let log = sessions.event_log().clone();
    let project = hub
        .create_project("demo".into(), tmp.path().display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    let mut events = log.subscribe();

    // A first "wait" prompt is admitted.
    hub.prompt_chat(&chat.chat.id, "wait".into()).await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("did not receive prompt event in time")
            .unwrap();
        if event.session_id == chat.chat.id
            && matches!(event.payload, EventPayload::UserMessage { ref text, .. } if text == "wait")
        {
            break;
        }
    }

    // A second prompt while "wait" is active returns an error immediately.
    let second = hub.prompt_chat(&chat.chat.id, "second prompt".into()).await;
    assert!(
        second.is_err(),
        "second prompt must be rejected immediately"
    );

    // Allow any potential background tasks to run.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Cancelling/completing the original "wait" produces exactly one completion for that turn.
    hub.cancel_chat(&chat.chat.id).await.unwrap();

    loop {
        let event = tokio::time::timeout_at(deadline + Duration::from_secs(10), events.recv())
            .await
            .expect("cancelled turn did not complete in time")
            .unwrap();
        if event.session_id == chat.chat.id
            && matches!(event.payload, EventPayload::TurnComplete { .. })
        {
            break;
        }
    }

    // The second request produces zero UserMessage, Error, and TurnComplete events.
    let all_events = log
        .replay_page(1, log.high_watermark().unwrap(), 10_000)
        .unwrap();
    let chat_events: Vec<_> = all_events
        .into_iter()
        .filter(|e| e.session_id == chat.chat.id)
        .collect();

    let user_messages: Vec<_> = chat_events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::UserMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(user_messages, vec!["wait"]);

    let errors: Vec<_> = chat_events
        .iter()
        .filter(|e| matches!(e.payload, EventPayload::Error { .. }))
        .collect();
    assert_eq!(errors.len(), 0);

    let completions: Vec<_> = chat_events
        .iter()
        .filter(|e| matches!(e.payload, EventPayload::TurnComplete { .. }))
        .collect();
    assert_eq!(completions.len(), 1);

    sessions.shutdown_all().await;
}

#[tokio::test]
async fn mcp_secret_edits_are_redacted_and_keep_replace_remove_are_exact() {
    let tmp = tempfile::tempdir().unwrap();
    let (service, sessions) = hub(tmp.path());
    let project = service
        .create_project("demo".into(), tmp.path().display().to_string())
        .unwrap();
    let chat = service
        .create_chat(&project.id, "codex", None)
        .await
        .unwrap();
    let input = McpServerInput {
        name: "tools".into(),
        transport: McpTransport::Stdio,
        url: None,
        command: Some("/bin/echo".into()),
        args: vec!["one".into(), "two".into()],
        secrets: vec![SecretInput {
            name: "TOKEN".into(),
            value: Some("initial-secret".into()),
            action: SecretEdit::Replace,
        }],
    };
    let created = service
        .create_mcp_server(&chat.chat.id, input)
        .await
        .unwrap();
    assert_eq!(created[0].args, ["one", "two"]);
    assert!(created[0].secrets[0].present);
    assert!(!serde_json::to_string(&created)
        .unwrap()
        .contains("initial-secret"));
    let id = created[0].id.clone();

    let edit = |action, value: Option<&str>| McpServerInput {
        name: "tools".into(),
        transport: McpTransport::Stdio,
        url: None,
        command: Some("/bin/echo".into()),
        args: vec!["package".into(), "subcommand".into()],
        secrets: vec![SecretInput {
            name: "TOKEN".into(),
            value: value.map(str::to_string),
            action,
        }],
    };
    service
        .edit_mcp_server(&chat.chat.id, &id, edit(SecretEdit::Keep, None))
        .await
        .unwrap();
    let raw = Store::open(&tmp.path().join("hub.db"))
        .unwrap()
        .mcp_server(&chat.chat.id, &id)
        .unwrap();
    assert_eq!(raw.secrets[0].value, "initial-secret");
    assert_eq!(raw.args, ["package", "subcommand"]);

    let redacted = service
        .edit_mcp_server(
            &chat.chat.id,
            &id,
            edit(SecretEdit::Replace, Some("replacement-secret")),
        )
        .await
        .unwrap();
    assert!(redacted[0].secrets[0].present);
    assert!(!serde_json::to_string(&redacted)
        .unwrap()
        .contains("replacement-secret"));
    let raw = Store::open(&tmp.path().join("hub.db"))
        .unwrap()
        .mcp_server(&chat.chat.id, &id)
        .unwrap();
    assert_eq!(raw.secrets[0].value, "replacement-secret");

    let removed = service
        .edit_mcp_server(&chat.chat.id, &id, edit(SecretEdit::Remove, None))
        .await
        .unwrap();
    assert!(removed[0].secrets.is_empty());
    sessions.shutdown_all().await;
}

#[tokio::test]
async fn active_turn_rejects_mcp_connection_config_edits() {
    let tmp = tempfile::tempdir().unwrap();
    let (service, sessions) = hub(tmp.path());
    let project = service
        .create_project("demo".into(), tmp.path().display().to_string())
        .unwrap();
    let chat = service
        .create_chat(&project.id, "codex", None)
        .await
        .unwrap();
    let mut events = sessions.event_log().subscribe();
    service
        .prompt_chat(&chat.chat.id, "wait".into())
        .await
        .unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .unwrap()
            .unwrap();
        if event.session_id == chat.chat.id
            && matches!(event.payload, EventPayload::UserMessage { ref text, .. } if text == "wait")
        {
            break;
        }
    }
    let error = service
        .create_mcp_server(
            &chat.chat.id,
            McpServerInput {
                name: "tools".into(),
                transport: McpTransport::Stdio,
                url: None,
                command: Some("/bin/echo".into()),
                args: vec![],
                secrets: vec![],
            },
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("active turn"));
    assert!(service.mcp_servers(&chat.chat.id).unwrap().is_empty());
    service.cancel_chat(&chat.chat.id).await.unwrap();
    sessions.shutdown_all().await;
}

#[tokio::test]
async fn additional_root_project_deletion_and_stale_path_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let primary_path = tmp.path().join("primary");
    let additional_path = tmp.path().join("additional");
    std::fs::create_dir_all(&primary_path).unwrap();
    std::fs::create_dir_all(&additional_path).unwrap();
    let (service, sessions) = hub(tmp.path());
    let primary = service
        .create_project("primary".into(), primary_path.display().to_string())
        .unwrap();
    let additional = service
        .create_project("additional".into(), additional_path.display().to_string())
        .unwrap();
    let chat = service
        .create_chat(&primary.id, "codex", None)
        .await
        .unwrap();
    service
        .set_additional_roots(&chat.chat.id, vec![additional.id.clone()])
        .await
        .unwrap();

    let conflict = service.delete_project(&additional.id).await.unwrap_err();
    assert!(
        matches!(conflict, ServiceError::Conflict(message) if message.contains("additional workspace roots"))
    );

    std::fs::rename(&additional_path, tmp.path().join("moved-additional")).unwrap();
    let error = service
        .prompt_chat(&chat.chat.id, "hello".into())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no longer exists"));
    sessions.shutdown_all().await;
}

#[tokio::test]
async fn concurrent_config_load_and_prompt_share_one_startup() {
    use batey::state::ProcessState;

    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&root.path().join("hub.db")).unwrap());
    let log = Arc::new(EventLog::persistent(store.clone()).unwrap());
    let history = root.path().join("history");
    std::fs::create_dir_all(&history).unwrap();
    let agent = AgentDefinition::codex_default()
        .with_command("python3".into())
        .with_args(vec![
            format!("{}/tests/fake_acp.py", env!("CARGO_MANIFEST_DIR")),
            history.display().to_string(),
            "slow-startup".into(),
        ]);
    let agents = Arc::new(AgentRegistry::new([agent]));
    let sessions = SessionManager::with_store(agents.clone(), log.clone(), Some(store.clone()));
    let mut config = Config {
        agents: agents.clone(),
        ..Default::default()
    };
    config.web.project_roots = vec![root.path().display().to_string()];
    let hub = HubService::new(store.clone(), sessions.clone(), agents, &config);

    let project = hub
        .create_project("demo".into(), root.path().display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();
    assert_eq!(chat.process_state, "STOPPED");
    let chat_id = chat.chat.id.clone();

    // Ensure the live session entry exists so both callers share one startup lock.
    let live = sessions.get_by_id(&chat_id).await.unwrap();

    // Begin route-time config loading on the stopped chat.
    let hub_for_config = hub.clone();
    let config_chat_id = chat_id.clone();
    let config_handle =
        tokio::spawn(async move { hub_for_config.chat_config(&config_chat_id).await });

    // Wait until startup is in progress so the prompt overlaps it deterministically.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if live.process_state().await == ProcessState::Starting {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "startup never reached STARTING"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Submit an ordinary Send while startup is still in progress. It must wait
    // for and reuse the first startup instead of failing on STARTING.
    hub.prompt_chat(&chat_id, "hello".into())
        .await
        .expect("concurrent prompt must succeed");

    let config_result = config_handle
        .await
        .expect("config task panicked")
        .expect("concurrent config load must succeed");
    assert!(config_result.is_array());

    await_turn(&log, &chat_id).await;

    // Exactly one ACP session/conversation was created.
    let session_files: Vec<_> = std::fs::read_dir(&history)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .collect();
    assert_eq!(
        session_files.len(),
        1,
        "concurrent startup must create exactly one ACP session"
    );

    // The persisted ACP session id is stable across both operations.
    let persisted = store
        .chat(&chat_id)
        .unwrap()
        .acp_session_id
        .expect("startup must persist acp_session_id");
    assert!(!persisted.is_empty());

    // The prompt completed against that same session.
    let high = log.high_watermark().unwrap();
    let events = log.replay_page(1, high, 10_000).unwrap();
    let chunks: Vec<_> = events
        .into_iter()
        .filter(|event| event.session_id == chat_id)
        .filter_map(|event| match event.payload {
            EventPayload::MessageChunk { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert!(
        chunks
            .iter()
            .any(|text| text.starts_with(&format!("{persisted}:"))),
        "prompt must complete against the shared ACP session {persisted}, got {chunks:?}"
    );

    sessions.shutdown_all().await;
}

#[tokio::test]
async fn startup_failure_causes_prompt_to_fail_without_events() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let store = Arc::new(Store::open(&root.join("hub.db")).unwrap());
    let log = Arc::new(EventLog::persistent(store.clone()).unwrap());
    let agent = AgentDefinition::codex_default()
        .with_command("nonexistent-command-fail-startup".into())
        .with_args(vec![]);
    let agents = Arc::new(AgentRegistry::new([agent]));
    let sessions = SessionManager::with_store(agents.clone(), log.clone(), Some(store.clone()));
    let mut config = Config {
        agents: agents.clone(),
        ..Default::default()
    };
    config.web.project_roots = vec![root.display().to_string()];
    let hub = HubService::new(store, sessions.clone(), agents, &config);

    let project = hub
        .create_project("demo".into(), root.display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();

    let result = hub.prompt_chat(&chat.chat.id, "hello".into()).await;
    assert!(
        result.is_err(),
        "startup failure must fail prompt immediately"
    );

    tokio::time::sleep(Duration::from_millis(50)).await;

    let chat_events: Vec<_> = log
        .replay_page(1, log.high_watermark().unwrap_or(0), 10_000)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.session_id == chat.chat.id)
        .collect();

    let turn_events: Vec<_> = chat_events
        .iter()
        .filter(|e| {
            matches!(
                e.payload,
                EventPayload::UserMessage { .. }
                    | EventPayload::Error { .. }
                    | EventPayload::TurnComplete { .. }
            )
        })
        .collect();
    assert!(
        turn_events.is_empty(),
        "startup failure must emit zero UserMessage, Error, or TurnComplete events: {turn_events:?}"
    );

    sessions.shutdown_all().await;
}

#[tokio::test]
async fn authorize_chat_environment_security_and_path_validation() {
    let tmp = tempfile::tempdir().unwrap();
    let (hub, sessions) = hub(tmp.path());

    let project = hub
        .create_project("test_proj".into(), tmp.path().display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();

    // 1. Unknown chat returns ChatNotFound
    let err_unknown = hub
        .authorize_chat_environment("nonexistent-chat-id", false)
        .await
        .unwrap_err();
    assert!(matches!(err_unknown, ServiceError::NotFound(_)));

    // 2. Archived chat returns Invalid
    hub.edit_chat(
        &chat.chat.id,
        ChatEdit {
            title: None,
            archived: Some(true),
        },
    )
    .await
    .unwrap();
    let err_archived = hub
        .authorize_chat_environment(&chat.chat.id, false)
        .await
        .unwrap_err();
    assert!(matches!(err_archived, ServiceError::Invalid(_)));

    // 3. Project without .envrc refuses to authorize ancestor .envrc above workspace boundary
    let root = tmp.path().join("boundary_test_root");
    let project_dir = root.join("subproject");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(root.join(".envrc"), "export ANCESTOR_INTRUSION=1\n").unwrap();

    let project2 = hub
        .create_project("boundary_proj".into(), project_dir.display().to_string())
        .unwrap();
    let chat2 = hub.create_chat(&project2.id, "codex", None).await.unwrap();

    let err_boundary = hub
        .authorize_chat_environment(&chat2.chat.id, false)
        .await
        .unwrap_err();
    match err_boundary {
        ServiceError::Invalid(msg) => {
            assert!(
                msg.contains("No .envrc found within validated workspace boundary"),
                "expected boundary escape message, got: {}",
                msg
            );
        }
        other => panic!(
            "expected ServiceError::Invalid with boundary error, got: {:?}",
            other
        ),
    }

    sessions.shutdown_all().await;
}

#[tokio::test]
async fn task_cleanup_on_chat_deletion() {
    let tmp = tempfile::tempdir().unwrap();
    let (hub, sessions) = hub(tmp.path());

    let project = hub
        .create_project("test_proj".into(), tmp.path().display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();

    let task = Arc::new(batey::tasks::ManagedTask::new(
        "task-test-del".into(),
        chat.chat.id.clone(),
        "echo hi".into(),
        tmp.path().to_path_buf(),
        None,
    ));
    sessions.task_tracker().register_task(task.clone()).await;

    assert_eq!(hub.list_chat_tasks(&chat.chat.id).await.unwrap().len(), 1);
    let details = hub
        .get_chat_task(&chat.chat.id, "task-test-del")
        .await
        .unwrap();
    assert_eq!(details.id, "task-test-del");

    // Delete chat -> task tracker must forget all tasks associated with chat
    hub.delete_chat(&chat.chat.id).await.unwrap();

    assert!(hub.list_chat_tasks(&chat.chat.id).await.is_err());
    assert!(sessions
        .task_tracker()
        .get_task("task-test-del")
        .await
        .is_none());

    sessions.shutdown_all().await;
}

#[tokio::test]
async fn observed_terminal_task_reconciles_when_turn_ends_without_a_final_update() {
    // Reproduces the reported bug: an agent reports a real command as
    // running, then ends its turn without ever sending a closing tool-call
    // update. Batey must not leave the Terminal Task shown as RUNNING once
    // the turn (and thus the command) is known to be over.
    let tmp = tempfile::tempdir().unwrap();
    let (hub, sessions) = hub(tmp.path());
    let log = sessions.event_log().clone();

    let project = hub
        .create_project("demo".into(), tmp.path().display().to_string())
        .unwrap();
    let chat = hub.create_chat(&project.id, "codex", None).await.unwrap();

    hub.prompt_chat(&chat.chat.id, "tool-stuck-running".into())
        .await
        .unwrap();
    await_turn(&log, &chat.chat.id).await;

    let tasks = hub.list_chat_tasks(&chat.chat.id).await.unwrap();
    assert_eq!(
        tasks.len(),
        1,
        "expected the observed command to be tracked"
    );
    let task = &tasks[0];
    assert_eq!(task.command, "cargo build");
    assert!(!task.managed);
    assert_eq!(task.state, batey::tasks::TaskState::Completed);
    assert!(task.exit_code.is_none(), "no exit code was ever reported");

    sessions.shutdown_all().await;
}
