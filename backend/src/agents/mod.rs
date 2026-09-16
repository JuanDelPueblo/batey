//! The set of agents this Batey knows about.
//!
//! Every source feeds one catalog: the built-in defaults, the file
//! `--agents-file` names, the ACP Registry, and the definitions a user creates
//! through the management API. Ownership is explicit, so a mutable API never
//! changes a declarative definition and an id collision is reported instead of
//! resolved by precedence.
mod compat;
mod custom;
mod declarative;
mod definition;
mod file;
mod installed;
mod manager;
pub mod registry;

pub use compat::{
    auth_env_defaults, method_warning, AuthEnvScope, ANTIGRAVITY_AUTH_WARNING,
    ANTIGRAVITY_REGISTRY_ID, CODEX_REGISTRY_ID, COPILOT_REGISTRY_ID, OPENCODE_REGISTRY_ID,
};

pub use custom::{CustomAgentInput, ValidationIssue, ValidationReport, MAX_IDLE_TIMEOUT_SECS};
pub use declarative::parse_declarative_agents;
pub use definition::{
    AgentAvailability, AgentDefinition, AgentDisplay, AgentLaunch, AgentMutability, AgentRuntime,
    AgentSource, AgentSummary, DEFAULT_IDLE_TIMEOUT_SECS,
};
pub use file::parse_agents;
pub use installed::{
    which, which_in, HostRuntimeProbe, InstalledAgent, InstalledDistribution, RegistrySnapshot,
    RuntimeProbe,
};
pub use manager::{
    AgentEnvEdit, AgentEnvPresence, AgentError, AgentManagementDetail, AgentManager, AgentResult,
    InstallRequest, RegistryCatalogView, RegistryEntryView, RegistryStatus, RemoveOutcome,
    UpdateOutcome,
};
pub use registry::{DistributionKind, PlatformTarget};

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::RwLock;

/// The shared installed-agent catalog. The lock makes replacement and
/// removal safe for future registry and web-managed sources while callers
/// retain stable runtime handles for sessions already in flight.
#[derive(Debug, Default)]
pub struct AgentCatalog {
    entries: RwLock<BTreeMap<String, AgentEntry>>,
}

#[derive(Debug)]
struct AgentEntry {
    definition: Arc<AgentDefinition>,
    runtime: Arc<AgentRuntime>,
}

impl AgentEntry {
    fn new(definition: AgentDefinition) -> Self {
        Self {
            runtime: Arc::new(definition.runtime()),
            definition: Arc::new(definition),
        }
    }
}

/// Two sources claimed the same agent id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogCollision {
    pub id: String,
    pub existing: AgentSource,
    pub incoming: AgentSource,
}

impl std::fmt::Display for CatalogCollision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Agent id '{}' is already defined by the {} source, so the {} source cannot use it. \
             Choose a different id or remove the other definition.",
            self.id, self.existing, self.incoming
        )
    }
}

impl std::error::Error for CatalogCollision {}

impl AgentCatalog {
    pub fn new(definitions: impl IntoIterator<Item = AgentDefinition>) -> Self {
        definitions.into_iter().collect()
    }

    pub fn definition(&self, id: &str) -> Option<Arc<AgentDefinition>> {
        self.entries
            .read()
            .expect("agent catalog lock poisoned")
            .get(id)
            .map(|entry| entry.definition.clone())
    }

    pub fn runtime(&self, id: &str) -> Option<Arc<AgentRuntime>> {
        self.entries
            .read()
            .expect("agent catalog lock poisoned")
            .get(id)
            .filter(|entry| entry.definition.availability == AgentAvailability::Available)
            .map(|entry| entry.runtime.clone())
    }

    pub fn contains(&self, id: &str) -> bool {
        self.entries
            .read()
            .expect("agent catalog lock poisoned")
            .contains_key(id)
    }

    pub fn is_available(&self, id: &str) -> bool {
        self.runtime(id).is_some()
    }

    /// Sorted agent ids.
    pub fn ids(&self) -> Vec<String> {
        self.entries
            .read()
            .expect("agent catalog lock poisoned")
            .keys()
            .cloned()
            .collect()
    }

    pub fn definitions(&self) -> Vec<Arc<AgentDefinition>> {
        self.entries
            .read()
            .expect("agent catalog lock poisoned")
            .values()
            .map(|entry| entry.definition.clone())
            .collect()
    }

    pub fn summaries(&self) -> Vec<AgentSummary> {
        self.entries
            .read()
            .expect("agent catalog lock poisoned")
            .values()
            .map(|entry| entry.definition.summary())
            .collect()
    }

