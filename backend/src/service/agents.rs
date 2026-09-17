//! Installed-agent operations every control surface shares.
//!
//! `AgentManager` owns the durable rows and the catalog. This layer adds what
//! only the Hub knows: whether a live session is using an agent. A destructive
//! operation therefore never reaches a running chat.
use super::{HubService, ServiceError, ServiceResult};
use crate::agents::{
    AgentEnvEdit, AgentEnvPresence, AgentError, AgentManagementDetail, AgentOperation,
    AgentOperationKind, AgentSource, AgentSummary, CustomAgentInput, InstallRequest,
    RegistryCatalogView, RemoveOutcome, ValidationReport,
};
use std::sync::Arc;

impl From<AgentError> for ServiceError {
    fn from(error: AgentError) -> Self {
        match error {
            AgentError::NotFound(message) => Self::NotFound(message),
            AgentError::Conflict(message) => Self::Conflict(message),
            AgentError::Unavailable(message) => Self::Unavailable(message),
            AgentError::Invalid(message) => Self::Invalid(message),
            AgentError::Validation(issues) => Self::Invalid(
                issues
                    .iter()
                    .map(|issue| format!("{}: {}", issue.field, issue.message))
                    .collect::<Vec<_>>()
                    .join("; "),
            ),
            AgentError::Internal(error) => Self::Internal(error),
        }
    }
}

impl HubService {
    /// Browses the ACP Registry. A refresh failure is reported inside the
    /// view; the cached catalog still comes back, so an outage never empties
    /// the browse surface.
    pub async fn registry_catalog(
        &self,
        refresh: bool,
        query: Option<&str>,
    ) -> RegistryCatalogView {
        self.agent_manager.registry_catalog(refresh, query).await
    }

    /// Fetches the registry now and keeps the last good catalog on failure.
    pub async fn refresh_registry(&self) -> RegistryCatalogView {
        self.agent_manager.registry_catalog(true, None).await
    }

    /// Starts an asynchronous install operation for an agent from the registry.
    /// Returns the initial `AgentOperation` immediately. Progress can be polled
    /// via `get_agent_operation`.
    pub fn install_registry_agent(
        self: &Arc<Self>,
        request: InstallRequest,
    ) -> ServiceResult<AgentOperation> {
        let registry_id = request.registry_id.clone();
        let agent_id = request
            .agent_id
            .as_deref()
            .unwrap_or(&registry_id)
            .to_string();

        self.agent_manager.ensure_id_is_free(&agent_id)?;
        crate::agents::AgentManager::validate_catalog_id(&registry_id)?;

        let op = self.agent_manager.operations().start(
            AgentOperationKind::Install,
            agent_id.clone(),
            registry_id,
        )?;

        let svc = Arc::clone(self);
        let op_id = op.id.clone();
        tokio::spawn(async move {
            match svc
                .agent_manager
                .install_with_operation(request, &op_id)
                .await
            {
                Ok(summary) => {
                    svc.agent_auth.invalidate_agent(&summary.id);
                    svc.notify_metadata_changed();
                }
                Err(err) => {
                    tracing::warn!(agent = %agent_id, error = %err, "Registry install failed");
                }
            }
        });

        Ok(op)
    }

    /// Starts an asynchronous update operation for a registry-installed agent.
    /// Returns the initial `AgentOperation` immediately.
    pub fn update_registry_agent(self: &Arc<Self>, id: &str) -> ServiceResult<AgentOperation> {
        let record = self.agent_manager.require_record(id)?;
        if record.source != AgentSource::Registry {
            return Err(ServiceError::Conflict(format!(
                "Agent '{id}' comes from the {} source, so it has no registry update.",
                record.source
            )));
        }
        let snapshot = record.registry.clone().ok_or_else(|| {
            ServiceError::Conflict(format!("Agent '{id}' has no registry snapshot to compare."))
        })?;

        let op = self.agent_manager.operations().start(
            AgentOperationKind::Update,
            id.to_string(),
            snapshot.registry_id.clone(),
        )?;

        let svc = Arc::clone(self);
        let op_id = op.id.clone();
        let agent_id = id.to_string();
        tokio::spawn(async move {
            match svc
                .agent_manager
                .update_with_operation(&agent_id, &op_id)
                .await
            {
                Ok(outcome) => {
                    if outcome.updated {
                        if svc.agent_has_live_session(&agent_id).await {
                            tracing::info!(
                                agent = %agent_id,
                                "Keeping the previous install; a live session is still using it"
                            );
                        } else {
                            svc.agent_manager
                                .remove_install_files(outcome.previous_install_dir.as_deref());
                        }
                        svc.agent_auth.invalidate_agent(&agent_id);
                        svc.sessions
                            .invalidate_stopped_sessions_for_agent(&agent_id)
                            .await;
                        svc.notify_metadata_changed();
                    }
                }
                Err(err) => {
                    tracing::warn!(agent = %agent_id, error = %err, "Registry update failed");
                }
            }
        });

        Ok(op)
    }

