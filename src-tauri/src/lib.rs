mod chat_pipeline;
mod confidence;
mod db;
mod embeddings;
mod extraction;
mod graph;
mod ledger;
mod mcp;
mod merge;
mod migrations;
mod nebius;
mod recap;
mod summarization;

// Streaming chat + retrieval + receipts + embed/label backfill live in
// chat_pipeline.rs; bring its Tauri commands into scope so the
// `generate_handler!` macro can reference them unqualified.
use chat_pipeline::{
    cancel_stream, embed_unembedded_beliefs, regenerate, regenerate_belief_labels, send_message,
};

// Memory-map commands live in graph.rs; bring them into scope so the
// `generate_handler!` macro can reference them unqualified.
use graph::{
    get_belief_embeddings, get_graph_edges_extended, get_graph_snapshot,
    get_turns_for_belief, save_belief_positions,
};

// Merge / dedup commands live in merge.rs.
use merge::{
    auto_merge_duplicates, dismiss_merge_candidate, list_merge_candidates, merge_all_candidates,
    merge_beliefs, recent_merges, run_auto_merge, undo_merge,
};
// Internal merge helpers used by the tests at the bottom of this file.
#[cfg(test)]
use merge::{merge_in_tx, pick_keeper, undo_merge_in_conn};

use db::{find_top_dedup_match, Artifact, Conversation, Db, Message as DbMessage};
use extraction::TurnContext;
use ledger::{Editor, Ledger, NewBelief, NewProvenance, NewVersion, ProvenanceRelation, Scope, SourceType, Status, TrustClass};
use nebius::NebiusClient;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tauri::{Manager, State};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

pub struct AppState {
    client: NebiusClient,
    db: Arc<Db>,
    active_streams: Mutex<HashMap<String, CancellationToken>>,
    data_dir: std::path::PathBuf,
    /// MCP server handle when running. None when disabled / stopped.
    /// `tokio::sync::Mutex` (not `std::sync::Mutex`) because the start /
    /// stop commands are async and need to hold the guard across `.await`.
    mcp_server: tokio::sync::Mutex<Option<mcp::server::McpServerHandle>>,
}

/// What the extracted beliefs are extracted *from*. A chat turn carries the
/// user's message id; a captured note carries the artifact id.
#[derive(Debug, Clone)]
pub(crate) enum ExtractionSource {
    Turn(String),
    Artifact(String),
}

impl ExtractionSource {
    fn id(&self) -> &str {
        match self {
            Self::Turn(id) | Self::Artifact(id) => id,
        }
    }
    fn source_type(&self) -> SourceType {
        match self {
            Self::Turn(_) => SourceType::Turn,
            Self::Artifact(_) => SourceType::Artifact,
        }
    }
    /// Only chat turns map cleanly to a `messages` row, so artifact-sourced
    /// extraction logs leave the turn_id NULL.
    fn turn_id_for_log(&self) -> Option<&str> {
        match self {
            Self::Turn(id) => Some(id),
            Self::Artifact(_) => None,
        }
    }
}

/// Kick off background belief extraction. Never blocks the UI and never
/// propagates errors back to the user — anything that goes wrong is written
/// to `extraction_log`. Called from `chat_pipeline::send_message` and from
/// `capture_note` below.
pub(crate) fn spawn_extraction(
    db: Arc<Db>,
    client: NebiusClient,
    model: String,
    source: ExtractionSource,
    user_content: String,
    assistant_content: String,
) {
    tokio::spawn(async move {
        let blocklist = match db.blocklist_hints() {
            Ok(v) => v,
            Err(e) => {
                let _ = db.log_extraction(
                    source.turn_id_for_log(),
                    "failed",
                    Some(&model),
                    None,
                    Some(&format!("blocklist fetch failed: {}", e)),
                );
                return;
            }
        };

        // Keep the user's text so we can verify "asserted" claims are actually
        // grounded in their own words before trusting that trust class.
        let user_text = user_content.clone();
        let ctx = TurnContext {
            user_content,
            assistant_content,
            blocklist_patterns: blocklist,
        };

        let outcome = match client.extract_beliefs(&model, ctx).await {
            Ok(o) => o,
            Err(e) => {
                let _ = db.log_extraction(
                    source.turn_id_for_log(),
                    "failed",
                    Some(&model),
                    None,
                    Some(&e.to_string()),
                );
                return;
            }
        };

        if outcome.drafts.is_empty() {
            let _ = db.log_extraction(
                source.turn_id_for_log(),
                "empty",
                Some(&model),
                Some(&outcome.raw_response),
                None,
            );
            return;
        }

        // Pre-embed every draft in parallel, OUTSIDE the DB transaction.
        // Each Some(vec) is a normalized embedding ready for dedup +
        // storage; None means the embedding call failed and we'll insert
        // the belief unembedded (the backfill command can recover later).
        let embedding_model = db
            .get_setting("embedding_model")
            .ok()
            .flatten()
            .unwrap_or_else(|| embeddings::DEFAULT_EMBEDDING_MODEL.to_string());
        let dedup_threshold: f64 = db
            .get_setting("dedup_cosine_threshold")
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(embeddings::DEFAULT_DEDUP_COSINE_THRESHOLD);

        let embed_futures = outcome.drafts.iter().map(|d| {
            let c = client.clone();
            let m = embedding_model.clone();
            let s = d.statement.clone();
            async move {
                match c.embed(&m, &s).await {
                    Ok(mut v) if v.len() == embeddings::EMBEDDING_DIM => {
                        embeddings::normalize(&mut v);
                        Some(v)
                    }
                    _ => None,
                }
            }
        });
        let embeddings_per_draft: Vec<Option<Vec<f32>>> =
            futures::future::join_all(embed_futures).await;

        // Now do all writes atomically. Each iteration either reinforces
        // an existing belief, drops the draft (matches a blocked one), or
        // inserts a new belief (and stores its embedding so later drafts
        // in this same batch can dedup against it via the same query).
        let now = chrono::Utc::now().to_rfc3339();
        let write_result = db.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let ledger = Ledger::new(&tx);
            let mut inserted_new = 0i32;
            let mut reinforced = 0i32;
            let mut dropped_blocked_match = 0i32;
            let mut unembedded = 0i32;
            let mut new_to_label: Vec<(String, String)> = Vec::new();

            for (d, maybe_emb) in outcome.drafts.iter().zip(embeddings_per_draft.iter()) {
                let dedup_decision = if let Some(emb) = maybe_emb {
                    let top = find_top_dedup_match(&tx, emb)?;
                    top.and_then(|t| {
                        let cosine = embeddings::cosine_from_l2(t.distance);
                        if cosine >= dedup_threshold {
                            Some(t)
                        } else {
                            None
                        }
                    })
                } else {
                    None
                };

                if let Some(existing) = dedup_decision {
                    if existing.status == "blocked" {
                        // Defense-in-depth: blocklist patterns already feed
                        // into the extraction prompt, but cosine catches
                        // semantic re-emergence the pattern matcher misses.
                        dropped_blocked_match += 1;
                        continue;
                    }
                    // Reinforce: add a `reinforced_by` provenance edge to the
                    // existing current version, bump last_reinforced_at. The
                    // statement/confidence/version stay untouched.
                    ledger.add_provenance(
                        &existing.version_id,
                        NewProvenance {
                            source_type: source.source_type(),
                            source_id: source.id().to_string(),
                            relation: ProvenanceRelation::ReinforcedBy,
                        },
                    )?;
                    tx.execute(
                        "UPDATE beliefs SET last_reinforced_at = ?1 WHERE id = ?2",
                        params![now, existing.belief_id],
                    )?;
                    reinforced += 1;
                    continue;
                }

                // Genuinely new belief. Map the model's claimed trust class,
                // then VERIFY "asserted": a directly-stated belief must be
                // backed by a verbatim quote from the user. If the model claims
                // "asserted" without a grounded quote it is over-claiming, so we
                // demote to "inferred" — the audit thesis rests on "asserted"
                // being something you can actually check against the source.
                let claimed = match d.trust_class.as_str() {
                    "asserted" => TrustClass::Asserted,
                    "hypothesized" => TrustClass::Hypothesized,
                    _ => TrustClass::Inferred,
                };
                let trust_class = if claimed == TrustClass::Asserted {
                    let grounded = d
                        .evidence_quote
                        .as_deref()
                        .map(|q| extraction::is_grounded(q, &user_text))
                        .unwrap_or(false);
                    if grounded {
                        TrustClass::Asserted
                    } else {
                        TrustClass::Inferred
                    }
                } else {
                    claimed
                };
                let status = match trust_class {
                    TrustClass::Asserted => Status::Asserted,
                    _ => Status::Inferred,
                };
                let (belief, version) = ledger.insert_belief(NewBelief {
                    subject: "user".into(),
                    category: Some(d.category.clone()),
                    status,
                    trust_class,
                    scope: Scope::Global,
                    scope_ref_id: None,
                    level: 0,
                    parent_summary_id: None,
                    initial_version: NewVersion {
                        statement: d.statement.clone(),
                        // Leaf beliefs never store confidence — it is derived
                        // structurally at read time (see confidence.rs).
                        confidence: None,
                        reason: d.evidence_quote.clone(),
                        editor: Editor::Ai,
                    },
                })?;
                ledger.add_provenance(
                    &version.id,
                    NewProvenance {
                        source_type: source.source_type(),
                        source_id: source.id().to_string(),
                        relation: ProvenanceRelation::ExtractedFrom,
                    },
                )?;

                if let Some(emb) = maybe_emb {
                    let blob = embeddings::vec_to_blob(emb)
                        .map_err(|e| anyhow::anyhow!("vec_to_blob: {}", e))?;
                    tx.execute(
                        "INSERT INTO vec_beliefs (belief_id, embedding) VALUES (?1, ?2)",
                        params![belief.id, blob],
                    )?;
                } else {
                    unembedded += 1;
                }
                new_to_label.push((belief.id.clone(), d.statement.clone()));
                inserted_new += 1;
            }

            tx.commit()?;
            Ok((
                inserted_new,
                reinforced,
                dropped_blocked_match,
                unembedded,
                new_to_label,
            ))
        });

        match write_result {
            Ok((inserted_new, reinforced, dropped_blocked_match, unembedded, new_to_label)) => {
                // Append a stats line to the raw response so the dedup picture
                // is visible without a schema change. A future polish would
                // promote these to first-class columns on extraction_log.
                let summary = format!(
                    "{}\n--- dedup: {} new, {} reinforced, {} blocked-dup dropped, {} unembedded",
                    outcome.raw_response,
                    inserted_new,
                    reinforced,
                    dropped_blocked_match,
                    unembedded
                );
                let _ = db.log_extraction(
                    source.turn_id_for_log(),
                    "ok",
                    Some(&model),
                    Some(&summary),
                    None,
                );

                // Fire-and-forget: ask the LLM for a 1–4 word label per new
                // belief and persist it. Labels are best-effort; a failure
                // here doesn't roll back the belief.
                if !new_to_label.is_empty() {
                    let label_futures = new_to_label.into_iter().map(|(id, statement)| {
                        let c = client.clone();
                        let db = db.clone();
                        async move {
                            if let Ok(label) = c.extract_label(&statement).await {
                                let _ = db.set_belief_label(&id, &label);
                            }
                        }
                    });
                    futures::future::join_all(label_futures).await;
                }

                // Background dedup: collapse any near-identical beliefs the new
                // extractions created. Best-effort — a failure here never rolls
                // back the beliefs that were just written.
                if inserted_new > 0 {
                    match run_auto_merge(&db) {
                        Ok(n) if n > 0 => eprintln!("auto-merged {} duplicate belief(s)", n),
                        Ok(_) => {}
                        Err(e) => eprintln!("auto-merge skipped: {}", e),
                    }
                }
            }
            Err(e) => {
                let _ = db.log_extraction(
                    source.turn_id_for_log(),
                    "failed",
                    Some(&model),
                    Some(&outcome.raw_response),
                    Some(&format!("ledger write failed: {}", e)),
                );
            }
        }
    });
}