    /// The source that owns one entry, so a caller can check ownership
    /// before it tries a mutation.
    pub fn source_of(&self, id: &str) -> Option<AgentSource> {
        self.entries
            .read()
            .expect("agent catalog lock poisoned")
            .get(id)
            .map(|entry| entry.definition.source)
    }

    /// Adds one entry. An id that another source already owns is a collision
    /// and is reported, never resolved by precedence.
    pub fn insert(&self, definition: AgentDefinition) -> Result<(), CatalogCollision> {
        let mut entries = self.entries.write().expect("agent catalog lock poisoned");
        if let Some(existing) = entries.get(&definition.id) {
            return Err(CatalogCollision {
                id: definition.id,
                existing: existing.definition.source,
                incoming: definition.source,
            });
        }
        let id = definition.id.clone();
        entries.insert(id, AgentEntry::new(definition));
        Ok(())
    }

    /// Replaces one entry that the same source already owns. A registry
    /// update and a custom edit both land here, so neither can take an id
    /// away from another source.
    pub fn replace(
        &self,
        definition: AgentDefinition,
    ) -> Result<AgentDefinition, CatalogCollision> {
        let mut entries = self.entries.write().expect("agent catalog lock poisoned");
        let existing = entries.get(&definition.id).ok_or(CatalogCollision {
            id: definition.id.clone(),
            existing: definition.source,
            incoming: definition.source,
        })?;
        if existing.definition.source != definition.source {
            return Err(CatalogCollision {
                id: definition.id,
                existing: existing.definition.source,
                incoming: definition.source,
            });
        }
        let previous = (*existing.definition).clone();
        entries.insert(definition.id.clone(), AgentEntry::new(definition));
        Ok(previous)
    }

    /// Inserts or replaces one installed agent. Existing session runtime
    /// handles remain valid because the catalog stores each projection in an
    /// `Arc` and only new lookups observe the replacement.
    pub fn upsert(&self, definition: AgentDefinition) -> Option<AgentDefinition> {
        let id = definition.id.clone();
        self.entries
            .write()
            .expect("agent catalog lock poisoned")
            .insert(id, AgentEntry::new(definition))
            .map(|old| (*old.definition).clone())
    }

    pub fn remove(&self, id: &str) -> Option<AgentDefinition> {
        self.entries
            .write()
            .expect("agent catalog lock poisoned")
            .remove(id)
            .map(|old| (*old.definition).clone())
    }

