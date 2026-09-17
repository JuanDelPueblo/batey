pub mod auth;
pub mod callbacks;
pub mod process;
pub mod protocol;
#[cfg(windows)]
mod windows_job;

use ::agent_client_protocol_schema::v1 as agent_client_protocol_schema;
use ::agent_client_protocol_schema::ProtocolVersion;
use agent_client_protocol_schema::{
    CancelNotification, ContentBlock, CreateTerminalRequest, InitializeRequest, InitializeResponse,
    KillTerminalRequest, LoadSessionRequest, McpServer, McpServerHttp, McpServerSse,
    McpServerStdio, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    ReadTextFileRequest, ReleaseTerminalRequest, RequestPermissionRequest, ResumeSessionRequest,
    SessionId, SessionNotification, SessionUpdate, TerminalOutputRequest, ToolCallContent,
    WaitForTerminalExitRequest, WriteTextFileRequest, CLIENT_METHOD_NAMES,
};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;

use crate::events::{EventLog, EventPayload};

use self::callbacks::CallbackHandler;
pub use self::process::StderrPolicy;
use self::process::{drain_stderr, AcpProcess, StderrTail};
use self::protocol::{
    IncomingKind, IncomingMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
};

type ResponseResult = anyhow::Result<serde_json::Value>;
type SharedChild = Arc<Mutex<Option<Box<dyn process_wrap::tokio::TokioChildWrapper>>>>;

/// A saved ACP option the agent genuinely rejects, or that is locally invalid.
///
/// Carries the `option_id` so `HubService::resume_chat` can map it to
/// `ServiceError::SavedConfigRejected` without parsing an error string.
/// Transient failures (timeouts, transport disconnects, writer failures,
/// malformed agent responses) must NOT use this type: they stay as plain
/// `anyhow` errors so the ordinary Retry path appears and `config_values`
/// are preserved.
#[derive(Debug)]
pub struct SavedConfigRejected {
    pub option_id: String,
    pub message: String,
}

impl std::fmt::Display for SavedConfigRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SavedConfigRejected {}

/// An outgoing ACP request passed its local deadline. The client already
/// emitted `$/cancel_request` for it, so callers treat this as transient:
/// it must never be mistaken for an agent rejection.
#[derive(Debug)]
pub struct RequestTimedOut {
    pub method: &'static str,
}

impl std::fmt::Display for RequestTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ACP request {} timed out", self.method)
    }
}

impl std::error::Error for RequestTimedOut {}

/// A JSON-RPC error an agent returned.
///
/// The code stays on the error so a caller can recognize a stable protocol
/// condition such as `auth_required`. A check against the user-facing message
/// would break on every agent that words that message differently.
#[derive(Debug, Clone)]
pub struct AcpRpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<serde_json::Value>,
}

impl AcpRpcError {
    /// Whether the agent reported the stable `auth_required` error code.
    pub fn is_auth_required(&self) -> bool {
        i32::try_from(self.code).is_ok_and(|code| {
            agent_client_protocol_schema::ErrorCode::from(code)
                == agent_client_protocol_schema::ErrorCode::AuthRequired
        })
    }
}

impl std::fmt::Display for AcpRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RPC error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for AcpRpcError {}

/// The agent needs authentication before it can serve a request.
///
/// The session layer attaches the agent id, so a Hub surface can offer the
/// matching authentication flow. The chat keeps its durable history; only the
/// agent process failed to start or to answer.
#[derive(Debug, Clone)]
pub struct AuthRequired {
    pub agent: String,
    pub message: String,
}

impl std::fmt::Display for AuthRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Agent '{}' requires authentication: {}",
            self.agent, self.message
        )
    }
}

impl std::error::Error for AuthRequired {}

/// Rewrites an agent-reported `auth_required` failure as the typed recoverable
/// error. Every other error passes through unchanged.
pub fn map_auth_required(agent: &str, error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<AcpRpcError>() {
        Some(rpc) if rpc.is_auth_required() => anyhow::Error::new(AuthRequired {
            agent: agent.to_owned(),
            message: rpc.message.clone(),
        }),
        _ => error,
    }
}

/// Returns a stable category for ACP failures without exposing agent-controlled
/// error text in operational logs.
pub fn failure_category(error: &anyhow::Error) -> &'static str {
    if error.is::<RequestTimedOut>() || error.to_string().contains("timed out") {
        "timeout"
    } else if error.is::<AuthRequired>() {
        "authentication_required"
    } else if error.is::<AcpRpcError>() {
        "agent_rpc_error"
    } else {
        "acp_failure"
    }
}

/// Returns a stable category for process-start failures without logging the
/// command or the operating system's raw error text.
pub fn spawn_failure_category(error: &anyhow::Error) -> &'static str {
    let message = error.to_string();
    if message.ends_with("executable not found") {
        "executable_not_found"
    } else if message.contains("executable exists but the process could not start") {
        "executable_start_failed"
    } else {
        "process_spawn_failed"
    }
}

fn sanitized_executable_identifier(command: &str) -> String {
    let basename = Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown");
    let identifier: String = basename
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
        .take(128)
        .collect();
    if identifier.is_empty() {
        "unknown".to_owned()
    } else {
        identifier
    }
}

enum WriterMsg {
    Line(String),
    Shutdown,
}

pub struct AcpClient {
    agent_id: String,
    chat_id: String,
    replaying: Arc<AtomicBool>,
    pub capabilities: tokio::sync::RwLock<serde_json::Value>,
    prompt_capabilities: tokio::sync::RwLock<agent_client_protocol_schema::PromptCapabilities>,
    pub config_options: Arc<tokio::sync::RwLock<serde_json::Value>>,
    pub available_commands: Arc<tokio::sync::RwLock<serde_json::Value>>,
    pub session_modes: Arc<tokio::sync::RwLock<serde_json::Value>>,
    pub last_usage: Arc<tokio::sync::RwLock<serde_json::Value>>,
    pub agent_info: tokio::sync::RwLock<serde_json::Value>,
    pub auth_methods: tokio::sync::RwLock<serde_json::Value>,
    /// Typed provider-neutral authentication state read at `initialize`.
    auth_state: tokio::sync::RwLock<self::auth::AgentAuthState>,
    writer_tx: mpsc::Sender<WriterMsg>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<ResponseResult>>>>,
    next_id: AtomicI64,
    writer_handle: Mutex<Option<JoinHandle<()>>>,
    reader_handle: Mutex<Option<JoinHandle<()>>>,
    wait_handle: Mutex<Option<JoinHandle<()>>>,
    stderr_handle: Mutex<Option<JoinHandle<()>>>,
    child: SharedChild,
    child_root_pid: Option<u32>,
    callback_handler: Arc<CallbackHandler>,
    connected: Arc<AtomicBool>,
    /// A bounded, best-effort diagnostic for a process that dies before
    /// completing the ACP handshake. Empty (and never populated) for
    /// authentication processes, which use a discard-only policy (with a
    /// narrowly scoped URL capture exception for compatible browser auth).
    stderr_tail: StderrTail,
}

impl AcpClient {
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn(
        command: &str,
        args: &[String],
        env_vars: &HashMap<String, String>,
        cwd: &Path,
        session_id: String,
        agent_name: String,
        event_log: Arc<EventLog>,
        store: Option<Arc<crate::store::Store>>,
        task_tracker: Arc<crate::tasks::TerminalTaskTracker>,
        effective_roots: Vec<std::path::PathBuf>,
        stderr_policy: StderrPolicy,
    ) -> anyhow::Result<Self> {
        let executable_identifier = sanitized_executable_identifier(command);
        tracing::info!(
            agent_id = %agent_name,
            chat_id = %session_id,
            executable = %executable_identifier,
            "ACP process spawn attempt"
        );
        let proc = match AcpProcess::spawn(command, args, env_vars, cwd) {
            Ok(proc) => proc,
            Err(error) => {
                tracing::error!(
                    agent_id = %agent_name,
                    chat_id = %session_id,
                    executable = %executable_identifier,
                    error_category = %spawn_failure_category(&error),
                    "ACP process spawn failed"
                );
                return Err(error);
            }
        };
        tracing::info!(
            agent_id = %agent_name,
            chat_id = %session_id,
            pid = ?proc.root_pid,
            "ACP process spawned"
        );

        let (writer_tx, writer_rx) = mpsc::channel::<WriterMsg>(64);
        let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<ResponseResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let connected = Arc::new(AtomicBool::new(true));

        let callback_handler = Arc::new(CallbackHandler::new_with_roots(
            session_id.clone(),
            agent_name.clone(),
            event_log.clone(),
            cwd.to_path_buf(),
            effective_roots,
            Arc::new(env_vars.clone()),
            task_tracker,
        ));

        let child_root_pid = proc.root_pid;
        let child = Arc::new(Mutex::new(Some(proc.child)));

        let writer_handle = tokio::spawn(writer_task(
            proc.stdin,
            writer_rx,
            connected.clone(),
            child.clone(),
            child_root_pid,
            session_id.clone(),
            agent_name.clone(),
        ));

        let stderr_tail = StderrTail::new();
        let stderr_handle = tokio::spawn(drain_stderr(
            proc.stderr,
            agent_name.clone(),
            stderr_policy,
            stderr_tail.clone(),
        ));

        let config_options = Arc::new(tokio::sync::RwLock::new(serde_json::json!([])));
        let available_commands = Arc::new(tokio::sync::RwLock::new(serde_json::json!([])));
        let session_modes = Arc::new(tokio::sync::RwLock::new(serde_json::Value::Null));
        let last_usage = Arc::new(tokio::sync::RwLock::new(serde_json::Value::Null));
        let replaying = Arc::new(AtomicBool::new(false));
        let reader_handle = tokio::spawn(reader_task(
            proc.stdout,
            pending.clone(),
            connected.clone(),
            callback_handler.clone(),
            writer_tx.clone(),
            child.clone(),
            child_root_pid,
            event_log.clone(),
            session_id.clone(),
            agent_name.clone(),
            config_options.clone(),
            available_commands.clone(),
            session_modes.clone(),
            last_usage.clone(),
            replaying.clone(),
            store,
            stderr_tail.clone(),
        ));

        let wait_handle = tokio::spawn(wait_task(
            child.clone(),
            child_root_pid,
            connected.clone(),
            session_id.clone(),
            agent_name.clone(),
        ));

        Ok(Self {
            agent_id: agent_name,
            chat_id: session_id,
            replaying,
            capabilities: tokio::sync::RwLock::new(serde_json::json!({})),
            prompt_capabilities: tokio::sync::RwLock::new(
                agent_client_protocol_schema::PromptCapabilities::default(),
            ),
            config_options,
            available_commands,
            session_modes,
            last_usage,
            agent_info: tokio::sync::RwLock::new(serde_json::Value::Null),
            auth_methods: tokio::sync::RwLock::new(serde_json::json!([])),
            auth_state: tokio::sync::RwLock::new(self::auth::AgentAuthState::default()),
            writer_tx,
            pending,
            next_id: AtomicI64::new(1),
            writer_handle: Mutex::new(Some(writer_handle)),
            reader_handle: Mutex::new(Some(reader_handle)),
            wait_handle: Mutex::new(Some(wait_handle)),
            stderr_handle: Mutex::new(Some(stderr_handle)),
            child,
            child_root_pid,
            callback_handler,
            connected,
            stderr_tail,
        })
    }

