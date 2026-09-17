//! Chat and turn operations.
use super::{
    workspaces::workspace_error, ChatHistoryPage, ChatView, HubService, ServiceError, ServiceResult,
};
use crate::store::{
    validate_name, AdditionalRoot, Chat, ChatWorkspace, McpServerConfig, McpServerInput,
    McpServerView, McpTransport, SecretEdit, SecretField, WorkspaceMode,
};
use crate::workspace::{self, BranchInfo, ManagedPaths, RepoInfo};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Optional workspace selection supplied with chat creation.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSelection {
    pub mode: WorkspaceMode,
    pub branch: Option<String>,
}

/// The fields `edit_chat` may change. `None` leaves a field alone.
#[derive(Debug, Default, Clone)]
pub struct ChatEdit {
    pub title: Option<String>,
    pub archived: Option<bool>,
}

/// A prompt must fit this range. The limit keeps one request from filling the
/// event log and the agent's context at once.
const MAX_PROMPT_BYTES: usize = 100_000;

#[derive(Debug, Clone, serde::Serialize)]
pub struct AdditionalRootView {
    pub project_id: String,
    pub position: i64,
}

fn validate_mcp_input(input: &McpServerInput) -> ServiceResult<()> {
    if input.name.trim().is_empty() || input.name.len() > 256 {
        return Err(ServiceError::Invalid("MCP server name is required".into()));
    }
    match input.transport {
        McpTransport::Http | McpTransport::Sse => {
            let url = input.url.as_deref().ok_or_else(|| {
                ServiceError::Invalid("HTTP and SSE MCP servers require a URL".into())
            })?;
            // Avoid parsing/exposing a raw URL in errors: userinfo is a credential.
            let scheme = url.split("://").next().unwrap_or("");
            if !matches!(scheme, "http" | "https")
                || url.split("://").nth(1).is_none()
                || url.split("://").nth(1).is_some_and(|rest| {
                    rest.split('/')
                        .next()
                        .is_some_and(|authority| authority.contains('@'))
                })
            {
                return Err(ServiceError::Invalid(
                    "MCP URL must be an http(s) URL without userinfo".into(),
                ));
            }
            if input.command.is_some() {
                return Err(ServiceError::Invalid(
                    "HTTP and SSE MCP servers cannot include a command".into(),
                ));
            }
        }
        McpTransport::Stdio => {
            let command = input.command.as_deref().ok_or_else(|| {
                ServiceError::Invalid("Stdio MCP servers require an absolute command path".into())
            })?;
            if !std::path::Path::new(command).is_absolute() || input.url.is_some() {
                return Err(ServiceError::Invalid(
                    "Stdio MCP command must be absolute and cannot include a URL".into(),
                ));
            }
        }
    }
    for secret in &input.secrets {
        if secret.name.trim().is_empty() || secret.name.contains(['\r', '\n']) {
            return Err(ServiceError::Invalid("MCP secret name is invalid".into()));
        }
        if secret.action == SecretEdit::Replace && secret.value.as_deref().unwrap_or("").is_empty()
        {
            return Err(ServiceError::Invalid(
                "A replacement MCP secret value is required".into(),
            ));
        }
    }
    Ok(())
}
pub const DEFAULT_HISTORY_PAGE_SIZE: usize = 100;
pub const MAX_HISTORY_PAGE_SIZE: usize = 200;

impl HubService {
    /// Adds the live process and turn state to a stored chat. A chat with no
    /// live session reads as stopped and idle.
    pub(crate) async fn view(&self, chat: Chat) -> ChatView {
        let (process_state, turn_state) = match self.sessions.get_by_id(&chat.id).await {
            Some(live) => (
                live.process_state().await.to_string(),
                live.turn_state().await.to_string(),
            ),
            None => ("STOPPED".into(), "IDLE".into()),
        };
        let workspace = match self.store.workspace(&chat.id) {
            Ok(workspace) => workspace.map(Into::into),
            Err(error) => {
                tracing::warn!(
                    chat_id = %chat.id,
                    %error,
                    "Failed to read chat workspace metadata; omitting display summary"
                );
                None
            }
        };
        let turn_started_at = match self.store.active_turn_started_at(&chat.id) {
            Ok(started_at) => started_at,
            Err(error) => {
                tracing::warn!(
                    chat_id = %chat.id,
                    %error,
                    "Failed to read active turn start; omitting timer metadata"
                );
                None
            }
        };
        let active_tasks = self
            .sessions
            .task_tracker()
            .active_task_count(&chat.id)
            .await;
        ChatView {
            chat,
            turn_started_at,
            process_state,
            turn_state,
            workspace,
            active_tasks,
        }
    }