    pub fn len(&self) -> usize {
        self.entries
            .read()
            .expect("agent catalog lock poisoned")
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl FromIterator<AgentDefinition> for AgentCatalog {
    fn from_iter<I: IntoIterator<Item = AgentDefinition>>(iter: I) -> Self {
        let catalog = Self::default();
        for definition in iter {
            catalog.upsert(definition);
        }
        catalog
    }
}

/// Compatibility name for callers that construct the pre-catalog model.
/// New code should use `AgentCatalog`.
pub type AgentRegistry = AgentCatalog;

/// Detect the small, explicit set of local ACP agents supported by Batey.
pub fn builtin_agents(probe: &dyn RuntimeProbe) -> Vec<AgentDefinition> {
    if probe.on_path("opencode") {
        vec![AgentDefinition::opencode_default()]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> AgentCatalog {
        AgentCatalog::new([AgentDefinition::opencode_default()])
    }

    struct FakeProbe(bool);

    impl RuntimeProbe for FakeProbe {
        fn on_path(&self, program: &str) -> bool {
            self.0 && program == "opencode"
        }

        fn is_file(&self, _path: &std::path::Path) -> bool {
            false
        }
    }

    #[test]
    fn builtins_are_empty_without_opencode() {
        assert!(builtin_agents(&FakeProbe(false)).is_empty());
    }

    #[test]
    fn builtins_include_only_opencode_when_present() {
        let builtins = builtin_agents(&FakeProbe(true));
        assert_eq!(
            builtins
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            ["opencode"]
        );
        assert_eq!(builtins[0].launch.command, "opencode");
        assert_eq!(builtins[0].launch.args, ["acp"]);
        assert_eq!(builtins[0].source, AgentSource::Builtin);
    }

    #[test]
    fn ids_are_sorted() {
        assert_eq!(registry().ids(), vec!["opencode"]);
    }

    #[test]
    fn lookup_by_id() {
        let registry = registry();
        assert!(registry.contains("opencode"));
        assert!(!registry.contains("gemini"));
        assert_eq!(registry.definition("opencode").unwrap().id, "opencode");
        assert_eq!(
            registry.runtime("opencode").unwrap().launch.command,
            "opencode"
        );
        assert!(registry.runtime("gemini").is_none());
        assert_eq!(registry.len(), 1);
        assert!(!registry.is_empty());
        assert!(AgentCatalog::default().is_empty());
    }

    /// The runtime handle must reflect the definition it was built from.
    #[test]
    fn runtime_matches_definition() {
        let registry =
            AgentCatalog::new([AgentDefinition::codex_default().with_command("custom-acp".into())]);
        assert_eq!(
            registry.runtime("codex").unwrap().launch.command,
            "custom-acp"
        );
    }

    #[test]
    fn catalog_can_replace_and_remove_definitions() {
        let catalog = AgentCatalog::new([AgentDefinition::codex_default()]);
        catalog.upsert(AgentDefinition::codex_default().with_command("replacement".into()));
        assert_eq!(
            catalog.runtime("codex").unwrap().launch.command,
            "replacement"
        );
        assert!(catalog.remove("codex").is_some());
        assert!(catalog.runtime("codex").is_none());
    }

    /// Requirement of the management API: an id that another source already
    /// owns fails loudly rather than replacing that source's definition.
    #[test]
    fn insert_reports_a_source_collision_instead_of_applying_precedence() {
        let catalog = AgentCatalog::new([AgentDefinition::codex_default()]);
        let collision = catalog
            .insert(
                AgentDefinition::new("codex", "my-codex").with_source(AgentSource::BateyManaged),
            )
            .unwrap_err();
        assert_eq!(collision.id, "codex");
        assert_eq!(collision.existing, AgentSource::Builtin);
        assert_eq!(collision.incoming, AgentSource::BateyManaged);
        assert!(collision.to_string().contains("already defined"));
        // The original definition is untouched.
        assert_eq!(
            catalog.definition("codex").unwrap().launch.command,
            "codex-acp"
        );

        catalog
            .insert(AgentDefinition::new("private", "private-acp"))
            .unwrap();
        assert!(catalog.contains("private"));
    }

    #[test]
    fn replace_only_accepts_the_owning_source() {
        let catalog = AgentCatalog::new([
            AgentDefinition::new("managed", "v1").with_source(AgentSource::BateyManaged)
        ]);
        let previous = catalog
            .replace(AgentDefinition::new("managed", "v2").with_source(AgentSource::BateyManaged))
            .unwrap();
        assert_eq!(previous.launch.command, "v1");
        assert_eq!(catalog.runtime("managed").unwrap().launch.command, "v2");
        assert_eq!(
            catalog.source_of("managed"),
            Some(AgentSource::BateyManaged)
        );

        let collision = catalog
            .replace(AgentDefinition::new("managed", "v3").with_source(AgentSource::Registry))
            .unwrap_err();
        assert_eq!(collision.existing, AgentSource::BateyManaged);
        assert_eq!(catalog.runtime("managed").unwrap().launch.command, "v2");

        assert!(catalog
            .replace(AgentDefinition::new("absent", "x"))
            .is_err());
        assert_eq!(catalog.source_of("absent"), None);
    }

    /// A session already holding a runtime handle must keep working when the
    /// catalog entry behind it is replaced or removed.
    #[test]
    fn live_runtime_handles_survive_catalog_changes() {
        let catalog = AgentCatalog::new([
            AgentDefinition::new("managed", "v1").with_source(AgentSource::BateyManaged)
        ]);
        let live = catalog.runtime("managed").unwrap();

        catalog
            .replace(AgentDefinition::new("managed", "v2").with_source(AgentSource::BateyManaged))
            .unwrap();
        assert_eq!(
            live.launch.command, "v1",
            "a live handle changed under a session"
        );
        assert_eq!(catalog.runtime("managed").unwrap().launch.command, "v2");

        catalog.remove("managed");
        assert_eq!(live.launch.command, "v1");
        assert!(catalog.runtime("managed").is_none());
    }

    #[test]
    fn unavailable_definition_is_listed_but_has_no_runtime() {
        let catalog = AgentCatalog::new([AgentDefinition::codex_default().with_available(false)]);
        assert_eq!(
            catalog.summaries()[0].availability,
            AgentAvailability::Unavailable
        );
        assert!(catalog.definition("codex").is_some());
        assert!(catalog.runtime("codex").is_none());
    }
}
