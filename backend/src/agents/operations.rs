//! In-memory tracking of ACP Registry install and update operations.
//!
//! Operations report granular, truthful lifecycle progress for long-running
//! downloads, checksum verification, and archive extraction without persisting
//! transient byte counts in SQLite.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub const MAX_TERMINAL_OPERATIONS: usize = 50;
pub const TERMINAL_RETENTION_SECS: i64 = 600; // 10 minutes

/// The kind of operation being executed on an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOperationKind {
    Install,
    Update,
}

/// The high-level lifecycle state of an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOperationState {
    Running,
    Succeeded,
    Failed,
}

/// Granular stage within the operation lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOperationStage {
    Resolving,
    Downloading,
    Verifying,
    Extracting,
    Preparing,
    Finalizing,
    Completed,
    Failed,
}

/// A read-only snapshot of an operation's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentOperationView {
    pub id: String,
    pub agent_id: String,
    pub registry_id: String,
    pub kind: AgentOperationKind,
    pub state: AgentOperationState,
    pub stage: AgentOperationStage,
    pub downloaded_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_version: Option<String>,
}

#[derive(Debug, Clone)]
struct OperationStatus {
    state: AgentOperationState,
    stage: AgentOperationStage,
    downloaded_bytes: u64,
    total_bytes: Option<u64>,
    error: Option<String>,
    completed_at: Option<DateTime<Utc>>,
    updated: Option<bool>,
    to_version: Option<String>,
}

/// One live or recently completed agent operation.
pub struct AgentOperation {
    pub id: String,
    pub agent_id: String,
    pub registry_id: String,
    pub kind: AgentOperationKind,
    pub started_at: DateTime<Utc>,
    status: Mutex<OperationStatus>,
}

impl AgentOperation {
    pub fn new(
        id: String,
        agent_id: String,
        registry_id: String,
        kind: AgentOperationKind,
    ) -> Self {
        Self {
            id,
            agent_id,
            registry_id,
            kind,
            started_at: Utc::now(),
            status: Mutex::new(OperationStatus {
                state: AgentOperationState::Running,
                stage: AgentOperationStage::Resolving,
                downloaded_bytes: 0,
                total_bytes: None,
                error: None,
                completed_at: None,
                updated: None,
                to_version: None,
            }),
        }
    }

    pub fn view(&self) -> AgentOperationView {
        let status = self.status.lock().expect("operation status poisoned");
        AgentOperationView {
            id: self.id.clone(),
            agent_id: self.agent_id.clone(),
            registry_id: self.registry_id.clone(),
            kind: self.kind,
            state: status.state,
            stage: status.stage,
            downloaded_bytes: status.downloaded_bytes,
            total_bytes: status.total_bytes,
            error: status.error.clone(),
            started_at: self.started_at,
            completed_at: status.completed_at,
            updated: status.updated,
            to_version: status.to_version.clone(),
        }
    }

    pub fn update_stage(&self, stage: AgentOperationStage) {
        let mut status = self.status.lock().expect("operation status poisoned");
        if status.state != AgentOperationState::Running {
            return;
        }
        status.stage = stage;
    }

    pub fn update_download(&self, downloaded: u64, total: Option<u64>) {
        let mut status = self.status.lock().expect("operation status poisoned");
        if status.state != AgentOperationState::Running {
            return;
        }
        status.stage = AgentOperationStage::Downloading;
        status.downloaded_bytes = downloaded;
        status.total_bytes = total;
    }

    pub fn succeed(&self, updated: Option<bool>, to_version: Option<String>) {
        let mut status = self.status.lock().expect("operation status poisoned");
        if status.state != AgentOperationState::Running {
            return;
        }
        status.state = AgentOperationState::Succeeded;
        status.stage = AgentOperationStage::Completed;
        status.updated = updated;
        status.to_version = to_version;
        status.completed_at = Some(Utc::now());
    }

    pub fn fail(&self, error: String) {
        let mut status = self.status.lock().expect("operation status poisoned");
        if status.state != AgentOperationState::Running {
            return;
        }
        status.state = AgentOperationState::Failed;
        status.stage = AgentOperationStage::Failed;
        status.error = Some(error);
        status.completed_at = Some(Utc::now());
    }
}

/// Interface for streaming progress reporting during an install or update.
pub trait InstallProgressTracker: Send + Sync {
    fn on_stage(&self, stage: AgentOperationStage);
    fn on_download(&self, downloaded: u64, total: Option<u64>);
}

pub struct OperationProgressTracker {
    operation: Arc<AgentOperation>,
}

