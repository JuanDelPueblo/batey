//! The agent-level authentication coordinator.
//!
//! Authentication belongs to an installed agent, not to a chat. A user must
//! be able to log an agent in before any chat exists, and a login must not
//! disturb a chat that is already running a turn. This coordinator therefore
//! starts its own short-lived ACP processes and never touches a chat session.
//!
//! Every process it starts uses the installed catalog runtime, a Batey-owned
//! working directory, and the sanitized per-agent environment. It never reads
//! a project `.envrc`, because an agent-level login has no project.
//!
//! A plain read of one agent's authentication state never starts a process:
//! it answers from the durable discovery cache in `crate::store`. Only the
//! explicit refresh operation, and an actual authentication or session
//! lifecycle event, may start an ACP process to update that cache.
use super::flow::{
    SuccessHook, TerminalAuthFlow, TerminalAuthFlowView, TerminalAuthFlows, TerminalFlowState,
};
use super::protocol::{
    ProtocolAuthFlow, ProtocolAuthFlowView, ProtocolAuthFlows, ProtocolFlowState,
};
use super::pty::{PtyCommand, TERMINAL_AUTH_SUPPORTED};
use crate::acp::auth::{
    AgentAuthState as LiveAuthState, AuthMethodKind, LegacyTerminalAuth, ObservedAuthState,
    TerminalAuthMethod,
};
use crate::acp::{AcpClient, StderrPolicy};
use crate::agents::{AgentCatalog, AgentRuntime};
use crate::events::EventLog;
use crate::session::SessionManager;
use crate::store::{AuthCacheData, AuthCacheEntry, CachedAuthMethod};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// How long one probe may take, including process start and `initialize`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(90);
/// The private event log of the probe processes. Nothing subscribes to it, so
/// no authentication traffic can reach a durable chat event.
const PROBE_EVENT_CAPACITY: usize = 64;
/// How long a cache entry stays labeled `cached` with no mutation before its
/// age alone moves the label to `stale`. This never spawns a probe by
/// itself; it only changes the freshness label shown to a client.
const AUTH_CACHE_STALE_AFTER_HOURS: i64 = 24;

/// Provider-neutral reason for a protocol-authentication timeout.
///
/// It never names a provider or guesses why the agent did not answer. It
/// states what Batey observed and why the method may need a browser or an
/// interactive environment that the agent did not expose through ACP.
pub const PROTOCOL_TIMEOUT_REASON: &str = "The agent did not complete authentication before the timeout. This method may require a browser or interactive environment that the agent did not expose through ACP.";

/// A failure of an authentication operation, in transport-neutral terms.
#[derive(Debug)]
pub enum AgentAuthError {
    NotFound(String),
    Invalid(String),
    Conflict(String),
    Unavailable(String),
    Internal(anyhow::Error),
}

impl std::fmt::Display for AgentAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(m) | Self::Invalid(m) | Self::Conflict(m) | Self::Unavailable(m) => {
                f.write_str(m)
            }
            Self::Internal(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AgentAuthError {}

type AuthResult<T> = std::result::Result<T, AgentAuthError>;

/// One advertised authentication method, as clients see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuthMethodView {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// The advertised method type, including a type this build cannot run.
    #[serde(rename = "type")]
    pub method_type: String,
    /// Whether this build can run the method.
    pub supported: bool,
    /// Scoped headless compatibility warning for this method, when the
    /// dedicated compatibility layer provides one. Never agent-wide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// Safe backend representation of one active unfinished auth flow.
///
/// Contains only flow id, kind, method id, lifecycle state, and start time.
/// Never PTY contents, credentials, tokens, device codes, sensitive URLs,
/// or other private buffered auth material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActiveAuthFlowView {
    pub flow_id: String,
    /// Either `protocol` or `terminal`.
    pub kind: String,
    pub method_id: String,
    /// Lifecycle state such as `running` or `waiting_for_user`.
    pub state: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

/// How current the data in one `AgentAuthView` is.
///
/// `Unknown` means this agent was never probed: there is no cache entry at
/// all. `Fresh` means this exact response is the direct result of a live
/// probe that just completed in this call. `Cached` means the response came
/// from the durable cache and nothing has invalidated it. `Stale` means the
/// cache is either explicitly invalidated by a mutation or old enough that
/// it should no longer be trusted without a fresh check; the data is still
/// returned as historical evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthFreshness {
    Unknown,
    Fresh,
    Cached,
    Stale,
}

/// The authentication state of one installed agent.
///
/// `logout_supported` is a capability only. `observed_state` is the
/// provider-neutral evidence Batey actually saw: `unknown` on a fresh
/// process, `authentication_required` after a stable `auth_required` or a
/// successful logout, and `authenticated` after a successful supported flow.
/// It is durable: it survives restart, but `freshness` tells a client
/// whether it is a live result or long-standing historical evidence, so an
/// `authenticated` value is never shown as timeless truth.
///
/// `unknown` is an internal absence-of-evidence state, never a user-visible
/// status. Clients show available methods normally and make no signed-in or
/// signed-out claim.
///
/// `active_flow` carries the safe discovery summary of one unfinished flow
/// for this agent, when one exists, so navigation or reload can resume or
/// cancel it instead of stranding it behind a conflict error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentAuthView {
    pub agent_id: String,
    pub methods: Vec<AuthMethodView>,
    /// Whether the agent advertised the stable logout capability.
    pub logout_supported: bool,
    /// Whether this build runs terminal authentication at all.
    pub terminal_supported: bool,
    pub observed_state: ObservedAuthState,
    pub freshness: AuthFreshness,
    /// When this data was last confirmed by a live probe or by explicit
    /// evidence. `None` when this agent was never probed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Safe active-flow discovery, when an unfinished flow exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_flow: Option<ActiveAuthFlowView>,
}

