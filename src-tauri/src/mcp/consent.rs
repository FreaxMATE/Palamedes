//! Per-client consent state for the MCP server.
//!
//! Each MCP client (identified by `clientInfo.name` from the MCP
//! `initialize` handshake) gets one row in `mcp_clients`. Consent splits
//! into two buckets: read (list/get/search) and write (propose/correct).
//! Each is NULL until the user decides, then 0 (denied) or 1 (granted).
//!
//! Day 2 ships the persistence layer. The state-machine half — holding
//! a pending tool call until the user resolves consent — lives in
//! `server.rs` (Day 3) since it needs the async runtime + Notify.

use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One row of `mcp_clients`. `consent_read` and `consent_write` are
/// `None` while pending, `Some(false)` denied, `Some(true)` granted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpClient {
    pub id: String,
    pub name: String,
    pub version: Option<String>,
    pub client_info: Option<String>,
    pub consent_read: Option<bool>,
    pub consent_write: Option<bool>,
    pub first_seen_at: String,
    pub last_seen_at: Option<String>,
    pub revoked_at: Option<String>,
}

impl McpClient {
    /// True iff the client is allowed to call read tools right now —
    /// must be explicitly granted AND not revoked.
    pub fn can_read(&self) -> bool {
        self.revoked_at.is_none() && self.consent_read == Some(true)
    }

    /// True iff the client is allowed to call write tools right now.
    pub fn can_write(&self) -> bool {
        self.revoked_at.is_none() && self.consent_write == Some(true)
    }

    /// Pending consent — at least one bucket is unresolved and the
    /// client isn't revoked. The server holds tool calls until this
    /// goes false.
    pub fn is_pending(&self) -> bool {
        self.revoked_at.is_none()
            && (self.consent_read.is_none() || self.consent_write.is_none())
    }
}

/// Either fetch an existing client row by name, or insert a new one
/// with both consent buckets unresolved. Returns the row + whether it
/// was newly created (the server emits a `mcp:client-pending` Tauri
/// event in that case so the GUI can pop the consent modal).
pub fn upsert_client(
    conn: &Connection,
    name: &str,
    version: Option<&str>,
    client_info_json: Option<&str>,
) -> Result<(McpClient, bool)> {
    if let Some(existing) = get_client_by_name(conn, name)? {
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE mcp_clients SET last_seen_at = ?1, version = COALESCE(?2, version),
                                    client_info = COALESCE(?3, client_info)
             WHERE id = ?4",
            params![now, version, client_info_json, existing.id],
        )?;
        // Re-fetch so the returned row has the updated last_seen_at.
        let refreshed = get_client_by_name(conn, name)?
            .expect("just updated by id, must still exist");
        return Ok((refreshed, false));
    }
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO mcp_clients
           (id, name, version, client_info, consent_read, consent_write,
            first_seen_at, last_seen_at, revoked_at)
         VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5, ?5, NULL)",
        params![id, name, version, client_info_json, now],
    )?;
    let fresh = get_client_by_name(conn, name)?
        .expect("just inserted, must exist");
    Ok((fresh, true))
}

pub fn get_client_by_name(conn: &Connection, name: &str) -> Result<Option<McpClient>> {
    conn.query_row(
        "SELECT id, name, version, client_info, consent_read, consent_write,
                first_seen_at, last_seen_at, revoked_at
         FROM mcp_clients WHERE name = ?1",
        params![name],
        row_to_client,
    )
    .optional()
    .map_err(Into::into)
}

pub fn get_client_by_id(conn: &Connection, id: &str) -> Result<Option<McpClient>> {
    conn.query_row(
        "SELECT id, name, version, client_info, consent_read, consent_write,
                first_seen_at, last_seen_at, revoked_at
         FROM mcp_clients WHERE id = ?1",
        params![id],
        row_to_client,
    )
    .optional()
    .map_err(Into::into)
}

pub fn list_clients(conn: &Connection) -> Result<Vec<McpClient>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, version, client_info, consent_read, consent_write,
                first_seen_at, last_seen_at, revoked_at
         FROM mcp_clients ORDER BY first_seen_at DESC",
    )?;
    let rows = stmt.query_map([], row_to_client)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Record the user's consent decision. Either bucket can be granted or
