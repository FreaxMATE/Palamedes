//! Numbered-migration runner.
//!
//! Tracks applied versions in `palamedes_schema_version` and applies any
//! `MIGRATIONS` entries whose `version` exceeds the current max. Each
//! migration runs inside its own transaction with foreign keys still enforced
//! by default — pass `FOREIGN_KEYS_OFF` in `flags` for table rebuilds that
//! need to disable them.
//!
//! ## Version 1 is implicit
//!
//! `schema.sql` is applied unconditionally on every `Db::open()` (it uses
//! `CREATE TABLE IF NOT EXISTS` throughout, so it is idempotent). The
//! runner treats that initial schema as version 1 and stamps the version
//! table on first run after this module was introduced. New schema changes
//! are migrations 2, 3, … — added both as files under `../migrations/` and
//! as entries in the `MIGRATIONS` slice below.
//!
//! ## Authoring a new migration
//!
//! 1. Create `migrations/00NN_short_name.sql`.
//! 2. Add a `Migration { version, name, sql, flags }` entry below in
//!    monotonic order. Use `include_str!` to embed the file at compile time.
//! 3. Run the app once; verify the row appears in `palamedes_schema_version`.

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection};

/// Bitflag-style options for a single migration step. Default is zero —
/// run inside a transaction with foreign keys left in their normal state.
#[allow(dead_code)] // First user lands in week 2 (audit chain rebuild).
pub const FOREIGN_KEYS_OFF: u32 = 1 << 0;

pub struct Migration {
    pub version: i32,
    pub name: &'static str,
    pub sql: &'static str,
    pub flags: u32,
}

/// Append new migrations to this slice. Order must be monotonic by `version`.
/// Version 1 ("baseline") is implicit — never list it here.
static MIGRATIONS: &[Migration] = &[
    // Example for the next migration (week 2):
    // Migration {
    //     version: 2,
    //     name: "audit_chain",
    //     sql: include_str!("../migrations/0002_audit_chain.sql"),
    //     flags: 0,
    // },
];

/// Run pending migrations. Safe to call on every startup.
pub fn run(conn: &Connection) -> Result<()> {
    ensure_version_table(conn)?;
    bootstrap_baseline_if_needed(conn)?;

    let current = current_version(conn)?;

    // MIGRATIONS is sorted by version (enforced by the assertion below). We
    // walk it in order and skip anything already applied.
    let mut prev = 1;
    for m in MIGRATIONS {
        assert!(
            m.version > prev,
            "migrations must be strictly monotonic; saw {} after {}",
            m.version,
            prev
        );
        prev = m.version;
        if m.version <= current {
            continue;
        }
        apply_migration(conn, m).with_context(|| {
            format!("failed to apply migration {} ({})", m.version, m.name)
        })?;
    }
    Ok(())
}

fn ensure_version_table(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS palamedes_schema_version (
             version    INTEGER PRIMARY KEY,
             name       TEXT NOT NULL,
             applied_at TEXT NOT NULL
         )",
        [],
    )?;
    Ok(())
}

/// If the DB already has user tables (i.e. the baseline schema is in place)
/// but no version rows yet, stamp it as version 1 so the runner doesn't try
/// to apply future migrations against a fresh empty DB twice.
fn bootstrap_baseline_if_needed(conn: &Connection) -> Result<()> {
    let has_versions: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM palamedes_schema_version)",
        [],
        |r| r.get(0),
    )?;
    if has_versions {
        return Ok(());
    }
    let has_beliefs_table: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master
                       WHERE type='table' AND name='beliefs')",
        [],
        |r| r.get(0),
    )?;
    if !has_beliefs_table {
        // First-ever open on this DB — `schema.sql` will create the tables
        // and we'll record the baseline immediately after.
        // The caller of Db::open() runs the schema BEFORE this function, so
        // by now the tables exist.
        return Ok(());
    }
    conn.execute(
        "INSERT INTO palamedes_schema_version (version, name, applied_at)
         VALUES (1, 'baseline', ?1)",
        params![Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

fn current_version(conn: &Connection) -> Result<i32> {
    let v: i32 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM palamedes_schema_version",
        [],
        |r| r.get(0),
    )?;
    Ok(v)
}

fn apply_migration(conn: &Connection, m: &Migration) -> Result<()> {
    let needs_fk_toggle = m.flags & FOREIGN_KEYS_OFF != 0;
    if needs_fk_toggle {
        // `PRAGMA foreign_keys` is a no-op inside a transaction, so set it
        // outside. The migration's own SQL may issue further pragmas if it
        // needs to.
        conn.pragma_update(None, "foreign_keys", false)?;
    }
    let result = (|| -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(m.sql)?;
        tx.execute(
            "INSERT INTO palamedes_schema_version (version, name, applied_at)
             VALUES (?1, ?2, ?3)",
            params![m.version, m.name, Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
        Ok(())
    })();
    if needs_fk_toggle {
        // Re-enable regardless of result so we don't leave the connection in
        // an unexpected pragma state.
        conn.pragma_update(None, "foreign_keys", true)?;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Connection {
        Connection::open_in_memory().unwrap()
    }

    #[test]
    fn empty_db_records_no_baseline() {
        let conn = fresh();
        run(&conn).unwrap();
        let v = current_version(&conn).unwrap();
        assert_eq!(v, 0, "fresh empty DB should be version 0, not 1");
    }

    #[test]
    fn db_with_baseline_tables_gets_stamped_v1() {
        let conn = fresh();
        // Simulate a baseline DB: just need the `beliefs` table to exist.
        conn.execute("CREATE TABLE beliefs (id TEXT PRIMARY KEY)", [])
            .unwrap();
        run(&conn).unwrap();
        let v = current_version(&conn).unwrap();
        assert_eq!(v, 1);
    }

    #[test]
    fn idempotent_on_repeat_calls() {
        let conn = fresh();
        conn.execute("CREATE TABLE beliefs (id TEXT PRIMARY KEY)", [])
            .unwrap();
        run(&conn).unwrap();
        run(&conn).unwrap();
        run(&conn).unwrap();
        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM palamedes_schema_version",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "baseline row should only be inserted once");
    }
}
