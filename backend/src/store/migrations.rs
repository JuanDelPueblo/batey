//! Ordered, versioned schema migrations.
//!
//! `PRAGMA application_id` identifies Batey databases. Fresh databases are
//! stamped with `BATEY_APPLICATION_ID` (`0x42415445` / `"BATE"`). Legacy
//! databases and unrecognized SQLite databases are refused without
//! mutation.
//!
//! `PRAGMA user_version` holds the schema version. Each migration runs in its
//! own transaction and advances that version inside the same transaction, so a
//! failure leaves the database exactly where it was.
//!
//! A migration may only use schema changes and narrowly-scoped data
//! initialization (`CREATE TABLE`, `CREATE INDEX`, `ALTER TABLE ... ADD
//! COLUMN`, `INSERT`, and `UPDATE`). SQLite's 12-step table rebuild
//! needs `PRAGMA foreign_keys=OFF` outside the transaction, so it needs its own
//! handling if it is ever required.
use anyhow::{Context, Result};
use rusqlite::{Connection, TransactionBehavior};
use std::path::Path;

/// Stable SQLite application ID identifying Batey databases.
/// ASCII representation: 0x4241_5445 is "BATE".
pub const BATEY_APPLICATION_ID: i32 = 0x4241_5445;

pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
    /// A query returning non-zero while `sql` still needs to run.
    pub precondition: Option<&'static str>,
}

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "baseline_v1",
        sql: "
        CREATE TABLE IF NOT EXISTS projects (
            id TEXT PRIMARY KEY,
            data TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS chats (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            data TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_chats_project_id ON chats(project_id);
        CREATE TABLE IF NOT EXISTS chat_workspaces (
            chat_id TEXT PRIMARY KEY REFERENCES chats(id) ON DELETE CASCADE,
            project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            mode TEXT NOT NULL CHECK(mode IN ('managed_worktree', 'project_checkout')),
            repository_root TEXT NOT NULL,
            workspace_path TEXT NOT NULL,
            project_subdir TEXT NOT NULL DEFAULT '',
            branch TEXT,
            base_commit TEXT,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_chat_workspaces_project_id ON chat_workspaces(project_id);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_chat_workspaces_managed_path ON chat_workspaces(workspace_path) WHERE mode='managed_worktree';
        CREATE TABLE IF NOT EXISTS chat_title_sequence (
            id INTEGER PRIMARY KEY CHECK(id = 1),
            next_number INTEGER NOT NULL
        );
        INSERT OR IGNORE INTO chat_title_sequence (id, next_number) VALUES (1, 1);
        CREATE TABLE IF NOT EXISTS installed_agents (
            id TEXT PRIMARY KEY,
            source TEXT NOT NULL CHECK(source IN ('batey_managed', 'registry')),
            data TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_installed_agents_source ON installed_agents(source);
        CREATE TABLE IF NOT EXISTS chat_mcp_servers (
            id TEXT PRIMARY KEY,
            chat_id TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
            position INTEGER NOT NULL CHECK(position >= 0),
            data TEXT NOT NULL,
            UNIQUE(chat_id, position)
        );
        CREATE INDEX IF NOT EXISTS idx_chat_mcp_servers_chat_position ON chat_mcp_servers(chat_id, position);
        CREATE TABLE IF NOT EXISTS chat_additional_roots (
            chat_id TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
            position INTEGER NOT NULL CHECK(position >= 0),
            project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE RESTRICT,
            canonical_path TEXT NOT NULL,
            PRIMARY KEY(chat_id, project_id),
            UNIQUE(chat_id, position)
        );
        CREATE INDEX IF NOT EXISTS idx_chat_additional_roots_project ON chat_additional_roots(project_id);
        CREATE TABLE IF NOT EXISTS events (
            seq INTEGER PRIMARY KEY,
            session_id TEXT NOT NULL DEFAULT '',
            data TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_events_session_id ON events(session_id);",
        precondition: None,
    },
    Migration {
        version: 2,
        name: "project_envrc_grants",
        sql: "
        CREATE TABLE IF NOT EXISTS project_envrc_grants (
            project_id TEXT PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
            relative_path TEXT NOT NULL,
            content_hash TEXT NOT NULL,
            created_at TEXT NOT NULL
        );",
        precondition: None,
    },
    Migration {
        version: 3,
        name: "agent_env_overrides",
        sql: "
        CREATE TABLE IF NOT EXISTS agent_env_overrides (
            agent_id TEXT NOT NULL,
            name TEXT NOT NULL,
            value TEXT NOT NULL,
            PRIMARY KEY (agent_id, name)
        );
        CREATE INDEX IF NOT EXISTS idx_agent_env_overrides_agent ON agent_env_overrides(agent_id);",
        precondition: None,
    },
    Migration {
        version: 4,
        name: "agent_auth_cache",
        sql: "
        CREATE TABLE IF NOT EXISTS agent_auth_cache (
            agent_id TEXT PRIMARY KEY,
            data TEXT NOT NULL,
            checked_at TEXT NOT NULL,
            stale INTEGER NOT NULL DEFAULT 0
        );",
        precondition: None,
    },
];

pub fn latest_version() -> i64 {
    MIGRATIONS.last().map_or(0, |m| m.version)
}

pub(crate) fn ensure_batey_identity(conn: &mut Connection, path: &Path) -> Result<()> {
    let app_id: i32 = conn.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    if app_id == BATEY_APPLICATION_ID {
        check_version(conn)?;
        return Ok(());
    }

    if app_id == 0 {
        let user_ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let table_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )?;
        if user_ver == 0 && table_count == 0 {
            // Fresh empty database: adopt as Batey database by stamping the application ID.
            conn.pragma_update(None, "application_id", BATEY_APPLICATION_ID)?;
            return Ok(());
        }
        anyhow::bail!(
            "Database at {} is a legacy or unrecognized database without the Batey application ID. \
             Batey does not migrate, reuse, or modify legacy databases. \
             Please configure a new database path.",
            path.display()
        );
    }

    anyhow::bail!(
        "Database at {} has application_id 0x{:08X} (expected Batey application_id 0x{:08X}). \
         Batey refuses to open databases belonging to other applications.",
        path.display(),
        app_id,
        BATEY_APPLICATION_ID
    );
}

pub(crate) fn check_version(conn: &Connection) -> Result<()> {
    let latest = latest_version();
    let current = user_version(conn)?;
    anyhow::ensure!(
        current <= latest,
        "Database schema version {current} comes from a newer Batey \
         (this build understands version {latest}). Upgrade Batey or restore a backup."
    );
    Ok(())
}

pub(crate) fn migrate(db: &mut Connection) -> Result<()> {
    apply(db, MIGRATIONS)
}

fn apply(db: &mut Connection, migrations: &[Migration]) -> Result<()> {
    debug_assert!(
        migrations.windows(2).all(|w| w[0].version < w[1].version),
        "migrations must be ordered by ascending version"
    );
    check_version(db)?;
    let current = user_version(db)?;

    for m in migrations.iter().filter(|m| m.version > current) {
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if user_version(&tx)? >= m.version {
            tx.rollback()?;
            continue;
        }
        if needs_body(&tx, m)? {
            tx.execute_batch(m.sql)
                .with_context(|| format!("Schema migration {} ({}) failed", m.version, m.name))?;
        } else {
            tracing::info!(
                version = m.version,
                name = m.name,
                "schema migration already applied; advancing version only"
            );
        }
        tx.pragma_update(None, "user_version", m.version)?;
        tx.commit()?;
        tracing::info!(
            version = m.version,
            name = m.name,
            "applied schema migration"
        );
    }
    Ok(())
}

/// Whether this migration's statements still have work to do.
fn needs_body(conn: &Connection, m: &Migration) -> Result<bool> {
    match m.precondition {
        None => Ok(true),
        Some(sql) => Ok(conn.query_row(sql, [], |r| r.get::<_, i64>(0))? != 0),
    }
}

fn user_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn table_names(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    #[test]
    fn migrations_are_strictly_ordered() {
        assert!(MIGRATIONS.windows(2).all(|w| w[0].version < w[1].version));
        assert_eq!(MIGRATIONS.first().map(|m| m.version), Some(1));
    }

    #[test]
    fn fresh_database_reaches_latest_version_and_application_id() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("batey.sqlite3");
        let store = Store::open(&path).unwrap();
        drop(store);

        let conn = Connection::open(&path).unwrap();
        let app_id: i32 = conn
            .query_row("PRAGMA application_id", [], |r| r.get(0))
            .unwrap();
        assert_eq!(app_id, BATEY_APPLICATION_ID);
        assert_eq!(user_version(&conn).unwrap(), latest_version());
        let tables = table_names(&conn);
        for expected in [
            "agent_auth_cache",
            "agent_env_overrides",
            "chats",
            "chat_additional_roots",
            "chat_mcp_servers",
            "chat_title_sequence",
            "chat_workspaces",
            "events",
            "installed_agents",
            "project_envrc_grants",
            "projects",
        ] {
            assert!(tables.contains(&expected.to_string()), "missing {expected}");
        }
    }

    #[test]
    fn refuse_legacy_batey_database_unmodified() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("batey.sqlite3");

        // Create a pre-identity database (no application_id, user_version > 0 and tables).
        {
            let legacy = Connection::open(&path).unwrap();
            legacy
                .execute_batch(
                    "PRAGMA journal_mode=DELETE;
                CREATE TABLE projects (id TEXT PRIMARY KEY, data TEXT NOT NULL);
                CREATE TABLE chats (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, data TEXT NOT NULL);
                CREATE TABLE events (seq INTEGER PRIMARY KEY, data TEXT NOT NULL);
                INSERT INTO projects (id, data) VALUES ('p1', '{\"id\":\"p1\"}');
                INSERT INTO chats (id, project_id, data) VALUES ('c1', 'p1', '{\"id\":\"c1\"}');
                PRAGMA user_version=1;",
                )
                .unwrap();
        }

        let before_bytes = std::fs::read(&path).unwrap();
        let err = Store::open(&path)
            .err()
            .expect("legacy database must be refused");
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("legacy or unrecognized database")
                || err_msg.contains("Batey does not migrate"),
            "unexpected error message: {err_msg}"
        );

        let after_bytes = std::fs::read(&path).unwrap();
        assert_eq!(before_bytes, after_bytes, "legacy database was mutated!");

        // Also verify with rusqlite that the schema/data are unchanged
        let conn = Connection::open(&path).unwrap();
        let app_id: i32 = conn
            .query_row("PRAGMA application_id", [], |r| r.get(0))
            .unwrap();
        assert_eq!(app_id, 0);
        assert_eq!(user_version(&conn).unwrap(), 1);
        let tables = table_names(&conn);
        assert_eq!(tables, vec!["chats", "events", "projects"]);
    }

    #[test]
    fn refuse_foreign_sqlite_database_unmodified() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("foreign.sqlite3");

        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "application_id", 0x1234_5678_i32)
                .unwrap();
            conn.execute("CREATE TABLE foo (id INTEGER);", []).unwrap();
        }

        let before_bytes = std::fs::read(&path).unwrap();
        let err = Store::open(&path)
            .err()
            .expect("foreign database must be refused");
        assert!(err.to_string().contains("application_id"), "{err}");

        let after_bytes = std::fs::read(&path).unwrap();
        assert_eq!(before_bytes, after_bytes, "foreign database was mutated!");
    }

    #[test]
    fn chat_workspaces_schema_has_expected_keys_and_indexes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("batey.sqlite3");
        Store::open(&path).unwrap();

        let conn = Connection::open(&path).unwrap();
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='chat_workspaces'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        for expected in [
            "chat_id TEXT PRIMARY KEY",
            "REFERENCES chats(id) ON DELETE CASCADE",
            "REFERENCES projects(id) ON DELETE CASCADE",
            "CHECK(mode IN ('managed_worktree', 'project_checkout'))",
        ] {
            assert!(sql.contains(expected), "missing {expected} in {sql}");
        }
        for index in [
            "idx_chat_workspaces_project_id",
            "idx_chat_workspaces_managed_path",
        ] {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
                    [index],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "index {index} is missing");
        }
        let partial: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='index' \
                 AND name='idx_chat_workspaces_managed_path'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            partial.contains("WHERE mode='managed_worktree'"),
            "managed-path uniqueness must be partial, got {partial}"
        );
    }

    #[test]
    fn project_envrc_grants_schema_has_expected_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("batey.sqlite3");
        Store::open(&path).unwrap();

        let conn = Connection::open(&path).unwrap();
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='project_envrc_grants'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        for expected in [
            "project_id TEXT PRIMARY KEY",
            "REFERENCES projects(id) ON DELETE CASCADE",
            "relative_path TEXT NOT NULL",
            "content_hash TEXT NOT NULL",
        ] {
            assert!(sql.contains(expected), "missing {expected} in {sql}");
        }
    }

    #[test]
    fn failed_migration_rolls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("batey.sqlite3");
        let mut conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "application_id", BATEY_APPLICATION_ID)
            .unwrap();

        let table = &[
            Migration {
                version: 1,
                name: "good",
                sql: "CREATE TABLE good (id TEXT PRIMARY KEY);",
                precondition: None,
            },
            Migration {
                version: 2,
                name: "broken",
                sql: "CREATE TABLE half (id TEXT); THIS IS NOT SQL;",
                precondition: None,
            },
        ];

        let err = apply(&mut conn, table).unwrap_err();
        assert!(err.to_string().contains("Schema migration 2"));
        assert_eq!(user_version(&conn).unwrap(), 1);
        let tables = table_names(&conn);
        assert!(tables.contains(&"good".to_string()));
        assert!(
            !tables.contains(&"half".to_string()),
            "partial DDL survived"
        );
    }

    #[test]
    fn newer_schema_version_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("batey.sqlite3");
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "application_id", BATEY_APPLICATION_ID)
            .unwrap();
        conn.pragma_update(None, "user_version", 999_i64).unwrap();
        conn.pragma_update(None, "journal_mode", "DELETE").unwrap();
        drop(conn);

        let err = Store::open(&path).err().unwrap();
        assert!(err.to_string().contains("newer Batey"), "{err}");

        let conn = Connection::open(&path).unwrap();
        assert_eq!(user_version(&conn).unwrap(), 999);
        assert!(table_names(&conn).is_empty(), "database was modified");
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            mode.to_uppercase(),
            "DELETE",
            "journal_mode was mutated before version check"
        );
    }
}