// ---------- conversation / message commands ----------

#[tauri::command]
fn list_conversations(state: State<'_, AppState>) -> Result<Vec<Conversation>, String> {
    state.db.list_conversations().map_err(|e| e.to_string())
}

#[tauri::command]
fn create_conversation(state: State<'_, AppState>, title: String) -> Result<Conversation, String> {
    state.db.create_conversation(&title).map_err(|e| e.to_string())
}

#[tauri::command]
fn wipe_chats(state: State<'_, AppState>) -> Result<(), String> {
    if !state.active_streams.lock().unwrap().is_empty() {
        return Err("cancel the active stream before wiping".into());
    }
    state.db.wipe_chats().map_err(|e| e.to_string())
}

#[tauri::command]
fn wipe_all_data(state: State<'_, AppState>) -> Result<(), String> {
    if !state.active_streams.lock().unwrap().is_empty() {
        return Err("cancel the active stream before wiping".into());
    }
    state.db.wipe_all_data().map_err(|e| e.to_string())?;
    // Best-effort: also nuke the on-disk recap markdown files so the
    // recap panel doesn't surface stale entries from a prior run.
    let recap_dir = state.data_dir.join("recaps");
    if recap_dir.exists() {
        let _ = std::fs::remove_dir_all(&recap_dir);
    }
    Ok(())
}

#[tauri::command]
fn delete_conversation(state: State<'_, AppState>, id: String) -> Result<(), String> {
    state.db.delete_conversation(&id).map_err(|e| e.to_string())
}

