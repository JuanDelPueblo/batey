//! Asynchronous protocol authentication flows.
//!
//! An ordinary `agent` method runs `authenticate(methodId)` on its own
//! short-lived ACP process. That RPC can take a long time: the agent may
//! emit request-scoped `elicitation/create` (for example a URL/device-code
//! step) and wait for explicit user action. Tying the whole browser request
//! to that RPC would leave the card in `Checking sign-in…` with no way to
//! cancel.
//!
//! One flow owns one ACP client and one `authenticate` attempt. The flow id
//! is opaque. Elicitations stay request-scoped: they are answered through
//! the flow's own callback handler and never reach durable chat events,
//! the store, or the tracing log. Only the lifecycle state and the reason
//! are ever reported.
//!
//! This reuses the same bounds, opaque-id, and first-wins finish rules as
//! the terminal flows, but it owns an ACP process instead of a PTY.
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::watch;

/// How long one protocol flow may live, even while the user completes a URL step.
pub const MAX_PROTOCOL_FLOW_LIFETIME: Duration = Duration::from_secs(10 * 60);
/// How often the supervisor rechecks the lifetime bound and elicitation presence.
const PROTOCOL_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// How long a finished flow stays readable before the registry drops it.
const FINISHED_RETENTION: Duration = Duration::from_secs(120);
/// Concurrent unfinished protocol flows across all agents.
pub const MAX_ACTIVE_PROTOCOL_FLOWS: usize = 4;
/// Concurrent unfinished protocol flows for one agent.
pub const MAX_ACTIVE_PROTOCOL_FLOWS_PER_AGENT: usize = 1;

/// The lifecycle of one protocol authentication attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolFlowState {
    Running,
    /// The agent waits for explicit user action on an elicitation.
    WaitingForUser,
    /// `authenticate` answered without error.
    Succeeded,
    /// `authenticate` answered with an error, timed out, or never started.
    Failed,
    /// A client cancelled the flow.
    Cancelled,
    /// The flow reached its overall lifetime.
    TimedOut,
}

impl ProtocolFlowState {
    pub fn is_finished(self) -> bool {
        !matches!(self, Self::Running | Self::WaitingForUser)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::WaitingForUser => "waiting_for_user",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }
}

/// The safe lifecycle summary of one protocol flow. It never carries URLs,
/// codes, tokens, or other sensitive auth material.
#[derive(Debug, Clone, Serialize)]
pub struct ProtocolAuthFlowView {
    pub flow_id: String,
    pub agent_id: String,
    pub method_id: String,
    pub state: ProtocolFlowState,
    /// Why the flow ended, in Batey's own words. Never sensitive material.
    pub reason: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
struct FlowStatus {
    state: ProtocolFlowState,
    reason: Option<String>,
    completed_at: Option<DateTime<Utc>>,
}

/// One protocol authentication flow and the ACP client behind it.
pub struct ProtocolAuthFlow {
    pub id: String,
    pub agent_id: String,
    pub method_id: String,
    pub started_at: DateTime<Utc>,
    status: StdMutex<FlowStatus>,
    status_tx: watch::Sender<()>,
    status_rx: watch::Receiver<()>,
    client: tokio::sync::Mutex<Option<Arc<crate::acp::AcpClient>>>,
}

impl ProtocolAuthFlow {
    pub fn view(&self) -> ProtocolAuthFlowView {
        let status = self
            .status
            .lock()
            .expect("protocol auth status lock poisoned")
            .clone();
        ProtocolAuthFlowView {
            flow_id: self.id.clone(),
            agent_id: self.agent_id.clone(),
            method_id: self.method_id.clone(),
            state: status.state,
            reason: status.reason,
            started_at: self.started_at,
            completed_at: status.completed_at,
        }
    }

    pub fn state(&self) -> ProtocolFlowState {
        self.status
            .lock()
            .expect("protocol auth status lock poisoned")
            .state
    }

    /// Sets the client that serves elicitations for this flow.
    pub async fn set_client(&self, client: Arc<crate::acp::AcpClient>) {
        *self.client.lock().await = Some(client);
    }

    pub async fn client(&self) -> Option<Arc<crate::acp::AcpClient>> {
        self.client.lock().await.clone()
    }