impl OperationProgressTracker {
    pub fn new(operation: Arc<AgentOperation>) -> Self {
        Self { operation }
    }
}

impl InstallProgressTracker for AgentOperation {
    fn on_stage(&self, stage: AgentOperationStage) {
        self.update_stage(stage);
    }

    fn on_download(&self, downloaded: u64, total: Option<u64>) {
        self.update_download(downloaded, total);
    }
}

impl InstallProgressTracker for OperationProgressTracker {
    fn on_stage(&self, stage: AgentOperationStage) {
        self.operation.update_stage(stage);
    }

    fn on_download(&self, downloaded: u64, total: Option<u64>) {
        self.operation.update_download(downloaded, total);
    }
}

/// In-memory bounded collection of operations.
#[derive(Default)]
pub struct AgentOperations {
    operations: Mutex<HashMap<String, Arc<AgentOperation>>>,
}

impl AgentOperations {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a new operation. Returns an error if an operation is already running
    /// for the given agent.
    pub fn register(
        &self,
        agent_id: String,
        registry_id: String,
        kind: AgentOperationKind,
    ) -> Result<Arc<AgentOperation>, String> {
        let mut ops = self.operations.lock().expect("operations poisoned");
        self.prune_locked(&mut ops);

        // Check if there is already a running operation for this agent
        for op in ops.values() {
            if op.agent_id == agent_id {
                let status = op.status.lock().expect("status poisoned");
                if status.state == AgentOperationState::Running {
                    return Err(format!(
                        "An operation is already in progress for agent '{agent_id}'"
                    ));
                }
            }
        }

        let id = format!("op-{}", uuid::Uuid::new_v4());
        let operation = Arc::new(AgentOperation::new(id.clone(), agent_id, registry_id, kind));
        ops.insert(id, operation.clone());
        Ok(operation)
    }

    pub fn get(&self, id: &str) -> Option<AgentOperationView> {
        let mut ops = self.operations.lock().expect("operations poisoned");
        self.prune_locked(&mut ops);
        ops.get(id).map(|op| op.view())
    }

    pub fn get_active_for_agent(&self, agent_id: &str) -> Option<AgentOperationView> {
        let mut ops = self.operations.lock().expect("operations poisoned");
        self.prune_locked(&mut ops);
        for op in ops.values() {
            if op.agent_id == agent_id {
                let view = op.view();
                if view.state == AgentOperationState::Running {
                    return Some(view);
                }
            }
        }
        None
    }

    pub fn list(&self) -> Vec<AgentOperationView> {
        let mut ops = self.operations.lock().expect("operations poisoned");
        self.prune_locked(&mut ops);
        let mut views: Vec<_> = ops.values().map(|op| op.view()).collect();
        views.sort_by_key(|a| std::cmp::Reverse(a.started_at));
        views
    }

