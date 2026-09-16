//! Terminal authentication flows.
//!
//! One flow owns one PTY process. The flow bounds what it keeps, ends on its
//! own when a client abandons it, and kills the whole process tree whenever
//! it stops.
//!
//! Terminal input and terminal output never reach the event log, the store,
//! or the tracing log. They exist only in the bounded in-memory scrollback
//! and on the live flow socket. Diagnostics report lifecycle state instead.
use super::pty::{self, PtyCommand, PtyExit, PtyHandle, PtyWindow};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, watch, Mutex};

/// Retained terminal output. A TUI redraws constantly, so only the tail is
/// useful, and an unbounded buffer would be a memory leak.
pub const MAX_SCROLLBACK_BYTES: usize = 64 * 1024;
/// Live chunks a slow socket may fall behind by before it resynchronizes.
const OUTPUT_BROADCAST_DEPTH: usize = 256;
/// How long one flow may live, even with a client attached.
pub const MAX_FLOW_LIFETIME: Duration = Duration::from_secs(15 * 60);
/// How long a flow survives with no client attached.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// How often the supervisor rechecks the idle bound.
const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How long a finished flow stays readable before the registry drops it.
const FINISHED_RETENTION: Duration = Duration::from_secs(120);
/// Concurrent unfinished flows across all agents.
pub const MAX_ACTIVE_FLOWS: usize = 4;
/// Concurrent unfinished flows for one agent.
pub const MAX_ACTIVE_FLOWS_PER_AGENT: usize = 1;

/// The lifecycle of one terminal authentication flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalFlowState {
    Running,
    /// The command exited with status zero.
    Succeeded,
    /// The command failed, was signalled, or never started.
    Failed,
    /// A client cancelled the flow.
    Cancelled,
    /// The flow reached its idle or overall lifetime.
    TimedOut,
}

impl TerminalFlowState {
    pub fn is_finished(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// The safe lifecycle summary of one flow. It never carries terminal output.
#[derive(Debug, Clone, Serialize)]
pub struct TerminalAuthFlowView {
    pub flow_id: String,
    pub agent_id: String,
    pub method_id: String,
    pub state: TerminalFlowState,
    pub exit_code: Option<u32>,
    /// Why the flow ended, in Batey's own words. Never a transcript.
    pub reason: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
struct FlowStatus {
    state: TerminalFlowState,
    exit_code: Option<u32>,
    reason: Option<String>,
    completed_at: Option<DateTime<Utc>>,
}

/// What a newly attached client needs: the retained tail, then the live feed.
pub struct FlowAttachment {
    pub scrollback: Vec<u8>,
    pub output: broadcast::Receiver<Arc<Vec<u8>>>,
    pub status: watch::Receiver<()>,
}

/// A synchronous hook that runs exactly once when a flow succeeds.
///
/// It runs under the status lock, before the state assignment, so no client
/// can observe `succeeded` before the hook ends. A hook must be cheap,
/// because it holds up both the transition and every status poller.
pub type SuccessHook = Arc<dyn Fn() + Send + Sync>;

struct OutputBuffer {
    scrollback: VecDeque<u8>,
    sender: broadcast::Sender<Arc<Vec<u8>>>,
}

/// One terminal authentication flow and the PTY behind it.
pub struct TerminalAuthFlow {
    pub id: String,
    pub agent_id: String,
    pub method_id: String,
    pub started_at: DateTime<Utc>,
    status: StdMutex<FlowStatus>,
    status_tx: watch::Sender<()>,
    status_rx: watch::Receiver<()>,
    output: Mutex<OutputBuffer>,
    handle: Arc<PtyHandle>,
    attached: AtomicUsize,
    last_detached: StdMutex<Instant>,
    on_success: Option<SuccessHook>,
}

impl TerminalAuthFlow {
    pub fn view(&self) -> TerminalAuthFlowView {
        let status = self.status();
        TerminalAuthFlowView {
            flow_id: self.id.clone(),
            agent_id: self.agent_id.clone(),
            method_id: self.method_id.clone(),
            state: status.state,
            exit_code: status.exit_code,
            reason: status.reason,
            started_at: self.started_at,
            completed_at: status.completed_at,
        }
    }

    pub fn state(&self) -> TerminalFlowState {
        self.status().state
    }

    fn status(&self) -> FlowStatus {
        self.status
            .lock()
            .expect("terminal auth status lock poisoned")
            .clone()
    }

    /// Writes user keystrokes to the PTY. Input is never stored or logged.
    pub fn send_input(&self, bytes: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.state().is_finished(),
            "This authentication flow ended"
        );
        self.handle.write_input(bytes)
    }

    pub fn resize(&self, window: PtyWindow) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.state().is_finished(),
            "This authentication flow ended"
        );
        self.handle.resize(window)
    }

