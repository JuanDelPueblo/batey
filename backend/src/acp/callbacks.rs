use ::agent_client_protocol_schema::v1 as agent_client_protocol_schema;
use agent_client_protocol_schema::{
    CreateTerminalRequest, CreateTerminalResponse, KillTerminalRequest, KillTerminalResponse,
    PermissionOptionId, ReadTextFileRequest, ReadTextFileResponse, ReleaseTerminalRequest,
    ReleaseTerminalResponse, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, TerminalOutputRequest,
    TerminalOutputResponse, WaitForTerminalExitRequest, WaitForTerminalExitResponse,
    WriteTextFileRequest, WriteTextFileResponse,
};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::sync::{oneshot, RwLock};

use super::process::AcpProcess;
use crate::events::{EventLog, EventPayload};
use crate::tasks::{ManagedTask, TerminalTaskTracker};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum CallbackPolicy {
    DenyAll,
    ReadOnly,
    #[default]
    Ask,
    AutoApprove,
}

pub struct PendingPermission {
    pub tx: oneshot::Sender<Option<PermissionOptionId>>,
    pub option_ids: HashSet<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingElicitationInfo {
    pub id: String,
    pub mode: String,
    pub message: String,
    pub schema: Option<serde_json::Value>,
    pub url: Option<String>,
    pub elicitation_id: Option<String>,
    pub tool_call_id: Option<String>,
}

struct PendingElicitation {
    tx: oneshot::Sender<agent_client_protocol_schema::ElicitationAction>,
    info: PendingElicitationInfo,
    /// Typed requested schema for form mode. Held so responses are
    /// validated against what the agent advertised before anything is
    /// sent back. `None` for URL mode.
    requested_schema: Option<agent_client_protocol_schema::ElicitationSchema>,
}

pub struct CallbackHandler {
    session_id: String,
    agent_name: String,
    event_log: Arc<EventLog>,
    cwd: PathBuf,
    roots: Vec<PathBuf>,
    base_env: Arc<HashMap<String, String>>,
    pub pending_permissions: Arc<RwLock<HashMap<String, PendingPermission>>>,
    pending_elicitations: Arc<RwLock<HashMap<String, PendingElicitation>>>,
    task_tracker: Arc<TerminalTaskTracker>,
    active_terminals: Arc<RwLock<HashSet<String>>>,
}

impl CallbackHandler {
    pub fn new(
        session_id: String,
        agent_name: String,
        event_log: Arc<EventLog>,
        cwd: PathBuf,
        base_env: Arc<HashMap<String, String>>,
        task_tracker: Arc<TerminalTaskTracker>,
    ) -> Self {
        let roots = vec![cwd.canonicalize().unwrap_or_else(|_| cwd.clone())];
        Self::new_with_roots(
            session_id,
            agent_name,
            event_log,
            cwd,
            roots,
            base_env,
            task_tracker,
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_roots(
        session_id: String,
        agent_name: String,
        event_log: Arc<EventLog>,
        cwd: PathBuf,
        roots: Vec<PathBuf>,
        base_env: Arc<HashMap<String, String>>,
        task_tracker: Arc<TerminalTaskTracker>,
    ) -> Self {
        Self {
            session_id,
            agent_name,
            event_log,
            cwd,
            roots,
            base_env,
            pending_permissions: Arc::new(RwLock::new(HashMap::new())),
            pending_elicitations: Arc::new(RwLock::new(HashMap::new())),
            task_tracker,
            active_terminals: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    fn resolve_path(
        &self,
        path: &std::path::Path,
        allow_missing_leaf: bool,
    ) -> Result<PathBuf, agent_client_protocol_schema::Error> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        };

        let canonical = if allow_missing_leaf && !absolute.exists() {
            let parent = absolute.parent().ok_or_else(|| {
                agent_client_protocol_schema::Error::new(-32002, "Path has no parent")
            })?;
            let canonical_parent = parent.canonicalize().map_err(|e| {
                agent_client_protocol_schema::Error::new(
                    -32002,
                    format!("Path canonicalization failed: {}", e),
                )
            })?;
            let leaf = absolute.file_name().ok_or_else(|| {
                agent_client_protocol_schema::Error::new(-32002, "Path has no leaf")
            })?;
            canonical_parent.join(leaf)
        } else {
            absolute.canonicalize().map_err(|e| {
                agent_client_protocol_schema::Error::new(
                    -32002,
                    format!("Path canonicalization failed: {}", e),
                )
            })?
        };

        self.validate_path(&canonical, allow_missing_leaf)?;
        Ok(canonical)
    }

    fn validate_path(
        &self,
        path: &std::path::Path,
        allow_missing_leaf: bool,
    ) -> Result<(), agent_client_protocol_schema::Error> {
        let canonical_target = if allow_missing_leaf && !path.exists() {
            let parent = path.parent().ok_or_else(|| {
                agent_client_protocol_schema::Error::new(-32002, "Path has no parent")
            })?;
            let canonical_parent = parent.canonicalize().map_err(|e| {
                agent_client_protocol_schema::Error::new(
                    -32002,
                    format!("Path canonicalization failed: {}", e),
                )
            })?;
            let leaf = path.file_name().ok_or_else(|| {
                agent_client_protocol_schema::Error::new(-32002, "Path has no leaf")
            })?;
            canonical_parent.join(leaf)
        } else {
            path.canonicalize().map_err(|e| {
                agent_client_protocol_schema::Error::new(
                    -32002,
                    format!("Path canonicalization failed: {}", e),
                )
            })?
        };

        if !self
            .roots
            .iter()
            .any(|root| canonical_target.starts_with(root))
        {
            return Err(agent_client_protocol_schema::Error::new(
                -32003,
                "Path is outside the configured workspace roots",
            ));
        }

        Ok(())
    }

    async fn get_terminal(
        &self,
        terminal_id: &agent_client_protocol_schema::TerminalId,
    ) -> agent_client_protocol_schema::Result<Arc<ManagedTask>> {
        if !self
            .active_terminals
            .read()
            .await
            .contains(terminal_id.0.as_ref())
        {
            return Err(agent_client_protocol_schema::Error::new(
                -32004,
                "Terminal not found",
            ));
        }
        self.task_tracker
            .get_task(terminal_id.0.as_ref())
            .await
            .ok_or_else(|| agent_client_protocol_schema::Error::new(-32004, "Terminal not found"))
    }

    async fn request_user_permission(
        &self,
        tool_name: &str,
        description: String,
        title: Option<String>,
        kind: Option<String>,
        options: serde_json::Value,
        option_ids: HashSet<String>,
    ) -> Option<PermissionOptionId> {
        let (tx, rx) = oneshot::channel();
        let perm_id = uuid::Uuid::new_v4().to_string();

        self.pending_permissions
            .write()
            .await
            .insert(perm_id.clone(), PendingPermission { tx, option_ids });

        if let Err(e) = self.event_log.append(
            &self.session_id,
            &self.agent_name,
            EventPayload::PermissionRequest {
                id: perm_id.clone(),
                method: tool_name.to_string(),
                description,
                title,
                kind,
                options,
            },
        ) {
            tracing::error!("Failed to emit PermissionRequest event: {}", e);
            self.pending_permissions.write().await.remove(&perm_id);
            return None;
        }

        // None means the turn was cancelled: the caller must answer with
        // ACP's cancelled outcome, never as a user denial.
        rx.await.unwrap_or(None)
    }

    pub async fn handle_request_permission(
        &self,
        req: RequestPermissionRequest,
    ) -> RequestPermissionResponse {
        let (title, description, kind) = format_permission_tool_call(&req.tool_call);
        let options = serde_json::to_value(&req.options).unwrap_or_else(|error| {
            tracing::warn!(%error, "Failed to serialize ACP permission options");
            serde_json::json!([])
        });
        let option_ids = req
            .options
            .iter()
            .filter_map(|option| {
                serde_json::to_value(&option.option_id)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
            })
            .collect();
        let selected = self
            .request_user_permission(
                "session/request_permission",
                description,
                title,
                kind,
                options,
                option_ids,
            )
            .await;
        match selected {
            None => RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled),
            Some(option_id) => RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
                SelectedPermissionOutcome::new(option_id),
            )),
        }
    }