    pub async fn list_chats(&self, project_id: &str) -> ServiceResult<Vec<ChatView>> {
        self.store.project(project_id)?;
        let chats: Vec<Chat> = self
            .store
            .chats()?
            .into_iter()
            .filter(|c| c.project_id == project_id)
            .collect();
        let mut views = Vec::with_capacity(chats.len());
        for chat in chats {
            views.push(self.view(chat).await);
        }
        Ok(views)
    }

    pub async fn get_chat(&self, chat_id: &str) -> ServiceResult<ChatView> {
        let chat = self.store.chat(chat_id)?;
        Ok(self.view(chat).await)
    }

    pub fn mcp_servers(&self, chat_id: &str) -> ServiceResult<Vec<McpServerView>> {
        self.store.chat(chat_id)?;
        Ok(self
            .store
            .mcp_servers(chat_id)?
            .iter()
            .map(McpServerConfig::redacted)
            .collect())
    }

    pub async fn create_mcp_server(
        &self,
        chat_id: &str,
        input: McpServerInput,
    ) -> ServiceResult<Vec<McpServerView>> {
        validate_mcp_input(&input)?;
        let live = self.live(chat_id).await?;
        let mut existing = self.store.mcp_servers(chat_id)?;
        let secrets = input
            .secrets
            .into_iter()
            .filter_map(|s| {
                s.value.map(|value| SecretField {
                    name: s.name,
                    value,
                })
            })
            .collect();
        let value = McpServerConfig {
            id: uuid::Uuid::new_v4().to_string(),
            chat_id: chat_id.into(),
            position: existing.len() as i64,
            name: input.name.trim().into(),
            transport: input.transport,
            url: input.url,
            command: input.command,
            args: input.args,
            secrets,
        };
        let saved = value.clone();
        live.change_connection_config(move |store| store.insert_mcp_server(&saved))
            .await?;
        existing.push(value);
        self.notify_metadata_changed();
        Ok(existing.iter().map(McpServerConfig::redacted).collect())
    }

    pub async fn edit_mcp_server(
        &self,
        chat_id: &str,
        id: &str,
        input: McpServerInput,
    ) -> ServiceResult<Vec<McpServerView>> {
        validate_mcp_input(&input)?;
        let live = self.live(chat_id).await?;
        let mut value = self.store.mcp_server(chat_id, id)?;
        let mut secrets = value.secrets.clone();
        for edit in input.secrets {
            match edit.action {
                SecretEdit::Keep => {
                    if !secrets.iter().any(|s| s.name == edit.name) {
                        return Err(ServiceError::Invalid(
                            "Cannot keep an unknown MCP secret".into(),
                        ));
                    }
                }
                SecretEdit::Remove => secrets.retain(|s| s.name != edit.name),
                SecretEdit::Replace => {
                    secrets.retain(|s| s.name != edit.name);
                    secrets.push(SecretField {
                        name: edit.name,
                        value: edit.value.unwrap(),
                    });
                }
            }
        }
        value.name = input.name.trim().into();
        value.transport = input.transport;
        value.url = input.url;
        value.command = input.command;
        value.args = input.args;
        value.secrets = secrets;
        live.change_connection_config(move |store| store.update_mcp_server(&value))
            .await?;
        self.notify_metadata_changed();
        self.mcp_servers(chat_id)
    }
    pub async fn delete_mcp_server(&self, chat_id: &str, id: &str) -> ServiceResult<()> {
        let live = self.live(chat_id).await?;
        let id = id.to_string();
        let chat = chat_id.to_string();
        live.change_connection_config(move |store| store.delete_mcp_server(&chat, &id))
            .await?;
        self.notify_metadata_changed();
        Ok(())
    }
    pub async fn reorder_mcp_servers(
        &self,
        chat_id: &str,
        ids: Vec<String>,
    ) -> ServiceResult<Vec<McpServerView>> {
        let live = self.live(chat_id).await?;
        let chat = chat_id.to_string();
        live.change_connection_config(move |store| store.reorder_mcp_servers(&chat, &ids))
            .await?;
        self.notify_metadata_changed();
        self.mcp_servers(chat_id)
    }
    pub fn additional_roots(&self, chat_id: &str) -> ServiceResult<Vec<AdditionalRootView>> {
        self.store.chat(chat_id)?;
        Ok(self
            .store
            .additional_roots(chat_id)?
            .into_iter()
            .map(|r| AdditionalRootView {
                project_id: r.project_id,
                position: r.position,
            })
            .collect())
    }
    pub async fn set_additional_roots(
        &self,
        chat_id: &str,
        project_ids: Vec<String>,
    ) -> ServiceResult<Vec<AdditionalRootView>> {
        let mut roots = Vec::new();
        for (position, id) in project_ids.iter().enumerate() {
            let project = self.store.project(id)?;
            let canonical = std::path::Path::new(&project.path)
                .canonicalize()
                .map_err(|_| {
                    ServiceError::Invalid("An additional project directory no longer exists".into())
                })?;
            if canonical.to_string_lossy() != project.path {
                return Err(ServiceError::Invalid(
                    "An additional project path changed; re-register it before use".into(),
                ));
            };
            roots.push(AdditionalRoot {
                chat_id: chat_id.into(),
                position: position as i64,
                project_id: id.clone(),
                canonical_path: project.path,
            });
        }
        let mut seen = std::collections::HashSet::new();
        if !project_ids.iter().all(|id| seen.insert(id)) {
            return Err(ServiceError::Invalid(
                "Additional projects must be unique".into(),
            ));
        };
        let live = self.live(chat_id).await?;
        let chat = chat_id.to_string();
        live.change_connection_config(move |store| store.replace_additional_roots(&chat, &roots))
            .await?;
        self.notify_metadata_changed();
        self.additional_roots(chat_id)
    }

