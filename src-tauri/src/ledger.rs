//! Belief Ledger — first-class, versioned memory about a subject (default: 'user').
//!
//! A belief is a claim the system holds. Every edit produces a new `BeliefVersion`;
//! the `beliefs.current_version_id` always points at the newest one.
//!
//! Summaries are beliefs at `level >= 1` whose provenance points at child beliefs
//! via `ProvenanceRelation::Summarizes`.

use anyhow::{anyhow, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Enums — stored as lowercase TEXT per the CHECK constraints in schema.sql
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustClass {
    Asserted,
    Inferred,
    Hypothesized,
    Summary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Asserted,
    Inferred,
    Corrected,
    Contested,
    Expired,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Global,
    BranchLocal,
    BranchIsolated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Editor {
    User,
    Ai,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceType {
    Turn,
    Artifact,
    Belief,
    /// A user-accepted external `belief_proposals` row. Used in the
    /// provenance of beliefs materialized from the MCP inbox.
    Proposal,
    /// The `mcp_clients` row whose tool call produced this edge. Paired
    /// with `Proposal` to show "via MCP from <client>" in the audit UI.
    McpClient,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceRelation {
    ExtractedFrom,
    ReinforcedBy,
    ContradictedBy,
    CorrectedBy,
    Summarizes,
}

// Simple string round-trip for enums. Macro avoids 50 lines of match boilerplate.
macro_rules! str_enum {
    ($ty:ty, $( $variant:ident => $s:literal ),+ $(,)?) => {
        impl $ty {
            pub fn as_str(&self) -> &'static str {
                match self { $( Self::$variant => $s ),+ }
            }
            pub fn from_str(s: &str) -> Result<Self> {
                match s {
                    $( $s => Ok(Self::$variant), )+
                    other => Err(anyhow!(concat!("invalid ", stringify!($ty), ": {}"), other)),
                }
            }
        }
    };
}

str_enum!(TrustClass,
    Asserted => "asserted", Inferred => "inferred",
    Hypothesized => "hypothesized", Summary => "summary");
str_enum!(Status,
    Asserted => "asserted", Inferred => "inferred", Corrected => "corrected",
    Contested => "contested", Expired => "expired", Blocked => "blocked");
str_enum!(Scope,
    Global => "global", BranchLocal => "branch_local", BranchIsolated => "branch_isolated");
str_enum!(Editor, User => "user", Ai => "ai", System => "system");
str_enum!(SourceType,
    Turn => "turn", Artifact => "artifact", Belief => "belief",
    Proposal => "proposal", McpClient => "mcp_client");
str_enum!(ProvenanceRelation,
    ExtractedFrom => "extracted_from", ReinforcedBy => "reinforced_by",
    ContradictedBy => "contradicted_by", CorrectedBy => "corrected_by",
    Summarizes => "summarizes");

// ---------------------------------------------------------------------------
// Structs mirroring rows
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Belief {
    pub id: String,
    pub subject: String,
    pub category: Option<String>,
    pub current_version_id: Option<String>,
    pub status: Status,
    pub trust_class: TrustClass,
    pub scope: Scope,
    pub scope_ref_id: Option<String>,
    pub level: i32,
    pub parent_summary_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub last_reinforced_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeliefVersion {
    pub id: String,
    pub belief_id: String,
    pub version_num: i32,
    pub statement: String,
    /// NULL for leaf beliefs — confidence is derived structurally at read time
    /// (see `confidence.rs`), never self-reported by the model. Summaries carry
    /// a Rust-computed aggregate of their children's structural scores.
    pub confidence: Option<f64>,
    pub reason: Option<String>,
    pub editor: Editor,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    pub id: String,
    pub belief_version_id: String,
    pub source_type: SourceType,
    pub source_id: String,
    pub relation: ProvenanceRelation,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlocklistEntry {
    pub id: String,
    pub pattern: Option<String>,
    pub belief_id: Option<String>,
    pub reason: Option<String>,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// Insert specifications (what callers pass in — IDs & timestamps auto-filled)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct NewBelief {
    pub subject: String,
    pub category: Option<String>,
    pub status: Status,
    pub trust_class: TrustClass,
    pub scope: Scope,
    pub scope_ref_id: Option<String>,
    pub level: i32,
    pub parent_summary_id: Option<String>,
    pub initial_version: NewVersion,
}

#[derive(Debug, Clone)]
pub struct NewVersion {
    pub statement: String,
    /// `None` for leaf beliefs (confidence is structural, not stored).
    /// `Some(_)` only for summaries, whose aggregate is computed in Rust.
    pub confidence: Option<f64>,
    pub reason: Option<String>,
    pub editor: Editor,
}

#[derive(Debug, Clone)]
pub struct NewProvenance {
    pub source_type: SourceType,
    pub source_id: String,
    pub relation: ProvenanceRelation,
}

// ---------------------------------------------------------------------------
// Ledger — thin wrapper around a rusqlite Connection
// ---------------------------------------------------------------------------

pub struct Ledger<'a> {
    conn: &'a Connection,
}

impl<'a> Ledger<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn insert_belief(&self, spec: NewBelief) -> Result<(Belief, BeliefVersion)> {
        let now = Utc::now().to_rfc3339();
        let belief_id = Uuid::new_v4().to_string();
        let version_id = Uuid::new_v4().to_string();

        self.conn.execute(
            "INSERT INTO beliefs (id, subject, category, current_version_id, status, trust_class,
                                   scope, scope_ref_id, level, parent_summary_id,
                                   created_at, updated_at, last_reinforced_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, NULL)",
            params![
                belief_id,
                spec.subject,
                spec.category,
                version_id,
                spec.status.as_str(),
                spec.trust_class.as_str(),
                spec.scope.as_str(),
                spec.scope_ref_id,
                spec.level,
                spec.parent_summary_id,
                now,
            ],
        )?;

        self.conn.execute(
            "INSERT INTO belief_versions (id, belief_id, version_num, statement, confidence,
                                          reason, editor, created_at)
             VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7)",
            params![
                version_id,
                belief_id,
                spec.initial_version.statement,
                spec.initial_version.confidence,
                spec.initial_version.reason,
                spec.initial_version.editor.as_str(),
                now,
            ],
        )?;

        let belief = self.get_belief(&belief_id)?.expect("just inserted");
        let version = self.get_version(&version_id)?.expect("just inserted");
        Ok((belief, version))
    }

    /// Append a new version to an existing belief, bumping version_num and
    /// updating beliefs.current_version_id + updated_at.
    pub fn add_version(
        &self,
        belief_id: &str,
        new_version: NewVersion,
        new_status: Option<Status>,
    ) -> Result<BeliefVersion> {
        let now = Utc::now().to_rfc3339();
        let version_id = Uuid::new_v4().to_string();

        let next_num: i32 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(version_num), 0) + 1 FROM belief_versions WHERE belief_id = ?1",
                params![belief_id],
                |r| r.get(0),
            )?;

        self.conn.execute(
            "INSERT INTO belief_versions (id, belief_id, version_num, statement, confidence,
                                          reason, editor, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                version_id,
                belief_id,
                next_num,
                new_version.statement,
                new_version.confidence,
                new_version.reason,
                new_version.editor.as_str(),
                now,
            ],
        )?;

        if let Some(status) = new_status {
            self.conn.execute(
                "UPDATE beliefs SET current_version_id = ?1, status = ?2, updated_at = ?3 WHERE id = ?4",
                params![version_id, status.as_str(), now, belief_id],
            )?;
        } else {
            self.conn.execute(
                "UPDATE beliefs SET current_version_id = ?1, updated_at = ?2 WHERE id = ?3",
                params![version_id, now, belief_id],
            )?;
        }

        self.get_version(&version_id)?
            .ok_or_else(|| anyhow!("version vanished after insert"))
    }

    pub fn add_provenance(
        &self,
        belief_version_id: &str,
        spec: NewProvenance,
    ) -> Result<Provenance> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO belief_provenance (id, belief_version_id, source_type, source_id,
                                             relation, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                belief_version_id,
                spec.source_type.as_str(),
                spec.source_id,
                spec.relation.as_str(),
                now,
            ],
        )?;
        Ok(Provenance {
            id,
            belief_version_id: belief_version_id.to_string(),
            source_type: spec.source_type,
            source_id: spec.source_id,
            relation: spec.relation,
            created_at: now,
        })
    }

    pub fn add_blocklist_entry(
        &self,
        pattern: Option<&str>,
        belief_id: Option<&str>,
        reason: Option<&str>,
    ) -> Result<BlocklistEntry> {
        if pattern.is_none() && belief_id.is_none() {
            return Err(anyhow!("blocklist entry needs pattern or belief_id"));
        }
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO belief_blocklist (id, pattern, belief_id, reason, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, pattern, belief_id, reason, now],
        )?;
        Ok(BlocklistEntry {
            id,
            pattern: pattern.map(String::from),
            belief_id: belief_id.map(String::from),
            reason: reason.map(String::from),
            created_at: now,
        })
    }

    pub fn get_belief(&self, id: &str) -> Result<Option<Belief>> {
        self.conn
            .query_row(
                "SELECT id, subject, category, current_version_id, status, trust_class,
                        scope, scope_ref_id, level, parent_summary_id,
                        created_at, updated_at, last_reinforced_at
                 FROM beliefs WHERE id = ?1",
                params![id],
                row_to_belief,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn get_version(&self, id: &str) -> Result<Option<BeliefVersion>> {
        self.conn
            .query_row(
                "SELECT id, belief_id, version_num, statement, confidence, reason, editor, created_at
                 FROM belief_versions WHERE id = ?1",
                params![id],
                row_to_version,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn get_versions(&self, belief_id: &str) -> Result<Vec<BeliefVersion>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, belief_id, version_num, statement, confidence, reason, editor, created_at
             FROM belief_versions WHERE belief_id = ?1 ORDER BY version_num ASC",
        )?;
        let rows = stmt.query_map(params![belief_id], row_to_version)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn get_provenance(&self, belief_version_id: &str) -> Result<Vec<Provenance>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, belief_version_id, source_type, source_id, relation, created_at
             FROM belief_provenance WHERE belief_version_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![belief_version_id], row_to_provenance)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    #[allow(dead_code)] // Public Ledger API; exercised by tests, kept for future consumers.
    pub fn list_beliefs(&self) -> Result<Vec<Belief>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, subject, category, current_version_id, status, trust_class,
                    scope, scope_ref_id, level, parent_summary_id,
                    created_at, updated_at, last_reinforced_at
             FROM beliefs ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_belief)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    #[allow(dead_code)] // Public Ledger API; exercised by tests, kept for future consumers.
    pub fn children_of(&self, summary_id: &str) -> Result<Vec<Belief>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, subject, category, current_version_id, status, trust_class,
                    scope, scope_ref_id, level, parent_summary_id,
                    created_at, updated_at, last_reinforced_at
             FROM beliefs WHERE parent_summary_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![summary_id], row_to_belief)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