    pub async fn handle_read_file(
        &self,
        req: ReadTextFileRequest,
    ) -> agent_client_protocol_schema::Result<ReadTextFileResponse> {
        let path = self.resolve_path(&req.path, false)?;
        let content = tokio::fs::read_to_string(&path).await.map_err(|e| {
            agent_client_protocol_schema::Error::new(-32002, format!("Read failed: {}", e))
        })?;
        // Honor stable `line` (1-based) and `limit` semantics,
        // including boundary and error cases.
        let sliced = apply_read_window(&content, req.line, req.limit)
            .map_err(|message| agent_client_protocol_schema::Error::new(-32002, message))?;
        Ok(ReadTextFileResponse::new(sliced))
    }

    pub async fn handle_write_file(
        &self,
        req: WriteTextFileRequest,
    ) -> agent_client_protocol_schema::Result<WriteTextFileResponse> {
        let path = self.resolve_path(&req.path, true)?;
        tokio::fs::write(&path, &req.content).await.map_err(|e| {
            agent_client_protocol_schema::Error::new(-32002, format!("Write failed: {}", e))
        })?;
        Ok(WriteTextFileResponse::new())
    }

    pub async fn handle_create_terminal(
        &self,
        req: CreateTerminalRequest,
    ) -> agent_client_protocol_schema::Result<CreateTerminalResponse> {
        let cwd = match req.cwd.as_ref() {
            Some(cwd) => self.resolve_path(cwd, false)?,
            None => self.cwd.clone(),
        };
        self.validate_path(&cwd, false)?;

        let env = crate::workspace_env::merge_terminal_env(&self.base_env, &req.env);
        let process = AcpProcess::spawn(&req.command, &req.args, &env, &cwd).map_err(|e| {
            agent_client_protocol_schema::Error::new(
                -32002,
                format!("Terminal spawn failed: {}", e),
            )
        })?;

        let terminal_id = uuid::Uuid::new_v4().to_string();
        let cmd_summary = if req.args.is_empty() {
            req.command.clone()
        } else {
            format!("{} {}", req.command, req.args.join(" "))
        };
        let task = Arc::new(ManagedTask::new(
            terminal_id.clone(),
            self.session_id.clone(),
            cmd_summary,
            cwd.clone(),
            req.output_byte_limit,
        ));

        let stdout = process.stdout;
        let stderr = process.stderr;
        drop(process.stdin);
        let (kill_tx, kill_rx) = oneshot::channel();
        *task.kill_tx.lock().unwrap() = Some(kill_tx);

        self.task_tracker.register_task(task.clone()).await;
        self.active_terminals
            .write()
            .await
            .insert(terminal_id.clone());

        let drain_handles = vec![
            tokio::spawn(drain_terminal_stream(stdout, task.clone())),
            tokio::spawn(drain_terminal_stream(stderr, task.clone())),
        ];
        tokio::spawn(supervise_terminal(
            process.child,
            task,
            kill_rx,
            drain_handles,
            self.event_log.clone(),
            self.task_tracker.clone(),
        ));
        let _ = self
            .event_log
            .append("", "", EventPayload::MetadataChanged {});

        Ok(CreateTerminalResponse::new(terminal_id))
    }

    pub async fn handle_terminal_output(
        &self,
        req: TerminalOutputRequest,
    ) -> agent_client_protocol_schema::Result<TerminalOutputResponse> {
        let task = self.get_terminal(&req.terminal_id).await?;
        let buffer = task.buffer.read().await;
        let exit_status = task.exit_status.read().await.clone();
        Ok(
            TerminalOutputResponse::new(buffer.output.clone(), buffer.truncated)
                .exit_status(exit_status),
        )
    }

    pub async fn handle_release_terminal(
        &self,
        req: ReleaseTerminalRequest,
    ) -> agent_client_protocol_schema::Result<ReleaseTerminalResponse> {
        let task = self.get_terminal(&req.terminal_id).await?;
        self.active_terminals
            .write()
            .await
            .remove(req.terminal_id.0.as_ref());
        task.stop();
        Ok(ReleaseTerminalResponse::new())
    }

    pub async fn handle_kill_terminal(
        &self,
        req: KillTerminalRequest,
    ) -> agent_client_protocol_schema::Result<KillTerminalResponse> {
        let task = self.get_terminal(&req.terminal_id).await?;
        task.stop();
        Ok(KillTerminalResponse::new())
    }

    pub async fn handle_wait_for_terminal_exit(
        &self,
        req: WaitForTerminalExitRequest,
    ) -> agent_client_protocol_schema::Result<WaitForTerminalExitResponse> {
        let task = self.get_terminal(&req.terminal_id).await?;
        loop {
            let notified = task.exit_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if let Some(exit_status) = task.exit_status.read().await.clone() {
                return Ok(WaitForTerminalExitResponse::new(exit_status));
            }
            notified.await;
        }
    }

    pub async fn respond_permission(&self, perm_id: &str, option_id: &str) -> bool {
        let mut pending = self.pending_permissions.write().await;
        if let Some(p) = pending.remove(perm_id) {
            if !p.option_ids.contains(option_id) {
                pending.insert(perm_id.to_string(), p);
                return false;
            }
            let option_id = option_id.to_string();
            if let Err(error) = self.event_log.append(
                &self.session_id,
                &self.agent_name,
                EventPayload::PermissionResponse {
                    id: perm_id.to_string(),
                    option_id: Some(option_id.clone()),
                    legacy_granted: None,
                },
            ) {
                tracing::error!(%error, "Failed to emit PermissionResponse event");
                pending.insert(perm_id.to_string(), p);
                return false;
            }
            return p.tx.send(Some(option_id.into())).is_ok();
        }
        false
    }

    /// Answer every pending permission with ACP `cancelled`. Used when the
    /// originating turn is cancelled; never records cancellation as denial.
    pub async fn cancel_pending_permissions(&self) {
        let mut pending = self.pending_permissions.write().await;
        for (id, p) in pending.drain() {
            if let Err(error) = self.event_log.append(
                &self.session_id,
                &self.agent_name,
                EventPayload::PermissionResponse {
                    id,
                    option_id: None,
                    legacy_granted: None,
                },
            ) {
                tracing::error!(%error, "Failed to emit cancelled PermissionResponse event");
            }
            let _ = p.tx.send(None);
        }
    }

    pub async fn cancel_all_pending(&self) {
        self.cancel_pending_permissions().await;
        self.cancel_pending_elicitations().await;
    }

    pub async fn list_pending_elicitations(&self) -> Vec<PendingElicitationInfo> {
        self.pending_elicitations
            .read()
            .await
            .values()
            .map(|p| p.info.clone())
            .collect()
    }

    pub async fn respond_elicitation(
        &self,
        id: &str,
        action: &str,
        content: Option<serde_json::Value>,
    ) -> anyhow::Result<bool> {
        // Validate an accept against the advertised schema BEFORE removing
        // the pending entry, so a rejected submission stays answerable.
        // Form values stay transient: only the action marker is recorded,
        // never the submitted values.
        if action == "accept" {
            let (mode, schema) = {
                let pending = self.pending_elicitations.read().await;
                match pending.get(id) {
                    Some(p) => (p.info.mode.clone(), p.requested_schema.clone()),
                    None => return Ok(false),
                }
            };
            if mode == "form" {
                let schema = schema
                    .ok_or_else(|| anyhow::anyhow!("Form elicitation has no advertised schema"))?;
                let raw = content.unwrap_or(serde_json::Value::Null);
                let map: std::collections::BTreeMap<
                    String,
                    agent_client_protocol_schema::ElicitationContentValue,
                > = serde_json::from_value(raw)
                    .map_err(|e| anyhow::anyhow!("Invalid elicitation content: {e}"))?;
                let validated = validate_elicitation_content(&schema, &map).map_err(|e| {
                    anyhow::anyhow!("Elicitation content does not match the requested form: {e}")
                })?;
                return self
                    .resolve_elicitation(
                        id,
                        agent_client_protocol_schema::ElicitationAction::Accept(
                            agent_client_protocol_schema::ElicitationAcceptAction::new()
                                .content(Some(validated)),
                        ),
                    )
                    .await;
            }
        }
        let elic_action = match action {
            "accept" => {
                // URL mode carries no form content; ignore any payload.
                agent_client_protocol_schema::ElicitationAction::Accept(
                    agent_client_protocol_schema::ElicitationAcceptAction::new().content(None),
                )
            }
            "decline" => agent_client_protocol_schema::ElicitationAction::Decline,
            _ => agent_client_protocol_schema::ElicitationAction::Cancel,
        };
        self.resolve_elicitation(id, elic_action).await
    }

