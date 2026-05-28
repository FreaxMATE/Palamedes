//! `belief_proposals` — inbox for external-AI writes that need user review.
//!
//! Two kinds of proposal flow through this table:
//! - `propose`: an external AI suggests a new belief about the user.
//!   Stored as `{statement, suggested_category, suggested_confidence,
//!   reasoning, source}`.
//! - `correct`: an external AI flags an existing belief as wrong.
//!   Stored as `{target_belief_id, suggested_status, correction_reason}`.
//!
//! Both land with `status='pending'`. The user resolves them via the
//! audit panel inbox; on accept, `accept_proposal` materializes a real
//! belief (or new version) via the existing `Ledger` and stamps the
//! proposal with `decided_at` + `decided_belief_id`. External writes
//! never auto-assert.

use anyhow::{anyhow, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ledger::{
    Editor, Ledger, NewBelief, NewProvenance, NewVersion, ProvenanceRelation, Scope, SourceType,
    Status, TrustClass,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    Propose,
    Correct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    Pending,
    Accepted,
    Rejected,
    Superseded,
}

impl ProposalKind {
    #[allow(dead_code)] // Symmetric counterpart to `from_str`; kept for serialization parity.
    fn as_str(&self) -> &'static str {
        match self {
            Self::Propose => "propose",
            Self::Correct => "correct",
        }
    }
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "propose" => Ok(Self::Propose),
            "correct" => Ok(Self::Correct),
            other => Err(anyhow!("invalid proposal kind: {}", other)),
        }
    }
}

impl ProposalStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Superseded => "superseded",
        }
    }
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "pending" => Ok(Self::Pending),
            "accepted" => Ok(Self::Accepted),
            "rejected" => Ok(Self::Rejected),
            "superseded" => Ok(Self::Superseded),
            other => Err(anyhow!("invalid proposal status: {}", other)),
        }
    }
}

/// One row of `belief_proposals`. Carries the union of fields for both
/// kinds; nullables are populated per `kind`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub id: String,
    pub client_id: String,
    pub kind: ProposalKind,

    // propose fields
    pub statement: Option<String>,
    pub suggested_category: Option<String>,
    pub suggested_confidence: Option<f64>,
    pub reasoning: Option<String>,
    pub source: Option<String>,

    // correct fields
    pub target_belief_id: Option<String>,
    pub suggested_status: Option<String>,
    pub correction_reason: Option<String>,

    // lifecycle
    pub status: ProposalStatus,
    pub decided_at: Option<String>,
    pub decided_belief_id: Option<String>,
    pub created_at: String,
}

/// Inputs an external AI passes to `propose_belief`. Optional fields are
/// suggestions; the user can edit them on accept.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposeInput {
    pub statement: String,
    pub source: String,
    pub category: Option<String>,
    pub reasoning: Option<String>,
}

/// Inputs an external AI passes to `correct_belief`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrectInput {
    pub belief_id: String,
    /// One of: contested, corrected, expired.
    pub suggested_status: String,
    pub reason: String,
}

/// Fields the user may override when accepting a `propose` proposal.
/// All `None` means "accept the AI's suggestion verbatim". Non-None
/// fields replace the AI's suggestion in the materialized belief.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AcceptOverride {
    pub statement: Option<String>,
    pub category: Option<String>,
    // No confidence override: confidence is structural, never a stored number.
    pub trust_class: Option<TrustClass>,
}

pub fn insert_propose(
    conn: &Connection,
    client_id: &str,
    input: &ProposeInput,
) -> Result<Proposal> {
    if input.statement.trim().is_empty() {
        return Err(anyhow!("propose_belief requires a non-empty statement"));
    }
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO belief_proposals
           (id, client_id, kind, statement, suggested_category, suggested_confidence,
            reasoning, source, target_belief_id, suggested_status, correction_reason,
            status, decided_at, decided_belief_id, created_at)
         VALUES (?1, ?2, 'propose', ?3, ?4, NULL, ?5, ?6, NULL, NULL, NULL,
                 'pending', NULL, NULL, ?7)",
        params![
            id,
            client_id,
            input.statement.trim(),
            input.category,
            input.reasoning,
            input.source,
            now,
        ],
    )?;
    get_proposal(conn, &id)?
        .ok_or_else(|| anyhow!("proposal vanished after insert"))
}

