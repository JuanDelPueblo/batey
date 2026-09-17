//! Installed-agent management: registry lifecycle and Batey-managed CRUD.
//!
//! The manager owns the durable rows and the one runtime catalog together, so
//! there is never a second registry at runtime. It enforces source ownership
//! on every mutation, it installs from a snapshot rather than from the live
//! registry, and it never removes a row that durable chats still need.
//!
//! It knows nothing about sessions. `HubService` checks live sessions before
//! it calls a destructive operation here.
use super::custom::{CustomAgentInput, ValidationIssue, ValidationReport};
use super::definition::{AgentDisplay, AgentSource, AgentSummary, DEFAULT_IDLE_TIMEOUT_SECS};
use super::installed::{InstalledAgent, RegistrySnapshot, RuntimeProbe};
use super::operations::{AgentOperationStage, AgentOperations, InstallProgressTracker};
use super::registry::{
    install, manifest::RegistryRejection, DistributionKind, PlatformTarget, RegistryAgent,
    RegistryClient,
};
use super::{AgentCatalog, CatalogCollision};
use crate::acp::callbacks::CallbackPolicy;
use crate::store::Store;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// What went wrong, without naming a transport.
#[derive(Debug)]
pub enum AgentError {
    NotFound(String),
    Conflict(String),
    Invalid(String),
    Unavailable(String),
    Validation(Vec<ValidationIssue>),
    Internal(anyhow::Error),
}

pub type AgentResult<T> = Result<T, AgentError>;

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(m) | Self::Conflict(m) | Self::Invalid(m) | Self::Unavailable(m) => {
                f.write_str(m)
            }
            Self::Validation(issues) => {
                let joined = issues
                    .iter()
                    .map(|issue| format!("{}: {}", issue.field, issue.message))
                    .collect::<Vec<_>>()
                    .join("; ");
                f.write_str(&joined)
            }
            Self::Internal(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for AgentError {}

impl From<crate::store::StoreError> for AgentError {
    fn from(error: crate::store::StoreError) -> Self {
        match error {
            crate::store::StoreError::NotFound(message) => Self::NotFound(message),
            crate::store::StoreError::Validation(message) => Self::Conflict(message),
            crate::store::StoreError::Internal(error) => Self::Internal(error),
        }
    }
}

impl From<CatalogCollision> for AgentError {
    fn from(collision: CatalogCollision) -> Self {
        Self::Conflict(collision.to_string())
    }
}

/// Where the catalog a browse call answered from came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryStatus {
    /// Fetched from the registry during this call.
    Fresh,
    /// The catalog came from memory or disk after an earlier successful fetch.
    Cached,
    /// Never fetched on this machine, and the network could not answer.
    Unavailable,
}

/// One registry entry, with what Batey knows about installing it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegistryEntryView {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Every kind this agent publishes.
    pub distributions: Vec<DistributionKind>,
    /// Every platform its binary distribution covers.
    pub platforms: Vec<PlatformTarget>,
    /// The kind an install would choose on this host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_distribution: Option<DistributionKind>,
    /// Why this host cannot install the agent, when it cannot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsupported_reason: Option<String>,
    /// The catalog id this agent is installed under, when it is installed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_as: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    pub update_available: bool,
}

/// The browse answer, including whether it came from the cache.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegistryCatalogView {
    pub status: RegistryStatus,
    pub source_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The refresh failure, when a refresh was attempted and failed. The
    /// catalog below is still usable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The host this build resolved, or `null` when the registry format has
    /// no name for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_platform: Option<PlatformTarget>,
    pub host: String,
    pub rejected: Vec<RegistryRejection>,
    pub agents: Vec<RegistryEntryView>,
}

/// What a management surface sends to install a registry agent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallRequest {
    pub registry_id: String,
    /// The catalog id to install under. It defaults to the registry id, and
    /// an existing Batey id is never rewritten to match the registry.
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub distribution: Option<DistributionKind>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub usage_provider: Option<String>,
    #[serde(default)]
    pub idle_timeout: Option<u64>,
    #[serde(default)]
    pub default_permission_policy: Option<CallbackPolicy>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

/// What an update did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateOutcome {
    pub updated: bool,
    pub from_version: String,
    pub to_version: String,
    pub agent: AgentSummary,
    /// The directory the previous version occupied, when a binary install was
    /// replaced. The caller decides whether removing it is safe.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_install_dir: Option<String>,
}

/// What a removal did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoveOutcome {
    pub id: String,
    /// True when the row went away. False when durable chats still name the
    /// agent, so the entry was retired instead and those chats stay readable.
    pub deleted: bool,
    pub retained_chats: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentSummary>,
}

/// Authenticated management data for an editable Batey-managed definition.
/// Unlike `AgentSummary`, this intentionally includes launch environment
/// values; it is exposed only by the authenticated per-agent management route.
/// Private per-agent overrides stay separate and are never part of this view.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentManagementDetail {
    pub id: String,
    pub display_name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
    pub idle_timeout: u64,
    pub usage_provider: Option<String>,
    pub metadata: serde_json::Value,
    pub default_permission_policy: CallbackPolicy,
    pub description: Option<String>,
}

/// Presence of private per-agent environment overrides. Values never leave
/// the store; the API returns names and presence only.
pub type AgentEnvPresence = crate::store::AgentEnvPresence;
/// One override edit with the shared Keep/Replace/Remove pattern.
pub type AgentEnvEdit = crate::store::AgentEnvEdit;

pub struct AgentManager {
    store: Arc<Store>,
    catalog: Arc<AgentCatalog>,
    registry: Arc<RegistryClient>,
    install_root: PathBuf,
    probe: Arc<dyn RuntimeProbe>,
    /// One catalog mutation at a time, so two installs cannot race on an id.
    mutation_lock: tokio::sync::Mutex<()>,
    operations: Arc<AgentOperations>,
}

impl std::fmt::Debug for AgentManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentManager")
            .field("install_root", &self.install_root)
            .field("registry_url", &self.registry.url())
            .finish_non_exhaustive()
    }
}