    pub fn chat_history(
        &self,
        chat_id: &str,
        before_seq: Option<u64>,
        through_seq: Option<u64>,
        limit: usize,
    ) -> ServiceResult<ChatHistoryPage> {
        self.store.chat(chat_id)?;
        let limit = limit.clamp(1, MAX_HISTORY_PAGE_SIZE);
        let (events, has_older) =
            self.store
                .chat_event_page(chat_id, before_seq, through_seq, limit)?;
        let next_cursor = has_older
            .then(|| events.first().map(|event| event.seq))
            .flatten();
        Ok(ChatHistoryPage {
            events,
            next_cursor,
            has_older,
        })
    }

    pub async fn create_chat(
        &self,
        project_id: &str,
        agent: &str,
        title: Option<String>,
    ) -> ServiceResult<ChatView> {
        self.create_chat_with_workspace(project_id, agent, title, None)
            .await
    }

    /// Create a chat and, for Git projects, provision its selected workspace
    /// before publishing either row or the metadata invalidation event.
    pub async fn create_chat_with_workspace(
        &self,
        project_id: &str,
        agent: &str,
        title: Option<String>,
        selection: Option<WorkspaceSelection>,
    ) -> ServiceResult<ChatView> {
        if !self.sessions.has_agent(agent) {
            return Err(ServiceError::Invalid("Unknown agent".into()));
        }
        let project = self.store.project(project_id)?;
        let _workspace_guard = self.workspace_lock.lock().await;
        let project_path = PathBuf::from(&project.path);
        let (info, branches) = inspect_with_branches(project_path.clone()).await?;

        if !info.is_git {
            if selection.is_some() {
                return Err(ServiceError::Invalid(
                    "Git workspace selection is only valid for Git projects".into(),
                ));
            }
            let chat = self
                .store
                .create_chat(project_id.to_string(), agent.to_string(), title)?;
            self.notify_metadata_changed();
            return Ok(self.view(chat).await);
        }

        let root = info
            .root
            .clone()
            .ok_or_else(|| ServiceError::Internal(anyhow::anyhow!("Git root is unavailable")))?;
        let project_subdir = info
            .subdir
            .clone()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let project_id_owned = project_id.to_string();
        let chat = self
            .store
            .new_chat(project_id.to_string(), agent.to_string(), title)?;
        let mode = selection
            .as_ref()
            .map(|choice| choice.mode)
            .unwrap_or(WorkspaceMode::ManagedWorktree);
        let requested_branch = selection
            .as_ref()
            .and_then(|choice| choice.branch.as_deref())
            .map(str::trim)
            .filter(|branch| !branch.is_empty())
            .map(ToOwned::to_owned);
        // Every direct checkout operation must reserve the same mutex that
        // direct and legacy turns use. The branch in `info` is only a
        // snapshot from before this reservation.
        let _checkout_guard = (mode == WorkspaceMode::ProjectCheckout)
            .then(|| {
                self.sessions
                    .try_acquire_checkout_guard(&root)
                    .map_err(|error| ServiceError::Conflict(error.to_string()))
            })
            .transpose()?;

        let (workspace, managed_paths) = match mode {
            WorkspaceMode::ManagedWorktree => {
                let base_commit = match requested_branch.as_deref() {
                    Some(branch) => local_branch_sha(&branches, branch)?,
                    None => {
                        if info.dirty {
                            return Err(ServiceError::Conflict(
                                "Git checkout is dirty; choose an explicit workspace mode and branch"
                                    .into(),
                            ));
                        }
                        info.head_sha.clone().ok_or_else(|| {
                            ServiceError::Invalid("Git HEAD is not a commit".into())
                        })?
                    }
                };
                let repo_for_blocking = root.clone();
                let workspace_root = self.store.managed_worktree_root();
                let chat_id = chat.id.clone();
                let cleanup_chat_id = chat_id.clone();
                let subdir = PathBuf::from(&project_subdir);
                let provision_base_commit = base_commit.clone();
                let (paths, workspace) = tokio::task::spawn_blocking(move || {
                    let paths = workspace::provision_managed(
                        &repo_for_blocking,
                        &workspace_root,
                        &chat_id,
                        &provision_base_commit,
                    )
                    .map_err(workspace_error)?;
                    let effective = paths.worktree.join(&subdir);
                    if !effective.is_dir() {
                        let message = format!(
                            "Project subdirectory does not exist in managed worktree: {}",
                            subdir.display()
                        );
                        let cleanup = cleanup_managed(
                            &repo_for_blocking,
                            &workspace_root,
                            &paths,
                            &cleanup_chat_id,
                            &provision_base_commit,
                        );
                        return Err(managed_cleanup_error(message, cleanup));
                    }
                    let workspace = ChatWorkspace::new(
                        chat_id,
                        project_id_owned,
                        WorkspaceMode::ManagedWorktree,
                        repo_for_blocking.to_string_lossy().into_owned(),
                        paths.worktree.to_string_lossy().into_owned(),
                        subdir.to_string_lossy().into_owned(),
                        Some(paths.branch.clone()),
                        Some(provision_base_commit),
                    );
                    Ok::<_, ServiceError>((paths, workspace))
                })
                .await
                .map_err(|error| ServiceError::Internal(anyhow::anyhow!(error)))??;
                (workspace, Some((paths, base_commit)))
            }
            WorkspaceMode::ProjectCheckout => {
                let (current_info, _) = inspect_with_branches(project_path.clone()).await?;
                let branch = requested_branch
                    .or(current_info.branch.clone())
                    .ok_or_else(|| {
                        ServiceError::Conflict(
                            "Project checkout mode requires an attached local branch".into(),
                        )
                    })?;
                // `prepare_direct` refuses dirty branch switches, but a live
                // direct/legacy chat must be checked before we ask it to do so.
                if current_info.branch.as_deref() != Some(branch.as_str()) {
                    self.ensure_primary_checkout_available(&root).await?;
                }
                let repo_for_blocking = root.clone();
                let subdir = PathBuf::from(&project_subdir);
                let branch_for_blocking = branch.clone();
                let base_commit = tokio::task::spawn_blocking(move || {
                    workspace::prepare_direct(&repo_for_blocking, &branch_for_blocking)
                        .map_err(workspace_error)?;
                    let effective = repo_for_blocking.join(&subdir);
                    if !effective.is_dir() {
                        return Err(ServiceError::Invalid(format!(
                            "Project subdirectory does not exist in project checkout: {}",
                            subdir.display()
                        )));
                    }
                    workspace::resolve_ref(&repo_for_blocking, &branch_for_blocking)
                        .map_err(workspace_error)
                })
                .await
                .map_err(|error| ServiceError::Internal(anyhow::anyhow!(error)))??;
                let workspace = ChatWorkspace::new(
                    chat.id.clone(),
                    project_id.to_string(),
                    WorkspaceMode::ProjectCheckout,
                    root.to_string_lossy().into_owned(),
                    root.to_string_lossy().into_owned(),
                    project_subdir,
                    Some(branch),
                    Some(base_commit),
                );
                (workspace, None)
            }
        };

        if let Err(error) = self.store.insert_chat_with_workspace(&chat, &workspace) {
            if let Some((paths, base_commit)) = managed_paths {
                let repo = root.clone();
                let workspace_root = self.store.managed_worktree_root();
                let chat_id = chat.id.clone();
                let cleanup = tokio::task::spawn_blocking(move || {
                    cleanup_managed(&repo, &workspace_root, &paths, &chat_id, &base_commit)
                })
                .await
                .map_err(|join| ServiceError::Internal(anyhow::anyhow!(join)))?;
                if let Err(cleanup_error) = cleanup {
                    tracing::error!(
                        chat_id = %chat.id,
                        %cleanup_error,
                        "Managed chat creation persistence failed; orphan branch/worktree preserved"
                    );
                    return Err(ServiceError::Internal(anyhow::anyhow!(
                        "Could not persist chat: {error}; cleanup did not complete: {cleanup_error}"
                    )));
                }
            }
            return Err(ServiceError::Internal(anyhow::anyhow!(
                "Could not persist chat: {error}"
            )));
        }
        self.notify_metadata_changed();
        Ok(self.view(chat).await)
    }

