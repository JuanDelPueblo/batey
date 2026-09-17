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
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex as StdMutex,
};
use std::time::Duration;
use tokio::sync::watch;
use url::Url;

/// Maximum size of an authorization or callback URL accepted by the
/// ephemeral authentication surface.
pub const MAX_AUTH_URL_BYTES: usize = 8 * 1024;

/// A validated OAuth authorization request captured from one live auth flow.
/// It deliberately has no `Debug` or `Serialize` implementation because the
/// URL and OAuth state may only cross the flow-scoped interaction endpoint.
pub struct AuthorizationRequest {
    authorization_url: String,
    redirect: LoopbackRedirect,
    state: String,
}

#[derive(Clone)]
struct LoopbackRedirect {
    host: LoopbackHost,
    port: u16,
    path: String,
}

/// The exact host spelling/class accepted in the authorization request. The
/// relay uses the corresponding literal IP, so `localhost` never reaches a
/// resolver or a hosts-file-selected destination.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LoopbackHost {
    Ipv4,
    Localhost,
    Ipv6,
}

impl LoopbackHost {
    fn from_url(url: &Url) -> Option<Self> {
        match url.host()? {
            url::Host::Ipv4(ip) if ip == Ipv4Addr::LOCALHOST => Some(Self::Ipv4),
            url::Host::Ipv6(ip) if ip == Ipv6Addr::LOCALHOST => Some(Self::Ipv6),
            url::Host::Domain(domain) if domain.eq_ignore_ascii_case("localhost") => {
                Some(Self::Localhost)
            }
            _ => None,
        }
    }

    fn relay_ip(self) -> IpAddr {
        match self {
            Self::Ipv4 | Self::Localhost => IpAddr::V4(Ipv4Addr::LOCALHOST),
            Self::Ipv6 => IpAddr::V6(Ipv6Addr::LOCALHOST),
        }
    }
}

/// Safe URL-validation errors. None includes a rejected URL or query value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthUrlError {
    TooLarge,
    Malformed,
    NotHttps,
    MissingRedirect,
    MissingState,
    InvalidRedirect,
    NotLoopback,
    MissingPort,
    Credentials,
    InvalidCallback,
    WrongEndpoint,
    WrongState,
    MissingResult,
    CallbackInProgress,
    FlowFinished,
}

impl std::fmt::Display for AuthUrlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TooLarge => "Authentication URL is too large",
            Self::Malformed => "Authentication URL is malformed",
            Self::NotHttps => "Authentication URL must use HTTPS",
            Self::MissingRedirect => "Authentication URL has no redirect URI",
            Self::MissingState => "Authentication URL has no OAuth state",
            Self::InvalidRedirect => "Authentication redirect URI is malformed",
            Self::NotLoopback => "Authentication redirect URI is not a loopback address",
            Self::MissingPort => "Authentication redirect URI has no explicit port",
            Self::Credentials => "Authentication URL may not contain credentials",
            Self::InvalidCallback => "Callback URL is malformed",
            Self::WrongEndpoint => "Callback URL does not match the authentication endpoint",
            Self::WrongState => "Callback OAuth state does not match",
            Self::MissingResult => "Callback URL has no authorization result",
            Self::CallbackInProgress => "Another callback relay is already running",
            Self::FlowFinished => "Authentication flow is already finished",
        })
    }
}

impl std::error::Error for AuthUrlError {}