    /// Ends the flow. The first caller wins, so a cancel racing success
    /// never overwrites the recorded outcome.
    pub fn finish(&self, state: ProtocolFlowState, reason: Option<String>) {
        {
            let mut status = self
                .status
                .lock()
                .expect("protocol auth status lock poisoned");
            if status.state.is_finished() {
                return;
            }
            // `WaitingForUser` is still unfinished; any finished state may
            // replace it. A finished state never replaces another finished one.
            status.state = state;
            status.reason = reason;
            status.completed_at = Some(Utc::now());
        }
        let _ = self.status_tx.send(());
        tracing::info!(
            flow = %self.id,
            agent = %self.agent_id,
            method = %self.method_id,
            state = ?state,
            "protocol authentication flow ended"
        );
    }

    pub fn cancel(&self) {
        self.finish(
            ProtocolFlowState::Cancelled,
            Some("Cancelled by the client".into()),
        );
    }

    pub(crate) fn note_waiting(&self, waiting: bool) {
        let mut status = self
            .status
            .lock()
            .expect("protocol auth status lock poisoned");
        if status.state.is_finished() {
            return;
        }
        if waiting && status.state == ProtocolFlowState::Running {
            status.state = ProtocolFlowState::WaitingForUser;
            let _ = self.status_tx.send(());
        } else if !waiting && status.state == ProtocolFlowState::WaitingForUser {
            status.state = ProtocolFlowState::Running;
            let _ = self.status_tx.send(());
        }
    }

    /// Resolves once the flow reaches a finished state.
    pub async fn wait_finished(&self) -> ProtocolFlowState {
        let mut rx = self.status_rx.clone();
        loop {
            let state = self.state();
            if state.is_finished() {
                return state;
            }
            if rx.changed().await.is_err() {
                return self.state();
            }
        }
    }
}

/// Every live protocol authentication flow.
pub struct ProtocolAuthFlows {
    flows: StdMutex<HashMap<String, Arc<ProtocolAuthFlow>>>,
    max_lifetime: Duration,
}

impl Default for ProtocolAuthFlows {
    fn default() -> Self {
        Self::new()
    }
}

impl ProtocolAuthFlows {
    pub fn new() -> Self {
        Self::with_lifetime(MAX_PROTOCOL_FLOW_LIFETIME)
    }

    pub fn with_lifetime(max_lifetime: Duration) -> Self {
        Self {
            flows: StdMutex::new(HashMap::new()),
            max_lifetime,
        }
    }

    pub fn get(&self, flow_id: &str) -> Option<Arc<ProtocolAuthFlow>> {
        self.flows
            .lock()
            .expect("protocol auth registry lock poisoned")
            .get(flow_id)
            .cloned()
    }

    /// One unfinished flow for this agent, when one exists. Used for
    /// recovery after navigation or reload. Only `running` and
    /// `waiting_for_user` count as active.
    pub fn active_for_agent(&self, agent_id: &str) -> Option<Arc<ProtocolAuthFlow>> {
        self.flows
            .lock()
            .expect("protocol auth registry lock poisoned")
            .values()
            .filter(|flow| flow.agent_id == agent_id)
            .filter(|flow| !flow.state().is_finished())
            .max_by_key(|flow| flow.started_at)
            .cloned()
    }

    /// Creates one flow entry in `Running`. The caller spawns the ACP work
    /// and finishes the flow. Fails when bounds are hit.
    pub fn create(&self, agent_id: &str, method_id: &str) -> anyhow::Result<Arc<ProtocolAuthFlow>> {
        self.prune();
        self.check_bounds(agent_id)?;
        let (status_tx, status_rx) = watch::channel(());
        let flow = Arc::new(ProtocolAuthFlow {
            id: new_flow_id(),
            agent_id: agent_id.to_owned(),
            method_id: method_id.to_owned(),
            started_at: Utc::now(),
            status: StdMutex::new(FlowStatus {
                state: ProtocolFlowState::Running,
                reason: None,
                completed_at: None,
            }),
            status_tx,
            status_rx,
            client: tokio::sync::Mutex::new(None),
        });
        self.flows
            .lock()
            .expect("protocol auth registry lock poisoned")
            .insert(flow.id.clone(), flow.clone());
        tracing::info!(
            flow = %flow.id,
            agent = %agent_id,
            method = %method_id,
            "protocol authentication flow started"
        );
        Ok(flow)
    }

