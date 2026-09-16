//! Application metadata, never a substitute for the agent's own conversation state.
//!
//! `Store` is the only owner of the SQLite connection. Each domain module holds
//! the SQL for one entity and takes a `&Connection`, so this facade decides how
//! long the lock is held and which calls share a transaction. Domain modules
//! never take the lock themselves, because `std::sync::Mutex` is not reentrant.
mod agent_env;
mod agents;
mod chats;
mod envrc_grants;
mod events;
pub mod migrations;
mod projects;
mod session_config;
mod validation;
mod workspaces;

pub use agent_env::{
    is_valid_env_name, AgentEnvAction, AgentEnvEdit, AgentEnvPresence, MAX_AGENT_ENV_VALUE_LENGTH,
    MAX_AGENT_ENV_VARS,
};
pub use chats::Chat;
pub use envrc_grants::ProjectEnvrcGrant;
pub use projects::Project;
pub use session_config::{
    AdditionalRoot, McpServerConfig, McpServerInput, McpServerView, McpTransport, SecretEdit,
    SecretField, SecretInput,
};
pub use validation::{validate_name, validate_project_path};
pub use workspaces::{ChatWorkspace, WorkspaceMode};

use crate::config::{BateyPaths, PathOverrides};
use rusqlite::{Connection, TransactionBehavior};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Debug)]
pub enum StoreError {
    NotFound(String),
    Validation(String),
    Internal(anyhow::Error),
}

pub type StoreResult<T> = std::result::Result<T, StoreError>;

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(m) | Self::Validation(m) => f.write_str(m),
            Self::Internal(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Internal(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Internal(e.into())
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        Self::Internal(e.into())
    }
}

impl From<anyhow::Error> for StoreError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e)
    }
}

pub struct Store {
    conn: Mutex<Connection>,
    state_dir: PathBuf,
    managed_worktrees: PathBuf,
}

impl Store {
    pub fn open(path: &Path) -> StoreResult<Self> {
        let paths = BateyPaths::from_overrides(PathOverrides {
            database: Some(path.to_path_buf()),
            ..Default::default()
        });
        Self::open_with_paths(&paths)
    }

    pub fn open_with_paths(paths: &BateyPaths) -> StoreResult<Self> {
        if paths.database.as_os_str() != ":memory:" {
            if let Some(parent) = paths
                .database
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
            {
                std::fs::create_dir_all(parent).map_err(|error| {
                    StoreError::Internal(anyhow::anyhow!(
                        "cannot create database directory {}: {error}",
                        parent.display()
                    ))
                })?;
            }
        }
        let mut db = Connection::open(&paths.database)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        migrations::ensure_batey_identity(&mut db, &paths.database)?;
        // Both pragmas must run outside a transaction. SQLite rejects
        // `journal_mode=WAL` inside one and silently ignores `foreign_keys`.
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        migrations::migrate(&mut db)?;
        Ok(Self {
            conn: Mutex::new(db),
            state_dir: paths.state_dir.clone(),
            managed_worktrees: paths.managed_worktrees.clone(),
        })
    }

    /// Directory that holds the database file and Hub-managed state.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Deterministic root for Hub-managed external worktrees:
    /// `<database parent>/worktrees`. The directory is resolved, never
    /// created here; worktree creation stays outside `Store`.
    pub fn worktrees_dir(&self) -> PathBuf {
        self.managed_worktrees.clone()
    }

    /// Alias for the managed-worktree root. Same value as `worktrees_dir`.
    pub fn managed_worktree_root(&self) -> PathBuf {
        self.worktrees_dir()
    }

    pub fn projects(&self) -> StoreResult<Vec<Project>> {
        projects::list(&self.conn.lock().unwrap())
    }

    pub fn project(&self, id: &str) -> StoreResult<Project> {
        projects::get(&self.conn.lock().unwrap(), id)
    }

    pub fn save_project(&self, p: &Project) -> StoreResult<()> {
        projects::save(&self.conn.lock().unwrap(), p)
    }

    pub fn create_project(&self, name: String, path: String) -> StoreResult<Project> {
        let p = projects::new(name, path)?;
        self.save_project(&p)?;
        Ok(p)
    }

    pub fn delete_project(&self, id: &str) -> StoreResult<()> {
        projects::delete(&self.conn.lock().unwrap(), id)
    }

    pub fn project_envrc_grant(&self, project_id: &str) -> StoreResult<Option<ProjectEnvrcGrant>> {
        envrc_grants::get(&self.conn.lock().unwrap(), project_id)
    }