pub fn insert_correct(
    conn: &Connection,
    client_id: &str,
    input: &CorrectInput,
) -> Result<Proposal> {
    if input.reason.trim().len() < 5 {
        return Err(anyhow!("correct_belief reason must be at least 5 chars"));
    }
    if !matches!(input.suggested_status.as_str(), "contested" | "corrected" | "expired") {
        return Err(anyhow!(
            "suggested_status must be one of: contested, corrected, expired (got {})",
            input.suggested_status
        ));
    }
    // Target belief must exist.
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM beliefs WHERE id = ?1",
        params![input.belief_id],
        |r| r.get(0),
    )?;
    if exists == 0 {
        return Err(anyhow!("no belief with id {}", input.belief_id));
    }
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO belief_proposals
           (id, client_id, kind, statement, suggested_category, suggested_confidence,
            reasoning, source, target_belief_id, suggested_status, correction_reason,
            status, decided_at, decided_belief_id, created_at)
         VALUES (?1, ?2, 'correct', NULL, NULL, NULL, NULL, NULL,
                 ?3, ?4, ?5, 'pending', NULL, NULL, ?6)",
        params![
            id,
            client_id,
            input.belief_id,
            input.suggested_status,
            input.reason.trim(),
            now,
        ],
    )?;
    get_proposal(conn, &id)?
        .ok_or_else(|| anyhow!("proposal vanished after insert"))
}

pub fn get_proposal(conn: &Connection, id: &str) -> Result<Option<Proposal>> {
    conn.query_row(
        "SELECT id, client_id, kind, statement, suggested_category, suggested_confidence,
                reasoning, source, target_belief_id, suggested_status, correction_reason,
                status, decided_at, decided_belief_id, created_at
         FROM belief_proposals WHERE id = ?1",
        params![id],
        row_to_proposal,
    )
    .optional()
    .map_err(Into::into)
}

/// List proposals, optionally filtered by status. Newest first.
pub fn list_proposals(
    conn: &Connection,
    status_filter: Option<ProposalStatus>,
) -> Result<Vec<Proposal>> {
    let rows = match status_filter {
        Some(s) => {
            let mut stmt = conn.prepare(
                "SELECT id, client_id, kind, statement, suggested_category, suggested_confidence,
                        reasoning, source, target_belief_id, suggested_status, correction_reason,
                        status, decided_at, decided_belief_id, created_at
                 FROM belief_proposals
                 WHERE status = ?1
                 ORDER BY created_at DESC",
            )?;
            let mapped = stmt
                .query_map(params![s.as_str()], row_to_proposal)?
                .collect::<Result<Vec<_>, _>>()?;
            mapped
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT id, client_id, kind, statement, suggested_category, suggested_confidence,
                        reasoning, source, target_belief_id, suggested_status, correction_reason,
                        status, decided_at, decided_belief_id, created_at
                 FROM belief_proposals
                 ORDER BY created_at DESC",
            )?;
            let mapped = stmt
                .query_map([], row_to_proposal)?
                .collect::<Result<Vec<_>, _>>()?;
            mapped
        }
    };
    Ok(rows)
}

pub fn reject_proposal(conn: &Connection, proposal_id: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let n = conn.execute(
        "UPDATE belief_proposals
         SET status = 'rejected', decided_at = ?1
         WHERE id = ?2 AND status = 'pending'",
        params![now, proposal_id],
    )?;
    if n == 0 {
        return Err(anyhow!("proposal {} not found or not pending", proposal_id));
    }
    Ok(())
}

/// Accept a pending proposal, materializing the appropriate ledger
/// write. Wraps everything in a transaction so we never end up with
/// a half-applied proposal.
///
/// For `propose`: inserts a new belief with `editor='ai'` and provenance
/// pointing at both the proposal row and the originating MCP client.
/// For `correct`: appends a new version to the target belief with
/// `editor='user'` (the user explicitly approved the correction) and a
/// reason that names the MCP client. Status is set to whatever the AI
/// suggested.
///
/// Returns the id of the affected belief (new for propose, target for
/// correct).
pub fn accept_proposal(
    conn: &mut Connection,
    proposal_id: &str,
    override_: Option<AcceptOverride>,
) -> Result<String> {
    let tx = conn.transaction()?;
    let proposal = get_proposal(&tx, proposal_id)?
        .ok_or_else(|| anyhow!("no proposal with id {}", proposal_id))?;
    if proposal.status != ProposalStatus::Pending {
        return Err(anyhow!(
            "proposal {} is {:?}, not pending",
            proposal_id,
            proposal.status
        ));
    }

    let belief_id = match proposal.kind {
        ProposalKind::Propose => materialize_propose(&tx, &proposal, override_.as_ref())?,
        ProposalKind::Correct => materialize_correct(&tx, &proposal)?,
    };

    let now = Utc::now().to_rfc3339();
    tx.execute(
        "UPDATE belief_proposals
         SET status = 'accepted', decided_at = ?1, decided_belief_id = ?2
         WHERE id = ?3",
        params![now, belief_id, proposal_id],
    )?;
    tx.commit()?;
    Ok(belief_id)
}