#[tauri::command]
fn rename_conversation(
    state: State<'_, AppState>,
    id: String,
    title: String,
) -> Result<(), String> {
    state
        .db
        .rename_conversation(&id, &title)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_messages(state: State<'_, AppState>, conversation_id: String) -> Result<Vec<DbMessage>, String> {
    state.db.get_messages(&conversation_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn set_branch_title(
    state: State<'_, AppState>,
    message_id: String,
    title: String,
) -> Result<(), String> {
    state
        .db
        .set_branch_title(&message_id, &title)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_current_leaf(
    state: State<'_, AppState>,
    conversation_id: String,
    leaf_id: Option<String>,
) -> Result<(), String> {
    state
        .db
        .set_current_leaf(&conversation_id, leaf_id.as_deref())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn deepest_descendant(state: State<'_, AppState>, message_id: String) -> Result<String, String> {
    state
        .db
        .deepest_descendant(&message_id)
        .map_err(|e| e.to_string())
}

// ---------- settings ----------

#[tauri::command]
fn get_setting(state: State<'_, AppState>, key: String) -> Result<Option<String>, String> {
    state.db.get_setting(&key).map_err(|e| e.to_string())
}

#[tauri::command]
fn set_setting(state: State<'_, AppState>, key: String, value: String) -> Result<(), String> {
    state.db.set_setting(&key, &value).map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_models(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    state.client.list_models().await.map_err(|e| e.to_string())
}

// ---------- structural confidence ----------

/// Build a belief's read-time effective confidence (structural score + coarse
/// bucket) from its row primitives. This is the ONLY path a confidence value
/// reaches the UI — the model never supplies one. `stored` is NULL for leaves
/// and the Rust-computed aggregate for summaries. Delegates to `confidence`.
fn effective_conf(
    trust_class: &str,
    reinforced_count: i64,
    created_at: &str,
    last_reinforced_at: Option<&str>,
    stored: Option<f64>,
) -> confidence::EffectiveConfidence {
    confidence::effective_for(trust_class, reinforced_count, created_at, last_reinforced_at, stored)
}

// ---------- audit (Belief Ledger) ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditBelief {
    pub id: String,
    pub statement: String,
    /// Structural confidence score [0,1], derived at read time — never
    /// self-reported by the model. The UI shows `confidence_bucket`, not this.
    pub effective_confidence: f64,
    /// Coarse bucket: "strong" | "moderate" | "tentative".
    pub confidence_bucket: String,
    pub category: Option<String>,
    pub status: String,
    pub trust_class: String,
    pub level: i32,
    pub parent_summary_id: Option<String>,
    pub provenance_count: i64,
    pub version_count: i64,
    /// Number of `reinforced_by` provenance edges across all versions of this
    /// belief. Surfaces dedup activity in the UI.
    pub reinforced_count: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvenanceItem {
    pub source_type: String,
    pub source_id: String,
    pub relation: String,
    pub preview: Option<String>,
    /// For `source_type == "turn"`: the conversation that message belongs to,
    /// so the audit UI can deep-link to the exact source utterance.
    pub conversation_id: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionItem {
    pub version_num: i32,
    pub statement: String,
    // Confidence is not versioned — it is structural and computed live. The
    // version history shows what changed (statement/status) and why, not a
    // per-version number.
    pub reason: Option<String>,
    pub editor: String,
    pub created_at: String,
    pub provenance: Vec<ProvenanceItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeliefDetail {
    pub belief: AuditBelief,
    pub versions: Vec<VersionItem>,
}

/// Map an audit row to an `AuditBelief`, computing structural confidence.
/// Both the list and detail queries select the same 14 columns in this order:
/// id, statement, confidence, category, status, trust_class, level,
/// parent_summary_id, prov_count, ver_count, reinforced_count, created_at,
/// updated_at, last_reinforced_at.
fn row_to_audit_belief(r: &rusqlite::Row<'_>) -> rusqlite::Result<AuditBelief> {
    let trust_class: String = r.get(5)?;
    let stored: Option<f64> = r.get(2)?;
    let reinforced_count: i64 = r.get(10)?;
    let created_at: String = r.get(11)?;
    let updated_at: String = r.get(12)?;
    let last_reinforced_at: Option<String> = r.get(13)?;
    let eff = effective_conf(
        &trust_class,
        reinforced_count,
        &created_at,
        last_reinforced_at.as_deref(),
        stored,
    );
    Ok(AuditBelief {
        id: r.get(0)?,
        statement: r.get(1)?,
        effective_confidence: eff.score,
        confidence_bucket: eff.bucket.as_str().to_string(),
        category: r.get(3)?,
        status: r.get(4)?,
        trust_class,
        level: r.get(6)?,
        parent_summary_id: r.get(7)?,
        provenance_count: r.get(8)?,
        version_count: r.get(9)?,
        reinforced_count,
        created_at,
        updated_at,
    })
}

#[tauri::command]
fn list_beliefs_audit(state: State<'_, AppState>) -> Result<Vec<AuditBelief>, String> {
    state
        .db
        .with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT b.id, bv.statement, bv.confidence, b.category,
                        b.status, b.trust_class, b.level, b.parent_summary_id,
                        (SELECT COUNT(*) FROM belief_provenance bp
                          WHERE bp.belief_version_id = b.current_version_id) AS prov_count,
                        (SELECT COUNT(*) FROM belief_versions v WHERE v.belief_id = b.id) AS ver_count,
                        (SELECT COUNT(*) FROM belief_provenance bp2
                          JOIN belief_versions bv2 ON bv2.id = bp2.belief_version_id
                          WHERE bv2.belief_id = b.id
                            AND bp2.relation = 'reinforced_by') AS reinforced_count,
                        b.created_at, b.updated_at, b.last_reinforced_at
                 FROM beliefs b
                 JOIN belief_versions bv ON bv.id = b.current_version_id
                 ORDER BY b.updated_at DESC",
            )?;
            let rows = stmt.query_map([], |r| Ok(row_to_audit_belief(r)?))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_belief_detail(state: State<'_, AppState>, id: String) -> Result<BeliefDetail, String> {
    state
        .db
        .with_conn(|conn| {
            let belief: AuditBelief = conn.query_row(
                "SELECT b.id, bv.statement, bv.confidence, b.category,
                        b.status, b.trust_class, b.level, b.parent_summary_id,
                        (SELECT COUNT(*) FROM belief_provenance bp
                          WHERE bp.belief_version_id = b.current_version_id) AS prov_count,
                        (SELECT COUNT(*) FROM belief_versions v WHERE v.belief_id = b.id) AS ver_count,
                        (SELECT COUNT(*) FROM belief_provenance bp2
                          JOIN belief_versions bv2 ON bv2.id = bp2.belief_version_id
                          WHERE bv2.belief_id = b.id
                            AND bp2.relation = 'reinforced_by') AS reinforced_count,
                        b.created_at, b.updated_at, b.last_reinforced_at
                 FROM beliefs b
                 JOIN belief_versions bv ON bv.id = b.current_version_id
                 WHERE b.id = ?1",
                params![id],
                row_to_audit_belief,
            )?;

            // All versions, oldest first.
            let mut vstmt = conn.prepare(
                "SELECT id, version_num, statement, confidence, reason, editor, created_at
                 FROM belief_versions WHERE belief_id = ?1 ORDER BY version_num ASC",
            )?;
            let raw_versions: Vec<(String, VersionItem)> = vstmt
                .query_map(params![id], |r| {
                    let vid: String = r.get(0)?;
                    let item = VersionItem {
                        version_num: r.get(1)?,
                        statement: r.get(2)?,
                        reason: r.get(4)?,
                        editor: r.get(5)?,
                        created_at: r.get(6)?,
                        provenance: Vec::new(),
                    };
                    Ok((vid, item))
                })?
                .collect::<Result<Vec<_>, _>>()?;

            // Provenance per version, with a short preview for 'turn' sources.
            let mut versions = Vec::with_capacity(raw_versions.len());
            for (vid, mut v) in raw_versions {
                let mut pstmt = conn.prepare(
                    "SELECT source_type, source_id, relation, created_at
                     FROM belief_provenance WHERE belief_version_id = ?1
                     ORDER BY created_at ASC",
                )?;
                let provs: Vec<(String, String, String, String)> = pstmt
                    .query_map(params![vid], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;

                for (source_type, source_id, relation, created_at) in provs {
                    let preview = match source_type.as_str() {
                        "turn" => conn
                            .query_row(
                                "SELECT content FROM messages WHERE id = ?1",
                                params![source_id],
                                |r| r.get::<_, String>(0),
                            )
                            .ok()
                            .map(|s| {
                                if s.chars().count() > 200 {
                                    let truncated: String = s.chars().take(200).collect();
                                    format!("{}…", truncated)
                                } else {
                                    s
                                }
                            }),
                        "belief" => conn
                            .query_row(
                                "SELECT bv.statement FROM beliefs b
                                 JOIN belief_versions bv ON bv.id = b.current_version_id
                                 WHERE b.id = ?1",
                                params![source_id],
                                |r| r.get::<_, String>(0),
                            )
                            .ok(),
                        "mcp_client" => conn
                            .query_row(
                                "SELECT name, version FROM mcp_clients WHERE id = ?1",
                                params![source_id],
                                |r| {
                                    let name: String = r.get(0)?;
                                    let version: Option<String> = r.get(1)?;
                                    Ok(match version {
                                        Some(v) => format!("via MCP from {name} v{v}"),
                                        None => format!("via MCP from {name}"),
                                    })
                                },
                            )
                            .ok(),
                        "proposal" => conn
                            .query_row(
                                "SELECT kind, statement, target_belief_id, correction_reason
                                 FROM belief_proposals WHERE id = ?1",
                                params![source_id],
                                |r| {
                                    let kind: String = r.get(0)?;
                                    let statement: Option<String> = r.get(1)?;
                                    let target: Option<String> = r.get(2)?;
                                    let cr: Option<String> = r.get(3)?;
                                    Ok(if kind == "propose" {
                                        match statement {
                                            Some(s) => format!("proposed: \"{s}\""),
                                            None => "proposed (no statement)".into(),
                                        }
                                    } else {
                                        let t = target.unwrap_or_default();
                                        let r = cr.unwrap_or_default();
                                        format!("correction of {}: \"{}\"", &t[..t.len().min(8)], r)
                                    })
                                },
                            )
                            .ok(),
                        _ => None,
                    };
                    // For chat turns, resolve the conversation so the UI can
                    // jump straight to the source message.
                    let conversation_id = if source_type == "turn" {
                        conn.query_row(
                            "SELECT conversation_id FROM messages WHERE id = ?1",
                            params![source_id],
                            |r| r.get::<_, String>(0),
                        )
                        .ok()
                    } else {
                        None
                    };
                    v.provenance.push(ProvenanceItem {
                        source_type,
                        source_id,
                        relation,
                        preview,
                        conversation_id,
                        created_at,
                    });
                }
                versions.push(v);
            }

            Ok(BeliefDetail { belief, versions })
        })
        .map_err(|e| e.to_string())
}

/// Flexible update used by all five audit actions. The frontend decides which
/// combination maps to which action; the backend just writes a new version
/// (editor='user') and optionally adjusts status, trust_class, and blocklist.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateBeliefArgs {
    pub id: String,
    pub new_status: Option<String>,
    pub new_trust_class: Option<String>,
    pub new_statement: Option<String>,
    // No new_confidence: confidence is structural and not user-settable as a
    // number. Users change trust_class / status / statement; the score follows.
    pub reason: Option<String>,
    pub blocklist_pattern: Option<String>,
}

#[tauri::command]
fn update_belief(state: State<'_, AppState>, args: UpdateBeliefArgs) -> Result<(), String> {
    state
        .db
        .with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;

            // Load current version so optional fields default to existing
            // values. Confidence (NULL for leaves, an aggregate for summaries)
            // is carried forward untouched — it is never user-set as a number.
            let (cur_statement, cur_confidence): (String, Option<f64>) = tx.query_row(
                "SELECT bv.statement, bv.confidence
                 FROM beliefs b JOIN belief_versions bv ON bv.id = b.current_version_id
                 WHERE b.id = ?1",
                params![args.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;

            let new_status = args
                .new_status
                .as_deref()
                .map(Status::from_str)
                .transpose()?;

            let ledger = Ledger::new(&tx);
            ledger.add_version(
                &args.id,
                NewVersion {
                    statement: args.new_statement.unwrap_or(cur_statement),
                    confidence: cur_confidence,
                    reason: args.reason.clone(),
                    editor: Editor::User,
                },
                new_status,
            )?;

            if let Some(tc) = args.new_trust_class.as_deref() {
                let parsed = TrustClass::from_str(tc)?;
                tx.execute(
                    "UPDATE beliefs SET trust_class = ?1 WHERE id = ?2",
                    params![parsed.as_str(), args.id],
                )?;
            }

            if let Some(pattern) = args.blocklist_pattern.as_deref() {
                ledger.add_blocklist_entry(Some(pattern), Some(&args.id), args.reason.as_deref())?;
            }

            tx.commit()?;
            Ok(())
        })
        .map_err(|e| e.to_string())
}

// ---------- capture inbox ----------

fn note_title_from_content(content: &str) -> String {
    let first_line = content.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        return "(empty note)".to_string();
    }
    if first_line.chars().count() <= 60 {
        first_line.to_string()
    } else {
        let truncated: String = first_line.chars().take(60).collect();
        format!("{}…", truncated)
    }
}

#[tauri::command]
fn capture_note(state: State<'_, AppState>, content: String) -> Result<Artifact, String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err("empty note".into());
    }
    let title = note_title_from_content(trimmed);
    let artifact = state
        .db
        .insert_artifact("note", Some(&title), Some(trimmed), None)
        .map_err(|e| e.to_string())?;

    let model = state
        .db
        .get_setting("model")
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| "moonshotai/Kimi-K2.5".to_string());

    spawn_extraction(
        state.db.clone(),
        state.client.clone(),
        model,
        ExtractionSource::Artifact(artifact.id.clone()),
        trimmed.to_string(),
        String::new(),
    );

    Ok(artifact)
}

#[tauri::command]
fn list_artifacts(state: State<'_, AppState>) -> Result<Vec<Artifact>, String> {
    state.db.list_artifacts().map_err(|e| e.to_string())
}

/// Name a cluster of belief statements via a short LLM call. Used by the
/// memory map to label constellations / regions / districts. Caller is
/// responsible for caching — this just takes a list of statements and
/// returns a name.
#[tauri::command]
async fn name_cluster(
    state: State<'_, AppState>,
    statements: Vec<String>,
) -> Result<String, String> {
    if statements.is_empty() {
        return Ok(String::new());
    }
    state
        .client
        .name_cluster(&statements)
        .await
        .map_err(|e| e.to_string())
}

// ---------- daily recap ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecapResponse {
    pub date: String,
    pub path: String,
    pub markdown: String,
}

/// Build the recap for `date_iso` (YYYY-MM-DD) — defaults to today in local
/// time. Writes the rendered markdown to `<data_dir>/recaps/<date>.md` and
/// returns it. Idempotent: rerunning overwrites with the latest data.
///
/// Auto-runs the summarization pass first so the "What the AI learned"
/// section reflects today's freshly-rolled-up summary layer. Summarize
/// failures are non-fatal — we still render whatever's in the ledger.
#[tauri::command]
async fn generate_recap(
    state: State<'_, AppState>,
    date_iso: Option<String>,
) -> Result<RecapResponse, String> {
    let date = date_iso.unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d").to_string());
    run_generate_recap(&state.client, &state.db, &state.data_dir, &date).await
}

/// Inner body of `generate_recap`, callable from other commands and from
/// the on-startup catch-up task.
pub(crate) async fn run_generate_recap(
    client: &NebiusClient,
    db: &Arc<Db>,
    data_dir: &std::path::Path,
    date: &str,
) -> Result<RecapResponse, String> {
    // Best-effort summarization pass. Logged-and-ignored on failure so the
    // recap path stays robust when Nebius is unreachable.
    if let Err(e) = run_summarize_pass(client, db).await {
        log::warn!("recap-time summarize failed (non-fatal): {}", e);
    }

    // SQLite stores RFC3339 with timezone. Compare on the date prefix; for
    // the user's mental model "today" is local-time-today, but we accept the
    // small fudge factor near midnight rather than building a full tz join.
    let data = db
        .with_conn(|conn| {
            let user_messages_today: i64 = conn.query_row(
                "SELECT COUNT(*) FROM messages
                 WHERE substr(created_at, 1, 10) = ?1 AND role = 'user'",
                params![date],
                |r| r.get(0),
            )?;
            let assistant_messages_today: i64 = conn.query_row(
                "SELECT COUNT(*) FROM messages
                 WHERE substr(created_at, 1, 10) = ?1 AND role = 'assistant'",
                params![date],
                |r| r.get(0),
            )?;

            let mut convs_stmt = conn.prepare(
                "SELECT c.title,
                        SUM(CASE WHEN substr(m.created_at, 1, 10) = ?1 THEN 1 ELSE 0 END) AS msgs_today,
                        SUM(CASE WHEN substr(m.created_at, 1, 10) = ?1
                                 AND m.parent_id IS NOT NULL
                                 AND (SELECT COUNT(*) FROM messages s
                                      WHERE s.parent_id = m.parent_id) > 1
                            THEN 1 ELSE 0 END) AS branches_today
                 FROM conversations c
                 JOIN messages m ON m.conversation_id = c.id
                 GROUP BY c.id
                 HAVING msgs_today > 0
                 ORDER BY msgs_today DESC",
            )?;
            let conversations: Vec<recap::ConversationActivity> = convs_stmt
                .query_map(params![date], |r| {
                    Ok(recap::ConversationActivity {
                        title: r.get(0)?,
                        messages_today: r.get(1)?,
                        branches_today: r.get(2)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;

            let mut beliefs_stmt = conn.prepare(
                "SELECT bv.statement, b.category, b.trust_class, b.status,
                        (SELECT COUNT(*) FROM belief_provenance bp
                          JOIN belief_versions bv2 ON bv2.id = bp.belief_version_id
                          WHERE bv2.belief_id = b.id AND bp.relation = 'reinforced_by') AS reinforced_count,
                        b.created_at, b.last_reinforced_at, bv.confidence
                 FROM beliefs b
                 JOIN belief_versions bv ON bv.id = b.current_version_id
                 WHERE substr(b.created_at, 1, 10) = ?1
                 ORDER BY
                    CASE b.status WHEN 'inferred' THEN 0 ELSE 1 END,
                    b.created_at DESC",
            )?;
            let beliefs: Vec<recap::BeliefRow> = beliefs_stmt
                .query_map(params![date], |r| {
                    let statement: String = r.get(0)?;
                    let category: Option<String> = r.get(1)?;
                    let trust_class: String = r.get(2)?;
                    let status: String = r.get(3)?;
                    let reinforced_count: i64 = r.get(4)?;
                    let created_at: String = r.get(5)?;
                    let last_reinforced_at: Option<String> = r.get(6)?;
                    let stored: Option<f64> = r.get(7)?;
                    let eff = effective_conf(
                        &trust_class,
                        reinforced_count,
                        &created_at,
                        last_reinforced_at.as_deref(),
                        stored,
                    );
                    Ok(recap::BeliefRow {
                        statement,
                        category,
                        confidence_bucket: eff.bucket.as_str().to_string(),
                        status,
                        trust_class,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;

            Ok(recap::RecapData {
                date: date.to_string(),
                user_messages_today,
                assistant_messages_today,
                conversations,
                beliefs,
            })
        })
        .map_err(|e| e.to_string())?;

    let markdown = recap::render_markdown(&data);

    let dir = data_dir.join("recaps");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{}.md", date));
    std::fs::write(&path, &markdown).map_err(|e| e.to_string())?;

    Ok(RecapResponse {
        date: date.to_string(),
        path: path.to_string_lossy().into_owned(),
        markdown,
    })
}

#[tauri::command]
fn list_recaps(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let dir = state.data_dir.join("recaps");
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut out: Vec<String> = std::fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.strip_suffix(".md").map(String::from)
        })
        .collect();
    out.sort();
    out.reverse();
    Ok(out)
}

// ---------- receipts (UI) ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiptItem {
    pub belief_id: String,
    pub statement: String,
    pub trust_class: String,
    pub status: String,
    pub weight: f64,
    pub rank: i32,
}

#[tauri::command]
fn get_receipts_for_turn(
    state: State<'_, AppState>,
    turn_id: String,
) -> Result<Vec<ReceiptItem>, String> {
    state
        .db
        .with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT rr.belief_id, bv.statement, b.trust_class, b.status,
                        rr.weight, rr.rank
                 FROM recall_receipts rr
                 JOIN beliefs b           ON b.id = rr.belief_id
                 JOIN belief_versions bv  ON bv.id = rr.belief_version_id
                 WHERE rr.turn_id = ?1
                 ORDER BY rr.rank ASC",
            )?;
            let rows = stmt.query_map(params![turn_id], |r| {
                Ok(ReceiptItem {
                    belief_id: r.get(0)?,
                    statement: r.get(1)?,
                    trust_class: r.get(2)?,
                    status: r.get(3)?,
                    weight: r.get(4)?,
                    rank: r.get(5)?,
                })
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .map_err(|e| e.to_string())
}



// ---------- summarization ----------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SummarizeReport {
    pub categories_processed: i32,
    pub summaries_created: i32,
    pub beliefs_covered: i32,
    /// Children the model claimed twice (overlapping summaries) that we
    /// silently demoted to first-claim-wins.
    pub overlaps_dropped: i32,
    /// Children the model invented (not present in the input set).
    pub hallucinations_dropped: i32,
    pub errors: Vec<String>,
}

/// Cluster + summarize all unsummarized level-0 beliefs, one category at a time.
/// Skips categories with fewer than 2 candidates. Wraps each category's writes
/// in its own transaction so a single LLM/parse failure can't corrupt the rest.
#[tauri::command]
async fn summarize_now(state: State<'_, AppState>) -> Result<SummarizeReport, String> {
    run_summarize_pass(&state.client, &state.db).await
}

/// Inner body of `summarize_now`, callable from other Tauri commands (e.g.
/// `generate_recap` runs this first so the daily recap reflects the
/// freshly-rolled-up summary layer). Idempotent — already-summarized
/// beliefs are excluded by the "level=0 AND parent_summary_id IS NULL"
/// query, so calling repeatedly on a quiet day is cheap.
pub(crate) async fn run_summarize_pass(
    client: &NebiusClient,
    db: &Arc<Db>,
) -> Result<SummarizeReport, String> {
    use summarization::SummarizationContext;

    let model = db
        .get_setting("model")
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| "moonshotai/Kimi-K2.5".to_string());

    // Collect candidate (category -> Vec<(id, statement)>) groups with >= 2 entries.
    let groups: Vec<(String, Vec<(String, String)>)> = db
        .with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT b.id, COALESCE(b.category, 'other'), bv.statement
                 FROM beliefs b
                 JOIN belief_versions bv ON bv.id = b.current_version_id
                 WHERE b.level = 0
                   AND b.parent_summary_id IS NULL
                   AND b.status NOT IN ('expired','blocked')
                 ORDER BY b.category, b.created_at ASC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            let mut by_cat: std::collections::BTreeMap<String, Vec<(String, String)>> =
                std::collections::BTreeMap::new();
            for row in rows {
                let (id, cat, stmt) = row?;
                by_cat.entry(cat).or_default().push((id, stmt));
            }
            Ok(by_cat
                .into_iter()
                .filter(|(_, v)| v.len() >= 2)
                .collect())
        })
        .map_err(|e| e.to_string())?;

    let mut report = SummarizeReport::default();

    for (category, beliefs) in groups {
        report.categories_processed += 1;
        let input_ids: std::collections::HashSet<String> =
            beliefs.iter().map(|(id, _)| id.clone()).collect();

        let outcome = match client
            .summarize(
                &model,
                SummarizationContext {
                    category: category.clone(),
                    beliefs,
                },
            )
            .await
        {
            Ok(o) => o,
            Err(e) => {
                report.errors.push(format!("{}: {}", category, e));
                continue;
            }
        };

        if outcome.summaries.is_empty() {
            continue;
        }

        // Pre-embed each summary statement in parallel before the write
        // transaction. Without this, summaries land without a vec_beliefs
        // row and the memory-map renderer has nothing to project them to,
        // so every hierarchy/summarizes edge ends up dangling.
        let embedding_model = db
            .get_setting("embedding_model")
            .ok()
            .flatten()
            .unwrap_or_else(|| embeddings::DEFAULT_EMBEDDING_MODEL.to_string());
        let embed_futures = outcome.summaries.iter().map(|s| {
            let c = client.clone();
            let m = embedding_model.clone();
            let stmt = s.statement.clone();
            async move {
                match c.embed(&m, &stmt).await {
                    Ok(mut v) if v.len() == embeddings::EMBEDDING_DIM => {
                        embeddings::normalize(&mut v);
                        Some(v)
                    }
                    _ => None,
                }
            }
        });
        let summary_embeddings: Vec<Option<Vec<f32>>> =
            futures::future::join_all(embed_futures).await;

        let category_for_log = category.clone();
        let summaries = outcome.summaries.clone();
        let write_result = db.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let ledger = Ledger::new(&tx);
            let mut written = 0;
            let mut covered = 0;
            let mut overlaps = 0;
            let mut hallucinated = 0;
            let mut claimed: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            let mut new_to_label: Vec<(String, String)> = Vec::new();

            for (s, maybe_emb) in summaries.iter().zip(summary_embeddings.iter()) {
                // Drop hallucinated ids (not in input) AND ids already claimed by
                // an earlier summary in this same run (first-claim-wins).
                let mut valid_children: Vec<String> = Vec::new();
                for id in &s.child_ids {
                    if !input_ids.contains(id) {
                        hallucinated += 1;
                        continue;
                    }
                    if !claimed.insert(id.clone()) {
                        overlaps += 1;
                        continue;
                    }
                    valid_children.push(id.clone());
                }
                // A single-child cluster is wasted indirection: the "summary"
                // adds a hop without compacting anything. Require ≥2 children
                // to justify a level-1 row.
                if valid_children.len() < 2 {
                    continue;
                }

                // A summary's confidence is the mean of its children's
                // structural scores — computed in Rust, never an LLM rating.
                let mut score_sum = 0.0f64;
                let mut scored = 0u32;
                for child_id in &valid_children {
                    if let Ok((tc, rc, created, last_reinf)) = tx.query_row(
                        "SELECT b.trust_class,
                                (SELECT COUNT(*) FROM belief_provenance bp
                                 JOIN belief_versions bv2 ON bv2.id = bp.belief_version_id
                                 WHERE bv2.belief_id = b.id AND bp.relation = 'reinforced_by'),
                                b.created_at, b.last_reinforced_at
                         FROM beliefs b WHERE b.id = ?1",
                        params![child_id],
                        |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, i64>(1)?,
                                r.get::<_, String>(2)?,
                                r.get::<_, Option<String>>(3)?,
                            ))
                        },
                    ) {
                        score_sum += effective_conf(&tc, rc, &created, last_reinf.as_deref(), None).score;
                        scored += 1;
                    }
                }
                let agg_confidence = if scored > 0 {
                    Some(score_sum / scored as f64)
                } else {
                    None
                };

                let (summary, summary_v) = ledger.insert_belief(NewBelief {
                    subject: "user".into(),
                    category: Some(category_for_log.clone()),
                    status: Status::Inferred,
                    trust_class: TrustClass::Summary,
                    scope: Scope::Global,
                    scope_ref_id: None,
                    level: 1,
                    parent_summary_id: None,
                    initial_version: NewVersion {
                        statement: s.statement.clone(),
                        confidence: agg_confidence,
                        reason: Some(format!("cluster of {}", valid_children.len())),
                        editor: Editor::Ai,
                    },
                })?;

                for child_id in &valid_children {
                    ledger.add_provenance(
                        &summary_v.id,
                        NewProvenance {
                            source_type: SourceType::Belief,
                            source_id: child_id.clone(),
                            relation: ProvenanceRelation::Summarizes,
                        },
                    )?;
                    tx.execute(
                        "UPDATE beliefs SET parent_summary_id = ?1 WHERE id = ?2",
                        params![summary.id, child_id],
                    )?;
                }

                if let Some(emb) = maybe_emb {
                    let blob = embeddings::vec_to_blob(emb)
                        .map_err(|e| anyhow::anyhow!("vec_to_blob: {}", e))?;
                    tx.execute(
                        "INSERT INTO vec_beliefs (belief_id, embedding) VALUES (?1, ?2)",
                        params![summary.id, blob],
                    )?;
                }
                new_to_label.push((summary.id.clone(), s.statement.clone()));

                written += 1;
                covered += valid_children.len() as i32;
            }

            tx.commit()?;
            Ok((written, covered, overlaps, hallucinated, new_to_label))
        });

        match write_result {
            Ok((written, covered, overlaps, hallucinated, new_to_label)) => {
                report.summaries_created += written;
                report.beliefs_covered += covered;
                report.overlaps_dropped += overlaps;
                report.hallucinations_dropped += hallucinated;

                // Fire-and-forget label calls for the new summaries.
                if !new_to_label.is_empty() {
                    let label_futures = new_to_label.into_iter().map(|(id, statement)| {
                        let c = client.clone();
                        let db_arc = db.clone();
                        async move {
                            if let Ok(label) = c.extract_label(&statement).await {
                                let _ = db_arc.set_belief_label(&id, &label);
                            }
                        }
                    });
                    futures::future::join_all(label_futures).await;
                }
            }
            Err(e) => {
                report
                    .errors
                    .push(format!("{}: write failed: {}", category, e));
            }
        }
    }

    Ok(report)
}

// ---------- MCP server (Phase C) ----------

#[derive(Debug, Serialize, Deserialize)]
pub struct McpStatus {
    pub enabled: bool,
    pub running: bool,
    pub port: Option<u16>,
    pub token: Option<String>,
    pub url: Option<String>,
}

#[tauri::command]
async fn mcp_status(state: State<'_, AppState>) -> Result<McpStatus, String> {
    let enabled = state
        .db
        .get_setting("mcp_server_enabled")
        .map_err(|e| e.to_string())?
        .map(|v| v == "true")
        .unwrap_or(false);
    let token = state
        .db
        .get_setting("mcp_server_token")
        .map_err(|e| e.to_string())?;
    let guard = state.mcp_server.lock().await;
    let (running, port, url) = match guard.as_ref() {
        Some(h) => (true, Some(h.port), Some(h.url())),
        None => (false, None, None),
    };
    Ok(McpStatus {
        enabled,
        running,
        port,
        token,
        url,
    })
}

#[tauri::command]
async fn mcp_start(state: State<'_, AppState>) -> Result<McpStatus, String> {
    let mut guard = state.mcp_server.lock().await;
    if guard.is_some() {
        // Already running — return current status idempotently.
        drop(guard);
        return mcp_status(state).await;
    }
    let port: u16 = state
        .db
        .get_setting("mcp_server_port")
        .map_err(|e| e.to_string())?
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5180);
    let token = match state
        .db
        .get_setting("mcp_server_token")
        .map_err(|e| e.to_string())?
    {
        Some(t) if !t.is_empty() => t,
        _ => {
            let t = mcp::server::generate_token();
            state
                .db
                .set_setting("mcp_server_token", &t)
                .map_err(|e| e.to_string())?;
            t
        }
    };
    let handle = mcp::server::start(state.db.clone(), state.client.clone(), port, token.clone())
        .await
        .map_err(|e| format!("mcp_start failed: {e}"))?;
    state
        .db
        .set_setting("mcp_server_enabled", "true")
        .map_err(|e| e.to_string())?;
    let info = McpStatus {
        enabled: true,
        running: true,
        port: Some(handle.port),
        token: Some(handle.token.clone()),
        url: Some(handle.url()),
    };
    *guard = Some(handle);
    Ok(info)
}

