mod agent_auth;
mod agents;
mod auth;
mod git;
mod handlers;
mod hub;
mod static_files;
mod websocket;

pub use agent_auth::*;
pub use agents::*;
pub use auth::*;
pub use git::*;
pub use handlers::*;
pub use hub::*;
pub use static_files::*;
pub use websocket::*;

use crate::config::Config;
use crate::service::HubService;
use crate::session::SessionManager;
use axum::{
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
    Router,
};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, Default)]
pub struct WebServerRunOptions {
    pub port_retry: u16,
    pub open_browser: bool,
}

pub struct WebServer {
    session_manager: Arc<SessionManager>,
    config: Arc<Config>,
}

impl WebServer {
    pub fn new(session_manager: Arc<SessionManager>, config: Arc<Config>) -> Self {
        Self {
            session_manager,
            config,
        }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        self.run_with_options(WebServerRunOptions::default()).await
    }

    pub async fn run_with_options(&self, options: WebServerRunOptions) -> anyhow::Result<()> {
        let session_manager = self.session_manager.clone();
        let config = self.config.clone();

        let host: IpAddr = config.server.host.parse()?;
        let base_port = config.server.port;
        let listener = bind_listener(host, base_port, options.port_retry).await?;

        let server_port = listener.local_addr()?.port();

        let ui_addr = ui_addr_for_bind(host, server_port);
        let ui_url = format!("http://{}", ui_addr);
        tracing::info!("Web server bound to {}", listener.local_addr()?);
        tracing::info!("Web server UI available at {}", ui_url);

        if options.open_browser {
            if let Err(e) = open_browser(&ui_url) {
                tracing::warn!("Failed to open browser: {}", e);
            }
        }

        let state = AppState::new(session_manager, config, server_port);

        let app = router(state);

        axum::serve(listener, app).await?;

        Ok(())
    }
}