    /// Takes the retained tail and the live feed together, so an attaching
    /// client neither loses nor repeats a chunk.
    pub async fn attach(&self) -> FlowAttachment {
        let buffer = self.output.lock().await;
        let attachment = FlowAttachment {
            scrollback: buffer.scrollback.iter().copied().collect(),
            output: buffer.sender.subscribe(),
            status: self.status_rx.clone(),
        };
        drop(buffer);
        self.attached.fetch_add(1, Ordering::SeqCst);
        attachment
    }

    pub fn detach(&self) {
        // `fetch_update` keeps the count at zero if a detach ever runs twice.
        let _ = self
            .attached
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                Some(count.saturating_sub(1))
            });
        *self
            .last_detached
            .lock()
            .expect("terminal auth idle lock poisoned") = Instant::now();
    }

    /// Ends the flow and kills the whole process tree.
    ///
    /// The first caller wins, so a cancel that races the program's own exit
    /// never overwrites the recorded outcome.
    pub fn finish(&self, state: TerminalFlowState, exit_code: Option<u32>, reason: Option<String>) {
        {
            let mut status = self
                .status
                .lock()
                .expect("terminal auth status lock poisoned");
            if status.state.is_finished() {
                return;
            }
            // The hook runs under the status lock and before the state
            // assignment, so the transition itself is the publication
            // barrier: no poller of the status view can observe `succeeded`
            // before the success has invalidated the cached auth state.
            if state == TerminalFlowState::Succeeded {
                if let Some(hook) = &self.on_success {
                    hook();
                }
            }
            status.state = state;
            status.exit_code = exit_code;
            status.reason = reason;
            status.completed_at = Some(Utc::now());
        }
        self.handle.kill_tree();
        // A send failure only means nobody is watching.
        let _ = self.status_tx.send(());
        tracing::info!(
            flow = %self.id,
            agent = %self.agent_id,
            method = %self.method_id,
            state = ?state,
            "terminal authentication flow ended"
        );
    }

    pub fn cancel(&self) {
        self.finish(
            TerminalFlowState::Cancelled,
            None,
            Some("Cancelled by the client".into()),
        );
    }

    /// Resolves once the flow reaches a finished state.
    pub async fn wait_finished(&self) -> TerminalFlowState {
        let mut status_rx = self.status_rx.clone();
        loop {
            let state = self.state();
            if state.is_finished() {
                return state;
            }
            if status_rx.changed().await.is_err() {
                return self.state();
            }
        }
    }

    fn idle_for(&self) -> Duration {
        if self.attached.load(Ordering::SeqCst) > 0 {
            return Duration::ZERO;
        }
        self.last_detached
            .lock()
            .expect("terminal auth idle lock poisoned")
            .elapsed()
    }
}

/// Every live terminal authentication flow.
pub struct TerminalAuthFlows {
    flows: StdMutex<HashMap<String, Arc<TerminalAuthFlow>>>,
    idle_timeout: Duration,
    max_lifetime: Duration,
}

impl Default for TerminalAuthFlows {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalAuthFlows {
    pub fn new() -> Self {
        Self::with_bounds(IDLE_TIMEOUT, MAX_FLOW_LIFETIME)
    }

    /// The same registry with explicit bounds. Tests use short ones so the
    /// abandonment rules are provable without a long wait.
    pub fn with_bounds(idle_timeout: Duration, max_lifetime: Duration) -> Self {
        Self {
            flows: StdMutex::new(HashMap::new()),
            idle_timeout,
            max_lifetime,
        }
    }

