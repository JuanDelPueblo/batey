use ::agent_client_protocol_schema::v1::ContentBlock;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    pub seq: u64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub session_id: String,
    pub agent: String,
    pub payload: EventPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventPayload {
    UserMessage {
        text: String,
        /// Ordered stable ACP blocks. `text` remains the backwards-compatible
        /// shorthand consumed by older clients and history rows.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content: Vec<ContentBlock>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },
    Error {
        message: String,
    },
    ConfigOptions {
        #[serde(default)]
        options: serde_json::Value,
    },
    MetadataChanged {},
    MessageChunk {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content: Vec<ContentBlock>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },
    ThoughtChunk {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content: Vec<ContentBlock>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },
    ToolCall {
        id: String,
        title: String,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locations: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<serde_json::Value>,
    },
    ToolCallUpdate {
        id: String,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        output: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locations: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<serde_json::Value>,
    },
    AvailableCommands {
        commands: serde_json::Value,
    },
    SessionModes {
        state: serde_json::Value,
    },
    UsageUpdate {
        used: u64,
        size: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_amount: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_currency: Option<String>,
    },
    SessionInfo {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updated_at: Option<String>,
        /// Opaque agent `_meta` preserved generically without interpretation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<serde_json::Value>,
    },
    ElicitationRequest {
        id: String,
        mode: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        elicitation_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
    },
    ElicitationResponse {
        id: String,
        action: String,
    },
    ElicitationComplete {
        elicitation_id: String,
    },
    Plan {
        entries: Vec<PlanEntry>,
    },
    PermissionRequest {
        id: String,
        method: String,
        description: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        /// The exact ACP permission options advertised by the agent.
        options: serde_json::Value,
    },
    PermissionResponse {
        id: String,
        /// The exact agent-supplied option selected by the user. `None` is a
        /// cancellation marker, never an implicit denial.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        option_id: Option<String>,
    },
    TurnComplete {
        stop_reason: String,
    },
    StateChange {
        process: String,
        turn: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanEntry {
    pub content: String,
    pub status: String,
}

pub enum ReplayResult {
    Complete(Vec<SessionEvent>),
    Partial {
        events: Vec<SessionEvent>,
        oldest_available_seq: u64,
    },
}

pub struct EventLog {
    store: Option<std::sync::Arc<crate::store::Store>>,
    events: RwLock<VecDeque<SessionEvent>>,
    min_seq: AtomicU64,
    next_seq: AtomicU64,
    max_entries: usize,
    broadcast_tx: broadcast::Sender<SessionEvent>,
    persistence_error: std::sync::Mutex<Option<String>>,
}

impl EventLog {
    pub fn new(max_entries: usize) -> Self {
        let (broadcast_tx, _) = broadcast::channel(1024);
        Self {
            store: None,
            events: RwLock::new(VecDeque::new()),
            min_seq: AtomicU64::new(0),
            next_seq: AtomicU64::new(1),
            max_entries,
            broadcast_tx,
            persistence_error: std::sync::Mutex::new(None),
        }
    }

    pub fn next_seq(&self) -> u64 {
        self.next_seq.load(Ordering::SeqCst)
    }

    pub fn persistent(store: std::sync::Arc<crate::store::Store>) -> anyhow::Result<Self> {
        Self::persistent_with_limit(store, 10_000)
    }

    fn persistent_with_limit(
        store: std::sync::Arc<crate::store::Store>,
        max_entries: usize,
    ) -> anyhow::Result<Self> {
        let mut log = Self::new(max_entries);
        // The replay cache holds only the recent window. Sequence allocation
        // must follow the durable high watermark instead.
        let next_seq = store.max_event_seq()?.saturating_add(1).max(1);
        log.next_seq.store(next_seq, Ordering::SeqCst);
        // `recent` seeds the in-memory window only; recovery below reads the
        // complete durable state so old rows outside this window still repair.
        let mut recent = store.events()?;
        if recent.len() > max_entries {
            recent.drain(0..recent.len() - max_entries);
        }
        // The recovery scan must see every durable permission/turn row even
        // when those rows have left the replay window, so query it separately.
        let recovery = store.recovery_events()?;
        log.min_seq
            .store(recent.first().map_or(0, |e| e.seq), Ordering::SeqCst);
        *log.events.write().unwrap() = recent.into();
        log.store = Some(store);
        // Browser approvals from a previous process can no longer authorize work.
        // Pending elicitations are cancelled the same way; their form values
        // never persist, only the cancel marker.
        let mut pending = std::collections::BTreeMap::new();
        let mut pending_elicitations = std::collections::BTreeMap::new();
        let mut active = std::collections::BTreeMap::new();
        for e in recovery {
            match &e.payload {
                EventPayload::PermissionRequest { id, .. } => {
                    pending.insert(id.clone(), (e.session_id.clone(), e.agent.clone()));
                }
                EventPayload::PermissionResponse { id, .. } => {
                    pending.remove(id);
                }
                EventPayload::ElicitationRequest { id, .. } => {
                    pending_elicitations
                        .insert(id.clone(), (e.session_id.clone(), e.agent.clone()));
                }
                EventPayload::ElicitationResponse { id, .. } => {
                    pending_elicitations.remove(id);
                }
                EventPayload::ElicitationComplete { elicitation_id } => {
                    pending_elicitations.remove(elicitation_id);
                }
                EventPayload::StateChange { turn, .. } if turn == "PROMPTING" => {
                    active.insert(e.session_id.clone(), e.agent.clone());
                }
                EventPayload::TurnComplete { .. } => {
                    active.remove(&e.session_id);
                }
                _ => {}
            }
        }
        for (id, (chat, agent)) in pending {
            log.append(
                &chat,
                &agent,
                EventPayload::PermissionResponse {
                    id,
                    option_id: None,
                },
            )?;
        }
        for (id, (chat, agent)) in pending_elicitations {
            log.append(
                &chat,
                &agent,
                EventPayload::ElicitationResponse {
                    id,
                    action: "cancel".to_string(),
                },
            )?;
        }
        for (chat, agent) in active {
            log.append(
                &chat,
                &agent,
                EventPayload::TurnComplete {
                    stop_reason: "backend_restarted".into(),
                },
            )?;
        }
        Ok(log)
    }

    pub fn append(
        &self,
        session_id: &str,
        agent: &str,
        payload: EventPayload,
    ) -> anyhow::Result<u64> {
        self.append_at(session_id, agent, payload, chrono::Utc::now())
    }

    pub fn append_at(
        &self,
        session_id: &str,
        agent: &str,
        payload: EventPayload,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<u64> {
        // Sequence allocation, persistence and publication share ordering.
        let mut events = self.events.write().unwrap();
        if let Some(error) = self.persistence_error.lock().unwrap().as_ref() {
            anyhow::bail!("Event persistence is unavailable: {error}");
        }
        let seq = self.next_seq.load(Ordering::SeqCst);
        let event = SessionEvent {
            seq,
            timestamp,
            session_id: session_id.to_string(),
            agent: agent.to_string(),
            payload,
        };

        // Keep binary ACP output from turning the durable event table into an
        // unbounded attachment store. This happens before SQLite or broadcast.
        let encoded = serde_json::to_vec(&event)?;
        if encoded.len() > crate::content::MAX_DURABLE_RICH_EVENT_BYTES {
            anyhow::bail!(
                "Durable event payload exceeds {} bytes",
                crate::content::MAX_DURABLE_RICH_EVENT_BYTES
            );
        }

        if let Some(store) = &self.store {
            if let Err(error) = store.save_event(&event) {
                tracing::error!(%error, "Failed to persist activity event");
                *self.persistence_error.lock().unwrap() = Some(error.to_string());
                return Err(anyhow::anyhow!("Failed to persist activity event: {error}"));
            }
        }
        self.next_seq.store(seq + 1, Ordering::SeqCst);
        events.push_back(event.clone());

        while events.len() > self.max_entries {
            events.pop_front();
        }
        if let Some(first) = events.front() {
            self.min_seq.store(first.seq, Ordering::SeqCst);
        }
        let _ = self.broadcast_tx.send(event);

        Ok(seq)
    }

    pub fn high_watermark(&self) -> anyhow::Result<u64> {
        if let Some(error) = self.persistence_error.lock().unwrap().as_ref() {
            anyhow::bail!("Event persistence is unavailable: {error}");
        }
        if let Some(store) = &self.store {
            return Ok(store.max_event_seq()?);
        }
        Ok(self.next_seq().saturating_sub(1))
    }

    pub fn replay_page(
        &self,
        from_seq: u64,
        through_seq: u64,
        limit: usize,
    ) -> anyhow::Result<Vec<SessionEvent>> {
        if let Some(error) = self.persistence_error.lock().unwrap().as_ref() {
            anyhow::bail!("Event persistence is unavailable: {error}");
        }
        if let Some(store) = &self.store {
            return Ok(store.event_page(from_seq, through_seq, limit)?);
        }
        Ok(self
            .events
            .read()
            .unwrap()
            .iter()
            .filter(|event| event.seq >= from_seq && event.seq <= through_seq)
            .take(limit)
            .cloned()
            .collect())
    }

    pub fn replay_from(&self, from_seq: u64) -> ReplayResult {
        if self.store.is_some() {
            let through = match self.high_watermark() {
                Ok(value) => value,
                Err(_) => return ReplayResult::Complete(Vec::new()),
            };
            let mut cursor = from_seq;
            let mut result = Vec::new();
            while cursor <= through {
                let page = match self.replay_page(cursor, through, 512) {
                    Ok(page) => page,
                    Err(_) => return ReplayResult::Complete(Vec::new()),
                };
                let Some(last) = page.last() else { break };
                cursor = last.seq.saturating_add(1);
                result.extend(page);
            }
            return ReplayResult::Complete(result);
        }
        let min = self.min_seq.load(Ordering::SeqCst);
        let events = self.events.read().unwrap();

        if from_seq > 0 && from_seq < min && min > 0 {
            let matching: Vec<SessionEvent> = events.iter().cloned().collect();
            return ReplayResult::Partial {
                events: matching,
                oldest_available_seq: min,
            };
        }

        let matching: Vec<SessionEvent> = events
            .iter()
            .filter(|e| e.seq >= from_seq)
            .cloned()
            .collect();
        ReplayResult::Complete(matching)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.broadcast_tx.subscribe()
    }

    pub fn forget_chat(&self, id: &str) {
        self.events.write().unwrap().retain(|e| e.session_id != id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_event_log_append_and_replay() {
        let log = EventLog::new(100);
        let seq1 = log.append(
            "s1",
            "codex",
            EventPayload::MessageChunk {
                message_id: None,
                text: "hello".to_string(),
                content: vec![],
            },
        );
        let seq2 = log.append(
            "s1",
            "codex",
            EventPayload::MessageChunk {
                message_id: None,
                text: "world".to_string(),
                content: vec![],
            },
        );

        assert_eq!(seq1.unwrap(), 1);
        assert_eq!(seq2.unwrap(), 2);

        match log.replay_from(1) {
            ReplayResult::Complete(events) => assert_eq!(events.len(), 2),
            _ => panic!("Expected Complete"),
        }

        match log.replay_from(2) {
            ReplayResult::Complete(events) => assert_eq!(events.len(), 1),
            _ => panic!("Expected Complete"),
        }
    }

    #[tokio::test]
    async fn test_next_seq() {
        let log = EventLog::new(100);
        assert_eq!(log.next_seq(), 1);
        log.append(
            "s1",
            "codex",
            EventPayload::MessageChunk {
                message_id: None,
                text: "a".into(),
                content: vec![],
            },
        )
        .unwrap();
        assert_eq!(log.next_seq(), 2);
    }

    #[tokio::test]
    async fn test_subscribe_receives_events() {
        let log = EventLog::new(100);
        let mut rx = log.subscribe();
        log.append(
            "s1",
            "codex",
            EventPayload::MessageChunk {
                message_id: None,
                text: "hi".into(),
                content: vec![],
            },
        )
        .unwrap();
        let event = rx.recv().await.unwrap();
        assert_eq!(event.session_id, "s1");
        assert_eq!(event.agent, "codex");
    }

    #[tokio::test]
    async fn test_replay_from_future_seq() {
        let log = EventLog::new(100);
        log.append(
            "s1",
            "codex",
            EventPayload::MessageChunk {
                message_id: None,
                text: "a".into(),
                content: vec![],
            },
        )
        .unwrap();
        match log.replay_from(999) {
            ReplayResult::Complete(events) => assert!(events.is_empty()),
            _ => panic!("Expected empty Complete"),
        }
    }

    #[tokio::test]
    async fn test_event_log_eviction() {
        let log = EventLog::new(3);
        for i in 0..5 {
            log.append(
                "s1",
                "codex",
                EventPayload::MessageChunk {
                    message_id: None,
                    text: format!("msg{}", i),
                    content: vec![],
                },
            )
            .unwrap();
        }

        match log.replay_from(1) {
            ReplayResult::Partial {
                events,
                oldest_available_seq,
            } => {
                assert_eq!(events.len(), 3);
                assert!(oldest_available_seq > 1);
            }
            _ => panic!("Expected Partial"),
        }
    }

    #[tokio::test]
    async fn test_replay_sees_event_before_broadcast_consumer_observes_it() {
        let log = Arc::new(EventLog::new(100));
        let mut rx = log.subscribe();

        let append_task = {
            let log = log.clone();
            tokio::spawn(async move {
                log.append(
                    "s1",
                    "codex",
                    EventPayload::MessageChunk {
                        message_id: None,
                        text: "ordered".to_string(),
                        content: vec![],
                    },
                )
            })
        };

        let event = rx.recv().await.unwrap();
        match log.replay_from(event.seq) {
            ReplayResult::Complete(events) => {
                assert!(events.iter().any(|replayed| replayed.seq == event.seq));
            }
            ReplayResult::Partial { .. } => panic!("Expected Complete"),
        }

        append_task.await.unwrap().unwrap();
    }

    #[test]
    fn persistent_append_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::store::Store::open(&temp.path().join("events.db")).unwrap());
        let log = EventLog::persistent(store.clone()).unwrap();
        assert_eq!(log.next_seq(), 1);
        store.set_query_only();

        let mut receiver = log.subscribe();
        assert!(log
            .append(
                "s1",
                "codex",
                EventPayload::MessageChunk {
                    message_id: None,
                    text: "lost".into(),
                    content: vec![],
                }
            )
            .is_err());
        assert_eq!(log.next_seq(), 1);
        assert!(receiver.try_recv().is_err());
        assert!(log
            .append(
                "s1",
                "codex",
                EventPayload::MessageChunk {
                    message_id: None,
                    text: "later".into(),
                    content: vec![],
                }
            )
            .is_err());
    }

    #[test]
    fn persistent_replay_is_not_limited_by_memory_window() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::store::Store::open(&temp.path().join("events.db")).unwrap());
        let log = EventLog::persistent_with_limit(store, 100).unwrap();
        for index in 0..105 {
            log.append(
                "s1",
                "codex",
                EventPayload::MessageChunk {
                    message_id: None,
                    text: index.to_string(),
                    content: vec![],
                },
            )
            .unwrap();
        }
        let ReplayResult::Complete(events) = log.replay_from(1) else {
            panic!("persistent replay was partial");
        };
        assert_eq!(events.len(), 105);
        assert_eq!(events.first().unwrap().seq, 1);
        assert_eq!(events.last().unwrap().seq, 105);
    }

    #[test]
    fn restart_repairs_permission_and_turn_outside_memory_window() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::store::Store::open(&temp.path().join("events.db")).unwrap());
        {
            let before = EventLog::persistent_with_limit(store.clone(), 100).unwrap();
            before
                .append(
                    "chat-a",
                    "codex",
                    EventPayload::StateChange {
                        process: "RUNNING".into(),
                        turn: "PROMPTING".into(),
                    },
                )
                .unwrap();
            before
                .append(
                    "chat-a",
                    "codex",
                    EventPayload::PermissionRequest {
                        id: "p1".into(),
                        method: "edit".into(),
                        description: "edit a file".into(),
                        options: serde_json::json!([]),
                        title: None,
                        kind: None,
                    },
                )
                .unwrap();
            for index in 0..5 {
                before
                    .append(
                        "chat-other",
                        "codex",
                        EventPayload::MessageChunk {
                            message_id: None,
                            text: format!("filler-{index}"),
                            content: vec![],
                        },
                    )
                    .unwrap();
            }
            assert_eq!(store.max_event_seq().unwrap(), 7);
            // Only the two recovery-relevant rows match the startup scan.
            assert_eq!(store.recovery_events().unwrap().len(), 2);
        }

        let after = EventLog::persistent_with_limit(store.clone(), 2).unwrap();
        // The interrupted rows sat at seq 1-2, far outside the tiny window.
        assert!(!after
            .events
            .read()
            .unwrap()
            .iter()
            .any(|e| e.seq == 1 || e.seq == 2));
        assert!(after.events.read().unwrap().len() <= 2);
        assert_eq!(after.next_seq(), 10);

        let ReplayResult::Complete(durable) = after.replay_from(1) else {
            panic!("persistent replay was partial");
        };
        assert_eq!(durable.len(), 9);
        let denied = &durable[7];
        assert_eq!(denied.seq, 8);
        assert_eq!(denied.session_id, "chat-a");
        assert!(matches!(
            &denied.payload,
            EventPayload::PermissionResponse { id, option_id: None } if id == "p1"
        ));
        let completed = &durable[8];
        assert_eq!(completed.seq, 9);
        assert_eq!(completed.session_id, "chat-a");
        assert!(matches!(
            &completed.payload,
            EventPayload::TurnComplete { stop_reason } if stop_reason == "backend_restarted"
        ));

        // Full durable replay must no longer show the turn as in progress.
        let mut pending = std::collections::HashSet::new();
        let mut active = std::collections::HashSet::new();
        for event in &durable {
            match &event.payload {
                EventPayload::PermissionRequest { id, .. } => {
                    pending.insert(id.clone());
                }
                EventPayload::PermissionResponse { id, .. } => {
                    pending.remove(id);
                }
                EventPayload::StateChange { turn, .. } if turn == "PROMPTING" => {
                    active.insert(event.session_id.clone());
                }
                EventPayload::TurnComplete { .. } => {
                    active.remove(&event.session_id);
                }
                _ => {}
            }
        }
        assert!(pending.is_empty());
        assert!(!active.contains("chat-a"));
    }

    #[test]
    fn restart_does_not_duplicate_completed_permission_and_turn() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::store::Store::open(&temp.path().join("events.db")).unwrap());
        {
            let before = EventLog::persistent_with_limit(store.clone(), 100).unwrap();
            before
                .append(
                    "chat-a",
                    "codex",
                    EventPayload::StateChange {
                        process: "RUNNING".into(),
                        turn: "PROMPTING".into(),
                    },
                )
                .unwrap();
            before
                .append(
                    "chat-a",
                    "codex",
                    EventPayload::PermissionRequest {
                        id: "p1".into(),
                        method: "edit".into(),
                        description: "edit a file".into(),
                        options: serde_json::json!([]),
                        title: None,
                        kind: None,
                    },
                )
                .unwrap();
            before
                .append(
                    "chat-a",
                    "codex",
                    EventPayload::PermissionResponse {
                        id: "p1".into(),
                        option_id: Some("allow-once".into()),
                    },
                )
                .unwrap();
            before
                .append(
                    "chat-a",
                    "codex",
                    EventPayload::TurnComplete {
                        stop_reason: "done".into(),
                    },
                )
                .unwrap();
            for index in 0..5 {
                before
                    .append(
                        "chat-other",
                        "codex",
                        EventPayload::MessageChunk {
                            message_id: None,
                            text: format!("filler-{index}"),
                            content: vec![],
                        },
                    )
                    .unwrap();
            }
            assert_eq!(store.max_event_seq().unwrap(), 9);
        }

        let after = EventLog::persistent_with_limit(store.clone(), 2).unwrap();
        assert_eq!(after.next_seq(), 10);
        let ReplayResult::Complete(durable) = after.replay_from(1) else {
            panic!("persistent replay was partial");
        };
        assert_eq!(durable.len(), 9);
        assert!(!durable.iter().any(|e| matches!(
            &e.payload,
            EventPayload::PermissionResponse { id, option_id: None } if id == "p1"
        )));
        assert!(!durable.iter().any(|e| matches!(
            &e.payload,
            EventPayload::TurnComplete { stop_reason } if stop_reason == "backend_restarted"
        )));
    }
}