    async fn resolve_elicitation(
        &self,
        id: &str,
        elic_action: agent_client_protocol_schema::ElicitationAction,
    ) -> anyhow::Result<bool> {
        let tx = {
            let mut pending = self.pending_elicitations.write().await;
            match pending.remove(id) {
                Some(p) => p.tx,
                None => return Ok(false),
            }
        };
        let action_str = match &elic_action {
            agent_client_protocol_schema::ElicitationAction::Accept(_) => "accept",
            agent_client_protocol_schema::ElicitationAction::Decline => "decline",
            _ => "cancel",
        }
        .to_string();
        let sent = tx.send(elic_action).is_ok();
        // Record only the action, never form values or URL secrets.
        let _ = self.event_log.append(
            &self.session_id,
            &self.agent_name,
            EventPayload::ElicitationResponse {
                id: id.to_string(),
                action: action_str,
            },
        );
        Ok(sent)
    }

    pub async fn cancel_pending_elicitations(&self) {
        let mut pending = self.pending_elicitations.write().await;
        for (id, p) in pending.drain() {
            let _ =
                p.tx.send(agent_client_protocol_schema::ElicitationAction::Cancel);
            let _ = self.event_log.append(
                &self.session_id,
                &self.agent_name,
                EventPayload::ElicitationResponse {
                    id,
                    action: "cancel".to_string(),
                },
            );
        }
    }

    pub async fn complete_elicitation(&self, elicitation_id: &str) {
        // URL completion dismisses pending UI without logging secrets.
        let removed = {
            let mut pending = self.pending_elicitations.write().await;
            // Pending id equals elicitation_id for URL mode.
            pending.remove(elicitation_id).is_some()
        };
        if removed {
            let _ = self.event_log.append(
                &self.session_id,
                &self.agent_name,
                EventPayload::ElicitationResponse {
                    id: elicitation_id.to_string(),
                    action: "cancel".to_string(),
                },
            );
        }
    }

    /// Stable `elicitation/create` for form and URL modes. Unknown modes are
    /// preserved generically but answered as cancelled rather than rendered
    /// as a known mode.
    pub async fn handle_elicitation(
        &self,
        rpc_id: String,
        req: agent_client_protocol_schema::CreateElicitationRequest,
    ) -> agent_client_protocol_schema::CreateElicitationResponse {
        use agent_client_protocol_schema::{ElicitationAction, ElicitationMode};
        let (mode_str, schema, url, elicitation_id, tool_call_id, requested_schema) =
            match &req.mode {
                ElicitationMode::Form(f) => {
                    let schema_val = serde_json::to_value(&f.requested_schema).ok();
                    let tool = match &f.scope {
                        agent_client_protocol_schema::ElicitationScope::Session(s) => {
                            s.tool_call_id.as_ref().map(|t| t.to_string())
                        }
                        _ => None,
                    };
                    (
                        "form".to_string(),
                        schema_val,
                        None,
                        None,
                        tool,
                        Some(f.requested_schema.clone()),
                    )
                }
                ElicitationMode::Url(u) => {
                    let tool = match &u.scope {
                        agent_client_protocol_schema::ElicitationScope::Session(s) => {
                            s.tool_call_id.as_ref().map(|t| t.to_string())
                        }
                        _ => None,
                    };
                    (
                        "url".to_string(),
                        None,
                        Some(u.url.clone()),
                        Some(u.elicitation_id.to_string()),
                        tool,
                        None,
                    )
                }
                ElicitationMode::Other(o) => {
                    // Never render unknown modes as known. Record opaquely and
                    // answer cancelled.
                    let _ = self.event_log.append(
                        &self.session_id,
                        &self.agent_name,
                        EventPayload::ElicitationRequest {
                            id: rpc_id.clone(),
                            mode: format!("other:{}", o.mode),
                            message: req.message.clone(),
                            schema: None,
                            url: None,
                            elicitation_id: None,
                            tool_call_id: None,
                        },
                    );
                    return agent_client_protocol_schema::CreateElicitationResponse::new(
                        ElicitationAction::Cancel,
                    );
                }
                _ => {
                    return agent_client_protocol_schema::CreateElicitationResponse::new(
                        ElicitationAction::Cancel,
                    );
                }
            };
        // Pending id is the stable elicitation_id for URL, else the RPC id.
        let pending_id = elicitation_id.clone().unwrap_or_else(|| rpc_id.clone());
        let info = PendingElicitationInfo {
            id: pending_id.clone(),
            mode: mode_str.clone(),
            message: req.message.clone(),
            schema: schema.clone(),
            url: url.clone(),
            elicitation_id: elicitation_id.clone(),
            tool_call_id: tool_call_id.clone(),
        };
        let (tx, rx) = oneshot::channel();
        self.pending_elicitations.write().await.insert(
            pending_id.clone(),
            PendingElicitation {
                tx,
                info: info.clone(),
                requested_schema,
            },
        );
        if let Err(e) = self.event_log.append(
            &self.session_id,
            &self.agent_name,
            EventPayload::ElicitationRequest {
                id: pending_id.clone(),
                mode: mode_str,
                message: req.message,
                schema,
                // Display the URL/host for explicit user action. Never
                // prefetch or open it here.
                url,
                elicitation_id,
                tool_call_id,
            },
        ) {
            tracing::error!("Failed to emit ElicitationRequest event: {}", e);
            self.pending_elicitations.write().await.remove(&pending_id);
            return agent_client_protocol_schema::CreateElicitationResponse::new(
                ElicitationAction::Cancel,
            );
        }
        // Wait for explicit user action. Cancellation (turn torn down)
        // resolves as Cancel, never as denial, and never logs secrets.
        let action = rx.await.unwrap_or(ElicitationAction::Cancel);
        agent_client_protocol_schema::CreateElicitationResponse::new(action)
    }

    pub async fn shutdown(&self) {
        self.cancel_all_pending().await;

        let active_ids: Vec<String> = self.active_terminals.write().await.drain().collect();
        for id in active_ids {
            if let Some(task) = self.task_tracker.get_task(&id).await {
                task.stop();
            }
        }
    }
}

/// Validates submitted form content against the agent's advertised stable
/// restricted schema and applies schema defaults for omitted optional
/// fields. Unknown submitted properties are rejected: the flat content
/// shape cannot represent anything the schema did not declare. Returns the
/// content to send back on success.
///
/// Enforced: required presence, value types, single-select membership
/// (`enum`/`oneOf`), string length bounds, numeric bounds, and multi-select
/// item counts and membership. String `pattern` and `format` are advisory
/// only: without a pattern engine they pass through untouched rather than
/// being interpreted.
fn validate_elicitation_content(
    schema: &agent_client_protocol_schema::ElicitationSchema,
    content: &std::collections::BTreeMap<
        String,
        agent_client_protocol_schema::ElicitationContentValue,
    >,
) -> Result<
    std::collections::BTreeMap<String, agent_client_protocol_schema::ElicitationContentValue>,
    String,