    pub fn get(&self, flow_id: &str) -> Option<Arc<TerminalAuthFlow>> {
        self.flows
            .lock()
            .expect("terminal auth registry lock poisoned")
            .get(flow_id)
            .cloned()
    }

    /// One unfinished flow for this agent, when one exists. Used for
    /// recovery after navigation or reload. Finished flows are never
    /// returned; only `running` counts as active.
    pub fn active_for_agent(&self, agent_id: &str) -> Option<Arc<TerminalAuthFlow>> {
        self.flows
            .lock()
            .expect("terminal auth registry lock poisoned")
            .values()
            .filter(|flow| flow.agent_id == agent_id)
            .filter(|flow| !flow.state().is_finished())
            .max_by_key(|flow| flow.started_at)
            .cloned()
    }

    /// Starts one flow for one prepared command.
    ///
    /// The caller has already derived the command from the installed runtime
    /// and from the advertised authentication method. `on_success`, when
    /// given, runs once inside the transition to `succeeded`.
    pub fn start(
        self: &Arc<Self>,
        agent_id: &str,
        method_id: &str,
        command: &PtyCommand,
        on_success: Option<SuccessHook>,
    ) -> anyhow::Result<Arc<TerminalAuthFlow>> {
        anyhow::ensure!(
            pty::TERMINAL_AUTH_SUPPORTED,
            "Terminal authentication is not supported on this platform"
        );
        self.prune();
        self.check_bounds(agent_id)?;

        let spawned = pty::spawn(command, pty::DEFAULT_WINDOW)?;
        let (status_tx, status_rx) = watch::channel(());
        let (output_tx, _) = broadcast::channel(OUTPUT_BROADCAST_DEPTH);
        let flow = Arc::new(TerminalAuthFlow {
            id: new_flow_id(),
            agent_id: agent_id.to_owned(),
            method_id: method_id.to_owned(),
            started_at: Utc::now(),
            status: StdMutex::new(FlowStatus {
                state: TerminalFlowState::Running,
                exit_code: None,
                reason: None,
                completed_at: None,
            }),
            status_tx,
            status_rx,
            output: Mutex::new(OutputBuffer {
                scrollback: VecDeque::new(),
                sender: output_tx,
            }),
            handle: spawned.handle,
            attached: AtomicUsize::new(0),
            last_detached: StdMutex::new(Instant::now()),
            on_success,
        });

        {
            let mut flows = self
                .flows
                .lock()
                .expect("terminal auth registry lock poisoned");
            flows.insert(flow.id.clone(), flow.clone());
        }

        tokio::spawn(pump_output(flow.clone(), spawned.output));
        tokio::spawn(supervise(
            flow.clone(),
            spawned.exit,
            self.idle_timeout,
            self.max_lifetime,
        ));
        tracing::info!(
            flow = %flow.id,
            agent = %agent_id,
            method = %method_id,
            "terminal authentication flow started"
        );
        Ok(flow)
    }

    /// Ends every flow and kills every process tree. Server shutdown calls
    /// this, so an abandoned login never outlives Batey.
    pub fn shutdown_all(&self) {
        let flows: Vec<Arc<TerminalAuthFlow>> = self
            .flows
            .lock()
            .expect("terminal auth registry lock poisoned")
            .values()
            .cloned()
            .collect();
        for flow in flows {
            flow.finish(
                TerminalFlowState::Cancelled,
                None,
                Some("Batey is shutting down".into()),
            );
        }
        self.flows
            .lock()
            .expect("terminal auth registry lock poisoned")
            .clear();
    }

    fn check_bounds(&self, agent_id: &str) -> anyhow::Result<()> {
        let flows = self
            .flows
            .lock()
            .expect("terminal auth registry lock poisoned");
        let active: Vec<&Arc<TerminalAuthFlow>> = flows
            .values()
            .filter(|flow| !flow.state().is_finished())
            .collect();
        anyhow::ensure!(
            active.len() < MAX_ACTIVE_FLOWS,
            "Too many authentication flows are already running. Finish or cancel one first."
        );
        anyhow::ensure!(
            active.iter().filter(|f| f.agent_id == agent_id).count() < MAX_ACTIVE_FLOWS_PER_AGENT,
            "Agent '{agent_id}' already has an authentication flow running. Finish or cancel it first."
        );
        Ok(())
    }

