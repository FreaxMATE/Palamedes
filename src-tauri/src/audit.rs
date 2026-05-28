//! Tamper-evident audit chain.
//!
//! Every write the user (or an MCP client) makes to the belief ledger is
//! recorded as one row in `audit_chain`. Each row carries the SHA-256 of
//! the prior row plus the operation, making the table a linear hash chain
//! you can walk top-to-bottom and verify against tampering after the fact.
//!
//! The chain lives in a **separate SQLite file** (`audit.db`, sibling of
//! `palamedes.db`) on its own connection. Three reasons:
//!
//! 1. Corruption isolation — a busted main DB still leaves the audit
//!    record intact, and vice versa.
//! 2. Backup independence — you can copy `audit.db` off without coupling
//!    to a hot main-DB transaction.
//! 3. Future signing — week 4 adds an Ed25519 signature over the head
//!    hash. That key + the audit chain together travel as a self-
//!    contained proof artifact.
//!
//! ## What this does NOT defend against
//!
//! An attacker with write access to the audit DB **right now** can
//! rewrite the entire chain — the head hash is the only fixed point.
//! Week 4 closes that with an Ed25519 signature over the head, written
//! to a file the user controls. This commit is the chain itself; signing
//! lands on top.

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Mutex;

/// Cap on the metadata JSON stored per row. The audit log is for forensic
/// evidence, not data warehousing — a search query that pasted a giant
/// document doesn't belong here.
pub const MAX_METADATA_BYTES: usize = 2 * 1024;

/// Sentinel used as `prev_hash` for the very first row in the chain.
/// Storing a literal (rather than NULL) means every row has the same
/// shape and `event_hash` always derives from a known input.
pub const GENESIS_PREV: &str = "GENESIS";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS audit_chain (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts           TEXT    NOT NULL,
    operation    TEXT    NOT NULL,
    actor        TEXT    NOT NULL,
    content_hash TEXT    NOT NULL,
    prev_hash    TEXT    NOT NULL,
    event_hash   TEXT    NOT NULL,
    metadata     TEXT
);

CREATE INDEX IF NOT EXISTS idx_audit_chain_ts ON audit_chain(ts);
"#;

/// One row of the audit chain, surfaced for inspection and verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub seq: i64,
    pub ts: String,
    pub operation: String,
    pub actor: String,
    pub content_hash: String,
    pub prev_hash: String,
    pub event_hash: String,
    pub metadata: Option<String>,
}

/// Latest row in the chain — what you'd anchor an external signature to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditHead {
    pub seq: i64,
    pub event_hash: String,
    pub ts: String,
}

/// Result of walking the chain top-to-bottom and re-deriving every hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyReport {
    pub checked: i64,
    pub ok: bool,
    /// `Some((seq, reason))` of the first row that failed verification,
    /// `None` if everything checked out.
    pub first_failure: Option<(i64, String)>,
    pub head: Option<AuditHead>,
}

pub struct AuditDb {
    conn: Mutex<Connection>,
}

