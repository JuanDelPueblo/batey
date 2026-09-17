//! HTTP adapters for the installed-agent catalog.
//!
//! Every handler parses the request, calls `HubService`, and maps
//! `ServiceError` to a status code. Ownership rules, validation, and registry
//! behavior live in the service and the agent manager, not here.
use super::hub::{hub, Result};
use super::AppState;
use crate::agents::{
    AgentEnvEdit, AgentEnvPresence, AgentManagementDetail, AgentOperation, AgentSummary,
    CustomAgentInput, InstallRequest, RegistryCatalogView, RemoveOutcome, ValidationReport,
};
use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;

pub async fn agents(State(s): State<AppState>) -> Result<Json<Vec<AgentSummary>>> {
    Ok(Json(hub(&s)?.list_agents()))
}

#[derive(Deserialize)]
pub struct RegistryQuery {
    /// Free-text filter over the id, the name, and the description.
    pub q: Option<String>,
    /// Whether to fetch before answering. The cache answers either way.
    pub refresh: Option<bool>,
}

pub async fn registry(
    State(s): State<AppState>,
    Query(query): Query<RegistryQuery>,
) -> Result<Json<RegistryCatalogView>> {
    let refresh = query.refresh.unwrap_or(false);
    let filter = query.q.as_deref().filter(|q| !q.trim().is_empty());
    Ok(Json(hub(&s)?.registry_catalog(refresh, filter).await))
}

pub async fn refresh_registry(State(s): State<AppState>) -> Result<Json<RegistryCatalogView>> {
    Ok(Json(hub(&s)?.refresh_registry().await))
}

pub async fn install_registry_agent(
    State(s): State<AppState>,
    Json(request): Json<InstallRequest>,
) -> Result<Json<AgentOperation>> {
    let h = hub(&s)?.clone();
    Ok(Json(h.install_registry_agent(request)?))
}

pub async fn update_agent(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AgentOperation>> {
    let h = hub(&s)?.clone();
    Ok(Json(h.update_registry_agent(&id)?))
}

pub async fn list_agent_operations(State(s): State<AppState>) -> Result<Json<Vec<AgentOperation>>> {
    Ok(Json(hub(&s)?.list_agent_operations()))
}

pub async fn get_agent_operation(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AgentOperation>> {
    Ok(Json(hub(&s)?.get_agent_operation(&id)?))
}

pub async fn agent_detail(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AgentManagementDetail>> {
    Ok(Json(hub(&s)?.agent_management_detail(&id)?))
}

pub async fn agent_env(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<AgentEnvPresence>>> {
    Ok(Json(hub(&s)?.agent_env_presence(&id)?))
}

pub async fn update_agent_env(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(edits): Json<Vec<AgentEnvEdit>>,
) -> Result<Json<Vec<AgentEnvPresence>>> {
    Ok(Json(hub(&s)?.update_agent_env(&id, edits).await?))
}

pub async fn remove_agent(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<RemoveOutcome>> {
    Ok(Json(hub(&s)?.remove_installed_agent(&id).await?))
}

pub async fn create_agent(
    State(s): State<AppState>,
    Json(input): Json<CustomAgentInput>,
) -> Result<Json<AgentSummary>> {
    Ok(Json(hub(&s)?.create_custom_agent(input).await?))
}

pub async fn validate_agent(
    State(s): State<AppState>,
    Json(input): Json<CustomAgentInput>,
) -> Result<Json<ValidationReport>> {
    Ok(Json(hub(&s)?.validate_custom_agent(&input)))
}

pub async fn edit_agent(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<CustomAgentInput>,
) -> Result<Json<AgentSummary>> {
    Ok(Json(hub(&s)?.edit_custom_agent(&id, input).await?))
}
