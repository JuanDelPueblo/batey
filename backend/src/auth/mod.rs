//! Agent-level authentication.
//!
//! An installed agent authenticates once, for every chat that uses it. This
//! module owns that operation end to end: it reads the stable `authMethods`
//! and authentication capabilities, runs `authenticate` and `logout`, and
//! runs terminal methods in a real PTY.
//!
//! Three rules shape everything here:
//!
//! - The invocation comes only from the installed runtime and from the
//!   method the agent advertised. A request body never supplies an
//!   executable, an argument, a working directory, or an environment value.
//! - Authentication never touches a chat, a session row, or an event. A
//!   running turn keeps running.
//! - Terminal input and output stay in memory. They never enter the store,
//!   a durable event, or the tracing log.
mod flow;
mod protocol;
mod pty;
mod service;

pub use flow::{
    SuccessHook, TerminalAuthFlow, TerminalAuthFlowView, TerminalAuthFlows, TerminalFlowState,
    IDLE_TIMEOUT, MAX_ACTIVE_FLOWS, MAX_ACTIVE_FLOWS_PER_AGENT, MAX_FLOW_LIFETIME,
    MAX_SCROLLBACK_BYTES,
};
pub use protocol::{
    ProtocolAuthFlow, ProtocolAuthFlowView, ProtocolAuthFlows, ProtocolFlowState,
    MAX_ACTIVE_PROTOCOL_FLOWS, MAX_ACTIVE_PROTOCOL_FLOWS_PER_AGENT, MAX_PROTOCOL_FLOW_LIFETIME,
};
pub use pty::{PtyCommand, PtyWindow, TERMINAL_AUTH_SUPPORTED};
pub use service::{
    legacy_terminal_command, legacy_terminal_command_for_agent, terminal_command,
    ActiveAuthFlowView, AgentAuthError, AgentAuthService, AgentAuthView, AuthMethodView,
    ProtocolElicitationView, PROTOCOL_TIMEOUT_REASON,
};