    /// Drops finished flows the client no longer needs.
    fn prune(&self) {
        let now = Utc::now();
        self.flows
            .lock()
            .expect("terminal auth registry lock poisoned")
            .retain(|_, flow| match flow.status().completed_at {
                Some(completed) => {
                    let age = now.signed_duration_since(completed);
                    age.to_std()
                        .map(|age| age < FINISHED_RETENTION)
                        .unwrap_or(true)
                }
                None => true,
            });
    }
}

/// Copies PTY output into the bounded scrollback and to live clients.
async fn pump_output(
    flow: Arc<TerminalAuthFlow>,
    mut output: tokio::sync::mpsc::Receiver<Vec<u8>>,
) {
    while let Some(chunk) = output.recv().await {
        let chunk = Arc::new(chunk);
        let mut buffer = flow.output.lock().await;
        buffer.scrollback.extend(chunk.iter().copied());
        while buffer.scrollback.len() > MAX_SCROLLBACK_BYTES {
            let excess = buffer.scrollback.len() - MAX_SCROLLBACK_BYTES;
            buffer.scrollback.drain(..excess);
        }
        // No receiver is normal: nobody is watching this flow right now.
        let _ = buffer.sender.send(chunk);
    }
}

/// Ends the flow on program exit, on the idle bound, or on the lifetime bound.
async fn supervise(
    flow: Arc<TerminalAuthFlow>,
    exit: tokio::sync::oneshot::Receiver<PtyExit>,
    idle_timeout: Duration,
    max_lifetime: Duration,
) {
    let deadline = tokio::time::Instant::now() + max_lifetime;
    let poll = IDLE_POLL_INTERVAL.min(idle_timeout);
    tokio::pin!(exit);
    loop {
        tokio::select! {
            outcome = &mut exit => {
                match outcome {
                    // A zero exit status is the stable success signal. Every
                    // other end of the command is a failure.
                    Ok(exit) if exit.succeeded() => {
                        flow.finish(TerminalFlowState::Succeeded, Some(0), None)
                    }
                    Ok(PtyExit::Code(code)) => flow.finish(
                        TerminalFlowState::Failed,
                        Some(code),
                        Some(format!("The authentication command exited with status {code}")),
                    ),
                    Ok(PtyExit::Signal(signal)) => flow.finish(
                        TerminalFlowState::Failed,
                        None,
                        Some(format!("A signal ({signal}) ended the authentication command")),
                    ),
                    Err(_) => flow.finish(
                        TerminalFlowState::Failed,
                        None,
                        Some("The authentication command could not be observed".into()),
                    ),
                }
                return;
            }
            _ = tokio::time::sleep_until(deadline) => {
                flow.finish(
                    TerminalFlowState::TimedOut,
                    None,
                    Some("The authentication flow reached its time limit".into()),
                );
                return;
            }
            _ = tokio::time::sleep(poll) => {
                if flow.state().is_finished() {
                    return;
                }
                if flow.idle_for() >= idle_timeout {
                    flow.finish(
                        TerminalFlowState::TimedOut,
                        None,
                        Some("No client watched the authentication flow".into()),
                    );
                    return;
                }
            }
        }
    }
}

/// A flow id a client cannot guess.
///
/// Two version 4 UUIDs supply 244 random bits from the operating system
/// generator. The id is the only credential the flow socket needs, so it must
/// be opaque.
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

    /// A command that waits until something ends it.
    #[cfg(unix)]
    fn waiting_command() -> PtyCommand {
        PtyCommand {
            program: "sleep".into(),
            args: vec!["600".into()],
            env: std::env::vars().collect(),
            cwd: std::env::temp_dir(),
        }
    }