    pub(crate) async fn send_request<P: serde::Serialize>(
        &self,
        method: &'static str,
        params: P,
    ) -> anyhow::Result<serde_json::Value> {
        let (_, rx) = self.dispatch_request(method, params).await?;
        rx.await
            .map_err(|_| anyhow::anyhow!("Response channel dropped"))?
    }

    /// Sends one JSON-RPC request and awaits it with a deadline. When the
    /// deadline passes first, the client emits stable `$/cancel_request`
    /// for that request id before reporting the timeout, so an abandoned
    /// outgoing request is actually protocol-cancelled. This stays distinct
    /// from `session/cancel`, which cancels a whole turn.
    pub async fn send_request_with_timeout<P: serde::Serialize>(
        &self,
        method: &'static str,
        params: P,
        timeout: std::time::Duration,
    ) -> anyhow::Result<serde_json::Value> {
        let (id, rx) = self.dispatch_request(method, params).await?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(outcome) => outcome.map_err(|_| anyhow::anyhow!("Response channel dropped"))?,
            Err(_) => {
                // A late answer finds no pending entry and is dropped.
                // Tell the agent the request is abandoned first.
                self.pending.lock().await.remove(&id);
                self.cancel_request(id).await;
                tracing::warn!(
                    agent_id = %self.agent_id,
                    chat_id = %self.chat_id,
                    request_id = id,
                    operation = method,
                    "ACP request timed out"
                );
                Err(anyhow::Error::new(RequestTimedOut { method }))
            }
        }
    }

    /// "ACP agent connection closed", with a bounded recent-stderr snippet
    /// appended when one was captured (never for an authentication process,
    /// whose stderr is always discarded). This is the first place a process
    /// that dies before completing `initialize` gets to report why: a plain
    /// "connection closed" otherwise looks identical whether the executable
    /// crashed, its interpreter/loader was missing, or it exited cleanly.
    fn connection_closed_message(&self) -> String {
        match self.stderr_tail.snippet() {
            Some(snippet) => format!("ACP agent connection closed (recent stderr: {snippet})"),
            None => "ACP agent connection closed".to_string(),
        }
    }

    async fn dispatch_request<P: serde::Serialize>(
        &self,
        method: &'static str,
        params: P,
    ) -> anyhow::Result<(i64, oneshot::Receiver<ResponseResult>)> {
        if !self.is_connected() {
            anyhow::bail!(self.connection_closed_message());
        }

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = JsonRpcRequest::new(id.into(), method, params);
        let line = serde_json::to_string(&req)?;

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(id, tx);
        }

        if !self.is_connected() {
            let mut pending = self.pending.lock().await;
            if let Some(tx) = pending.remove(&id) {
                let _ = tx.send(Err(anyhow::anyhow!(self.connection_closed_message())));
            }
            anyhow::bail!(self.connection_closed_message());
        }

        if self.writer_tx.send(WriterMsg::Line(line)).await.is_err() {
            let mut pending = self.pending.lock().await;
            pending.remove(&id);
            anyhow::bail!("Writer channel closed");
        }

        Ok((id, rx))
    }

    async fn send_notification<P: serde::Serialize>(
        &self,
        method: &'static str,
        params: P,
    ) -> anyhow::Result<()> {
        let notif = JsonRpcNotification::new(method, params);
        let line = serde_json::to_string(&notif)?;
        self.writer_tx
            .send(WriterMsg::Line(line))
            .await
            .map_err(|_| anyhow::anyhow!("Writer channel closed"))?;
        Ok(())
    }

    pub async fn initialize(&self, _cwd: &Path) -> anyhow::Result<InitializeResponse> {
        // Advertise only stable v1 capabilities Batey actually implements:
        // filesystem read/write, terminal, boolean session config, and form
        // plus URL elicitation. Never advertise partial capabilities.
        let mut caps = agent_client_protocol_schema::ClientCapabilities::new()
            .fs(agent_client_protocol_schema::FileSystemCapabilities::new()
                .read_text_file(true)
                .write_text_file(true))
            .terminal(true)
            .session(
                agent_client_protocol_schema::ClientSessionCapabilities::new().config_options(
                    agent_client_protocol_schema::SessionConfigOptionsCapabilities::new().boolean(
                        agent_client_protocol_schema::BooleanConfigOptionCapabilities::new(),
                    ),
                ),
            )
            .elicitation(
                agent_client_protocol_schema::ElicitationCapabilities::new()
                    .form(agent_client_protocol_schema::ElicitationFormCapabilities::new())
                    .url(agent_client_protocol_schema::ElicitationUrlCapabilities::new()),
            )
            // Advertise terminal authentication only where the real PTY runs.
            // An agent may offer a `terminal` auth method only after the
            // client claims this capability, so a build without a usable PTY
            // must never claim it.
            .auth(
                agent_client_protocol_schema::AuthCapabilities::new()
                    .terminal(crate::auth::TERMINAL_AUTH_SUPPORTED),
            );
        // Legacy interoperability: advertise `_meta["terminal-auth"]` only
        // when the PTY bridge actually exists. Deployed OpenCode/Copilot
        // agents read this flag before sending their command descriptor.
        if crate::auth::TERMINAL_AUTH_SUPPORTED {
            let mut meta = agent_client_protocol_schema::Meta::new();
            meta.insert(
                self::auth::LEGACY_TERMINAL_AUTH_CLIENT_KEY.to_string(),
                serde_json::Value::Bool(true),
            );
            caps = caps.meta(meta);
        }
        let req = InitializeRequest::new(ProtocolVersion::LATEST)
            .client_info(agent_client_protocol_schema::Implementation::new(
                "batey",
                env!("CARGO_PKG_VERSION"),
            ))
            .client_capabilities(caps);
        let result = self
            .send_request_with_timeout("initialize", req, std::time::Duration::from_secs(60))
            .await?;
        let response: InitializeResponse = serde_json::from_value(result.clone())?;
        // Target stable ACP v1 only. Reject pre-release or future versions
        // explicitly rather than negotiating silently.
        anyhow::ensure!(
            response.protocol_version == ProtocolVersion::LATEST,
            "Agent negotiated unsupported protocol version {}",
            response.protocol_version.as_u16()
        );
        // Keep session lifecycle capabilities in their typed stable-v1 wire
        // representation, and retain the prompt surface separately so prompt
        // admission never has to guess at a JSON shape.
        *self.capabilities.write().await = serde_json::to_value(&response.agent_capabilities)?;
        *self.prompt_capabilities.write().await =
            response.agent_capabilities.prompt_capabilities.clone();
        // Preserve generic agent identity and auth metadata for later use.
        *self.agent_info.write().await = result
            .get("agentInfo")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let auth_methods = result
            .get("authMethods")
            .cloned()
            .unwrap_or(serde_json::json!([]));
        // Keep the typed state beside the raw snapshot. The typed state is
        // what the authentication surface reads; the raw value stays for
        // diagnostics and for fields this build does not model.
        *self.auth_state.write().await = self::auth::AgentAuthState::from_initialize(
            &auth_methods,
            &*self.capabilities.read().await,
        );
        *self.auth_methods.write().await = auth_methods;
        Ok(response)
    }

    /// The typed authentication state the last `initialize` returned.
    pub async fn auth_state(&self) -> self::auth::AgentAuthState {
        self.auth_state.read().await.clone()
    }

    /// Runs the stable `authenticate` method for one advertised `agent`
    /// method.
    ///
    /// The method must come from the agent's own `authMethods`. A terminal
    /// method never reaches `authenticate`: the stable schema requires the
    /// client to run the configured program instead. A legacy bridge method
    /// never reaches it either: it runs its advertised command in a
    /// terminal. An unsupported method kind is reported, never guessed.
    pub async fn authenticate(&self, method_id: &str) -> anyhow::Result<serde_json::Value> {
        let state = self.auth_state.read().await.clone();
        let method = state.method(method_id).ok_or_else(|| {
            anyhow::anyhow!("Agent does not advertise the authentication method '{method_id}'")
        })?;
        match &method.kind {
            self::auth::AuthMethodKind::Agent => {}
            self::auth::AuthMethodKind::Terminal(_) => anyhow::bail!(
                "Authentication method '{method_id}' is a terminal method; run it in a terminal instead"
            ),
            self::auth::AuthMethodKind::LegacyTerminal(_) => anyhow::bail!(
                "Authentication method '{method_id}' runs its advertised login command in a terminal; start a terminal authentication flow instead"
            ),
            self::auth::AuthMethodKind::Unsupported(kind) => anyhow::bail!(
                "Authentication method '{method_id}' uses the unsupported type '{kind}'"
            ),
        }
        let request = agent_client_protocol_schema::AuthenticateRequest::new(method_id.to_owned());
        self.send_request_with_timeout("authenticate", request, std::time::Duration::from_secs(120))
            .await
    }

    /// Runs the capability-gated stable `logout` method.
    ///
    /// The request goes out only when the agent advertised
    /// `agentCapabilities.auth.logout`. Batey never sends it
    /// speculatively.
    pub async fn logout(&self) -> anyhow::Result<serde_json::Value> {
        anyhow::ensure!(
            self.auth_state.read().await.logout_supported,
            "Agent does not support logout"
        );
        self.send_request_with_timeout(
            "logout",
            agent_client_protocol_schema::LogoutRequest::new(),
            std::time::Duration::from_secs(60),
        )
        .await
    }

    pub async fn new_session(&self, cwd: &Path) -> anyhow::Result<NewSessionResponse> {
        let req = NewSessionRequest::new(cwd.to_path_buf());
        let result = self.send_request("session/new", req).await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Preserve the agent's identity. Never fall back to session/new on resume failure.
    pub async fn open_session(
        &self,
        cwd: &Path,
        saved: Option<&str>,
        mcp_servers: Vec<McpServer>,
        additional_directories: Vec<std::path::PathBuf>,
    ) -> anyhow::Result<String> {
        // Clear transient dynamic state BEFORE load/resume so updates that
        // arrive as part of ACP replay are retained rather than wiped by a
        // reset that runs after the request returns.
        *self.available_commands.write().await = serde_json::json!([]);
        *self.last_usage.write().await = serde_json::Value::Null;
        let result = if let Some(id) = saved {
            let caps = self.capabilities.read().await.clone();
            let method = if caps["loadSession"] == true {
                "session/load"
            } else if caps
                .pointer("/sessionCapabilities/resume")
                .is_some_and(|v| v.is_object())
            {
                "session/resume"
            } else {
                anyhow::bail!("This agent cannot resume saved chat {id}. Create a new chat to start another conversation.")
            };
            // Our durable event log already contains the displayed history.
            // Loading still restores agent-owned state; do not append its replay twice.
            self.replaying.store(true, Ordering::SeqCst);
            let result = if method == "session/load" {
                self.send_request_with_timeout(
                    method,
                    LoadSessionRequest::new(id.to_owned(), cwd.to_path_buf())
                        .mcp_servers(mcp_servers)
                        .additional_directories(additional_directories),
                    std::time::Duration::from_secs(60),
                )
                .await
            } else {
                self.send_request_with_timeout(
                    method,
                    ResumeSessionRequest::new(id.to_owned(), cwd.to_path_buf())
                        .mcp_servers(mcp_servers)
                        .additional_directories(additional_directories),
                    std::time::Duration::from_secs(60),
                )
                .await
            };
            self.replaying.store(false, Ordering::SeqCst);
            result?
        } else {
            self.send_request_with_timeout(
                "session/new",
                NewSessionRequest::new(cwd.to_path_buf())
                    .mcp_servers(mcp_servers)
                    .additional_directories(additional_directories),
                std::time::Duration::from_secs(60),
            )
            .await?
        };
        *self.config_options.write().await = result
            .get("configOptions")
            .cloned()
            .unwrap_or(serde_json::json!([]));
        // Preserve advertised modes/current mode generically. Absent means
        // the agent has no legacy mode support. The response snapshot is
        // authoritative; live `current_mode_update` notifications that
        // arrive during replay merge into it on arrival.
        *self.session_modes.write().await = result
            .get("modes")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        if let Some(saved) = saved {
            if let Some(returned) = result["sessionId"].as_str() {
                anyhow::ensure!(
                    returned == saved,
                    "Agent returned a different session identity on resume"
                );
            }
            Ok(saved.to_owned())
        } else {
            Ok(result["sessionId"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Agent omitted sessionId"))?
                .to_owned())
        }
    }

    pub async fn set_config(
        &self,
        session_id: &SessionId,
        id: &str,
        value: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        // A stale saved value that no longer matches the agent's advertised
        // options is a genuine rejection: the user must reset that option.
        // Map the local validation failure to the typed rejection.
        if let Err(error) = validate_config_value(&*self.config_options.read().await, id, &value) {
            return Err(anyhow::Error::new(SavedConfigRejected {
                option_id: id.to_owned(),
                message: format!(
                    "Saved ACP option {id} could not be reapplied; reset that option before reconnecting ({error})"
                ),
            }));
        }
        // Stable wire shape: `select` sends a bare value id, `boolean`
        // requires the `type: "boolean"` discriminator. Build the params
        // from the advertised option kind so strict agents accept both.
        let is_boolean = self
            .config_options
            .read()
            .await
            .as_array()
            .and_then(|a| {
                a.iter()
                    .find(|o| o.get("id").and_then(|v| v.as_str()) == Some(id))
            })
            .and_then(|o| o.get("type").and_then(|v| v.as_str()))
            == Some("boolean");
        let params = if is_boolean {
            serde_json::json!({"sessionId":session_id,"configId":id,"type":"boolean","value":value})
        } else {
            serde_json::json!({"sessionId":session_id,"configId":id,"value":value})
        };
        let result = self
            .send_request_with_timeout(
                "session/set_config_option",
                params,
                std::time::Duration::from_secs(30),
            )
            .await
            .map_err(|error| {
                // Only an agent RPC rejection is a saved-config rejection.
                // Transport disconnects, timeouts (already protocol-cancelled
                // via `$/cancel_request`), writer failures, and dropped
                // response channels are transient and must keep the ordinary
                // Retry path.
                let message = error.to_string();
                if message.starts_with("RPC error") {
                    anyhow::Error::new(SavedConfigRejected {
                        option_id: id.to_owned(),
                        message: format!(
                            "Saved ACP option {id} could not be reapplied; reset that option before reconnecting ({message})"
                        ),
                    })
                } else {
                    error
                }
            })?;
        let options = result
            .get("configOptions")
            .filter(|v| v.is_array())
            .ok_or_else(|| anyhow::anyhow!("Agent omitted authoritative configOptions"))?
            .clone();
        *self.config_options.write().await = options.clone();
        Ok(options)
    }

    pub async fn available_commands_snapshot(&self) -> serde_json::Value {
        self.available_commands.read().await.clone()
    }

    pub async fn session_modes_snapshot(&self) -> serde_json::Value {
        self.session_modes.read().await.clone()
    }

    pub async fn agent_info_snapshot(&self) -> serde_json::Value {
        self.agent_info.read().await.clone()
    }

    pub async fn auth_methods_snapshot(&self) -> serde_json::Value {
        self.auth_methods.read().await.clone()
    }

    pub async fn usage_snapshot(&self) -> serde_json::Value {
        self.last_usage.read().await.clone()
    }

    /// Capability-gated legacy `session/set_mode`. Fails explicitly when the
    /// agent never advertised modes instead of sending speculatively.
    pub async fn set_mode(&self, session_id: &SessionId, mode_id: &str) -> anyhow::Result<()> {
        let has_modes = {
            let modes = self.session_modes.read().await;
            match &*modes {
                serde_json::Value::Null => false,
                v => {
                    v.get("available_modes")
                        .and_then(|a| a.as_array())
                        .is_some_and(|a| !a.is_empty())
                        || v.get("availableModes")
                            .and_then(|a| a.as_array())
                            .is_some_and(|a| !a.is_empty())
                }
            }
        };
        anyhow::ensure!(has_modes, "Agent does not advertise session modes");
        self.send_request_with_timeout(
            "session/set_mode",
            serde_json::json!({"sessionId":session_id,"modeId":mode_id}),
            std::time::Duration::from_secs(30),
        )
        .await?;
        Ok(())
    }

    /// Capability-gated `session/delete` for agent-owned remote history.
    /// Batey's own chat deletion stays separate; this only removes the
    /// agent's remote session record.
    pub async fn delete_remote_session(&self, session_id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.capabilities
                .read()
                .await
                .pointer("/sessionCapabilities/delete")
                .is_some_and(|v| v.is_object()),
            "Agent does not advertise session/delete"
        );
        self.send_request_with_timeout(
            "session/delete",
            serde_json::json!({"sessionId":session_id}),
            std::time::Duration::from_secs(30),
        )
        .await?;
        Ok(())
    }

    /// Generic stable `$/cancel_request` for a single outstanding JSON-RPC
    /// request. Distinct from `session/cancel`, which cancels a whole turn.
    /// The receiver may ignore `$`-prefixed notifications; never treat a
    /// failure here as a turn failure.
    pub async fn cancel_request(&self, request_id: i64) {
        let _ = self
            .send_notification(
                "$/cancel_request",
                serde_json::json!({"requestId":request_id}),
            )
            .await;
    }

    pub async fn close_session(&self, session_id: &SessionId) {
        if self
            .capabilities
            .read()
            .await
            .pointer("/sessionCapabilities/close")
            .is_some_and(|v| v.is_object())
        {
            let _ = self
                .send_request_with_timeout(
                    "session/close",
                    serde_json::json!({"sessionId":session_id}),
                    std::time::Duration::from_secs(5),
                )
                .await;
        }
    }

    pub async fn supports_resume(&self) -> bool {
        let capabilities = self.capabilities.read().await;
        capabilities["loadSession"] == true
            || capabilities
                .pointer("/sessionCapabilities/resume")
                .is_some_and(|value| value.is_object())
    }

    pub async fn list_sessions(
        &self,
        cwd: &Path,
        cursor: Option<String>,
    ) -> anyhow::Result<serde_json::Value> {
        anyhow::ensure!(
            self.capabilities
                .read()
                .await
                .pointer("/sessionCapabilities/list")
                .is_some_and(|v| v.is_object()),
            "Agent does not advertise session/list"
        );
        self.send_request_with_timeout(
            "session/list",
            serde_json::json!({"cwd":cwd,"cursor":cursor}),
            std::time::Duration::from_secs(30),
        )
        .await
    }

    pub async fn prompt(
        &self,
        session_id: &SessionId,
        content: &[ContentBlock],
        user_message_id: &str,
    ) -> anyhow::Result<PromptResponse> {
        // Stable v1 has no dedicated user-message-id field, so the identity
        // travels on the generic `_meta` extension point under Batey's
        // own namespace. Agents that do not understand it ignore it, as the
        // spec requires; agents that echo it let the turn correlate the
        // response with the durable user message.
        crate::content::validate_prompt(content)?;
        self.validate_prompt_capabilities(content).await?;
        let req = PromptRequest::new(session_id.clone(), content.to_vec())
            .meta(user_message_meta(user_message_id));
        let result = self.send_request("session/prompt", req).await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Reject non-text prompt blocks unless the initialized stable-v1 agent
    /// capabilities explicitly permit them. Resource links are baseline ACP
    /// content and do not need a prompt capability.
    pub async fn validate_prompt_capabilities(
        &self,
        content: &[ContentBlock],
    ) -> anyhow::Result<()> {
        let capabilities = self.prompt_capabilities.read().await;
        validate_prompt_capabilities(&capabilities, content)
    }

    pub async fn cancel(&self, session_id: &SessionId) -> anyhow::Result<()> {
        let notif = CancelNotification::new(session_id.clone());
        self.send_notification("session/cancel", notif).await
    }

    pub fn callback_handler(&self) -> &Arc<CallbackHandler> {
        &self.callback_handler
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    pub fn root_pid(&self) -> Option<u32> {
        self.child_root_pid
    }

    pub async fn terminate(&self) {
        self.connected.store(false, Ordering::SeqCst);
        start_kill_child(&self.child, self.child_root_pid).await;
    }

    pub async fn shutdown(&self) {
        self.callback_handler.shutdown().await;
        self.connected.store(false, Ordering::SeqCst);
        let _ = self.writer_tx.try_send(WriterMsg::Shutdown);
        kill_child(&self.child, self.child_root_pid).await;

        let writer = self.writer_handle.lock().await.take();
        let reader = self.reader_handle.lock().await.take();
        let stderr = self.stderr_handle.lock().await.take();
        let wait = self.wait_handle.lock().await.take();
        if let Some(h) = &writer {
            h.abort();
        }
        if let Some(h) = &reader {
            h.abort();
        }
        if let Some(h) = &stderr {
            h.abort();
        }
        if let Some(h) = &wait {
            h.abort();
        }
        if let Some(h) = wait {
            let _ = h.await;
        }
        kill_child(&self.child, self.child_root_pid).await;
        if let Some(h) = writer {
            let _ = h.await;
        }
        if let Some(h) = reader {
            let _ = h.await;
        }
        if let Some(h) = stderr {
            let _ = h.await;
        }
        fail_pending_requests(&self.pending, "ACP client shutdown".to_string()).await;
    }
}

fn validate_prompt_capabilities(
    capabilities: &agent_client_protocol_schema::PromptCapabilities,
    content: &[ContentBlock],
) -> anyhow::Result<()> {
    for block in content {
        match block {
            ContentBlock::Image(_) => anyhow::ensure!(
                capabilities.image,
                "Agent does not advertise image prompt capability"
            ),
            ContentBlock::Audio(_) => anyhow::ensure!(
                capabilities.audio,
                "Agent does not advertise audio prompt capability"
            ),
            ContentBlock::Resource(_) => anyhow::ensure!(
                capabilities.embedded_context,
                "Agent does not advertise embedded context prompt capability"
            ),
            _ => {}
        }
    }
    Ok(())
}

/// Converts Batey's typed durable records into the stable-v1 wire variants.
/// ACP-over-ACP is deliberately absent: it is not stable v1.
pub fn configured_mcp_servers(
    values: &[crate::store::McpServerConfig],
) -> anyhow::Result<Vec<McpServer>> {
    values
        .iter()
        .map(|value| {
            anyhow::ensure!(!value.name.trim().is_empty(), "Invalid stored MCP server");
            match value.transport {
                crate::store::McpTransport::Http => Ok(McpServer::Http(
                    McpServerHttp::new(
                        &value.name,
                        value
                            .url
                            .as_deref()
                            .ok_or_else(|| anyhow::anyhow!("Invalid stored HTTP MCP server"))?,
                    )
                    .headers(
                        value
                            .secrets
                            .iter()
                            .map(|s| {
                                agent_client_protocol_schema::HttpHeader::new(&s.name, &s.value)
                            })
                            .collect(),
                    ),
                )),
                crate::store::McpTransport::Sse => Ok(McpServer::Sse(
                    McpServerSse::new(
                        &value.name,
                        value
                            .url
                            .as_deref()
                            .ok_or_else(|| anyhow::anyhow!("Invalid stored SSE MCP server"))?,
                    )
                    .headers(
                        value
                            .secrets
                            .iter()
                            .map(|s| {
                                agent_client_protocol_schema::HttpHeader::new(&s.name, &s.value)
                            })
                            .collect(),
                    ),
                )),
                crate::store::McpTransport::Stdio => Ok(McpServer::Stdio(
                    McpServerStdio::new(
                        &value.name,
                        value
                            .command
                            .as_deref()
                            .ok_or_else(|| anyhow::anyhow!("Invalid stored stdio MCP server"))?,
                    )
                    .args(value.args.clone())
                    .env(
                        value
                            .secrets
                            .iter()
                            .map(|s| {
                                agent_client_protocol_schema::EnvVariable::new(&s.name, &s.value)
                            })
                            .collect(),
                    ),
                )),
            }
        })
        .collect()
}

/// Stable ACP v1 advertises additional directories with an object. Both an
/// absent field and `null` explicitly mean unsupported.
pub fn supports_additional_directories(capabilities: &serde_json::Value) -> bool {
    capabilities
        .pointer("/sessionCapabilities/additionalDirectories")
        .is_some_and(serde_json::Value::is_object)
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        // Process-tree teardown is asymmetric per OS:
        //
        // - Windows: JobOwnedChild::drop closes the job HANDLE, which fires
        //   KILL_ON_JOB_CLOSE and kills every process in the tree.
        //
        // - Unix: tokio's kill_on_drop only SIGKILLs the *direct* child, not
        //   the process group. The actual subtree teardown happens through the
        //   explicit `c.start_kill()` call below (process-wrap's
        //   ProcessGroupChild::start_kill = killpg(-pgid, SIGKILL)). If we
        //   can't acquire the child lock — extremely rare since tasks are
        //   aborted right above — the killpg never fires from here and we fall
        //   back to whatever the inner Child does on drop (direct-child kill).
        //   Force-killing batey itself on Unix (no Drop runs) is a known
        //   limitation; we have no JobObject equivalent.
        self.connected.store(false, Ordering::SeqCst);
        if let Ok(mut handle) = self.writer_handle.try_lock() {
            if let Some(h) = handle.take() {
                h.abort();
            }
        }
        if let Ok(mut handle) = self.reader_handle.try_lock() {
            if let Some(h) = handle.take() {
                h.abort();
            }
        }
        if let Ok(mut handle) = self.stderr_handle.try_lock() {
            if let Some(h) = handle.take() {
                h.abort();
            }
        }
        if let Ok(mut handle) = self.wait_handle.try_lock() {
            if let Some(h) = handle.take() {
                h.abort();
            }
        }
        if let Ok(mut child) = self.child.try_lock() {
            if let Some(mut c) = child.take() {
                if let Err(e) = c.start_kill() {
                    tracing::warn!(root_pid = ?self.child_root_pid, error = %e, "AcpClient::drop: start_kill failed");
                }
            }
        }
    }
}

async fn start_kill_child(child: &SharedChild, root_pid: Option<u32>) {
    let mut guard = child.lock().await;
    if let Some(c) = guard.as_mut() {
        if let Err(e) = c.start_kill() {
            tracing::warn!(?root_pid, error = %e, "start_kill_child: start_kill failed");
        }
    }
}

async fn fail_pending_requests(
    pending: &Arc<Mutex<HashMap<i64, oneshot::Sender<ResponseResult>>>>,
    message: String,
) {
    let mut pending = pending.lock().await;
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(anyhow::anyhow!(message.clone())));
    }
}

