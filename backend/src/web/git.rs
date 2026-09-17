//! Cloning a repository into a project directory.
//!
//! This stays in the web layer. It is a browser workflow with its own timeout
//! and cleanup semantics, and it shells out to git rather than coordinating the
//! store, the sessions, and the event log. Only the final step, registering the
//! finished clone as a project, goes through the Hub service.
use super::hub::{hub, ApiError, Result};
use super::AppState;
use crate::store::validate_project_path;
pub use crate::workspace::sanitize_credentials;
use axum::{extract::State, http::StatusCode, Json};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloneProjectInput {
    pub url: String,
    pub parent_path: String,
    pub name: Option<String>,
}

pub fn validate_git_url(url: &str) -> anyhow::Result<()> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Repository URL cannot be empty");
    }
    if trimmed.starts_with("file://")
        || trimmed.starts_with('/')
        || trimmed.starts_with("./")
        || trimmed.starts_with("../")
        || trimmed.starts_with('~')
        || trimmed.contains("::")
    {
        anyhow::bail!("Unsafe or unsupported repository URL transport");
    }
    let lower = trimmed.to_ascii_lowercase();
    // Plain HTTP sends credentials and repository content without encryption.
    if lower.starts_with("http://") {
        anyhow::bail!("Plain HTTP repository URLs are not allowed. Use HTTPS or SSH");
    }
    let is_https_ssh = lower.starts_with("https://") || lower.starts_with("ssh://");
    let is_scp_ssh = trimmed.contains('@') && trimmed.contains(':') && !trimmed.contains("://");
    if !is_https_ssh && !is_scp_ssh {
        anyhow::bail!("Repository URL must be a valid HTTPS or SSH URL");
    }
    Ok(())
}

pub fn derive_repo_name(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let without_git = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let last_part = without_git.rsplit(['/', ':']).next()?;
    let clean = last_part.trim();
    if clean.is_empty() {
        None
    } else {
        Some(clean.to_string())
    }
}

pub fn validate_clone_destination_name(name: &str) -> anyhow::Result<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() || name.len() > 200 {
        anyhow::bail!("Clone destination name must contain 1–200 bytes");
    }
    if name.contains('/') || name.contains('\\') {
        anyhow::bail!("Clone destination name cannot contain path separators");
    }
    if trimmed == "." || trimmed == ".." {
        anyhow::bail!("Clone destination name cannot be '.' or '..'");
    }
    let p = std::path::Path::new(name);
    if p.is_absolute() {
        anyhow::bail!("Clone destination name cannot be an absolute path");
    }
    let mut components = p.components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(_)), None) => Ok(()),
        _ => anyhow::bail!("Clone destination name must be a single filesystem component"),
    }
}

pub async fn run_command_with_timeout(
    mut child: tokio::process::Child,
    timeout: std::time::Duration,
    cleanup_path: Option<&std::path::Path>,
) -> std::result::Result<std::process::Output, ApiError> {
    use tokio::io::AsyncReadExt;

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let stdout_fut = async {
        let mut out = Vec::new();
        if let Some(mut r) = stdout_pipe.take() {
            let _ = r.read_to_end(&mut out).await;
        }
        out
    };
    let stderr_fut = async {
        let mut err = Vec::new();
        if let Some(mut r) = stderr_pipe.take() {
            let _ = r.read_to_end(&mut err).await;
        }
        err
    };

    let wait_fut = async {
        let (status_res, stdout, stderr) = tokio::join!(child.wait(), stdout_fut, stderr_fut);
        let status = status_res?;
        Ok::<_, std::io::Error>(std::process::Output {
            status,
            stdout,
            stderr,
        })
    };

    match tokio::time::timeout(timeout, wait_fut).await {
        Ok(res) => res.map_err(|e| {
            if let Some(path) = cleanup_path {
                if path.exists() {
                    let _ = std::fs::remove_dir_all(path);
                }
            }
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to execute command: {e}"),
                None,
            )
        }),
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            if let Some(path) = cleanup_path {
                if path.exists() {
                    let _ = std::fs::remove_dir_all(path);
                }
            }
            Err(ApiError(
                StatusCode::GATEWAY_TIMEOUT,
                "Git clone timed out after 5 minutes".into(),
                None,
            ))
        }
    }
}

pub async fn clone_project(
    State(s): State<AppState>,
    Json(input): Json<CloneProjectInput>,
) -> Result<Json<Value>> {
    validate_git_url(&input.url)?;
    let parent_path = validate_project_path(&input.parent_path, &s.config.web.project_roots)?;
    let name = match input.name.filter(|n| !n.trim().is_empty()) {
        Some(n) => n.trim().to_string(),
        None => derive_repo_name(&input.url)
            .ok_or_else(|| anyhow::anyhow!("Could not derive project name from repository URL"))?,
    };
    validate_clone_destination_name(&name)?;

    let parent = std::path::Path::new(&parent_path).canonicalize()?;
    let dest = parent.join(&name);

    if !dest.starts_with(&parent) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Clone destination path escapes selected parent directory".into(),
            None,
        ));
    }
    let beneath_roots = s
        .config
        .web
        .project_roots
        .iter()
        .filter_map(|r| std::path::Path::new(r).canonicalize().ok())
        .any(|r| dest.starts_with(&r));
    if !beneath_roots {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Clone destination path is outside configured project roots".into(),
            None,
        ));
    }

    if dest.exists() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("Destination directory already exists: {}", dest.display()),
            None,
        ));
    }

    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("clone")
        .arg("--")
        .arg(&input.url)
        .arg(&dest)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true);

    let child = cmd.spawn().map_err(|e| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to execute git: {e}"),
            None,
        )
    })?;

    let output =
        run_command_with_timeout(child, std::time::Duration::from_secs(300), Some(&dest)).await?;

    if !output.status.success() {
        if dest.exists() {
            let _ = std::fs::remove_dir_all(&dest);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let sanitized = sanitize_credentials(&stderr);
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("Git clone failed: {}", sanitized.trim()),
            None,
        ));
    }

    // The clone is on disk and already canonicalized, so registering it goes
    // through the Hub service and raises the same metadata event as any other
    // project creation.
    let p = hub(&s)?.register_cloned_project(name, dest.canonicalize()?.display().to_string())?;
    Ok(Json(json!(p)))
}