/// Parses and validates one captured authorization URL. The ACP agent remains
/// the OAuth client and credential owner.
pub fn parse_authorization_url(raw: &str) -> Result<AuthorizationRequest, AuthUrlError> {
    if raw.len() > MAX_AUTH_URL_BYTES {
        return Err(AuthUrlError::TooLarge);
    }
    let url = Url::parse(raw.trim()).map_err(|_| AuthUrlError::Malformed)?;
    if url.scheme() != "https" {
        return Err(AuthUrlError::NotHttps);
    }
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err(AuthUrlError::Credentials);
    }
    if url.fragment().is_some() {
        return Err(AuthUrlError::Malformed);
    }
    let pairs: Vec<_> = url.query_pairs().collect();
    let redirect =
        unique_query_value(&pairs, "redirect_uri").ok_or(AuthUrlError::MissingRedirect)?;
    let state = unique_query_value(&pairs, "state").ok_or(AuthUrlError::MissingState)?;
    if state.is_empty() {
        return Err(AuthUrlError::MissingState);
    }
    let redirect_url = Url::parse(&redirect).map_err(|_| AuthUrlError::InvalidRedirect)?;
    if redirect_url.scheme() != "http"
        || !redirect_url.username().is_empty()
        || redirect_url.password().is_some()
        || redirect_url.fragment().is_some()
        || redirect_url.query().is_some()
    {
        return Err(AuthUrlError::InvalidRedirect);
    }
    let host = LoopbackHost::from_url(&redirect_url).ok_or(AuthUrlError::NotLoopback)?;
    let port = redirect_url.port().ok_or(AuthUrlError::MissingPort)?;
    Ok(AuthorizationRequest {
        authorization_url: raw.trim().to_owned(),
        redirect: LoopbackRedirect {
            host,
            port,
            path: if redirect_url.path().is_empty() {
                "/".to_owned()
            } else {
                redirect_url.path().to_owned()
            },
        },
        state,
    })
}