async fn writer_task(
    mut stdin: tokio::process::ChildStdin,
    mut rx: mpsc::Receiver<WriterMsg>,
    connected: Arc<AtomicBool>,
    child: SharedChild,
    child_root_pid: Option<u32>,
    chat_id: String,
    agent_id: String,
) {
    let mut needs_child_cleanup = false;
    while let Some(msg) = rx.recv().await {
        match msg {
            WriterMsg::Line(line) => {
                let data = format!("{}\n", line);
                if let Err(error) = stdin.write_all(data.as_bytes()).await {
                    tracing::warn!(
                        agent_id = %agent_id,
                        chat_id = %chat_id,
                        pid = ?child_root_pid,
                        error = %error,
                        "ACP transport write failed"
                    );
                    needs_child_cleanup = true;
                    break;
                }
                if let Err(error) = stdin.flush().await {
                    tracing::warn!(
                        agent_id = %agent_id,
                        chat_id = %chat_id,
                        pid = ?child_root_pid,
                        error = %error,
                        "ACP transport flush failed"
                    );
                    needs_child_cleanup = true;
                    break;
                }
            }
            WriterMsg::Shutdown => {
                let _ = stdin.shutdown().await;
                break;
            }
        }
    }

    connected.store(false, Ordering::SeqCst);
    if needs_child_cleanup {
        kill_child(&child, child_root_pid).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn reader_task(
    mut stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<ResponseResult>>>>,
    connected: Arc<AtomicBool>,
    callback_handler: Arc<CallbackHandler>,
    writer_tx: mpsc::Sender<WriterMsg>,
    child: SharedChild,
    child_root_pid: Option<u32>,
    event_log: Arc<EventLog>,
    session_id: String,
    agent_name: String,
    config_options: Arc<tokio::sync::RwLock<serde_json::Value>>,
    available_commands: Arc<tokio::sync::RwLock<serde_json::Value>>,
    session_modes: Arc<tokio::sync::RwLock<serde_json::Value>>,
    last_usage: Arc<tokio::sync::RwLock<serde_json::Value>>,
    replaying: Arc<AtomicBool>,
    store: Option<Arc<crate::store::Store>>,
    stderr_tail: StderrTail,
) {
    let request_semaphore = Arc::new(tokio::sync::Semaphore::new(16));
    let mut line = String::new();
    loop {
        line.clear();
        match stdout.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                let msg: IncomingMessage = match serde_json::from_str(trimmed) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(
                            agent_id = %agent_name,
                            chat_id = %session_id,
                            error = %e,
                            "ACP agent sent invalid JSON"
                        );
                        continue;
                    }
                };

                match msg.classify() {
                    IncomingKind::Response { id, result, error } => {
                        let numeric_id = match &id {
                            agent_client_protocol_schema::RequestId::Number(n) => *n,
                            _ => continue,
                        };
                        let mut pending = pending.lock().await;
                        pending.retain(|_, tx| !tx.is_closed());
                        if let Some(tx) = pending.remove(&numeric_id) {
                            let response = if let Some(err) = error {
                                // Keep the code, so a caller can recognize a
                                // stable condition such as `auth_required`.
                                Err(anyhow::Error::new(AcpRpcError {
                                    code: err.code,
                                    message: err.message,
                                    data: err.data,
                                }))
                            } else {
                                Ok(result.unwrap_or(serde_json::Value::Null))
                            };
                            let _ = tx.send(response);
                        }
                    }
                    IncomingKind::Notification { method, params } => {
                        if method == "session/update" {
                            // Dynamic snapshots update memory even during ACP
                            // replay so the state stays queryable after
                            // reconnect. Durable events emit only for live
                            // updates: replaying an authoritative snapshot
                            // would duplicate history on every reconnect.
                            let kind = params
                                .pointer("/update/sessionUpdate")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            if kind == "config_option_update" {
                                if let Some(options) = params
                                    .pointer("/update/configOptions")
                                    .filter(|v| v.is_array())
                                {
                                    *config_options.write().await = options.clone();
                                    if !replaying.load(Ordering::SeqCst) {
                                        if let Err(error) = event_log.append(
                                            &session_id,
                                            &agent_name,
                                            EventPayload::ConfigOptions {
                                                options: options.clone(),
                                            },
                                        ) {
                                            tracing::error!(%error, "Stopping ACP reader after event persistence failure");
                                            break;
                                        }
                                    }
                                }
                            } else if kind == "available_commands_update" {
                                if let Ok(notif) =
                                    serde_json::from_value::<SessionNotification>(params.clone())
                                {
                                    if let SessionUpdate::AvailableCommandsUpdate(u) = &notif.update
                                    {
                                        let cmds = serde_json::to_value(&u.available_commands)
                                            .unwrap_or(serde_json::json!([]));
                                        *available_commands.write().await = cmds.clone();
                                        if !replaying.load(Ordering::SeqCst) {
                                            if let Err(error) = event_log.append(
                                                &session_id,
                                                &agent_name,
                                                EventPayload::AvailableCommands { commands: cmds },
                                            ) {
                                                tracing::error!(%error, "Stopping ACP reader after event persistence failure");
                                                break;
                                            }
                                        }
                                    }
                                }
                                if replaying.load(Ordering::SeqCst) {
                                    continue;
                                }
                                // Already handled above; avoid double emit.
                                continue;
                            } else if kind == "current_mode_update" {
                                if let Ok(notif) =
                                    serde_json::from_value::<SessionNotification>(params.clone())
                                {
                                    if let SessionUpdate::CurrentModeUpdate(u) = &notif.update {
                                        let new_id = u.current_mode_id.to_string();
                                        // Merge into stored SessionModeState so
                                        // available modes survive the update.
                                        let mut guard = session_modes.write().await;
                                        let mut state = (*guard).clone();
                                        if state.is_null() {
                                            state = serde_json::json!({
                                                "current_mode_id": new_id,
                                                "available_modes": []
                                            });
                                        } else if let Some(obj) = state.as_object_mut() {
                                            obj.insert(
                                                "current_mode_id".to_string(),
                                                serde_json::Value::String(new_id.clone()),
                                            );
                                            // Accept both wire casings for the
                                            // current id when merging.
                                            obj.insert(
                                                "currentModeId".to_string(),
                                                serde_json::Value::String(new_id),
                                            );
                                        }
                                        *guard = state.clone();
                                        drop(guard);
                                        if !replaying.load(Ordering::SeqCst) {
                                            if let Err(error) = event_log.append(
                                                &session_id,
                                                &agent_name,
                                                EventPayload::SessionModes { state },
                                            ) {
                                                tracing::error!(%error, "Stopping ACP reader after event persistence failure");
                                                break;
                                            }
                                        }
                                    }
                                }
                                if replaying.load(Ordering::SeqCst) {
                                    continue;
                                }
                                continue;
                            } else if kind == "usage_update" {
                                if let Ok(notif) =
                                    serde_json::from_value::<SessionNotification>(params.clone())
                                {
                                    if let SessionUpdate::UsageUpdate(u) = &notif.update {
                                        let snapshot = serde_json::json!({
                                            "used": u.used,
                                            "size": u.size,
                                            "cost": u.cost,
                                        });
                                        *last_usage.write().await = snapshot;
                                        if !replaying.load(Ordering::SeqCst) {
                                            let (amount, currency) = u
                                                .cost
                                                .as_ref()
                                                .map(|c| (Some(c.amount), Some(c.currency.clone())))
                                                .unwrap_or((None, None));
                                            if let Err(error) = event_log.append(
                                                &session_id,
                                                &agent_name,
                                                EventPayload::UsageUpdate {
                                                    used: u.used,
                                                    size: u.size,
                                                    cost_amount: amount,
                                                    cost_currency: currency,
                                                },
                                            ) {
                                                tracing::error!(%error, "Stopping ACP reader after event persistence failure");
                                                break;
                                            }
                                        }
                                    }
                                }
                                if replaying.load(Ordering::SeqCst) {
                                    continue;
                                }
                                continue;
                            }
                            if replaying.load(Ordering::SeqCst) {
                                continue;
                            }
                            if let Ok(notif) = serde_json::from_value::<SessionNotification>(params)
                            {
                                if let Err(error) = handle_session_update(
                                    &event_log,
                                    &store,
                                    &session_id,
                                    &agent_name,
                                    &notif.update,
                                    &available_commands,
                                    &session_modes,
                                    &last_usage,
                                )
                                .await
                                {
                                    tracing::error!(%error, "Stopping ACP reader after event persistence failure");
                                    break;
                                }
                            }
                        } else if method == CLIENT_METHOD_NAMES.elicitation_complete
                            || method == "elicitation/complete"
                        {
                            // Agent signals a URL elicitation finished
                            // out-of-band. Clear pending state and tell the
                            // UI to dismiss it. Never log secret material.
                            if let Ok(notif) = serde_json::from_value::<
                                agent_client_protocol_schema::CompleteElicitationNotification,
                            >(params)
                            {
                                let eid = notif.elicitation_id.to_string();
                                callback_handler.complete_elicitation(&eid).await;
                                let _ = event_log.append(
                                    &session_id,
                                    &agent_name,
                                    EventPayload::ElicitationComplete {
                                        elicitation_id: eid,
                                    },
                                );
                            }
                        } else if method.starts_with("$/") {
                            // Protocol-level notifications are advisory.
                            // `$`-prefixed methods may be ignored per spec.
                            continue;
                        }
                    }
                    IncomingKind::Request { id, method, params } => {
                        let handler = callback_handler.clone();
                        let tx = writer_tx.clone();
                        let sem = request_semaphore.clone();
                        tokio::spawn(async move {
                            let _permit = match sem.acquire().await {
                                Ok(p) => p,
                                Err(_) => return,
                            };
                            let response =
                                handle_agent_request(&method, params, &id, &handler).await;

                            let resp = JsonRpcResponse {
                                jsonrpc: "2.0",
                                id,
                                result: response.as_ref().ok().cloned(),
                                error: response.err().map(|e| protocol::JsonRpcError {
                                    code: -32000,
                                    message: e.to_string(),
                                    data: None,
                                }),
                            };

                            if let Ok(line) = serde_json::to_string(&resp) {
                                let _ = tx.send(WriterMsg::Line(line)).await;
                            }
                        });
                    }
                    IncomingKind::Invalid => {
                        tracing::warn!(
                            agent_id = %agent_name,
                            chat_id = %session_id,
                            "ACP agent sent an invalid JSON-RPC message"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    agent_id = %agent_name,
                    chat_id = %session_id,
                    error = %e,
                    "ACP transport read failed"
                );
                break;
            }
        }
    }

    let unexpected = connected.swap(false, Ordering::SeqCst);
    if unexpected {
        tracing::warn!(
            agent_id = %agent_name,
            chat_id = %session_id,
            pid = ?child_root_pid,
            "ACP transport closed unexpectedly"
        );
    }
    let _ = event_log.append(
        &session_id,
        &agent_name,
        EventPayload::StateChange {
            process: "DEAD".into(),
            turn: "IDLE".into(),
        },
    );
    kill_child(&child, child_root_pid).await;
    let mut message = format!("ACP agent connection closed for {}", agent_name);
    if let Some(snippet) = stderr_tail.snippet() {
        message = format!("{message} (recent stderr: {snippet})");
    }
    fail_pending_requests(&pending, message).await;
}