impl AgentAuthView {
    /// Builds a view from the durable cache. `just_probed` is true only when
    /// this exact call completed a live probe: it is the sole source of
    /// `AuthFreshness::Fresh`, so a plain cache read can never claim it.
    fn build(
        agent_id: &str,
        entry: Option<&AuthCacheEntry>,
        just_probed: bool,
        registry_id: Option<&str>,
        active_flow: Option<ActiveAuthFlowView>,
    ) -> Self {
        let data = entry.map(|entry| &entry.data);
        Self {
            agent_id: agent_id.to_owned(),
            methods: data
                .map(|data| {
                    data.methods
                        .iter()
                        .map(|method| AuthMethodView {
                            id: method.id.clone(),
                            name: method.name.clone(),
                            description: method.description.clone(),
                            method_type: method.method_type.clone(),
                            supported: method.supported,
                            warning: crate::agents::method_warning(
                                registry_id,
                                &method.id,
                                &method.method_type,
                            ),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            logout_supported: data.is_some_and(|data| data.logout_supported),
            terminal_supported: TERMINAL_AUTH_SUPPORTED,
            observed_state: data
                .map(|data| data.observed_state)
                .unwrap_or(ObservedAuthState::Unknown),
            freshness: freshness_of(entry, just_probed),
            checked_at: entry.and_then(|entry| parse_checked_at(&entry.checked_at)),
            active_flow,
        }
    }
}

/// The response of an explicit refresh.
///
/// `refresh_error` is set only when the probe itself failed. The cache still
/// answers with the best data it has: a failed check never erases a useful
/// cache and never makes the agent look unusable.
#[derive(Debug, Clone, Serialize)]
pub struct AgentAuthRefreshView {
    #[serde(flatten)]
    pub auth: AgentAuthView,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_error: Option<String>,
}

fn parse_checked_at(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

fn freshness_of(entry: Option<&AuthCacheEntry>, just_probed: bool) -> AuthFreshness {
    let Some(entry) = entry else {
        return AuthFreshness::Unknown;
    };
    if just_probed {
        return AuthFreshness::Fresh;
    }
    if entry.stale {
        return AuthFreshness::Stale;
    }
    match parse_checked_at(&entry.checked_at) {
        Some(checked_at) => {
            let age = chrono::Utc::now().signed_duration_since(checked_at);
            if age > chrono::Duration::hours(AUTH_CACHE_STALE_AFTER_HOURS) {
                AuthFreshness::Stale
            } else {
                AuthFreshness::Cached
            }
        }
        None => AuthFreshness::Stale,
    }
}

/// One request-scoped elicitation pending on a protocol flow.
///
/// Only the display fields travel to the browser. Form values and URL
/// secrets never enter durable storage; the browser answers through the
/// flow-scoped respond endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct ProtocolElicitationView {
    pub id: String,
    pub mode: String,
    pub message: String,
    pub schema: Option<serde_json::Value>,
    pub url: Option<String>,
    pub elicitation_id: Option<String>,
    pub tool_call_id: Option<String>,
}

impl From<crate::acp::callbacks::PendingElicitationInfo> for ProtocolElicitationView {
    fn from(info: crate::acp::callbacks::PendingElicitationInfo) -> Self {
        Self {
            id: info.id,
            mode: info.mode,
            message: info.message,
            schema: info.schema,
            url: info.url,
            elicitation_id: info.elicitation_id,
            tool_call_id: info.tool_call_id,
        }
    }
}

pub struct AgentAuthService {
    agents: Arc<AgentCatalog>,
    sessions: Arc<SessionManager>,
    /// The working directory every authentication process runs in. It belongs
    /// to Batey, so no browser path and no project workspace is involved.
    work_dir: PathBuf,
    flows: Arc<TerminalAuthFlows>,
    protocol_flows: Arc<ProtocolAuthFlows>,
    /// Per-agent locks for the explicit refresh operation, so repeated
    /// clicks or concurrent requests for one agent coalesce into a single
    /// probe. Never a global lock: refreshing one agent never waits on
    /// another agent's probe.
    refresh_locks: RwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// When one agent's cache was last updated by a completed probe. A
    /// refresh call reads this after it acquires the per-agent lock, so it
    /// can tell whether a concurrent call already did the work while it
    /// waited, instead of starting a second process.
    last_probed_at: RwLock<HashMap<String, Instant>>,
    /// When one agent's cache was last invalidated by a mutation. A probe
    /// that started before this instant may carry pre-change data, so its
    /// result must not resurrect what the invalidation exists to forget.
    invalidated_at: RwLock<HashMap<String, Instant>>,
    /// A private in-memory log for the probe processes.
    events: Arc<EventLog>,
    /// A tracker separate from the chat task tracker, so an authentication
    /// process never appears among a chat's terminal tasks.
    tasks: Arc<crate::tasks::TerminalTaskTracker>,
}

impl std::fmt::Debug for AgentAuthService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentAuthService")
            .field("work_dir", &self.work_dir)
            .finish_non_exhaustive()
    }
}

impl AgentAuthService {
    pub fn new(
        agents: Arc<AgentCatalog>,
        sessions: Arc<SessionManager>,
        work_dir: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            agents,
            sessions,
            work_dir,
            flows: Arc::new(TerminalAuthFlows::new()),
            protocol_flows: Arc::new(ProtocolAuthFlows::new()),
            refresh_locks: RwLock::new(HashMap::new()),
            last_probed_at: RwLock::new(HashMap::new()),
            invalidated_at: RwLock::new(HashMap::new()),
            events: Arc::new(EventLog::new(PROBE_EVENT_CAPACITY)),
            tasks: Arc::new(crate::tasks::TerminalTaskTracker::default()),
        })
    }

    /// The authentication state of one agent, from the durable cache only.
    /// This never starts a process: an unknown agent is a plain lookup
    /// failure, and a known-but-never-probed agent truthfully reports
    /// `AuthFreshness::Unknown` with no methods, rather than guessing.
    pub async fn auth_view(&self, agent_id: &str) -> AuthResult<AgentAuthView> {
        if !self.agents.contains(agent_id) {
            return Err(AgentAuthError::NotFound(format!(
                "Unknown agent '{agent_id}'"
            )));
        }
        let entry = self.cache_entry(agent_id);
        let registry_id = self.registry_id_for(agent_id);
        let active_flow = self.active_flow_for(agent_id);
        Ok(AgentAuthView::build(
            agent_id,
            entry.as_ref(),
            false,
            registry_id.as_deref(),
            active_flow,
        ))
    }

    /// Explicit refresh: the only read path, besides an actual
    /// authentication or session lifecycle event, that may start an ACP
    /// process. Single-flight per agent: concurrent calls for the same
    /// agent coalesce behind one probe instead of spawning duplicates.
    ///
    /// A probe failure never erases the existing cache and never makes the
    /// agent look unavailable: the last known data comes back, alongside
    /// `refresh_error`.
    pub async fn refresh(self: &Arc<Self>, agent_id: &str) -> AuthResult<AgentAuthRefreshView> {
        if !self.agents.contains(agent_id) {
            return Err(AgentAuthError::NotFound(format!(
                "Unknown agent '{agent_id}'"
            )));
        }
        let requested_at = Instant::now();
        let lock = self.refresh_lock_for(agent_id);
        let _guard = lock.lock().await;

        // Another concurrent refresh may have already probed while this
        // call waited for the lock. Reuse that result instead of starting a
        // second process.
        let already_fresh = self
            .last_probed_at
            .read()
            .expect("agent auth probe-time lock poisoned")
            .get(agent_id)
            .is_some_and(|probed_at| *probed_at >= requested_at);

        let refresh_error = if already_fresh {
            None
        } else {
            self.probe(agent_id)
                .await
                .err()
                .map(|error| error.to_string())
        };

        let registry_id = self.registry_id_for(agent_id);
        let active_flow = self.active_flow_for(agent_id);
        let entry = self.cache_entry(agent_id);
        let auth = AgentAuthView::build(
            agent_id,
            entry.as_ref(),
            refresh_error.is_none(),
            registry_id.as_deref(),
            active_flow,
        );
        Ok(AgentAuthRefreshView {
            auth,
            refresh_error,
        })
    }