    pub(super) async fn ensure_primary_checkout_available(
        &self,
        repository_root: &Path,
    ) -> ServiceResult<()> {
        let chats = self.store.chats()?;
        let projects = self.store.projects()?;
        let workspaces = self.store.list_workspaces()?;
        let root = repository_root.to_path_buf();
        let direct_chat_ids = tokio::task::spawn_blocking(move || {
            let project_paths: HashMap<_, _> = projects
                .into_iter()
                .map(|project| (project.id, PathBuf::from(project.path)))
                .collect();
            let mut ids = Vec::new();
            for chat in chats {
                let workspace = workspaces.iter().find(|ws| ws.chat_id == chat.id);
                match workspace {
                    Some(ws) if ws.mode == WorkspaceMode::ManagedWorktree => continue,
                    Some(ws)
                        if ws.mode == WorkspaceMode::ProjectCheckout
                            && Path::new(&ws.repository_root) == root =>
                    {
                        ids.push(chat.id);
                    }
                    None => {
                        let Some(path) = project_paths.get(&chat.project_id) else {
                            continue;
                        };
                        let info = workspace::inspect(path).map_err(workspace_error)?;
                        if info.root.as_ref() == Some(&root) {
                            ids.push(chat.id);
                        }
                    }
                    _ => {}
                }
            }
            Ok::<_, ServiceError>(ids)
        })
        .await
        .map_err(|error| ServiceError::Internal(anyhow::anyhow!(error)))??;

        for chat_id in direct_chat_ids {
            if let Some(session) = self.sessions.get_by_id(&chat_id).await {
                if session.process_state().await.is_running() {
                    return Err(ServiceError::Conflict(format!(
                        "Cannot switch the primary checkout while chat {chat_id} has a live Batey process"
                    )));
                }
            }
        }
        Ok(())
    }

