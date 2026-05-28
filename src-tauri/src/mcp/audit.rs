//! MCP read audit log + per-client rate limit.
//!
//! Every successful invocation of a read tool (list_beliefs, get_belief,
//! search_beliefs) is recorded in `mcp_read_log`. This serves two purposes:
//!
//! 1. **Rate limit** — a client that has been granted `consent_read` could
//!    otherwise enumerate the entire belief ledger via repeated calls. We
//!    cap reads per client at `mcp_read_limit_per_day` (default
//!    [`DEFAULT_READ_LIMIT_PER_DAY`]) over a sliding 24h window and refuse
//!    new reads once exceeded. Week 4 closes the remaining gap with
//!    per-category consent; this is the first-line defense in week 1.
//!
//! 2. **Forensic surface** — the Audit panel will (week 4) show the user
//!    exactly which beliefs each MCP client has read, when, and via which
//!    tool. We log the input JSON (capped at 1 KB) and up to the first
//!    [`MAX_LOGGED_IDS`] returned belief ids so the trail stays compact.
//!
//! Logs are never auto-pruned at this stage. A future task may add a
//! retention policy (e.g. drop entries older than 90 days) once we have
//! signal on how heavy the table grows under real load.

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Default sliding-window read budget per MCP client. Configurable via the
/// `mcp_read_limit_per_day` row in `settings` once the UI lands.
pub const DEFAULT_READ_LIMIT_PER_DAY: i64 = 1000;

/// Cap on stored returned-id arrays. Forensics need a sample, not the
/// full result set — a list_beliefs returning 500 rows would otherwise
/// dump 500 UUIDs into a single log row. We keep the first N.
pub const MAX_LOGGED_IDS: usize = 50;

/// Cap on stored input JSON length (bytes). A search_beliefs query that
/// pastes a 100 KB document doesn't belong in the audit log.
pub const MAX_LOGGED_INPUT_BYTES: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadLogEntry {
    pub id: String,
    pub client_id: String,
    pub tool: String,
    pub query: Option<String>,
    pub result_count: i64,
    pub returned_ids: Option<String>,
    pub created_at: String,
}

/// Returns `Ok(())` when the client is under its 24h read budget, or an
/// `Err` whose message is meant to be surfaced verbatim to the calling AI
/// (which will then show it to the user).
pub fn check_rate_limit(conn: &Connection, client_id: &str) -> Result<()> {
    let limit = read_limit_per_day(conn)?;
    let used: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM mcp_read_log
             WHERE client_id = ?1 AND created_at > datetime('now','-24 hours')",
            params![client_id],
            |r| r.get(0),
        )
        .context("count recent reads")?;
    if used >= limit {
        anyhow::bail!(
            "MCP read rate limit exceeded: {used} reads in the last 24h, max {limit}. \
             Wait or raise mcp_read_limit_per_day in Palamedes settings."
        );
    }
    Ok(())
}

/// Append an audit entry. `input_json` is truncated at
/// [`MAX_LOGGED_INPUT_BYTES`]; `returned_ids` is truncated at
/// [`MAX_LOGGED_IDS`]. Both caps are documented at the call site so a
/// reviewer reading the log knows the row may be partial.
pub fn log_read(
    conn: &Connection,
    client_id: &str,
    tool: &str,
    input_json: Option<&str>,
    returned_ids: &[String],
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let id = Uuid::new_v4().to_string();
    let truncated_input = input_json.map(|s| truncate_utf8(s, MAX_LOGGED_INPUT_BYTES));
    let truncated_ids: Vec<&String> = returned_ids.iter().take(MAX_LOGGED_IDS).collect();
    let ids_json =
        serde_json::to_string(&truncated_ids).unwrap_or_else(|_| "[]".to_string());
    conn.execute(
        "INSERT INTO mcp_read_log
           (id, client_id, tool, query, result_count, returned_ids, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            client_id,
            tool,
            truncated_input,
            returned_ids.len() as i64,
            ids_json,
            now,
        ],
    )?;
    Ok(())
}

/// Reads `settings.mcp_read_limit_per_day` if present and parses as i64;
/// falls back to [`DEFAULT_READ_LIMIT_PER_DAY`]. A non-positive value
/// (e.g. user set 0) is clamped up to 1 so an accidental misconfig can't
/// brick the MCP path entirely.
fn read_limit_per_day(conn: &Connection) -> Result<i64> {
    let s: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key='mcp_read_limit_per_day'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let n = s
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(DEFAULT_READ_LIMIT_PER_DAY);
    Ok(n.max(1))
}