impl AgentManager {
    pub fn new(
        store: Arc<Store>,
        catalog: Arc<AgentCatalog>,
        registry: Arc<RegistryClient>,
        install_root: PathBuf,
        probe: Arc<dyn RuntimeProbe>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            catalog,
            registry,
            install_root,
            probe,
            mutation_lock: tokio::sync::Mutex::new(()),
            operations: Arc::new(AgentOperations::new()),
        })
    }

    pub fn catalog(&self) -> &Arc<AgentCatalog> {
        &self.catalog
    }

    pub fn registry(&self) -> &Arc<RegistryClient> {
        &self.registry
    }

    pub fn install_root(&self) -> &std::path::Path {
        &self.install_root
    }

    pub fn operations(&self) -> &Arc<AgentOperations> {
        &self.operations
    }

    pub fn validate_install(&self, request: &InstallRequest) -> AgentResult<(String, String)> {
        let registry_id = request.registry_id.trim().to_string();
        if registry_id.is_empty() {
            return Err(AgentError::Invalid(
                "An install needs a registry id.".into(),
            ));
        }
        let agent_id = request
            .agent_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .unwrap_or(&registry_id)
            .to_string();
        validate_catalog_id(&agent_id)?;
        self.ensure_id_is_free(&agent_id)?;
        Ok((agent_id, registry_id))
    }

    pub fn validate_update(&self, id: &str) -> AgentResult<String> {
        let record = self.require_record(id)?;
        if record.source != AgentSource::Registry {
            return Err(AgentError::Conflict(format!(
                "Agent '{id}' comes from the {} source, so it has no registry update.",
                record.source
            )));
        }
        let snapshot = record.registry.ok_or_else(|| {
            AgentError::Conflict(format!("Agent '{id}' has no registry snapshot to compare."))
        })?;
        Ok(snapshot.registry_id)
    }

    // ----------------------------------------------------------- startup

    /// Puts every durable record into the catalog.
    ///
    /// A stored id that a declarative source already defines is a collision
    /// and fails, because silently letting one win would start sessions with
    /// a definition the user did not choose.
    pub fn load_persisted(&self) -> AgentResult<usize> {
        let records = self.store.installed_agents()?;
        let mut loaded = 0;
        for record in records {
            self.catalog.insert(record.to_definition(&*self.probe))?;
            loaded += 1;
        }
        Ok(loaded)
    }

    /// Re-reads availability for every stored record. Installed files and
    /// package runtimes come and go between runs.
    pub fn refresh_availability(&self) -> AgentResult<()> {
        for record in self.store.installed_agents()? {
            if self.catalog.source_of(&record.id) == Some(record.source) {
                self.catalog.replace(record.to_definition(&*self.probe))?;
            }
        }
        Ok(())
    }

    pub fn summaries(&self) -> Vec<AgentSummary> {
        self.catalog.summaries()
    }

    // ---------------------------------------------------------- registry

    /// The registry catalog. A first browse fetches when no cache exists.
    /// Later ordinary browses use the cache, and refresh always fetches.
    pub async fn registry_catalog(
        &self,
        refresh: bool,
        query: Option<&str>,
    ) -> RegistryCatalogView {
        let (cached, error) = if refresh {
            self.registry.refresh_or_cached().await
        } else if let Some(cached) = self.registry.cached() {
            (Some(cached), None)
        } else {
            self.registry.refresh_or_cached().await
        };
        let host = PlatformTarget::host();
        let installed = self.installed_registry_index();
        let rejected = error
            .as_ref()
            .and_then(|error| error.downcast_ref::<super::registry::client::EmptyCatalog>())
            .map(|error| error.rejected.clone());

        let Some(cached) = cached else {
            return RegistryCatalogView {
                status: RegistryStatus::Unavailable,
                source_url: self.registry.url().to_string(),
                registry_version: None,
                fetched_at: None,
                error: error
                    .map(|error| error.to_string())
                    .or_else(|| Some("The ACP Registry is unavailable.".into())),
                host_platform: host,
                host: PlatformTarget::host_description(),
                rejected: rejected.unwrap_or_default(),
                agents: Vec::new(),
            };
        };

        let entries = match query {
            Some(query) => cached.catalog.search(query),
            None => cached.catalog.agents.iter().collect(),
        };
        let agents = entries
            .into_iter()
            .map(|agent| self.entry_view(agent, host, &installed))
            .collect();

        RegistryCatalogView {
            status: if cached.catalog.agents.is_empty() {
                RegistryStatus::Unavailable
            } else if cached.from_cache {
                RegistryStatus::Cached
            } else {
                RegistryStatus::Fresh
            },
            source_url: cached.metadata.source_url.clone(),
            registry_version: Some(cached.catalog.version.clone()),
            fetched_at: Some(cached.metadata.fetched_at),
            error: error.map(|error| error.to_string()).or_else(|| {
                cached.catalog.agents.is_empty().then(|| {
                    super::registry::client::EmptyCatalog::from_catalog(&cached.catalog).to_string()
                })
            }),
            host_platform: host,
            host: PlatformTarget::host_description(),
            rejected: rejected.unwrap_or_else(|| cached.catalog.rejected.clone()),
            agents,
        }
    }

    /// Registry id to (catalog id, installed version) for every install.
    fn installed_registry_index(&self) -> Vec<(String, String, String)> {
        self.store
            .installed_agents()
            .unwrap_or_default()
            .into_iter()
            .filter(|record| !record.retired)
            .filter_map(|record| {
                let snapshot = record.registry?;
                Some((snapshot.registry_id, record.id, snapshot.registry_version))
            })
            .collect()
    }

    fn entry_view(
        &self,
        agent: &RegistryAgent,
        host: Option<PlatformTarget>,
        installed: &[(String, String, String)],
    ) -> RegistryEntryView {
        let plan = install::select(agent, None, host);
        let installed_entry = installed
            .iter()
            .find(|(registry_id, _, _)| registry_id == &agent.id);
        let installed_version = installed_entry.map(|(_, _, version)| version.clone());
        RegistryEntryView {
            id: agent.id.clone(),
            name: agent.name.clone(),
            version: agent.version.clone(),
            description: agent.description.clone(),
            repository: agent.repository.clone(),
            website: agent.website.clone(),
            authors: agent.authors.clone(),
            license: agent.license.clone(),
            license_url: agent.license_url.clone(),
            icon: agent.icon.clone(),
            distributions: agent.distribution.kinds(),
            platforms: agent.distribution.binary.keys().copied().collect(),
            selected_distribution: plan.as_ref().ok().map(|plan| plan.kind),
            unsupported_reason: plan.as_ref().err().map(|error| error.to_string()),
            installed_as: installed_entry.map(|(_, catalog_id, _)| catalog_id.clone()),
            update_available: installed_version
                .as_deref()
                .is_some_and(|version| version != agent.version),
            installed_version,
        }
    }

    /// The catalog needed for an install or an update. It prefers the cache,
    /// because an install must not depend on a live registry any more than a
    /// launch does, and refreshes only when nothing is cached.
    async fn resolve_registry_agent(&self, registry_id: &str) -> AgentResult<RegistryAgent> {
        let cached = match self.registry.cached() {
            Some(cached) => cached,
            None => self.registry.refresh().await.map_err(|error| {
                AgentError::Unavailable(format!("The ACP Registry is unavailable: {error}"))
            })?,
        };
        cached
            .catalog
            .agent(registry_id)
            .cloned()
            .ok_or_else(|| AgentError::NotFound(format!("'{registry_id}' is not in the registry")))
    }

    /// Installs one registry agent under a Batey catalog id.
    pub async fn install(&self, request: InstallRequest) -> AgentResult<AgentSummary> {
        self.install_with_tracker(request, None).await
    }

    /// Installs one registry agent with progress tracking.
    pub async fn install_with_tracker(
        &self,
        request: InstallRequest,
        tracker: Option<Arc<dyn InstallProgressTracker>>,
    ) -> AgentResult<AgentSummary> {
        let _guard = self.mutation_lock.lock().await;
        let registry_id = request.registry_id.trim().to_string();
        if registry_id.is_empty() {
            return Err(AgentError::Invalid(
                "An install needs a registry id.".into(),
            ));
        }
        let agent_id = request
            .agent_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .unwrap_or(&registry_id)
            .to_string();
        validate_catalog_id(&agent_id)?;
        self.ensure_id_is_free(&agent_id)?;

        if let Some(ref t) = tracker {
            t.on_stage(AgentOperationStage::Resolving);
        }

        let agent = self.resolve_registry_agent(&registry_id).await?;
        let plan = install::select(&agent, request.distribution, PlatformTarget::host())
            .map_err(|error| AgentError::Invalid(error.to_string()))?;
        let prepared = install::prepare_with_progress(
            &agent,
            plan,
            &*self.registry.http(),
            &self.install_root,
            &agent_id,
            tracker.clone(),
        )
        .await
        .map_err(|error| AgentError::Invalid(error.to_string()))?;

        if let Some(ref t) = tracker {
            t.on_stage(AgentOperationStage::Finalizing);
        }

        let mut record = InstalledAgent::new(
            agent_id.clone(),
            AgentSource::Registry,
            prepared.command.clone(),
        );
        record.display_name = request
            .display_name
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| agent.name.clone());
        record.args = prepared.args;
        record.env = prepared.env;
        record.idle_timeout_secs = request.idle_timeout.unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS);
        // A registry entry never implies a usage provider. Only an explicit
        // request attaches one.
        record.usage_provider = request
            .usage_provider
            .map(|provider| provider.trim().to_string())
            .filter(|provider| !provider.is_empty());
        record.metadata = request.metadata.unwrap_or(serde_json::Value::Null);
        record.default_permission_policy = request.default_permission_policy.unwrap_or_default();
        record.display = display_from_registry(&agent);
        record.registry = Some(RegistrySnapshot {
            registry_id: agent.id.clone(),
            registry_version: agent.version.clone(),
            distribution: prepared.distribution,
            install_dir: prepared
                .install_dir
                .as_ref()
                .map(|path| path.display().to_string()),
            installed_at: chrono::Utc::now().to_rfc3339(),
        });

        self.store.insert_agent(&record)?;
        let definition = record.to_definition(&*self.probe);
        let summary = definition.summary();
        if let Err(collision) = self.catalog.insert(definition) {
            // The durable row must not outlive a failed catalog insert.
            let _ = self.store.delete_agent(&record.id);
            return Err(collision.into());
        }
        Ok(summary)
    }

    /// Compares the installed snapshot with the registry and installs the
    /// newer version. The catalog entry changes only after the replacement is
    /// on disk, so a failed update leaves the working install in place.
    pub async fn update(&self, id: &str) -> AgentResult<UpdateOutcome> {
        self.update_with_tracker(id, None).await
    }

    /// Compares and updates with progress tracking.
    pub async fn update_with_tracker(
        &self,
        id: &str,
        tracker: Option<Arc<dyn InstallProgressTracker>>,
    ) -> AgentResult<UpdateOutcome> {
        let _guard = self.mutation_lock.lock().await;
        let record = self.require_record(id)?;
        if record.source != AgentSource::Registry {
            return Err(AgentError::Conflict(format!(
                "Agent '{id}' comes from the {} source, so it has no registry update.",
                record.source
            )));
        }
        let snapshot = record.registry.clone().ok_or_else(|| {
            AgentError::Conflict(format!("Agent '{id}' has no registry snapshot to compare."))
        })?;

        if let Some(ref t) = tracker {
            t.on_stage(AgentOperationStage::Resolving);
        }

        // An update is the one operation that should see the newest registry.
        let (cached, error) = self.registry.refresh_or_cached().await;
        let cached = cached.ok_or_else(|| {
            AgentError::Unavailable(format!(
                "The ACP Registry is unavailable: {}",
                error
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "nothing is cached".into())
            ))
        })?;
        let agent = cached
            .catalog
            .agent(&snapshot.registry_id)
            .cloned()
            .ok_or_else(|| {
                AgentError::NotFound(format!(
                    "'{}' is no longer in the registry",
                    snapshot.registry_id
                ))
            })?;

        if agent.version == snapshot.registry_version {
            return Ok(UpdateOutcome {
                updated: false,
                from_version: snapshot.registry_version.clone(),
                to_version: agent.version,
                agent: record.to_definition(&*self.probe).summary(),
                previous_install_dir: None,
            });
        }

        // Keep the distribution kind the user installed with.
        let preferred = Some(snapshot.distribution.kind());
        let plan = install::select(&agent, preferred, PlatformTarget::host())
            .or_else(|_| install::select(&agent, None, PlatformTarget::host()))
            .map_err(|error| AgentError::Invalid(error.to_string()))?;
        let prepared = install::prepare_with_progress(
            &agent,
            plan,
            &*self.registry.http(),
            &self.install_root,
            &record.id,
            tracker.clone(),
        )
        .await
        .map_err(|error| AgentError::Invalid(error.to_string()))?;

        if let Some(ref t) = tracker {
            t.on_stage(AgentOperationStage::Finalizing);
        }

        let previous_install_dir = snapshot.install_dir.clone();
        let mut updated = record.clone();
        updated.command = prepared.command;
        updated.args = prepared.args;
        updated.env = prepared.env;
        updated.display = display_from_registry(&agent);
        updated.registry = Some(RegistrySnapshot {
            registry_id: agent.id.clone(),
            registry_version: agent.version.clone(),
            distribution: prepared.distribution,
            install_dir: prepared
                .install_dir
                .as_ref()
                .map(|path| path.display().to_string()),
            installed_at: chrono::Utc::now().to_rfc3339(),
        });
        updated.touch();

        self.store.update_agent(&updated)?;
        let definition = updated.to_definition(&*self.probe);
        let summary = definition.summary();
        self.catalog.replace(definition)?;

        Ok(UpdateOutcome {
            updated: true,
            from_version: snapshot.registry_version,
            to_version: agent.version,
            agent: summary,
            previous_install_dir: previous_install_dir
                .filter(|previous| Some(previous.as_str()) != updated_install_dir(&updated)),
        })
    }

    /// Removes one installed agent.
    ///
    /// A declarative definition is read-only and is refused. An agent that
    /// durable chats still name is retired instead of deleted: the row and the
    /// catalog entry stay, marked unavailable, so those chats keep their
    /// history and no new session starts.
    pub async fn remove(&self, id: &str) -> AgentResult<RemoveOutcome> {
        let _guard = self.mutation_lock.lock().await;
        let record = self.require_record(id)?;
        match record.source {
            AgentSource::Registry | AgentSource::BateyManaged => {}
            other => {
                return Err(AgentError::Conflict(format!(
                    "Agent '{id}' comes from the {other} source and is read-only here. \
                     Change the source that defines it."
                )))
            }
        }

        let retained_chats = self.store.chat_count_for_agent(id)?;
        let install_dir = record
            .registry
            .as_ref()
            .and_then(|snapshot| snapshot.install_dir.clone());

        if retained_chats > 0 {
            let mut retired = record.clone();
            retired.retired = true;
            retired.retired_reason = Some(format!(
                "This agent was uninstalled. {retained_chats} chat(s) still refer to it, \
                 so their history stays readable."
            ));
            retired.touch();
            self.store.update_agent(&retired)?;
            let definition = retired.to_definition(&*self.probe);
            let summary = definition.summary();
            self.catalog.replace(definition)?;
            self.remove_install_files(install_dir.as_deref());
            // A retired row keeps its private overrides while the historical
            // record exists, but they are inert: a retired agent never starts
            // a new process. Deleting the row deletes the overrides.
            return Ok(RemoveOutcome {
                id: id.to_string(),
                deleted: false,
                retained_chats,
                agent: Some(summary),
            });
        }

        self.store.delete_agent(id)?;
        // No retained historical record requires the overrides anymore, so
        // they go away with the row and never linger as orphan secret rows.
        // A best-effort cleanup: the row is already gone, so a failure here
        // must not turn a successful uninstall into an error.
        if let Err(error) = self.store.delete_agent_env_for_agent(id) {
            tracing::warn!(agent = id, %error, "Could not clean up agent environment overrides");
        }
        self.catalog.remove(id);
        self.remove_install_files(install_dir.as_deref());
        Ok(RemoveOutcome {
            id: id.to_string(),
            deleted: true,
            retained_chats: 0,
            agent: None,
        })
    }

    /// Deletes a directory only when it sits under the Batey-managed install
    /// root, so a bad snapshot can never reach unrelated user data.
    pub fn remove_install_files(&self, install_dir: Option<&str>) {
        let Some(install_dir) = install_dir else {
            return;
        };
        let path = PathBuf::from(install_dir);
        let Ok(root) = self.install_root.canonicalize() else {
            return;
        };
        let Ok(path) = path.canonicalize() else {
            return;
        };
        if !path.starts_with(&root) || path == root {
            tracing::warn!(
                path = %path.display(),
                root = %root.display(),
                "Refusing to remove an install directory outside the managed install root"
            );
            return;
        }
        if let Err(error) = std::fs::remove_dir_all(&path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %path.display(), %error, "Could not remove the install directory");
            }
        }
    }

    // ----------------------------------------------------- batey-managed

    pub fn validate_custom(&self, input: &CustomAgentInput) -> ValidationReport {
        let mut report = input.report();
        let id = input.id.trim();
        if report.valid && !id.is_empty() {
            if let Some(existing) = self.catalog.source_of(id) {
                report.valid = false;
                report.issues.push(ValidationIssue {
                    field: "id".into(),
                    message: format!(
                        "Agent id '{id}' is already defined by the {existing} source."
                    ),
                });
            }
        }
        report
    }

    pub async fn create_custom(&self, input: CustomAgentInput) -> AgentResult<AgentSummary> {
        let _guard = self.mutation_lock.lock().await;
        let record = input.into_record().map_err(AgentError::Validation)?;
        self.ensure_id_is_free(&record.id)?;
        self.store.insert_agent(&record)?;
        let definition = record.to_definition(&*self.probe);
        let summary = definition.summary();
        if let Err(collision) = self.catalog.insert(definition) {
            let _ = self.store.delete_agent(&record.id);
            return Err(collision.into());
        }
        Ok(summary)
    }

    pub async fn edit_custom(
        &self,
        id: &str,
        input: CustomAgentInput,
    ) -> AgentResult<AgentSummary> {
        let _guard = self.mutation_lock.lock().await;
        let record = self.require_record(id)?;
        if record.source != AgentSource::BateyManaged {
            return Err(AgentError::Conflict(format!(
                "Agent '{id}' comes from the {} source, so it cannot be edited here.",
                record.source
            )));
        }
        let updated = input.apply_to(&record).map_err(AgentError::Validation)?;
        self.store.update_agent(&updated)?;
        let definition = updated.to_definition(&*self.probe);
        let summary = definition.summary();
        self.catalog.replace(definition)?;
        Ok(summary)
    }

    pub fn installed_record(&self, id: &str) -> AgentResult<Option<InstalledAgent>> {
        Ok(self.store.installed_agent(id)?)
    }

    /// Names and presence of private per-agent overrides, never values.
    /// It supports Registry-managed and Batey-managed agents, including
    /// retired rows (which keep their overrides inertly while history exists).
    pub fn agent_env_presence(&self, id: &str) -> AgentResult<Vec<AgentEnvPresence>> {
        self.require_installed_for_env(id)?;
        Ok(self.store.agent_env_presence(id)?)
    }

    /// Applies `Keep`/`Replace`/`Remove` edits to private per-agent overrides.
    /// Registry snapshots stay immutable and pinned: only this table changes.
    /// Values never return to the caller; the answer is presence only.
    pub async fn apply_agent_env_edits(
        &self,
        id: &str,
        edits: Vec<AgentEnvEdit>,
    ) -> AgentResult<Vec<AgentEnvPresence>> {
        let _guard = self.mutation_lock.lock().await;
        self.require_installed_for_env(id)?;
        Ok(self.store.apply_agent_env_edits(id, &edits)?)
    }

    fn require_installed_for_env(&self, id: &str) -> AgentResult<InstalledAgent> {
        match self.store.installed_agent(id)? {
            Some(record) => match record.source {
                AgentSource::Registry | AgentSource::BateyManaged => Ok(record),
                other => Err(AgentError::Conflict(format!(
                    "Agent '{id}' comes from the {other} source, so it has no Batey-owned environment."
                ))),
            },
            None => match self.catalog.source_of(id) {
                Some(source) => Err(AgentError::Conflict(format!(
                    "Agent '{id}' comes from the {source} source and is not an installed agent. \
                     Only Registry-managed and Batey-managed agents take private environment overrides."
                ))),
                None => Err(AgentError::NotFound(format!("Agent '{id}' not found"))),
            },
        }
    }

    pub fn management_detail(&self, id: &str) -> AgentResult<AgentManagementDetail> {
        let record = self.require_record(id)?;
        if record.source != AgentSource::BateyManaged {
            return Err(AgentError::Conflict(format!(
                "Agent '{id}' is not an editable Batey-managed definition."
            )));
        }
        Ok(AgentManagementDetail {
            id: record.id,
            display_name: record.display_name,
            command: record.command,
            args: record.args,
            env: record.env,
            idle_timeout: record.idle_timeout_secs,
            usage_provider: record.usage_provider,
            metadata: record.metadata,
            default_permission_policy: record.default_permission_policy,
            description: record.display.description,
        })
    }

    fn require_record(&self, id: &str) -> AgentResult<InstalledAgent> {
        match self.store.installed_agent(id)? {
            Some(record) => Ok(record),
            None => match self.catalog.source_of(id) {
                Some(source) => Err(AgentError::Conflict(format!(
                    "Agent '{id}' comes from the {source} source and is read-only here. \
                     Change the source that defines it."
                ))),
                None => Err(AgentError::NotFound(format!("Agent '{id}' not found"))),
            },
        }
    }

    fn ensure_id_is_free(&self, id: &str) -> AgentResult<()> {
        if let Some(existing) = self.catalog.source_of(id) {
            return Err(AgentError::Conflict(
                CatalogCollision {
                    id: id.to_string(),
                    existing,
                    incoming: AgentSource::BateyManaged,
                }
                .to_string(),
            ));
        }
        if self.store.installed_agent(id)?.is_some() {
            return Err(AgentError::Conflict(format!(
                "An agent with id '{id}' is already stored."
            )));
        }
        Ok(())
    }
}