> {
    use agent_client_protocol_schema::{ElicitationContentValue, ElicitationPropertySchema};
    let mut validated = std::collections::BTreeMap::new();
    for (name, property) in &schema.properties {
        let required = schema
            .required
            .as_ref()
            .is_some_and(|names| names.iter().any(|n| n == name));
        match content.get(name) {
            None => {
                if required {
                    return Err(format!("Missing required field: {name}"));
                }
                // Omitted optional fields fall back to the schema default
                // when the agent declared one.
                match property {
                    ElicitationPropertySchema::String(s) => {
                        if let Some(d) = &s.default {
                            validated
                                .insert(name.clone(), ElicitationContentValue::String(d.clone()));
                        }
                    }
                    ElicitationPropertySchema::Number(n) => {
                        if let Some(d) = n.default {
                            validated.insert(name.clone(), ElicitationContentValue::Number(d));
                        }
                    }
                    ElicitationPropertySchema::Integer(i) => {
                        if let Some(d) = i.default {
                            validated.insert(name.clone(), ElicitationContentValue::Integer(d));
                        }
                    }
                    ElicitationPropertySchema::Boolean(b) => {
                        if let Some(d) = b.default {
                            validated.insert(name.clone(), ElicitationContentValue::Boolean(d));
                        }
                    }
                    ElicitationPropertySchema::Array(a) => {
                        if let Some(d) = &a.default {
                            validated.insert(
                                name.clone(),
                                ElicitationContentValue::StringArray(d.clone()),
                            );
                        }
                    }
                    _ => {}
                }
            }
            Some(value) => {
                validated.insert(name.clone(), check_property_value(name, property, value)?);
            }
        }
    }
    for name in content.keys() {
        if !schema.properties.contains_key(name) {
            return Err(format!("Unknown field: {name}"));
        }
    }
    Ok(validated)
}

fn check_property_value(
    name: &str,
    property: &agent_client_protocol_schema::ElicitationPropertySchema,
    value: &agent_client_protocol_schema::ElicitationContentValue,
) -> Result<agent_client_protocol_schema::ElicitationContentValue, String> {
    use agent_client_protocol_schema::{ElicitationContentValue, ElicitationPropertySchema};
    let err = |msg: &str| Err(format!("Invalid value for {name}: {msg}"));
    match property {
        ElicitationPropertySchema::String(s) => {
            let text = match value {
                ElicitationContentValue::String(t) => t,
                _ => return err("expected a string"),
            };
            if let Some(allowed) = &s.enum_values {
                if !allowed.iter().any(|v| v == text) {
                    return err("value is not one of the advertised choices");
                }
            }
            if let Some(options) = &s.one_of {
                if !options.iter().any(|o| o.value == *text) {
                    return err("value is not one of the advertised choices");
                }
            }
            let len = text.chars().count() as u32;
            if let Some(min) = s.min_length {
                if len < min {
                    return err("value is shorter than the advertised minimum");
                }
            }
            if let Some(max) = s.max_length {
                if len > max {
                    return err("value is longer than the advertised maximum");
                }
            }
            Ok(ElicitationContentValue::String(text.clone()))
        }
        ElicitationPropertySchema::Number(n) => {
            // Integer JSON input coerces: number fields commonly receive
            // whole values from numeric controls.
            let num = match value {
                ElicitationContentValue::Number(f) => *f,
                ElicitationContentValue::Integer(i) => *i as f64,
                _ => return err("expected a number"),
            };
            if let Some(min) = n.minimum {
                if num < min {
                    return err("value is below the advertised minimum");
                }
            }
            if let Some(max) = n.maximum {
                if num > max {
                    return err("value is above the advertised maximum");
                }
            }
            Ok(ElicitationContentValue::Number(num))
        }
        ElicitationPropertySchema::Integer(i) => {
            let num = match value {
                ElicitationContentValue::Integer(v) => *v,
                ElicitationContentValue::Number(f)
                    if f.fract() == 0.0 && *f >= i64::MIN as f64 && *f <= i64::MAX as f64 =>
                {
                    *f as i64
                }
                _ => return err("expected an integer"),
            };
            if let Some(min) = i.minimum {
                if num < min {
                    return err("value is below the advertised minimum");
                }
            }
            if let Some(max) = i.maximum {
                if num > max {
                    return err("value is above the advertised maximum");
                }
            }
            Ok(ElicitationContentValue::Integer(num))
        }
        ElicitationPropertySchema::Boolean(_) => match value {
            ElicitationContentValue::Boolean(b) => Ok(ElicitationContentValue::Boolean(*b)),
            _ => err("expected a boolean"),
        },
        ElicitationPropertySchema::Array(a) => {
            let items = match value {
                ElicitationContentValue::StringArray(v) => v,
                _ => return err("expected a list of strings"),
            };
            let len = items.len() as u64;
            if let Some(min) = a.min_items {
                if len < min {
                    return err("fewer items than the advertised minimum");
                }
            }
            if let Some(max) = a.max_items {
                if len > max {
                    return err("more items than the advertised maximum");
                }
            }
            let allowed: Option<Vec<&str>> = match &a.items {
                agent_client_protocol_schema::MultiSelectItems::String(s) => {
                    Some(s.values.iter().map(String::as_str).collect())
                }
                agent_client_protocol_schema::MultiSelectItems::Titled(t) => {
                    Some(t.options.iter().map(|o| o.value.as_str()).collect())
                }
                _ => None,
            };
            if let Some(allowed) = allowed {
                if let Some(bad) = items.iter().find(|v| !allowed.contains(&v.as_str())) {
                    return Err(format!(
                        "Invalid value for {name}: {bad} is not one of the advertised choices"
                    ));
                }
            }
            Ok(ElicitationContentValue::StringArray(items.clone()))
        }
        _ => Err(format!(
            "Unsupported property type for {name}; cannot verify the value"
        )),
    }
}

fn apply_read_window(
    content: &str,
    line: Option<u32>,
    limit: Option<u32>,
) -> Result<String, String> {
    // No window means the whole file. `line` is 1-based; `limit` caps the
    // number of lines. Boundary errors are explicit, never silent truncation.
    let start = match line {
        None => 0,
        Some(0) => return Err("`line` is 1-based and must be >= 1".to_string()),
        Some(n) => (n - 1) as usize,
    };
    let lines: Vec<&str> = content.lines().collect();
    if start > lines.len() {
        return Err(format!(
            "`line` {} is past end of file with {} lines",
            start + 1,
            lines.len()
        ));
    }
    let end = match limit {
        None => lines.len(),
        Some(0) => start,
        Some(n) => start.saturating_add(n as usize).min(lines.len()),
    };
    if end < start {
        return Err("Invalid `limit` for `fs/read_text_file`".to_string());
    }
    let mut out = lines[start..end].join("\n");
    // Preserve trailing newline semantics of the original slice.
    if !out.is_empty() && end < lines.len() {
        // Middle slice: lines() stripped newlines, rejoin is exact.
    } else if !content.is_empty()
        && end == lines.len()
        && content.ends_with('\n')
        && !out.is_empty()
    {
        out.push('\n');
    }
    Ok(out)
}

async fn drain_terminal_stream<R>(mut reader: R, task: Arc<ManagedTask>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut buf = [0_u8; 4096];
    let mut leftover = Vec::new();
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let data = if leftover.is_empty() {
                    &buf[..n]
                } else {
                    leftover.extend_from_slice(&buf[..n]);
                    leftover.as_slice()
                };

                let valid_len = match std::str::from_utf8(data) {
                    Ok(_) => data.len(),
                    Err(e) => e.valid_up_to(),
                };

                if valid_len > 0 {
                    let text = unsafe { std::str::from_utf8_unchecked(&data[..valid_len]) };
                    task.append_output(text).await;
                }

                let remaining = &data[valid_len..];
                if remaining.is_empty() {
                    leftover.clear();
                } else if remaining.len() >= 4 {
                    let chunk = String::from_utf8_lossy(remaining);
                    task.append_output(chunk.as_ref()).await;
                    leftover.clear();
                } else {
                    let saved = remaining.to_vec();
                    leftover.clear();
                    leftover = saved;
                }
            }
            Err(_) => break,
        }
    }

    if !leftover.is_empty() {
        let chunk = String::from_utf8_lossy(&leftover);
        task.append_output(chunk.as_ref()).await;
    }
}