fn row_to_belief(r: &Row<'_>) -> rusqlite::Result<Belief> {
    let status_s: String = r.get(4)?;
    let trust_s: String = r.get(5)?;
    let scope_s: String = r.get(6)?;
    Ok(Belief {
        id: r.get(0)?,
        subject: r.get(1)?,
        category: r.get(2)?,
        current_version_id: r.get(3)?,
        status: Status::from_str(&status_s).map_err(|e| to_sqlite(&e))?,
        trust_class: TrustClass::from_str(&trust_s).map_err(|e| to_sqlite(&e))?,
        scope: Scope::from_str(&scope_s).map_err(|e| to_sqlite(&e))?,
        scope_ref_id: r.get(7)?,
        level: r.get(8)?,
        parent_summary_id: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
        last_reinforced_at: r.get(12)?,
    })
}

fn row_to_version(r: &Row<'_>) -> rusqlite::Result<BeliefVersion> {
    let editor_s: String = r.get(6)?;
    Ok(BeliefVersion {
        id: r.get(0)?,
        belief_id: r.get(1)?,
        version_num: r.get(2)?,
        statement: r.get(3)?,
        confidence: r.get(4)?,
        reason: r.get(5)?,
        editor: Editor::from_str(&editor_s).map_err(|e| to_sqlite(&e))?,
        created_at: r.get(7)?,
    })
}

