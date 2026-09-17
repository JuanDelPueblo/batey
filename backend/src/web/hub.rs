use super::AppState;
use crate::service::{
    ChatEdit, ChatView, HubService, ServiceError, WorkspaceOptions, WorkspaceSelection,
    WorkspaceSyncResult, DEFAULT_HISTORY_PAGE_SIZE, MAX_HISTORY_PAGE_SIZE,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct ApiError(pub StatusCode, pub String, pub Option<Value>);
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self(StatusCode::BAD_REQUEST, e.to_string(), None)
    }
}
impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, e.to_string(), None)
    }
}
impl From<crate::store::StoreError> for ApiError {
    fn from(e: crate::store::StoreError) -> Self {
        ServiceError::from(e).into()
    }
}
/// The one place a Hub failure becomes a status code.
impl From<ServiceError> for ApiError {
    fn from(e: ServiceError) -> Self {
        let status = match &e {
            ServiceError::NotFound(_) => StatusCode::NOT_FOUND,
            ServiceError::Invalid(_) => StatusCode::BAD_REQUEST,
            ServiceError::Conflict(_) => StatusCode::CONFLICT,
            ServiceError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            ServiceError::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
            ServiceError::SavedConfigRejected { .. } => StatusCode::CONFLICT,
            ServiceError::EnvrcBlocked { .. } => StatusCode::CONFLICT,
            ServiceError::AuthRequired { .. } => StatusCode::CONFLICT,
            ServiceError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let details = match &e {
            ServiceError::SavedConfigRejected { option_id, .. } => Some(json!({
                "code": "saved_config_rejected",
                "details": { "option_id": option_id }
            })),
            ServiceError::EnvrcBlocked {
                path,
                relative_path,
                message,
            } => Some(json!({
                "code": "envrc_blocked",
                "details": {
                    "path": path.to_string_lossy(),
                    "relative_path": relative_path,
                    "message": message,
                }
            })),
            // A recoverable state, not a lost chat: the client offers the
            // authentication flow for this agent and retries afterwards.
            ServiceError::AuthRequired { agent_id, .. } => Some(json!({
                "code": "auth_required",
                "details": { "agent_id": agent_id }
            })),
            _ => None,
        };
        Self(status, e.to_string(), details)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({"error":self.1});
        if let Some(extra) = self.2 {
            if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
                body.extend(extra.clone());
            }
        }
        (self.0, Json(body)).into_response()
    }
}
pub type Result<T> = std::result::Result<T, ApiError>;

/// The Hub service, or the error the legacy non-persistent mode returns.
pub(crate) fn hub(s: &AppState) -> Result<&Arc<HubService>> {
    s.hub.as_ref().ok_or(ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "Run the batey binary for persistent projects".into(),
        None,
    ))
}