    pub fn remember_project_envrc_grant(
        &self,
        project_id: &str,
        relative_path: String,
        content_hash: String,
    ) -> StoreResult<ProjectEnvrcGrant> {
        envrc_grants::remember(
            &self.conn.lock().unwrap(),
            project_id,
            relative_path,
            content_hash,
        )
    }

    pub fn forget_project_envrc_grant(&self, project_id: &str) -> StoreResult<()> {
        envrc_grants::forget(&self.conn.lock().unwrap(), project_id)
    }

    pub fn chats(&self) -> StoreResult<Vec<Chat>> {
        chats::list(&self.conn.lock().unwrap())
    }

    pub fn chat(&self, id: &str) -> StoreResult<Chat> {
        chats::get(&self.conn.lock().unwrap(), id)
    }

    pub fn create_chat(
        &self,
        project_id: String,
        agent: String,
        title: Option<String>,
    ) -> StoreResult<Chat> {
        let c = self.new_chat(project_id, agent, title)?;
        self.insert_chat(&c)?;
        Ok(c)
    }

    /// Build a chat with its final ID without making it visible yet. Services
    /// use this to name external resources before the durable transaction.
    pub(crate) fn new_chat(
        &self,
        project_id: String,
        agent: String,
        title: Option<String>,
    ) -> StoreResult<Chat> {
        match title {
            Some(title) if !title.trim().is_empty() => {
                chats::new(project_id, agent, Some(title), None)
            }
            _ => {
                let mut db = self.conn.lock().unwrap();
                let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let number = chats::reserve_default_number(&tx)?;
                let chat = chats::new(project_id, agent, None, Some(number))?;
                tx.commit()?;
                Ok(chat)
            }
        }
    }

    fn insert_chat(&self, chat: &Chat) -> StoreResult<()> {
        chats::insert(&self.conn.lock().unwrap(), chat)
    }

    /// Persist a newly-created chat and its workspace as one atomic mutation.
    /// Neither row is visible if either insert fails.
    pub(crate) fn insert_chat_with_workspace(
        &self,
        chat: &Chat,
        workspace: &ChatWorkspace,
    ) -> StoreResult<()> {
        let mut db = self.conn.lock().unwrap();
        let tx = db.transaction()?;
        chats::insert(&tx, chat)?;
        workspaces::insert(&tx, workspace)?;
        tx.commit()?;
        Ok(())
    }

    /// Read/modify/write under one lock so a config notification cannot overwrite a rename.
    pub fn update_chat(&self, id: &str, edit: impl FnOnce(&mut Chat)) -> StoreResult<Chat> {
        chats::update(&self.conn.lock().unwrap(), id, edit)
    }

    pub fn touch_chat(&self, id: &str, updated_at: &str) -> StoreResult<Chat> {
        chats::touch(&self.conn.lock().unwrap(), id, updated_at)
    }

    /// A chat and its events go together, so one transaction covers both tables.
    /// The workspace row goes away through the `chat_workspaces` foreign key.
    pub fn delete_chat(&self, id: &str) -> StoreResult<()> {
        let mut db = self.conn.lock().unwrap();
        let tx = db.transaction()?;
        chats::delete(&tx, id)?;
        events::delete_for_session(&tx, id)?;
        tx.commit()?;
        Ok(())
    }

    pub fn insert_workspace(&self, ws: &ChatWorkspace) -> StoreResult<()> {
        workspaces::insert(&self.conn.lock().unwrap(), ws)
    }

    /// Returns `None` for chats without a workspace row, which includes
    /// every chat created before workspaces existed.
    pub fn workspace(&self, chat_id: &str) -> StoreResult<Option<ChatWorkspace>> {
        workspaces::get(&self.conn.lock().unwrap(), chat_id)
    }

    pub fn delete_workspace(&self, chat_id: &str) -> StoreResult<()> {
        workspaces::delete(&self.conn.lock().unwrap(), chat_id)
    }

    pub fn list_workspaces(&self) -> StoreResult<Vec<ChatWorkspace>> {
        workspaces::list(&self.conn.lock().unwrap())
    }

    pub fn list_project_workspaces(&self, project_id: &str) -> StoreResult<Vec<ChatWorkspace>> {
        workspaces::list_for_project(&self.conn.lock().unwrap(), project_id)
    }