async fn supervise_terminal(
    mut child: Box<dyn process_wrap::tokio::TokioChildWrapper>,
    task: Arc<ManagedTask>,
    mut kill_rx: oneshot::Receiver<()>,
    drain_handles: Vec<tokio::task::JoinHandle<()>>,
    event_log: Arc<crate::events::EventLog>,
    task_tracker: Arc<crate::tasks::TerminalTaskTracker>,
) {
    let status = loop {
        tokio::select! {
            _ = &mut kill_rx => {
                let _ = child.start_kill();
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(error) => break Err(error),
        }
    };
    // The direct child may exit while descendants remain. Both ProcessGroup
    // and JobObject wrappers use this final kill/drop to reap the whole tree.
    let _ = child.start_kill();

    for handle in drain_handles {
        let _ = handle.await;
    }

    task.record_exit(status).await;
    task_tracker.prune_chat_tasks(&task.chat_id).await;
    let _ = event_log.append("", "", EventPayload::MetadataChanged {});
}

fn format_permission_tool_call(
    tool_call: &agent_client_protocol_schema::ToolCallUpdate,
) -> (Option<String>, String, Option<String>) {
    let kind_str = tool_call.fields.kind.map(|k| {
        serde_json::to_value(k)
            .ok()
            .and_then(|v| v.as_str().map(ToOwned::to_owned))
            .unwrap_or_else(|| format!("{:?}", k).to_lowercase())
    });
    let title_opt = tool_call.fields.title.clone();

    let is_plan = title_opt.as_deref() == Some("Approve Plan")
        || tool_call
            .meta
            .as_ref()
            .and_then(|m| serde_json::to_value(m).ok())
            .and_then(|v| {
                v.pointer("/claudeCode/toolName")
                    .and_then(|s| s.as_str())
                    .map(|s| s == "ExitPlanMode")
            })
            .unwrap_or(false)
        || tool_call
            .fields
            .raw_input
            .as_ref()
            .and_then(|i| i.get("plan"))
            .is_some();

    if is_plan {
        let plan_text = tool_call
            .fields
            .raw_input
            .as_ref()
            .and_then(|i| i.get("plan"))
            .and_then(|p| p.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                tool_call.fields.content.as_ref().and_then(|content| {
                    content.iter().find_map(|item| match item {
                        agent_client_protocol_schema::ToolCallContent::Content(c) => {
                            match &c.content {
                                agent_client_protocol_schema::ContentBlock::Text(t) => {
                                    Some(t.text.clone())
                                }
                                _ => None,
                            }
                        }
                        _ => None,
                    })
                })
            })
            .unwrap_or_else(|| "Plan ready for approval".to_string());

        return (
            Some("Approve Plan".to_string()),
            plan_text,
            Some("switch_mode".to_string()),
        );
    }

    let description = if let Some(raw_input) = &tool_call.fields.raw_input {
        if let Some(cmd) = raw_input.get("command").and_then(|c| c.as_str()) {
            format!("Execute command: {}", cmd)
        } else if let Some(path) = raw_input
            .get("file_path")
            .or_else(|| raw_input.get("path"))
            .and_then(|p| p.as_str())
        {
            format!("File: {}", path)
        } else if let Some(desc) = raw_input.get("description").and_then(|d| d.as_str()) {
            desc.to_string()
        } else {
            tool_call
                .fields
                .title
                .clone()
                .unwrap_or_else(|| "Action requested".to_string())
        }
    } else if let Some(content) = &tool_call.fields.content {
        let text_parts: Vec<String> = content
            .iter()
            .filter_map(|item| match item {
                agent_client_protocol_schema::ToolCallContent::Content(c) => match &c.content {
                    agent_client_protocol_schema::ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        if !text_parts.is_empty() {
            text_parts.join("\n")
        } else {
            tool_call
                .fields
                .title
                .clone()
                .unwrap_or_else(|| "Action requested".to_string())
        }
    } else {
        tool_call
            .fields
            .title
            .clone()
            .unwrap_or_else(|| "Action requested".to_string())
    };

    (title_opt, description, kind_str)
}

#[cfg(test)]
mod tests {
    use super::agent_client_protocol_schema::{
        CreateTerminalRequest, PermissionOption, ReleaseTerminalRequest,
        SessionId as SchemaSessionId, TerminalOutputRequest, WaitForTerminalExitRequest,
        WriteTextFileRequest,
    };
    use super::*;
    use std::sync::Arc;

    fn make_handler(_policy: CallbackPolicy) -> CallbackHandler {
        let event_log = Arc::new(crate::events::EventLog::new(100));
        let tracker = Arc::new(TerminalTaskTracker::default());
        CallbackHandler::new(
            "test-session".into(),
            "test-agent".into(),
            event_log,
            std::env::temp_dir(),
            Arc::new(std::env::vars().collect()),
            tracker,
        )
    }

    #[test]
    fn test_callback_policy_default() {
        assert_eq!(CallbackPolicy::default(), CallbackPolicy::Ask);
    }

    #[test]
    fn test_callback_policy_serde() {
        let json = serde_json::to_string(&CallbackPolicy::AutoApprove).unwrap();
        assert_eq!(json, "\"auto-approve\"");
        let parsed: CallbackPolicy = serde_json::from_str("\"deny-all\"").unwrap();
        assert_eq!(parsed, CallbackPolicy::DenyAll);
    }

    #[tokio::test]
    async fn test_deny_all_rejects_read() {
        let handler = make_handler(CallbackPolicy::DenyAll);
        let req = ReadTextFileRequest::new("s1", std::path::PathBuf::from("/nonexistent"));
        let result = handler.handle_read_file(req).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_read_only_allows_read() {
        let handler = make_handler(CallbackPolicy::ReadOnly);
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "hello").unwrap();
        let req = ReadTextFileRequest::new("s1", tmp.path().to_path_buf());
        let result = handler.handle_read_file(req).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().content, "hello");
    }

    #[tokio::test]
    async fn test_direct_write_is_not_gated_by_batey_policy() {
        let handler = make_handler(CallbackPolicy::ReadOnly);
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let req = WriteTextFileRequest::new("s1", tmp.path().to_path_buf(), "data");
        let result = handler.handle_write_file(req).await;
        assert!(result.is_ok());
        assert!(handler.pending_permissions.read().await.is_empty());
    }

    #[tokio::test]
    async fn test_auto_approve_allows_write() {
        let handler = make_handler(CallbackPolicy::AutoApprove);
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let req = WriteTextFileRequest::new("s1", tmp.path().to_path_buf(), "written");
        let result = handler.handle_write_file(req).await;
        assert!(result.is_ok());
        let content = std::fs::read_to_string(tmp.path()).unwrap();
        assert_eq!(content, "written");
    }

    #[tokio::test]
    async fn test_auto_approve_allows_creating_new_file() {
        let temp = tempfile::tempdir().unwrap();
        let event_log = Arc::new(crate::events::EventLog::new(100));
        let tracker = Arc::new(TerminalTaskTracker::default());
        let handler = CallbackHandler::new(
            "test-session".into(),
            "test-agent".into(),
            event_log,
            temp.path().to_path_buf(),
            Arc::new(std::env::vars().collect()),
            tracker,
        );
        let target = temp.path().join("new-file.txt");

        let req = WriteTextFileRequest::new("s1", target.clone(), "created");
        handler.handle_write_file(req).await.unwrap();

        assert_eq!(std::fs::read_to_string(target).unwrap(), "created");
    }

    #[tokio::test]
    async fn additional_roots_are_independently_authorized_and_symlink_escapes_are_rejected() {
        let primary = tempfile::tempdir().unwrap();
        let additional = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let event_log = Arc::new(crate::events::EventLog::new(100));
        let tracker = Arc::new(TerminalTaskTracker::default());
        let handler = CallbackHandler::new_with_roots(
            "test-session".into(),
            "test-agent".into(),
            event_log,
            primary.path().to_path_buf(),
            vec![
                primary.path().canonicalize().unwrap(),
                additional.path().canonicalize().unwrap(),
            ],
            Arc::new(std::env::vars().collect()),
            tracker,
        );
        let additional_file = additional.path().join("shared.txt");
        std::fs::write(&additional_file, "shared").unwrap();

        let read = handler
            .handle_read_file(ReadTextFileRequest::new("s1", additional_file.clone()))
            .await
            .unwrap();
        assert_eq!(read.content, "shared");
        handler
            .handle_write_file(WriteTextFileRequest::new(
                "s1",
                additional_file.clone(),
                "updated",
            ))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&additional_file).unwrap(),
            "updated"
        );

        let outside_file = outside.path().join("outside.txt");
        std::fs::write(&outside_file, "outside").unwrap();
        assert!(handler
            .handle_read_file(ReadTextFileRequest::new("s1", outside_file.clone()))
            .await
            .is_err());

        #[cfg(unix)]
        {
            let escape = additional.path().join("escape");
            std::os::unix::fs::symlink(outside.path(), &escape).unwrap();
            assert!(handler
                .handle_read_file(ReadTextFileRequest::new("s1", escape.join("outside.txt")))
                .await
                .is_err());
        }
    }

    #[tokio::test]
    async fn test_request_permission_preserves_agent_options_and_waits_for_choice() {
        let handler = Arc::new(make_handler(CallbackPolicy::AutoApprove));

        let request = RequestPermissionRequest::new(
            SchemaSessionId::new("s1"),
            agent_client_protocol_schema::ToolCallUpdate::new(
                "tool-1",
                agent_client_protocol_schema::ToolCallUpdateFields::new(),
            ),
            vec![
                PermissionOption::new(
                    "allow-always",
                    "Always allow",
                    agent_client_protocol_schema::PermissionOptionKind::AllowAlways,
                ),
                PermissionOption::new(
                    "allow-once",
                    "Allow once",
                    agent_client_protocol_schema::PermissionOptionKind::AllowOnce,
                ),
            ],
        );

        let task = tokio::spawn({
            let handler = handler.clone();
            async move { handler.handle_request_permission(request).await }
        });
        tokio::task::yield_now().await;
        let permission_id = handler
            .pending_permissions
            .read()
            .await
            .keys()
            .next()
            .cloned()
            .expect("request must wait for a browser choice");
        assert!(
            handler
                .respond_permission(&permission_id, "allow-always")
                .await
        );
        let response = task.await.unwrap();
        match response.outcome {
            RequestPermissionOutcome::Selected(selected) => {
                assert_eq!(selected.option_id, "allow-always".into());
            }
            other => panic!("unexpected outcome: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_request_permission_can_select_a_reject_always_option() {
        let handler = Arc::new(make_handler(CallbackPolicy::ReadOnly));
        let request = RequestPermissionRequest::new(
            SchemaSessionId::new("s1"),
            agent_client_protocol_schema::ToolCallUpdate::new(
                "tool-1",
                agent_client_protocol_schema::ToolCallUpdateFields::new(),
            ),
            vec![
                PermissionOption::new(
                    "allow-1",
                    "Allow once",
                    agent_client_protocol_schema::PermissionOptionKind::AllowOnce,
                ),
                PermissionOption::new(
                    "deny-1",
                    "Reject once",
                    agent_client_protocol_schema::PermissionOptionKind::RejectOnce,
                ),
                PermissionOption::new(
                    "deny-always",
                    "Always reject",
                    agent_client_protocol_schema::PermissionOptionKind::RejectAlways,
                ),
            ],
        );

        let task = tokio::spawn({
            let handler = handler.clone();
            async move { handler.handle_request_permission(request).await }
        });
        tokio::task::yield_now().await;
        let permission_id = handler
            .pending_permissions
            .read()
            .await
            .keys()
            .next()
            .cloned()
            .expect("request must wait for a browser choice");
        assert!(
            handler
                .respond_permission(&permission_id, "deny-always")
                .await
        );
        let response = task.await.unwrap();
        match response.outcome {
            RequestPermissionOutcome::Selected(selected) => {
                assert_eq!(selected.option_id, "deny-always".into());
            }
            other => panic!("unexpected outcome: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_auto_approve_terminal_callbacks_work() {
        let handler = make_handler(CallbackPolicy::AutoApprove);
        let (command, args) = terminal_echo_command("hello-from-terminal");
        let create = CreateTerminalRequest::new("s1", command).args(args);
        let created = handler.handle_create_terminal(create).await.unwrap();
        assert!(handler.pending_permissions.read().await.is_empty());

        let waited = handler
            .handle_wait_for_terminal_exit(WaitForTerminalExitRequest::new(
                "s1",
                created.terminal_id.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(waited.exit_status.exit_code, Some(0));

        let output = handler
            .handle_terminal_output(TerminalOutputRequest::new(
                "s1",
                created.terminal_id.clone(),
            ))
            .await
            .unwrap();
        assert!(output.output.contains("hello-from-terminal"));

        handler
            .handle_release_terminal(ReleaseTerminalRequest::new("s1", created.terminal_id))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_terminal_output_is_truncated_to_limit() {
        let handler = make_handler(CallbackPolicy::AutoApprove);
        let (command, args) = terminal_echo_command("123456789");
        let create = CreateTerminalRequest::new("s1", command)
            .args(args)
            .output_byte_limit(5_u64);
        let created = handler.handle_create_terminal(create).await.unwrap();

        handler
            .handle_wait_for_terminal_exit(WaitForTerminalExitRequest::new(
                "s1",
                created.terminal_id.clone(),
            ))
            .await
            .unwrap();

        let output = handler
            .handle_terminal_output(TerminalOutputRequest::new(
                "s1",
                created.terminal_id.clone(),
            ))
            .await
            .unwrap();

        assert!(output.truncated);
        assert!(output.output.len() <= 5);
        assert!(output.output.contains("6789") || output.output.contains("789"));
    }

    #[tokio::test]
    async fn test_shutdown_releases_terminals() {
        let handler = make_handler(CallbackPolicy::AutoApprove);
        let (command, args) = terminal_echo_command("shutdown-check");
        let create = CreateTerminalRequest::new("s1", command).args(args);
        let created = handler.handle_create_terminal(create).await.unwrap();

        handler.shutdown().await;

        let result = handler
            .handle_terminal_output(TerminalOutputRequest::new("s1", created.terminal_id))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_respond_permission() {
        let handler = make_handler(CallbackPolicy::Ask);
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut pending = handler.pending_permissions.write().await;
            pending.insert(
                "perm-1".into(),
                PendingPermission {
                    tx,
                    option_ids: ["allow-once".to_string()].into_iter().collect(),
                },
            );
        }
        assert!(handler.respond_permission("perm-1", "allow-once").await);
        assert_eq!(
            rx.await.unwrap().map(|id| id.to_string()),
            Some("allow-once".into())
        );
    }

    #[tokio::test]
    async fn test_cancel_all_pending_is_cancel_not_denial() {
        let handler = make_handler(CallbackPolicy::Ask);
        let (tx1, rx1) = tokio::sync::oneshot::channel();
        let (tx2, rx2) = tokio::sync::oneshot::channel();
        {
            let mut pending = handler.pending_permissions.write().await;
            pending.insert(
                "p1".into(),
                PendingPermission {
                    tx: tx1,
                    option_ids: ["allow".to_string()].into_iter().collect(),
                },
            );
            pending.insert(
                "p2".into(),
                PendingPermission {
                    tx: tx2,
                    option_ids: ["allow".to_string()].into_iter().collect(),
                },
            );
        }
        handler.cancel_all_pending().await;
        // Cancellation resolves as None so the permission layer can answer
        // with ACP `cancelled` instead of recording a denial.
        assert_eq!(rx1.await.unwrap(), None);
        assert_eq!(rx2.await.unwrap(), None);
        assert!(handler.pending_permissions.read().await.is_empty());
    }

    fn terminal_echo_command(message: &str) -> (String, Vec<String>) {
        if cfg!(windows) {
            (
                "cmd".to_string(),
                vec!["/C".to_string(), format!("echo {}", message)],
            )
        } else {
            (
                "sh".to_string(),
                vec!["-c".to_string(), format!("printf '{}\\n'", message)],
            )
        }
    }

    #[test]
    fn test_format_permission_tool_call_plan_mode() {
        use super::agent_client_protocol_schema::{
            ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
        };
        let update = ToolCallUpdate::new(
            ToolCallId::new("call-1"),
            ToolCallUpdateFields::new()
                .title("Approve Plan".to_string())
                .raw_input(serde_json::json!({
                    "plan": "### Step 1: Fix bug\n### Step 2: Add test"
                })),
        );
        let (title, description, kind) = format_permission_tool_call(&update);
        assert_eq!(title, Some("Approve Plan".to_string()));
        assert_eq!(description, "### Step 1: Fix bug\n### Step 2: Add test");
        assert_eq!(kind, Some("switch_mode".to_string()));
    }

    #[test]
    fn test_format_permission_tool_call_command() {
        use super::agent_client_protocol_schema::{
            ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
        };
        let update = ToolCallUpdate::new(
            ToolCallId::new("call-2"),
            ToolCallUpdateFields::new()
                .title("Run tests".to_string())
                .raw_input(serde_json::json!({
                    "command": "cargo test --all"
                })),
        );
        let (title, description, kind) = format_permission_tool_call(&update);
        assert_eq!(title, Some("Run tests".to_string()));
        assert_eq!(description, "Execute command: cargo test --all");
        assert!(kind.is_none());
    }

    fn elicitation_map(
        pairs: Vec<(&str, agent_client_protocol_schema::ElicitationContentValue)>,
    ) -> std::collections::BTreeMap<String, agent_client_protocol_schema::ElicitationContentValue>
    {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    #[test]
    fn test_elicitation_validation_required_types_and_unknown() {
        use super::agent_client_protocol_schema::{
            ElicitationContentValue as V, ElicitationSchema,
        };
        let schema = ElicitationSchema::new()
            .string("name", true)
            .property(
                "age",
                agent_client_protocol_schema::IntegerPropertySchema::new(),
                false,
            )
            .property(
                "admin",
                agent_client_protocol_schema::BooleanPropertySchema::new(),
                false,
            );
        // Missing required field is rejected.
        assert!(validate_elicitation_content(&schema, &elicitation_map(vec![])).is_err());
        // Wrong types are rejected.
        assert!(validate_elicitation_content(
            &schema,
            &elicitation_map(vec![("name", V::Integer(3)), ("age", V::Integer(3))]),
        )
        .is_err());
        assert!(validate_elicitation_content(
            &schema,
            &elicitation_map(vec![
                ("name", V::String("a".into())),
                ("admin", V::String("x".into()))
            ]),
        )
        .is_err());
        // Unknown fields are rejected.
        assert!(validate_elicitation_content(
            &schema,
            &elicitation_map(vec![
                ("name", V::String("a".into())),
                ("zzz", V::String("b".into())),
            ]),
        )
        .is_err());
        // Valid content passes through, omitting unset optionals.
        let out = validate_elicitation_content(
            &schema,
            &elicitation_map(vec![
                ("name", V::String("a".into())),
                ("age", V::Integer(3)),
            ]),
        )
        .unwrap();
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn test_elicitation_validation_enum_and_bounds() {
        use super::agent_client_protocol_schema::{
            ElicitationContentValue as V, ElicitationSchema,
        };
        let schema = ElicitationSchema::new().property(
            "color",
            agent_client_protocol_schema::StringPropertySchema::new()
                .enum_values(vec!["red".to_string(), "blue".to_string()]),
            true,
        );
        assert!(validate_elicitation_content(
            &schema,
            &elicitation_map(vec![("color", V::String("green".into()))]),
        )
        .is_err());
        assert!(validate_elicitation_content(
            &schema,
            &elicitation_map(vec![("color", V::String("red".into()))]),
        )
        .is_ok());

        let bounded = ElicitationSchema::new()
            .property(
                "nick",
                agent_client_protocol_schema::StringPropertySchema::new()
                    .min_length(2_u32)
                    .max_length(4_u32),
                true,
            )
            .property(
                "n",
                agent_client_protocol_schema::IntegerPropertySchema::new()
                    .minimum(2_i64)
                    .maximum(4_i64),
                true,
            );
        let base = elicitation_map(vec![
            ("nick", V::String("abc".into())),
            ("n", V::Integer(3)),
        ]);
        assert!(validate_elicitation_content(&bounded, &base).is_ok());
        let mut short = base.clone();
        short.insert("nick".to_string(), V::String("a".into()));
        assert!(validate_elicitation_content(&bounded, &short).is_err());
        let mut high = base.clone();
        high.insert("n".to_string(), V::Integer(9));
        assert!(validate_elicitation_content(&bounded, &high).is_err());
        // Whole JSON numbers coerce into integer fields.
        let mut float_whole = base.clone();
        float_whole.insert("n".to_string(), V::Number(3.0));
        assert!(validate_elicitation_content(&bounded, &float_whole).is_ok());
        let mut float_frac = base;
        float_frac.insert("n".to_string(), V::Number(3.5));
        assert!(validate_elicitation_content(&bounded, &float_frac).is_err());
    }

    #[test]
    fn test_elicitation_validation_defaults_and_multiselect() {
        use super::agent_client_protocol_schema::{
            ElicitationContentValue as V, ElicitationSchema,
        };
        let schema = ElicitationSchema::new()
            .string("name", true)
            .property(
                "level",
                agent_client_protocol_schema::StringPropertySchema::new()
                    .default_value("low".to_string()),
                false,
            )
            .property(
                "tags",
                agent_client_protocol_schema::MultiSelectPropertySchema::new(vec![
                    "a".to_string(),
                    "b".to_string(),
                ])
                .min_items(1_u64)
                .max_items(2_u64),
                false,
            );
        // Omitted optionals fill declared defaults.
        let out = validate_elicitation_content(
            &schema,
            &elicitation_map(vec![("name", V::String("n".into()))]),
        )
        .unwrap();
        assert_eq!(out.get("level"), Some(&V::String("low".to_string())));
        // Membership and item counts are enforced.
        assert!(validate_elicitation_content(
            &schema,
            &elicitation_map(vec![
                ("name", V::String("n".into())),
                ("tags", V::StringArray(vec!["zzz".to_string()])),
            ]),
        )
        .is_err());
        assert!(validate_elicitation_content(
            &schema,
            &elicitation_map(vec![
                ("name", V::String("n".into())),
                ("tags", V::StringArray(vec![])),
            ]),
        )
        .is_err());
        assert!(validate_elicitation_content(
            &schema,
            &elicitation_map(vec![
                ("name", V::String("n".into())),
                (
                    "tags",
                    V::StringArray(vec!["a".to_string(), "b".to_string()])
                ),
            ]),
        )
        .is_ok());
    }

    #[test]
    fn test_elicitation_validation_rejects_unknown_property_types() {
        use super::agent_client_protocol_schema::{
            ElicitationContentValue as V, ElicitationSchema,
        };
        let schema: ElicitationSchema = serde_json::from_value(serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "weird": {"type": "_custom", "title": "Weird"},
            },
            "required": ["name"],
        }))
        .unwrap();
        // An omitted optional unknown-typed property stays acceptable...
        assert!(validate_elicitation_content(
            &schema,
            &elicitation_map(vec![("name", V::String("n".into()))]),
        )
        .is_ok());
        // ...but no value for it can be verified, so any submission fails
        // instead of reaching the agent as supposedly valid content.
        assert!(validate_elicitation_content(
            &schema,
            &elicitation_map(vec![
                ("name", V::String("n".into())),
                ("weird", V::String("x".into())),
            ]),
        )
        .is_err());
        let required: ElicitationSchema = serde_json::from_value(serde_json::json!({
            "type": "object",
            "properties": {"weird": {"type": "_custom"}},
            "required": ["weird"],
        }))
        .unwrap();
        assert!(validate_elicitation_content(&required, &elicitation_map(vec![]),).is_err());
    }

    #[tokio::test]
    async fn test_elicitation_rejected_accept_stays_pending() {
        // A submission that fails schema validation keeps the elicitation
        // answerable instead of resolving it.
        let event_log = Arc::new(crate::events::EventLog::new(100));
        let tracker = Arc::new(TerminalTaskTracker::default());
        let handler = Arc::new(CallbackHandler::new(
            "s1".into(),
            "codex".into(),
            event_log,
            std::env::temp_dir(),
            Arc::new(std::collections::HashMap::new()),
            tracker,
        ));
        let schema = agent_client_protocol_schema::ElicitationSchema::new().string("name", true);
        let scope = agent_client_protocol_schema::ElicitationSessionScope::new("sess-1");
        let req = agent_client_protocol_schema::CreateElicitationRequest::new(
            agent_client_protocol_schema::ElicitationFormMode::new(scope, schema),
            "Provide name",
        );
        let h = handler.clone();
        let handle =
            tokio::spawn(async move { h.handle_elicitation("rpc-reject".to_string(), req).await });
        let mut pending_id = None;
        for _ in 0..50 {
            if let Some(first) = handler.list_pending_elicitations().await.first() {
                pending_id = Some(first.id.clone());
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let pid = pending_id.expect("elicitation pending");
        let bad = serde_json::json!({});
        assert!(handler
            .respond_elicitation(&pid, "accept", Some(bad))
            .await
            .is_err());
        assert_eq!(handler.list_pending_elicitations().await.len(), 1);
        let good = serde_json::json!({"name": " Ada "});
        assert!(handler
            .respond_elicitation(&pid, "accept", Some(good))
            .await
            .unwrap());
        let resp = handle.await.unwrap();
        assert!(matches!(
            resp.action,
            agent_client_protocol_schema::ElicitationAction::Accept(_)
        ));
    }

    #[test]
    fn test_apply_read_window_honors_line_and_limit() {
        let content = "a\nb\nc\nd\n";
        assert_eq!(
            apply_read_window(content, None, None).unwrap(),
            "a\nb\nc\nd\n"
        );
        assert_eq!(
            apply_read_window(content, Some(2), None).unwrap(),
            "b\nc\nd\n"
        );
        assert_eq!(
            apply_read_window(content, Some(2), Some(2)).unwrap(),
            "b\nc"
        );
        assert_eq!(apply_read_window(content, Some(1), Some(0)).unwrap(), "");
        assert!(apply_read_window(content, Some(0), None).is_err());
        assert!(apply_read_window(content, Some(10), None).is_err());
    }

    #[tokio::test]
    async fn test_read_with_line_limit_slices_file() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("lines.txt");
        std::fs::write(&file, "one\ntwo\nthree\nfour\n").unwrap();
        let event_log = Arc::new(crate::events::EventLog::new(100));
        let tracker = Arc::new(TerminalTaskTracker::default());
        let handler = CallbackHandler::new(
            "s1".into(),
            "codex".into(),
            event_log,
            temp.path().to_path_buf(),
            Arc::new(std::collections::HashMap::new()),
            tracker,
        );
        let req = ReadTextFileRequest::new("s1", file.clone())
            .line(2_u32)
            .limit(2_u32);
        let resp = handler.handle_read_file(req).await.unwrap();
        assert_eq!(resp.content, "two\nthree");
        let bad = ReadTextFileRequest::new("s1", file).line(10_u32);
        assert!(handler.handle_read_file(bad).await.is_err());
    }

    #[tokio::test]
    async fn test_permission_cancel_returns_cancelled_not_denial() {
        let handler = Arc::new(make_handler(CallbackPolicy::Ask));
        let request = agent_client_protocol_schema::RequestPermissionRequest::new(
            SchemaSessionId::new("s1"),
            agent_client_protocol_schema::ToolCallUpdate::new(
                "tool-1",
                agent_client_protocol_schema::ToolCallUpdateFields::new(),
            ),
            vec![
                PermissionOption::new(
                    "allow-1",
                    "Allow once",
                    agent_client_protocol_schema::PermissionOptionKind::AllowOnce,
                ),
                PermissionOption::new(
                    "deny-1",
                    "Reject once",
                    agent_client_protocol_schema::PermissionOptionKind::RejectOnce,
                ),
            ],
        );
        // Spawn the permission and cancel before the user answers. The
        // outcome must be Cancelled, never a denial.
        let h = handler.clone();
        let handle = tokio::spawn(async move { h.handle_request_permission(request).await });
        // Wait for pending to appear, then cancel.
        for _ in 0..50 {
            if !handler.pending_permissions.read().await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        handler.cancel_pending_permissions().await;
        let resp = handle.await.unwrap();
        assert!(matches!(
            resp.outcome,
            agent_client_protocol_schema::RequestPermissionOutcome::Cancelled
        ));
    }

    #[tokio::test]
    async fn test_elicitation_form_accept_decline_cancel() {
        let handler = Arc::new(make_handler(CallbackPolicy::Ask));
        for action in ["accept", "decline", "cancel"] {
            let schema =
                agent_client_protocol_schema::ElicitationSchema::new().string("name", true);
            let scope = agent_client_protocol_schema::ElicitationSessionScope::new("sess-1");
            let req = agent_client_protocol_schema::CreateElicitationRequest::new(
                agent_client_protocol_schema::ElicitationFormMode::new(scope, schema),
                "Provide name",
            );
            let rpc_id = format!("rpc-{action}-{}", uuid::Uuid::new_v4());
            let h = handler.clone();
            // Run elicitation in background so we can answer it.
            let handle = tokio::spawn(async move { h.handle_elicitation(rpc_id, req).await });
            // Wait for pending to appear.
            let mut pending_id = None;
            for _ in 0..50 {
                let list = handler.list_pending_elicitations().await;
                if let Some(first) = list.first() {
                    pending_id = Some(first.id.clone());
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let pid = pending_id.expect("elicitation pending");
            let content = if action == "accept" {
                Some(serde_json::json!({"name": " Ada "}))
            } else {
                None
            };
            assert!(handler
                .respond_elicitation(&pid, action, content)
                .await
                .unwrap());
            let resp = handle.await.unwrap();
            match action {
                "accept" => assert!(matches!(
                    resp.action,
                    agent_client_protocol_schema::ElicitationAction::Accept(_)
                )),
                "decline" => assert!(matches!(
                    resp.action,
                    agent_client_protocol_schema::ElicitationAction::Decline
                )),
                _ => assert!(matches!(
                    resp.action,
                    agent_client_protocol_schema::ElicitationAction::Cancel
                )),
            }
        }
    }

    #[tokio::test]
    async fn test_elicitation_url_does_not_prefetch_and_cancels_cleanly() {
        let handler = Arc::new(make_handler(CallbackPolicy::Ask));
        let scope = agent_client_protocol_schema::ElicitationSessionScope::new("sess-1");
        let req = agent_client_protocol_schema::CreateElicitationRequest::new(
            agent_client_protocol_schema::ElicitationUrlMode::new(
                scope,
                "elic-123",
                "https://example.invalid/auth",
            ),
            "Sign in",
        );
        let rpc_id = format!("rpc-url-{}", uuid::Uuid::new_v4());
        let h = handler.clone();
        let handle = tokio::spawn(async move { h.handle_elicitation(rpc_id, req).await });
        let mut pending_id = None;
        for _ in 0..50 {
            let list = handler.list_pending_elicitations().await;
            if let Some(first) = list.first() {
                pending_id = Some(first.clone());
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let info = pending_id.expect("url pending");
        // URL is exposed for explicit user action; nothing is fetched here.
        assert_eq!(info.mode, "url");
        assert_eq!(info.url.as_deref(), Some("https://example.invalid/auth"));
        assert_eq!(info.elicitation_id.as_deref(), Some("elic-123"));
        // Cancel while pending resolves as Cancel and records only the action.
        handler.cancel_pending_elicitations().await;
        let resp = handle.await.unwrap();
        assert!(matches!(
            resp.action,
            agent_client_protocol_schema::ElicitationAction::Cancel
        ));
        assert!(handler.list_pending_elicitations().await.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn releasing_terminal_reaps_descendants() {
        let temp = tempfile::tempdir().unwrap();
        let tracker = Arc::new(TerminalTaskTracker::default());
        let handler = CallbackHandler::new(
            "session-1".into(),
            "codex".into(),
            Arc::new(crate::events::EventLog::new(100)),
            temp.path().to_path_buf(),
            Arc::new(std::env::vars().collect()),
            tracker,
        );
        let response = handler
            .handle_create_terminal(
                CreateTerminalRequest::new(SchemaSessionId::new("s1"), "sh").args(vec![
                    "-c".into(),
                    "sleep 30 & echo $! > child.pid; wait".into(),
                ]),
            )
            .await
            .unwrap();
        let pid_file = temp.path().join("child.pid");
        for _ in 0..100 {
            if pid_file.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        handler
            .handle_release_terminal(ReleaseTerminalRequest::new(
                SchemaSessionId::new("s1"),
                response.terminal_id,
            ))
            .await
            .unwrap();
        for _ in 0..100 {
            if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("terminal descendant {pid} survived release");
    }
}
