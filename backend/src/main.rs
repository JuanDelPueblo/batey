use batey::{
    agents::{parse_agents, parse_declarative_agents, AgentManager, HostRuntimeProbe},
    auth::AgentAuthService,
    config::{BateyPaths, Config, PathOverrides},
    events::EventLog,
    session::SessionManager,
    store::Store,
    web::WebServer,
};
use clap::Parser;
use std::{path::PathBuf, sync::Arc};

/// The auth flow temporarily points `BROWSER` at this executable. Browser
/// launchers append the URL as an argument; this tiny process forwards it to
/// the flow's loopback listener and opens it with the platform browser API.
/// It is inert unless both private capture environment variables are present.
fn run_browser_capture_helper() -> bool {
    let Ok(address) = std::env::var("BATEY_AUTH_BROWSER_CAPTURE_ADDR") else {
        return false;
    };
    let Ok(token) = std::env::var("BATEY_AUTH_BROWSER_CAPTURE_TOKEN") else {
        return false;
    };
    let Some(url) = std::env::args().skip(1).last() else {
        return true;
    };
    let Ok(address) = address.parse() else {
        return true;
    };
    let Ok(mut stream) =
        std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_secs(2))
    else {
        return true;
    };
    use std::io::Write;
    let _ = write!(stream, "{token}\n{url}");
    // Preserve ordinary local authentication when a desktop opener exists.
    // The capture is already delivered to Batey, and opener diagnostics are
    // silenced so they cannot become authentication stderr.
    #[cfg(target_os = "windows")]
    open_url_with_windows(&url);
    #[cfg(target_os = "macos")]
    open_url_with_command("open", &url);
    #[cfg(all(unix, not(target_os = "macos")))]
    open_url_with_command("xdg-open", &url);
    true
}