fn materialize_propose(
    conn: &Connection,
    proposal: &Proposal,
    override_: Option<&AcceptOverride>,
) -> Result<String> {
    let statement = override_
        .and_then(|o| o.statement.clone())
        .or_else(|| proposal.statement.clone())
        .ok_or_else(|| anyhow!("propose proposal {} has no statement", proposal.id))?;
    let category = override_
        .and_then(|o| o.category.clone())
        .or_else(|| proposal.suggested_category.clone());
    let trust_class = override_
        .and_then(|o| o.trust_class)
        .unwrap_or(TrustClass::Inferred);
    let status = match trust_class {
        TrustClass::Asserted => Status::Asserted,
        TrustClass::Hypothesized => Status::Inferred,
        TrustClass::Summary => Status::Inferred,
        TrustClass::Inferred => Status::Inferred,
    };

    let ledger = Ledger::new(conn);
    let (belief, version) = ledger.insert_belief(NewBelief {
        subject: "user".into(),
        category,
        status,
        trust_class,
        scope: Scope::Global,
        scope_ref_id: None,
        level: 0,
        parent_summary_id: None,
        initial_version: NewVersion {
            statement,
            // Leaf belief — confidence is structural, computed at read time.
            confidence: None,
            reason: proposal
                .reasoning
                .clone()
                .or_else(|| Some(format!("via MCP proposal {}", &proposal.id))),
            editor: Editor::Ai,
        },
    })?;

    // Provenance: the originating proposal + the MCP client. Two edges
    // so the audit UI can render both "approved from inbox proposal X"
    // and "via MCP from <client name>".
    ledger.add_provenance(
        &version.id,
        NewProvenance {
            source_type: SourceType::Proposal,
            source_id: proposal.id.clone(),
            relation: ProvenanceRelation::ExtractedFrom,
        },
    )?;
    ledger.add_provenance(
        &version.id,
        NewProvenance {
            source_type: SourceType::McpClient,
            source_id: proposal.client_id.clone(),
            relation: ProvenanceRelation::ExtractedFrom,
        },
    )?;

    Ok(belief.id)
}

fn materialize_correct(conn: &Connection, proposal: &Proposal) -> Result<String> {
    let target_id = proposal
        .target_belief_id
        .as_deref()
        .ok_or_else(|| anyhow!("correct proposal {} has no target_belief_id", proposal.id))?;
    let suggested_status = proposal
        .suggested_status
        .as_deref()
        .ok_or_else(|| anyhow!("correct proposal {} has no suggested_status", proposal.id))?;
    let new_status = Status::from_str(suggested_status)?;
    let reason = proposal.correction_reason.clone().unwrap_or_default();

    let ledger = Ledger::new(conn);
    let current = ledger
        .get_belief(target_id)?
        .ok_or_else(|| anyhow!("target belief {} not found", target_id))?;
    let current_version_id = current
        .current_version_id
        .ok_or_else(|| anyhow!("target belief {} has no current version", target_id))?;
    let current_version = ledger
        .get_version(&current_version_id)?
        .ok_or_else(|| anyhow!("current version {} missing", current_version_id))?;

    let new_version = ledger.add_version(
        target_id,
        NewVersion {
            // Keep the same statement on a correction — what changes is
            // the status (corrected / contested / expired), recorded
            // here as a versioned reason that names the MCP client.
            statement: current_version.statement,
            confidence: current_version.confidence,
            reason: Some(format!("via MCP proposal {}: {}", &proposal.id, reason)),
            editor: Editor::User,
        },
        Some(new_status),
    )?;

    let relation = match new_status {
        Status::Corrected => ProvenanceRelation::CorrectedBy,
        Status::Contested | Status::Expired => ProvenanceRelation::ContradictedBy,
        _ => ProvenanceRelation::ContradictedBy,
    };

    ledger.add_provenance(
        &new_version.id,
        NewProvenance {
            source_type: SourceType::Proposal,
            source_id: proposal.id.clone(),
            relation,
        },
    )?;
    ledger.add_provenance(
        &new_version.id,
        NewProvenance {
            source_type: SourceType::McpClient,
            source_id: proposal.client_id.clone(),
            relation,
        },
    )?;

    Ok(target_id.to_string())
}

