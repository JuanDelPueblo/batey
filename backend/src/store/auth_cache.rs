//! Durable per-agent authentication discovery cache.
//!
//! One row per installed agent, holding only safe presentation data: the
//! advertised methods (id, name, description, type, whether this build
//! supports it), the logout capability, the last observed authentication
//! state, and when that data was last confirmed by a live probe. It never
//! holds terminal command arguments, environment values, OAuth URLs, device
//! codes, tokens, callback URLs, credentials, raw ACP responses, or stderr.
//!
//! A mutation that can change an agent's initialization or authentication
//! methods marks the row `stale` instead of deleting it, so a client still
//! sees the last known methods as historical evidence while it invites a
//! fresh check. Only agent removal deletes the row outright.
use super::StoreResult;
use crate::acp::auth::ObservedAuthState;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// One advertised method, as safe to keep as it is to show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedAuthMethod {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// The advertised method type, including a type this build cannot run.
    pub method_type: String,
    /// Whether this build can run the method.
    pub supported: bool,
}

/// The durable presentation data cached for one agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthCacheData {
    pub methods: Vec<CachedAuthMethod>,
    pub logout_supported: bool,
    pub observed_state: ObservedAuthState,
}

impl Default for AuthCacheData {
    fn default() -> Self {
        Self {
            methods: Vec::new(),
            logout_supported: false,
            observed_state: ObservedAuthState::Unknown,
        }
    }
}

/// One durable cache row, plus its freshness bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthCacheEntry {
    pub agent_id: String,
    pub data: AuthCacheData,
    /// When this data was last confirmed by a live probe or by explicit
    /// evidence, as RFC 3339. Never a substitute for `stale`.
    pub checked_at: String,
    /// Set by a mutation that can affect initialization or authentication
    /// methods. A stale row is still shown, as historical evidence, and
    /// invites a fresh check; it is never trusted as current truth.
    pub stale: bool,
}

impl AuthCacheEntry {
    pub fn fresh(agent_id: &str, data: AuthCacheData, checked_at: String) -> Self {
        Self {
            agent_id: agent_id.to_owned(),
            data,
            checked_at,
            stale: false,
        }
    }
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuthCacheEntry> {
    let agent_id: String = row.get(0)?;
    let data: String = row.get(1)?;
    let checked_at: String = row.get(2)?;
    let stale: i64 = row.get(3)?;
    let data: AuthCacheData = serde_json::from_str(&data).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(AuthCacheEntry {
        agent_id,
        data,
        checked_at,
        stale: stale != 0,
    })
}

pub(crate) fn get(conn: &Connection, agent_id: &str) -> StoreResult<Option<AuthCacheEntry>> {
    Ok(conn
        .query_row(
            "SELECT agent_id, data, checked_at, stale FROM agent_auth_cache WHERE agent_id=?1",
            [agent_id],
            row_to_entry,
        )
        .optional()?)
}

/// Replaces the row with fresh data and clears `stale`. Never used to store
/// credentials or execution details: only the safe presentation fields.
pub(crate) fn save(conn: &Connection, entry: &AuthCacheEntry) -> StoreResult<()> {
    conn.execute(
        "INSERT INTO agent_auth_cache (agent_id, data, checked_at, stale) \
         VALUES (?1, ?2, ?3, 0) \
         ON CONFLICT (agent_id) DO UPDATE SET data=excluded.data, checked_at=excluded.checked_at, stale=0",
        params![
            entry.agent_id,
            serde_json::to_string(&entry.data)?,
            entry.checked_at,
        ],
    )?;
    Ok(())
}

/// Marks one agent's cache stale, keeping its last known data as historical
/// evidence. A no-op when no row exists yet: a mutation on an agent that was
/// never probed has nothing to invalidate.
pub(crate) fn mark_stale(conn: &Connection, agent_id: &str) -> StoreResult<()> {
    conn.execute(
        "UPDATE agent_auth_cache SET stale=1 WHERE agent_id=?1",
        [agent_id],
    )?;
    Ok(())
}

/// Deletes the row outright. Only a full agent removal calls this; a retired
/// agent keeps its row as historical evidence, marked stale instead.
pub(crate) fn delete(conn: &Connection, agent_id: &str) -> StoreResult<()> {
    conn.execute("DELETE FROM agent_auth_cache WHERE agent_id=?1", [agent_id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn data() -> AuthCacheData {
        AuthCacheData {
            methods: vec![CachedAuthMethod {
                id: "api-key".into(),
                name: "API key".into(),
                description: Some("Paste a key".into()),
                method_type: "agent".into(),
                supported: true,
            }],
            logout_supported: true,
            observed_state: ObservedAuthState::Authenticated,
        }
    }

    #[test]
    fn round_trips_through_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hub.db");
        let store = Store::open(&path).unwrap();
        let entry = AuthCacheEntry::fresh("codex", data(), "2026-01-01T00:00:00Z".into());
        store.save_agent_auth_cache(&entry).unwrap();
        drop(store);

        let store = Store::open(&path).unwrap();
        let read = store.agent_auth_cache("codex").unwrap().unwrap();
        assert_eq!(read.data, data());
        assert_eq!(read.checked_at, "2026-01-01T00:00:00Z");
        assert!(!read.stale);
        assert!(store.agent_auth_cache("claude").unwrap().is_none());
    }

    #[test]
    fn saving_again_clears_a_stale_row() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("hub.db")).unwrap();
        let entry = AuthCacheEntry::fresh("codex", data(), "2026-01-01T00:00:00Z".into());
        store.save_agent_auth_cache(&entry).unwrap();
        store.mark_agent_auth_cache_stale("codex").unwrap();
        assert!(store.agent_auth_cache("codex").unwrap().unwrap().stale);

        let mut updated = entry.clone();
        updated.checked_at = "2026-01-02T00:00:00Z".into();
        store.save_agent_auth_cache(&updated).unwrap();
        let read = store.agent_auth_cache("codex").unwrap().unwrap();
        assert!(!read.stale);
        assert_eq!(read.checked_at, "2026-01-02T00:00:00Z");
    }

    #[test]
    fn marking_stale_is_a_no_op_without_a_row() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("hub.db")).unwrap();
        store.mark_agent_auth_cache_stale("never-probed").unwrap();
        assert!(store.agent_auth_cache("never-probed").unwrap().is_none());
    }

    #[test]
    fn delete_removes_only_the_named_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("hub.db")).unwrap();
        store
            .save_agent_auth_cache(&AuthCacheEntry::fresh(
                "codex",
                data(),
                "2026-01-01T00:00:00Z".into(),
            ))
            .unwrap();
        store
            .save_agent_auth_cache(&AuthCacheEntry::fresh(
                "claude",
                data(),
                "2026-01-01T00:00:00Z".into(),
            ))
            .unwrap();
        store.delete_agent_auth_cache("codex").unwrap();
        assert!(store.agent_auth_cache("codex").unwrap().is_none());
        assert!(store.agent_auth_cache("claude").unwrap().is_some());
    }
}