    pub fn mcp_servers(&self, chat_id: &str) -> StoreResult<Vec<McpServerConfig>> {
        session_config::mcp_list(&self.conn.lock().unwrap(), chat_id)
    }
    pub fn mcp_server(&self, chat_id: &str, id: &str) -> StoreResult<McpServerConfig> {
        session_config::mcp_get(&self.conn.lock().unwrap(), chat_id, id)
    }
    pub fn insert_mcp_server(&self, value: &McpServerConfig) -> StoreResult<()> {
        session_config::mcp_insert(&self.conn.lock().unwrap(), value)
    }
    pub fn update_mcp_server(&self, value: &McpServerConfig) -> StoreResult<()> {
        session_config::mcp_update(&self.conn.lock().unwrap(), value)
    }
    pub fn delete_mcp_server(&self, chat_id: &str, id: &str) -> StoreResult<()> {
        session_config::mcp_delete(&self.conn.lock().unwrap(), chat_id, id)
    }
    pub fn reorder_mcp_servers(&self, chat_id: &str, ids: &[String]) -> StoreResult<()> {
        session_config::mcp_reorder(&mut self.conn.lock().unwrap(), chat_id, ids)
    }
    pub fn additional_roots(&self, chat_id: &str) -> StoreResult<Vec<AdditionalRoot>> {
        session_config::roots_list(&self.conn.lock().unwrap(), chat_id)
    }
    pub fn replace_additional_roots(
        &self,
        chat_id: &str,
        roots: &[AdditionalRoot],
    ) -> StoreResult<()> {
        session_config::roots_replace(&mut self.conn.lock().unwrap(), chat_id, roots)
    }
    /// Whether a chat outside `project_id` still lists it as an additional
    /// workspace root. A reference from a chat that belongs to `project_id`
    /// itself does not count, since deleting the project deletes that chat
    /// too.
    pub fn additional_root_references_project_externally(
        &self,
        project_id: &str,
    ) -> StoreResult<bool> {
        session_config::roots_reference_project_externally(&self.conn.lock().unwrap(), project_id)
    }

    /// Every installed-agent record, sorted by id. This is the durable half
    /// of the one runtime catalog.
    pub fn installed_agents(&self) -> StoreResult<Vec<crate::agents::InstalledAgent>> {
        agents::list(&self.conn.lock().unwrap())
    }

    pub fn installed_agent(&self, id: &str) -> StoreResult<Option<crate::agents::InstalledAgent>> {
        agents::get(&self.conn.lock().unwrap(), id)
    }

    pub fn insert_agent(&self, agent: &crate::agents::InstalledAgent) -> StoreResult<()> {
        agents::insert(&self.conn.lock().unwrap(), agent)
    }

    pub fn update_agent(&self, agent: &crate::agents::InstalledAgent) -> StoreResult<()> {
        agents::update(&self.conn.lock().unwrap(), agent)
    }

    pub fn delete_agent(&self, id: &str) -> StoreResult<()> {
        agents::delete(&self.conn.lock().unwrap(), id)
    }

    /// How many chats name this agent. Uninstall reads it before it decides
    /// whether a row may go away or must be retired instead.
    pub fn chat_count_for_agent(&self, agent_id: &str) -> StoreResult<u64> {
        agents::chat_count_for_agent(&self.conn.lock().unwrap(), agent_id)
    }

    pub fn referenced_agent_ids(&self) -> StoreResult<Vec<String>> {
        agents::referenced_agent_ids(&self.conn.lock().unwrap())
    }

    pub fn installed_agent_sources(
        &self,
    ) -> StoreResult<Vec<(String, crate::agents::AgentSource)>> {
        agents::sources(&self.conn.lock().unwrap())
    }

    /// Private per-agent environment values, for the process-spawn path only.
    /// API output must use `agent_env_presence`.
    pub fn agent_env(
        &self,
        agent_id: &str,
    ) -> StoreResult<std::collections::BTreeMap<String, String>> {
        agent_env::list(&self.conn.lock().unwrap(), agent_id)
    }

    /// Names and presence only, sorted by name. Values never leave the store.
    pub fn agent_env_presence(&self, agent_id: &str) -> StoreResult<Vec<AgentEnvPresence>> {
        agent_env::presence(&self.conn.lock().unwrap(), agent_id)
    }

    /// Applies `Keep`/`Replace`/`Remove` edits and returns the presence view.
    pub fn apply_agent_env_edits(
        &self,
        agent_id: &str,
        edits: &[AgentEnvEdit],
    ) -> StoreResult<Vec<AgentEnvPresence>> {
        let conn = self.conn.lock().unwrap();
        agent_env::apply_edits(&conn, agent_id, edits)?;
        agent_env::presence(&conn, agent_id)
    }