#[tauri::command]
async fn mcp_stop(state: State<'_, AppState>) -> Result<McpStatus, String> {
    let handle = state.mcp_server.lock().await.take();
    if let Some(h) = handle {
        h.shutdown().await;
    }
    state
        .db
        .set_setting("mcp_server_enabled", "false")
        .map_err(|e| e.to_string())?;
    let token = state
        .db
        .get_setting("mcp_server_token")
        .map_err(|e| e.to_string())?;
    Ok(McpStatus {
        enabled: false,
        running: false,
        port: None,
        token,
        url: None,
    })
}

#[tauri::command]
async fn mcp_list_proposals(
    state: State<'_, AppState>,
    status: Option<String>,
) -> Result<Vec<mcp::proposals::Proposal>, String> {
    let filter = match status.as_deref() {
        Some("pending") => Some(mcp::proposals::ProposalStatus::Pending),
        Some("accepted") => Some(mcp::proposals::ProposalStatus::Accepted),
        Some("rejected") => Some(mcp::proposals::ProposalStatus::Rejected),
        Some("superseded") => Some(mcp::proposals::ProposalStatus::Superseded),
        Some(other) => return Err(format!("unknown status filter: {other}")),
        None => None,
    };
    state
        .db
        .with_conn(|conn| mcp::proposals::list_proposals(conn, filter))
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn mcp_accept_proposal(
    state: State<'_, AppState>,
    proposal_id: String,
    statement: Option<String>,
    category: Option<String>,
    trust_class: Option<String>,
) -> Result<String, String> {
    let tc = match trust_class.as_deref() {
        None => None,
        Some(s) => Some(ledger::TrustClass::from_str(s).map_err(|e| e.to_string())?),
    };
    let override_ = if statement.is_some() || category.is_some() || tc.is_some() {
        Some(mcp::proposals::AcceptOverride {
            statement,
            category,
            trust_class: tc,
        })
    } else {
        None
    };
    let belief_id = state
        .db
        .with_conn_mut(|conn| mcp::proposals::accept_proposal(conn, &proposal_id, override_.clone()))
        .map_err(|e| e.to_string())?;
    // Kick off embedding for the new belief in the background — the
    // existing embed_unembedded_beliefs path will pick it up on its
    // next sweep too, but doing it now means the next chat turn
    // immediately benefits from retrieval.
    let db_clone = state.db.clone();
    let client_clone = state.client.clone();
    let belief_id_clone = belief_id.clone();
    tauri::async_runtime::spawn(async move {
        if let Ok(Some(statement)) = db_clone.with_conn(|conn| {
            let s: Option<String> = conn
                .query_row(
                    "SELECT bv.statement FROM beliefs b
                     JOIN belief_versions bv ON bv.id = b.current_version_id
                     WHERE b.id = ?1",
                    rusqlite::params![&belief_id_clone],
                    |r| r.get(0),
                )
                .ok();
            anyhow::Ok(s)
        }) {
            let model = db_clone
                .get_setting("embedding_model")
                .ok()
                .flatten()
                .unwrap_or_else(|| embeddings::DEFAULT_EMBEDDING_MODEL.to_string());
            if let Ok(mut v) = client_clone.embed(&model, &statement).await {
                if v.len() == embeddings::EMBEDDING_DIM {
                    embeddings::normalize(&mut v);
                    if let Ok(blob) = embeddings::vec_to_blob(&v) {
                        let _ = db_clone.with_conn(|conn| {
                            conn.execute(
                                "INSERT OR REPLACE INTO vec_beliefs (belief_id, embedding) VALUES (?1, ?2)",
                                rusqlite::params![&belief_id_clone, &blob],
                            )?;
                            anyhow::Ok(())
                        });
                    }
                }
            }
        }
    });
    Ok(belief_id)
}

#[tauri::command]
async fn mcp_reject_proposal(
    state: State<'_, AppState>,
    proposal_id: String,
) -> Result<(), String> {
    state
        .db
        .with_conn(|conn| mcp::proposals::reject_proposal(conn, &proposal_id))
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn mcp_list_clients(state: State<'_, AppState>) -> Result<Vec<mcp::consent::McpClient>, String> {
    state
        .db
        .with_conn(|conn| mcp::consent::list_clients(conn))
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn mcp_set_consent(
    state: State<'_, AppState>,
    client_id: String,
    consent_read: bool,
    consent_write: bool,
) -> Result<(), String> {
    state
        .db
        .with_conn(|conn| mcp::consent::set_consent(conn, &client_id, consent_read, consent_write))
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn mcp_revoke_client(state: State<'_, AppState>, client_id: String) -> Result<(), String> {
    state
        .db
        .with_conn(|conn| mcp::consent::revoke_client(conn, &client_id))
        .map_err(|e| e.to_string())
}

/// Pending vec_beliefs dim swap, if any. Read at app startup so the UI
/// can surface the situation. Closes the silent-data-loss gap flagged
/// in docs/ANALYSIS.md §2.7.
#[tauri::command]
async fn get_embedding_swap_state(
    state: State<'_, AppState>,
) -> Result<Option<db::EmbeddingSwapState>, String> {
    state.db.get_embedding_swap_state().map_err(|e| e.to_string())
}

/// User accepted the swap: drop the legacy `vec_beliefs_legacy_<dim>`
/// archive and clear the pending flags. The background
/// `embed_unembedded_beliefs` loop will refill the new vec_beliefs.
#[tauri::command]
async fn confirm_embedding_swap(state: State<'_, AppState>) -> Result<(), String> {
    state.db.confirm_embedding_swap().map_err(|e| e.to_string())
}

/// User dismissed the warning without re-embedding. Keeps the legacy
/// archive intact for manual recovery and just clears the banner flag.
#[tauri::command]
async fn dismiss_embedding_swap(state: State<'_, AppState>) -> Result<(), String> {
    state.db.dismiss_embedding_swap().map_err(|e| e.to_string())
}

#[tauri::command]
async fn mcp_rotate_token(state: State<'_, AppState>) -> Result<McpStatus, String> {
    let new_token = mcp::server::generate_token();
    state
        .db
        .set_setting("mcp_server_token", &new_token)
        .map_err(|e| e.to_string())?;
    // If the server is running, stop it — the new token won't take effect
    // until the user clicks Start again. We surface this by setting enabled
    // back to false; the Settings UI explains why.
    let handle = state.mcp_server.lock().await.take();
    if let Some(h) = handle {
        h.shutdown().await;
        state
            .db
            .set_setting("mcp_server_enabled", "false")
            .map_err(|e| e.to_string())?;
    }
    mcp_status(state).await
}

/// Start the MCP HTTP server without launching the Tauri GUI. Used by the
/// `palamedes-mcp-serve` binary when Cursor/Claude need the ledger but the
/// desktop app isn't running.
pub async fn headless_mcp_serve(data_dir: std::path::PathBuf) -> anyhow::Result<mcp::server::McpServerHandle> {
    embeddings::register_vec_extension();
    let db_path = data_dir.join("palamedes.db");
    let db = Arc::new(Db::open(&db_path)?);
    let client = NebiusClient::from_env()?;
    let port: u16 = db
        .get_setting("mcp_server_port")?
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5180);
    let token = match db.get_setting("mcp_server_token")? {
        Some(t) if !t.is_empty() => t,
        _ => {
            let t = mcp::server::generate_token();
            db.set_setting("mcp_server_token", &t)?;
            t
        }
    };
    db.set_setting("mcp_server_enabled", "true")?;
    mcp::server::start(db, client, port, token).await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = dotenvy::dotenv();

    // Register sqlite-vec on every new SQLite connection. Must run before
    // any Connection::open to take effect.
    embeddings::register_vec_extension();

    let client = NebiusClient::from_env()
        .expect("failed to init Nebius client — is NEBIUS_API_KEY set in .env?");

    tauri::Builder::default()
        .setup(move |app| {
            let data_dir = app
                .path()
                .app_data_dir()
                .expect("no app data dir");
            let db_path = data_dir.join("palamedes.db");
            let db = Arc::new(Db::open(&db_path).expect("failed to open SQLite db"));

            // Hold clones for the on-startup recap catch-up task and the
            // MCP auto-start task. Both spawned after `manage` so the main
            // UI never blocks on them.
            let recap_client = client.clone();
            let recap_db = db.clone();
            let recap_data_dir = data_dir.clone();
            let mcp_client = client.clone();
            let mcp_db = db.clone();
            let mcp_handle = app.handle().clone();

            app.manage(AppState {
                client,
                db,
                active_streams: Mutex::new(HashMap::new()),
                data_dir,
                mcp_server: tokio::sync::Mutex::new(None),
            });

            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }

            // Catch-up recap on startup: if today's recap markdown isn't on
            // disk yet, run summarize + recap once in the background. Keeps
            // the daily artifact fresh without a cron job; works even when
            // the app was closed at midnight.
            tauri::async_runtime::spawn(async move {
                let date = chrono::Local::now().format("%Y-%m-%d").to_string();
                let path = recap_data_dir.join("recaps").join(format!("{}.md", date));
                if path.exists() {
                    log::info!("recap for {} already exists, skipping startup catch-up", date);
                    return;
                }
                log::info!("recap for {} missing, running startup catch-up", date);
                match run_generate_recap(&recap_client, &recap_db, &recap_data_dir, &date).await {
                    Ok(_) => log::info!("startup recap catch-up wrote {}", path.display()),
                    Err(e) => log::warn!("startup recap catch-up failed (non-fatal): {}", e),
                }
            });

            // MCP server auto-start: if `mcp_server_enabled=true` in
            // settings, bind the HTTP+SSE listener in the background and
            // stash the handle in AppState. Survives Tauri dev rebuilds
            // and avoids the "click Start after every restart" friction.
            // Non-fatal on failure — the user can still click Start.
            tauri::async_runtime::spawn(async move {
                let enabled = mcp_db
                    .get_setting("mcp_server_enabled")
                    .ok()
                    .flatten()
                    .map(|v| v == "true")
                    .unwrap_or(false);
                if !enabled {
                    return;
                }
                let port: u16 = mcp_db
                    .get_setting("mcp_server_port")
                    .ok()
                    .flatten()
                    .as_deref()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(5180);
                let token = match mcp_db.get_setting("mcp_server_token").ok().flatten() {
                    Some(t) if !t.is_empty() => t,
                    _ => {
                        log::warn!("mcp auto-start: enabled=true but token missing; skipping");
                        return;
                    }
                };
                log::info!("mcp auto-start: binding on 127.0.0.1:{}", port);
                match mcp::server::start(mcp_db, mcp_client, port, token).await {
                    Ok(handle) => {
                        log::info!("mcp server listening at {}", handle.url());
                        let state = mcp_handle.state::<AppState>();
                        *state.mcp_server.lock().await = Some(handle);
                    }
                    Err(e) => {
                        log::warn!("mcp auto-start failed (non-fatal): {}", e);
                    }
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            send_message,
            regenerate,
            cancel_stream,
            list_conversations,
            create_conversation,
            delete_conversation,
            wipe_chats,
            wipe_all_data,
            rename_conversation,
            get_messages,
            set_branch_title,
            set_current_leaf,
            deepest_descendant,
            get_setting,
            set_setting,
            list_models,
            list_beliefs_audit,
            get_belief_detail,
            update_belief,
            summarize_now,
            embed_unembedded_beliefs,
            regenerate_belief_labels,
            get_receipts_for_turn,
            generate_recap,
            list_recaps,
            capture_note,
            list_artifacts,
            name_cluster,
            list_merge_candidates,
            merge_beliefs,
            dismiss_merge_candidate,
            auto_merge_duplicates,
            merge_all_candidates,
            recent_merges,
            undo_merge,
            get_graph_snapshot,
            get_graph_edges_extended,
            get_turns_for_belief,
            get_belief_embeddings,
            save_belief_positions,
            mcp_status,
            mcp_start,
            mcp_stop,
            mcp_rotate_token,
            mcp_list_clients,
            mcp_set_consent,
            mcp_revoke_client,
            mcp_list_proposals,
            mcp_accept_proposal,
            mcp_reject_proposal,
            get_embedding_swap_state,
            confirm_embedding_swap,
            dismiss_embedding_swap,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod merge_tests {
    use super::*;
    use rusqlite::Connection;

    fn fresh_conn() -> Connection {
        // register_vec_extension MUST run before open_in_memory — sqlite-vec
        // loads via sqlite3_auto_extension, which only fires on new connections.
        crate::embeddings::register_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../schema.sql")).unwrap();
        crate::migrations::run(&conn).unwrap();
        conn
    }

    fn insert_leaf(conn: &Connection, statement: &str) -> (String, String) {
        let ledger = Ledger::new(conn);
        let (b, v) = ledger
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
                    statement: statement.into(),
                    confidence: None,
                    reason: None,
                    editor: Editor::Ai,
                },
            })
            .unwrap();
        (b.id, v.id)
    }

    fn embed(conn: &Connection, belief_id: &str) {
        let blob = embeddings::vec_to_blob(&vec![0.1f32; embeddings::EMBEDDING_DIM]).unwrap();
        conn.execute(
            "INSERT INTO vec_beliefs (belief_id, embedding) VALUES (?1, ?2)",
            params![belief_id, blob],
        )
        .unwrap();
    }

    #[test]
    fn merge_then_undo_restores_absorbed_belief() {
        let conn = fresh_conn();
        let (a_id, a_ver) = insert_leaf(&conn, "Likes tea");
        let (b_id, b_ver) = insert_leaf(&conn, "Enjoys tea");
        embed(&conn, &a_id);
        embed(&conn, &b_id);

        // Give the soon-to-be-absorbed belief a provenance edge the keeper
        // should inherit (and lose again on undo).
        Ledger::new(&conn)
            .add_provenance(
                &b_ver,
                NewProvenance {
                    source_type: SourceType::Turn,
                    source_id: "turn-1".into(),
                    relation: ProvenanceRelation::ExtractedFrom,
                },
            )
            .unwrap();

        let status_of = |id: &str| -> String {
            conn.query_row("SELECT status FROM beliefs WHERE id=?1", params![id], |r| r.get(0))
                .unwrap()
        };
        let cur_ver = |id: &str| -> String {
            conn.query_row("SELECT current_version_id FROM beliefs WHERE id=?1", params![id], |r| {
                r.get(0)
            })
            .unwrap()
        };
        let in_vec = |id: &str| -> bool {
            conn.query_row("SELECT COUNT(*) FROM vec_beliefs WHERE belief_id=?1", params![id], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
                > 0
        };
        let ver_count = |id: &str| -> i64 {
            conn.query_row("SELECT COUNT(*) FROM belief_versions WHERE belief_id=?1", params![id], |r| {
                r.get(0)
            })
            .unwrap()
        };
        let a_edges = || -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM belief_provenance WHERE belief_version_id=?1",
                params![a_ver],
                |r| r.get(0),
            )
            .unwrap()
        };

        let b_prior_status = status_of(&b_id);
        let b_prior_ver = cur_ver(&b_id);
        let b_prior_vercount = ver_count(&b_id);
        let a_edges_before = a_edges();

        // Merge B into A.
        {
            let tx = conn.unchecked_transaction().unwrap();
            merge_in_tx(&tx, &a_id, &b_id, Some("dup"), "manual", Some(0.95)).unwrap();
            tx.commit().unwrap();
        }

        assert_eq!(status_of(&b_id), "corrected", "absorbed belief is tombstoned");
        assert!(!in_vec(&b_id), "absorbed belief leaves the vector index");
        assert!(in_vec(&a_id), "keeper stays indexed");
        assert_eq!(a_edges(), a_edges_before + 1, "keeper inherits the provenance edge");
        assert_eq!(ver_count(&b_id), b_prior_vercount + 1, "tombstone version was written");

        let merge_id: String = conn
            .query_row("SELECT id FROM belief_merges WHERE absorbed_id=?1", params![b_id], |r| {
                r.get(0)
            })
            .unwrap();

        // Undo.
        undo_merge_in_conn(&conn, &merge_id).unwrap();

        assert_eq!(status_of(&b_id), b_prior_status, "status restored");
        assert_eq!(cur_ver(&b_id), b_prior_ver, "current version restored");
        assert_eq!(ver_count(&b_id), b_prior_vercount, "tombstone version removed");
        assert!(in_vec(&b_id), "absorbed belief re-indexed");
        assert_eq!(a_edges(), a_edges_before, "copied edge removed from keeper");

        let reverted: Option<String> = conn
            .query_row("SELECT reverted_at FROM belief_merges WHERE id=?1", params![merge_id], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(reverted.is_some(), "merge marked reverted");

        // Undo is idempotent — a second call is a no-op.
        undo_merge_in_conn(&conn, &merge_id).unwrap();
    }

    #[test]
    fn pick_keeper_prefers_more_grounded_trust_class() {
        let conn = fresh_conn();
        // A is inferred, B asserted → B should be kept.
        let (a_id, _) = insert_leaf(&conn, "Probably likes tea");
        let ledger = Ledger::new(&conn);
        let (b, _) = ledger
            .insert_belief(NewBelief {
                subject: "user".into(),
                category: Some("preference".into()),
                status: Status::Asserted,
                trust_class: TrustClass::Asserted,
                scope: Scope::Global,
                scope_ref_id: None,
                level: 0,
                parent_summary_id: None,
                initial_version: NewVersion {
                    statement: "Likes tea".into(),
                    confidence: None,
                    reason: None,
                    editor: Editor::User,
                },
            })
            .unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        let (keeper, absorbed) = pick_keeper(&tx, &a_id, &b.id).unwrap();
        assert_eq!(keeper, b.id, "asserted belief survives");
        assert_eq!(absorbed, a_id);
    }
}