/// Namespace for Batey's generic `_meta` extensions. Extension keys
/// never drive protocol behavior; agents that do not understand them must
/// ignore them per the ACP extensibility rules.
pub const BATEY_META_KEY: &str = "batey";
/// User-message identity key inside Batey's `_meta` namespace.
pub const USER_MESSAGE_ID_META_KEY: &str = "userMessageId";

/// Builds the generic `_meta` carrying one user-message identity.
pub fn user_message_meta(user_message_id: &str) -> agent_client_protocol_schema::Meta {
    let mut inner = serde_json::Map::new();
    inner.insert(
        USER_MESSAGE_ID_META_KEY.to_string(),
        serde_json::Value::String(user_message_id.to_string()),
    );
    let mut meta = agent_client_protocol_schema::Meta::new();
    meta.insert(BATEY_META_KEY.to_string(), serde_json::Value::Object(inner));
    meta
}

/// Reads a user-message identity an agent echoed back on the generic
/// `_meta` extension point. Returns `None` for agents that omit it, which
/// normal operation must always tolerate.
pub fn echoed_user_message_id(meta: &Option<agent_client_protocol_schema::Meta>) -> Option<String> {
    meta.as_ref()?
        .get(BATEY_META_KEY)?
        .get(USER_MESSAGE_ID_META_KEY)?
        .as_str()
        .map(|s| s.to_string())
}