/// denied independently. Clears `revoked_at` since a fresh decision
/// supersedes a prior revocation.
pub fn set_consent(
    conn: &Connection,
    client_id: &str,
    consent_read: bool,
    consent_write: bool,
) -> Result<()> {
    let n = conn.execute(
        "UPDATE mcp_clients
         SET consent_read = ?1, consent_write = ?2, revoked_at = NULL
         WHERE id = ?3",
        params![consent_read as i32, consent_write as i32, client_id],
    )?;
    if n == 0 {
        anyhow::bail!("no mcp_clients row with id {}", client_id);
    }
    Ok(())
}

/// Soft-revoke: future tool calls fail until the user re-consents.
/// History (the row + audit trail) is preserved.
pub fn revoke_client(conn: &Connection, client_id: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let n = conn.execute(
        "UPDATE mcp_clients SET revoked_at = ?1 WHERE id = ?2",
        params![now, client_id],
    )?;
    if n == 0 {
        anyhow::bail!("no mcp_clients row with id {}", client_id);
    }
    Ok(())
}

fn row_to_client(r: &rusqlite::Row<'_>) -> rusqlite::Result<McpClient> {
    Ok(McpClient {
        id: r.get(0)?,
        name: r.get(1)?,
        version: r.get(2)?,
        client_info: r.get(3)?,
        consent_read: r.get::<_, Option<i64>>(4)?.map(|n| n != 0),
        consent_write: r.get::<_, Option<i64>>(5)?.map(|n| n != 0),
        first_seen_at: r.get(6)?,
        last_seen_at: r.get(7)?,
        revoked_at: r.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA: &str = include_str!("../../schema.sql");

    fn fresh_conn() -> Connection {
        crate::embeddings::register_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn
    }

    #[test]
    fn upsert_inserts_new_client_with_pending_consent() {
        let conn = fresh_conn();
        let (client, created) =
            upsert_client(&conn, "claude-desktop", Some("0.13.2"), Some("{}")).unwrap();
        assert!(created);
        assert_eq!(client.name, "claude-desktop");
        assert_eq!(client.version.as_deref(), Some("0.13.2"));
        assert!(client.is_pending());
        assert!(!client.can_read());
        assert!(!client.can_write());
    }

    #[test]
    fn upsert_returns_existing_client_and_updates_last_seen() {
        let conn = fresh_conn();
        let (first, c1) = upsert_client(&conn, "cursor", None, None).unwrap();
        assert!(c1);
        // Microsleep so the timestamp moves; chrono::Utc::now is ms-resolution.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let (second, c2) =
            upsert_client(&conn, "cursor", Some("0.45.0"), Some(r#"{"k":"v"}"#)).unwrap();
        assert!(!c2);
        assert_eq!(first.id, second.id);
        assert_eq!(second.version.as_deref(), Some("0.45.0"));
        assert!(second.last_seen_at > first.last_seen_at);
    }

    #[test]
    fn set_consent_unlocks_tool_categories_independently() {
        let conn = fresh_conn();
        let (client, _) = upsert_client(&conn, "witsy", None, None).unwrap();
        set_consent(&conn, &client.id, true, false).unwrap();
        let read_only = get_client_by_id(&conn, &client.id).unwrap().unwrap();
        assert!(read_only.can_read());
        assert!(!read_only.can_write());
        assert!(!read_only.is_pending());

        set_consent(&conn, &client.id, true, true).unwrap();
        let full = get_client_by_id(&conn, &client.id).unwrap().unwrap();
        assert!(full.can_read());
        assert!(full.can_write());
    }

    #[test]
    fn revoke_blocks_all_tools_but_preserves_consent_fields() {
        let conn = fresh_conn();
        let (client, _) = upsert_client(&conn, "open-webui", None, None).unwrap();
        set_consent(&conn, &client.id, true, true).unwrap();
        revoke_client(&conn, &client.id).unwrap();
        let revoked = get_client_by_id(&conn, &client.id).unwrap().unwrap();
        assert!(!revoked.can_read());
        assert!(!revoked.can_write());
        assert!(!revoked.is_pending());
        // Setting consent again clears the revocation.
        set_consent(&conn, &client.id, true, false).unwrap();
        let restored = get_client_by_id(&conn, &client.id).unwrap().unwrap();
        assert!(restored.can_read());
        assert!(!restored.can_write());
        assert!(restored.revoked_at.is_none());
    }

    #[test]
    fn list_clients_orders_newest_first() {
        let conn = fresh_conn();
        upsert_client(&conn, "a", None, None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        upsert_client(&conn, "b", None, None).unwrap();
        let all = list_clients(&conn).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].name, "b");
        assert_eq!(all[1].name, "a");
    }
}