#[cfg(target_os = "windows")]
fn open_url_with_windows(url: &str) {
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

    let wide_url: Vec<u16> = std::ffi::OsStr::new(url)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // ShellExecuteW delegates directly to the user's registered browser and
    // does not interpret the URL as command-line or shell syntax.
    unsafe {
        let _ = ShellExecuteW(
            ptr::null_mut(),
            ptr::null(),
            wide_url.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

#[cfg(any(target_os = "macos", all(unix, not(target_os = "macos"))))]
fn open_url_with_command(program: &str, url: &str) {
    let _ = std::process::Command::new(program)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[derive(Parser)]
#[command(about = "Persistent single-owner ACP project/chat supervisor", version)]
struct Args {
    #[arg(long, env = "BATEY_DATABASE")]
    database: Option<PathBuf>,
    #[arg(long, env = "BATEY_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[arg(long, env = "BATEY_CONFIG_DIR")]
    config_dir: Option<PathBuf>,
    #[arg(long, env = "BATEY_STATE_DIR")]
    state_dir: Option<PathBuf>,
    #[arg(long, env = "BATEY_LOG_DIR")]
    log_dir: Option<PathBuf>,
    #[arg(long, env = "BATEY_WORKTREES_DIR")]
    worktrees_dir: Option<PathBuf>,
    #[arg(long, env = "BATEY_AGENTS_FILE")]
    agents_file: Option<PathBuf>,
    /// Declarative deployment agents in the same shape as `--agents-file`
    /// plus `pass_env`, `default_permission_policy`, and `description`.
    /// Entries join the catalog with `AgentSource::Declarative`, stay
    /// read-only through the management APIs, and collide explicitly with any
    /// other source. The NixOS module generates this file; values are
    /// non-secret because the path lands in the Nix store.
    #[arg(long, env = "BATEY_DECLARATIVE_AGENTS_FILE")]
    declarative_agents_file: Option<PathBuf>,
    /// Environment variable names treated as secrets. Their values are moved
    /// out of the process environment at startup into a stash, so the
    /// workspace environment every agent inherits never carries them. Each
    /// value is then injected only into the agents whose `pass_env` names it.
    /// The NixOS module derives this list from declarative `passEnv` names.
    #[arg(long, env = "BATEY_SECRET_ENV_VARS", value_delimiter = ',')]
    secret_env_vars: Vec<String>,
    /// The ACP Registry document to read. The default is the official one.
    #[arg(long, env = "BATEY_REGISTRY_URL")]
    registry_url: Option<String>,
    #[arg(long, default_value_t = 8765, env = "BATEY_PORT")]
    port: u16,
    #[arg(long, default_value = "127.0.0.1", env = "BATEY_HOST")]
    host: String,
    #[arg(long, env = "BATEY_PUBLIC_ORIGIN")]
    public_origin: Option<String>,
    /// Optional inactivity watchdog for prompts. Unset means no silence timeout.
    #[arg(long, env = "BATEY_PROMPT_TIMEOUT")]
    prompt_timeout: Option<u64>,
    #[arg(
        long,
        required = true,
        env = "BATEY_PROJECT_ROOTS",
        value_delimiter = ','
    )]
    project_root: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if run_browser_capture_helper() {
        return Ok(());
    }
    let args = Args::parse();
    // Take secrets out of the process environment before anything else runs,
    // so no inherited workspace environment and no spawned child can observe
    // them. Sessions inject each value only into agents naming it in
    // `pass_env`.
    let secrets = batey::workspace_env::take_secret_env(&args.secret_env_vars);
    let secret_count = secrets.len();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    tracing::info!(count = secret_count, "stashed secret environment variables");
    let paths = BateyPaths::from_overrides(PathOverrides {
        database: args.database,
        data_dir: args.data_dir,
        config_dir: args.config_dir,
        state_dir: args.state_dir,
        log_dir: args.log_dir,
        managed_worktrees: args.worktrees_dir,
    });
    let store = Arc::new(Store::open_with_paths(&paths)?);
    let events = Arc::new(EventLog::persistent(store.clone())?);
    let mut config = Config {
        paths,
        ..Config::default()
    };
    config.server.port = args.port;
    config.server.host = args.host;
    config.web.public_origin = args.public_origin;
    config.web.project_roots = args.project_root;
    config.timeouts.prompt = args.prompt_timeout;
    if let Some(path) = args.agents_file {
        config.agents = Arc::new(parse_agents(&std::fs::read_to_string(path)?)?);
    }
    if let Some(path) = args.declarative_agents_file {
        let declarative =
            parse_declarative_agents(&std::fs::read_to_string(&path).map_err(|error| {
                anyhow::anyhow!(
                    "cannot read declarative agents file {}: {error}",
                    path.display()
                )
            })?)?;
        for definition in declarative.definitions() {
            config
                .agents
                .insert((*definition).clone())
                .map_err(|collision| {
                    anyhow::anyhow!(
                        "Declarative agent id '{}' collides: {}",
                        collision.id,
                        collision
                    )
                })?;
        }
        tracing::info!(
            count = declarative.len(),
            file = %path.display(),
            "loaded declarative agents"
        );
    }
    if let Some(url) = args.registry_url {
        config.registry.url = url;
    }

    // Durable installed agents join the same catalog the sessions read. An id
    // that a declarative source already defines fails here, so a collision is
    // reported instead of resolved by precedence.
    let agent_manager = AgentManager::new(
        store.clone(),
        config.agents.clone(),
        config.registry.client(config.paths.registry_cache.clone()),
        config.paths.installed_agents.clone(),
        Arc::new(HostRuntimeProbe),
    );
    let loaded = agent_manager
        .load_persisted()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    tracing::info!(count = loaded, "loaded installed agents");
    config.agent_manager = Some(agent_manager);

    let manager = SessionManager::with_store(config.agents.clone(), events, Some(store));
    manager.set_secret_env(secrets);
    // One authentication service for the whole process, so shutdown ends
    // every terminal authentication flow and kills its process tree.
    let agent_auth = AgentAuthService::new(
        config.agents.clone(),
        manager.clone(),
        Config::agent_auth_dir(&config.paths),
    );
    config.agent_auth = Some(agent_auth.clone());
    let web = WebServer::new(manager.clone(), Arc::new(config));
    #[cfg(unix)]
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let shutdown = async {
        #[cfg(unix)]
        tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
    };
    let result = tokio::select! { r = web.run() => r, _ = shutdown => Ok(()) };
    agent_auth.shutdown();
    manager.shutdown_all().await;
    result
}