    fn prune_locked(&self, ops: &mut HashMap<String, Arc<AgentOperation>>) {
        let now = Utc::now();
        // Prune expired terminal operations
        ops.retain(|_, op| {
            let status = op.status.lock().expect("status poisoned");
            match status.completed_at {
                Some(completed) => (now - completed).num_seconds() < TERMINAL_RETENTION_SECS,
                None => true,
            }
        });

        // If terminal operations still exceed max, remove oldest terminal operations
        let mut terminal_keys: Vec<(String, DateTime<Utc>)> = ops
            .iter()
            .filter_map(|(id, op)| {
                let status = op.status.lock().expect("status poisoned");
                status.completed_at.map(|completed| (id.clone(), completed))
            })
            .collect();

        if terminal_keys.len() > MAX_TERMINAL_OPERATIONS {
            terminal_keys.sort_by_key(|(_, completed)| *completed);
            let to_remove = terminal_keys.len() - MAX_TERMINAL_OPERATIONS;
            for (id, _) in terminal_keys.into_iter().take(to_remove) {
                ops.remove(&id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_initial_state_and_stage_transitions() {
        let op = AgentOperation::new(
            "op-1".into(),
            "agent-a".into(),
            "registry-a".into(),
            AgentOperationKind::Install,
        );
        let initial = op.view();
        assert_eq!(initial.id, "op-1");
        assert_eq!(initial.agent_id, "agent-a");
        assert_eq!(initial.registry_id, "registry-a");
        assert_eq!(initial.kind, AgentOperationKind::Install);
        assert_eq!(initial.state, AgentOperationState::Running);
        assert_eq!(initial.stage, AgentOperationStage::Resolving);
        assert_eq!(initial.downloaded_bytes, 0);
        assert_eq!(initial.total_bytes, None);
        assert_eq!(initial.error, None);
        assert!(initial.completed_at.is_none());

        // Update to downloading with known total
        op.update_download(1024, Some(2048));
        let downloading = op.view();
        assert_eq!(downloading.stage, AgentOperationStage::Downloading);
        assert_eq!(downloading.downloaded_bytes, 1024);
        assert_eq!(downloading.total_bytes, Some(2048));

        // Update to verifying
        op.update_stage(AgentOperationStage::Verifying);
        assert_eq!(op.view().stage, AgentOperationStage::Verifying);

        // Update to extracting
        op.update_stage(AgentOperationStage::Extracting);
        assert_eq!(op.view().stage, AgentOperationStage::Extracting);

        // Update to finalizing
        op.update_stage(AgentOperationStage::Finalizing);
        assert_eq!(op.view().stage, AgentOperationStage::Finalizing);

        // Complete successfully
        op.succeed(Some(true), Some("1.0.0".into()));
        let succeeded = op.view();
        assert_eq!(succeeded.state, AgentOperationState::Succeeded);
        assert_eq!(succeeded.stage, AgentOperationStage::Completed);
        assert_eq!(succeeded.updated, Some(true));
        assert_eq!(succeeded.to_version.as_deref(), Some("1.0.0"));
        assert!(succeeded.completed_at.is_some());

        // Further stage update is ignored once terminal
        op.update_stage(AgentOperationStage::Extracting);
        assert_eq!(op.view().state, AgentOperationState::Succeeded);
        assert_eq!(op.view().stage, AgentOperationStage::Completed);
    }

    #[test]
    fn operation_indeterminate_download_and_failure() {
        let op = AgentOperation::new(
            "op-2".into(),
            "agent-b".into(),
            "registry-b".into(),
            AgentOperationKind::Update,
        );

        // Indeterminate download (unknown total)
        op.update_download(4096, None);
        let view = op.view();
        assert_eq!(view.stage, AgentOperationStage::Downloading);
        assert_eq!(view.downloaded_bytes, 4096);
        assert_eq!(view.total_bytes, None);

        // Fail with message
        op.fail("Checksum mismatch".into());
        let failed = op.view();
        assert_eq!(failed.state, AgentOperationState::Failed);
        assert_eq!(failed.stage, AgentOperationStage::Failed);
        assert_eq!(failed.error.as_deref(), Some("Checksum mismatch"));
        assert!(failed.completed_at.is_some());
    }

    #[test]
    fn operations_registry_isolation_between_agents() {
        let operations = AgentOperations::new();
        let op_a = operations
            .register(
                "agent-a".into(),
                "reg-a".into(),
                AgentOperationKind::Install,
            )
            .unwrap();
        let op_b = operations
            .register("agent-b".into(), "reg-b".into(), AgentOperationKind::Update)
            .unwrap();

        assert_ne!(op_a.id, op_b.id);

        op_a.update_download(500, Some(1000));
        op_b.update_stage(AgentOperationStage::Preparing);

        let view_a = operations.get(&op_a.id).unwrap();
        let view_b = operations.get(&op_b.id).unwrap();

        assert_eq!(view_a.agent_id, "agent-a");
        assert_eq!(view_a.stage, AgentOperationStage::Downloading);
        assert_eq!(view_a.downloaded_bytes, 500);
        assert_eq!(view_a.total_bytes, Some(1000));

        assert_eq!(view_b.agent_id, "agent-b");
        assert_eq!(view_b.stage, AgentOperationStage::Preparing);
        assert_eq!(view_b.downloaded_bytes, 0);

        // Cannot start another operation for agent-a while op_a is running
        let collision = operations.register(
            "agent-a".into(),
            "reg-a".into(),
            AgentOperationKind::Install,
        );
        assert!(collision.is_err());

        // Completing op_a allows another operation for agent-a
        op_a.succeed(None, None);
        let second_a =
            operations.register("agent-a".into(), "reg-a".into(), AgentOperationKind::Update);
        assert!(second_a.is_ok());
    }

    #[test]
    fn bounded_retention_cleans_old_terminal_operations() {
        let operations = AgentOperations::new();
        // Register and complete 60 operations
        for i in 0..60 {
            let op = operations
                .register(
                    format!("agent-{i}"),
                    format!("reg-{i}"),
                    AgentOperationKind::Install,
                )
                .unwrap();
            op.succeed(None, None);
        }

        let list = operations.list();
        assert!(
            list.len() <= MAX_TERMINAL_OPERATIONS,
            "Operations list length {} exceeds MAX_TERMINAL_OPERATIONS {}",
            list.len(),
            MAX_TERMINAL_OPERATIONS
        );
    }
}