    pub async fn edit_chat(&self, chat_id: &str, edit: ChatEdit) -> ServiceResult<ChatView> {
        let title = edit
            .title
            .map(|title| validate_name(&title).map(|()| title.trim().to_string()))
            .transpose()?;
        let live = self.live(chat_id).await?;
        let chat = live.edit_metadata(title, edit.archived).await?;
        self.notify_metadata_changed();
        Ok(self.view(chat).await)
    }

    /// Deletion does not require the chat's agent to still be configured.
    /// A live session runs the normal shutdown-then-cleanup sequence; a
    /// historical chat whose agent is gone has no session to ask, so its
    /// durable chat/workspace metadata is the only authority. Both paths
    /// share the same worktree/branch safety rules (see
    /// `session::resolve_managed_cleanup`).
    pub async fn delete_chat(&self, chat_id: &str) -> ServiceResult<()> {
        let chat = self.store.chat(chat_id)?;
        let result = if self.sessions.has_agent(&chat.agent) {
            let live = self
                .sessions
                .get_by_id(chat_id)
                .await
                .ok_or_else(|| ServiceError::NotFound("Chat not found".into()))?;
            live.delete_metadata().await
        } else {
            crate::session::delete_chat_durable(&self.store, &chat).await
        };
        result.map_err(map_chat_deletion_error)?;
        self.events.forget_chat(chat_id);
        self.sessions.remove_session(chat_id).await;
        self.sessions.task_tracker().forget_chat(chat_id).await;
        self.notify_metadata_changed();
        Ok(())
    }

    /// Starts a turn and returns as soon as it is accepted.
    ///
    /// The turn runs in a task that owns the session, so a client that
    /// disconnects cannot cancel work the agent already started. Every
    /// completion and failure reaches the caller over the event stream.
    pub async fn prompt_chat(&self, chat_id: &str, text: String) -> ServiceResult<()> {
        if text.trim().is_empty() || text.len() > MAX_PROMPT_BYTES {
            return Err(ServiceError::Invalid(format!(
                "Prompt must contain 1–{MAX_PROMPT_BYTES} bytes"
            )));
        }
        self.prompt_chat_content(chat_id, vec![crate::content::text(text)])
            .await
    }

    /// Stable ACP rich prompt surface. `prompt_chat` remains the text-only
    /// shorthand for callers that predate content blocks.
    pub async fn prompt_chat_content(
        &self,
        chat_id: &str,
        content: Vec<::agent_client_protocol_schema::v1::ContentBlock>,
    ) -> ServiceResult<()> {
        crate::content::validate_prompt(&content)
            .map_err(|error| ServiceError::Invalid(error.to_string()))?;
        let live = self.live(chat_id).await?;
        let timeout = self.prompt_timeout;
        if let Err(error) = live.start_turn_content(content, timeout).await {
            if let Some(rejected) = error.downcast_ref::<crate::acp::SavedConfigRejected>() {
                return Err(ServiceError::SavedConfigRejected {
                    option_id: rejected.option_id.clone(),
                    message: rejected.message.clone(),
                });
            }
            return Err(error.into());
        }
        Ok(())
    }