pub fn router(state: AppState) -> Router {
    let legacy = if state.hub.is_none() {
        Router::new()
            .route("/api/sessions", get(api_list_sessions))
            .route("/api/prompt/:session_id", post(api_prompt_session))
            .route("/api/permission/:session_id", post(api_permission_response))
    } else {
        Router::new()
    };
    Router::new()
        .merge(legacy)
        .route(
            "/api/projects",
            get(hub::projects).post(hub::create_project),
        )
        .route("/api/projects/clone", post(git::clone_project))
        .route(
            "/api/filesystem/directories",
            get(hub::filesystem_directories),
        )
        .route(
            "/api/projects/:id",
            axum::routing::patch(hub::edit_project).delete(hub::delete_project),
        )
        .route(
            "/api/projects/:id/workspace-options",
            get(hub::workspace_options),
        )
        .route(
            "/api/projects/:id/chats",
            get(hub::chats).post(hub::create_chat),
        )
        .route(
            "/api/projects/:id/envrc-grant",
            axum::routing::delete(hub::forget_project_envrc_grant),
        )
        .route(
            "/api/chats/:id",
            get(hub::chat)
                .patch(hub::edit_chat)
                .delete(hub::delete_chat),
        )
        .route("/api/chats/:id/history", get(hub::history))
        .route(
            "/api/chats/:id/prompt",
            post(hub::prompt).layer(DefaultBodyLimit::max(
                crate::content::MAX_RICH_PROMPT_HTTP_BYTES,
            )),
        )
        .route("/api/chats/:id/cancel", post(hub::cancel))
        .route("/api/chats/:id/resume", post(hub::resume))
        .route(
            "/api/chats/:id/environment/authorize",
            post(hub::authorize_environment),
        )
        .route("/api/chats/:id/tasks", get(hub::list_tasks))
        .route("/api/chats/:id/tasks/:task_id", get(hub::get_task))
        .route("/api/chats/:id/tasks/:task_id/stop", post(hub::stop_task))
        .route("/api/chats/:id/stop", post(hub::stop))
        .route("/api/chats/:id/permission", post(api_permission_response))
        .route(
            "/api/chats/:id/config",
            get(hub::config).patch(hub::set_config),
        )
        .route(
            "/api/chats/:id/config/:option_id",
            axum::routing::delete(hub::clear_config),
        )
        .route("/api/chats/:id/remote-sessions", get(hub::remote_sessions))
        .route(
            "/api/chats/:id/remote-sessions/:remote_id",
            axum::routing::delete(hub::delete_remote_session),
        )
        .route("/api/chats/:id/commands", get(hub::chat_commands))
        .route(
            "/api/chats/:id/modes",
            get(hub::chat_modes).patch(hub::set_chat_mode),
        )
        .route("/api/chats/:id/usage", get(hub::chat_usage))
        .route("/api/chats/:id/session-info", get(hub::chat_session_info))
        .route(
            "/api/chats/:id/mcp-servers",
            get(hub::mcp_servers).post(hub::create_mcp_server),
        )
        .route(
            "/api/chats/:id/mcp-servers/order",
            axum::routing::put(hub::reorder_mcp_servers),
        )
        .route(
            "/api/chats/:id/mcp-servers/:server_id",
            axum::routing::patch(hub::edit_mcp_server).delete(hub::delete_mcp_server),
        )
        .route(
            "/api/chats/:id/additional-roots",
            get(hub::additional_roots).put(hub::set_additional_roots),
        )
        .route("/api/chats/:id/elicitations", get(hub::list_elicitations))
        .route(
            "/api/chats/:id/elicitations/:eid/respond",
            post(hub::respond_elicitation),
        )
        // The installed-agent catalog. The static `registry` segments come
        // before `:id`, so a browse never matches the per-agent routes.
        .route(
            "/api/agents",
            get(agents::agents).post(agents::create_agent),
        )
        .route("/api/agents/validate", post(agents::validate_agent))
        .route("/api/agents/registry", get(agents::registry))
        .route(
            "/api/agents/registry/refresh",
            post(agents::refresh_registry),
        )
        .route(
            "/api/agents/registry/install",
            post(agents::install_registry_agent),
        )
        .route(
            "/api/agents/:id",
            get(agents::agent_detail)
                .patch(agents::edit_agent)
                .delete(agents::remove_agent),
        )
        .route("/api/agents/:id/update", post(agents::update_agent))
        .route(
            "/api/agents/:id/environment",
            get(agents::agent_env).patch(agents::update_agent_env),
        )
        // Agent-level authentication. The static `terminal` and `protocol`
        // segments come before the method id, so a flow start never matches
        // the legacy synchronous `authenticate` route.
        .route("/api/agents/:id/auth", get(agent_auth::agent_auth))
        .route(
            "/api/agents/:id/auth/refresh",
            post(agent_auth::refresh_agent_auth),
        )
        .route(
            "/api/agents/:id/auth/terminal/:method_id",
            post(agent_auth::start_terminal_auth),
        )
        .route(
            "/api/agents/:id/auth/protocol/:method_id",
            post(agent_auth::start_protocol_auth),
        )
        .route(
            "/api/agents/:id/auth/:method_id",
            post(agent_auth::authenticate_agent),
        )
        .route("/api/agents/:id/logout", post(agent_auth::logout_agent))
        .route(
            "/api/agent-auth/:flow_id",
            get(agent_auth::terminal_auth_flow),
        )
        .route(
            "/api/agent-auth/:flow_id/cancel",
            post(agent_auth::cancel_terminal_auth),
        )
        .route(
            "/api/agent-auth/:flow_id/ws",
            get(agent_auth::terminal_auth_socket),
        )
        .route(
            "/api/protocol-auth/:flow_id",
            get(agent_auth::protocol_auth_flow),
        )
        .route(
            "/api/protocol-auth/:flow_id/cancel",
            post(agent_auth::cancel_protocol_auth),
        )
        .route(
            "/api/protocol-auth/:flow_id/elicitations",
            get(agent_auth::protocol_auth_elicitations),
        )
        .route(
            "/api/protocol-auth/:flow_id/interaction",
            get(agent_auth::protocol_auth_interaction),
        )
        .route(
            "/api/protocol-auth/:flow_id/interaction/callback",
            post(agent_auth::relay_protocol_auth_callback),
        )
        .route(
            "/api/protocol-auth/:flow_id/elicitations/:eid/respond",
            post(agent_auth::respond_protocol_auth_elicitation),
        )
        .route("/api/status", get(api_get_status))
        .route("/ws", get(ws_handler))
        .fallback(static_handler)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

async fn bind_listener(
    host: IpAddr,
    base_port: u16,
    port_retry: u16,
) -> anyhow::Result<tokio::net::TcpListener> {
    let bind_port_once = base_port == 0 || port_retry == 0;
    if bind_port_once {
        let addr = SocketAddr::new(host, base_port);
        return Ok(tokio::net::TcpListener::bind(addr).await?);
    }

    let mut retries_left = port_retry;
    let mut port = base_port;
    loop {
        let addr = SocketAddr::new(host, port);
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && retries_left > 0 => {
                if port == u16::MAX {
                    return Err(anyhow::anyhow!(
                        "Port {} is in use and cannot retry past u16::MAX",
                        port
                    ));
                }
                let next_port = port + 1;
                tracing::warn!("Port {} is in use, trying {}", port, next_port);
                port = next_port;
                retries_left -= 1;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn ui_addr_for_bind(bind_host: IpAddr, port: u16) -> SocketAddr {
    match bind_host {
        IpAddr::V4(v4) if v4.is_unspecified() => {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
        }
        IpAddr::V6(v6) if v6.is_unspecified() => {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port)
        }
        _ => SocketAddr::new(bind_host, port),
    }
}

fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer").arg(url).spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    Ok(())
}

#[derive(Clone)]
pub struct AppState {
    pub session_manager: Arc<SessionManager>,
    pub config: Arc<Config>,
    pub server_port: u16,
    /// Absent only in the legacy non-persistent mode, which has no store.
    pub hub: Option<Arc<HubService>>,
}

impl AppState {
    pub fn new(
        session_manager: Arc<SessionManager>,
        config: Arc<Config>,
        server_port: u16,
    ) -> Self {
        Self {
            hub: HubService::from_session_manager(session_manager.clone(), &config),
            session_manager,
            config,
            server_port,
        }
    }
}
