pub mod observed;
pub use observed::{observed_task_id, parse_observed_update, tool_kind_str, ObservedUpdate};

use agent_client_protocol_schema::v1::TerminalExitStatus;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{oneshot, Notify, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Running,
    Completed,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalTaskSummary {
    pub id: String,
    pub chat_id: String,
    pub command: String,
    pub cwd: String,
    pub state: TaskState,
    pub exit_code: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    /// False for agent-owned observational tasks that Batey did not spawn.
    /// The frontend must not offer a Stop action when this is false.
    /// Defaults to true so older payloads stay stoppable.
    #[serde(default = "default_managed_true")]
    pub managed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalTaskDetails {
    pub id: String,
    pub chat_id: String,
    pub command: String,
    pub cwd: String,
    pub state: TaskState,
    pub exit_code: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub output: String,
    pub truncated: bool,
    /// False for agent-owned observational tasks that Batey did not spawn.
    #[serde(default = "default_managed_true")]
    pub managed: bool,
}

fn default_managed_true() -> bool {
    true
}

pub struct TerminalBuffer {
    pub output: String,
    pub truncated: bool,
}

pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024;

pub struct ManagedTask {
    pub id: String,
    pub chat_id: String,
    pub command: RwLock<String>,
    pub cwd: RwLock<PathBuf>,
    /// True for native ACP terminal/create tasks Batey spawns.
    /// False for observational agent-owned command executions.
    pub managed: bool,
    pub started_at: DateTime<Utc>,
    pub completed_at: RwLock<Option<DateTime<Utc>>>,
    pub state: RwLock<TaskState>,
    pub exit_code: RwLock<Option<i32>>,
    pub buffer: RwLock<TerminalBuffer>,
    pub output_limit: usize,
    pub exit_notify: Arc<Notify>,
    pub kill_tx: std::sync::Mutex<Option<oneshot::Sender<()>>>,
    pub killed_by_user: AtomicBool,
    pub exit_status: RwLock<Option<TerminalExitStatus>>,
}

impl ManagedTask {
    pub fn new(
        id: String,
        chat_id: String,
        command: String,
        cwd: PathBuf,
        output_limit: Option<u64>,
    ) -> Self {
        Self {
            id,
            chat_id,
            command: RwLock::new(command),
            cwd: RwLock::new(cwd),
            managed: true,
            started_at: Utc::now(),
            completed_at: RwLock::new(None),
            state: RwLock::new(TaskState::Running),
            exit_code: RwLock::new(None),
            buffer: RwLock::new(TerminalBuffer {
                output: String::new(),
                truncated: false,
            }),
            output_limit: match output_limit.and_then(|limit| usize::try_from(limit).ok()) {
                Some(requested) => requested.min(DEFAULT_MAX_OUTPUT_BYTES),
                None => DEFAULT_MAX_OUTPUT_BYTES,
            },
            exit_notify: Arc::new(Notify::new()),
            kill_tx: std::sync::Mutex::new(None),
            killed_by_user: AtomicBool::new(false),
            exit_status: RwLock::new(None),
        }
    }

    /// Creates an observational record for an agent-owned command execution.
    /// Batey never spawns a subprocess for these; the agent runs the command
    /// itself and reports structured lifecycle data through tool calls.
    pub fn new_observed(id: String, chat_id: String, command: String, cwd: PathBuf) -> Self {
        Self {
            id,
            chat_id,
            command: RwLock::new(command),
            cwd: RwLock::new(cwd),
            managed: false,
            started_at: Utc::now(),
            completed_at: RwLock::new(None),
            state: RwLock::new(TaskState::Running),
            exit_code: RwLock::new(None),
            buffer: RwLock::new(TerminalBuffer {
                output: String::new(),
                truncated: false,
            }),
            output_limit: DEFAULT_MAX_OUTPUT_BYTES,
            exit_notify: Arc::new(Notify::new()),
            kill_tx: std::sync::Mutex::new(None),
            killed_by_user: AtomicBool::new(false),
            exit_status: RwLock::new(None),
        }
    }

    pub fn is_managed(&self) -> bool {
        self.managed
    }

    pub fn stoppable(&self) -> bool {
        self.managed
    }

    pub async fn task_command(&self) -> String {
        self.command.read().await.clone()
    }

    pub async fn task_cwd(&self) -> PathBuf {
        self.cwd.read().await.clone()
    }

    pub async fn append_output(&self, chunk: &str) {
        let mut buffer = self.buffer.write().await;
        buffer.output.push_str(chunk);

        if buffer.output.len() > self.output_limit {
            let mut trim_at = buffer.output.len() - self.output_limit;
            while trim_at < buffer.output.len() && !buffer.output.is_char_boundary(trim_at) {
                trim_at += 1;
            }
            buffer.output.drain(..trim_at);
            buffer.truncated = true;
        }
    }

    pub async fn record_exit(&self, status: Result<std::process::ExitStatus, std::io::Error>) {
        let now = Utc::now();
        *self.completed_at.write().await = Some(now);

        let was_killed = self.killed_by_user.load(Ordering::SeqCst);
        let mut state_guard = self.state.write().await;

        let (exit_code, acp_exit_status) = match status {
            Ok(s) => {
                let code = s.code();
                if was_killed {
                    *state_guard = TaskState::Stopped;
                } else if code == Some(0) {
                    *state_guard = TaskState::Completed;
                } else {
                    *state_guard = TaskState::Failed;
                }
                *self.exit_code.write().await = code;
                let acp_status = TerminalExitStatus::new()
                    .exit_code(code.and_then(|c| u32::try_from(c).ok()))
                    .signal(None::<String>);
                (code, acp_status)
            }
            Err(e) => {
                if was_killed {
                    *state_guard = TaskState::Stopped;
                } else {
                    *state_guard = TaskState::Failed;
                }
                let acp_status =
                    TerminalExitStatus::new().signal(Some(format!("wait_error: {}", e)));
                (None, acp_status)
            }
        };

        *self.exit_status.write().await = Some(acp_exit_status);
        self.exit_notify.notify_waiters();
        tracing::debug!(
            task_id = %self.id,
            chat_id = %self.chat_id,
            exit_code = ?exit_code,
            state = ?*state_guard,
            "Terminal task finished"
        );
    }

    pub fn stop(&self) -> bool {
        // Observational tasks have no subprocess to kill. Never mark them
        // as user-stopped here; a misleading stop would corrupt the final
        // state mapping when the agent later reports completion.
        if !self.managed {
            return false;
        }
        self.killed_by_user.store(true, Ordering::SeqCst);
        if let Some(tx) = self.kill_tx.lock().unwrap().take() {
            let _ = tx.send(());
            true
        } else {
            false
        }
    }

    /// Updates metadata for an observational task. Late-arriving command or
    /// working directory fills the record; empty values never clear it.
    pub async fn update_observed_metadata(&self, command: Option<&str>, cwd: Option<&PathBuf>) {
        if self.managed {
            return;
        }
        if let Some(cmd) = command {
            let trimmed = cmd.trim();
            if !trimmed.is_empty() {
                let mut guard = self.command.write().await;
                if guard.trim().is_empty() || *guard != trimmed {
                    // Prefer the first non-empty command, but accept a later
                    // fuller value when the initial record used a placeholder.
                    if guard.trim().is_empty() || trimmed.len() >= guard.len() {
                        *guard = trimmed.to_string();
                    }
                }
            }
        }
        if let Some(dir) = cwd {
            let mut guard = self.cwd.write().await;
            if guard.as_os_str().is_empty() {
                *guard = dir.clone();
            }
        }
    }

    /// Merges new observational output without duplicating repeated snapshots.
    /// Snapshot growth replaces, out-of-order older snapshots are kept, and
    /// disjoint deltas accumulate with a newline separator.
    pub async fn merge_observed_output(&self, new_output: Option<&str>) {
        if self.managed {
            return;
        }
        let Some(new_raw) = new_output else { return };
        // Preserve trailing content but ignore pure whitespace updates.
        if new_raw.trim().is_empty() {
            return;
        }
        let limit = self.output_limit;
        let mut buffer = self.buffer.write().await;
        if buffer.output.is_empty() {
            buffer.output.push_str(new_raw);
        } else if buffer.output == new_raw {
            return;
        } else if new_raw.starts_with(buffer.output.as_str()) {
            buffer.output.clear();
            buffer.output.push_str(new_raw);
        } else if buffer.output.starts_with(new_raw) || buffer.output.contains(new_raw) {
            return;
        } else {
            if !buffer.output.ends_with('\n') {
                buffer.output.push('\n');
            }
            buffer.output.push_str(new_raw);
        }
        if buffer.output.len() > limit {
            let mut trim_at = buffer.output.len() - limit;
            while trim_at < buffer.output.len() && !buffer.output.is_char_boundary(trim_at) {
                trim_at += 1;
            }
            buffer.output.drain(..trim_at);
            buffer.truncated = true;
        }
    }

    /// Records an observational state transition. Terminal states are sticky:
    /// a late running update never reopens a completed task. The completion
    /// timestamp is set once on the first terminal transition.
    pub async fn record_observed_state(&self, state: TaskState, exit_code: Option<i32>) -> bool {
        if self.managed {
            return false;
        }
        if let Some(code) = exit_code {
            *self.exit_code.write().await = Some(code);
        }
        let mut state_guard = self.state.write().await;
        let current = *state_guard;
        let is_terminal = matches!(
            current,
            TaskState::Completed | TaskState::Failed | TaskState::Stopped
        );
        if is_terminal && state == TaskState::Running {
            return false;
        }
        if current == state && exit_code.is_none() {
            return false;
        }
        let became_terminal = !is_terminal
            && matches!(
                state,
                TaskState::Completed | TaskState::Failed | TaskState::Stopped
            );
        *state_guard = state;
        drop(state_guard);
        if became_terminal {
            let mut completed = self.completed_at.write().await;
            if completed.is_none() {
                *completed = Some(Utc::now());
            }
            self.exit_notify.notify_waiters();
        }
        // Update the ACP exit status mirror for observational tasks so
        // terminal/output queries stay consistent when they race updates.
        if let Some(code) = exit_code {
            let acp_status = TerminalExitStatus::new()
                .exit_code(u32::try_from(code).ok())
                .signal(None::<String>);
            *self.exit_status.write().await = Some(acp_status);
        }
        became_terminal
    }

    /// Applies one observational lifecycle update. Returns true when the task
    /// newly reached a terminal state (the caller emits a metadata change).
    pub async fn apply_observed_update(
        &self,
        command: Option<&str>,
        cwd: Option<&PathBuf>,
        output: Option<&str>,
        exit_code: Option<i32>,
        state: TaskState,
    ) -> bool {
        self.update_observed_metadata(command, cwd).await;
        self.merge_observed_output(output).await;
        self.record_observed_state(state, exit_code).await
    }

    pub async fn summary(&self) -> TerminalTaskSummary {
        TerminalTaskSummary {
            id: self.id.clone(),
            chat_id: self.chat_id.clone(),
            command: self.command.read().await.clone(),
            cwd: self.cwd.read().await.to_string_lossy().to_string(),
            state: *self.state.read().await,
            exit_code: *self.exit_code.read().await,
            started_at: self.started_at,
            completed_at: *self.completed_at.read().await,
            managed: self.managed,
        }
    }

    pub async fn details(&self) -> TerminalTaskDetails {
        let buffer = self.buffer.read().await;
        TerminalTaskDetails {
            id: self.id.clone(),
            chat_id: self.chat_id.clone(),
            command: self.command.read().await.clone(),
            cwd: self.cwd.read().await.to_string_lossy().to_string(),
            state: *self.state.read().await,
            exit_code: *self.exit_code.read().await,
            started_at: self.started_at,
            completed_at: *self.completed_at.read().await,
            output: buffer.output.clone(),
            truncated: buffer.truncated,
            managed: self.managed,
        }
    }
}

#[derive(Default)]
struct TrackerInner {
    tasks_by_chat: HashMap<String, VecDeque<Arc<ManagedTask>>>,
    tasks_by_id: HashMap<String, Arc<ManagedTask>>,
}

pub struct TerminalTaskTracker {
    max_tasks_per_chat: usize,
    inner: RwLock<TrackerInner>,
}

impl Default for TerminalTaskTracker {
    fn default() -> Self {
        Self::new(50)
    }
}

impl TerminalTaskTracker {
    pub fn new(max_tasks_per_chat: usize) -> Self {
        Self {
            max_tasks_per_chat,
            inner: RwLock::new(TrackerInner::default()),
        }
    }

    pub async fn register_task(&self, task: Arc<ManagedTask>) {
        {
            let mut inner = self.inner.write().await;
            inner.tasks_by_id.insert(task.id.clone(), task.clone());
            inner
                .tasks_by_chat
                .entry(task.chat_id.clone())
                .or_default()
                .push_back(task.clone());
        }
        self.prune_chat_tasks(&task.chat_id).await;
    }

    pub async fn prune_chat_tasks(&self, chat_id: &str) {
        let candidates = {
            let inner = self.inner.read().await;
            match inner.tasks_by_chat.get(chat_id) {
                Some(queue) if queue.len() > self.max_tasks_per_chat => {
                    queue.iter().cloned().collect::<Vec<_>>()
                }
                _ => return,
            }
        };

        let mut completed_task_ids = Vec::new();
        for candidate in &candidates {
            let state = *candidate.state.read().await;
            if state != TaskState::Running {
                completed_task_ids.push(candidate.id.clone());
            }
        }

        if completed_task_ids.is_empty() {
            return;
        }

        let mut inner = self.inner.write().await;
        let TrackerInner {
            tasks_by_chat,
            tasks_by_id,
        } = &mut *inner;
        if let Some(queue) = tasks_by_chat.get_mut(chat_id) {
            let mut to_remove = queue.len().saturating_sub(self.max_tasks_per_chat);
            for task_id in completed_task_ids {
                if to_remove == 0 {
                    break;
                }
                if let Some(pos) = queue.iter().position(|t| t.id == task_id) {
                    queue.remove(pos);
                    tasks_by_id.remove(&task_id);
                    to_remove -= 1;
                }
            }
        }
    }

    pub async fn get_task(&self, task_id: &str) -> Option<Arc<ManagedTask>> {
        self.inner.read().await.tasks_by_id.get(task_id).cloned()
    }

    /// Creates or updates an observational task for an agent-owned command.
    /// Returns the task and whether it newly reached a terminal state.
    /// `task_id` is the agent's tool-call id, which is only unique within its
    /// own chat; the tracker's public id namespaces it by chat (see
    /// [`observed_task_id`]), so the same tool id in two chats yields one task
    /// per chat. Managed terminal ids live in a disjoint namespace and keep
    /// their current identity semantics.
    /// A missing command means there is not enough structured information;
    /// the update is ignored unless the task already exists.
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_observed(
        &self,
        chat_id: &str,
        task_id: &str,
        command: Option<&str>,
        cwd: Option<&PathBuf>,
        output: Option<&str>,
        exit_code: Option<i32>,
        state: TaskState,
    ) -> Option<(Arc<ManagedTask>, bool)> {
        let public_id = observed_task_id(chat_id, task_id);
        let existing = self.get_task(&public_id).await;
        if let Some(task) = existing {
            debug_assert!(!task.managed);
            let became_terminal = task
                .apply_observed_update(command, cwd, output, exit_code, state)
                .await;
            if became_terminal {
                self.prune_chat_tasks(chat_id).await;
            }
            return Some((task, became_terminal));
        }
        let cmd = command.map(str::trim).filter(|s| !s.is_empty())?;
        if cmd.is_empty() {
            return None;
        }
        let cwd_value = cwd.cloned().unwrap_or_else(|| PathBuf::from(""));
        let task = Arc::new(ManagedTask::new_observed(
            public_id.clone(),
            chat_id.to_string(),
            cmd.to_string(),
            cwd_value,
        ));
        // Apply the initial output/exit/state without overwriting the command
        // that was just set. Metadata was already provided above.
        task.merge_observed_output(output).await;
        let became_terminal = task.record_observed_state(state, exit_code).await;
        // Insert directly so a concurrent duplicate upsert cannot produce a
        // duplicate queue entry.
        {
            let mut inner = self.inner.write().await;
            if let Some(existing) = inner.tasks_by_id.get(&public_id) {
                return Some((existing.clone(), false));
            }
            inner.tasks_by_id.insert(task.id.clone(), task.clone());
            inner
                .tasks_by_chat
                .entry(chat_id.to_string())
                .or_default()
                .push_back(task.clone());
        }
        self.prune_chat_tasks(chat_id).await;
        Some((task, became_terminal))
    }

    /// Fetches an observational task by its chat and agent tool-call id.
    pub async fn get_observed_task(
        &self,
        chat_id: &str,
        tool_call_id: &str,
    ) -> Option<Arc<ManagedTask>> {
        self.get_task(&observed_task_id(chat_id, tool_call_id))
            .await
    }

    /// Whether the id names a managed terminal Batey spawned. Used to
    /// deduplicate tool-call content that merely embeds a native terminal.
    pub async fn is_managed_task(&self, task_id: &str) -> bool {
        self.inner
            .read()
            .await
            .tasks_by_id
            .get(task_id)
            .is_some_and(|t| t.managed)
    }

    pub async fn list_chat_tasks(&self, chat_id: &str) -> Vec<TerminalTaskSummary> {
        let tasks = {
            let inner = self.inner.read().await;
            inner.tasks_by_chat.get(chat_id).cloned()
        };
        let Some(tasks) = tasks else {
            return Vec::new();
        };

        let mut summaries = Vec::with_capacity(tasks.len());
        for task in tasks.iter().rev() {
            summaries.push(task.summary().await);
        }
        summaries
    }

    pub async fn active_task_count(&self, chat_id: &str) -> usize {
        let tasks = {
            let inner = self.inner.read().await;
            inner.tasks_by_chat.get(chat_id).cloned()
        };
        let Some(tasks) = tasks else {
            return 0;
        };

        let mut count = 0;
        for task in &tasks {
            if *task.state.read().await == TaskState::Running {
                count += 1;
            }
        }
        count
    }

    pub async fn stop_chat_tasks(&self, chat_id: &str) {
        let tasks = {
            let inner = self.inner.read().await;
            inner.tasks_by_chat.get(chat_id).cloned()
        };
        if let Some(tasks) = tasks {
            for task in tasks {
                task.stop();
            }
        }
    }

    pub async fn forget_chat(&self, chat_id: &str) {
        let mut inner = self.inner.write().await;
        if let Some(tasks) = inner.tasks_by_chat.remove(chat_id) {
            for task in tasks {
                inner.tasks_by_id.remove(&task.id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_output_limit_default_and_agent_override() {
        let task_default = ManagedTask::new(
            "task-1".into(),
            "chat-1".into(),
            "test".into(),
            PathBuf::from("/tmp"),
            None,
        );
        assert_eq!(task_default.output_limit, DEFAULT_MAX_OUTPUT_BYTES);

        let task_small = ManagedTask::new(
            "task-2".into(),
            "chat-1".into(),
            "test".into(),
            PathBuf::from("/tmp"),
            Some(20),
        );
        assert_eq!(task_small.output_limit, 20);
        task_small
            .append_output("hello world 1234567890 extra bytes")
            .await;
        let details = task_small.details().await;
        assert_eq!(details.output.len(), 20);
        assert!(details.truncated);

        let task_large = ManagedTask::new(
            "task-3".into(),
            "chat-1".into(),
            "test".into(),
            PathBuf::from("/tmp"),
            Some(100 * 1024 * 1024),
        );
        assert_eq!(task_large.output_limit, DEFAULT_MAX_OUTPUT_BYTES);
    }

    #[tokio::test]
    async fn test_forget_chat_clears_tasks_and_indexes() {
        let tracker = TerminalTaskTracker::new(10);
        let task1 = Arc::new(ManagedTask::new(
            "t1".into(),
            "c1".into(),
            "echo 1".into(),
            PathBuf::from("/tmp"),
            None,
        ));
        let task2 = Arc::new(ManagedTask::new(
            "t2".into(),
            "c2".into(),
            "echo 2".into(),
            PathBuf::from("/tmp"),
            None,
        ));

        tracker.register_task(task1.clone()).await;
        tracker.register_task(task2.clone()).await;

        assert_eq!(tracker.list_chat_tasks("c1").await.len(), 1);
        assert_eq!(tracker.list_chat_tasks("c2").await.len(), 1);
        assert!(tracker.get_task("t1").await.is_some());
        assert!(tracker.get_task("t2").await.is_some());

        tracker.forget_chat("c1").await;

        assert_eq!(tracker.list_chat_tasks("c1").await.len(), 0);
        assert!(tracker.get_task("t1").await.is_none());
        assert_eq!(tracker.list_chat_tasks("c2").await.len(), 1);
        assert!(tracker.get_task("t2").await.is_some());
    }
    #[tokio::test]
    async fn test_prune_on_task_completion_without_discarding_running_tasks() {
        let tracker = Arc::new(TerminalTaskTracker::new(2));
        let t1 = Arc::new(ManagedTask::new(
            "t1".into(),
            "c1".into(),
            "echo 1".into(),
            PathBuf::from("/tmp"),
            None,
        ));
        let t2 = Arc::new(ManagedTask::new(
            "t2".into(),
            "c1".into(),
            "echo 2".into(),
            PathBuf::from("/tmp"),
            None,
        ));
        let t3 = Arc::new(ManagedTask::new(
            "t3".into(),
            "c1".into(),
            "echo 3".into(),
            PathBuf::from("/tmp"),
            None,
        ));

        tracker.register_task(t1.clone()).await;
        tracker.register_task(t2.clone()).await;
        tracker.register_task(t3.clone()).await;

        // All 3 are running, so none should be pruned even though max is 2
        assert_eq!(tracker.list_chat_tasks("c1").await.len(), 3);

        // t1 completes -> triggers pruning on task completion
        t1.record_exit(Ok(std::process::ExitStatus::default()))
            .await;
        tracker.prune_chat_tasks("c1").await;

        // Now t1 should be pruned, queue length drops to 2 (t2 and t3, which are still running)
        let remaining = tracker.list_chat_tasks("c1").await;
        assert_eq!(remaining.len(), 2);
        assert!(tracker.get_task("t1").await.is_none());
        assert!(tracker.get_task("t2").await.is_some());
        assert!(tracker.get_task("t3").await.is_some());
    }

    #[tokio::test]
    async fn test_no_deadlock_between_register_and_forget() {
        let tracker = Arc::new(TerminalTaskTracker::new(50));
        let tracker1 = tracker.clone();
        let tracker2 = tracker.clone();

        let h1 = tokio::spawn(async move {
            for i in 0..100 {
                let task = Arc::new(ManagedTask::new(
                    format!("reg-{}", i),
                    "c1".into(),
                    "cmd".into(),
                    PathBuf::from("/tmp"),
                    None,
                ));
                tracker1.register_task(task).await;
            }
        });

        let h2 = tokio::spawn(async move {
            for _ in 0..100 {
                tracker2.forget_chat("c1").await;
            }
        });

        tokio::try_join!(h1, h2).unwrap();
    }

    #[tokio::test]
    async fn test_observed_lifecycle_running_to_completed_with_output() {
        let tracker = TerminalTaskTracker::new(10);
        let cwd = PathBuf::from("/repo");
        // Initial tool call before output creates a running task.
        let (task, became_terminal) = tracker
            .upsert_observed(
                "c1",
                "tool-1",
                Some("cargo test"),
                Some(&cwd),
                None,
                None,
                TaskState::Running,
            )
            .await
            .expect("observed creation needs a command");
        assert!(!became_terminal);
        assert!(!task.managed);
        assert_eq!(task.task_command().await, "cargo test");
        let summary = task.summary().await;
        assert!(!summary.managed);
        assert_eq!(summary.state, TaskState::Running);

        // Updates before full metadata and repeated updates accumulate without
        // duplication.
        let (_, became_terminal) = tracker
            .upsert_observed(
                "c1",
                "tool-1",
                None,
                None,
                Some("running tests..."),
                None,
                TaskState::Running,
            )
            .await
            .unwrap();
        assert!(!became_terminal);
        let (_, became_terminal) = tracker
            .upsert_observed(
                "c1",
                "tool-1",
                None,
                None,
                Some("running tests..."),
                None,
                TaskState::Running,
            )
            .await
            .unwrap();
        assert!(!became_terminal);
        let details = tracker
            .get_observed_task("c1", "tool-1")
            .await
            .unwrap()
            .details()
            .await;
        assert_eq!(details.output, "running tests...");

        // Completion with exit code and final output.
        let (_, became_terminal) = tracker
            .upsert_observed(
                "c1",
                "tool-1",
                None,
                None,
                Some("test result: ok"),
                Some(0),
                TaskState::Completed,
            )
            .await
            .unwrap();
        assert!(became_terminal);
        let details = tracker
            .get_observed_task("c1", "tool-1")
            .await
            .unwrap()
            .details()
            .await;
        assert_eq!(details.state, TaskState::Completed);
        assert_eq!(details.exit_code, Some(0));
        assert!(details.completed_at.is_some());
        assert!(details.output.contains("test result: ok"));
        // Sticky terminal: a late running update never reopens it.
        let (_, became_terminal) = tracker
            .upsert_observed(
                "c1",
                "tool-1",
                None,
                None,
                Some("stale running"),
                None,
                TaskState::Running,
            )
            .await
            .unwrap();
        assert!(!became_terminal);
        let details = tracker
            .get_observed_task("c1", "tool-1")
            .await
            .unwrap()
            .details()
            .await;
        assert_eq!(details.state, TaskState::Completed);
    }

    #[tokio::test]
    async fn test_observed_completion_without_exit_code_and_failed_state() {
        let tracker = TerminalTaskTracker::new(10);
        let cwd = PathBuf::from("/tmp");
        tracker
            .upsert_observed(
                "c1",
                "tool-fail",
                Some("cargo check"),
                Some(&cwd),
                Some("error: failed"),
                None,
                TaskState::Running,
            )
            .await
            .unwrap();
        let (_, became_terminal) = tracker
            .upsert_observed(
                "c1",
                "tool-fail",
                None,
                None,
                None,
                Some(1),
                TaskState::Failed,
            )
            .await
            .unwrap();
        assert!(became_terminal);
        let details = tracker
            .get_observed_task("c1", "tool-fail")
            .await
            .unwrap()
            .details()
            .await;
        assert_eq!(details.state, TaskState::Failed);
        assert_eq!(details.exit_code, Some(1));
    }

    #[tokio::test]
    async fn test_observed_requires_command() {
        let tracker = TerminalTaskTracker::new(10);
        let cwd = PathBuf::from("/tmp");
        assert!(tracker
            .upsert_observed(
                "c1",
                "tool-nocmd",
                None,
                Some(&cwd),
                Some("hi"),
                None,
                TaskState::Running
            )
            .await
            .is_none());
    }

    #[tokio::test]
    async fn test_observed_same_tool_id_in_two_chats_yields_two_tasks() {
        // Agent-owned tool-call ids are only unique within their own chat.
        // The same id reported by two chats must produce one task per chat
        // rather than dropping the second.
        let tracker = TerminalTaskTracker::new(10);
        let cwd = PathBuf::from("/tmp");
        let (first, _) = tracker
            .upsert_observed(
                "c1",
                "tool-1",
                Some("echo hi"),
                Some(&cwd),
                Some("hi from c1"),
                None,
                TaskState::Running,
            )
            .await
            .unwrap();
        let (second, _) = tracker
            .upsert_observed(
                "c2",
                "tool-1",
                Some("echo hi"),
                Some(&cwd),
                Some("hi from c2"),
                None,
                TaskState::Running,
            )
            .await
            .unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(first.chat_id, "c1");
        assert_eq!(second.chat_id, "c2");
        assert_eq!(tracker.list_chat_tasks("c1").await.len(), 1);
        assert_eq!(tracker.list_chat_tasks("c2").await.len(), 1);
        assert_eq!(
            tracker
                .get_observed_task("c1", "tool-1")
                .await
                .unwrap()
                .details()
                .await
                .output,
            "hi from c1"
        );
        assert_eq!(
            tracker
                .get_observed_task("c2", "tool-1")
                .await
                .unwrap()
                .details()
                .await
                .output,
            "hi from c2"
        );
        // Completing one chat's task leaves the other's running.
        tracker
            .upsert_observed(
                "c1",
                "tool-1",
                None,
                None,
                None,
                Some(0),
                TaskState::Completed,
            )
            .await
            .unwrap();
        assert_eq!(
            tracker
                .get_observed_task("c1", "tool-1")
                .await
                .unwrap()
                .details()
                .await
                .state,
            TaskState::Completed
        );
        assert_eq!(
            tracker
                .get_observed_task("c2", "tool-1")
                .await
                .unwrap()
                .details()
                .await
                .state,
            TaskState::Running
        );
    }

    #[tokio::test]
    async fn test_managed_and_observed_namespaces_do_not_collide() {
        // Managed terminal ids and observational task ids live in disjoint
        // namespaces, so an agent tool id equal to a terminal id can never
        // overwrite the native record.
        let tracker = TerminalTaskTracker::new(10);
        let managed = Arc::new(ManagedTask::new(
            "shared-id".into(),
            "c1".into(),
            "sleep 30".into(),
            PathBuf::from("/tmp"),
            None,
        ));
        tracker.register_task(managed.clone()).await;
        let cwd = PathBuf::from("/tmp");
        let (observed, _) = tracker
            .upsert_observed(
                "c1",
                "shared-id",
                Some("sleep 30"),
                Some(&cwd),
                Some("agent output"),
                Some(0),
                TaskState::Completed,
            )
            .await
            .unwrap();
        assert!(!observed.managed);
        assert_ne!(observed.id, managed.id);
        // Managed output is never overwritten by observational data.
        assert!(managed.details().await.output.is_empty());
        assert_eq!(observed.details().await.output, "agent output");
        assert_eq!(tracker.list_chat_tasks("c1").await.len(), 2);
    }

    #[tokio::test]
    async fn test_observed_stop_is_unsupported_and_managed_stop_works() {
        let tracker = TerminalTaskTracker::new(10);
        let cwd = PathBuf::from("/tmp");
        let (observed, _) = tracker
            .upsert_observed(
                "c1",
                "tool-obs",
                Some("npm run dev"),
                Some(&cwd),
                None,
                None,
                TaskState::Running,
            )
            .await
            .unwrap();
        assert!(!observed.stoppable());
        assert!(!observed.stop());
        assert_eq!(observed.details().await.state, TaskState::Running);
        let managed = Arc::new(ManagedTask::new(
            "managed-1".into(),
            "c1".into(),
            "sleep 30".into(),
            PathBuf::from("/tmp"),
            None,
        ));
        assert!(managed.stoppable());
    }

    #[tokio::test]
    async fn test_observed_history_stays_bounded() {
        let tracker = TerminalTaskTracker::new(2);
        let cwd = PathBuf::from("/tmp");
        for i in 0..4 {
            let id = format!("tool-{i}");
            tracker
                .upsert_observed(
                    "c1",
                    &id,
                    Some(&format!("cmd {i}")),
                    Some(&cwd),
                    None,
                    Some(0),
                    TaskState::Completed,
                )
                .await
                .unwrap();
        }
        // Retention keeps at most max completed tasks; running tasks are never
        // pruned.
        assert!(tracker.list_chat_tasks("c1").await.len() <= 4);
        let running = tracker
            .upsert_observed(
                "c1",
                "tool-running",
                Some("sleep 30"),
                Some(&cwd),
                None,
                None,
                TaskState::Running,
            )
            .await
            .unwrap()
            .0;
        assert_eq!(running.details().await.state, TaskState::Running);
    }
}