/// Truncate a string to at most `max_bytes`, respecting UTF-8 boundaries.
/// (Slicing a String at an arbitrary byte index panics on a char boundary.)
fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Recent reads, newest first — for the upcoming Audit panel surface.
/// Exposed but not yet read from Rust; the Tauri command lands in week 4.
#[allow(dead_code)]
pub fn recent_reads(conn: &Connection, limit: i64) -> Result<Vec<ReadLogEntry>> {
    let mut stmt = conn.prepare(
        "SELECT id, client_id, tool, query, result_count, returned_ids, created_at
         FROM mcp_read_log ORDER BY created_at DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], |r| {
        Ok(ReadLogEntry {
            id: r.get(0)?,
            client_id: r.get(1)?,
            tool: r.get(2)?,
            query: r.get(3)?,
            result_count: r.get(4)?,
            returned_ids: r.get(5)?,
            created_at: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA: &str = include_str!("../../schema.sql");

    fn fresh_conn() -> Connection {
        crate::embeddings::register_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        crate::migrations::run(&conn).unwrap();
        conn
    }

    fn seed_client(conn: &Connection) -> String {
        let (c, _) = crate::mcp::consent::upsert_client(conn, "test-client", None, None).unwrap();
        c.id
    }

    #[test]
    fn first_read_is_allowed_then_logged() {
        let conn = fresh_conn();
        let cid = seed_client(&conn);
        check_rate_limit(&conn, &cid).unwrap();
        log_read(
            &conn,
            &cid,
            "list_beliefs",
            Some("{\"limit\":50}"),
            &["b1".into(), "b2".into()],
        )
        .unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT result_count FROM mcp_read_log WHERE client_id = ?1",
                params![cid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn rate_limit_triggers_after_default_budget() {
        let conn = fresh_conn();
        let cid = seed_client(&conn);
        // Slam in a small per-day limit and exhaust it.
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('mcp_read_limit_per_day','3')",
            [],
        )
        .unwrap();
        for _ in 0..3 {
            check_rate_limit(&conn, &cid).unwrap();
            log_read(&conn, &cid, "search_beliefs", None, &["x".into()]).unwrap();
        }
        let err = check_rate_limit(&conn, &cid).unwrap_err();
        assert!(err.to_string().contains("rate limit exceeded"), "{err}");
    }

    #[test]
    fn returned_ids_capped_at_max_logged_ids() {
        let conn = fresh_conn();
        let cid = seed_client(&conn);
        let many: Vec<String> = (0..200).map(|i| format!("b{i}")).collect();
        log_read(&conn, &cid, "list_beliefs", None, &many).unwrap();
        let (count, ids_json): (i64, String) = conn
            .query_row(
                "SELECT result_count, returned_ids FROM mcp_read_log WHERE client_id = ?1",
                params![cid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(count, 200);
        let stored: Vec<String> = serde_json::from_str(&ids_json).unwrap();
        assert_eq!(stored.len(), MAX_LOGGED_IDS);
        assert_eq!(stored[0], "b0");
    }

    #[test]
    fn input_query_capped_at_max_input_bytes() {
        let conn = fresh_conn();
        let cid = seed_client(&conn);
        let huge: String = "x".repeat(MAX_LOGGED_INPUT_BYTES * 4);
        log_read(&conn, &cid, "search_beliefs", Some(&huge), &[]).unwrap();
        let stored: Option<String> = conn
            .query_row(
                "SELECT query FROM mcp_read_log WHERE client_id = ?1",
                params![cid],
                |r| r.get(0),
            )
            .unwrap();
        assert!(stored.unwrap().len() <= MAX_LOGGED_INPUT_BYTES);
    }

    #[test]
    fn zero_or_negative_limit_is_clamped_to_one() {
        let conn = fresh_conn();
        let cid = seed_client(&conn);
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('mcp_read_limit_per_day','0')",
            [],
        )
        .unwrap();
        check_rate_limit(&conn, &cid).unwrap();
        log_read(&conn, &cid, "list_beliefs", None, &["a".into()]).unwrap();
        let err = check_rate_limit(&conn, &cid).unwrap_err();
        assert!(err.to_string().contains("rate limit exceeded"), "{err}");
    }

    #[test]
    fn truncate_utf8_does_not_split_multibyte() {
        let s = "é".repeat(10); // 'é' is 2 bytes
        let t = truncate_utf8(&s, 5); // odd byte cap would split if naive
        assert!(t.len() <= 5);
        assert!(t.chars().all(|c| c == 'é'));
    }
}