    /// A flow nobody watches must not survive its idle bound.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_abandoned_flow_times_out_and_is_killed() {
        let flows = Arc::new(TerminalAuthFlows::with_bounds(
            Duration::from_millis(50),
            MAX_FLOW_LIFETIME,
        ));
        let flow = flows
            .start("demo", "tui", &waiting_command(), None)
            .unwrap();
        let state = tokio::time::timeout(Duration::from_secs(20), flow.wait_finished())
            .await
            .expect("the abandoned flow never ended");
        assert_eq!(state, TerminalFlowState::TimedOut);
        assert!(flow.view().reason.is_some());
    }

    /// A flow must not outlive its overall bound, even with a client attached.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_watched_flow_still_reaches_its_lifetime_bound() {
        let flows = Arc::new(TerminalAuthFlows::with_bounds(
            IDLE_TIMEOUT,
            Duration::from_millis(200),
        ));
        let flow = flows
            .start("demo", "tui", &waiting_command(), None)
            .unwrap();
        let _attachment = flow.attach().await;
        let state = tokio::time::timeout(Duration::from_secs(20), flow.wait_finished())
            .await
            .expect("the watched flow never reached its time limit");
        assert_eq!(state, TerminalFlowState::TimedOut);
    }

    /// The global bound holds even across agents.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_global_flow_bound_holds_across_agents() {
        let flows = Arc::new(TerminalAuthFlows::new());
        let mut started = Vec::new();
        for index in 0..MAX_ACTIVE_FLOWS {
            started.push(
                flows
                    .start(&format!("agent-{index}"), "tui", &waiting_command(), None)
                    .expect("a flow inside the bound was refused"),
            );
        }
        let refused = flows
            .start("agent-extra", "tui", &waiting_command(), None)
            .err()
            .expect("the global bound did not hold");
        assert!(refused.to_string().contains("Too many"));
        flows.shutdown_all();
    }

    #[test]
    fn flow_ids_are_long_and_unique() {
        let first = new_flow_id();
        let second = new_flow_id();
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn only_running_is_unfinished() {
        assert!(!TerminalFlowState::Running.is_finished());
        for state in [
            TerminalFlowState::Succeeded,
            TerminalFlowState::Failed,
            TerminalFlowState::Cancelled,
            TerminalFlowState::TimedOut,
        ] {
            assert!(state.is_finished(), "{state:?} should be finished");
        }
    }

    /// A status poller must never see `succeeded` before the success hook
    /// ends. `GET /api/agent-auth/:flowId` reads the status view under the
    /// same lock the transition takes, so the transition itself must be the
    /// publication barrier.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_status_view_publishes_succeeded_only_after_the_hook() {
        use std::sync::atomic::AtomicBool;

        let hook_started = Arc::new(AtomicBool::new(false));
        let hook_done = Arc::new(AtomicBool::new(false));
        let hook_count = Arc::new(AtomicUsize::new(0));
        let started = hook_started.clone();
        let done = hook_done.clone();
        let count = hook_count.clone();
        let on_success: SuccessHook = Arc::new(move || {
            started.store(true, Ordering::SeqCst);
            count.fetch_add(1, Ordering::SeqCst);
            // Hold the transition open long enough for a poller to race it.
            std::thread::sleep(Duration::from_millis(100));
            done.store(true, Ordering::SeqCst);
        });

        let flows = Arc::new(TerminalAuthFlows::new());
        let flow = flows
            .start("demo", "tui", &waiting_command(), Some(on_success))
            .unwrap();

        let poller_done = hook_done.clone();
        let poller_flow = flow.clone();
        let poller = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                assert!(
                    Instant::now() < deadline,
                    "the status view never showed a finished state"
                );
                let state = poller_flow.view().state;
                if state == TerminalFlowState::Succeeded {
                    assert!(
                        poller_done.load(Ordering::SeqCst),
                        "a status poller observed `succeeded` before the success hook ended"
                    );
                    return true;
                }
                assert!(!state.is_finished(), "the flow ended in {state:?}");
                std::thread::sleep(Duration::from_millis(1));
            }
        });

        flow.finish(TerminalFlowState::Succeeded, Some(0), None);
        let observed = poller.join().unwrap();
        assert!(observed, "the poller never observed `succeeded`");
        assert!(hook_started.load(Ordering::SeqCst));
        assert!(hook_done.load(Ordering::SeqCst));
        assert_eq!(hook_count.load(Ordering::SeqCst), 1);
    }
}