    /// Deletes every override for one agent. Uninstall calls this when the
    /// installed row goes away.
    pub fn delete_agent_env_for_agent(&self, agent_id: &str) -> StoreResult<()> {
        agent_env::delete_for_agent(&self.conn.lock().unwrap(), agent_id)
    }

    pub fn save_event(&self, event: &crate::events::SessionEvent) -> StoreResult<()> {
        events::save(&self.conn.lock().unwrap(), event)
    }

    pub fn events(&self) -> StoreResult<Vec<crate::events::SessionEvent>> {
        events::recent(&self.conn.lock().unwrap())
    }

    /// Durable rows needed to repair pending permissions and interrupted
    /// turns on startup, in sequence order. See `events::recovery`.
    pub fn recovery_events(&self) -> StoreResult<Vec<crate::events::SessionEvent>> {
        events::recovery(&self.conn.lock().unwrap())
    }

    pub fn event_page(
        &self,
        from_seq: u64,
        through_seq: u64,
        limit: usize,
    ) -> StoreResult<Vec<crate::events::SessionEvent>> {
        events::page(&self.conn.lock().unwrap(), from_seq, through_seq, limit)
    }

    pub fn chat_event_page(
        &self,
        session_id: &str,
        before_seq: Option<u64>,
        through_seq: Option<u64>,
        limit: usize,
    ) -> StoreResult<(Vec<crate::events::SessionEvent>, bool)> {
        events::chat_page(
            &self.conn.lock().unwrap(),
            session_id,
            before_seq,
            through_seq,
            limit,
        )
    }

    pub fn active_turn_started_at(
        &self,
        session_id: &str,
    ) -> StoreResult<Option<chrono::DateTime<chrono::Utc>>> {
        events::active_turn_started_at(&self.conn.lock().unwrap(), session_id)
    }

    pub fn max_event_seq(&self) -> StoreResult<u64> {
        events::max_seq(&self.conn.lock().unwrap())
    }

    #[cfg(test)]
    pub(crate) fn set_query_only(&self) {
        self.conn
            .lock()
            .unwrap()
            .execute_batch("PRAGMA query_only=ON")
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn restart_and_independent_chats() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hub.db");
        let db = Store::open(&path).unwrap();
        let p = db
            .create_project("project".into(), tmp.path().display().to_string())
            .unwrap();
        let a = db
            .create_chat(p.id.clone(), "codex".into(), Some("one".into()))
            .unwrap();
        let b = db
            .create_chat(p.id.clone(), "codex".into(), Some("two".into()))
            .unwrap();
        assert_ne!(a.id, b.id);
        db.update_chat(&a.id, |c| c.acp_session_id = Some("remote-one".into()))
            .unwrap();
        drop(db);
        let db = Store::open(&path).unwrap();
        assert_eq!(db.projects().unwrap().len(), 1);
        assert_eq!(db.chats().unwrap().len(), 2);
        assert_eq!(
            db.chat(&a.id).unwrap().acp_session_id.as_deref(),
            Some("remote-one")
        );
        assert!(db.chat(&b.id).unwrap().acp_session_id.is_none());
    }

    #[test]
    fn default_chat_titles_are_numbered_and_not_reused_after_restart_or_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hub.db");
        let db = Store::open(&path).unwrap();
        let project = db
            .create_project("project".into(), tmp.path().display().to_string())
            .unwrap();

        let first = db
            .create_chat(project.id.clone(), "codex".into(), None)
            .unwrap();
        let second = db
            .create_chat(project.id.clone(), "codex".into(), None)
            .unwrap();
        assert_eq!(first.title, "New chat 1");
        assert_eq!(second.title, "New chat 2");
        assert!(!first.title_overridden);
        assert!(!second.title_overridden);

        db.delete_chat(&second.id).unwrap();
        drop(db);