    pub async fn resume_chat(&self, chat_id: &str) -> ServiceResult<ChatView> {
        // The agent id is needed to record observed auth evidence.
        let agent_id = self
            .store
            .chat(chat_id)
            .map(|chat| chat.agent)
            .unwrap_or_default();
        if let Err(error) = self.live(chat_id).await?.resume().await {
            if let Some(rejected) = error.downcast_ref::<crate::acp::SavedConfigRejected>() {
                return Err(ServiceError::SavedConfigRejected {
                    option_id: rejected.option_id.clone(),
                    message: rejected.message.clone(),
                });
            }
            if let Some(crate::workspace_env::WorkspaceEnvError::EnvrcBlocked {
                path,
                relative_path,
                message,
            }) = error.downcast_ref::<crate::workspace_env::WorkspaceEnvError>()
            {
                return Err(ServiceError::EnvrcBlocked {
                    path: path.clone(),
                    relative_path: relative_path.clone(),
                    message: message.clone(),
                });
            }
            if error.downcast_ref::<crate::acp::AuthRequired>().is_some() && !agent_id.is_empty() {
                // Stable `auth_required` is observed evidence, never a guess.
                self.note_agent_auth_required(&agent_id);
            }
            return Err(error.into());
        }
        if !agent_id.is_empty() {
            // A successful setup reinforces `authenticated` only when Batey
            // already saw `authentication_required`. `Unknown` stays unknown.
            self.note_agent_session_success(&agent_id);
        }
        let chat = self.store.chat(chat_id)?;
        Ok(self.view(chat).await)
    }

    pub async fn authorize_chat_environment(
        &self,
        chat_id: &str,
        remember: bool,
    ) -> ServiceResult<ChatView> {
        let chat = self.store.chat(chat_id)?;
        if chat.archived {
            return Err(ServiceError::Invalid(
                "Chat is archived; restore it before authorizing its environment".into(),
            ));
        }
        let project = self.store.project(&chat.project_id)?;
        // `project` is moved into the `spawn_blocking` closure below; the
        // remember flow needs the id again afterward, so capture it first.
        let project_id = project.id.clone();
        let workspace = self.store.workspace(&chat.id)?;
        let state_worktrees = self.store.worktrees_dir();
        let session = self.live(chat_id).await?;

        let cwd = session.cwd().to_path_buf();
        tokio::task::spawn_blocking(move || {
            crate::session::validate_persistent_workspace(
                &chat,
                &project,
                workspace.as_ref(),
                &state_worktrees,
                &cwd,
            )
        })
        .await
        .map_err(|e| ServiceError::Internal(e.into()))??;

        let boundary = session.workspace_boundary().to_path_buf();
        let cwd = session.cwd().to_path_buf();

        // Fingerprint before the allow so a remembered grant is only ever
        // written for content the user actually saw approved, never for
        // content a race (or a symlink escaping the boundary) substituted
        // in the window around the `direnv allow` call.
        let before_fp = if remember {
            Some(
                crate::workspace_env::envrc_fingerprint(&cwd, &boundary)
                    .map_err(|e| ServiceError::Internal(e.into()))?,
            )
        } else {
            None
        };

        crate::workspace_env::direnv_allow(&cwd, &boundary).await?;

        if remember {
            let after_fp = crate::workspace_env::envrc_fingerprint(&cwd, &boundary)
                .map_err(|e| ServiceError::Internal(e.into()))?;
            match (before_fp.flatten(), after_fp) {
                (Some(before), Some(after)) if before == after => {
                    self.store.remember_project_envrc_grant(
                        &project_id,
                        after.relative_path,
                        after.content_hash,
                    )?;
                }
                _ => {
                    // Content changed during approval, vanished, or its
                    // .envrc resolves outside the boundary via a symlink:
                    // never persist a grant for content the user did not
                    // actually see approved. Revert the allow and fail
                    // closed rather than silently degrading to a
                    // workspace-only allow the user did not ask for.
                    let _ = crate::workspace_env::direnv_deny(&cwd, &boundary).await;
                    return Err(ServiceError::Invalid(
                        "The .envrc content changed while it was being approved. \
                         Approve it again."
                            .into(),
                    ));
                }
            }
        }

        session.invalidate_cached_env().await;
        let new_env = crate::workspace_env::resolve_workspace_env(&cwd, &boundary).await?;
        session.set_cached_env(new_env).await;
        self.resume_chat(chat_id).await
    }

    pub async fn forget_project_envrc_grant(&self, project_id: &str) -> ServiceResult<()> {
        self.store.project(project_id)?;
        self.store.forget_project_envrc_grant(project_id)?;
        Ok(())
    }