pub fn validate_config_value(
    options: &serde_json::Value,
    id: &str,
    value: &serde_json::Value,
) -> anyhow::Result<()> {
    let option = options
        .as_array()
        .and_then(|a| a.iter().find(|o| o["id"] == id))
        .ok_or_else(|| anyhow::anyhow!("Unknown ACP config option"))?;
    fn contains(options: &serde_json::Value, value: &serde_json::Value) -> bool {
        options.as_array().is_some_and(|a| {
            a.iter()
                .any(|o| o.get("value") == Some(value) || contains(&o["options"], value))
        })
    }
    let valid = match option["type"].as_str() {
        Some("select") => contains(&option["options"], value),
        Some("boolean") => value.is_boolean(),
        _ => false,
    };
    anyhow::ensure!(valid, "Unsupported ACP config value");
    Ok(())
}

async fn handle_agent_request(
    method: &str,
    params: serde_json::Value,
    id: &agent_client_protocol_schema::RequestId,
    handler: &CallbackHandler,
) -> Result<serde_json::Value, String> {
    match method {
        m if m == CLIENT_METHOD_NAMES.session_request_permission => {
            let req: RequestPermissionRequest =
                serde_json::from_value(params).map_err(|e| e.to_string())?;
            let resp = handler.handle_request_permission(req).await;
            serde_json::to_value(resp).map_err(|e| e.to_string())
        }
        m if m == CLIENT_METHOD_NAMES.elicitation_create => {
            let req: agent_client_protocol_schema::CreateElicitationRequest =
                serde_json::from_value(params).map_err(|e| e.to_string())?;
            let rpc_id = match id {
                agent_client_protocol_schema::RequestId::Number(n) => n.to_string(),
                agent_client_protocol_schema::RequestId::Str(s) => s.to_string(),
                agent_client_protocol_schema::RequestId::Null => "null".to_string(),
            };
            let resp = handler.handle_elicitation(rpc_id, req).await;
            serde_json::to_value(resp).map_err(|e| e.to_string())
        }
        m if m == CLIENT_METHOD_NAMES.fs_read_text_file => {
            let req: ReadTextFileRequest =
                serde_json::from_value(params).map_err(|e| e.to_string())?;
            match handler.handle_read_file(req).await {
                Ok(resp) => serde_json::to_value(resp).map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            }
        }
        m if m == CLIENT_METHOD_NAMES.fs_write_text_file => {
            let req: WriteTextFileRequest =
                serde_json::from_value(params).map_err(|e| e.to_string())?;
            match handler.handle_write_file(req).await {
                Ok(resp) => serde_json::to_value(resp).map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            }
        }
        m if m == CLIENT_METHOD_NAMES.terminal_create => {
            let req: CreateTerminalRequest =
                serde_json::from_value(params).map_err(|e| e.to_string())?;
            match handler.handle_create_terminal(req).await {
                Ok(resp) => serde_json::to_value(resp).map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            }
        }
        m if m == CLIENT_METHOD_NAMES.terminal_output => {
            let req: TerminalOutputRequest =
                serde_json::from_value(params).map_err(|e| e.to_string())?;
            match handler.handle_terminal_output(req).await {
                Ok(resp) => serde_json::to_value(resp).map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            }
        }
        m if m == CLIENT_METHOD_NAMES.terminal_release => {
            let req: ReleaseTerminalRequest =
                serde_json::from_value(params).map_err(|e| e.to_string())?;
            match handler.handle_release_terminal(req).await {
                Ok(resp) => serde_json::to_value(resp).map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            }
        }
        m if m == CLIENT_METHOD_NAMES.terminal_wait_for_exit => {
            let req: WaitForTerminalExitRequest =
                serde_json::from_value(params).map_err(|e| e.to_string())?;
            match handler.handle_wait_for_terminal_exit(req).await {
                Ok(resp) => serde_json::to_value(resp).map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            }
        }
        m if m == CLIENT_METHOD_NAMES.terminal_kill => {
            let req: KillTerminalRequest =
                serde_json::from_value(params).map_err(|e| e.to_string())?;
            match handler.handle_kill_terminal(req).await {
                Ok(resp) => serde_json::to_value(resp).map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            }
        }
        _ => Err(format!("Unknown method: {}", method)),
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_session_update(
    event_log: &EventLog,
    store: &Option<Arc<crate::store::Store>>,
    session_id: &str,
    agent_name: &str,
    update: &SessionUpdate,
    available_commands: &Arc<tokio::sync::RwLock<serde_json::Value>>,
    session_modes: &Arc<tokio::sync::RwLock<serde_json::Value>>,
    last_usage: &Arc<tokio::sync::RwLock<serde_json::Value>>,
) -> anyhow::Result<()> {
    let payload = match update {
        SessionUpdate::SessionInfoUpdate(info) => {
            // Preserve title for the chat row plus generic updated_at.
            // Unknown _meta stays opaque; no provider interpretation.
            let title_opt = match &info.title {
                ::agent_client_protocol_schema::MaybeUndefined::Value(t) => {
                    let trimmed = t.trim();
                    if !trimmed.is_empty() && trimmed.len() <= 200 {
                        if let Some(st) = store {
                            let mut changed = false;
                            let _ = st.update_chat(session_id, |c| {
                                if !c.title_overridden && c.title != trimmed {
                                    c.title = trimmed.to_string();
                                    changed = true;
                                }
                            });
                            if changed {
                                event_log.append(
                                    session_id,
                                    agent_name,
                                    EventPayload::MetadataChanged {},
                                )?;
                            }
                        }
                        Some(trimmed.to_string())
                    } else {
                        None
                    }
                }
                _ => None,
            };
            let updated_at_opt = match &info.updated_at {
                ::agent_client_protocol_schema::MaybeUndefined::Value(t) => {
                    let trimmed = t.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    }
                }
                _ => None,
            };
            // Preserve the generic `_meta` opaquely without interpreting
            // it. Unknown agent metadata stays available to surfaces that
            // can represent it instead of being silently dropped.
            let meta_opt = serde_json::to_value(&info.meta)
                .ok()
                .filter(|v| !v.is_null());
            // Emit generic session metadata even when the title was
            // ignored for the chat row, so updated_at is not silently
            // dropped. Skip entirely when the agent sent nothing useful.
            if title_opt.is_none() && updated_at_opt.is_none() && meta_opt.is_none() {
                // Still check Null-clear: if either was explicit Null, emit
                // a clear marker so the UI can drop stale metadata.
                let title_is_null = matches!(
                    &info.title,
                    ::agent_client_protocol_schema::MaybeUndefined::Null
                );
                let updated_is_null = matches!(
                    &info.updated_at,
                    ::agent_client_protocol_schema::MaybeUndefined::Null
                );
                if !title_is_null && !updated_is_null {
                    return Ok(());
                }
            }
            EventPayload::SessionInfo {
                title: title_opt,
                updated_at: updated_at_opt,
                meta: meta_opt,
            }
        }
        SessionUpdate::UserMessageChunk(_) => {
            // Agent-reflected user chunks must not duplicate Batey's
            // locally persisted UserMessage during replay/resume. The local
            // echo in admit_turn is authoritative; drop the reflection.
            return Ok(());
        }
        SessionUpdate::AgentMessageChunk(chunk) => {
            crate::content::validate_durable(std::slice::from_ref(&chunk.content))?;
            let text = match &chunk.content {
                ContentBlock::Text(t) => t.text.clone(),
                _ => String::new(),
            };
            EventPayload::MessageChunk {
                text,
                content: vec![chunk.content.clone()],
                message_id: chunk.message_id.as_ref().map(|m| m.to_string()),
            }
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            crate::content::validate_durable(std::slice::from_ref(&chunk.content))?;
            let text = match &chunk.content {
                ContentBlock::Text(t) => t.text.clone(),
                _ => String::new(),
            };
            EventPayload::ThoughtChunk {
                text,
                content: vec![chunk.content.clone()],
                message_id: chunk.message_id.as_ref().map(|m| m.to_string()),
            }
        }
        SessionUpdate::ToolCall(tc) => {
            validate_tool_call_content(&tc.content)?;
            let title = extract_tool_call_title(Some(&tc.title), tc.raw_input.as_ref());
            let kind = serde_json::to_value(tc.kind)
                .ok()
                .and_then(|v| v.as_str().map(ToOwned::to_owned))
                .or_else(|| Some(format!("{:?}", tc.kind).to_lowercase()));
            let parent_id = extract_parent_id(tc.meta.as_ref());
            let locations = if tc.locations.is_empty() {
                None
            } else {
                serde_json::to_value(&tc.locations).ok()
            };
            EventPayload::ToolCall {
                id: tc.tool_call_id.to_string(),
                title,
                status: "in_progress".to_string(),
                kind,
                parent_id,
                locations,
                content: (!tc.content.is_empty())
                    .then(|| serde_json::to_value(&tc.content))
                    .transpose()?,
            }
        }
        SessionUpdate::ToolCallUpdate(tcu) => {
            if let Some(items) = &tcu.fields.content {
                validate_tool_call_content(items)?;
            }
            let title = tcu
                .fields
                .title
                .as_deref()
                .map(|t| extract_tool_call_title(Some(t), tcu.fields.raw_input.as_ref()))
                .or_else(|| {
                    tcu.fields
                        .raw_input
                        .as_ref()
                        .map(|i| extract_tool_call_title(None, Some(i)))
                });
            let kind = tcu.fields.kind.map(|k| {
                serde_json::to_value(k)
                    .ok()
                    .and_then(|v| v.as_str().map(ToOwned::to_owned))
                    .unwrap_or_else(|| format!("{:?}", k).to_lowercase())
            });
            let locations = tcu
                .fields
                .locations
                .as_ref()
                .filter(|l| !l.is_empty())
                .and_then(|l| serde_json::to_value(l).ok());
            EventPayload::ToolCallUpdate {
                id: tcu.tool_call_id.to_string(),
                status: serialize_optional_enum(&tcu.fields.status),
                title,
                kind,
                output: clean_tool_output(format_tool_call_output(&tcu.fields)),
                locations,
                content: tcu
                    .fields
                    .content
                    .as_ref()
                    .map(serde_json::to_value)
                    .transpose()?,
            }
        }
        SessionUpdate::Plan(plan) => {
            let entries = plan
                .entries
                .iter()
                .map(|e| crate::events::PlanEntry {
                    content: e.content.clone(),
                    status: format!("{:?}", e.status),
                })
                .collect();
            EventPayload::Plan { entries }
        }
        SessionUpdate::AvailableCommandsUpdate(u) => {
            let cmds = serde_json::to_value(&u.available_commands).unwrap_or(serde_json::json!([]));
            *available_commands.write().await = cmds.clone();
            EventPayload::AvailableCommands { commands: cmds }
        }
        SessionUpdate::CurrentModeUpdate(u) => {
            let new_id = u.current_mode_id.to_string();
            let mut guard = session_modes.write().await;
            let mut state = (*guard).clone();
            if state.is_null() {
                state = serde_json::json!({
                    "current_mode_id": new_id,
                    "available_modes": []
                });
            } else if let Some(obj) = state.as_object_mut() {
                obj.insert(
                    "current_mode_id".to_string(),
                    serde_json::Value::String(new_id.clone()),
                );
                obj.insert(
                    "currentModeId".to_string(),
                    serde_json::Value::String(new_id),
                );
            }
            *guard = state.clone();
            drop(guard);
            EventPayload::SessionModes { state }
        }
        SessionUpdate::ConfigOptionUpdate(_) => {
            // Owned by reader_task: it updates the snapshot memory and
            // emits the durable event (live updates only, never during ACP
            // replay). Handled here as a no-op so live notifications are
            // never persisted twice.
            return Ok(());
        }
        SessionUpdate::UsageUpdate(u) => {
            let snapshot = serde_json::json!({
                "used": u.used,
                "size": u.size,
                "cost": u.cost,
            });
            *last_usage.write().await = snapshot;
            let (amount, currency) = u
                .cost
                .as_ref()
                .map(|c| (Some(c.amount), Some(c.currency.clone())))
                .unwrap_or((None, None));
            EventPayload::UsageUpdate {
                used: u.used,
                size: u.size,
                cost_amount: amount,
                cost_currency: currency,
            }
        }
        _ => return Ok(()),
    };

    event_log.append(session_id, agent_name, payload)?;
    Ok(())
}

fn validate_tool_call_content(items: &[ToolCallContent]) -> anyhow::Result<()> {
    for item in items {
        if let ToolCallContent::Content(content) = item {
            crate::content::validate_durable(std::slice::from_ref(&content.content))?;
        }
    }
    Ok(())
}

fn serialize_optional_enum<T: serde::Serialize>(value: &Option<T>) -> String {
    value
        .as_ref()
        .and_then(|inner| serde_json::to_value(inner).ok())
        .and_then(|serialized| serialized.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "unknown".to_string())
}

fn format_tool_call_output(
    fields: &agent_client_protocol_schema::ToolCallUpdateFields,
) -> Option<String> {
    if let Some(content) = &fields.content {
        let parts = content
            .iter()
            .filter_map(|item| match item {
                ToolCallContent::Content(content) => match &content.content {
                    ContentBlock::Text(text) => Some(text.text.clone()),
                    other => serde_json::to_string(other).ok(),
                },
                ToolCallContent::Diff(diff) => serde_json::to_string(diff).ok(),
                ToolCallContent::Terminal(terminal) => {
                    Some(format!("terminal: {}", terminal.terminal_id))
                }
                _ => None,
            })
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        if !parts.is_empty() {
            return Some(parts.join("\n"));
        }
    }

    fields.raw_output.as_ref().map(|raw_output| {
        raw_output
            .as_str()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| raw_output.to_string())
    })
}

