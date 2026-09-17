//! The Hub operations every control surface shares.
//!
//! HTTP handlers are adapters over this layer. A later MCP or federation
//! surface calls the same methods instead of reimplementing chat and session
//! behavior, so a chat created over one surface is immediately visible and
//! manageable on the other.
//!
//! This layer coordinates the store, the session manager, the event log, and
//! the agent registry. It never speaks ACP itself; that stays in `acp/`.
mod agents;
mod auth;
mod chats;
mod error;
mod projects;
mod view;
mod workspaces;

pub use chats::{
    AdditionalRootView, ChatEdit, WorkspaceSelection, DEFAULT_HISTORY_PAGE_SIZE,
    MAX_HISTORY_PAGE_SIZE,
};
pub use error::{ServiceError, ServiceResult};
pub use view::{ChatHistoryPage, ChatView, ChatWorkspaceSummary};
pub use workspaces::{WorkspaceBranch, WorkspaceOptions, WorkspaceSyncResult};

use crate::agents::{AgentCatalog, AgentManager, AgentSummary, HostRuntimeProbe};
use crate::auth::AgentAuthService;
use crate::config::Config;
use crate::events::{EventLog, EventPayload};
use crate::session::{AcpSession, SessionManager};
use crate::store::Store;
use std::sync::Arc;
use std::time::Duration;

pub struct HubService {
    store: Arc<Store>,
    sessions: Arc<SessionManager>,
    events: Arc<EventLog>,
    agents: Arc<AgentCatalog>,
    /// Installed-agent management over the same catalog the sessions use.
    agent_manager: Arc<AgentManager>,
    /// Agent-level authentication over that same catalog.
    agent_auth: Arc<AgentAuthService>,
    workspace_lock: tokio::sync::Mutex<()>,
    /// The boundary every project path is validated against.
    project_roots: Vec<String>,
    prompt_timeout: Option<Duration>,
}

impl HubService {
    pub fn new(
        store: Arc<Store>,
        sessions: Arc<SessionManager>,
        agents: Arc<AgentCatalog>,
        config: &Config,
    ) -> Arc<Self> {
        let agent_manager = AgentManager::new(
            store.clone(),
            agents.clone(),
            config.registry.client(config.paths.registry_cache.clone()),
            config.paths.installed_agents.clone(),
            Arc::new(HostRuntimeProbe),
        );
        Self::with_agent_manager(store, sessions, agents, agent_manager, config)
    }

    /// The same service with an agent manager the caller built. Startup uses
    /// it so one manager loads the durable rows and then serves requests, and
    /// tests use it to supply a fixture registry instead of the network.
    pub fn with_agent_manager(
        store: Arc<Store>,
        sessions: Arc<SessionManager>,
        agents: Arc<AgentCatalog>,
        agent_manager: Arc<AgentManager>,
        config: &Config,
    ) -> Arc<Self> {
        // Startup builds one authentication service so a shutdown can reach
        // its flows. A caller that supplies none gets a private one.
        let agent_auth = config.agent_auth.clone().unwrap_or_else(|| {
            AgentAuthService::new(
                agents.clone(),
                sessions.clone(),
                Config::agent_auth_dir(&config.paths),
            )
        });
        Arc::new(Self {
            events: sessions.event_log().clone(),
            store,
            sessions,
            agents,
            agent_manager,
            agent_auth,
            workspace_lock: tokio::sync::Mutex::new(()),
            project_roots: config.web.project_roots.clone(),
            prompt_timeout: config.timeouts.prompt.map(Duration::from_secs),
        })
    }

    /// Builds the service when the session manager has a store. A session
    /// manager without one runs the legacy non-persistent routes instead.
    pub fn from_session_manager(
        sessions: Arc<SessionManager>,
        config: &Config,
    ) -> Option<Arc<Self>> {
        let store = sessions.store.clone()?;
        match config.agent_manager.clone() {
            Some(manager) => Some(Self::with_agent_manager(
                store,
                sessions,
                config.agents.clone(),
                manager,
                config,
            )),
            None => Some(Self::new(store, sessions, config.agents.clone(), config)),
        }
    }

    pub fn agent_manager(&self) -> &Arc<AgentManager> {
        &self.agent_manager
    }

    /// Sorted provider-neutral catalog summaries.
    pub fn list_agents(&self) -> Vec<AgentSummary> {
        self.agents.summaries()
    }

    /// Tells every connected client that project or chat metadata moved. A
    /// chat created over any surface therefore appears on the others at once.
    ///
    /// Best-effort only: the project/chat SQLite rows are authoritative, so a
    /// failure to publish the invalidation event is logged but never turns a
    /// completed mutation into an error. Turn/activity events stay fail-closed
    /// in their own call sites.
    pub(crate) fn notify_metadata_changed(&self) {
        if let Err(error) = self.events.append("", "", EventPayload::MetadataChanged {}) {
            tracing::warn!(
                %error,
                "Failed to publish metadata_changed invalidation; SQLite rows remain authoritative"
            );
        }
    }

    /// The live session for a chat, with the same distinctions the HTTP layer
    /// made before: an unknown chat is not found, and a chat whose agent left
    /// the configuration is a conflict rather than a missing chat.
    pub(crate) async fn live(&self, chat_id: &str) -> ServiceResult<Arc<AcpSession>> {
        let chat = self.store.chat(chat_id)?;
        if !self.sessions.has_agent(&chat.agent) {
            return Err(ServiceError::Conflict(
                "This chat's agent is no longer configured".into(),
            ));
        }
        self.sessions
            .get_by_id(chat_id)
            .await
            .ok_or_else(|| ServiceError::NotFound("Chat not found".into()))
    }
}
