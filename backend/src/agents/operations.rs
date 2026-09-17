//! Provider-neutral agent-operation progress model and tracking.
//!
//! Long-running registry operations (install and update) progress through
//! observable lifecycle stages. The operation state stays in memory with
//! bounded retention so terminal operations do not accumulate indefinitely.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::definition::AgentSummary;
use super::manager::{AgentError, AgentResult, UpdateOutcome};

/// How long a completed or failed operation stays readable before eviction.
pub const OPERATION_RETENTION: Duration = Duration::from_secs(5 * 60);
/// Maximum terminal operations kept in memory.
pub const MAX_RETAINED_OPERATIONS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOperationKind {
    Install,
    Update,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOperationState {
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOperationStage {
    Queued,
    Resolving,
    Downloading,
    Verifying,
    Extracting,
    Preparing,
    Finalizing,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentOperation {
    pub id: String,
    pub agent_id: String,
    pub registry_id: String,
    pub kind: AgentOperationKind,
    pub state: AgentOperationState,
    pub stage: AgentOperationStage,
    pub bytes_downloaded: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_outcome: Option<UpdateOutcome>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug)]
struct OperationRecord {
    operation: AgentOperation,
    completed_at: Option<Instant>,
}

#[derive(Debug)]
struct OperationsInner {
    operations: HashMap<String, OperationRecord>,
    active_by_agent: HashMap<String, String>,
    active_by_registry: HashMap<String, String>,
    retention_order: VecDeque<String>,
}

impl OperationsInner {
    fn prune(&mut self, now: Instant) {
        // Prune expired terminal operations.
        while let Some(front_id) = self.retention_order.front() {
            if let Some(record) = self.operations.get(front_id) {
                if let Some(completed_at) = record.completed_at {
                    if now.duration_since(completed_at) > OPERATION_RETENTION {
                        let id = self.retention_order.pop_front().unwrap();
                        self.operations.remove(&id);
                        continue;
                    }
                }
            } else {
                self.retention_order.pop_front();
                continue;
            }
            break;
        }

        // Bound maximum retained terminal operations.
        while self.retention_order.len() > MAX_RETAINED_OPERATIONS {
            if let Some(id) = self.retention_order.pop_front() {
                self.operations.remove(&id);
            }
        }
    }
}

/// In-memory tracker for asynchronous agent operations.
#[derive(Debug, Clone)]
pub struct AgentOperations {
    inner: Arc<Mutex<OperationsInner>>,
}

impl Default for AgentOperations {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(OperationsInner {
                operations: HashMap::new(),
                active_by_agent: HashMap::new(),
                active_by_registry: HashMap::new(),
                retention_order: VecDeque::new(),
            })),
        }
    }
}

impl AgentOperations {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn start(
        &self,
        kind: AgentOperationKind,
        agent_id: String,
        registry_id: String,
    ) -> AgentResult<AgentOperation> {
        self.start_operation(kind, agent_id, registry_id)
    }

    pub fn get(&self, id: &str) -> Option<AgentOperation> {
        self.get_operation(id)
    }

    pub fn list(&self) -> Vec<AgentOperation> {
        self.list_operations()
    }

    pub fn update_progress(&self, id: &str, downloaded: u64, total: Option<u64>) {
        self.set_download_progress(id, downloaded, total)
    }

    pub fn succeed(&self, id: &str) {
        self.set_success(id, None, None)
    }

    pub fn succeed_with_summary(&self, id: &str, agent: AgentSummary) {
        self.set_success(id, Some(agent), None)
    }

    pub fn succeed_with_outcome(&self, id: &str, outcome: UpdateOutcome) {
        self.set_success(id, None, Some(outcome))
    }

    pub fn fail(&self, id: &str, error: &str) {
        self.set_failed(id, error.to_string())
    }