    /// Ends every flow. Server shutdown calls this.
    pub fn shutdown_all(&self) {
        let flows: Vec<Arc<ProtocolAuthFlow>> = self
            .flows
            .lock()
            .expect("protocol auth registry lock poisoned")
            .values()
            .cloned()
            .collect();
        for flow in flows {
            flow.finish(
                ProtocolFlowState::Cancelled,
                Some("Batey is shutting down".into()),
            );
        }
        self.flows
            .lock()
            .expect("protocol auth registry lock poisoned")
            .clear();
    }

    fn check_bounds(&self, agent_id: &str) -> anyhow::Result<()> {
        let flows = self
            .flows
            .lock()
            .expect("protocol auth registry lock poisoned");
        let active: Vec<&Arc<ProtocolAuthFlow>> = flows
            .values()
            .filter(|flow| !flow.state().is_finished())
            .collect();
        anyhow::ensure!(
            active.len() < MAX_ACTIVE_PROTOCOL_FLOWS,
            "Too many authentication flows are already running. Finish or cancel one first."
        );
        anyhow::ensure!(
            active.iter().filter(|f| f.agent_id == agent_id).count()
                < MAX_ACTIVE_PROTOCOL_FLOWS_PER_AGENT,
            "Agent '{agent_id}' already has an authentication flow running. Finish or cancel it first."
        );
        Ok(())
    }

    fn prune(&self) {
        let now = Utc::now();
        self.flows
            .lock()
            .expect("protocol auth registry lock poisoned")
            .retain(|_, flow| {
                match flow
                    .status
                    .lock()
                    .expect("protocol auth status lock poisoned")
                    .completed_at
                {
                    Some(completed) => now
                        .signed_duration_since(completed)
                        .to_std()
                        .map(|age| age < FINISHED_RETENTION)
                        .unwrap_or(true),
                    None => true,
                }
            });
    }

    pub fn max_lifetime(&self) -> Duration {
        self.max_lifetime
    }

    pub fn poll_interval() -> Duration {
        PROTOCOL_POLL_INTERVAL
    }
}

/// A flow id a client cannot guess. Same shape as terminal flow ids.
fn new_flow_id() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_running_and_waiting_are_unfinished() {
        assert!(!ProtocolFlowState::Running.is_finished());
        assert!(!ProtocolFlowState::WaitingForUser.is_finished());
        for state in [
            ProtocolFlowState::Succeeded,
            ProtocolFlowState::Failed,
            ProtocolFlowState::Cancelled,
            ProtocolFlowState::TimedOut,
        ] {
            assert!(state.is_finished(), "{state:?} should be finished");
        }
    }

    #[test]
    fn flow_ids_are_long_and_unique() {
        let flows = ProtocolAuthFlows::new();
        let first = flows.create("demo", "oauth").unwrap();
        let second = flows.create("other", "oauth").unwrap();
        assert_eq!(first.id.len(), 64);
        assert!(first.id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first.id, second.id);
        flows.shutdown_all();
    }

    #[test]
    fn waiting_transitions_only_while_unfinished() {
        let flows = ProtocolAuthFlows::new();
        let flow = flows.create("demo", "oauth").unwrap();
        assert_eq!(flow.state(), ProtocolFlowState::Running);
        flow.note_waiting(true);
        assert_eq!(flow.state(), ProtocolFlowState::WaitingForUser);
        flow.note_waiting(false);
        assert_eq!(flow.state(), ProtocolFlowState::Running);
        flow.finish(ProtocolFlowState::Succeeded, None);
        flow.note_waiting(true);
        assert_eq!(flow.state(), ProtocolFlowState::Succeeded);
        flows.shutdown_all();
    }

    #[test]
    fn first_finish_wins() {
        let flows = ProtocolAuthFlows::new();
        let flow = flows.create("demo", "oauth").unwrap();
        flow.finish(ProtocolFlowState::Succeeded, None);
        flow.finish(ProtocolFlowState::Failed, Some("late".into()));
        assert_eq!(flow.state(), ProtocolFlowState::Succeeded);
        assert!(flow.view().reason.is_none());
        flows.shutdown_all();
    }

    #[test]
    fn per_agent_bound_holds() {
        let flows = ProtocolAuthFlows::new();
        let _first = flows.create("demo", "oauth").unwrap();
        assert!(flows.create("demo", "other").is_err());
        // Another agent still fits inside the global bound.
        assert!(flows.create("other", "oauth").is_ok());
        flows.shutdown_all();
    }
}