    pub async fn list_chat_tasks(
        &self,
        chat_id: &str,
    ) -> ServiceResult<Vec<crate::tasks::TerminalTaskSummary>> {
        self.store.chat(chat_id)?;
        Ok(self.sessions.task_tracker().list_chat_tasks(chat_id).await)
    }

    pub async fn get_chat_task(
        &self,
        chat_id: &str,
        task_id: &str,
    ) -> ServiceResult<crate::tasks::TerminalTaskDetails> {
        self.store.chat(chat_id)?;
        let task = self
            .sessions
            .task_tracker()
            .get_task(task_id)
            .await
            .ok_or_else(|| ServiceError::NotFound(format!("Task {task_id} not found")))?;
        if task.chat_id != chat_id {
            return Err(ServiceError::NotFound(format!(
                "Task {task_id} does not belong to chat {chat_id}"
            )));
        }
        Ok(task.details().await)
    }

    pub async fn stop_chat_task(
        &self,
        chat_id: &str,
        task_id: &str,
    ) -> ServiceResult<crate::tasks::TerminalTaskSummary> {
        self.store.chat(chat_id)?;
        let task = self
            .sessions
            .task_tracker()
            .get_task(task_id)
            .await
            .ok_or_else(|| ServiceError::NotFound(format!("Task {task_id} not found")))?;
        if task.chat_id != chat_id {
            return Err(ServiceError::NotFound(format!(
                "Task {task_id} does not belong to chat {chat_id}"
            )));
        }
        if !task.managed {
            return Err(ServiceError::Conflict(
                "This task is reported by the agent and does not support stopping".into(),
            ));
        }
        task.stop();
        Ok(task.summary().await)
    }

    pub async fn clear_saved_config(&self, chat_id: &str, option_id: &str) -> ServiceResult<()> {
        let live = self.live(chat_id).await?;
        if live.turn_state().await != crate::state::TurnState::Idle {
            return Err(ServiceError::Conflict(
                "Wait for the active turn before resetting configuration".into(),
            ));
        }
        let chat = self.store.update_chat(chat_id, |chat| {
            if let Some(values) = chat.config_values.as_object_mut() {
                values.remove(option_id);
            }
        })?;
        if chat.config_values.get(option_id).is_some() {
            return Err(ServiceError::Internal(anyhow::anyhow!(
                "Failed to reset saved configuration"
            )));
        }
        Ok(())
    }

    pub async fn cancel_chat(&self, chat_id: &str) -> ServiceResult<()> {
        self.live(chat_id).await?.cancel().await?;
        Ok(())
    }

    pub async fn stop_chat(&self, chat_id: &str) -> ServiceResult<()> {
        self.live(chat_id).await?.stop().await?;
        Ok(())
    }

    pub async fn chat_config(&self, chat_id: &str) -> ServiceResult<Value> {
        let live = self.live(chat_id).await?;
        if let Err(error) = live.ensure_running().await {
            if let Some(rejected) = error.downcast_ref::<crate::acp::SavedConfigRejected>() {
                return Err(ServiceError::SavedConfigRejected {
                    option_id: rejected.option_id.clone(),
                    message: rejected.message.clone(),
                });
            }
            if let Some(crate::workspace_env::WorkspaceEnvError::EnvrcBlocked {
                path,
                relative_path,
                message,
            }) = error.downcast_ref::<crate::workspace_env::WorkspaceEnvError>()
            {
                return Err(ServiceError::EnvrcBlocked {
                    path: path.clone(),
                    relative_path: relative_path.clone(),
                    message: message.clone(),
                });
            }
            return Err(error.into());
        }
        Ok(live.config_options().await)
    }

    pub async fn set_chat_config(
        &self,
        chat_id: &str,
        option_id: &str,
        value: Value,
    ) -> ServiceResult<Value> {
        let live = self.live(chat_id).await?;
        match live.set_config(option_id, value).await {
            Ok(v) => Ok(v),
            Err(error) => {
                if let Some(rejected) = error.downcast_ref::<crate::acp::SavedConfigRejected>() {
                    Err(ServiceError::SavedConfigRejected {
                        option_id: rejected.option_id.clone(),
                        message: rejected.message.clone(),
                    })
                } else {
                    Err(error.into())
                }
            }
        }
    }

    pub async fn remote_sessions(
        &self,
        chat_id: &str,
        cursor: Option<String>,
    ) -> ServiceResult<Value> {
        Ok(self
            .live(chat_id)
            .await?
            .list_remote_sessions(cursor)
            .await?)
    }

