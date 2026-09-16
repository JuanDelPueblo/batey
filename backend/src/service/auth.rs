//! Agent-level authentication operations every control surface shares.
//!
//! `AgentAuthService` owns the ACP and PTY work. This layer adds the Hub
//! rules: which agent ids exist, and how a failure becomes a transport-neutral
//! error. An HTTP handler calls these methods and maps the error; it never
//! decides which authentication method to run.
use super::{HubService, ServiceError, ServiceResult};
use crate::auth::{
    AgentAuthError, AgentAuthRefreshView, AgentAuthView, ProtocolAuthFlowView,
    ProtocolAuthInteractionView, ProtocolElicitationView, TerminalAuthFlow, TerminalAuthFlowView,
};
use std::sync::Arc;

impl From<AgentAuthError> for ServiceError {
    fn from(error: AgentAuthError) -> Self {
        match error {
            AgentAuthError::NotFound(message) => Self::NotFound(message),
            AgentAuthError::Invalid(message) => Self::Invalid(message),
            AgentAuthError::Conflict(message) => Self::Conflict(message),
            AgentAuthError::Unavailable(message) => Self::Unavailable(message),
            AgentAuthError::Internal(error) => Self::Internal(error),
        }
    }
}

impl HubService {
    /// The authentication methods and capabilities one agent advertises.
    pub async fn agent_auth(&self, agent_id: &str) -> ServiceResult<AgentAuthView> {
        Ok(self.agent_auth.auth_view(agent_id).await?)
    }

    /// Explicit refresh: the only read path, besides an actual
    /// authentication or session lifecycle event, that may probe this
    /// agent's ACP process. Single-flight per agent.
    pub async fn refresh_agent_auth(&self, agent_id: &str) -> ServiceResult<AgentAuthRefreshView> {
        Ok(self.agent_auth.refresh(agent_id).await?)
    }

    /// Runs one advertised `agent` authentication method.
    pub async fn authenticate_agent(
        &self,
        agent_id: &str,
        method_id: &str,
    ) -> ServiceResult<AgentAuthView> {
        Ok(self.agent_auth.authenticate(agent_id, method_id).await?)
    }

    /// Runs the capability-gated stable logout method.
    ///
    /// Batey chats, sessions, and history stay exactly as they are.
    pub async fn logout_agent(&self, agent_id: &str) -> ServiceResult<AgentAuthView> {
        Ok(self.agent_auth.logout(agent_id).await?)
    }

    /// Starts a terminal authentication flow for one advertised `terminal`
    /// method.
    pub async fn start_terminal_auth(
        &self,
        agent_id: &str,
        method_id: &str,
    ) -> ServiceResult<TerminalAuthFlowView> {
        Ok(self.agent_auth.start_terminal(agent_id, method_id).await?)
    }

    pub fn terminal_auth_flow(&self, flow_id: &str) -> ServiceResult<TerminalAuthFlowView> {
        Ok(self.agent_auth.terminal_flow_view(flow_id)?)
    }

    pub fn cancel_terminal_auth(&self, flow_id: &str) -> ServiceResult<TerminalAuthFlowView> {
        Ok(self.agent_auth.cancel_terminal_flow(flow_id)?)
    }

    /// The live flow one socket attaches to.
    pub fn terminal_auth_socket(&self, flow_id: &str) -> ServiceResult<Arc<TerminalAuthFlow>> {
        Ok(self.agent_auth.terminal_flow(flow_id)?)
    }

    /// Starts an asynchronous protocol flow for one advertised `agent`
    /// method. The browser polls the flow instead of blocking on one long
    /// `authenticate` RPC.
    pub async fn start_protocol_auth(
        &self,
        agent_id: &str,
        method_id: &str,
    ) -> ServiceResult<ProtocolAuthFlowView> {
        Ok(self.agent_auth.start_protocol(agent_id, method_id).await?)
    }

    pub async fn protocol_auth_flow(&self, flow_id: &str) -> ServiceResult<ProtocolAuthFlowView> {
        Ok(self.agent_auth.protocol_flow_view(flow_id).await?)
    }

    pub async fn cancel_protocol_auth(&self, flow_id: &str) -> ServiceResult<ProtocolAuthFlowView> {
        Ok(self.agent_auth.cancel_protocol_flow(flow_id).await?)
    }

    pub async fn protocol_auth_elicitations(
        &self,
        flow_id: &str,
    ) -> ServiceResult<Vec<ProtocolElicitationView>> {
        Ok(self.agent_auth.protocol_elicitations(flow_id).await?)
    }

    pub fn protocol_auth_interaction(
        &self,
        flow_id: &str,
    ) -> ServiceResult<Option<ProtocolAuthInteractionView>> {
        Ok(self.agent_auth.protocol_interaction(flow_id)?)
    }

    pub async fn relay_protocol_auth_callback(
        &self,
        flow_id: &str,
        callback_url: &str,
    ) -> ServiceResult<()> {
        Ok(self
            .agent_auth
            .relay_protocol_callback(flow_id, callback_url)
            .await?)
    }

    pub async fn respond_protocol_auth_elicitation(
        &self,
        flow_id: &str,
        elicitation_id: &str,
        action: &str,
        content: Option<serde_json::Value>,
    ) -> ServiceResult<bool> {
        Ok(self
            .agent_auth
            .respond_protocol_elicitation(flow_id, elicitation_id, action, content)
            .await?)
    }

    /// Records stable `auth_required` evidence seen by another surface.
    pub fn note_agent_auth_required(&self, agent_id: &str) {
        self.agent_auth.note_auth_required(agent_id);
    }

    /// Reinforces `authenticated` after a session setup succeeded.
    pub fn note_agent_session_success(&self, agent_id: &str) {
        self.agent_auth.note_session_success(agent_id);
    }
}