fn unique_query_value(
    pairs: &[(std::borrow::Cow<'_, str>, std::borrow::Cow<'_, str>)],
    key: &str,
) -> Option<String> {
    let mut values = pairs
        .iter()
        .filter(|(name, _)| name == key)
        .map(|(_, value)| value.to_string());
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

impl AuthorizationRequest {
    pub(crate) fn interaction_view(&self) -> ProtocolAuthInteractionView {
        ProtocolAuthInteractionView {
            kind: "browser".to_owned(),
            url: self.authorization_url.clone(),
            manual_callback: true,
        }
    }

    fn validate_callback(&self, raw: &str) -> Result<Url, AuthUrlError> {
        if raw.len() > MAX_AUTH_URL_BYTES {
            return Err(AuthUrlError::TooLarge);
        }
        let callback = Url::parse(raw.trim()).map_err(|_| AuthUrlError::InvalidCallback)?;
        if callback.scheme() != "http"
            || !callback.username().is_empty()
            || callback.password().is_some()
            || callback.fragment().is_some()
        {
            return Err(AuthUrlError::InvalidCallback);
        }
        let host = LoopbackHost::from_url(&callback).ok_or(AuthUrlError::InvalidCallback)?;
        let path = if callback.path().is_empty() {
            "/"
        } else {
            callback.path()
        };
        if host != self.redirect.host
            || callback.port() != Some(self.redirect.port)
            || path != self.redirect.path
        {
            return Err(AuthUrlError::WrongEndpoint);
        }
        let pairs: Vec<_> = callback.query_pairs().collect();
        let state = unique_query_value(&pairs, "state").ok_or(AuthUrlError::WrongState)?;
        if state != self.state {
            return Err(AuthUrlError::WrongState);
        }
        let code = unique_query_value(&pairs, "code");
        let error = unique_query_value(&pairs, "error");
        if code.as_deref().is_some_and(str::is_empty)
            || error.as_deref().is_some_and(str::is_empty)
            || (code.is_some() && error.is_some())
            || (code.is_none() && error.is_none())
        {
            return Err(AuthUrlError::MissingResult);
        }
        let relay_base = match self.redirect.host.relay_ip() {
            IpAddr::V4(_) => "http://127.0.0.1",
            IpAddr::V6(_) => "http://[::1]",
        };
        let mut relay = Url::parse(relay_base).expect("literal loopback URL is valid");
        relay
            .set_port(Some(self.redirect.port))
            .map_err(|_| AuthUrlError::InvalidCallback)?;
        relay.set_path(&self.redirect.path);
        relay.set_query(callback.query());
        Ok(relay)
    }
}

/// Separate from `ProtocolAuthFlowView` and `AgentAuthView` so reload
/// discovery cannot accidentally include private interaction data.
#[derive(Debug, Clone, Serialize)]
pub struct ProtocolAuthInteractionView {
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    pub manual_callback: bool,
}

struct ProtocolAuthInteraction {
    request: AuthorizationRequest,
    relaying: bool,
}

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
    interaction: StdMutex<Option<ProtocolAuthInteraction>>,
    authorization_captured: AtomicBool,
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
        // Keep the terminal transition and interaction cleanup in the same
        // critical section as interaction installation. This prevents a
        // capture which started before cancellation from restoring sensitive
        // OAuth material after the flow is terminal.
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
        self.interaction
            .lock()
            .expect("protocol auth interaction lock poisoned")
            .take();
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

    pub(crate) fn set_authorization_request(&self, request: AuthorizationRequest) -> bool {
        {
            // Hold status while installing the interaction so finish() cannot
            // transition the flow and clear it between the terminal check and
            // this write.
            let status = self
                .status
                .lock()
                .expect("protocol auth status lock poisoned");
            if status.state.is_finished() {
                return false;
            }
            if self
                .authorization_captured
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return false;
            }
            *self
                .interaction
                .lock()
                .expect("protocol auth interaction lock poisoned") =
                Some(ProtocolAuthInteraction {
                    request,
                    relaying: false,
                });
        }
        self.note_waiting(true);
        true
    }

    pub(crate) fn authorization_captured(&self) -> bool {
        self.authorization_captured.load(Ordering::Acquire)
    }

    pub fn interaction(&self) -> Option<ProtocolAuthInteractionView> {
        self.interaction
            .lock()
            .expect("protocol auth interaction lock poisoned")
            .as_ref()
            .map(|interaction| interaction.request.interaction_view())
    }

    pub(crate) fn begin_callback(&self, callback: &str) -> Result<Url, AuthUrlError> {
        if self.state().is_finished() {
            return Err(AuthUrlError::FlowFinished);
        }
        let mut interaction = self
            .interaction
            .lock()
            .expect("protocol auth interaction lock poisoned");
        let interaction = interaction.as_mut().ok_or(AuthUrlError::InvalidCallback)?;
        if interaction.relaying {
            return Err(AuthUrlError::CallbackInProgress);
        }
        let target = interaction.request.validate_callback(callback)?;
        interaction.relaying = true;
        Ok(target)
    }

    pub(crate) fn finish_callback(&self, success: bool) {
        let mut interaction = self
            .interaction
            .lock()
            .expect("protocol auth interaction lock poisoned");
        if success {
            interaction.take();
        } else if let Some(interaction) = interaction.as_mut() {
            interaction.relaying = false;
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
            interaction: StdMutex::new(None),
            authorization_captured: AtomicBool::new(false),
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

    const AUTH_URL: &str = "https://accounts.example.test/authorize?client_id=x&redirect_uri=http%3A%2F%2F127.0.0.1%3A43123%2Foauth%2Fcallback&state=state-123&response_type=code";

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
    fn authorization_url_requires_https_loopback_and_state() {
        let request = parse_authorization_url(AUTH_URL).unwrap();
        assert_eq!(request.interaction_view().url, AUTH_URL);
        assert!(matches!(
            parse_authorization_url(
                "http://accounts.example.test/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A43123%2Foauth%2Fcallback&state=x"
            ),
            Err(AuthUrlError::NotHttps)
        ));
        assert!(matches!(
            parse_authorization_url(
                "https://accounts.example.test/authorize?redirect_uri=http%3A%2F%2F10.0.0.1%3A43123%2Fcallback&state=x"
            ),
            Err(AuthUrlError::NotLoopback)
        ));
    }

    #[test]
    fn callback_validation_is_exact_and_requires_code_or_error() {
        let request = parse_authorization_url(AUTH_URL).unwrap();
        assert!(request
            .validate_callback(
                "http://127.0.0.1:43123/oauth/callback?code=secret-code&state=state-123"
            )
            .is_ok());
        for callback in [
            "http://127.0.0.1:43124/oauth/callback?code=x&state=state-123",
            "http://127.0.0.1:43123/other?code=x&state=state-123",
            "http://127.0.0.1:43123/oauth/callback?code=x&state=wrong",
            "http://127.0.0.1:43123/oauth/callback?state=state-123",
            "http://user:pass@127.0.0.1:43123/oauth/callback?code=x&state=state-123",
        ] {
            assert!(
                request.validate_callback(callback).is_err(),
                "accepted {callback}"
            );
        }
        assert!(request
            .validate_callback(
                "http://127.0.0.1:43123/oauth/callback?error=access_denied&state=state-123"
            )
            .is_ok());
    }

    #[test]
    fn callback_relay_uses_literal_loopback_ips() {
        let localhost = parse_authorization_url(
            "https://accounts.example.test/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A43123%2Fcallback&state=local",
        )
        .unwrap();
        let relay = localhost
            .validate_callback("http://localhost:43123/callback?code=x&state=local")
            .unwrap();
        assert_eq!(relay.host_str(), Some("127.0.0.1"));

        let ipv6 = parse_authorization_url(
            "https://accounts.example.test/authorize?redirect_uri=http%3A%2F%2F%5B%3A%3A1%5D%3A43123%2Fcallback&state=v6",
        )
        .unwrap();
        let relay = ipv6
            .validate_callback("http://[::1]:43123/callback?error=denied&state=v6")
            .unwrap();
        assert_eq!(relay.host_str(), Some("[::1]"));
    }

    #[test]
    fn authorization_capture_is_first_valid_request_for_flow_lifetime() {
        let flows = ProtocolAuthFlows::new();
        let flow = flows.create("demo", "oauth").unwrap();
        assert!(flow.set_authorization_request(parse_authorization_url(AUTH_URL).unwrap()));
        assert!(!flow.set_authorization_request(
            parse_authorization_url(
                "https://accounts.example.test/authorize?redirect_uri=http%3A%2F%2F127.0.0.1%3A43124%2Fcallback&state=second"
            )
            .unwrap()
        ));
        assert_eq!(flow.interaction().unwrap().url, AUTH_URL);
        flow.finish(ProtocolFlowState::Succeeded, None);
        assert!(flow.interaction().is_none());
        assert!(!flow.set_authorization_request(parse_authorization_url(AUTH_URL).unwrap()));
        flows.shutdown_all();
    }

    #[test]
    fn authorization_capture_racing_finish_cannot_restore_interaction() {
        let flows = ProtocolAuthFlows::new();
        for _ in 0..1024 {
            let flow = flows.create("demo", "oauth").unwrap();
            let start = Arc::new(std::sync::Barrier::new(3));
            std::thread::scope(|scope| {
                let finish_flow = Arc::clone(&flow);
                let finish_start = Arc::clone(&start);
                scope.spawn(move || {
                    finish_start.wait();
                    finish_flow.finish(ProtocolFlowState::Cancelled, Some("race test".to_owned()));
                });

                let capture_flow = Arc::clone(&flow);
                let capture_start = Arc::clone(&start);
                scope.spawn(move || {
                    capture_start.wait();
                    let _ = capture_flow
                        .set_authorization_request(parse_authorization_url(AUTH_URL).unwrap());
                });

                start.wait();
            });

            assert!(flow.state().is_finished());
            assert!(
                flow.interaction().is_none(),
                "terminal flow retained an authorization interaction"
            );
        }
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