    fn refresh_lock_for(&self, agent_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        if let Some(lock) = self
            .refresh_locks
            .read()
            .expect("agent auth refresh-lock lock poisoned")
            .get(agent_id)
        {
            return lock.clone();
        }
        self.refresh_locks
            .write()
            .expect("agent auth refresh-lock lock poisoned")
            .entry(agent_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// The official Registry id behind one catalog id, when the agent is a
    /// Registry install. Compatibility defaults match on this, never on a
    /// Batey catalog id or an agent name.
    fn registry_id_for(&self, agent_id: &str) -> Option<String> {
        let store = self.sessions.store.as_ref()?;
        match store.installed_agent(agent_id) {
            Ok(Some(record)) => record.registry.map(|snapshot| snapshot.registry_id),
            _ => None,
        }
    }

    /// Safe discovery summary of one unfinished flow for this agent, when one
    /// exists. Covers both protocol and terminal flows. Contains only flow
    /// id, kind, method id, lifecycle state, and start time.
    pub fn active_flow_for(&self, agent_id: &str) -> Option<ActiveAuthFlowView> {
        let protocol = self.protocol_flows.active_for_agent(agent_id).map(|flow| {
            let view = flow.view();
            ActiveAuthFlowView {
                flow_id: view.flow_id,
                kind: "protocol".to_string(),
                method_id: view.method_id,
                state: view.state.as_str().to_string(),
                started_at: view.started_at,
            }
        });
        let terminal = self.flows.active_for_agent(agent_id).map(|flow| {
            let view = flow.view();
            ActiveAuthFlowView {
                flow_id: view.flow_id,
                kind: "terminal".to_string(),
                method_id: view.method_id,
                state: match view.state {
                    super::flow::TerminalFlowState::Running => "running".to_string(),
                    super::flow::TerminalFlowState::Succeeded => "succeeded".to_string(),
                    super::flow::TerminalFlowState::Failed => "failed".to_string(),
                    super::flow::TerminalFlowState::Cancelled => "cancelled".to_string(),
                    super::flow::TerminalFlowState::TimedOut => "timed_out".to_string(),
                },
                started_at: view.started_at,
            }
        });
        match (protocol, terminal) {
            (Some(p), Some(t)) => {
                if t.started_at > p.started_at {
                    Some(t)
                } else {
                    Some(p)
                }
            }
            (Some(p), None) => Some(p),
            (None, Some(t)) => Some(t),
            (None, None) => None,
        }
    }

    /// The durable observed state. `Unknown` when this agent was never
    /// probed and has no recorded evidence.
    fn observed_state(&self, agent_id: &str) -> ObservedAuthState {
        self.cache_entry(agent_id)
            .map(|entry| entry.data.observed_state)
            .unwrap_or(ObservedAuthState::Unknown)
    }

    /// Records that Batey saw a stable `auth_required` for this agent.
    pub fn note_auth_required(&self, agent_id: &str) {
        self.cache_write_observed(agent_id, ObservedAuthState::AuthenticationRequired);
    }

    /// Records a successful supported authentication flow.
    pub fn note_authenticated(&self, agent_id: &str) {
        self.cache_write_observed(agent_id, ObservedAuthState::Authenticated);
    }

    /// Reinforces `authenticated` after a session setup succeeded.
    ///
    /// Only moves `AuthenticationRequired` forward. `Unknown` stays
    /// `Unknown`: a session that never needed auth is not evidence of a
    /// login. `Authenticated` stays as it is.
    pub fn note_session_success(&self, agent_id: &str) {
        if self.observed_state(agent_id) == ObservedAuthState::AuthenticationRequired {
            self.cache_write_observed(agent_id, ObservedAuthState::Authenticated);
        }
    }

    /// Runs the stable `authenticate` method for one advertised `agent`
    /// method, then reads the authoritative state again.
    ///
    /// A success records observed `authenticated`. It never claims success
    /// it did not get: an agent rejection stays an error and the observed
    /// state is unchanged.
    pub async fn authenticate(&self, agent_id: &str, method_id: &str) -> AuthResult<AgentAuthView> {
        let probe_started_at = Instant::now();
        let client = self.connect(agent_id).await?;
        let state = client.auth_state().await;
        // Discovery data from a live initialize is safe to cache regardless
        // of whether the authenticate call below succeeds.
        self.cache_write_full(agent_id, &state, probe_started_at);
        let method = state
            .method(method_id)
            .ok_or_else(|| {
                AgentAuthError::NotFound(format!(
                    "Agent '{agent_id}' does not advertise the authentication method '{method_id}'"
                ))
            })
            .cloned();
        let outcome = match method {
            Err(error) => Err(error),
            Ok(method) => match &method.kind {
                AuthMethodKind::Agent => client
                    .authenticate(method_id)
                    .await
                    .map(|_| ())
                    .map_err(|error| AgentAuthError::Invalid(error.to_string())),
                AuthMethodKind::Terminal(_) => Err(AgentAuthError::Invalid(format!(
                    "Authentication method '{method_id}' runs in a terminal. Start a terminal authentication flow instead."
                ))),
                AuthMethodKind::LegacyTerminal(_) => Err(AgentAuthError::Invalid(format!(
                    "Authentication method '{method_id}' runs its advertised login command in a terminal. Start a terminal authentication flow instead."
                ))),
                AuthMethodKind::Unsupported(kind) => Err(AgentAuthError::Invalid(format!(
                    "Authentication method '{method_id}' uses the unsupported type '{kind}'"
                ))),
            },
        };
        client.shutdown().await;
        outcome?;
        self.note_authenticated(agent_id);
        self.refresh_after_change(agent_id).await
    }

    /// Runs the capability-gated stable `logout` method, then reads the
    /// authoritative state again.
    ///
    /// Batey chats, sessions, and history are untouched. Logout only
    /// removes the credentials the agent itself holds. A success records
    /// observed `authentication_required`; the capability alone never
    /// implied `authenticated`.
    pub async fn logout(&self, agent_id: &str) -> AuthResult<AgentAuthView> {
        let probe_started_at = Instant::now();
        let client = self.connect(agent_id).await?;
        let state = client.auth_state().await;
        self.cache_write_full(agent_id, &state, probe_started_at);
        let supported = state.logout_supported;
        let outcome = if supported {
            client
                .logout()
                .await
                .map(|_| ())
                .map_err(|error| AgentAuthError::Invalid(error.to_string()))
        } else {
            Err(AgentAuthError::Conflict(format!(
                "Agent '{agent_id}' does not support logout"
            )))
        };
        client.shutdown().await;
        outcome?;
        self.note_auth_required(agent_id);
        self.refresh_after_change(agent_id).await
    }

    /// Starts a terminal authentication flow for one advertised `terminal`
    /// method or one legacy bridge method.
    ///
    /// A stable method reuses the installed runtime plus the advertised
    /// args/env. A legacy bridge runs the advertised command/args from the
    /// agent's `initialize` response. Nothing in either comes from the
    /// request. Both run in the same PTY lifecycle, never through a shell,
    /// and never reach `authenticate`. The exact advertised command/args are
    /// preserved; a bare relative legacy command that PATH cannot resolve
    /// may resolve inside this same Registry-managed agent's own validated
    /// install directory, never another agent's.
    pub async fn start_terminal(
        self: &Arc<Self>,
        agent_id: &str,
        method_id: &str,
    ) -> AuthResult<TerminalAuthFlowView> {
        if !TERMINAL_AUTH_SUPPORTED {
            return Err(AgentAuthError::Unavailable(
                "Terminal authentication is not supported on this platform".into(),
            ));
        }
        // One active flow per agent across both kinds, so recovery always
        // finds a single resumable flow instead of stranding one behind a
        // conflict error with no UI path.
        if self.protocol_flows.active_for_agent(agent_id).is_some() {
            return Err(AgentAuthError::Conflict(format!(
                "Agent '{agent_id}' already has an authentication flow running. Finish or cancel it first."
            )));
        }
        let runtime = self.runtime(agent_id)?;
        // Live ACP initialization is the authority before executing a
        // method: a cached method is for discovery/UI only.
        let state = self.probe(agent_id).await?;
        let method = state
            .method(method_id)
            .ok_or_else(|| {
                AgentAuthError::NotFound(format!(
                    "Agent '{agent_id}' does not advertise the authentication method '{method_id}'"
                ))
            })
            .cloned()?;

        let cwd = self.work_dir()?;
        let base_env = self.agent_env(&runtime, crate::agents::AuthEnvScope::Terminal);
        let install_dir = self.install_dir_for(agent_id);
        let command = match &method.kind {
            AuthMethodKind::Terminal(terminal) => {
                terminal_command(&runtime, terminal, &base_env, &cwd)
            }
            AuthMethodKind::LegacyTerminal(legacy) => {
                legacy_terminal_command_for_agent(
                    agent_id,
                    legacy,
                    &base_env,
                    &cwd,
                    install_dir.as_deref(),
                )
                .map_err(|error| {
                    AgentAuthError::Invalid(format!(
                        "Authentication method '{method_id}' carries an invalid legacy login command: {error}"
                    ))
                })?
            }
            AuthMethodKind::Agent => {
                return Err(AgentAuthError::Invalid(format!(
                    "Authentication method '{method_id}' is not a terminal method"
                )))
            }
            AuthMethodKind::Unsupported(kind) => {
                return Err(AgentAuthError::Invalid(format!(
                    "Authentication method '{method_id}' uses the unsupported type '{kind}'"
                )))
            }
        };
        // A successful terminal command changes the agent's stored
        // credentials. The hook records observed `authenticated` inside the
        // transition to `succeeded`, so a client that reacts to the success
        // always reads durable evidence at once, instead of waiting for the
        // async re-probe below to finish.
        let on_success: SuccessHook = {
            let service = Arc::downgrade(self);
            let agent_id = agent_id.to_owned();
            Arc::new(move || {
                if let Some(service) = service.upgrade() {
                    service.note_authenticated(&agent_id);
                }
            })
        };
        let flow = self
            .flows
            .start(agent_id, method_id, &command, Some(on_success))
            .map_err(|error| AgentAuthError::Conflict(error.to_string()))?;
        self.watch_terminal_flow(flow.clone());
        Ok(flow.view())
    }

    pub fn terminal_flow(&self, flow_id: &str) -> AuthResult<Arc<TerminalAuthFlow>> {
        self.flows
            .get(flow_id)
            .ok_or_else(|| AgentAuthError::NotFound("Authentication flow not found".into()))
    }

    pub fn terminal_flow_view(&self, flow_id: &str) -> AuthResult<TerminalAuthFlowView> {
        Ok(self.terminal_flow(flow_id)?.view())
    }

    pub fn cancel_terminal_flow(&self, flow_id: &str) -> AuthResult<TerminalAuthFlowView> {
        let flow = self.terminal_flow(flow_id)?;
        flow.cancel();
        Ok(flow.view())
    }

    /// Starts an asynchronous protocol authentication flow for one
    /// advertised `agent` method.
    ///
    /// The browser request returns at once with a running flow id. The ACP
    /// `authenticate` RPC runs in the background so a long device-code or
    /// URL step never ties up the request. Elicitations stay request-scoped
    /// on the flow and never reach durable chat events. Cancel stays
    /// available throughout the wait.
    pub async fn start_protocol(
        self: &Arc<Self>,
        agent_id: &str,
        method_id: &str,
    ) -> AuthResult<ProtocolAuthFlowView> {
        // One active flow per agent across both kinds, so a terminal flow
        // never hides behind a protocol conflict with no resume path.
        if self.flows.active_for_agent(agent_id).is_some() {
            return Err(AgentAuthError::Conflict(format!(
                "Agent '{agent_id}' already has an authentication flow running. Finish or cancel it first."
            )));
        }
        // Live ACP initialization is the authority before executing a
        // method: a cached method is for discovery/UI only.
        let state = self.probe(agent_id).await?;
        let method = state
            .method(method_id)
            .ok_or_else(|| {
                AgentAuthError::NotFound(format!(
                    "Agent '{agent_id}' does not advertise the authentication method '{method_id}'"
                ))
            })
            .cloned()?;
        match &method.kind {
            AuthMethodKind::Agent => {}
            AuthMethodKind::Terminal(_) | AuthMethodKind::LegacyTerminal(_) => {
                return Err(AgentAuthError::Invalid(format!(
                    "Authentication method '{method_id}' runs in a terminal. Start a terminal authentication flow instead."
                )))
            }
            AuthMethodKind::Unsupported(kind) => {
                return Err(AgentAuthError::Invalid(format!(
                    "Authentication method '{method_id}' uses the unsupported type '{kind}'"
                )))
            }
        }
        let flow = self
            .protocol_flows
            .create(agent_id, method_id)
            .map_err(|error| AgentAuthError::Conflict(error.to_string()))?;
        self.drive_protocol_flow(flow.clone());
        Ok(flow.view())
    }

    pub fn protocol_flow(&self, flow_id: &str) -> AuthResult<Arc<ProtocolAuthFlow>> {
        self.protocol_flows
            .get(flow_id)
            .ok_or_else(|| AgentAuthError::NotFound("Authentication flow not found".into()))
    }

    pub async fn protocol_flow_view(&self, flow_id: &str) -> AuthResult<ProtocolAuthFlowView> {
        let flow = self.protocol_flow(flow_id)?;
        let mut view = flow.view();
        // Derive `waiting_for_user` while elicitations are pending so the
        // card never sits in a bare `running` with no way to act, even if
        // the driver poll has not ticked yet.
        if view.state == ProtocolFlowState::Running {
            if let Some(client) = flow.client().await {
                if !client
                    .callback_handler()
                    .list_pending_elicitations()
                    .await
                    .is_empty()
                {
                    view.state = ProtocolFlowState::WaitingForUser;
                }
            }
        }
        Ok(view)
    }

    pub async fn cancel_protocol_flow(&self, flow_id: &str) -> AuthResult<ProtocolAuthFlowView> {
        let flow = self.protocol_flow(flow_id)?;
        flow.cancel();
        if let Some(client) = flow.client().await {
            client
                .callback_handler()
                .cancel_pending_elicitations()
                .await;
            client.shutdown().await;
        }
        Ok(flow.view())
    }

    pub async fn protocol_elicitations(
        &self,
        flow_id: &str,
    ) -> AuthResult<Vec<ProtocolElicitationView>> {
        let flow = self.protocol_flow(flow_id)?;
        let Some(client) = flow.client().await else {
            return Ok(Vec::new());
        };
        Ok(client
            .callback_handler()
            .list_pending_elicitations()
            .await
            .into_iter()
            .map(ProtocolElicitationView::from)
            .collect())
    }

    pub async fn respond_protocol_elicitation(
        &self,
        flow_id: &str,
        elicitation_id: &str,
        action: &str,
        content: Option<serde_json::Value>,
    ) -> AuthResult<bool> {
        if !matches!(action, "accept" | "decline" | "cancel") {
            return Err(AgentAuthError::Invalid(
                "Elicitation action must be accept, decline, or cancel".into(),
            ));
        }
        let flow = self.protocol_flow(flow_id)?;
        let Some(client) = flow.client().await else {
            return Err(AgentAuthError::NotFound(
                "Authentication flow has no live authentication process".into(),
            ));
        };
        client
            .callback_handler()
            .respond_elicitation(elicitation_id, action, content)
            .await
            .map_err(|error| AgentAuthError::Invalid(error.to_string()))
            .map(|_| true)
    }

    /// Runs the background `authenticate` for one protocol flow.
    fn drive_protocol_flow(self: &Arc<Self>, flow: Arc<ProtocolAuthFlow>) {
        let service = self.clone();
        tokio::spawn(async move {
            let agent_id = flow.agent_id.clone();
            let method_id = flow.method_id.clone();
            let deadline = tokio::time::Instant::now() + service.protocol_flows.max_lifetime();
            // Connect first; a connect failure ends the flow as failed.
            let client = match service.connect_for_protocol(&agent_id, &flow.id).await {
                Ok(client) => {
                    let client = Arc::new(client);
                    flow.set_client(client.clone()).await;
                    client
                }
                Err(error) => {
                    flow.finish(ProtocolFlowState::Failed, Some(error.to_string()));
                    return;
                }
            };
            // The authenticate future owns its client clone so shutdown below
            // cannot drop it mid-request.
            let auth_client = client.clone();
            let auth_method = method_id.clone();
            let mut auth_fut = Box::pin(auth_client.authenticate(&auth_method));
            let poll = super::protocol::ProtocolAuthFlows::poll_interval();
            loop {
                tokio::select! {
                    outcome = &mut auth_fut => {
                        match outcome {
                            Ok(_) => {
                                service.note_authenticated(&agent_id);
                                flow.finish(ProtocolFlowState::Succeeded, None);
                                client.shutdown().await;
                                // Refresh stopped sessions and probe fresh
                                // state, like a terminal success does.
                                if let Err(error) = service.refresh_after_change(&agent_id).await {
                                    tracing::warn!(
                                        agent = %agent_id,
                                        %error,
                                        "Could not read the agent authentication state after protocol authentication"
                                    );
                                }
                            }
                            Err(error) => {
                                // A cancel that raced success never overwrites:
                                // `finish` keeps the first outcome.
                                if flow.state().is_finished() {
                                    client.shutdown().await;
                                } else {
                                    let message = error.to_string();
                                    flow.finish(ProtocolFlowState::Failed, Some(message));
                                    client.shutdown().await;
                                }
                            }
                        }
                        return;
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        flow.finish(
                            ProtocolFlowState::TimedOut,
                            Some(PROTOCOL_TIMEOUT_REASON.into()),
                        );
                        client.callback_handler().cancel_pending_elicitations().await;
                        client.shutdown().await;
                        return;
                    }
                    _ = tokio::time::sleep(poll) => {
                        if flow.state().is_finished() {
                            return;
                        }
                        // Surface request-scoped elicitations as an explicit
                        // waiting state so the UI can offer accept/decline/
                        // cancel instead of spinning forever.
                        let pending = client.callback_handler().list_pending_elicitations().await;
                        // Use the internal hook: waiting only while running.
                        // The view also derives this, so a missed poll still shows.
                        if pending.is_empty() {
                            // Back to running when the user answered.
                            // `note_waiting(false)` only moves Waiting->Running.
                            flow.note_waiting(false);
                        } else {
                            flow.note_waiting(true);
                        }
                    }
                }
            }
        });
    }

    /// Starts one agent process for a protocol flow, with a flow-scoped
    /// session id so its elicitations never mix with chat elicitations.
    async fn connect_for_protocol(&self, agent_id: &str, flow_id: &str) -> AuthResult<AcpClient> {
        let runtime = self.runtime(agent_id)?;
        let cwd = self.work_dir()?;
        let env = self.agent_env(&runtime, crate::agents::AuthEnvScope::Auth);
        let client = tokio::time::timeout(
            PROBE_TIMEOUT,
            AcpClient::spawn(
                &runtime.launch.command,
                &runtime.launch.args,
                &env,
                &cwd,
                format!("protocol-auth:{flow_id}"),
                agent_id.to_owned(),
                self.events.clone(),
                None,
                self.tasks.clone(),
                vec![cwd.clone()],
                StderrPolicy::Discard,
            ),
        )
        .await
        .map_err(|_| {
            AgentAuthError::Unavailable(format!("Agent '{agent_id}' did not start in time"))
        })?
        .map_err(|error| AgentAuthError::Unavailable(error.to_string()))?;

        match tokio::time::timeout(PROBE_TIMEOUT, client.initialize(&cwd)).await {
            Ok(Ok(_)) => Ok(client),
            Ok(Err(error)) => {
                client.shutdown().await;
                Err(AgentAuthError::Unavailable(format!(
                    "Agent '{agent_id}' could not report its authentication methods: {error}"
                )))
            }
            Err(_) => {
                client.shutdown().await;
                Err(AgentAuthError::Unavailable(format!(
                    "Agent '{agent_id}' did not answer initialize in time"
                )))
            }
        }
    }

    /// Ends every flow and kills every process tree.
    pub fn shutdown(&self) {
        self.flows.shutdown_all();
        self.protocol_flows.shutdown_all();
    }

    /// Reads the authoritative state again after an authentication change,
    /// and lets stopped sessions pick up the new credentials.
    async fn refresh_after_change(&self, agent_id: &str) -> AuthResult<AgentAuthView> {
        // A stopped session starts again from the catalog, so retiring it
        // makes the next start observe the new credentials. A starting or
        // running session keeps its process: an active turn must survive an
        // authentication change.
        self.sessions
            .invalidate_stopped_sessions_for_agent(agent_id)
            .await;
        self.probe(agent_id).await?;
        let registry_id = self.registry_id_for(agent_id);
        let active_flow = self.active_flow_for(agent_id);
        let entry = self.cache_entry(agent_id);
        Ok(AgentAuthView::build(
            agent_id,
            entry.as_ref(),
            true,
            registry_id.as_deref(),
            active_flow,
        ))
    }

    /// Refreshes the agent state once a terminal flow succeeds.
    ///
    /// A successful terminal command means the agent stored its own
    /// credentials. The stable protocol forbids `authenticate` for that
    /// method, so Batey starts the agent again and reads `initialize`.
    /// The observed state was already set inside the success transition.
    fn watch_terminal_flow(self: &Arc<Self>, flow: Arc<TerminalAuthFlow>) {
        let service = self.clone();
        tokio::spawn(async move {
            if flow.wait_finished().await != TerminalFlowState::Succeeded {
                return;
            }
            if let Err(error) = service.refresh_after_change(&flow.agent_id).await {
                tracing::warn!(
                    agent = %flow.agent_id,
                    %error,
                    "Could not read the agent authentication state after terminal authentication"
                );
            }
        });
    }

    /// Marks the durable discovery cache of one agent stale, without erasing
    /// its last known methods. A mutation that can affect initialization or
    /// authentication methods calls this: environment overrides, updates,
    /// reinstalls, and custom-definition edits. The last known data stays
    /// available as historical evidence and invites an explicit refresh.
    pub fn invalidate_agent(&self, agent_id: &str) {
        self.invalidated_at
            .write()
            .expect("agent auth invalidation lock poisoned")
            .insert(agent_id.to_owned(), Instant::now());
        let Some(store) = self.sessions.store.as_ref() else {
            return;
        };
        if let Err(error) = store.mark_agent_auth_cache_stale(agent_id) {
            tracing::warn!(
                agent = agent_id,
                %error,
                "Could not mark the agent authentication discovery cache stale"
            );
        }
    }

    /// Deletes the durable discovery cache row outright. Only a full agent
    /// removal calls this; a retired agent keeps its row, marked stale, as
    /// historical evidence for the chats that still reference it.
    pub fn forget_agent(&self, agent_id: &str) {
        self.invalidated_at
            .write()
            .expect("agent auth invalidation lock poisoned")
            .insert(agent_id.to_owned(), Instant::now());
        let Some(store) = self.sessions.store.as_ref() else {
            return;
        };
        if let Err(error) = store.delete_agent_auth_cache(agent_id) {
            tracing::warn!(
                agent = agent_id,
                %error,
                "Could not delete the agent authentication discovery cache"
            );
        }
    }

    /// The durable cache entry of one agent, if any. Never spawns a process.
    fn cache_entry(&self, agent_id: &str) -> Option<AuthCacheEntry> {
        let store = self.sessions.store.as_ref()?;
        match store.agent_auth_cache(agent_id) {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(
                    agent = agent_id,
                    %error,
                    "Could not read the agent authentication discovery cache"
                );
                None
            }
        }
    }

    /// Writes fresh discovery data (methods, logout capability) from a
    /// completed live probe. Never overwrites `observed_state`: a probe of
    /// the advertised methods alone is not authentication evidence, so the
    /// previous observed state carries forward untouched.
    fn cache_write_full(&self, agent_id: &str, state: &LiveAuthState, probe_started_at: Instant) {
        let Some(store) = self.sessions.store.as_ref() else {
            return;
        };
        // A probe that started before the last invalidation may carry the
        // state an invalidation exists to forget. Skip the write, so the
        // mutation's stale marker stays in place until a later probe.
        let superseded = self
            .invalidated_at
            .read()
            .expect("agent auth invalidation lock poisoned")
            .get(agent_id)
            .is_some_and(|invalidated_at| probe_started_at <= *invalidated_at);
        if superseded {
            return;
        }
        let previous_observed = self
            .cache_entry(agent_id)
            .map(|entry| entry.data.observed_state)
            .unwrap_or(ObservedAuthState::Unknown);
        let data = AuthCacheData {
            methods: state
                .methods
                .iter()
                .map(|method| CachedAuthMethod {
                    id: method.id.clone(),
                    name: method.name.clone(),
                    description: method.description.clone(),
                    method_type: method.type_name().to_owned(),
                    supported: method.is_supported(TERMINAL_AUTH_SUPPORTED),
                })
                .collect(),
            logout_supported: state.logout_supported,
            observed_state: previous_observed,
        };
        let entry = AuthCacheEntry::fresh(agent_id, data, chrono::Utc::now().to_rfc3339());
        if let Err(error) = store.save_agent_auth_cache(&entry) {
            tracing::warn!(
                agent = agent_id,
                %error,
                "Could not save the agent authentication discovery cache"
            );
        }
        self.last_probed_at
            .write()
            .expect("agent auth probe-time lock poisoned")
            .insert(agent_id.to_owned(), Instant::now());
    }

    /// Records new observed-state evidence, preserving whatever methods and
    /// logout capability are already cached. Creates a minimal entry when
    /// none exists yet, so evidence from a chat session is never lost just
    /// because this agent has never been explicitly probed.
    fn cache_write_observed(&self, agent_id: &str, observed: ObservedAuthState) {
        let Some(store) = self.sessions.store.as_ref() else {
            return;
        };
        let mut entry = self.cache_entry(agent_id).unwrap_or_else(|| {
            AuthCacheEntry::fresh(agent_id, AuthCacheData::default(), String::new())
        });
        entry.data.observed_state = observed;
        entry.checked_at = chrono::Utc::now().to_rfc3339();
        entry.stale = false;
        if let Err(error) = store.save_agent_auth_cache(&entry) {
            tracing::warn!(
                agent = agent_id,
                %error,
                "Could not save the agent authentication discovery cache"
            );
        }
    }

    /// Runs one live probe: spawn, `initialize`, read the auth state, shut
    /// down. Writes the discovery cache with the result. Only the explicit
    /// refresh operation and an authentication/session lifecycle event call
    /// this; a plain read never does.
    async fn probe(&self, agent_id: &str) -> AuthResult<LiveAuthState> {
        let started_at = Instant::now();
        let client = self.connect(agent_id).await?;
        let state = client.auth_state().await;
        client.shutdown().await;
        self.cache_write_full(agent_id, &state, started_at);
        Ok(state)
    }

    /// Starts one agent process and completes `initialize`.
    ///
    /// The caller owns the returned client and must shut it down. The process
    /// never opens a session, so it can never run a turn.
    async fn connect(&self, agent_id: &str) -> AuthResult<AcpClient> {
        let runtime = self.runtime(agent_id)?;
        let cwd = self.work_dir()?;
        let env = self.agent_env(&runtime, crate::agents::AuthEnvScope::Auth);
        let client = tokio::time::timeout(
            PROBE_TIMEOUT,
            AcpClient::spawn(
                &runtime.launch.command,
                &runtime.launch.args,
                &env,
                &cwd,
                format!("agent-auth:{agent_id}"),
                agent_id.to_owned(),
                self.events.clone(),
                None,
                self.tasks.clone(),
                vec![cwd.clone()],
                // Authentication helpers print device codes, URLs, and
                // tokens to stderr. Discard it all, so no credential line
                // ever reaches the log.
                StderrPolicy::Discard,
            ),
        )
        .await
        .map_err(|_| {
            AgentAuthError::Unavailable(format!("Agent '{agent_id}' did not start in time"))
        })?
        .map_err(|error| AgentAuthError::Unavailable(error.to_string()))?;

        match tokio::time::timeout(PROBE_TIMEOUT, client.initialize(&cwd)).await {
            Ok(Ok(_)) => Ok(client),
            Ok(Err(error)) => {
                client.shutdown().await;
                Err(AgentAuthError::Unavailable(format!(
                    "Agent '{agent_id}' could not report its authentication methods: {error}"
                )))
            }
            Err(_) => {
                client.shutdown().await;
                Err(AgentAuthError::Unavailable(format!(
                    "Agent '{agent_id}' did not answer initialize in time"
                )))
            }
        }
    }

    fn runtime(&self, agent_id: &str) -> AuthResult<Arc<AgentRuntime>> {
        if let Some(runtime) = self.agents.runtime(agent_id) {
            return Ok(runtime);
        }
        if self.agents.contains(agent_id) {
            return Err(AgentAuthError::Unavailable(format!(
                "Agent '{agent_id}' is not available"
            )));
        }
        Err(AgentAuthError::NotFound(format!(
            "Unknown agent '{agent_id}'"
        )))
    }

    /// The sanitized environment one authentication process starts with.
    ///
    /// The base is the Batey process environment, which startup already
    /// emptied of every stashed secret. The resolver scrubs the stashed names
    /// again, injects only the names this agent's `pass_env` lists, and then
    /// applies compatibility defaults plus this agent's private overrides, so
    /// one agent never observes another agent's value. Compatibility defaults
    /// lose to T131 overrides, and they are scoped: a Codex `NO_BROWSER`
    /// default reaches auth processes only, while a Copilot `CI` default
    /// reaches auth and terminal-auth processes. Terminal authentication
    /// later overlays the method-specific environment on top of this base.
    /// Ordinary chat sessions never pass through here, so headless defaults
    /// never leak into them.
    fn agent_env(
        &self,
        runtime: &AgentRuntime,
        scope: crate::agents::AuthEnvScope,
    ) -> HashMap<String, String> {
        let base: HashMap<String, String> = std::env::vars().collect();
        let overrides = self.agent_env_overrides(&runtime.id);
        let registry_id = self.registry_id_for(&runtime.id);
        let compat = crate::agents::auth_env_defaults(registry_id.as_deref(), scope);
        // Compatibility defaults behave as lower-precedence overrides: they
        // win over base/launch/pass_env but lose to explicit T131 values.
        let mut effective = compat;
        for (name, value) in overrides {
            effective.insert(name, value);
        }
        crate::workspace_env::resolve_agent_env_with_overrides(
            &base,
            &runtime.launch.env,
            &runtime.launch.pass_env,
            &self.sessions.secret_env(),
            &effective,
        )
    }

    /// Private per-agent overrides for one agent id. A store failure leaves
    /// the process without overrides rather than without authentication, and
    /// values never reach logs or errors.
    fn agent_env_overrides(&self, agent_id: &str) -> HashMap<String, String> {
        match self.sessions.store.as_ref() {
            Some(store) => match store.agent_env(agent_id) {
                Ok(values) => values.into_iter().collect(),
                Err(error) => {
                    tracing::warn!(
                        agent = agent_id,
                        %error,
                        "Could not read agent environment overrides; continuing without them"
                    );
                    HashMap::new()
                }
            },
            None => HashMap::new(),
        }
    }

    fn work_dir(&self) -> AuthResult<PathBuf> {
        std::fs::create_dir_all(&self.work_dir).map_err(|error| {
            AgentAuthError::Internal(anyhow::anyhow!(
                "Could not create the authentication working directory {}: {error}",
                self.work_dir.display()
            ))
        })?;
        Ok(self.work_dir.clone())
    }

    /// The validated Batey-managed install directory of one Registry install,
    /// when it has one. Only a binary distribution records an install
    /// directory. Resolution never searches another agent's directory.
    fn install_dir_for(&self, agent_id: &str) -> Option<PathBuf> {
        let store = self.sessions.store.as_ref()?;
        let record = store.installed_agent(agent_id).ok().flatten()?;
        let snapshot = record.registry?;
        let install_dir = snapshot.install_dir?;
        Some(PathBuf::from(install_dir))
    }
}

/// Builds the exact terminal authentication invocation.
///
/// The stable rule is precise, so this stays one small pure function a test
/// can check:
///
/// - the base executable is unchanged;
/// - the base arguments are unchanged;
/// - the method arguments follow them, in advertised order;
/// - the environment is the same sanitized base environment;
/// - the method environment overrides the same names in that base.
pub fn terminal_command(
    runtime: &AgentRuntime,
    method: &TerminalAuthMethod,
    base_env: &HashMap<String, String>,
    cwd: &Path,
) -> PtyCommand {
    let mut args = runtime.launch.args.clone();
    args.extend(method.args.iter().cloned());
    let mut env: BTreeMap<String, String> = base_env
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    for (name, value) in &method.env {
        env.insert(name.clone(), value.clone());
    }
    PtyCommand {
        program: runtime.launch.command.clone(),
        args,
        env,
        cwd: cwd.to_path_buf(),
    }
}

/// Builds the legacy bridge invocation, resolving a bare relative command
/// inside the same Registry-managed agent's own install directory when the
/// sanitized `PATH` cannot resolve it.
///
/// The exact advertised command/args are preserved. Only a bare relative
/// command with no slash is a candidate for install-directory resolution,
/// and only when `PATH` does not already resolve it. The resolved file must
/// canonicalize inside `install_dir`, be a regular executable file, match
/// the advertised command name, and pass the existing path-boundary checks.
/// Never a shell. Never another agent's directory.
pub fn legacy_terminal_command_for_agent(
    agent_id: &str,
    legacy: &LegacyTerminalAuth,
    base_env: &HashMap<String, String>,
    cwd: &Path,
    install_dir: Option<&Path>,
) -> anyhow::Result<PtyCommand> {
    crate::acp::auth::validate_legacy_terminal_auth(legacy)?;
    validate_legacy_program(&legacy.command)?;
    let program = resolve_legacy_program(&legacy.command, base_env, install_dir, agent_id)?;
    let env: BTreeMap<String, String> = base_env
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    Ok(PtyCommand {
        program,
        args: legacy.args.clone(),
        env,
        cwd: cwd.to_path_buf(),
    })
}

/// Resolves the advertised legacy command to an executable path.
///
/// An absolute path or a name already on `PATH` is used unchanged. A bare
/// relative name that `PATH` cannot resolve is resolved inside this agent's
/// own validated install directory, under strict boundary rules.
fn resolve_legacy_program(
    command: &str,
    base_env: &HashMap<String, String>,
    install_dir: Option<&Path>,
    agent_id: &str,
) -> anyhow::Result<String> {
    // An absolute path stays exactly as advertised.
    if command.starts_with('/') {
        return Ok(command.to_string());
    }
    // A name that resolves through the sanitized PATH stays as advertised.
    if let Some(path) =
        crate::agents::which_in(command, std::ffi::OsStr::new(&path_value(base_env)))
    {
        if path.is_file() {
            return Ok(command.to_string());
        }
    }
    // A bare relative name may resolve inside this agent's own install dir.
    let Some(install_dir) = install_dir else {
        return Ok(command.to_string());
    };
    let resolved = resolve_inside_install_dir(command, install_dir, agent_id)?;
    Ok(resolved.display().to_string())
}

/// The sanitized `PATH` of one auth process, as a string for `which_in`.
fn path_value(base_env: &HashMap<String, String>) -> String {
    base_env.get("PATH").cloned().unwrap_or_default()
}

/// Resolves one advertised relative command inside the validated install
/// directory, rejecting traversal, symlink escape, cross-agent paths, and
/// non-executable or mismatched files.
fn resolve_inside_install_dir(
    command: &str,
    install_dir: &Path,
    agent_id: &str,
) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        !command.contains('/') && !command.contains('\\'),
        "A legacy login command with a path separator needs an absolute path"
    );
    // The install root must exist and canonicalize inside itself.
    let root = install_dir.canonicalize().map_err(|error| {
        anyhow::anyhow!(
            "The installed agent directory {} is not usable: {error}",
            install_dir.display()
        )
    })?;
    let candidate = root.join(command);
    let resolved = candidate.canonicalize().map_err(|error| {
        anyhow::anyhow!(
            "The advertised login command '{command}' was not found for agent '{agent_id}': {error}"
        )
    })?;
    // The resolved file must stay inside this agent's own install directory.
    anyhow::ensure!(
        resolved.starts_with(&root),
        "The advertised login command '{command}' resolves outside the installed agent directory"
    );
    anyhow::ensure!(
        resolved.is_file(),
        "The advertised login command '{command}' is not a file"
    );
    // The resolved file name must match the advertised command, so a symlink
    // to a differently named program is refused.
    let name_matches = resolved
        .file_name()
        .map(|name| name.to_string_lossy() == command)
        .unwrap_or(false);
    anyhow::ensure!(
        name_matches,
        "The advertised login command '{command}' resolved to a file with another name"
    );
    anyhow::ensure!(
        is_executable(&resolved),
        "The advertised login command '{command}' is not executable"
    );
    Ok(resolved)
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// The legacy bridge invocation without install-directory resolution. Kept
/// for callers that only have the advertised descriptor; it never searches
/// an install directory.
pub fn legacy_terminal_command(
    legacy: &LegacyTerminalAuth,
    base_env: &HashMap<String, String>,
    cwd: &Path,
) -> anyhow::Result<PtyCommand> {
    legacy_terminal_command_for_agent("", legacy, base_env, cwd, None)
}