fn updated_install_dir(record: &InstalledAgent) -> Option<&str> {
    record.registry.as_ref()?.install_dir.as_deref()
}

fn display_from_registry(agent: &RegistryAgent) -> AgentDisplay {
    AgentDisplay {
        description: Some(agent.description.clone()),
        version: Some(agent.version.clone()),
        icon: agent.icon.clone(),
        repository: agent.repository.clone(),
        website: agent.website.clone(),
        license: agent.license.clone(),
        license_url: agent.license_url.clone(),
        authors: agent.authors.clone(),
    }
}

/// The catalog id rules, shared with the agents file so a definition can move
/// between the two without a rename.
fn validate_catalog_id(id: &str) -> AgentResult<()> {
    if id.is_empty()
        || id.len() > super::custom::MAX_ID_LENGTH
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(AgentError::Invalid(format!(
            "'{id}' is not a usable agent id. An id holds letters, digits, '-', and '_'."
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::registry::client::testing::FixtureFetch;
    use super::super::registry::RegistryClient;
    use super::super::{
        AgentAvailability, AgentCatalog, AgentDefinition, AgentOperationKind, AgentOperationState,
        HostRuntimeProbe, InstalledDistribution,
    };
    use super::*;
    use std::io::Write;
    use std::path::Path;

    const REGISTRY_URL: &str = "https://registry.invalid/registry.json";
    const ARCHIVE_URL: &str = "https://registry.invalid/example-1.0.0.tar.gz";

    /// A probe that says every runtime and every file is present, so a test
    /// never depends on what the host has installed.
    struct AlwaysPresent;
    impl RuntimeProbe for AlwaysPresent {
        fn on_path(&self, _program: &str) -> bool {
            true
        }
        fn is_file(&self, _path: &Path) -> bool {
            true
        }
    }

    fn tar_gz(name: &str, body: &[u8]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, name, body).unwrap();
        let tar = builder.into_inner().unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    /// One registry document with a binary agent and a package agent.
    fn document(version: &str, archive_digest: Option<&str>) -> String {
        let sha = archive_digest
            .map(|digest| format!(r#""sha256":"{digest}","#))
            .unwrap_or_default();
        format!(
            r#"{{"version":"1.0.0","agents":[
              {{"id":"example-acp","name":"Example","version":"{version}",
               "description":"An example agent","repository":"https://e.invalid/repo",
               "license":"MIT","icon":"https://e.invalid/icon.svg",
               "distribution":{{"binary":{{"linux-x86_64":{{
                 "archive":"{ARCHIVE_URL}",{sha}"cmd":"./example"}}}}}}}},
              {{"id":"package-acp","name":"Package","version":"{version}",
               "description":"A package agent",
               "distribution":{{"npx":{{"package":"package-acp@{version}","args":["--acp"]}}}}}}
            ]}}"#
        )
    }

    struct Harness {
        _tmp: tempfile::TempDir,
        store: Arc<Store>,
        catalog: Arc<AgentCatalog>,
        manager: Arc<AgentManager>,
        http: Arc<FixtureFetch>,
        root: PathBuf,
    }

    fn harness_with(catalog: Arc<AgentCatalog>, http: Arc<FixtureFetch>) -> Harness {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&tmp.path().join("hub.db")).unwrap());
        let registry = Arc::new(RegistryClient::new(
            REGISTRY_URL,
            tmp.path().join("registry-cache"),
            http.clone(),
        ));
        let root = tmp.path().join("agents");
        let manager = AgentManager::new(
            store.clone(),
            catalog.clone(),
            registry,
            root.clone(),
            Arc::new(AlwaysPresent),
        );
        Harness {
            _tmp: tmp,
            store,
            catalog,
            manager,
            http,
            root,
        }
    }

    fn harness() -> Harness {
        let archive = tar_gz("example", b"#!/bin/sh\nexit 0\n");
        let digest = super::super::registry::sha256_hex(&archive);
        let http = Arc::new(
            FixtureFetch::new()
                .with(REGISTRY_URL, document("1.0.0", Some(&digest)))
                .with(ARCHIVE_URL, archive),
        );
        harness_with(Arc::new(AgentCatalog::default()), http)
    }

    fn custom(id: &str) -> CustomAgentInput {
        CustomAgentInput {
            id: id.into(),
            command: format!("/opt/{id}"),
            ..CustomAgentInput::default()
        }
    }

    // ------------------------------------------------------------ browsing

    #[tokio::test]
    async fn production_shape_returns_entries_and_explicit_rejections() {
        let harness = harness();
        harness.http.set(
            REGISTRY_URL,
            include_str!("../../../tests/fixtures/registry-v1-current.json"),
        );
        let view = harness.manager.registry_catalog(false, None).await;
        assert_eq!(view.status, RegistryStatus::Fresh);
        assert_eq!(view.agents.len(), 8);
        assert_eq!(view.rejected.len(), 2);
        assert!(view.agents.iter().any(|agent| agent.id == "codex-acp"));
        assert!(view.agents.iter().any(|agent| agent.id == "claude-acp"));
        assert!(view.fetched_at.is_some());
        harness.http.set(REGISTRY_URL, document("2.0.0", None));
        let view = harness.manager.registry_catalog(true, None).await;
        let response = serde_json::to_value(&view).unwrap();
        assert_eq!(response["rejected"], serde_json::json!([]));
        assert_eq!(response["agents"].as_array().unwrap().len(), 2);
        assert_eq!(response["status"], "fresh");
    }

    #[tokio::test]
    async fn zero_accepted_entries_report_diagnostics_and_preserve_good_entries() {
        let harness = harness();
        let bad = r#"{"version":"1.0.0","agents":[{"id":"bad"}]}"#;
        harness.http.set(REGISTRY_URL, bad);
        let view = harness.manager.registry_catalog(false, None).await;
        assert_eq!(view.status, RegistryStatus::Unavailable);
        assert!(view
            .error
            .unwrap()
            .contains("zero accepted entries (1 rejected)"));
        assert_eq!(view.rejected[0].reason, "missing name");
        assert!(harness.manager.registry.cached().is_none());

        harness.http.set(REGISTRY_URL, document("1.0.0", None));
        let first = harness.manager.registry_catalog(true, None).await;
        harness.http.set(REGISTRY_URL, bad);
        let failed = harness.manager.registry_catalog(true, None).await;
        assert_eq!(failed.status, RegistryStatus::Cached);
        assert_eq!(failed.agents, first.agents);
        assert_eq!(failed.fetched_at, first.fetched_at);
        assert_eq!(failed.rejected[0].id.as_deref(), Some("bad"));
        assert!(failed.error.unwrap().contains("zero accepted"));
    }

    #[tokio::test]
    async fn an_old_empty_cache_cannot_appear_as_a_normal_catalog() {
        let harness = harness();
        let cache = harness.manager.registry.cache_dir();
        std::fs::create_dir_all(cache).unwrap();
        std::fs::write(
            cache.join("registry.json"),
            r#"{"version":"1.0.0","agents":[{"id":"bad"}]}"#,
        )
        .unwrap();
        harness.http.set_failing(REGISTRY_URL, "offline");
        let view = harness.manager.registry_catalog(false, None).await;
        assert_eq!(view.status, RegistryStatus::Unavailable);
        assert!(view.error.unwrap().contains("zero accepted"));
        assert_eq!(view.rejected.len(), 1);
    }

    #[tokio::test]
    async fn browsing_bootstraps_and_preserves_cache_statuses() {
        let harness = harness();
        // A first ordinary browse fetches the catalog.
        let view = harness.manager.registry_catalog(false, None).await;
        assert_eq!(view.status, RegistryStatus::Fresh);
        assert_eq!(view.agents.len(), 2);
        assert_eq!(harness.http.call_count(), 1);

        // A later ordinary browse uses the cache.
        let view = harness.manager.registry_catalog(false, None).await;
        assert_eq!(view.status, RegistryStatus::Cached);
        assert_eq!(view.agents.len(), 2);
        assert_eq!(view.registry_version.as_deref(), Some("1.0.0"));
        assert!(view.error.is_none());
        assert_eq!(harness.http.call_count(), 1);

        // A failed force refresh serves the cache and reports the failure.
        harness.http.set_failing(REGISTRY_URL, "offline");
        let view = harness.manager.registry_catalog(true, None).await;
        assert_eq!(view.status, RegistryStatus::Cached);
        assert_eq!(view.agents.len(), 2);
        assert!(view.error.unwrap().contains("offline"));

        // A successful force refresh replaces the cache.
        harness.http.set(REGISTRY_URL, document("2.0.0", None));
        let view = harness.manager.registry_catalog(true, None).await;
        assert_eq!(view.status, RegistryStatus::Fresh);
        assert_eq!(view.registry_version.as_deref(), Some("1.0.0"));
        assert_eq!(view.agents[0].version, "2.0.0");
    }

    #[tokio::test]
    async fn browsing_reports_the_initial_fetch_failure_without_a_cache() {
        let http = Arc::new(FixtureFetch::new().failing(REGISTRY_URL, "DNS lookup failed"));
        let harness = harness_with(Arc::new(AgentCatalog::default()), http.clone());

        let view = harness.manager.registry_catalog(false, None).await;
        assert_eq!(view.status, RegistryStatus::Unavailable);
        assert!(view.agents.is_empty());
        assert_eq!(http.call_count(), 1);
        assert!(view.error.unwrap().contains("DNS lookup failed"));
    }

    #[tokio::test]
    async fn browsing_filters_and_reports_host_support() {
        let harness = harness();
        harness.manager.registry_catalog(true, None).await;

        let view = harness
            .manager
            .registry_catalog(false, Some("package"))
            .await;
        assert_eq!(view.agents.len(), 1);
        assert_eq!(view.agents[0].id, "package-acp");
        assert_eq!(
            view.agents[0].selected_distribution,
            Some(DistributionKind::Npx)
        );
        assert_eq!(view.agents[0].distributions, vec![DistributionKind::Npx]);
        assert!(!view.agents[0].update_available);
        assert!(view.agents[0].installed_as.is_none());

        let view = harness
            .manager
            .registry_catalog(false, Some("nothing"))
            .await;
        assert!(view.agents.is_empty());
    }

    // ---------------------------------------------------------- installing

    #[tokio::test]
    async fn a_binary_install_is_durable_and_launches_from_the_snapshot() {
        let harness = harness();
        let summary = harness
            .manager
            .install(InstallRequest {
                registry_id: "example-acp".into(),
                ..InstallRequest::default()
            })
            .await
            .unwrap();
        assert_eq!(summary.id, "example-acp");
        assert_eq!(summary.source, AgentSource::Registry);
        assert_eq!(summary.display_name, "Example");
        assert_eq!(summary.display.version.as_deref(), Some("1.0.0"));
        assert_eq!(
            summary.display.description.as_deref(),
            Some("An example agent")
        );
        // A registry entry never implies a usage provider.
        assert_eq!(summary.usage_provider, None);

        let record = harness
            .store
            .installed_agent("example-acp")
            .unwrap()
            .unwrap();
        let snapshot = record.registry.unwrap();
        assert_eq!(snapshot.registry_id, "example-acp");
        assert_eq!(snapshot.registry_version, "1.0.0");
        match snapshot.distribution {
            InstalledDistribution::Binary {
                integrity_verified, ..
            } => assert!(integrity_verified),
            other => panic!("unexpected distribution {other:?}"),
        }
        assert_eq!(
            record.command,
            harness
                .root
                .join("example-acp/1.0.0/example")
                .display()
                .to_string()
        );
        assert!(Path::new(&record.command).is_file());

        // The catalog can launch it without the registry.
        let runtime = harness.catalog.runtime("example-acp").unwrap();
        assert_eq!(runtime.launch.command, record.command);
    }

    /// The core durability rule: a restart rebuilds the catalog from the rows,
    /// and the agent launches without any registry call.
    #[tokio::test]
    async fn installed_agents_survive_a_restart_without_the_registry() {
        let harness = harness();
        harness
            .manager
            .install(InstallRequest {
                registry_id: "package-acp".into(),
                agent_id: Some("my-package".into()),
                ..InstallRequest::default()
            })
            .await
            .unwrap();

        // A fresh catalog, a fresh manager, and a registry that always fails.
        let offline = Arc::new(FixtureFetch::new().failing(REGISTRY_URL, "offline"));
        let catalog = Arc::new(AgentCatalog::default());
        let restarted = AgentManager::new(
            harness.store.clone(),
            catalog.clone(),
            Arc::new(RegistryClient::new(
                REGISTRY_URL,
                harness.root.join("no-cache"),
                offline.clone(),
            )),
            harness.root.clone(),
            Arc::new(AlwaysPresent),
        );
        assert_eq!(restarted.load_persisted().unwrap(), 1);

        let runtime = catalog
            .runtime("my-package")
            .expect("agent did not survive");
        assert_eq!(runtime.launch.command, "npx");
        assert_eq!(
            runtime.launch.args,
            vec!["--yes", "package-acp@1.0.0", "--acp"]
        );
        assert_eq!(offline.call_count(), 0, "a launch consulted the registry");
    }

    #[tokio::test]
    async fn a_package_runtime_that_is_missing_is_reported_not_hidden() {
        struct NothingPresent;
        impl RuntimeProbe for NothingPresent {
            fn on_path(&self, _program: &str) -> bool {
                false
            }
            fn is_file(&self, _path: &Path) -> bool {
                false
            }
        }
        let archive = tar_gz("example", b"x");
        let http = Arc::new(
            FixtureFetch::new()
                .with(REGISTRY_URL, document("1.0.0", None))
                .with(ARCHIVE_URL, archive),
        );
        let harness = harness_with(Arc::new(AgentCatalog::default()), http);
        let strict = AgentManager::new(
            harness.store.clone(),
            harness.catalog.clone(),
            harness.manager.registry().clone(),
            harness.root.clone(),
            Arc::new(NothingPresent),
        );
        let summary = strict
            .install(InstallRequest {
                registry_id: "package-acp".into(),
                ..InstallRequest::default()
            })
            .await
            .unwrap();
        assert_eq!(summary.availability, AgentAvailability::Unavailable);
        assert!(summary.unavailable_reason.unwrap().contains("npx"));
        // The entry is listed, but no session can start with it.
        assert!(harness.catalog.contains("package-acp"));
        assert!(harness.catalog.runtime("package-acp").is_none());
    }

    #[tokio::test]
    async fn an_install_refuses_an_id_another_source_owns() {
        let catalog = Arc::new(AgentCatalog::new([AgentDefinition::new(
            "example-acp",
            "x",
        )
        .with_source(AgentSource::File)]));
        let archive = tar_gz("example", b"x");
        let http = Arc::new(
            FixtureFetch::new()
                .with(REGISTRY_URL, document("1.0.0", None))
                .with(ARCHIVE_URL, archive),
        );
        let harness = harness_with(catalog, http);
        let error = harness
            .manager
            .install(InstallRequest {
                registry_id: "example-acp".into(),
                ..InstallRequest::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::Conflict(_)));
        assert!(error.to_string().contains("file"), "{error}");
        // The read-only definition is untouched and nothing was stored.
        assert_eq!(
            harness
                .catalog
                .definition("example-acp")
                .unwrap()
                .launch
                .command,
            "x"
        );
        assert!(harness
            .store
            .installed_agent("example-acp")
            .unwrap()
            .is_none());

        // A different catalog id installs the same registry agent cleanly.
        harness
            .manager
            .install(InstallRequest {
                registry_id: "example-acp".into(),
                agent_id: Some("example-2".into()),
                ..InstallRequest::default()
            })
            .await
            .unwrap();
        assert_eq!(
            harness.catalog.source_of("example-2"),
            Some(AgentSource::Registry)
        );
    }

    #[tokio::test]
    async fn an_install_reports_a_missing_registry_entry_and_a_bad_id() {
        let harness = harness();
        let error = harness
            .manager
            .install(InstallRequest {
                registry_id: "absent".into(),
                ..InstallRequest::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::NotFound(_)));

        let error = harness
            .manager
            .install(InstallRequest {
                registry_id: "example-acp".into(),
                agent_id: Some("bad id!".into()),
                ..InstallRequest::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::Invalid(_)));
    }

    #[tokio::test]
    async fn an_install_without_a_registry_reports_it_as_unavailable() {
        let http = Arc::new(FixtureFetch::new().failing(REGISTRY_URL, "offline"));
        let harness = harness_with(Arc::new(AgentCatalog::default()), http);
        let error = harness
            .manager
            .install(InstallRequest {
                registry_id: "example-acp".into(),
                ..InstallRequest::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::Unavailable(_)), "{error}");
    }

    // ------------------------------------------------------------ updating

    #[tokio::test]
    async fn an_update_switches_the_catalog_only_after_the_new_version_is_ready() {
        let harness = harness();
        harness
            .manager
            .install(InstallRequest {
                registry_id: "example-acp".into(),
                ..InstallRequest::default()
            })
            .await
            .unwrap();
        let first_command = harness
            .catalog
            .runtime("example-acp")
            .unwrap()
            .launch
            .command
            .clone();

        // The registry now offers 2.0.0, but the archive cannot be fetched.
        let archive = tar_gz("example", b"v2");
        let digest = super::super::registry::sha256_hex(&archive);
        harness
            .http
            .set(REGISTRY_URL, document("2.0.0", Some(&digest)));
        harness.http.set_failing(ARCHIVE_URL, "download failed");
        let error = harness.manager.update("example-acp").await.unwrap_err();
        assert!(error.to_string().contains("download failed"), "{error}");
        assert_eq!(
            harness
                .catalog
                .runtime("example-acp")
                .unwrap()
                .launch
                .command,
            first_command,
            "the catalog switched before the replacement was ready"
        );
        assert_eq!(
            harness
                .store
                .installed_agent("example-acp")
                .unwrap()
                .unwrap()
                .registry
                .unwrap()
                .registry_version,
            "1.0.0"
        );

        // With the archive available, the update lands.
        harness.http.set(ARCHIVE_URL, archive);
        let outcome = harness.manager.update("example-acp").await.unwrap();
        assert!(outcome.updated);
        assert_eq!(outcome.from_version, "1.0.0");
        assert_eq!(outcome.to_version, "2.0.0");
        assert_eq!(
            outcome.previous_install_dir.as_deref(),
            Some(
                harness
                    .root
                    .join("example-acp/1.0.0")
                    .display()
                    .to_string()
                    .as_str()
            )
        );
        let updated_command = harness
            .catalog
            .runtime("example-acp")
            .unwrap()
            .launch
            .command
            .clone();
        assert!(updated_command.contains("2.0.0"));
        assert!(Path::new(&updated_command).is_file());
        // Both versions exist until the caller removes the old one.
        assert!(harness.root.join("example-acp/1.0.0/example").is_file());
    }

    #[tokio::test]
    async fn an_update_at_the_newest_version_changes_nothing() {
        let harness = harness();
        harness
            .manager
            .install(InstallRequest {
                registry_id: "package-acp".into(),
                ..InstallRequest::default()
            })
            .await
            .unwrap();
        let outcome = harness.manager.update("package-acp").await.unwrap();
        assert!(!outcome.updated);
        assert_eq!(outcome.from_version, "1.0.0");
        assert_eq!(outcome.to_version, "1.0.0");
        assert!(outcome.previous_install_dir.is_none());
    }

    #[tokio::test]
    async fn install_and_update_with_progress_tracker() {
        let harness = harness();
        let tracker = harness
            .manager
            .operations()
            .register(
                "example-acp".into(),
                "example-acp".into(),
                AgentOperationKind::Install,
            )
            .unwrap();

        assert_eq!(tracker.view().stage, AgentOperationStage::Resolving);
        assert_eq!(tracker.view().state, AgentOperationState::Running);

        let summary = harness
            .manager
            .install_with_tracker(
                InstallRequest {
                    registry_id: "example-acp".into(),
                    ..InstallRequest::default()
                },
                Some(tracker.clone()),
            )
            .await
            .unwrap();

        assert_eq!(summary.id, "example-acp");
        assert_eq!(tracker.view().stage, AgentOperationStage::Finalizing);
        tracker.succeed(None, None);
        assert_eq!(tracker.view().state, AgentOperationState::Succeeded);
        assert_eq!(tracker.view().stage, AgentOperationStage::Completed);

        let archive = tar_gz("example", b"v2");
        let digest = super::super::registry::sha256_hex(&archive);
        harness
            .http
            .set(REGISTRY_URL, document("2.0.0", Some(&digest)));
        harness.http.set(ARCHIVE_URL, archive);

        let update_tracker = harness
            .manager
            .operations()
            .register(
                "example-acp".into(),
                "example-acp".into(),
                AgentOperationKind::Update,
            )
            .unwrap();

        let outcome = harness
            .manager
            .update_with_tracker("example-acp", Some(update_tracker.clone()))
            .await
            .unwrap();
        assert!(outcome.updated);
        assert_eq!(update_tracker.view().stage, AgentOperationStage::Finalizing);
        update_tracker.succeed(Some(outcome.updated), Some(outcome.to_version));
        assert_eq!(update_tracker.view().state, AgentOperationState::Succeeded);
        assert_eq!(update_tracker.view().stage, AgentOperationStage::Completed);
    }

    #[tokio::test]
    async fn only_registry_agents_have_an_update() {
        let harness = harness();
        harness
            .manager
            .create_custom(custom("private"))
            .await
            .unwrap();
        let error = harness.manager.update("private").await.unwrap_err();
        assert!(matches!(error, AgentError::Conflict(_)));
        assert!(error.to_string().contains("batey_managed"), "{error}");

        harness
            .catalog
            .insert(AgentDefinition::new("from-file", "x").with_source(AgentSource::File))
            .unwrap();
        let error = harness.manager.update("from-file").await.unwrap_err();
        assert!(error.to_string().contains("read-only"), "{error}");

        assert!(matches!(
            harness.manager.update("absent").await.unwrap_err(),
            AgentError::NotFound(_)
        ));
    }

    // ------------------------------------------------------------ removing

    #[tokio::test]
    async fn removing_an_unused_agent_deletes_its_row_and_its_files() {
        let harness = harness();
        harness
            .manager
            .install(InstallRequest {
                registry_id: "example-acp".into(),
                ..InstallRequest::default()
            })
            .await
            .unwrap();
        assert!(harness.root.join("example-acp/1.0.0").is_dir());

        let outcome = harness.manager.remove("example-acp").await.unwrap();
        assert!(outcome.deleted);
        assert_eq!(outcome.retained_chats, 0);
        assert!(harness
            .store
            .installed_agent("example-acp")
            .unwrap()
            .is_none());
        assert!(!harness.catalog.contains("example-acp"));
        assert!(!harness.root.join("example-acp/1.0.0").exists());
    }

    /// Durable chats keep their agent entry. The entry is retired, so the
    /// chats stay readable and no new session can start.
    #[tokio::test]
    async fn removing_an_agent_that_chats_use_retires_it_instead_of_deleting_it() {
        let harness = harness();
        harness
            .manager
            .install(InstallRequest {
                registry_id: "package-acp".into(),
                ..InstallRequest::default()
            })
            .await
            .unwrap();
        let project = harness
            .store
            .create_project("project".into(), harness.root.display().to_string())
            .unwrap();
        let chat = harness
            .store
            .create_chat(project.id, "package-acp".into(), None)
            .unwrap();

        let outcome = harness.manager.remove("package-acp").await.unwrap();
        assert!(!outcome.deleted);
        assert_eq!(outcome.retained_chats, 1);
        let summary = outcome.agent.unwrap();
        assert_eq!(summary.availability, AgentAvailability::Unavailable);
        assert!(summary.unavailable_reason.unwrap().contains("uninstalled"));

        // The chat and the catalog entry are both still there.
        assert_eq!(harness.store.chat(&chat.id).unwrap().agent, "package-acp");
        assert!(harness.catalog.contains("package-acp"));
        assert!(harness.catalog.runtime("package-acp").is_none());

        // The retired entry survives a restart.
        let catalog = Arc::new(AgentCatalog::default());
        let restarted = AgentManager::new(
            harness.store.clone(),
            catalog.clone(),
            harness.manager.registry().clone(),
            harness.root.clone(),
            Arc::new(AlwaysPresent),
        );
        restarted.load_persisted().unwrap();
        assert_eq!(
            catalog.summaries()[0].availability,
            AgentAvailability::Unavailable
        );
        assert!(catalog.runtime("package-acp").is_none());
    }

    #[tokio::test]
    async fn a_declarative_agent_cannot_be_removed_here() {
        let catalog = Arc::new(AgentCatalog::new([
            AgentDefinition::new("from-file", "x").with_source(AgentSource::File),
            AgentDefinition::codex_default(),
        ]));
        let harness = harness_with(catalog, Arc::new(FixtureFetch::new()));
        for id in ["from-file", "codex"] {
            let error = harness.manager.remove(id).await.unwrap_err();
            assert!(matches!(error, AgentError::Conflict(_)), "{id}: {error}");
            assert!(error.to_string().contains("read-only"), "{id}: {error}");
            assert!(harness.catalog.contains(id));
        }
    }

    /// Removal must never reach outside the managed install root.
    #[tokio::test]
    async fn install_files_outside_the_managed_root_are_never_removed() {
        let harness = harness();
        let outside = harness._tmp.path().join("user-data");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("notes.txt"), "keep me").unwrap();

        harness
            .manager
            .remove_install_files(Some(&outside.display().to_string()));
        assert!(outside.join("notes.txt").is_file());

        let lexical_escape = harness.root.join("..").join("user-data");
        harness
            .manager
            .remove_install_files(Some(&lexical_escape.display().to_string()));
        assert!(outside.join("notes.txt").is_file());

        harness
            .manager
            .remove_install_files(Some(&harness.root.display().to_string()));
        harness.manager.remove_install_files(None);
    }

    // ------------------------------------------------------ custom agents

    #[tokio::test]
    async fn custom_agents_are_created_edited_and_removed() {
        let harness = harness();
        let summary = harness
            .manager
            .create_custom(CustomAgentInput {
                id: "private".into(),
                display_name: Some("Private".into()),
                command: "/opt/private-acp".into(),
                args: vec!["--acp".into()],
                idle_timeout: Some(60),
                usage_provider: Some("internal".into()),
                metadata: Some(serde_json::json!({"team": "platform"})),
                default_permission_policy: Some(CallbackPolicy::ReadOnly),
                description: Some("Our agent".into()),
                ..CustomAgentInput::default()
            })
            .await
            .unwrap();
        assert_eq!(summary.source, AgentSource::BateyManaged);
        assert_eq!(summary.mutability, super::super::AgentMutability::Editable);
        assert_eq!(summary.display_name, "Private");
        assert_eq!(summary.usage_provider.as_deref(), Some("internal"));

        let runtime = harness.catalog.runtime("private").unwrap();
        assert_eq!(runtime.launch.command, "/opt/private-acp");
        assert_eq!(
            runtime.launch.idle_timeout,
            std::time::Duration::from_secs(60)
        );
        assert_eq!(runtime.default_permission_policy, CallbackPolicy::ReadOnly);

        let edited = harness
            .manager
            .edit_custom(
                "private",
                CustomAgentInput {
                    id: "private".into(),
                    command: "/opt/private-acp-v2".into(),
                    ..CustomAgentInput::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(edited.display_name, "private");
        assert_eq!(
            harness.catalog.runtime("private").unwrap().launch.command,
            "/opt/private-acp-v2"
        );
        assert_eq!(
            harness
                .store
                .installed_agent("private")
                .unwrap()
                .unwrap()
                .command,
            "/opt/private-acp-v2"
        );

        let outcome = harness.manager.remove("private").await.unwrap();
        assert!(outcome.deleted);
        assert!(!harness.catalog.contains("private"));
    }

    #[tokio::test]
    async fn custom_agents_survive_a_restart() {
        let harness = harness();
        harness
            .manager
            .create_custom(custom("private"))
            .await
            .unwrap();
        harness
            .manager
            .create_custom(custom("second"))
            .await
            .unwrap();

        let catalog = Arc::new(AgentCatalog::default());
        let restarted = AgentManager::new(
            harness.store.clone(),
            catalog.clone(),
            harness.manager.registry().clone(),
            harness.root.clone(),
            Arc::new(HostRuntimeProbe),
        );
        assert_eq!(restarted.load_persisted().unwrap(), 2);
        assert_eq!(catalog.ids(), vec!["private", "second"]);
        assert_eq!(
            catalog.runtime("private").unwrap().launch.command,
            "/opt/private"
        );
    }

    #[tokio::test]
    async fn creating_a_custom_agent_refuses_a_taken_id() {
        let catalog = Arc::new(AgentCatalog::new([AgentDefinition::codex_default()]));
        let harness = harness_with(catalog, Arc::new(FixtureFetch::new()));
        let error = harness
            .manager
            .create_custom(custom("codex"))
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::Conflict(_)));
        assert!(error.to_string().contains("builtin"), "{error}");
        assert_eq!(
            harness.catalog.definition("codex").unwrap().launch.command,
            "codex-acp"
        );
        assert!(harness.store.installed_agent("codex").unwrap().is_none());
    }

    #[tokio::test]
    async fn editing_refuses_a_definition_another_source_owns() {
        let catalog = Arc::new(AgentCatalog::new([
            AgentDefinition::new("from-file", "x").with_source(AgentSource::File)
        ]));
        let harness = harness_with(catalog, Arc::new(FixtureFetch::new()));
        let error = harness
            .manager
            .edit_custom("from-file", custom("from-file"))
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::Conflict(_)));
        assert_eq!(
            harness
                .catalog
                .definition("from-file")
                .unwrap()
                .launch
                .command,
            "x"
        );
    }

    #[tokio::test]
    async fn editing_refuses_a_registry_agent() {
        let harness = harness();
        harness
            .manager
            .install(InstallRequest {
                registry_id: "package-acp".into(),
                ..InstallRequest::default()
            })
            .await
            .unwrap();
        let error = harness
            .manager
            .edit_custom("package-acp", custom("package-acp"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("registry"), "{error}");
    }

    #[tokio::test]
    async fn validation_reports_taken_ids_and_field_problems() {
        let catalog = Arc::new(AgentCatalog::new([AgentDefinition::codex_default()]));
        let harness = harness_with(catalog, Arc::new(FixtureFetch::new()));

        let report = harness.manager.validate_custom(&custom("private"));
        assert!(report.valid, "{report:?}");

        let report = harness.manager.validate_custom(&custom("codex"));
        assert!(!report.valid);
        assert_eq!(report.issues[0].field, "id");
        assert!(report.issues[0].message.contains("builtin"));

        let report = harness.manager.validate_custom(&CustomAgentInput {
            id: "ok".into(),
            command: String::new(),
            ..CustomAgentInput::default()
        });
        assert!(!report.valid);
        assert_eq!(report.issues[0].field, "command");
        // Validation never stores anything.
        assert!(harness.store.installed_agents().unwrap().is_empty());
    }

    // ------------------------------------------------------------- startup

    #[tokio::test]
    async fn loading_reports_a_collision_between_a_row_and_a_file_definition() {
        let harness = harness();
        harness
            .manager
            .create_custom(custom("shared"))
            .await
            .unwrap();

        let catalog = Arc::new(AgentCatalog::new([AgentDefinition::new(
            "shared", "file-acp",
        )
        .with_source(AgentSource::File)]));
        let restarted = AgentManager::new(
            harness.store.clone(),
            catalog,
            harness.manager.registry().clone(),
            harness.root.clone(),
            Arc::new(AlwaysPresent),
        );
        let error = restarted.load_persisted().unwrap_err();
        assert!(matches!(error, AgentError::Conflict(_)));
        assert!(error.to_string().contains("shared"), "{error}");
        assert!(error.to_string().contains("file"), "{error}");
    }

    // ------------------------------------------- per-agent environment

    #[tokio::test]
    async fn env_overrides_are_scoped_redacted_and_survive_updates() {
        use crate::store::{AgentEnvAction, AgentEnvEdit};
        let harness = harness();
        harness
            .manager
            .install(InstallRequest {
                registry_id: "package-acp".into(),
                agent_id: Some("pkg".into()),
                ..InstallRequest::default()
            })
            .await
            .unwrap();
        let before = harness.store.installed_agent("pkg").unwrap().unwrap();
        let snapshot_env = before.registry.clone().unwrap().distribution;
        let replace = |name: &str, value: &str| AgentEnvEdit {
            name: name.into(),
            value: Some(value.into()),
            action: AgentEnvAction::Replace,
        };

        let presence = harness
            .manager
            .apply_agent_env_edits("pkg", vec![replace("CODEX_API_KEY", "secret")])
            .await
            .unwrap();
        assert_eq!(presence.len(), 1);
        assert_eq!(presence[0].name, "CODEX_API_KEY");
        let exposed = serde_json::to_string(&presence).unwrap();
        assert!(!exposed.contains("secret"));

        // A registry update keeps the snapshot kind and the separate overrides.
        harness.http.set(REGISTRY_URL, document("2.0.0", None));
        let outcome = harness.manager.update("pkg").await.unwrap();
        assert!(outcome.updated);
        let after = harness.store.installed_agent("pkg").unwrap().unwrap();
        assert_eq!(after.registry.as_ref().unwrap().registry_version, "2.0.0");
        assert_eq!(
            after.registry.as_ref().unwrap().distribution.kind(),
            snapshot_env.kind()
        );
        assert_eq!(
            harness.store.agent_env("pkg").unwrap()["CODEX_API_KEY"],
            "secret"
        );

        // Deleting the row cleans up the overrides; retiring keeps them.
        harness
            .manager
            .create_custom(custom("other"))
            .await
            .unwrap();
        harness
            .manager
            .apply_agent_env_edits("other", vec![replace("GH_TOKEN", "t")])
            .await
            .unwrap();
        harness.manager.remove("other").await.unwrap();
        assert!(harness.store.agent_env("other").unwrap().is_empty());
    }

    #[tokio::test]
    async fn env_overrides_reject_non_installed_and_unknown_ids() {
        use crate::store::{AgentEnvAction, AgentEnvEdit};
        let catalog = Arc::new(AgentCatalog::new([
            AgentDefinition::new("from-file", "x").with_source(AgentSource::File)
        ]));
        let harness = harness_with(catalog, harness().http.clone());
        let edit = AgentEnvEdit {
            name: "CODEX_API_KEY".into(),
            value: Some("x".into()),
            action: AgentEnvAction::Replace,
        };
        assert!(matches!(
            harness
                .manager
                .apply_agent_env_edits("absent", vec![edit.clone()])
                .await
                .unwrap_err(),
            AgentError::NotFound(_)
        ));
        assert!(matches!(
            harness
                .manager
                .apply_agent_env_edits("from-file", vec![edit])
                .await
                .unwrap_err(),
            AgentError::Conflict(_)
        ));
    }

    #[tokio::test]
    async fn refreshing_availability_follows_the_host_without_touching_the_rows() {
        let archive = tar_gz("example", b"x");
        let http = Arc::new(
            FixtureFetch::new()
                .with(REGISTRY_URL, document("1.0.0", None))
                .with(ARCHIVE_URL, archive),
        );
        let harness = harness_with(Arc::new(AgentCatalog::default()), http);
        // This test asks what the real host reports, so it uses the host probe.
        let host_manager = AgentManager::new(
            harness.store.clone(),
            harness.catalog.clone(),
            harness.manager.registry().clone(),
            harness.root.clone(),
            Arc::new(HostRuntimeProbe),
        );
        host_manager
            .install(InstallRequest {
                registry_id: "example-acp".into(),
                ..InstallRequest::default()
            })
            .await
            .unwrap();
        assert!(harness.catalog.is_available("example-acp"));

        // The installed binary disappears between runs.
        std::fs::remove_dir_all(harness.root.join("example-acp")).unwrap();
        host_manager.refresh_availability().unwrap();
        assert!(!harness.catalog.is_available("example-acp"));
        assert!(harness.catalog.contains("example-acp"));
        // The durable row is unchanged, so a reinstall still knows the source.
        assert_eq!(
            harness
                .store
                .installed_agent("example-acp")
                .unwrap()
                .unwrap()
                .registry
                .unwrap()
                .registry_version,
            "1.0.0"
        );
    }
}
