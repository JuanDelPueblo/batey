//! Durable per-agent authentication discovery cache.
//!
//! One row per installed agent, holding only safe presentation data: the
//! advertised methods (id, name, description, type, whether this build
//! supports it), the logout capability, and the last observed authentication
//! state. It never holds terminal command arguments, environment values,
//! OAuth URLs, device codes, tokens, callback URLs, credentials, raw ACP
//! responses, or stderr.
//!
//! Discovery (the advertised methods/logout capability) and observed
//! authentication evidence age independently. `checked_at`/`stale` track
//! only discovery: they move when a live `initialize` probe runs, and a
//! mutation that can change initialization or authentication methods marks
//! them stale. `observed_checked_at` tracks only the observed-state
//! evidence: it moves when Batey sees a real authentication event
//! (`authenticate`/`logout`/`auth_required`/session success), never as a
//! side effect of a bare discovery probe. Neither can silently refresh the
//! other: a probe that only re-confirms the method list must never make
//! old sign-in evidence look freshly verified, and new sign-in evidence
//! must never erase a discovery mutation's stale marker.
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
    /// When `observed_state` was last set by real evidence, as RFC 3339.
    /// Independent of the entry's `checked_at`/`stale`, which describe only
    /// the discovered methods.
    #[serde(default)]
    pub observed_checked_at: Option<String>,
}

impl Default for AuthCacheData {
    fn default() -> Self {
        Self {
            methods: Vec::new(),
            logout_supported: false,
            observed_state: ObservedAuthState::Unknown,
            observed_checked_at: None,
        }
    }
}

/// One durable cache row, plus its discovery freshness bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthCacheEntry {
    pub agent_id: String,
    pub data: AuthCacheData,
    /// When the advertised methods/logout capability were last confirmed by
    /// a live probe, as RFC 3339. Empty when never probed. Never a
    /// substitute for `stale`, and never touched by observed-evidence-only
    /// writes.
    pub checked_at: String,
    /// Set by a mutation that can affect initialization or authentication
    /// methods. A stale row is still shown, as historical evidence, and
    /// invites a fresh check; it is never trusted as current truth. Only a
    /// fresh discovery probe clears it; recording new observed evidence
    /// never does.
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

/// Replaces the row with fresh discovery data (methods, logout capability)
/// and clears `stale`. Only a completed live probe calls this. Never used to
/// store credentials or execution details: only the safe presentation
/// fields.
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

/// Records new observed-authentication evidence: `observed_state` and its
/// own `observed_checked_at`. Deliberately never touches the discovery
/// `checked_at`/`stale` columns, so evidence from a chat session or an
/// authenticate/logout call can never silently clear a mutation's stale
/// marker on the (still unverified) method list. Creates a minimal row when
/// none exists yet, so evidence is never lost just because this agent was
/// never explicitly probed.
pub(crate) fn save_observed(
    conn: &Connection,
    agent_id: &str,
    observed_state: ObservedAuthState,
    observed_checked_at: &str,
) -> StoreResult<()> {
    let mut data = get(conn, agent_id)?
        .map(|entry| entry.data)
        .unwrap_or_default();
    data.observed_state = observed_state;
    data.observed_checked_at = Some(observed_checked_at.to_owned());
    conn.execute(
        "INSERT INTO agent_auth_cache (agent_id, data, checked_at, stale) \
         VALUES (?1, ?2, '', 0) \
         ON CONFLICT (agent_id) DO UPDATE SET data=excluded.data",
        params![agent_id, serde_json::to_string(&data)?],
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
            observed_checked_at: Some("2026-01-01T00:00:00Z".into()),
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

    /// Recording new observed evidence must never touch the discovery
    /// `checked_at`/`stale` columns: a mutation's stale marker on the method
    /// list survives an unrelated sign-in observation.
    #[test]
    fn observed_writes_never_touch_discovery_freshness() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("hub.db")).unwrap();
        store
            .save_agent_auth_cache(&AuthCacheEntry::fresh(
                "codex",
                data(),
                "2026-01-01T00:00:00Z".into(),
            ))
            .unwrap();
        store.mark_agent_auth_cache_stale("codex").unwrap();

        store
            .save_agent_auth_observed(
                "codex",
                ObservedAuthState::AuthenticationRequired,
                "2026-01-02T00:00:00Z",
            )
            .unwrap();

        let read = store.agent_auth_cache("codex").unwrap().unwrap();
        // Discovery is still the mutation-invalidated pre-existing row.
        assert!(read.stale, "an observed-only write cleared discovery stale");
        assert_eq!(read.checked_at, "2026-01-01T00:00:00Z");
        assert_eq!(read.data.methods.len(), 1);
        // Observed evidence updated independently.
        assert_eq!(
            read.data.observed_state,
            ObservedAuthState::AuthenticationRequired
        );
        assert_eq!(
            read.data.observed_checked_at.as_deref(),
            Some("2026-01-02T00:00:00Z")
        );
    }

    /// An observed write on an agent that was never discovery-probed
    /// creates a minimal row without inventing discovery data.
    #[test]
    fn observed_write_without_a_prior_row_creates_a_minimal_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("hub.db")).unwrap();
        store
            .save_agent_auth_observed(
                "never-probed",
                ObservedAuthState::Authenticated,
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
        let read = store.agent_auth_cache("never-probed").unwrap().unwrap();
        assert_eq!(read.checked_at, "");
        assert!(!read.stale);
        assert!(read.data.methods.is_empty());
        assert_eq!(read.data.observed_state, ObservedAuthState::Authenticated);
        assert_eq!(
            read.data.observed_checked_at.as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
    }
}