    /// Starts a new tracked operation for the given agent and registry id.
    ///
    /// Fails with `Conflict` if an operation is already running for either id.
    pub fn start_operation(
        &self,
        kind: AgentOperationKind,
        agent_id: String,
        registry_id: String,
    ) -> AgentResult<AgentOperation> {
        let mut inner = self.inner.lock().unwrap();
        inner.prune(Instant::now());

        if let Some(active_id) = inner.active_by_agent.get(&agent_id) {
            return Err(AgentError::Conflict(format!(
                "An operation is already in progress for agent '{agent_id}' ({active_id})"
            )));
        }
        if let Some(active_id) = inner.active_by_registry.get(&registry_id) {
            return Err(AgentError::Conflict(format!(
                "An operation is already in progress for registry entry '{registry_id}' ({active_id})"
            )));
        }

        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        let op = AgentOperation {
            id: id.clone(),
            agent_id: agent_id.clone(),
            registry_id: registry_id.clone(),
            kind,
            state: AgentOperationState::Running,
            stage: AgentOperationStage::Resolving,
            bytes_downloaded: 0,
            total_bytes: None,
            error: None,
            agent: None,
            update_outcome: None,
            created_at: now,
            updated_at: now,
        };

        inner.active_by_agent.insert(agent_id, id.clone());
        inner.active_by_registry.insert(registry_id, id.clone());
        inner.operations.insert(
            id,
            OperationRecord {
                operation: op.clone(),
                completed_at: None,
            },
        );

        Ok(op)
    }

    pub fn get_operation(&self, id: &str) -> Option<AgentOperation> {
        let mut inner = self.inner.lock().unwrap();
        inner.prune(Instant::now());
        inner.operations.get(id).map(|r| r.operation.clone())
    }

    pub fn active_operation_for_agent(&self, agent_id: &str) -> Option<AgentOperation> {
        let mut inner = self.inner.lock().unwrap();
        inner.prune(Instant::now());
        let op_id = inner
            .active_by_agent
            .get(agent_id)
            .or_else(|| inner.active_by_registry.get(agent_id))?;
        inner.operations.get(op_id).map(|r| r.operation.clone())
    }

    pub fn list_operations(&self) -> Vec<AgentOperation> {
        let mut inner = self.inner.lock().unwrap();
        inner.prune(Instant::now());
        let mut list: Vec<AgentOperation> = inner
            .operations
            .values()
            .map(|r| r.operation.clone())
            .collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        list
    }