fn row_to_provenance(r: &Row<'_>) -> rusqlite::Result<Provenance> {
    let source_s: String = r.get(2)?;
    let rel_s: String = r.get(4)?;
    Ok(Provenance {
        id: r.get(0)?,
        belief_version_id: r.get(1)?,
        source_type: SourceType::from_str(&source_s).map_err(|e| to_sqlite(&e))?,
        source_id: r.get(3)?,
        relation: ProvenanceRelation::from_str(&rel_s).map_err(|e| to_sqlite(&e))?,
        created_at: r.get(5)?,
    })
}

fn to_sqlite(e: &anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())),
    )
}

// ---------------------------------------------------------------------------
// Round-trip tests — the 7 validation cases for Phase 1.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA: &str = include_str!("../schema.sql");

    fn fresh_conn() -> Connection {
        crate::embeddings::register_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn
    }

    fn insert_conversation_and_message(conn: &Connection, content: &str) -> (String, String) {
        let conv_id = Uuid::new_v4().to_string();
        let msg_id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO conversations (id, title, created_at, updated_at, current_leaf_id)
             VALUES (?1, 'test', ?2, ?2, NULL)",
            params![conv_id, now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, conversation_id, parent_id, role, content,
                                   branch_title, created_at, model, tokens_in, tokens_out, cost_micro_usd)
             VALUES (?1, ?2, NULL, 'user', ?3, NULL, ?4, NULL, NULL, NULL, NULL)",
            params![msg_id, conv_id, content, now],
        )
        .unwrap();
        (conv_id, msg_id)
    }

    #[test]
    fn case_1_asserted_belief_roundtrip() {
        let conn = fresh_conn();
        let ledger = Ledger::new(&conn);
        let (b, v) = ledger
            .insert_belief(NewBelief {
                subject: "user".into(),
                category: Some("fact".into()),
                status: Status::Asserted,
                trust_class: TrustClass::Asserted,
                scope: Scope::Global,
                scope_ref_id: None,
                level: 0,
                parent_summary_id: None,
                initial_version: NewVersion {
                    statement: "Uses Tauri + Svelte for Palamedes".into(),
                    confidence: Some(1.0),
                    reason: Some("user stated directly".into()),
                    editor: Editor::User,
                },
            })
            .unwrap();

        assert_eq!(b.status, Status::Asserted);
        assert_eq!(b.trust_class, TrustClass::Asserted);
        assert_eq!(v.version_num, 1);
        assert_eq!(b.current_version_id.as_deref(), Some(v.id.as_str()));
    }

    #[test]
    fn case_2_inferred_with_provenance_to_turn() {
        let conn = fresh_conn();
        let ledger = Ledger::new(&conn);
        let (_, msg_id) =
            insert_conversation_and_message(&conn, "I prefer transparent memory over black-box");

        let (_b, v) = ledger
            .insert_belief(NewBelief {
                subject: "user".into(),
                category: Some("preference".into()),
                status: Status::Inferred,
                trust_class: TrustClass::Inferred,
                scope: Scope::Global,
                scope_ref_id: None,
                level: 0,
                parent_summary_id: None,
                initial_version: NewVersion {
                    statement: "Prefers transparent memory over black-box".into(),
                    confidence: Some(0.8),
                    reason: None,
                    editor: Editor::Ai,
                },
            })
            .unwrap();

        ledger
            .add_provenance(
                &v.id,
                NewProvenance {
                    source_type: SourceType::Turn,
                    source_id: msg_id.clone(),
                    relation: ProvenanceRelation::ExtractedFrom,
                },
            )
            .unwrap();

        let prov = ledger.get_provenance(&v.id).unwrap();
        assert_eq!(prov.len(), 1);
        assert_eq!(prov[0].source_id, msg_id);
        assert_eq!(prov[0].source_type, SourceType::Turn);
        assert_eq!(prov[0].relation, ProvenanceRelation::ExtractedFrom);
    }

    #[test]
    fn case_3_inferred_then_corrected() {
        let conn = fresh_conn();
        let ledger = Ledger::new(&conn);
        let (b, _) = ledger
            .insert_belief(NewBelief {
                subject: "user".into(),
                category: Some("preference".into()),
                status: Status::Inferred,
                trust_class: TrustClass::Inferred,
                scope: Scope::Global,
                scope_ref_id: None,
                level: 0,
                parent_summary_id: None,
                initial_version: NewVersion {
                    statement: "Is vegan".into(),
                    confidence: Some(0.71),
                    reason: None,
                    editor: Editor::Ai,
                },
            })
            .unwrap();

        let v2 = ledger
            .add_version(
                &b.id,
                NewVersion {
                    statement: "Is not vegan; was asking out of curiosity".into(),
                    confidence: Some(1.0),
                    reason: Some("user corrected".into()),
                    editor: Editor::User,
                },
                Some(Status::Corrected),
            )
            .unwrap();

        assert_eq!(v2.version_num, 2);
        let versions = ledger.get_versions(&b.id).unwrap();
        assert_eq!(versions.len(), 2);

        let refreshed = ledger.get_belief(&b.id).unwrap().unwrap();
        assert_eq!(refreshed.status, Status::Corrected);
        assert_eq!(refreshed.current_version_id, Some(v2.id));
    }

    #[test]
    fn case_4_contested_beliefs_from_different_weeks() {
        let conn = fresh_conn();
        let ledger = Ledger::new(&conn);

        let (b1, _) = ledger
            .insert_belief(NewBelief {
                subject: "user".into(),
                category: Some("preference".into()),
                status: Status::Contested,
                trust_class: TrustClass::Inferred,
                scope: Scope::Global,
                scope_ref_id: None,
                level: 0,
                parent_summary_id: None,
                initial_version: NewVersion {
                    statement: "Prefers Rust for new services".into(),
                    confidence: Some(0.8),
                    reason: None,
                    editor: Editor::Ai,
                },
            })
            .unwrap();
        let (b2, _) = ledger
            .insert_belief(NewBelief {
                subject: "user".into(),
                category: Some("preference".into()),
                status: Status::Contested,
                trust_class: TrustClass::Inferred,
                scope: Scope::Global,
                scope_ref_id: None,
                level: 0,
                parent_summary_id: None,
                initial_version: NewVersion {
                    statement: "Prefers Zig for new services".into(),
                    confidence: Some(0.7),
                    reason: None,
                    editor: Editor::Ai,
                },
            })
            .unwrap();

        assert_ne!(b1.id, b2.id);
        let all = ledger.list_beliefs().unwrap();
        assert!(all.iter().all(|b| b.status == Status::Contested));
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn case_5_blocked_belief() {
        let conn = fresh_conn();
        let ledger = Ledger::new(&conn);
        let (b, _) = ledger
            .insert_belief(NewBelief {
                subject: "user".into(),
                category: Some("preference".into()),
                status: Status::Blocked,
                trust_class: TrustClass::Inferred,
                scope: Scope::Global,
                scope_ref_id: None,
                level: 0,
                parent_summary_id: None,
                initial_version: NewVersion {
                    statement: "Is vegan".into(),
                    confidence: Some(0.0),
                    reason: Some("user corrected twice; hard-pinned wrong".into()),
                    editor: Editor::System,
                },
            })
            .unwrap();

        let entry = ledger
            .add_blocklist_entry(
                Some("user is vegan"),
                Some(&b.id),
                Some("hard-pinned after repeated corrections"),
            )
            .unwrap();

        assert_eq!(entry.belief_id.as_deref(), Some(b.id.as_str()));
        let refreshed = ledger.get_belief(&b.id).unwrap().unwrap();
        assert_eq!(refreshed.status, Status::Blocked);
    }

    #[test]
    fn case_6_summary_with_two_children() {
        let conn = fresh_conn();
        let ledger = Ledger::new(&conn);

        let (c1, _) = ledger
            .insert_belief(NewBelief {
                subject: "user".into(),
                category: Some("skill".into()),
                status: Status::Inferred,
                trust_class: TrustClass::Inferred,
                scope: Scope::Global,
                scope_ref_id: None,
                level: 0,
                parent_summary_id: None,
                initial_version: NewVersion {
                    statement: "Working on Palamedes in Rust".into(),
                    confidence: Some(0.9),
                    reason: None,
                    editor: Editor::Ai,
                },
            })
            .unwrap();
        let (c2, _) = ledger
            .insert_belief(NewBelief {
                subject: "user".into(),
                category: Some("skill".into()),
                status: Status::Inferred,
                trust_class: TrustClass::Inferred,
                scope: Scope::Global,
                scope_ref_id: None,
                level: 0,
                parent_summary_id: None,
                initial_version: NewVersion {
                    statement: "Familiar with Tauri 2".into(),
                    confidence: Some(0.85),
                    reason: None,
                    editor: Editor::Ai,
                },
            })
            .unwrap();

        let (summary, summary_v) = ledger
            .insert_belief(NewBelief {
                subject: "user".into(),
                category: Some("skill".into()),
                status: Status::Inferred,
                trust_class: TrustClass::Summary,
                scope: Scope::Global,
                scope_ref_id: None,
                level: 1,
                parent_summary_id: None,
                initial_version: NewVersion {
                    statement: "Building a Rust+Tauri personal AI tool".into(),
                    confidence: Some(0.85),
                    reason: Some("cluster of 2".into()),
                    editor: Editor::Ai,
                },
            })
            .unwrap();

        for child_id in [&c1.id, &c2.id] {
            ledger
                .add_provenance(
                    &summary_v.id,
                    NewProvenance {
                        source_type: SourceType::Belief,
                        source_id: child_id.clone(),
                        relation: ProvenanceRelation::Summarizes,
                    },
                )
                .unwrap();
            conn.execute(
                "UPDATE beliefs SET parent_summary_id = ?1 WHERE id = ?2",
                params![summary.id, child_id],
            )
            .unwrap();
        }

        let children = ledger.children_of(&summary.id).unwrap();
        assert_eq!(children.len(), 2);

        let prov = ledger.get_provenance(&summary_v.id).unwrap();
        assert_eq!(prov.len(), 2);
        assert!(prov.iter().all(|p| p.relation == ProvenanceRelation::Summarizes));
    }

    #[test]
    fn case_7_corrected_summary_children_intact() {
        let conn = fresh_conn();
        let ledger = Ledger::new(&conn);

        let (c1, _) = ledger
            .insert_belief(NewBelief {
                subject: "user".into(),
                category: Some("skill".into()),
                status: Status::Inferred,
                trust_class: TrustClass::Inferred,
                scope: Scope::Global,
                scope_ref_id: None,
                level: 0,
                parent_summary_id: None,
                initial_version: NewVersion {
                    statement: "Working on Palamedes in Rust".into(),
                    confidence: Some(0.9),
                    reason: None,
                    editor: Editor::Ai,
                },
            })
            .unwrap();
        let (summary, summary_v1) = ledger
            .insert_belief(NewBelief {
                subject: "user".into(),
                category: Some("skill".into()),
                status: Status::Inferred,
                trust_class: TrustClass::Summary,
                scope: Scope::Global,
                scope_ref_id: None,
                level: 1,
                parent_summary_id: None,
                initial_version: NewVersion {
                    statement: "Builds Rust + Tauri apps in general".into(),
                    confidence: Some(0.7),
                    reason: None,
                    editor: Editor::Ai,
                },
            })
            .unwrap();
        ledger
            .add_provenance(
                &summary_v1.id,
                NewProvenance {
                    source_type: SourceType::Belief,
                    source_id: c1.id.clone(),
                    relation: ProvenanceRelation::Summarizes,
                },
            )
            .unwrap();
        conn.execute(
            "UPDATE beliefs SET parent_summary_id = ?1 WHERE id = ?2",
            params![summary.id, c1.id],
        )
        .unwrap();

        let child_before = ledger.get_belief(&c1.id).unwrap().unwrap();

        let summary_v2 = ledger
            .add_version(
                &summary.id,
                NewVersion {
                    statement: "Specifically building Palamedes (Rust + Tauri personal AI tool)".into(),
                    confidence: Some(0.9),
                    reason: Some("user corrected — too general".into()),
                    editor: Editor::User,
                },
                Some(Status::Corrected),
            )
            .unwrap();

        let refreshed_summary = ledger.get_belief(&summary.id).unwrap().unwrap();
        assert_eq!(refreshed_summary.status, Status::Corrected);
        assert_eq!(refreshed_summary.current_version_id, Some(summary_v2.id));

        let child_after = ledger.get_belief(&c1.id).unwrap().unwrap();
        assert_eq!(child_before.current_version_id, child_after.current_version_id);
        assert_eq!(child_before.status, child_after.status);
        assert_eq!(child_after.parent_summary_id.as_deref(), Some(summary.id.as_str()));
    }
}