        let db = Store::open(&path).unwrap();
        let third = db.create_chat(project.id, "codex".into(), None).unwrap();
        assert_eq!(third.title, "New chat 3");
        assert!(!third.title_overridden);
    }

    #[test]
    fn manual_title_wins_against_a_racing_generated_update() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(Store::open(&tmp.path().join("hub.db")).unwrap());
        let project = db
            .create_project("project".into(), tmp.path().display().to_string())
            .unwrap();
        let chat = db.create_chat(project.id, "codex".into(), None).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));

        let generated_db = db.clone();
        let generated_barrier = barrier.clone();
        let generated_id = chat.id.clone();
        let generated = std::thread::spawn(move || {
            generated_barrier.wait();
            generated_db
                .update_chat(&generated_id, |c| {
                    if !c.title_overridden {
                        c.title = "Generated title".into();
                    }
                })
                .unwrap();
        });

        let manual_db = db.clone();
        let manual_barrier = barrier;
        let manual_id = chat.id.clone();
        let manual = std::thread::spawn(move || {
            manual_barrier.wait();
            manual_db
                .update_chat(&manual_id, |c| {
                    c.title = "Manual title".into();
                    c.title_overridden = true;
                })
                .unwrap();
        });

        generated.join().unwrap();
        manual.join().unwrap();
        let final_chat = db.chat(&chat.id).unwrap();
        assert_eq!(final_chat.title, "Manual title");
        assert!(final_chat.title_overridden);
    }

    /// Two writers that touch different fields must not lose each other's work.
    /// A read outside the lock followed by a write inside it would drop one.
    #[test]
    fn update_chat_is_read_modify_write() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(Store::open(&tmp.path().join("hub.db")).unwrap());
        let p = db
            .create_project("project".into(), tmp.path().display().to_string())
            .unwrap();
        let chat = db.create_chat(p.id, "codex".into(), None).unwrap();

        let titles = {
            let db = db.clone();
            let id = chat.id.clone();
            std::thread::spawn(move || {
                for i in 0..50 {
                    db.update_chat(&id, |c| c.title = format!("title-{i}"))
                        .unwrap();
                }
            })
        };
        let sessions = {
            let db = db.clone();
            let id = chat.id.clone();
            std::thread::spawn(move || {
                for i in 0..50 {
                    db.update_chat(&id, |c| c.acp_session_id = Some(format!("acp-{i}")))
                        .unwrap();
                }
            })
        };
        titles.join().unwrap();
        sessions.join().unwrap();

        let final_chat = db.chat(&chat.id).unwrap();
        assert!(final_chat.title.starts_with("title-"), "rename was lost");
        assert!(
            final_chat
                .acp_session_id
                .as_deref()
                .is_some_and(|s| s.starts_with("acp-")),
            "ACP session id was lost"
        );
    }

    #[test]
    fn chats_are_listed_by_activity_newest_first_with_id_tie_breaker() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Store::open(&tmp.path().join("hub.db")).unwrap();
        let project = db
            .create_project("project".into(), tmp.path().display().to_string())
            .unwrap();
        let older = db
            .create_chat(project.id.clone(), "codex".into(), Some("older".into()))
            .unwrap();
        let newer = db
            .create_chat(project.id, "codex".into(), Some("newer".into()))
            .unwrap();

        db.touch_chat(&older.id, "2026-01-01T00:00:00Z").unwrap();
        db.touch_chat(&newer.id, "2026-02-01T00:00:00Z").unwrap();
        let listed = db.chats().unwrap();
        assert_eq!(listed[0].id, newer.id);
        assert_eq!(listed[1].id, older.id);

        db.touch_chat(&older.id, "2026-02-01T00:00:00Z").unwrap();
        let tied = db.chats().unwrap();
        let mut expected = [newer.id.clone(), older.id.clone()];
        expected.sort_by(|left, right| right.cmp(left));
        assert_eq!(tied[0].id, expected[0]);
        assert_eq!(tied[1].id, expected[1]);
    }

    #[test]
    fn worktrees_dir_lives_beside_database() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hub.db");
        let db = Store::open(&path).unwrap();
        assert_eq!(db.state_dir(), tmp.path());
        assert_eq!(db.worktrees_dir(), tmp.path().join("worktrees"));
        assert_eq!(db.managed_worktree_root(), tmp.path().join("worktrees"));
    }

    #[test]
    fn configured_worktrees_are_used_by_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = BateyPaths::from_overrides(PathOverrides {
            database: Some(tmp.path().join("hub.db")),
            managed_worktrees: Some(tmp.path().join("managed")),
            ..Default::default()
        });
        let db = Store::open_with_paths(&paths).unwrap();
        assert_eq!(db.worktrees_dir(), tmp.path().join("managed"));
    }

    #[test]
    fn open_with_paths_creates_only_missing_database_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let database_parent = tmp.path().join("new").join("nested");
        let paths = BateyPaths::from_overrides(PathOverrides {
            database: Some(database_parent.join("hub.db")),
            ..Default::default()
        });

        assert!(!database_parent.exists());
        let store = Store::open_with_paths(&paths).unwrap();
        assert!(database_parent.is_dir());
        assert!(paths.database.is_file());
        assert!(!tmp.path().join("config").exists());
        assert!(!tmp.path().join("logs").exists());
        assert_eq!(store.worktrees_dir(), database_parent.join("worktrees"));
    }

    #[test]
    fn relative_database_paths_resolve_under_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let resolved = BateyPaths::state_dir_for_database(Path::new("hub.db"));
        assert_eq!(resolved, cwd);
        let resolved = BateyPaths::state_dir_for_database(Path::new("data/hub.db"));
        assert_eq!(resolved, cwd.join("data"));
        let absolute = std::path::PathBuf::from("/var/lib/batey/hub.db");
        assert_eq!(
            BateyPaths::state_dir_for_database(&absolute),
            std::path::PathBuf::from("/var/lib/batey")
        );
    }

    /// Deleting one chat must not touch another chat's history.
    #[test]
    fn delete_chat_only_removes_that_chat_events() {
        use crate::events::EventPayload;
        let tmp = tempfile::tempdir().unwrap();
        let db = Arc::new(Store::open(&tmp.path().join("hub.db")).unwrap());
        let p = db
            .create_project("project".into(), tmp.path().display().to_string())
            .unwrap();
        let doomed = db.create_chat(p.id.clone(), "codex".into(), None).unwrap();
        let kept = db.create_chat(p.id, "codex".into(), None).unwrap();

        let log = crate::events::EventLog::persistent(db.clone()).unwrap();
        log.append(
            &doomed.id,
            "codex",
            EventPayload::MessageChunk {
                message_id: None,
                text: "a".into(),
                content: vec![],
            },
        )
        .unwrap();
        log.append(
            &kept.id,
            "codex",
            EventPayload::MessageChunk {
                message_id: None,
                text: "b".into(),
                content: vec![],
            },
        )
        .unwrap();

        db.delete_chat(&doomed.id).unwrap();

        let remaining = db.events().unwrap();
        assert!(remaining.iter().all(|e| e.session_id != doomed.id));
        assert_eq!(
            remaining.iter().filter(|e| e.session_id == kept.id).count(),
            1
        );
    }

    #[test]
    fn chat_event_pages_are_bounded_and_do_not_scan_other_chat_history() {
        use crate::events::{EventPayload, SessionEvent};
        let tmp = tempfile::tempdir().unwrap();
        let db = Store::open(&tmp.path().join("hub.db")).unwrap();
        let mut connection = db.conn.lock().unwrap();
        let transaction = connection.transaction().unwrap();
        for index in 0..20_050 {
            let event = SessionEvent {
                seq: index + 1,
                timestamp: chrono::Utc::now(),
                session_id: if index % 2 == 0 {
                    "target".into()
                } else {
                    "other".into()
                },
                agent: "codex".into(),
                payload: EventPayload::MessageChunk {
                    message_id: None,
                    text: index.to_string(),
                    content: vec![],
                },
            };
            transaction
                .execute(
                    "INSERT INTO events (seq, session_id, data) VALUES (?1, ?2, ?3)",
                    rusqlite::params![
                        event.seq as i64,
                        event.session_id,
                        serde_json::to_string(&event).unwrap(),
                    ],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        drop(connection);

        let (first, has_older) = db.chat_event_page("target", None, None, 100).unwrap();
        assert_eq!(first.len(), 100);
        assert!(has_older);
        assert!(first.iter().all(|event| event.session_id == "target"));
        assert!(first.windows(2).all(|pair| pair[0].seq < pair[1].seq));

        let (bounded, has_older) = db
            .chat_event_page("target", None, Some(10_000), 100)
            .unwrap();
        assert!(bounded.iter().all(|event| event.seq <= 10_000));
        assert!(has_older);

        let mut total = first.len();
        if has_older {
            let mut cursor = first.first().unwrap().seq;
            loop {
                let (page, more) = db
                    .chat_event_page("target", Some(cursor), None, 100)
                    .unwrap();
                if page.is_empty() {
                    break;
                }
                assert!(page.iter().all(|event| event.session_id == "target"));
                cursor = page.first().unwrap().seq;
                total += page.len();
                if !more {
                    break;
                }
            }
        }
        assert_eq!(total, 10_025);
    }
}