impl AuditDb {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Append a row. `content_payload` is hashed verbatim (it's the
    /// canonical representation of whatever changed — e.g. a serialized
    /// belief version or a merge record); pass an empty string for ops
    /// with no payload (e.g. consent grant). `metadata` is JSON, capped
    /// at [`MAX_METADATA_BYTES`].
    pub fn log(
        &self,
        operation: &str,
        actor: &str,
        content_payload: &str,
        metadata: Option<&str>,
    ) -> Result<AuditEntry> {
        if operation.is_empty() {
            return Err(anyhow!("audit: operation must be non-empty"));
        }
        if actor.is_empty() {
            return Err(anyhow!("audit: actor must be non-empty"));
        }
        let content_hash = sha256_hex(content_payload.as_bytes());
        let ts = Utc::now().to_rfc3339();
        let metadata_trimmed = metadata.map(|m| truncate_utf8(m, MAX_METADATA_BYTES));

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        let prev_hash: String = tx
            .query_row(
                "SELECT event_hash FROM audit_chain ORDER BY seq DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_else(|| GENESIS_PREV.to_string());

        let event_hash = compute_event_hash(&prev_hash, operation, actor, &content_hash, &ts);

        tx.execute(
            "INSERT INTO audit_chain
               (ts, operation, actor, content_hash, prev_hash, event_hash, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                ts,
                operation,
                actor,
                content_hash,
                prev_hash,
                event_hash,
                metadata_trimmed,
            ],
        )?;
        let seq = tx.last_insert_rowid();
        tx.commit()?;

        Ok(AuditEntry {
            seq,
            ts,
            operation: operation.into(),
            actor: actor.into(),
            content_hash,
            prev_hash,
            event_hash,
            metadata: metadata_trimmed,
        })
    }

    pub fn head(&self) -> Result<Option<AuditHead>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT seq, event_hash, ts FROM audit_chain ORDER BY seq DESC LIMIT 1",
            [],
            |r| {
                Ok(AuditHead {
                    seq: r.get(0)?,
                    event_hash: r.get(1)?,
                    ts: r.get(2)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn recent(&self, limit: i64) -> Result<Vec<AuditEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT seq, ts, operation, actor, content_hash, prev_hash, event_hash, metadata
             FROM audit_chain ORDER BY seq DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], row_to_entry)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Walk the chain top-to-bottom and re-derive every hash. Returns OK
    /// when every row is consistent with its predecessor, otherwise the
    /// first failing seq + a short explanation.
    pub fn verify_integrity(&self) -> Result<VerifyReport> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT seq, ts, operation, actor, content_hash, prev_hash, event_hash
             FROM audit_chain ORDER BY seq ASC",
        )?;
        let mut rows = stmt.query([])?;

        let mut expected_prev: String = GENESIS_PREV.to_string();
        let mut last_seen: Option<(i64, String, String)> = None; // (seq, event_hash, ts)
        let mut checked = 0i64;

        while let Some(r) = rows.next()? {
            let seq: i64 = r.get(0)?;
            let ts: String = r.get(1)?;
            let operation: String = r.get(2)?;
            let actor: String = r.get(3)?;
            let content_hash: String = r.get(4)?;
            let prev_hash: String = r.get(5)?;
            let event_hash: String = r.get(6)?;

            if prev_hash != expected_prev {
                return Ok(VerifyReport {
                    checked,
                    ok: false,
                    first_failure: Some((
                        seq,
                        format!(
                            "prev_hash {} does not match expected {}",
                            short_hash(&prev_hash),
                            short_hash(&expected_prev)
                        ),
                    )),
                    head: None,
                });
            }
            let recomputed =
                compute_event_hash(&prev_hash, &operation, &actor, &content_hash, &ts);
            if recomputed != event_hash {
                return Ok(VerifyReport {
                    checked,
                    ok: false,
                    first_failure: Some((
                        seq,
                        format!(
                            "event_hash {} does not match recomputed {} — \
                             row was tampered with",
                            short_hash(&event_hash),
                            short_hash(&recomputed)
                        ),
                    )),
                    head: None,
                });
            }
            checked += 1;
            expected_prev = event_hash.clone();
            last_seen = Some((seq, event_hash, ts));
        }

        let head = last_seen.map(|(seq, event_hash, ts)| AuditHead {
            seq,
            event_hash,
            ts,
        });
        Ok(VerifyReport {
            checked,
            ok: true,
            first_failure: None,
            head,
        })
    }
}

fn row_to_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<AuditEntry> {
    Ok(AuditEntry {
        seq: r.get(0)?,
        ts: r.get(1)?,
        operation: r.get(2)?,
        actor: r.get(3)?,
        content_hash: r.get(4)?,
        prev_hash: r.get(5)?,
        event_hash: r.get(6)?,
        metadata: r.get(7)?,
    })
}

fn compute_event_hash(
    prev_hash: &str,
    operation: &str,
    actor: &str,
    content_hash: &str,
    ts: &str,
) -> String {
    let mut h = Sha256::new();
    h.update(prev_hash.as_bytes());
    h.update(b"|");
    h.update(operation.as_bytes());
    h.update(b"|");
    h.update(actor.as_bytes());
    h.update(b"|");
    h.update(content_hash.as_bytes());
    h.update(b"|");
    h.update(ts.as_bytes());
    format!("{:x}", h.finalize())
}

pub fn sha256_hex(payload: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(payload);
    format!("{:x}", h.finalize())
}

fn short_hash(h: &str) -> String {
    if h.len() <= 12 {
        h.to_string()
    } else {
        format!("{}…", &h[..12])
    }
}

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

/// Best-effort wrapper for callers that don't want a failed audit-log to
/// fail their main-DB op. Returns the `AuditEntry` on success or a
/// `tracing`-style error logged via `eprintln!` on failure.
#[inline]
pub fn log_best_effort(
    audit: &AuditDb,
    operation: &str,
    actor: &str,
    content_payload: &str,
    metadata: Option<&str>,
) {
    if let Err(e) = audit.log(operation, actor, content_payload, metadata) {
        eprintln!(
            "[palamedes] audit-log failed for op={operation} actor={actor}: {e:#}"
        );
    }
}

/// Build a structured `content_payload` string for a belief mutation. Always
/// JSON so future tooling can inspect it without parsing free-form text.
#[allow(dead_code)] // First caller lands when ledger writes get wired through audit.
pub fn belief_payload(belief_id: &str, version_id: &str, statement: &str) -> String {
    serde_json::json!({
        "belief_id": belief_id,
        "version_id": version_id,
        "statement": statement,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> AuditDb {
        AuditDb::open_in_memory().unwrap()
    }

    #[test]
    fn empty_db_has_no_head_and_verifies_trivially() {
        let a = fresh();
        assert!(a.head().unwrap().is_none());
        let v = a.verify_integrity().unwrap();
        assert!(v.ok);
        assert_eq!(v.checked, 0);
        assert!(v.head.is_none());
    }

    #[test]
    fn log_appends_and_chains_to_prior_hash() {
        let a = fresh();
        let e1 = a.log("belief.create", "user", "payload-1", None).unwrap();
        assert_eq!(e1.prev_hash, GENESIS_PREV);
        let e2 = a
            .log("belief.update", "user", "payload-2", Some("{\"x\":1}"))
            .unwrap();
        assert_eq!(e2.prev_hash, e1.event_hash);
        let head = a.head().unwrap().unwrap();
        assert_eq!(head.seq, e2.seq);
        assert_eq!(head.event_hash, e2.event_hash);
    }

    #[test]
    fn verify_integrity_passes_on_a_long_chain() {
        let a = fresh();
        for i in 0..25 {
            a.log("belief.create", "user", &format!("p{i}"), None).unwrap();
        }
        let v = a.verify_integrity().unwrap();
        assert!(v.ok, "expected OK got {:?}", v);
        assert_eq!(v.checked, 25);
    }

    #[test]
    fn verify_integrity_detects_tampered_event_hash() {
        let a = fresh();
        a.log("belief.create", "user", "p1", None).unwrap();
        a.log("belief.create", "user", "p2", None).unwrap();
        a.log("belief.create", "user", "p3", None).unwrap();
        // Tamper: change one row's content_hash but leave event_hash alone.
        {
            let conn = a.conn.lock().unwrap();
            conn.execute(
                "UPDATE audit_chain SET content_hash = 'tampered' WHERE seq = 2",
                [],
            )
            .unwrap();
        }
        let v = a.verify_integrity().unwrap();
        assert!(!v.ok);
        assert_eq!(v.first_failure.as_ref().unwrap().0, 2);
    }

    #[test]
    fn verify_integrity_detects_broken_prev_link() {
        let a = fresh();
        a.log("belief.create", "user", "p1", None).unwrap();
        a.log("belief.create", "user", "p2", None).unwrap();
        // Splice: rewrite row 2's prev_hash. Even if we also recomputed its
        // event_hash, the chain would then no longer match what the original
        // signed head asserted — but here we only break the link.
        {
            let conn = a.conn.lock().unwrap();
            conn.execute(
                "UPDATE audit_chain SET prev_hash = 'forged' WHERE seq = 2",
                [],
            )
            .unwrap();
        }
        let v = a.verify_integrity().unwrap();
        assert!(!v.ok);
        assert_eq!(v.first_failure.as_ref().unwrap().0, 2);
    }

    #[test]
    fn metadata_is_capped_at_max_metadata_bytes() {
        let a = fresh();
        let huge = "x".repeat(MAX_METADATA_BYTES * 3);
        let e = a.log("belief.create", "user", "p", Some(&huge)).unwrap();
        assert!(e.metadata.unwrap().len() <= MAX_METADATA_BYTES);
    }

    #[test]
    fn recent_returns_newest_first() {
        let a = fresh();
        for i in 0..5 {
            a.log("belief.create", "user", &format!("p{i}"), None).unwrap();
        }
        let rs = a.recent(10).unwrap();
        assert_eq!(rs.len(), 5);
        assert_eq!(rs[0].seq, 5);
        assert_eq!(rs[4].seq, 1);
    }
}