    /// Lists all tracked agent operations.
    pub fn list_agent_operations(&self) -> Vec<AgentOperation> {
        self.agent_manager.operations().list()
    }

    /// Gets one agent operation by its ID.
    pub fn get_agent_operation(&self, id: &str) -> ServiceResult<AgentOperation> {
        self.agent_manager
            .operations()
            .get(id)
            .ok_or_else(|| ServiceError::NotFound(format!("Agent operation '{id}' not found")))
    }

    pub async fn remove_installed_agent(&self, id: &str) -> ServiceResult<RemoveOutcome> {
        if self.agent_has_live_session(id).await {
            return Err(ServiceError::Conflict(format!(
                "Agent '{id}' is running a session. Stop the chats that use it first."
            )));
        }
        let outcome = self.agent_manager.remove(id).await?;
        if outcome.deleted {
            // Nothing durable references this agent anymore: the discovery
            // cache goes away with it.
            self.agent_auth.forget_agent(id);
        } else {
            // Retired: durable chats still name it. Keep the last known
            // methods as historical evidence, marked stale, since it will
            // never run a new process again under this id's old identity.
            self.agent_auth.invalidate_agent(id);
        }
        self.sessions
            .invalidate_stopped_sessions_for_agent(id)
            .await;
        self.notify_metadata_changed();
        Ok(outcome)
    }

    pub fn validate_custom_agent(&self, input: &CustomAgentInput) -> ValidationReport {
        self.agent_manager.validate_custom(input)
    }

    pub fn agent_management_detail(&self, id: &str) -> ServiceResult<AgentManagementDetail> {
        Ok(self.agent_manager.management_detail(id)?)
    }

    /// Names and presence of private per-agent overrides, never values.
    pub fn agent_env_presence(&self, id: &str) -> ServiceResult<Vec<AgentEnvPresence>> {
        Ok(self.agent_manager.agent_env_presence(id)?)
    }

    /// Applies `Keep`/`Replace`/`Remove` edits to private per-agent overrides.
    ///
    /// Changing an override invalidates the cached authentication state and
    /// the stopped sessions for that agent, so the next launch observes it.
    /// A live session keeps the environment it started with; only the next
    /// launch changes. Removing an override removes it from future launches.
    pub async fn update_agent_env(
        &self,
        id: &str,
        edits: Vec<AgentEnvEdit>,
    ) -> ServiceResult<Vec<AgentEnvPresence>> {
        let presence = self.agent_manager.apply_agent_env_edits(id, edits).await?;
        self.agent_auth.invalidate_agent(id);
        self.sessions
            .invalidate_stopped_sessions_for_agent(id)
            .await;
        self.notify_metadata_changed();
        Ok(presence)
    }

    pub async fn create_custom_agent(
        &self,
        input: CustomAgentInput,
    ) -> ServiceResult<AgentSummary> {
        let summary = self.agent_manager.create_custom(input).await?;
        self.notify_metadata_changed();
        Ok(summary)
    }

    /// Edits one Batey-managed definition. A live session keeps the runtime
    /// handle it started with; the edit reaches the next session.
    pub async fn edit_custom_agent(
        &self,
        id: &str,
        input: CustomAgentInput,
    ) -> ServiceResult<AgentSummary> {
        let summary = self.agent_manager.edit_custom(id, input).await?;
        // The edited definition can change initialization or authentication
        // methods, so the cached discovery data can no longer be trusted as
        // current.
        self.agent_auth.invalidate_agent(id);
        self.sessions
            .invalidate_stopped_sessions_for_agent(id)
            .await;
        self.notify_metadata_changed();
        Ok(summary)
    }

    /// Whether any live session is using this agent.
    pub(crate) async fn agent_has_live_session(&self, agent_id: &str) -> bool {
        self.sessions
            .agent_ids_in_use()
            .await
            .iter()
            .any(|id| id == agent_id)
    }
}