pub async fn projects(State(s): State<AppState>) -> Result<Json<Value>> {
    Ok(Json(json!(hub(&s)?.list_projects()?)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectInput {
    name: String,
    path: String,
}
pub async fn create_project(
    State(s): State<AppState>,
    Json(p): Json<ProjectInput>,
) -> Result<Json<Value>> {
    Ok(Json(json!(hub(&s)?.create_project(p.name, p.path)?)))
}

#[derive(Debug, serde::Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub name: String,
    pub path: String,
}

#[derive(Debug, serde::Serialize, Deserialize)]
pub struct Breadcrumb {
    pub name: String,
    pub path: String,
}

#[derive(Debug, serde::Serialize, Deserialize)]
pub struct DirectoryListing {
    pub current: String,
    pub name: String,
    pub parent: Option<String>,
    pub roots: Vec<String>,
    pub breadcrumbs: Vec<Breadcrumb>,
    pub directories: Vec<DirectoryEntry>,
}

#[derive(Deserialize)]
pub struct DirectoryQuery {
    pub path: Option<String>,
}

pub async fn filesystem_directories(
    State(s): State<AppState>,
    Query(q): Query<DirectoryQuery>,
) -> Result<Json<DirectoryListing>> {
    let roots = &s.config.web.project_roots;
    if roots.is_empty() {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "No project roots configured".into(),
            None,
        ));
    }
    let canonical_roots: Vec<std::path::PathBuf> = roots
        .iter()
        .filter_map(|r| std::path::Path::new(r).canonicalize().ok())
        .collect();
    if canonical_roots.is_empty() {
        return Err(ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Configured project roots do not exist on disk".into(),
            None,
        ));
    }

    let target_path = match q.path.as_deref().filter(|p| !p.trim().is_empty()) {
        Some(p) => {
            let path_obj = std::path::Path::new(p);
            if !path_obj.is_absolute() {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "Directory path must be absolute".into(),
                    None,
                ));
            }
            path_obj.canonicalize().map_err(|_| {
                ApiError(
                    StatusCode::NOT_FOUND,
                    "Directory does not exist".into(),
                    None,
                )
            })?
        }
        None => canonical_roots[0].clone(),
    };

    if !target_path.is_dir() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Path is not a directory".into(),
            None,
        ));
    }

    let matching_root = canonical_roots
        .iter()
        .find(|root| target_path.starts_with(root))
        .ok_or_else(|| {
            ApiError(
                StatusCode::FORBIDDEN,
                "Directory is outside configured project roots".into(),
                None,
            )
        })?;

    let parent = target_path.parent().and_then(|p| {
        if canonical_roots.iter().any(|r| p.starts_with(r)) {
            Some(p.to_string_lossy().into_owned())
        } else {
            None
        }
    });

    let mut directories = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&target_path) {
        for entry in entries.flatten() {
            let file_name = entry.file_name().to_string_lossy().into_owned();
            if file_name.starts_with('.') {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if meta.is_dir() {
                    if let Ok(canon) = entry.path().canonicalize() {
                        if canon.is_dir() && canonical_roots.iter().any(|r| canon.starts_with(r)) {
                            directories.push(DirectoryEntry {
                                name: file_name,
                                path: canon.to_string_lossy().into_owned(),
                            });
                        }
                    }
                }
            }
        }
    }
    directories.sort_by_key(|a| a.name.to_lowercase());

    let mut breadcrumbs = Vec::new();
    let root_name = matching_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| matching_root.to_string_lossy().into_owned());
    breadcrumbs.push(Breadcrumb {
        name: root_name,
        path: matching_root.to_string_lossy().into_owned(),
    });

    if let Ok(relative) = target_path.strip_prefix(matching_root) {
        let mut cur = matching_root.clone();
        for comp in relative.components() {
            let comp_str = comp.as_os_str().to_string_lossy().into_owned();
            cur.push(&comp_str);
            breadcrumbs.push(Breadcrumb {
                name: comp_str,
                path: cur.to_string_lossy().into_owned(),
            });
        }
    }

    let dir_name = target_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| target_path.to_string_lossy().into_owned());

    Ok(Json(DirectoryListing {
        current: target_path.to_string_lossy().into_owned(),
        name: dir_name,
        parent,
        roots: roots.clone(),
        breadcrumbs,
        directories,
    }))
}