fn validate_legacy_program(program: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!program.is_empty(), "Legacy login command is empty");
    anyhow::ensure!(program.len() <= 1024, "Legacy login command is too long");
    anyhow::ensure!(!program.contains('\0'), "Legacy login command is invalid");
    if program.contains('/') {
        // A path must be absolute and must not escape via `..`. A bare name
        // resolves through `PATH`; a relative path with a slash would depend
        // on the cwd and is rejected.
        anyhow::ensure!(
            program.starts_with('/'),
            "Legacy login command path must be absolute"
        );
        anyhow::ensure!(
            !program.contains(".."),
            "Legacy login command path is invalid"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentDefinition;

    fn runtime() -> AgentRuntime {
        AgentDefinition::new("demo", "demo-acp")
            .with_args(vec!["acp".into(), "--stdio".into()])
            .with_env(HashMap::from([("BASE_ONLY".into(), "base".into())]))
            .runtime()
    }

    #[test]
    fn terminal_command_appends_method_args_after_base_args() {
        let method = TerminalAuthMethod {
            args: vec!["login".into(), "--device".into()],
            env: BTreeMap::new(),
        };
        let command = terminal_command(
            &runtime(),
            &method,
            &HashMap::new(),
            Path::new("/var/lib/batey"),
        );
        assert_eq!(command.program, "demo-acp");
        assert_eq!(command.args, vec!["acp", "--stdio", "login", "--device"]);
        assert_eq!(command.cwd, Path::new("/var/lib/batey"));
    }

    #[test]
    fn method_env_overrides_the_base_env() {
        let method = TerminalAuthMethod {
            args: Vec::new(),
            env: BTreeMap::from([
                ("SHARED".into(), "from-method".into()),
                ("METHOD_ONLY".into(), "yes".into()),
            ]),
        };
        let base = HashMap::from([
            ("SHARED".to_string(), "from-base".to_string()),
            ("BASE_ONLY".to_string(), "kept".to_string()),
        ]);
        let command = terminal_command(&runtime(), &method, &base, Path::new("/tmp"));
        assert_eq!(
            command.env.get("SHARED").map(String::as_str),
            Some("from-method")
        );
        assert_eq!(
            command.env.get("BASE_ONLY").map(String::as_str),
            Some("kept")
        );
        assert_eq!(
            command.env.get("METHOD_ONLY").map(String::as_str),
            Some("yes")
        );
    }

    /// A method that advertises nothing must reproduce the base invocation.
    #[test]
    fn an_empty_method_reproduces_the_base_invocation() {
        let base = HashMap::from([("BASE_ONLY".to_string(), "kept".to_string())]);
        let command = terminal_command(
            &runtime(),
            &TerminalAuthMethod::default(),
            &base,
            Path::new("/tmp"),
        );
        assert_eq!(command.program, "demo-acp");
        assert_eq!(command.args, vec!["acp", "--stdio"]);
        assert_eq!(command.env.len(), 1);
    }

    #[test]
    fn legacy_command_uses_the_advertised_program_and_args() {
        let legacy = LegacyTerminalAuth {
            command: "/opt/copilot".into(),
            args: vec!["login".into()],
            label: Some("Copilot Login".into()),
        };
        let base = HashMap::from([("BASE_ONLY".to_string(), "kept".to_string())]);
        let command = legacy_terminal_command(&legacy, &base, Path::new("/var/lib/batey")).unwrap();
        assert_eq!(command.program, "/opt/copilot");
        assert_eq!(command.args, vec!["login"]);
        assert_eq!(command.cwd, Path::new("/var/lib/batey"));
        assert_eq!(
            command.env.get("BASE_ONLY").map(String::as_str),
            Some("kept")
        );
    }

    #[test]
    fn legacy_command_rejects_shell_lines_and_relative_paths() {
        let base = HashMap::new();
        for command in [
            "",
            "opencode; rm -rf /",
            "a/b/copilot",
            "/tmp/../etc/passwd",
        ] {
            let legacy = LegacyTerminalAuth {
                command: command.into(),
                args: Vec::new(),
                label: None,
            };
            assert!(
                legacy_terminal_command(&legacy, &base, Path::new("/tmp")).is_err(),
                "accepted {command:?}"
            );
        }
        // Bare names resolve through PATH and stay allowed.
        let legacy = LegacyTerminalAuth {
            command: "opencode".into(),
            args: vec!["auth".into(), "login".into()],
            label: None,
        };
        assert!(legacy_terminal_command(&legacy, &base, Path::new("/tmp")).is_ok());
    }

    fn write_executable(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms).unwrap();
        }
    }

    /// The timeout reason is provider-neutral and actionable.
    #[test]
    fn protocol_timeout_reason_is_provider_neutral() {
        assert!(PROTOCOL_TIMEOUT_REASON.contains("timeout"));
        assert!(PROTOCOL_TIMEOUT_REASON.contains("browser"));
        for provider in [
            "Codex",
            "Copilot",
            "Claude",
            "Antigravity",
            "OpenAI",
            "GitHub",
        ] {
            assert!(
                !PROTOCOL_TIMEOUT_REASON.contains(provider),
                "the timeout reason named {provider}"
            );
        }
    }

    /// A Registry-installed OpenCode advertises a bare `opencode` command.
    /// When PATH cannot resolve it, the resolver finds it inside that same
    /// agent's validated install directory.
    #[test]
    fn a_bare_legacy_command_resolves_inside_the_agents_own_install_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let install_dir = tmp.path().join("opencode/1.18.30");
        std::fs::create_dir_all(&install_dir).unwrap();
        let executable = install_dir.join("opencode");
        write_executable(&executable, "#!/bin/sh\nexit 0\n");

        let legacy = LegacyTerminalAuth {
            command: "opencode".into(),
            args: vec!["auth".into(), "login".into()],
            label: None,
        };
        let base = HashMap::from([("PATH".to_string(), String::new())]);
        let command = legacy_terminal_command_for_agent(
            "opencode",
            &legacy,
            &base,
            Path::new("/var/lib/batey"),
            Some(&install_dir),
        )
        .unwrap();
        // The advertised args are preserved exactly.
        assert_eq!(command.args, vec!["auth", "login"]);
        // The program is the resolved install-dir path.
        assert_eq!(
            Path::new(&command.program).canonicalize().unwrap(),
            executable.canonicalize().unwrap()
        );
    }

    /// A command that already resolves through PATH is never rewritten.
    #[test]
    fn a_path_resolvable_legacy_command_is_used_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let on_path = tmp.path().join("opencode");
        write_executable(&on_path, "#!/bin/sh\nexit 0\n");
        let install_dir = tmp.path().join("agents/opencode/1.0.0");
        std::fs::create_dir_all(&install_dir).unwrap();
        write_executable(&install_dir.join("opencode"), "#!/bin/sh\nexit 1\n");

        let legacy = LegacyTerminalAuth {
            command: "opencode".into(),
            args: vec!["auth".into(), "login".into()],
            label: None,
        };
        let base = HashMap::from([("PATH".to_string(), tmp.path().display().to_string())]);
        let command = legacy_terminal_command_for_agent(
            "opencode",
            &legacy,
            &base,
            Path::new("/tmp"),
            Some(&install_dir),
        )
        .unwrap();
        assert_eq!(command.program, "opencode");
    }

    /// A symlink inside the install directory that escapes it is refused.
    #[cfg(unix)]
    #[test]
    fn a_symlink_escaping_the_install_directory_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let install_dir = tmp.path().join("opencode/1.0.0");
        std::fs::create_dir_all(&install_dir).unwrap();
        let outside = tmp.path().join("outside-opencode");
        write_executable(&outside, "#!/bin/sh\nexit 0\n");
        std::os::unix::fs::symlink(&outside, install_dir.join("opencode")).unwrap();

        let legacy = LegacyTerminalAuth {
            command: "opencode".into(),
            args: vec!["auth".into(), "login".into()],
            label: None,
        };
        let base = HashMap::from([("PATH".to_string(), String::new())]);
        let error = legacy_terminal_command_for_agent(
            "opencode",
            &legacy,
            &base,
            Path::new("/tmp"),
            Some(&install_dir),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("outside") || error.to_string().contains("another name"),
            "{error}"
        );
    }

    /// Never search another agent's install directory: a command that only
    /// exists in a sibling agent's directory does not resolve.
    #[test]
    fn another_agents_install_directory_is_never_searched() {
        let tmp = tempfile::tempdir().unwrap();
        let other = tmp.path().join("other/1.0.0");
        std::fs::create_dir_all(&other).unwrap();
        write_executable(&other.join("opencode"), "#!/bin/sh\nexit 0\n");
        let own = tmp.path().join("opencode/1.0.0");
        std::fs::create_dir_all(&own).unwrap();

        let legacy = LegacyTerminalAuth {
            command: "opencode".into(),
            args: vec!["auth".into(), "login".into()],
            label: None,
        };
        let base = HashMap::from([("PATH".to_string(), String::new())]);
        let error = legacy_terminal_command_for_agent(
            "opencode",
            &legacy,
            &base,
            Path::new("/tmp"),
            Some(&own),
        )
        .unwrap_err();
        assert!(error.to_string().contains("not found"), "{error}");
    }

    /// A non-executable file inside the install directory is refused.
    #[cfg(unix)]
    #[test]
    fn a_non_executable_install_file_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let install_dir = tmp.path().join("opencode/1.0.0");
        std::fs::create_dir_all(&install_dir).unwrap();
        std::fs::write(install_dir.join("opencode"), "not executable").unwrap();

        let legacy = LegacyTerminalAuth {
            command: "opencode".into(),
            args: vec!["auth".into(), "login".into()],
            label: None,
        };
        let base = HashMap::from([("PATH".to_string(), String::new())]);
        let error = legacy_terminal_command_for_agent(
            "opencode",
            &legacy,
            &base,
            Path::new("/tmp"),
            Some(&install_dir),
        )
        .unwrap_err();
        assert!(error.to_string().contains("executable"), "{error}");
    }
}
