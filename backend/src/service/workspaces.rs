//! Workspace discovery shared by the HTTP and other Hub surfaces.

use super::{HubService, ServiceError, ServiceResult};
use crate::workspace::{self, WorkspaceError};
use serde::Serialize;
use std::path::PathBuf;

/// The Git choices available when creating a chat.
///
/// Server-side repository and worktree paths intentionally do not appear in
/// this transport shape. They are durable implementation metadata, not UI
/// display data.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceOptions {
    pub is_git: bool,
    pub current_branch: Option<String>,
    pub head_sha: Option<String>,
    pub dirty: bool,
    pub branches: Vec<WorkspaceBranch>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceBranch {
    pub name: String,
    pub sha: String,
    pub current: bool,
}

/// Result of fetching a project's Git remotes and fast-forwarding its
/// checked-out branch when that was safe.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceSyncResult {
    pub branch: String,
    pub remote: String,
    pub updated: bool,
    pub head_sha: String,
}

pub(crate) fn workspace_error(error: WorkspaceError) -> ServiceError {
    match error {
        WorkspaceError::Conflict(message) => ServiceError::Conflict(message),
        WorkspaceError::Failed(message) => ServiceError::Invalid(message),
    }
}

impl HubService {
    /// Inspect the project's effective Git repository and list local branches.
    /// All Git commands run on the blocking pool because the workspace engine
    /// deliberately uses synchronous Git plumbing.
    pub async fn workspace_options(&self, project_id: &str) -> ServiceResult<WorkspaceOptions> {
        let project = self.store.project(project_id)?;
        let path = PathBuf::from(project.path);
        let result = tokio::task::spawn_blocking(move || {
            let info = workspace::inspect(&path)?;
            let branches = if info.is_git {
                let root = info
                    .root
                    .as_deref()
                    .ok_or_else(|| WorkspaceError::Failed("Git root is unavailable".into()))?;
                workspace::list_local_branches(root)?
            } else {
                Vec::new()
            };
            Ok::<_, WorkspaceError>((info, branches))
        })
        .await
        .map_err(|error| ServiceError::Internal(anyhow::anyhow!(error)))?
        .map_err(workspace_error)?;

        let (info, branches) = result;
        let branches = branches
            .into_iter()
            .map(|branch| WorkspaceBranch {
                name: branch.name,
                sha: branch.sha,
                current: branch.current,
            })
            .collect();
        Ok(WorkspaceOptions {
            is_git: info.is_git,
            current_branch: info.branch,
            head_sha: info.head_sha,
            dirty: info.dirty,
            branches,
        })
    }

    /// Fetch the project's configured remotes and, when safe, fast-forward
    /// the checked-out branch to its upstream.
    ///
    /// Scoped to the project checkout's current branch: it never touches a
    /// Batey-managed worktree, and it refuses instead of stashing, merging,
    /// rebasing, resetting, or switching branches. See
    /// `workspace::fetch_and_fast_forward` for the exact refusal conditions.
    pub async fn sync_workspace(&self, project_id: &str) -> ServiceResult<WorkspaceSyncResult> {
        tracing::info!(project_id, "Starting Git workspace sync");
        let result = self.sync_workspace_inner(project_id).await;
        match &result {
            Ok(outcome) => tracing::info!(
                project_id,
                branch = %outcome.branch,
                updated = outcome.updated,
                head_sha = %outcome.head_sha,
                "Git workspace sync finished"
            ),
            Err(error) => tracing::warn!(project_id, %error, "Git workspace sync failed"),
        }
        result
    }

    async fn sync_workspace_inner(&self, project_id: &str) -> ServiceResult<WorkspaceSyncResult> {
        let project = self.store.project(project_id)?;
        let path = PathBuf::from(project.path);
        let _workspace_guard = self.workspace_lock.lock().await;
        let info = tokio::task::spawn_blocking({
            let path = path.clone();
            move || workspace::inspect(&path)
        })
        .await
        .map_err(|error| ServiceError::Internal(anyhow::anyhow!(error)))?
        .map_err(workspace_error)?;
        if !info.is_git {
            return Err(ServiceError::Invalid(
                "Project is not a Git repository".into(),
            ));
        }
        let root = info
            .root
            .ok_or_else(|| ServiceError::Internal(anyhow::anyhow!("Git root is unavailable")))?;

        // The same reservation a project-checkout branch switch takes: a live
        // process in a direct chat bound to this checkout must finish first.
        self.ensure_primary_checkout_available(&root).await?;

        // `ensure_primary_checkout_available` is only a snapshot: nothing
        // stops a new turn from starting immediately after it returns. Hold
        // the same checkout mutex a turn's admission and a branch switch
        // take, for as long as the fetch and fast-forward run, so a turn
        // cannot start and touch the checkout while Git is mutating it.
        let _checkout_guard = self
            .sessions
            .try_acquire_checkout_guard(&root)
            .map_err(|error| ServiceError::Conflict(error.to_string()))?;

        let outcome = tokio::task::spawn_blocking({
            let root = root.clone();
            move || workspace::fetch_and_fast_forward(&root)
        })
        .await
        .map_err(|error| ServiceError::Internal(anyhow::anyhow!(error)))?
        .map_err(workspace_error)?;

        Ok(WorkspaceSyncResult {
            branch: outcome.branch,
            remote: outcome.remote,
            updated: outcome.updated,
            head_sha: outcome.head_sha,
        })
    }
}