pub async fn edit_project(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<ProjectInput>,
) -> Result<Json<Value>> {
    Ok(Json(json!(
        hub(&s)?.edit_project(&id, input.name, input.path)?
    )))
}
pub async fn delete_project(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    hub(&s)?.delete_project(&id).await?;
    Ok(Json(json!({"success":true})))
}
pub async fn chats(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<ChatView>>> {
    Ok(Json(hub(&s)?.list_chats(&id).await?))
}

pub async fn workspace_options(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<WorkspaceOptions>> {
    Ok(Json(hub(&s)?.workspace_options(&id).await?))
}

pub async fn sync_workspace(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<WorkspaceSyncResult>> {
    Ok(Json(hub(&s)?.sync_workspace(&id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatInput {
    pub agent: String,
    pub title: Option<String>,
    pub workspace: Option<WorkspaceSelection>,
}
pub async fn create_chat(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(c): Json<ChatInput>,
) -> Result<Json<ChatView>> {
    Ok(Json(
        hub(&s)?
            .create_chat_with_workspace(&id, &c.agent, c.title, c.workspace)
            .await?,
    ))
}
pub async fn chat(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<ChatView>> {
    Ok(Json(hub(&s)?.get_chat(&id).await?))
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    pub before_seq: Option<u64>,
    pub through_seq: Option<u64>,
    pub limit: Option<usize>,
}

pub async fn history(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<crate::service::ChatHistoryPage>> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_HISTORY_PAGE_SIZE)
        .min(MAX_HISTORY_PAGE_SIZE);
    Ok(Json(hub(&s)?.chat_history(
        &id,
        query.before_seq,
        query.through_seq,
        limit,
    )?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatEditInput {
    title: Option<String>,
    archived: Option<bool>,
}
pub async fn edit_chat(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(edit): Json<ChatEditInput>,
) -> Result<Json<ChatView>> {
    let edit = ChatEdit {
        title: edit.title,
        archived: edit.archived,
    };
    Ok(Json(hub(&s)?.edit_chat(&id, edit).await?))
}
pub async fn delete_chat(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>> {
    hub(&s)?.delete_chat(&id).await?;
    Ok(Json(json!({"success":true})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prompt {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    content: Option<Vec<::agent_client_protocol_schema::v1::ContentBlock>>,
}
pub async fn prompt(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(p): Json<Prompt>,
) -> Result<(StatusCode, Json<Value>)> {
    match (p.text, p.content) {
        (Some(text), None) => hub(&s)?.prompt_chat(&id, text).await?,
        (None, Some(content)) => hub(&s)?.prompt_chat_content(&id, content).await?,
        _ => {
            return Err(crate::service::ServiceError::Invalid(
                "Provide exactly one of text or content".into(),
            )
            .into())
        }
    }
    Ok((StatusCode::ACCEPTED, Json(json!({"accepted":true}))))
}
pub async fn resume(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<ChatView>> {
    Ok(Json(hub(&s)?.resume_chat(&id).await?))
}
pub async fn cancel(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>> {
    hub(&s)?.cancel_chat(&id).await?;
    Ok(Json(json!({"success":true})))
}
pub async fn stop(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>> {
    hub(&s)?.stop_chat(&id).await?;
    Ok(Json(json!({"success":true})))
}
pub async fn config(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>> {
    Ok(Json(hub(&s)?.chat_config(&id).await?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigEdit {
    id: String,
    value: Value,
}
pub async fn set_config(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(c): Json<ConfigEdit>,
) -> Result<Json<Value>> {
    Ok(Json(hub(&s)?.set_chat_config(&id, &c.id, c.value).await?))
}
pub async fn clear_config(
    State(s): State<AppState>,
    Path((id, option_id)): Path<(String, String)>,
) -> Result<StatusCode> {
    hub(&s)?.clear_saved_config(&id, &option_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
pub struct Cursor {
    cursor: Option<String>,
}
pub async fn remote_sessions(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<Cursor>,
) -> Result<Json<Value>> {
    Ok(Json(hub(&s)?.remote_sessions(&id, q.cursor).await?))
}
pub async fn delete_remote_session(
    State(s): State<AppState>,
    Path((id, remote_id)): Path<(String, String)>,
) -> Result<Json<Value>> {
    hub(&s)?.delete_remote_session(&id, &remote_id).await?;
    Ok(Json(serde_json::json!({"success": true})))
}
pub async fn chat_commands(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    Ok(Json(hub(&s)?.chat_commands(&id).await?))
}
pub async fn chat_modes(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>> {
    Ok(Json(hub(&s)?.chat_modes(&id).await?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeEdit {
    mode_id: String,
}
pub async fn set_chat_mode(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(m): Json<ModeEdit>,
) -> Result<Json<Value>> {
    Ok(Json(hub(&s)?.set_chat_mode(&id, &m.mode_id).await?))
}
pub async fn chat_usage(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>> {
    Ok(Json(hub(&s)?.chat_usage(&id).await?))
}
pub async fn chat_session_info(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    Ok(Json(hub(&s)?.chat_session_info(&id).await?))
}

pub async fn mcp_servers(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<Value>> {
    Ok(Json(json!(hub(&s)?.mcp_servers(&id)?)))
}
pub async fn create_mcp_server(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<crate::store::McpServerInput>,
) -> Result<Json<Value>> {
    Ok(Json(json!(hub(&s)?.create_mcp_server(&id, input).await?)))
}
pub async fn edit_mcp_server(
    State(s): State<AppState>,
    Path((id, server_id)): Path<(String, String)>,
    Json(input): Json<crate::store::McpServerInput>,
) -> Result<Json<Value>> {
    Ok(Json(json!(
        hub(&s)?.edit_mcp_server(&id, &server_id, input).await?
    )))
}
pub async fn delete_mcp_server(
    State(s): State<AppState>,
    Path((id, server_id)): Path<(String, String)>,
) -> Result<StatusCode> {
    hub(&s)?.delete_mcp_server(&id, &server_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpOrder {
    ids: Vec<String>,
}
pub async fn reorder_mcp_servers(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<McpOrder>,
) -> Result<Json<Value>> {
    Ok(Json(json!(
        hub(&s)?.reorder_mcp_servers(&id, input.ids).await?
    )))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdditionalRoots {
    project_ids: Vec<String>,
}
pub async fn additional_roots(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    Ok(Json(json!(hub(&s)?.additional_roots(&id)?)))
}
pub async fn set_additional_roots(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<AdditionalRoots>,
) -> Result<Json<Value>> {
    Ok(Json(json!(
        hub(&s)?
            .set_additional_roots(&id, input.project_ids)
            .await?
    )))
}
pub async fn list_elicitations(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    Ok(Json(hub(&s)?.list_elicitations(&id).await?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ElicitationAnswer {
    action: String,
    #[serde(default)]
    content: Option<Value>,
}
pub async fn respond_elicitation(
    State(s): State<AppState>,
    Path((id, eid)): Path<(String, String)>,
    Json(a): Json<ElicitationAnswer>,
) -> Result<Json<Value>> {
    let accepted = hub(&s)?
        .respond_elicitation(&id, &eid, &a.action, a.content)
        .await?;
    Ok(Json(serde_json::json!({"success": accepted})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizeEnvironmentRequest {
    #[serde(default)]
    remember: bool,
}
pub async fn authorize_environment(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AuthorizeEnvironmentRequest>,
) -> Result<Json<ChatView>> {
    Ok(Json(
        hub(&s)?
            .authorize_chat_environment(&id, body.remember)
            .await?,
    ))
}

pub async fn forget_project_envrc_grant(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    hub(&s)?.forget_project_envrc_grant(&id).await?;
    Ok(Json(json!({"success": true})))
}

pub async fn list_tasks(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<crate::tasks::TerminalTaskSummary>>> {
    Ok(Json(hub(&s)?.list_chat_tasks(&id).await?))
}

pub async fn get_task(
    State(s): State<AppState>,
    Path((id, task_id)): Path<(String, String)>,
) -> Result<Json<crate::tasks::TerminalTaskDetails>> {
    Ok(Json(hub(&s)?.get_chat_task(&id, &task_id).await?))
}

pub async fn stop_task(
    State(s): State<AppState>,
    Path((id, task_id)): Path<(String, String)>,
) -> Result<Json<crate::tasks::TerminalTaskSummary>> {
    Ok(Json(hub(&s)?.stop_chat_task(&id, &task_id).await?))
}
