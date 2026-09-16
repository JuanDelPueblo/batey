use super::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use serde::{Deserialize, Serialize};
use std::path::Path as FsPath;

#[derive(Debug, Serialize)]
pub struct AgentStatus {
    pub name: String,
    pub process_state: String,
    pub turn_state: String,
}

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub agents: Vec<AgentStatus>,
    pub project_root: String,
}

pub async fn api_get_status(
    State(state): State<AppState>,
) -> Result<Json<StatusResponse>, StatusCode> {
    let statuses = state.session_manager.get_all_status().await;

    let agents = statuses
        .into_iter()
        .map(|(name, ps, ts)| AgentStatus {
            name,
            process_state: ps.to_string(),
            turn_state: ts.to_string(),
        })
        .collect();

    Ok(Json(StatusResponse {
        agents,
        project_root: state.config.web.project_root.clone(),
    }))
}

#[derive(Debug, Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub agent: String,
    pub cwd: String,
    pub process_state: String,
    pub turn_state: String,
}

#[derive(Debug, Serialize)]
pub struct SessionListResponse {
    pub sessions: Vec<SessionInfo>,
}

pub async fn api_list_sessions(
    State(state): State<AppState>,
) -> Result<Json<SessionListResponse>, StatusCode> {
    let project_root = FsPath::new(&state.config.web.project_root);
    for agent in state.config.agents.ids() {
        state
            .session_manager
            .get_or_create(&agent, project_root)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }

    let sessions = state.session_manager.list_sessions().await;
    Ok(Json(SessionListResponse { sessions }))
}

#[derive(Debug, Deserialize)]
pub struct PromptPayload {
    pub text: String,
    #[serde(default)]
    pub timeout: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct PromptApiResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub async fn api_prompt_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(payload): Json<PromptPayload>,
) -> Result<Json<PromptApiResponse>, StatusCode> {
    if payload.text.trim().is_empty() {
        return Ok(Json(PromptApiResponse {
            success: false,
            response: None,
            error: Some("prompt text cannot be empty".to_string()),
        }));
    }

    if matches!(payload.timeout, Some(0)) {
        return Ok(Json(PromptApiResponse {
            success: false,
            response: None,
            error: Some("timeout must be greater than zero".to_string()),
        }));
    }

    let session = state
        .session_manager
        .get_by_id(&session_id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;

    let timeout = payload
        .timeout
        .or(state.config.timeouts.prompt)
        .map(std::time::Duration::from_secs);

    match session.ask(payload.text, timeout).await {
        Ok(response) => Ok(Json(PromptApiResponse {
            success: true,
            response: Some(response),
            error: None,
        })),
        Err(e) => Ok(Json(PromptApiResponse {
            success: false,
            response: None,
            error: Some(e.to_string()),
        })),
    }
}

#[derive(Debug, Deserialize)]
pub struct PermissionPayload {
    pub id: String,
    pub option_id: String,
}

#[derive(Debug, Serialize)]
pub struct PermissionApiResponse {
    pub success: bool,
}

pub async fn api_permission_response(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(payload): Json<PermissionPayload>,
) -> Result<Json<PermissionApiResponse>, StatusCode> {
    let session = state
        .session_manager
        .get_by_id(&session_id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;

    let success = session
        .respond_to_permission(&payload.id, &payload.option_id)
        .await;
    if !success {
        return Err(StatusCode::CONFLICT);
    }
    Ok(Json(PermissionApiResponse { success }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{AgentCatalog, AgentDefinition};
    use crate::config::Config;
    use crate::events::EventLog;
    use crate::session::SessionManager;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_api_list_sessions_creates_default_project_sessions() {
        let temp = tempfile::tempdir().unwrap();

        let mut config = Config {
            agents: Arc::new(AgentCatalog::new([
                AgentDefinition::codex_default(),
                AgentDefinition::gemini_default(),
            ])),
            ..Default::default()
        };
        config.web.project_root = temp.path().to_string_lossy().to_string();

        let state = AppState::new(
            SessionManager::new(config.agents.clone(), Arc::new(EventLog::new(100))),
            Arc::new(config),
            0,
        );

        let Json(response) = api_list_sessions(State(state)).await.unwrap();

        assert_eq!(response.sessions.len(), 2);
        assert!(response
            .sessions
            .iter()
            .all(|session| session.cwd == temp.path().to_string_lossy()));
    }
}