fn row_to_proposal(r: &rusqlite::Row<'_>) -> rusqlite::Result<Proposal> {
    let kind_s: String = r.get(2)?;
    let status_s: String = r.get(11)?;
    let to_sqlite = |e: anyhow::Error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())),
        )
    };
    Ok(Proposal {
        id: r.get(0)?,
        client_id: r.get(1)?,
        kind: ProposalKind::from_str(&kind_s).map_err(to_sqlite)?,
        statement: r.get(3)?,
        suggested_category: r.get(4)?,
        suggested_confidence: r.get(5)?,
        reasoning: r.get(6)?,
        source: r.get(7)?,
        target_belief_id: r.get(8)?,
        suggested_status: r.get(9)?,
        correction_reason: r.get(10)?,
        status: ProposalStatus::from_str(&status_s).map_err(to_sqlite)?,
        decided_at: r.get(12)?,
        decided_belief_id: r.get(13)?,
        created_at: r.get(14)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::consent::upsert_client;

    const SCHEMA: &str = include_str!("../../schema.sql");

    fn fresh_conn() -> Connection {
        crate::embeddings::register_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn
    }

    fn seed_client(conn: &Connection) -> String {
        let (client, _) = upsert_client(conn, "claude-desktop", Some("0.13"), None).unwrap();
        client.id
    }

    #[test]
    fn insert_propose_persists_all_optional_fields() {
        let conn = fresh_conn();
        let cid = seed_client(&conn);
        let p = insert_propose(
            &conn,
            &cid,
            &ProposeInput {
                statement: "User prefers Vim keybindings".into(),
                source: "inferred from chat".into(),
                category: Some("preference".into()),
                reasoning: Some("Mentioned hjkl three times".into()),
            },
        )
        .unwrap();
        assert_eq!(p.kind, ProposalKind::Propose);
        assert_eq!(p.status, ProposalStatus::Pending);
        assert_eq!(p.statement.as_deref(), Some("User prefers Vim keybindings"));
        assert_eq!(p.suggested_category.as_deref(), Some("preference"));
        assert_eq!(p.suggested_confidence, None);
        assert_eq!(p.reasoning.as_deref(), Some("Mentioned hjkl three times"));
        assert_eq!(p.source.as_deref(), Some("inferred from chat"));
    }

    #[test]
    fn insert_propose_rejects_empty_statement() {
        let conn = fresh_conn();
        let cid = seed_client(&conn);
        assert!(insert_propose(
            &conn,
            &cid,
            &ProposeInput {
                statement: "   ".into(),
                source: "x".into(),
                category: None,
                reasoning: None
            }
        )
        .is_err());
    }

    #[test]
    fn insert_correct_requires_existing_target() {
        let conn = fresh_conn();
        let cid = seed_client(&conn);
        let bad = insert_correct(
            &conn,
            &cid,
            &CorrectInput {
                belief_id: "no-such-id".into(),
                suggested_status: "contested".into(),
                reason: "user contradicted in chat".into(),
            },
        );
        assert!(bad.is_err());
    }

    fn seed_belief(conn: &Connection) -> String {
        let ledger = Ledger::new(conn);
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
                    statement: "User likes spaces over tabs".into(),
                    confidence: None,
                    reason: None,
                    editor: Editor::Ai,
                },
            })
            .unwrap();
        b.id
    }

    #[test]
    fn accept_propose_materializes_belief_with_dual_provenance() {
        let mut conn = fresh_conn();
        let cid = seed_client(&conn);
        let p = insert_propose(
            &conn,
            &cid,
            &ProposeInput {
                statement: "User uses Neovim daily".into(),
                source: "claude-desktop".into(),
                category: Some("preference".into()),                reasoning: None,
            },
        )
        .unwrap();

        let belief_id = accept_proposal(&mut conn, &p.id, None).unwrap();
        let after = get_proposal(&conn, &p.id).unwrap().unwrap();
        assert_eq!(after.status, ProposalStatus::Accepted);
        assert_eq!(after.decided_belief_id.as_deref(), Some(belief_id.as_str()));

        // Belief landed via the ledger.
        let ledger = Ledger::new(&conn);
        let b = ledger.get_belief(&belief_id).unwrap().unwrap();
        assert_eq!(b.trust_class, TrustClass::Inferred);
        let v = ledger
            .get_version(&b.current_version_id.unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(v.statement, "User uses Neovim daily");
        // Leaf beliefs never store confidence — it's structural.
        assert_eq!(v.confidence, None);
        assert_eq!(v.editor, Editor::Ai);

        // Two provenance edges: one for the proposal, one for the client.
        let prov = ledger.get_provenance(&v.id).unwrap();
        assert_eq!(prov.len(), 2);
        let src_types: Vec<_> = prov.iter().map(|p| p.source_type).collect();
        assert!(src_types.contains(&SourceType::Proposal));
        assert!(src_types.contains(&SourceType::McpClient));
    }

    #[test]
    fn accept_propose_respects_user_overrides() {
        let mut conn = fresh_conn();
        let cid = seed_client(&conn);
        let p = insert_propose(
            &conn,
            &cid,
            &ProposeInput {
                statement: "User uses Neovim daily".into(),
                source: "claude-desktop".into(),
                category: Some("preference".into()),                reasoning: None,
            },
        )
        .unwrap();
        let belief_id = accept_proposal(
            &mut conn,
            &p.id,
            Some(AcceptOverride {
                statement: Some("User uses Neovim, not vanilla Vim".into()),
                category: Some("skill".into()),
                trust_class: Some(TrustClass::Asserted),
            }),
        )
        .unwrap();
        let ledger = Ledger::new(&conn);
        let b = ledger.get_belief(&belief_id).unwrap().unwrap();
        assert_eq!(b.trust_class, TrustClass::Asserted);
        assert_eq!(b.status, Status::Asserted);
        assert_eq!(b.category.as_deref(), Some("skill"));
        let v = ledger
            .get_version(&b.current_version_id.unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(v.statement, "User uses Neovim, not vanilla Vim");
        assert_eq!(v.confidence, None);
    }

    #[test]
    fn accept_correct_appends_version_with_new_status() {
        let mut conn = fresh_conn();
        let cid = seed_client(&conn);
        let target_id = seed_belief(&conn);
        let p = insert_correct(
            &conn,
            &cid,
            &CorrectInput {
                belief_id: target_id.clone(),
                suggested_status: "corrected".into(),
                reason: "user said tabs in last turn".into(),
            },
        )
        .unwrap();

        let returned = accept_proposal(&mut conn, &p.id, None).unwrap();
        assert_eq!(returned, target_id);

        let ledger = Ledger::new(&conn);
        let after = ledger.get_belief(&target_id).unwrap().unwrap();
        assert_eq!(after.status, Status::Corrected);
        let versions = ledger.get_versions(&target_id).unwrap();
        assert_eq!(versions.len(), 2);
        let v2 = versions.last().unwrap();
        assert_eq!(v2.editor, Editor::User);
        assert!(v2.reason.as_deref().unwrap().contains("via MCP proposal"));
        let prov = ledger.get_provenance(&v2.id).unwrap();
        assert_eq!(prov.len(), 2);
        assert!(prov
            .iter()
            .all(|p| p.relation == ProvenanceRelation::CorrectedBy));
    }

    #[test]
    fn reject_blocks_subsequent_accept() {
        let mut conn = fresh_conn();
        let cid = seed_client(&conn);
        let p = insert_propose(
            &conn,
            &cid,
            &ProposeInput {
                statement: "Test".into(),
                source: "x".into(),
                category: None,
                reasoning: None,
            },
        )
        .unwrap();
        reject_proposal(&conn, &p.id).unwrap();
        let after = get_proposal(&conn, &p.id).unwrap().unwrap();
        assert_eq!(after.status, ProposalStatus::Rejected);
        assert!(accept_proposal(&mut conn, &p.id, None).is_err());
    }

    #[test]
    fn list_proposals_filters_by_status() {
        let conn = fresh_conn();
        let cid = seed_client(&conn);
        let p1 = insert_propose(
            &conn,
            &cid,
            &ProposeInput {
                statement: "A".into(),
                source: "x".into(),
                category: None,
                reasoning: None,
            },
        )
        .unwrap();
        let _p2 = insert_propose(
            &conn,
            &cid,
            &ProposeInput {
                statement: "B".into(),
                source: "x".into(),
                category: None,
                reasoning: None,
            },
        )
        .unwrap();
        reject_proposal(&conn, &p1.id).unwrap();

        let pending = list_proposals(&conn, Some(ProposalStatus::Pending)).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].statement.as_deref(), Some("B"));

        let all = list_proposals(&conn, None).unwrap();
        assert_eq!(all.len(), 2);
    }
}