    pub fn set_stage(&self, id: &str, stage: AgentOperationStage) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(record) = inner.operations.get_mut(id) {
            record.operation.stage = stage;
            record.operation.updated_at = Utc::now();
        }
    }

    pub fn set_download_progress(&self, id: &str, downloaded: u64, total: Option<u64>) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(record) = inner.operations.get_mut(id) {
            record.operation.stage = AgentOperationStage::Downloading;
            record.operation.bytes_downloaded = downloaded;
            if total.is_some() {
                record.operation.total_bytes = total;
            }
            record.operation.updated_at = Utc::now();
        }
    }

    pub fn set_success(
        &self,
        id: &str,
        agent: Option<AgentSummary>,
        outcome: Option<UpdateOutcome>,
    ) {
        let mut inner = self.inner.lock().unwrap();
        let now = Utc::now();
        let instant = Instant::now();
        if let Some(record) = inner.operations.get_mut(id) {
            record.operation.state = AgentOperationState::Succeeded;
            record.operation.stage = AgentOperationStage::Completed;
            record.operation.agent = agent;
            record.operation.update_outcome = outcome;
            record.operation.updated_at = now;
            record.completed_at = Some(instant);

            let agent_id = record.operation.agent_id.clone();
            let registry_id = record.operation.registry_id.clone();
            if inner.active_by_agent.get(&agent_id) == Some(&id.to_string()) {
                inner.active_by_agent.remove(&agent_id);
            }
            if inner.active_by_registry.get(&registry_id) == Some(&id.to_string()) {
                inner.active_by_registry.remove(&registry_id);
            }
            inner.retention_order.push_back(id.to_string());
        }
        inner.prune(instant);
    }

    pub fn set_failed(&self, id: &str, error: String) {
        let mut inner = self.inner.lock().unwrap();
        let now = Utc::now();
        let instant = Instant::now();
        if let Some(record) = inner.operations.get_mut(id) {
            record.operation.state = AgentOperationState::Failed;
            record.operation.stage = AgentOperationStage::Failed;
            record.operation.error = Some(error);
            record.operation.updated_at = now;
            record.completed_at = Some(instant);

            let agent_id = record.operation.agent_id.clone();
            let registry_id = record.operation.registry_id.clone();
            if inner.active_by_agent.get(&agent_id) == Some(&id.to_string()) {
                inner.active_by_agent.remove(&agent_id);
            }
            if inner.active_by_registry.get(&registry_id) == Some(&id.to_string()) {
                inner.active_by_registry.remove(&registry_id);
            }
            inner.retention_order.push_back(id.to_string());
        }
        inner.prune(instant);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_starts_in_resolving_running_state() {
        let ops = AgentOperations::new();
        let op = ops
            .start_operation(
                AgentOperationKind::Install,
                "my-agent".into(),
                "reg-agent".into(),
            )
            .unwrap();
        assert_eq!(op.state, AgentOperationState::Running);
        assert_eq!(op.stage, AgentOperationStage::Resolving);
        assert_eq!(op.bytes_downloaded, 0);
        assert_eq!(op.total_bytes, None);
        assert_eq!(op.error, None);

        let fetched = ops.get_operation(&op.id).unwrap();
        assert_eq!(fetched.id, op.id);
        assert_eq!(fetched.agent_id, "my-agent");
        assert_eq!(fetched.registry_id, "reg-agent");
    }

    #[test]
    fn prevents_concurrent_operations_on_same_agent_or_registry_id() {
        let ops = AgentOperations::new();
        let _op = ops
            .start_operation(
                AgentOperationKind::Install,
                "agent-1".into(),
                "reg-1".into(),
            )
            .unwrap();

        let conflict1 = ops.start_operation(
            AgentOperationKind::Install,
            "agent-1".into(),
            "reg-other".into(),
        );
        assert!(matches!(conflict1, Err(AgentError::Conflict(_))));

        let conflict2 = ops.start_operation(
            AgentOperationKind::Install,
            "agent-other".into(),
            "reg-1".into(),
        );
        assert!(matches!(conflict2, Err(AgentError::Conflict(_))));

        // Unrelated agent can start simultaneously.
        let op2 = ops.start_operation(
            AgentOperationKind::Install,
            "agent-2".into(),
            "reg-2".into(),
        );
        assert!(op2.is_ok());
    }

    #[test]
    fn completion_frees_active_slots_and_transitions_state() {
        let ops = AgentOperations::new();
        let op = ops
            .start_operation(
                AgentOperationKind::Install,
                "agent-1".into(),
                "reg-1".into(),
            )
            .unwrap();

        ops.set_download_progress(&op.id, 1024, Some(2048));
        let progress = ops.get_operation(&op.id).unwrap();
        assert_eq!(progress.stage, AgentOperationStage::Downloading);
        assert_eq!(progress.bytes_downloaded, 1024);
        assert_eq!(progress.total_bytes, Some(2048));

        ops.set_stage(&op.id, AgentOperationStage::Extracting);
        assert_eq!(
            ops.get_operation(&op.id).unwrap().stage,
            AgentOperationStage::Extracting
        );

        ops.set_success(&op.id, None, None);
        let completed = ops.get_operation(&op.id).unwrap();
        assert_eq!(completed.state, AgentOperationState::Succeeded);
        assert_eq!(completed.stage, AgentOperationStage::Completed);

        // Can start another operation for the same agent once completed.
        let op_again =
            ops.start_operation(AgentOperationKind::Update, "agent-1".into(), "reg-1".into());
        assert!(op_again.is_ok());
    }

    #[test]
    fn failure_records_error_and_frees_active_slot() {
        let ops = AgentOperations::new();
        let op = ops
            .start_operation(
                AgentOperationKind::Install,
                "agent-1".into(),
                "reg-1".into(),
            )
            .unwrap();

        ops.set_failed(&op.id, "Checksum mismatch".into());
        let failed = ops.get_operation(&op.id).unwrap();
        assert_eq!(failed.state, AgentOperationState::Failed);
        assert_eq!(failed.stage, AgentOperationStage::Failed);
        assert_eq!(failed.error.as_deref(), Some("Checksum mismatch"));

        let op_again = ops.start_operation(
            AgentOperationKind::Install,
            "agent-1".into(),
            "reg-1".into(),
        );
        assert!(op_again.is_ok());
    }
}
