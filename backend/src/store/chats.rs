//! Chat rows. `project_id` has its own column because the foreign key and the
//! per-project listing need it; the rest stays a JSON document in `data`.
use super::validation::validate_name;
use super::{StoreError, StoreResult};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chat {
    pub id: String,
    pub project_id: String,
    pub agent: String,
    pub title: String,
    pub acp_session_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub archived: bool,
    pub config_values: Value,
    #[serde(default)]
    pub title_overridden: bool,
}

pub(crate) fn new(
    project_id: String,
    agent: String,
    title: Option<String>,
    default_number: Option<i64>,
) -> StoreResult<Chat> {
    let (final_title, title_overridden) = match title {
        Some(t) if !t.trim().is_empty() => {
            validate_name(&t)?;
            let trimmed = t.trim().to_string();
            (trimmed, true)
        }
        _ => (
            format!(
                "New chat {}",
                default_number.ok_or_else(|| {
                    StoreError::Internal(anyhow::anyhow!(
                        "Missing default chat title sequence number"
                    ))
                })?
            ),
            false,
        ),
    };
    let now = chrono::Utc::now().to_rfc3339();
    Ok(Chat {
        id: uuid::Uuid::new_v4().to_string(),
        project_id,
        agent,
        title: final_title,
        acp_session_id: None,
        created_at: now.clone(),
        updated_at: now,
        archived: false,
        config_values: serde_json::json!({}),
        title_overridden,
    })
}

/// Reserves a default title number in SQLite. The caller must hold an
/// immediate transaction so concurrent Hub processes cannot receive the same
/// number. Numbers are intentionally consumed even if later workspace setup
/// fails; that keeps normal forward operation monotonic and never reuses a
/// title after deletion.
pub(crate) fn reserve_default_number(conn: &Connection) -> StoreResult<i64> {
    conn.execute(
        "INSERT OR IGNORE INTO chat_title_sequence (id, next_number) VALUES (1, 1)",
        [],
    )?;
    let number: i64 = conn.query_row(
        "SELECT next_number FROM chat_title_sequence WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    if number < 1 {
        return Err(StoreError::Internal(anyhow::anyhow!(
            "Invalid default chat title sequence number"
        )));
    }
    let next = number
        .checked_add(1)
        .ok_or_else(|| StoreError::Internal(anyhow::anyhow!("Chat title sequence exhausted")))?;
    conn.execute(
        "UPDATE chat_title_sequence SET next_number=?1 WHERE id=1",
        [next],
    )?;
    Ok(number)
}

pub(crate) fn list(conn: &Connection) -> StoreResult<Vec<Chat>> {
    let mut stmt = conn.prepare(
        "SELECT data FROM chats
         ORDER BY json_extract(data, '$.updated_at') DESC, id DESC",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut chats = Vec::new();
    for r in rows {
        let data = r?;
        chats.push(serde_json::from_str(&data)?);
    }
    Ok(chats)
}

pub(crate) fn get(conn: &Connection, id: &str) -> StoreResult<Chat> {
    let data: Option<String> = conn
        .query_row("SELECT data FROM chats WHERE id=?1", [id], |r| r.get(0))
        .optional()?;
    match data {
        Some(d) => Ok(serde_json::from_str(&d)?),
        None => Err(StoreError::NotFound("Chat not found".into())),
    }
}

pub(crate) fn insert(conn: &Connection, c: &Chat) -> StoreResult<()> {
    conn.execute(
        "INSERT INTO chats (id, project_id, data) VALUES (?1, ?2, ?3)",
        params![c.id, c.project_id, serde_json::to_string(c)?],
    )?;
    Ok(())
}

/// Read/modify/write. The caller holds the connection lock for the whole call,
/// so a config notification cannot overwrite a concurrent rename.
pub(crate) fn update(
    conn: &Connection,
    id: &str,
    edit: impl FnOnce(&mut Chat),
) -> StoreResult<Chat> {
    let data: Option<String> = conn
        .query_row("SELECT data FROM chats WHERE id=?1", [id], |r| r.get(0))
        .optional()?;
    let data = match data {
        Some(d) => d,
        None => return Err(StoreError::NotFound("Chat not found".into())),
    };
    let mut c: Chat = serde_json::from_str(&data)?;
    edit(&mut c);
    c.updated_at = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE chats SET data=?2 WHERE id=?1",
        params![id, serde_json::to_string(&c)?],
    )?;
    Ok(c)
}

/// Records conversation activity without changing any other chat metadata.
/// The caller supplies the event timestamp so the durable chat row and the
/// durable user-message event describe the same activity instant.
pub(crate) fn touch(conn: &Connection, id: &str, updated_at: &str) -> StoreResult<Chat> {
    let data: Option<String> = conn
        .query_row("SELECT data FROM chats WHERE id=?1", [id], |r| r.get(0))
        .optional()?;
    let data = match data {
        Some(d) => d,
        None => return Err(StoreError::NotFound("Chat not found".into())),
    };
    let mut c: Chat = serde_json::from_str(&data)?;
    c.updated_at = updated_at.to_string();
    conn.execute(
        "UPDATE chats SET data=?2 WHERE id=?1",
        params![id, serde_json::to_string(&c)?],
    )?;
    Ok(c)
}

pub(crate) fn delete(conn: &Connection, id: &str) -> StoreResult<()> {
    let affected = conn.execute("DELETE FROM chats WHERE id=?1", [id])?;
    if affected == 0 {
        return Err(StoreError::NotFound("Chat not found".into()));
    }
    Ok(())
}
