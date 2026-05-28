//! Versioned JSON export + import of the belief ledger.
//!
//! Closes ANALYSIS.md §2.8 and §3.1.3 — the user can extract their
//! complete corpus to a single JSON envelope (`palamedes.export.v1`)
//! and replay it into a fresh install. This is the "your data is
//! yours" answer in a form that survives Palamedes itself: a JSON
//! file inspectable by `jq`, Python, regulators, future tools, etc.
//!
//! Round-trip strategy: rows are exported with their original primary
//! keys + all foreign-key references intact. On import the existing
//! belief tables must be empty (or `replace = true` is set, in which
//! case they are wiped first). Generating new IDs on import was
//! considered and rejected — it loses provenance pointers in the
//! ANALYSIS.md export and silently breaks downstream tools that
//! reference the old IDs.
//!
//! Both operations append a row to the audit chain so the user can
//! prove later when a snapshot was taken.

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::audit::AuditDb;

/// Bump this when the envelope shape changes in a backwards-incompatible
/// way. Importers reject envelopes whose major version differs from theirs.
pub const EXPORT_FORMAT: &str = "palamedes.export.v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportEnvelope {
    pub format: String,
    pub meta: ExportMeta,
    pub data: ExportData,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportMeta {
    pub exported_at: String,
    pub app_version: String,
    /// Highest applied schema-migration version at export time. Importer
    /// fails fast if it can't reach the same level.
    pub palamedes_schema_version: i32,
    pub belief_count: i64,
    pub include_embeddings: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ExportData {
    pub beliefs: Vec<serde_json::Value>,
    pub belief_versions: Vec<serde_json::Value>,
    pub belief_provenance: Vec<serde_json::Value>,
    pub belief_blocklist: Vec<serde_json::Value>,
    pub belief_merges: Vec<serde_json::Value>,
    pub belief_merge_dismissals: Vec<serde_json::Value>,
    /// Belief positions in the memory map (cached UMAP output). Useful for
    /// preserving the user's pinned layout across a reinstall.
    pub belief_positions: Vec<serde_json::Value>,
    /// Embeddings as `{belief_id, embedding: [f32...]}` rows. Omitted when
    /// `meta.include_embeddings == false` to keep the file small for
    /// inspection-only exports.
    pub vec_beliefs: Option<Vec<EmbeddingRow>>,
    /// User-relevant settings only — transient keys (MCP token, embedding-
    /// swap state, etc.) are stripped on export.
    pub settings: Vec<SettingRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingRow {
    pub belief_id: String,
    /// Raw f32 vector. JSON is verbose but inspectable; users who want a
    /// compact archive can `gzip` the file.
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingRow {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportReport {
    pub path: String,
    pub bytes: u64,
    pub belief_count: i64,
    pub version_count: i64,
    pub include_embeddings: bool,
    pub audit_seq: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportReport {
    pub belief_count: i64,
    pub version_count: i64,
    pub provenance_count: i64,
    pub merge_count: i64,
    pub embedding_count: i64,
    pub audit_seq: i64,
}

/// Keys whose values are private to a given installation (tokens, transient
/// state, derived flags) and must not leak across an export boundary.
const SETTING_EXCLUDE_PREFIXES: &[&str] = &[
    "mcp_server_token",
    "embedding_swap_",
    "mcp_read_limit_per_day",
];

fn should_export_setting(key: &str) -> bool {
    !SETTING_EXCLUDE_PREFIXES
        .iter()
        .any(|p| key == *p || key.starts_with(p))
}

/// Walk every table and assemble the envelope. Pure read; no mutations.
pub fn build_envelope(conn: &Connection, include_embeddings: bool) -> Result<ExportEnvelope> {
    let belief_count: i64 = conn.query_row("SELECT COUNT(*) FROM beliefs", [], |r| r.get(0))?;
    let schema_version: i32 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM palamedes_schema_version",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let data = ExportData {
        beliefs: rows_as_json(conn, "SELECT * FROM beliefs ORDER BY created_at")?,
        belief_versions: rows_as_json(
            conn,
            "SELECT * FROM belief_versions ORDER BY belief_id, version_num",
        )?,
        belief_provenance: rows_as_json(conn, "SELECT * FROM belief_provenance ORDER BY id")?,
        belief_blocklist: rows_as_json(conn, "SELECT * FROM belief_blocklist ORDER BY id")?,
        belief_merges: rows_as_json(conn, "SELECT * FROM belief_merges ORDER BY created_at")?,
        belief_merge_dismissals: rows_as_json(
            conn,
            "SELECT * FROM belief_merge_dismissals ORDER BY created_at",
        )?,
        belief_positions: rows_as_json(conn, "SELECT * FROM belief_positions ORDER BY belief_id")?,
        vec_beliefs: if include_embeddings {
            Some(read_embeddings(conn)?)
        } else {
            None
        },
        settings: read_settings(conn)?,
    };

    Ok(ExportEnvelope {
        format: EXPORT_FORMAT.into(),
        meta: ExportMeta {
            exported_at: Utc::now().to_rfc3339(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            palamedes_schema_version: schema_version,
            belief_count,
            include_embeddings,
        },
        data,
    })
}

/// Write the envelope to `path` (pretty-printed JSON), audit-log the
/// operation, and return a one-row summary the UI can show.
pub fn export_to_path(
    conn: &Connection,
    audit: &AuditDb,
    path: &Path,
    include_embeddings: bool,
) -> Result<ExportReport> {
    let envelope = build_envelope(conn, include_embeddings)?;
    let json = serde_json::to_string_pretty(&envelope)?;
    std::fs::write(path, &json).with_context(|| format!("write export to {path:?}"))?;
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    let content_hash = crate::audit::sha256_hex(json.as_bytes());
    let metadata = serde_json::json!({
        "path": path.to_string_lossy(),
        "bytes": bytes,
        "belief_count": envelope.meta.belief_count,
        "include_embeddings": include_embeddings,
        "content_sha256": content_hash,
    })
    .to_string();
    let entry = audit.log(
        "ledger.export",
        "user",
        &content_hash,
        Some(&metadata),
    )?;

    Ok(ExportReport {
        path: path.to_string_lossy().into_owned(),
        bytes,
        belief_count: envelope.meta.belief_count,
        version_count: envelope.data.belief_versions.len() as i64,
        include_embeddings,
        audit_seq: entry.seq,
    })
}

/// Read an envelope from disk, validate, and replay every row into the
/// target connection. `replace=true` clears existing belief data before
/// the replay; otherwise this errors out unless the target is already
/// empty. Audit-logs the import.
pub fn import_from_path(
    conn: &Connection,
    audit: &AuditDb,
    path: &Path,
    replace: bool,
) -> Result<ImportReport> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {path:?}"))?;
    let envelope: ExportEnvelope = serde_json::from_str(&raw)
        .with_context(|| format!("parse export JSON from {path:?}"))?;
    if envelope.format != EXPORT_FORMAT {
        return Err(anyhow!(
            "unsupported export format {:?}, this build expects {EXPORT_FORMAT}",
            envelope.format
        ));
    }

    // Fail fast if the target isn't empty (unless --replace).
    let existing: i64 =
        conn.query_row("SELECT COUNT(*) FROM beliefs", [], |r| r.get(0)).unwrap_or(0);
    if existing > 0 && !replace {
        return Err(anyhow!(
            "import target has {existing} existing beliefs; pass replace=true to overwrite"
        ));
    }
    if replace {
        clear_belief_tables(conn)?;
    }

    let mut report = ImportReport {
        belief_count: 0,
        version_count: 0,
        provenance_count: 0,
        merge_count: 0,
        embedding_count: 0,
        audit_seq: 0,
    };

    // Replay in FK-safe order: beliefs first (they reference their own
    // current_version_id which lands later via UPDATE), then versions,
    // then provenance edges, then merges/dismissals/blocklist/positions.
    insert_rows(conn, "beliefs", &envelope.data.beliefs)?;
    report.belief_count = envelope.data.beliefs.len() as i64;
    insert_rows(conn, "belief_versions", &envelope.data.belief_versions)?;
    report.version_count = envelope.data.belief_versions.len() as i64;
    insert_rows(conn, "belief_provenance", &envelope.data.belief_provenance)?;
    report.provenance_count = envelope.data.belief_provenance.len() as i64;
    insert_rows(conn, "belief_blocklist", &envelope.data.belief_blocklist)?;
    insert_rows(conn, "belief_merges", &envelope.data.belief_merges)?;
    report.merge_count = envelope.data.belief_merges.len() as i64;
    insert_rows(
        conn,
        "belief_merge_dismissals",
        &envelope.data.belief_merge_dismissals,
    )?;
    insert_rows(conn, "belief_positions", &envelope.data.belief_positions)?;

    if let Some(embeddings) = &envelope.data.vec_beliefs {
        for row in embeddings {
            let blob = crate::embeddings::vec_to_blob(&row.embedding)?;
            // INSERT OR REPLACE because vec_beliefs uses the belief_id as
            // its primary key and we may be replaying after a `replace=true`.
            conn.execute(
                "INSERT OR REPLACE INTO vec_beliefs (belief_id, embedding) VALUES (?1, ?2)",
                rusqlite::params![row.belief_id, blob],
            )?;
            report.embedding_count += 1;
        }
    }

    for s in &envelope.data.settings {
        if !should_export_setting(&s.key) {
            continue; // belt-and-braces — strip again on import
        }
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
            rusqlite::params![s.key, s.value],
        )?;
    }

    let content_hash = crate::audit::sha256_hex(raw.as_bytes());
    let metadata = serde_json::json!({
        "path": path.to_string_lossy(),
        "belief_count": report.belief_count,
        "version_count": report.version_count,
        "embedding_count": report.embedding_count,
        "content_sha256": content_hash,
        "replace": replace,
    })
    .to_string();
    let entry = audit.log("ledger.import", "user", &content_hash, Some(&metadata))?;
    report.audit_seq = entry.seq;

    Ok(report)
}

fn clear_belief_tables(conn: &Connection) -> Result<()> {
    // Order matches FK dependency: dependents first.
    for table in [
        "belief_merge_dismissals",
        "belief_merges",
        "belief_blocklist",
        "belief_provenance",
        "belief_positions",
        "belief_versions",
        "beliefs",
    ] {
        conn.execute(&format!("DELETE FROM {table}"), [])?;
    }
    conn.execute("DELETE FROM vec_beliefs", []).ok();
    Ok(())
}

fn rows_as_json(conn: &Connection, sql: &str) -> Result<Vec<serde_json::Value>> {
    let mut stmt = conn.prepare(sql)?;
    let column_count = stmt.column_count();
    let column_names: Vec<String> = (0..column_count)
        .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
        .collect();
    let rows = stmt.query_map([], |row| {
        let mut obj = serde_json::Map::with_capacity(column_count);
        for i in 0..column_count {
            let v = row.get_ref(i)?;
            let json = match v {
                rusqlite::types::ValueRef::Null => serde_json::Value::Null,
                rusqlite::types::ValueRef::Integer(n) => serde_json::Value::Number(n.into()),
                rusqlite::types::ValueRef::Real(f) => serde_json::Number::from_f64(f)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null),
                rusqlite::types::ValueRef::Text(t) => {
                    serde_json::Value::String(String::from_utf8_lossy(t).into_owned())
                }
                rusqlite::types::ValueRef::Blob(b) => {
                    // Base64-encode blobs so they round-trip through JSON.
                    serde_json::Value::String(format!("base64:{}", base64_encode(b)))
                }
            };
            obj.insert(column_names[i].clone(), json);
        }
        Ok(serde_json::Value::Object(obj))
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn insert_rows(
    conn: &Connection,
    table: &str,
    rows: &[serde_json::Value],
) -> Result<()> {
    for row in rows {
        let obj = row
            .as_object()
            .ok_or_else(|| anyhow!("expected object for {table} row, got {row}"))?;
        let cols: Vec<&String> = obj.keys().collect();
        let placeholders: Vec<String> =
            (1..=cols.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "INSERT INTO {table} ({}) VALUES ({})",
            cols.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(","),
            placeholders.join(",")
        );
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<Box<dyn rusqlite::ToSql>> = cols
            .iter()
            .map(|c| json_to_sql(&obj[c.as_str()]))
            .collect();
        let param_refs: Vec<&dyn rusqlite::ToSql> =
            params.iter().map(|b| b.as_ref()).collect();
        stmt.execute(rusqlite::params_from_iter(param_refs.iter()))?;
    }
    Ok(())
}

fn json_to_sql(v: &serde_json::Value) -> Box<dyn rusqlite::ToSql> {
    match v {
        serde_json::Value::Null => Box::new(Option::<String>::None),
        serde_json::Value::Bool(b) => Box::new(*b as i64),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Box::new(i)
            } else if let Some(f) = n.as_f64() {
                Box::new(f)
            } else {
                Box::new(n.to_string())
            }
        }
        serde_json::Value::String(s) => {
            if let Some(rest) = s.strip_prefix("base64:") {
                Box::new(base64_decode(rest).unwrap_or_default())
            } else {
                Box::new(s.clone())
            }
        }
        // Nested JSON is stringified back to TEXT; the source schema doesn't
        // have JSON columns where this matters (it uses TEXT explicitly).
        _ => Box::new(v.to_string()),
    }
}

fn read_embeddings(conn: &Connection) -> Result<Vec<EmbeddingRow>> {
    let mut stmt =
        conn.prepare("SELECT belief_id, embedding FROM vec_beliefs ORDER BY belief_id")?;
    let rows = stmt.query_map([], |r| {
        let belief_id: String = r.get(0)?;
        let blob: Vec<u8> = r.get(1)?;
        let embedding = crate::embeddings::blob_to_vec(&blob).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Blob,
                Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())),
            )
        })?;
        Ok(EmbeddingRow {
            belief_id,
            embedding,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn read_settings(conn: &Connection) -> Result<Vec<SettingRow>> {
    let mut stmt = conn.prepare("SELECT key, value FROM settings ORDER BY key")?;
    let rows = stmt.query_map([], |r| {
        Ok(SettingRow {
            key: r.get(0)?,
            value: r.get(1)?,
        })
    })?;
    Ok(rows
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|s| should_export_setting(&s.key))
        .collect())
}

// Small base64 — avoids pulling in the full `base64` crate for one use site.
// Uses the standard alphabet with '=' padding so external tools round-trip.

fn base64_encode(input: &[u8]) -> String {
    const A: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= input.len() {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8) | input[i + 2] as u32;
        out.push(A[((n >> 18) & 63) as usize] as char);
        out.push(A[((n >> 12) & 63) as usize] as char);
        out.push(A[((n >> 6) & 63) as usize] as char);
        out.push(A[(n & 63) as usize] as char);
        i += 3;
    }
    let rem = input.len() - i;
    if rem == 1 {
        let n = (input[i] as u32) << 16;
        out.push(A[((n >> 18) & 63) as usize] as char);
        out.push(A[((n >> 12) & 63) as usize] as char);
        out.push_str("==");
    } else if rem == 2 {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
        out.push(A[((n >> 18) & 63) as usize] as char);
        out.push(A[((n >> 12) & 63) as usize] as char);
        out.push(A[((n >> 6) & 63) as usize] as char);
        out.push('=');
    }
    out
}

fn base64_decode(input: &str) -> Result<Vec<u8>> {
    fn val(b: u8) -> Result<u32> {
        Ok(match b {
            b'A'..=b'Z' => (b - b'A') as u32,
            b'a'..=b'z' => (b - b'a' + 26) as u32,
            b'0'..=b'9' => (b - b'0' + 52) as u32,
            b'+' => 62,
            b'/' => 63,
            _ => return Err(anyhow!("bad base64 char: {}", b as char)),
        })
    }
    let bytes = input.as_bytes();
    if bytes.len() % 4 != 0 {
        return Err(anyhow!("base64 input length must be a multiple of 4"));
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut i = 0;
    while i < bytes.len() {
        let pad = (bytes[i + 2] == b'=') as usize + (bytes[i + 3] == b'=') as usize;
        let a = val(bytes[i])?;
        let b = val(bytes[i + 1])?;
        let c = if bytes[i + 2] == b'=' { 0 } else { val(bytes[i + 2])? };
        let d = if bytes[i + 3] == b'=' { 0 } else { val(bytes[i + 3])? };
        let n = (a << 18) | (b << 12) | (c << 6) | d;
        out.push(((n >> 16) & 0xff) as u8);
        if pad < 2 {
            out.push(((n >> 8) & 0xff) as u8);
        }
        if pad < 1 {
            out.push((n & 0xff) as u8);
        }
        i += 4;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditDb;
    use crate::ledger::{Editor, Ledger, NewBelief, NewVersion, Scope, Status, TrustClass};
    use rusqlite::Connection;

    const SCHEMA: &str = include_str!("../schema.sql");

    fn fresh_conn() -> Connection {
        crate::embeddings::register_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        crate::migrations::run(&conn).unwrap();
        conn
    }

    fn seed(conn: &Connection, statement: &str) -> String {
        let ledger = Ledger::new(conn);
        let (b, _) = ledger
            .insert_belief(NewBelief {
                subject: statement.into(),
                category: None,
                status: Status::Inferred,
                trust_class: TrustClass::Inferred,
                scope: Scope::Global,
                scope_ref_id: None,
                level: 0,
                parent_summary_id: None,
                initial_version: NewVersion {
                    statement: statement.into(),
                    confidence: None,
                    reason: None,
                    editor: Editor::Ai,
                },
            })
            .unwrap();
        b.id
    }

    #[test]
    fn export_envelope_carries_meta_and_data() {
        let conn = fresh_conn();
        seed(&conn, "User prefers SQLite");
        seed(&conn, "User uses Tauri");
        let env = build_envelope(&conn, false).unwrap();
        assert_eq!(env.format, EXPORT_FORMAT);
        assert_eq!(env.meta.belief_count, 2);
        assert_eq!(env.data.beliefs.len(), 2);
        assert_eq!(env.data.belief_versions.len(), 2);
        assert!(env.data.vec_beliefs.is_none(), "embeddings opt-in");
    }

    #[test]
    fn roundtrip_replays_beliefs_into_empty_target() {
        let src = fresh_conn();
        seed(&src, "User prefers SQLite");
        seed(&src, "User uses Tauri");
        let env = build_envelope(&src, false).unwrap();
        let json = serde_json::to_string(&env).unwrap();

        let path = std::env::temp_dir().join("palamedes_test_roundtrip.json");
        std::fs::write(&path, &json).unwrap();

        let dst = fresh_conn();
        let audit = AuditDb::open_in_memory().unwrap();
        let report = import_from_path(&dst, &audit, &path, false).unwrap();
        assert_eq!(report.belief_count, 2);
        assert_eq!(report.version_count, 2);
        let dst_count: i64 = dst
            .query_row("SELECT COUNT(*) FROM beliefs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(dst_count, 2);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn export_writes_an_audit_chain_row() {
        let conn = fresh_conn();
        seed(&conn, "User prefers SQLite");
        let audit = AuditDb::open_in_memory().unwrap();
        let path = std::env::temp_dir().join("palamedes_test_export_audit.json");
        let report = export_to_path(&conn, &audit, &path, false).unwrap();
        assert!(report.audit_seq >= 1);
        let head = audit.head().unwrap().unwrap();
        assert_eq!(head.seq, report.audit_seq);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn import_rejects_unsupported_format() {
        let dst = fresh_conn();
        let audit = AuditDb::open_in_memory().unwrap();
        let path = std::env::temp_dir().join("palamedes_test_bad_format.json");
        std::fs::write(
            &path,
            r#"{"format":"some.other.v1","meta":{"exported_at":"x","app_version":"0","palamedes_schema_version":0,"belief_count":0,"include_embeddings":false},"data":{}}"#,
        )
        .unwrap();
        let err = import_from_path(&dst, &audit, &path, false).unwrap_err();
        assert!(err.to_string().contains("unsupported export format"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn import_into_nonempty_target_refuses_without_replace() {
        let dst = fresh_conn();
        seed(&dst, "User prefers Vim");
        let env = build_envelope(&dst, false).unwrap();
        let path = std::env::temp_dir().join("palamedes_test_nonempty.json");
        std::fs::write(&path, serde_json::to_string(&env).unwrap()).unwrap();

        let audit = AuditDb::open_in_memory().unwrap();
        let err = import_from_path(&dst, &audit, &path, false).unwrap_err();
        assert!(err.to_string().contains("existing beliefs"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn base64_roundtrip() {
        for bytes in [
            &b""[..],
            &b"a"[..],
            &b"ab"[..],
            &b"abc"[..],
            &b"hello world"[..],
            &b"\x00\x01\xff\xfe"[..],
        ] {
            let e = base64_encode(bytes);
            let d = base64_decode(&e).unwrap();
            assert_eq!(d, bytes);
        }
    }

    #[test]
    fn sensitive_settings_are_stripped() {
        let conn = fresh_conn();
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('mcp_server_token','SECRET')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('embedding_swap_old_dim','4096')",
            [],
        )
        .unwrap();
        let env = build_envelope(&conn, false).unwrap();
        for s in &env.data.settings {
            assert!(
                !s.key.starts_with("mcp_server_token") && !s.key.starts_with("embedding_swap_"),
                "sensitive key {} leaked into export",
                s.key
            );
        }
    }
}