fn clean_tool_output(output: Option<String>) -> Option<String> {
    let s = output?;
    let trimmed = s.trim();
    if trimmed.starts_with("```") && trimmed.ends_with("```") && trimmed.len() >= 6 {
        let inner = &trimmed[3..trimmed.len() - 3];
        let content = if let Some(newline_pos) = inner.find('\n') {
            &inner[newline_pos + 1..]
        } else {
            inner
        };
        Some(content.trim_end().to_string())
    } else {
        Some(s)
    }
}

fn extract_tool_call_title(title: Option<&str>, raw_input: Option<&serde_json::Value>) -> String {
    if let Some(t) = title {
        let trimmed = t.trim();
        if !trimmed.is_empty()
            && trimmed != "Read File"
            && trimmed != "Terminal"
            && trimmed != "Tool Call"
        {
            return trimmed.to_string();
        }
    }

    if let Some(input) = raw_input {
        if let Some(cmd) = input.get("command").and_then(|c| c.as_str()) {
            return format!("Terminal: {}", cmd);
        }
        if let Some(path) = input
            .get("file_path")
            .or_else(|| input.get("path"))
            .and_then(|p| p.as_str())
        {
            return format!("Read: {}", path);
        }
        if let Some(desc) = input.get("description").and_then(|d| d.as_str()) {
            return desc.to_string();
        }
    }

    title
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Tool Call".to_string())
}