    pub async fn delete_remote_session(&self, chat_id: &str, remote_id: &str) -> ServiceResult<()> {
        Ok(self
            .live(chat_id)
            .await?
            .delete_remote_session(remote_id)
            .await?)
    }

    pub async fn chat_commands(&self, chat_id: &str) -> ServiceResult<Value> {
        let live = self.live(chat_id).await?;
        // Ensure the agent is running so commands are current, but do not
        // fail the query when the chat is stopped; return the last snapshot.
        let _ = live.ensure_running().await;
        Ok(live.available_commands().await)
    }

    pub async fn chat_modes(&self, chat_id: &str) -> ServiceResult<Value> {
        let live = self.live(chat_id).await?;
        let _ = live.ensure_running().await;
        Ok(live.session_modes().await)
    }

    pub async fn set_chat_mode(&self, chat_id: &str, mode_id: &str) -> ServiceResult<Value> {
        Ok(self.live(chat_id).await?.set_mode(mode_id).await?)
    }

    pub async fn chat_usage(&self, chat_id: &str) -> ServiceResult<Value> {
        let live = self.live(chat_id).await?;
        Ok(live.usage_snapshot().await)
    }

    pub async fn chat_session_info(&self, chat_id: &str) -> ServiceResult<Value> {
        let live = self.live(chat_id).await?;
        Ok(live.agent_info().await)
    }

    pub async fn list_elicitations(&self, chat_id: &str) -> ServiceResult<Value> {
        let live = self.live(chat_id).await?;
        let pending = live.pending_elicitations().await;
        Ok(serde_json::to_value(pending).unwrap_or(serde_json::json!([])))
    }

    pub async fn respond_elicitation(
        &self,
        chat_id: &str,
        id: &str,
        action: &str,
        content: Option<Value>,
    ) -> ServiceResult<bool> {
        let normalized = match action {
            "accept" | "decline" | "cancel" => action,
            _ => {
                return Err(ServiceError::Invalid(
                    "Elicitation action must be accept, decline, or cancel".into(),
                ))
            }
        };
        Ok(self
            .live(chat_id)
            .await?
            .respond_elicitation(id, normalized, content)
            .await?)
    }
}

async fn inspect_with_branches(path: PathBuf) -> ServiceResult<(RepoInfo, Vec<BranchInfo>)> {
    tokio::task::spawn_blocking(move || {
        let info = workspace::inspect(&path).map_err(workspace_error)?;
        let branches = match info.root.as_deref() {
            Some(root) => workspace::list_local_branches(root).map_err(workspace_error)?,
            None => Vec::new(),
        };
        Ok::<_, ServiceError>((info, branches))
    })
    .await
    .map_err(|error| ServiceError::Internal(anyhow::anyhow!(error)))?
}

fn local_branch_sha(branches: &[BranchInfo], branch: &str) -> ServiceResult<String> {
    branches
        .iter()
        .find(|candidate| candidate.name == branch)
        .map(|candidate| candidate.sha.clone())
        .ok_or_else(|| ServiceError::Invalid(format!("Local branch does not exist: {branch}")))
}

fn cleanup_managed(
    repository_root: &Path,
    workspace_root: &Path,
    paths: &ManagedPaths,
    chat_id: &str,
    base_commit: &str,
) -> Result<(), String> {
    workspace::remove_managed(repository_root, workspace_root, chat_id).map_err(|error| {
        format!(
            "managed worktree removal was refused ({error}); preserved branch {} and worktree {}",
            paths.branch,
            paths.worktree.display()
        )
    })?;
    workspace::rollback_provision(repository_root, &paths.branch, base_commit, true).map_err(
        |error| {
            format!(
                "managed branch rollback was refused ({error}); preserved branch {} after removing worktree {}",
                paths.branch,
                paths.worktree.display()
            )
        },
    )
}

/// Maps a chat-deletion failure to its service error kind. A `WorkspaceError`
/// carries its own safe-refusal-versus-failure distinction; anything else
/// (a locked turn guard, an unpersisted chat) is a bad request.
pub(crate) fn map_chat_deletion_error(error: anyhow::Error) -> ServiceError {
    if let Some(error) = error.downcast_ref::<workspace::WorkspaceError>() {
        return match error {
            workspace::WorkspaceError::Conflict(message) => ServiceError::Conflict(message.clone()),
            workspace::WorkspaceError::Failed(message) => ServiceError::Invalid(message.clone()),
        };
    }
    ServiceError::Invalid(error.to_string())
}

fn managed_cleanup_error(message: String, cleanup: Result<(), String>) -> ServiceError {
    match cleanup {
        Ok(()) => ServiceError::Invalid(message),
        Err(cleanup_error) => ServiceError::Internal(anyhow::anyhow!("{message}; {cleanup_error}")),
    }
}