fn extract_parent_id(meta: Option<&agent_client_protocol_schema::Meta>) -> Option<String> {
    meta.and_then(|m| serde_json::to_value(m).ok())
        .and_then(|v| {
            v.pointer("/claudeCode/parentToolUseId")
                .and_then(|p| p.as_str())
                .map(ToOwned::to_owned)
        })
}

async fn wait_task(
    child: SharedChild,
    child_root_pid: Option<u32>,
    connected: Arc<AtomicBool>,
    chat_id: String,
    agent_id: String,
) {
    let exit_status = loop {
        {
            let mut guard = child.lock().await;
            let Some(c) = guard.as_mut() else { break None };
            match c.try_wait() {
                Ok(Some(status)) => break Some(status),
                Err(error) => {
                    tracing::warn!(
                        agent_id = %agent_id,
                        chat_id = %chat_id,
                        pid = ?child_root_pid,
                        error = %error,
                        "failed to read ACP process exit status"
                    );
                    break None;
                }
                Ok(None) => {}
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    };
    let unexpected = connected.swap(false, Ordering::SeqCst);
    if unexpected {
        tracing::warn!(
            agent_id = %agent_id,
            chat_id = %chat_id,
            pid = ?child_root_pid,
            exit_status = ?exit_status,
            "ACP process exited unexpectedly"
        );
    }
    kill_child(&child, child_root_pid).await;
}

async fn kill_child(child: &SharedChild, root_pid: Option<u32>) {
    let Some(mut c) = child.lock().await.take() else {
        return;
    };
    if let Err(e) = c.start_kill() {
        tracing::warn!(?root_pid, error = %e, "kill_child: start_kill failed");
    }
    // Wait briefly for the wrapper to observe exit so subsequent shutdown work
    // doesn't race the kill. On Windows TerminateJobObject is asynchronous;
    // 5s is plenty for the kernel to tear the tree down.
    let _ = tokio::task::spawn_blocking(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match c.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) if std::time::Instant::now() >= deadline => {
                    tracing::warn!(?root_pid, "kill_child: timed out waiting for exit");
                    return;
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::agent_client_protocol_schema::{
        Content, ImageContent, ResourceLink, TextContent, ToolCall, ToolCallContent,
        ToolCallUpdateFields,
    };
    use super::*;

    #[tokio::test]
    async fn test_handle_agent_request_accepts_schema_method_names() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("note.txt");
        std::fs::write(&file_path, "hello").unwrap();

        let tracker = Arc::new(crate::tasks::TerminalTaskTracker::default());
        let handler = CallbackHandler::new(
            "session-1".into(),
            "codex".into(),
            Arc::new(EventLog::new(100)),
            temp.path().to_path_buf(),
            Arc::new(std::collections::HashMap::new()),
            tracker,
        );

        let params =
            serde_json::to_value(ReadTextFileRequest::new("session-1", file_path)).unwrap();
        let result = handle_agent_request(
            CLIENT_METHOD_NAMES.fs_read_text_file,
            params,
            &agent_client_protocol_schema::RequestId::Number(1),
            &handler,
        )
        .await
        .unwrap();

        assert_eq!(result["content"], "hello");
    }

    #[test]
    fn test_format_tool_call_output_prefers_text_content() {
        let fields = ToolCallUpdateFields::new().content(vec![ToolCallContent::Content(
            agent_client_protocol_schema::Content::new(ContentBlock::Text(TextContent::new(
                "tool output",
            ))),
        )]);

        assert_eq!(
            format_tool_call_output(&fields),
            Some("tool output".to_string())
        );
    }

    #[test]
    fn prompt_capabilities_use_typed_stable_initialize_shape() {
        let image = [ContentBlock::Image(ImageContent::new(
            "iVBORw0KGgo=",
            "image/png",
        ))];
        for (label, prompt_capabilities, accepted) in [
            ("absent", None, false),
            ("null", Some(serde_json::Value::Null), false),
            ("object", Some(serde_json::json!({"image": {}})), false),
            ("true", Some(serde_json::json!({"image": true})), true),
        ] {
            let mut agent_capabilities = serde_json::json!({
                "sessionCapabilities": {"prompt": {"image": {}}}
            });
            if let Some(value) = prompt_capabilities {
                agent_capabilities["promptCapabilities"] = value;
            }
            let response: InitializeResponse = serde_json::from_value(serde_json::json!({
                "protocolVersion": 1,
                "agentCapabilities": agent_capabilities,
                "agentInfo": {"name": "test", "version": "1"},
                "authMethods": []
            }))
            .unwrap();
            let result = validate_prompt_capabilities(
                &response.agent_capabilities.prompt_capabilities,
                &image,
            );
            assert_eq!(result.is_ok(), accepted, "{label}");
        }
    }

    #[tokio::test]
    async fn test_initial_tool_call_content_is_retained() {
        let log = test_log();
        let (cmds, modes, usage) = test_arcs();
        let update = SessionUpdate::ToolCall(ToolCall::new("rich-tool", "Inspect").content(vec![
            ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(
                "summary",
            )))),
            ToolCallContent::Content(Content::new(ContentBlock::ResourceLink(ResourceLink::new(
                "Batey",
                "https://example.test/tool",
            )))),
            ToolCallContent::Content(Content::new(ContentBlock::Image(ImageContent::new(
                "iVBORw0KGgo=",
                "image/png",
            )))),
        ]));
        handle_session_update(&log, &None, "s1", "codex", &update, &cmds, &modes, &usage)
            .await
            .unwrap();
        let events = match log.replay_from(1) {
            crate::events::ReplayResult::Complete(events) => events,
            _ => panic!("expected complete replay"),
        };
        assert!(matches!(
            &events[0].payload,
            EventPayload::ToolCall { content: Some(content), .. }
                if matches!(content.as_array().map(Vec::as_slice), Some([
                    first, second, third
                ]) if first["content"]["type"] == "text"
                    && second["content"]["type"] == "resource_link"
                    && third["content"]["type"] == "image")
        ));
    }

    #[test]
    fn test_serialize_optional_enum_uses_wire_name() {
        let status = Some(agent_client_protocol_schema::ToolCallStatus::Completed);

        assert_eq!(serialize_optional_enum(&status), "completed");
    }

    #[test]
    fn test_clean_tool_output_strips_markdown_code_fence() {
        let fenced = Some("```rust\nfn main() {}\n```".to_string());
        assert_eq!(clean_tool_output(fenced), Some("fn main() {}".to_string()));

        let unfenced = Some("plain output".to_string());
        assert_eq!(
            clean_tool_output(unfenced),
            Some("plain output".to_string())
        );
    }

    #[test]
    fn test_extract_tool_call_title() {
        assert_eq!(
            extract_tool_call_title(
                Some("Read File"),
                Some(&serde_json::json!({"file_path": "src/lib.rs"}))
            ),
            "Read: src/lib.rs"
        );
        assert_eq!(
            extract_tool_call_title(
                Some("Terminal"),
                Some(&serde_json::json!({"command": "cargo check"}))
            ),
            "Terminal: cargo check"
        );
        assert_eq!(
            extract_tool_call_title(Some("Read src/lib.rs (1 - 50)"), None),
            "Read src/lib.rs (1 - 50)"
        );
    }

    fn test_log() -> Arc<EventLog> {
        Arc::new(EventLog::new(100))
    }

    type SharedJson = Arc<tokio::sync::RwLock<serde_json::Value>>;

    #[allow(clippy::type_complexity)]
    fn test_arcs() -> (SharedJson, SharedJson, SharedJson) {
        (
            Arc::new(tokio::sync::RwLock::new(serde_json::json!([]))),
            Arc::new(tokio::sync::RwLock::new(serde_json::Value::Null)),
            Arc::new(tokio::sync::RwLock::new(serde_json::Value::Null)),
        )
    }

    #[tokio::test]
    async fn test_available_commands_preserve_ordering_and_hints() {
        let log = test_log();
        let (cmds, modes, usage) = test_arcs();
        let update = SessionUpdate::AvailableCommandsUpdate(
            agent_client_protocol_schema::AvailableCommandsUpdate::new(vec![
                agent_client_protocol_schema::AvailableCommand::new("plan", "Make a plan").input(
                    agent_client_protocol_schema::AvailableCommandInput::Unstructured(
                        agent_client_protocol_schema::UnstructuredCommandInput::new("goal"),
                    ),
                ),
                agent_client_protocol_schema::AvailableCommand::new("review", "Review changes"),
            ]),
        );
        handle_session_update(&log, &None, "s1", "codex", &update, &cmds, &modes, &usage)
            .await
            .unwrap();
        assert_eq!(cmds.read().await.as_array().unwrap().len(), 2);
        let events = match log.replay_from(1) {
            crate::events::ReplayResult::Complete(e) => e,
            _ => panic!("expected complete"),
        };
        assert!(matches!(
            &events[0].payload,
            crate::events::EventPayload::AvailableCommands { commands } if commands.as_array().unwrap().len() == 2
        ));
    }

    #[tokio::test]
    async fn test_current_mode_update_merges_and_old_agents_without_modes_work() {
        let log = test_log();
        let (cmds, modes, usage) = test_arcs();
        // Agent without modes: Null stays Null, no crash.
        let update = SessionUpdate::CurrentModeUpdate(
            agent_client_protocol_schema::CurrentModeUpdate::new("act"),
        );
        handle_session_update(&log, &None, "s1", "codex", &update, &cmds, &modes, &usage)
            .await
            .unwrap();
        assert_eq!(modes.read().await["current_mode_id"], "act");
    }

    #[tokio::test]
    async fn test_user_message_chunk_does_not_duplicate() {
        let log = test_log();
        let (cmds, modes, usage) = test_arcs();
        let chunk = agent_client_protocol_schema::ContentChunk::new(ContentBlock::Text(
            TextContent::new("hello"),
        ));
        let update = SessionUpdate::UserMessageChunk(chunk);
        handle_session_update(&log, &None, "s1", "codex", &update, &cmds, &modes, &usage)
            .await
            .unwrap();
        let events = match log.replay_from(1) {
            crate::events::ReplayResult::Complete(e) => e,
            _ => panic!("expected complete"),
        };
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn test_message_ids_survive_and_old_chunks_without_ids_work() {
        let log = test_log();
        let (cmds, modes, usage) = test_arcs();
        let with_id = SessionUpdate::AgentMessageChunk(
            agent_client_protocol_schema::ContentChunk::new(ContentBlock::Text(TextContent::new(
                "hi",
            )))
            .message_id("msg-1"),
        );
        handle_session_update(&log, &None, "s1", "codex", &with_id, &cmds, &modes, &usage)
            .await
            .unwrap();
        let without_id =
            SessionUpdate::AgentMessageChunk(agent_client_protocol_schema::ContentChunk::new(
                ContentBlock::Text(TextContent::new("old")),
            ));
        handle_session_update(
            &log,
            &None,
            "s1",
            "codex",
            &without_id,
            &cmds,
            &modes,
            &usage,
        )
        .await
        .unwrap();
        let events = match log.replay_from(1) {
            crate::events::ReplayResult::Complete(e) => e,
            _ => panic!("expected complete"),
        };
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0].payload,
            crate::events::EventPayload::MessageChunk { message_id: Some(id), .. } if id == "msg-1"
        ));
        assert!(matches!(
            &events[1].payload,
            crate::events::EventPayload::MessageChunk {
                message_id: None,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn test_usage_update_exposes_context_and_cost_generically() {
        let log = test_log();
        let (cmds, modes, usage) = test_arcs();
        let update = SessionUpdate::UsageUpdate(
            agent_client_protocol_schema::UsageUpdate::new(100, 2000)
                .cost(agent_client_protocol_schema::Cost::new(0.5, "USD")),
        );
        handle_session_update(&log, &None, "s1", "codex", &update, &cmds, &modes, &usage)
            .await
            .unwrap();
        assert_eq!(usage.read().await["used"], 100);
        let events = match log.replay_from(1) {
            crate::events::ReplayResult::Complete(e) => e,
            _ => panic!("expected complete"),
        };
        assert!(matches!(
            &events[0].payload,
            crate::events::EventPayload::UsageUpdate {
                used: 100,
                size: 2000,
                cost_amount: Some(_),
                cost_currency: Some(_),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn test_tool_locations_preserved() {
        let log = test_log();
        let (cmds, modes, usage) = test_arcs();
        let tc = agent_client_protocol_schema::ToolCall::new("t1", "Edit").locations(vec![
            agent_client_protocol_schema::ToolCallLocation::new("/a/b.rs").line(3_u32),
        ]);
        let update = SessionUpdate::ToolCall(tc);
        handle_session_update(&log, &None, "s1", "codex", &update, &cmds, &modes, &usage)
            .await
            .unwrap();
        let events = match log.replay_from(1) {
            crate::events::ReplayResult::Complete(e) => e,
            _ => panic!("expected complete"),
        };
        assert!(matches!(
            &events[0].payload,
            crate::events::EventPayload::ToolCall {
                locations: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn test_user_message_meta_round_trip_and_omission() {
        let meta = user_message_meta("msg-123");
        let wire = serde_json::to_value(&meta).unwrap();
        assert_eq!(wire["batey"]["userMessageId"], "msg-123");
        assert_eq!(
            echoed_user_message_id(&Some(meta)),
            Some("msg-123".to_string())
        );
        // Agents that omit the extension stay supported.
        assert_eq!(echoed_user_message_id(&None), None);
        assert_eq!(
            echoed_user_message_id(&Some(agent_client_protocol_schema::Meta::new())),
            None
        );
    }

    #[test]
    fn test_validate_boolean_config_and_select() {
        let options = serde_json::json!([
            {"id": "model", "type": "select", "options": [{"value": "small", "name": "Small"}]},
            {"id": "web", "type": "boolean"}
        ]);
        assert!(validate_config_value(&options, "model", &serde_json::json!("small")).is_ok());
        assert!(validate_config_value(&options, "model", &serde_json::json!("big")).is_err());
        assert!(validate_config_value(&options, "web", &serde_json::json!(true)).is_ok());
        assert!(validate_config_value(&options, "web", &serde_json::json!("yes")).is_err());
    }

    #[test]
    fn additional_directories_requires_an_object_capability() {
        assert!(!supports_additional_directories(
            &serde_json::json!({"sessionCapabilities": {}})
        ));
        assert!(!supports_additional_directories(
            &serde_json::json!({"sessionCapabilities": {"additionalDirectories": null}})
        ));
        assert!(supports_additional_directories(
            &serde_json::json!({"sessionCapabilities": {"additionalDirectories": {}}})
        ));
    }

    #[test]
    fn operational_error_categories_do_not_expose_error_text() {
        let error = anyhow::anyhow!("agent returned a private prompt and timed out");
        assert_eq!(failure_category(&error), "timeout");
        assert_eq!(
            spawn_failure_category(&anyhow::anyhow!(
                "Failed to spawn ACP agent '/private/agent': executable not found"
            )),
            "executable_not_found"
        );
    }

    #[test]
    fn executable_identifier_keeps_only_a_safe_basename() {
        assert_eq!(
            sanitized_executable_identifier("/private/path/agent-with_args"),
            "agent-with_args"
        );
        assert_eq!(
            sanitized_executable_identifier("/private/path/agent secret"),
            "agentsecret"
        );
    }
}
