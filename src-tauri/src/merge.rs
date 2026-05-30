//! Merge candidates — duplicate detection, manual + automatic merging, and
//! reversible undo. The audit panel's "Duplicates" tab speaks to this module.
//!
//! Pair detection sweeps every embedded, active belief and finds nearest
//! neighbors above a *suggest* cosine threshold. Pairs at or above a
//! *dedup* threshold are tagged "definite" (eligible for background
//! auto-merge); the rest are "likely" (one-click review tier).
//!
//! Every merge — auto or manual — is logged to `belief_merges` with enough
//! data (prior status, prior version, copied provenance edges, absorbed
//! embedding) to **undo** without loss. That's what lets the audit panel
//! show a 24h "Recently merged" list with per-row Undo.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::db::Db;
use crate::embeddings;
use crate::ledger::{
    Editor, Ledger, NewProvenance, NewVersion, ProvenanceRelation, SourceType, Status,
};
use crate::AppState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeCandidate {
    pub a_id: String,
    pub a_statement: String,
    pub a_status: String,
    pub a_trust_class: String,
    pub b_id: String,
    pub b_statement: String,
    pub b_status: String,
    pub b_trust_class: String,
    pub cosine: f64,
    /// "definite" (>= dedup_cosine_threshold) or "likely" (>= suggest threshold).
    pub tier: String,
}

/// Sweep all embedded beliefs (excluding blocked/expired/corrected) and return
/// pairs whose cosine similarity is at or above `suggest_threshold`. Pairs at or
/// above `dedup_threshold` are tagged "definite" (auto-merge tier); the rest are
/// "likely" (review tier). O(N) MATCH queries; fine for personal corpora.
/// Shared by `list_merge_candidates` and the auto-merge sweep.
pub(crate) fn compute_merge_candidates(
    conn: &rusqlite::Connection,
    suggest_threshold: f64,
    dedup_threshold: f64,
) -> rusqlite::Result<Vec<MergeCandidate>> {
    // ids of every belief that has an embedding AND is in active status
    // (excludes blocked/expired/corrected). Corrected beliefs were previously
    // merged or marked wrong; don't resurface.
    let mut id_stmt = conn.prepare(
        "SELECT v.belief_id
         FROM vec_beliefs v
         JOIN beliefs b ON b.id = v.belief_id
         WHERE b.status NOT IN ('blocked','expired','corrected')",
    )?;
    let ids: Vec<String> = id_stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;

    // Pairs the user explicitly declared "not a duplicate". Stored ordered
    // (a < b), matching the pair_key we build below, so the lookup is direct.
    let dismissed: std::collections::HashSet<(String, String)> = conn
        .prepare("SELECT belief_a_id, belief_b_id FROM belief_merge_dismissals")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<_, _>>()?;

    // For each belief, fetch its embedding blob and find its top-5 nearest
    // neighbors. Dedup pairs (a, b) by ordering id strings.
    let mut emb_stmt = conn.prepare("SELECT embedding FROM vec_beliefs WHERE belief_id = ?1")?;
    let mut nn_stmt = conn.prepare(
        "SELECT v.belief_id, v.distance
         FROM (
             SELECT belief_id, distance
             FROM vec_beliefs
             WHERE embedding MATCH ?1 AND k = 6
             ORDER BY distance
         ) v
         JOIN beliefs b ON b.id = v.belief_id
         WHERE b.status NOT IN ('blocked','expired','corrected')",
    )?;

    // Cache (statement, status, trust_class) for each id we touch.
    type Meta = (String, String, String);
    let mut meta: std::collections::HashMap<String, Meta> = std::collections::HashMap::new();
    let mut load_meta = |id: &str| -> rusqlite::Result<Option<Meta>> {
        if let Some(m) = meta.get(id) {
            return Ok(Some(m.clone()));
        }
        let r: Option<Meta> = conn
            .query_row(
                "SELECT bv.statement, b.status, b.trust_class
                 FROM beliefs b JOIN belief_versions bv ON bv.id = b.current_version_id
                 WHERE b.id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some(ref m) = r {
            meta.insert(id.to_string(), m.clone());
        }
        Ok(r)
    };

    let mut seen_pairs: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    let mut out: Vec<MergeCandidate> = Vec::new();

    for id_a in &ids {
        let blob: Vec<u8> = match emb_stmt.query_row(params![id_a], |r| r.get(0)) {
            Ok(b) => b,
            Err(_) => continue, // no embedding for this id (shouldn't happen)
        };
        let mut rows = nn_stmt.query(params![blob])?;
        while let Some(row) = rows.next()? {
            let id_b: String = row.get(0)?;
            if id_b == *id_a {
                continue;
            }
            let dist: f64 = row.get(1)?;
            let cosine = embeddings::cosine_from_l2(dist);
            if cosine < suggest_threshold {
                continue;
            }
            let pair_key = if id_a < &id_b {
                (id_a.clone(), id_b.clone())
            } else {
                (id_b.clone(), id_a.clone())
            };
            if !seen_pairs.insert(pair_key.clone()) {
                continue;
            }
            if dismissed.contains(&pair_key) {
                continue; // user said these are not duplicates
            }

            let a_meta = match load_meta(id_a)? {
                Some(m) => m,
                None => continue,
            };
            let b_meta = match load_meta(&id_b)? {
                Some(m) => m,
                None => continue,
            };
            out.push(MergeCandidate {
                a_id: id_a.clone(),
                a_statement: a_meta.0,
                a_status: a_meta.1,
                a_trust_class: a_meta.2,
                b_id: id_b,
                b_statement: b_meta.0,
                b_status: b_meta.1,
                b_trust_class: b_meta.2,
                cosine,
                tier: if cosine >= dedup_threshold {
                    "definite".to_string()
                } else {
                    "likely".to_string()
                },
            });
        }
    }

    out.sort_by(|a, b| {
        b.cosine
            .partial_cmp(&a.cosine)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(out)
}

/// Read the two dedup cosine thresholds from settings, falling back to defaults.
pub(crate) fn dedup_thresholds(db: &Db) -> anyhow::Result<(f64, f64)> {
    let suggest = db
        .get_setting("dedup_suggest_threshold")?
        .and_then(|s| s.parse().ok())
        .unwrap_or(embeddings::DEFAULT_SUGGEST_COSINE_THRESHOLD);
    let dedup = db
        .get_setting("dedup_cosine_threshold")?
        .and_then(|s| s.parse().ok())
        .unwrap_or(embeddings::DEFAULT_DEDUP_COSINE_THRESHOLD);
    Ok((suggest, dedup))
}

#[tauri::command]
pub fn list_merge_candidates(state: State<'_, AppState>) -> Result<Vec<MergeCandidate>, String> {
    let (suggest, dedup) = dedup_thresholds(&state.db).map_err(|e| e.to_string())?;
    state
        .db
        .with_conn(|conn| Ok(compute_merge_candidates(conn, suggest, dedup)?))
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeBeliefsArgs {
    /// Belief that survives the merge.
    pub keeper_id: String,
    /// Belief that gets absorbed (marked corrected, embedding removed).
    pub absorbed_id: String,
    pub reason: Option<String>,
}

/// Absorb `absorbed_id` into `keeper_id` inside an open transaction, recording
/// the merge in `belief_merges` so it can be reversed. Mechanics:
/// 1. Copy every provenance edge on absorbed's current version to keeper's
///    current version (so keeper inherits all the source citations).
/// 2. Write a new version of absorbed with status='corrected', editor='user',
///    reason "merged into <keeper_id>: <user reason>".
/// 3. Add a `corrected_by` provenance edge from absorbed's new version to keeper.
/// 4. Delete absorbed's row from vec_beliefs (saving the embedding for undo) so
///    it stops surfacing in retrieval and merge-candidate sweeps.
/// 5. Log everything needed to reverse the merge into `belief_merges`.
pub(crate) fn merge_in_tx(
    tx: &rusqlite::Transaction<'_>,
    keeper_id: &str,
    absorbed_id: &str,
    reason: Option<&str>,
    kind: &str,
    cosine: Option<f64>,
) -> anyhow::Result<()> {
    let keeper_version_id: String = tx.query_row(
        "SELECT current_version_id FROM beliefs WHERE id = ?1",
        params![keeper_id],
        |r| r.get(0),
    )?;
    let (absorbed_version_id, absorbed_prior_status): (String, String) = tx.query_row(
        "SELECT current_version_id, status FROM beliefs WHERE id = ?1",
        params![absorbed_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let keeper_statement: String = tx.query_row(
        "SELECT bv.statement FROM beliefs b
         JOIN belief_versions bv ON bv.id = b.current_version_id WHERE b.id = ?1",
        params![keeper_id],
        |r| r.get(0),
    )?;
    let absorbed_statement: String = tx.query_row(
        "SELECT bv.statement FROM beliefs b
         JOIN belief_versions bv ON bv.id = b.current_version_id WHERE b.id = ?1",
        params![absorbed_id],
        |r| r.get(0),
    )?;

    // 1. Copy provenance edges from absorbed → keeper, skipping duplicates.
    // Track the ids we insert so undo can remove exactly these edges.
    let mut select_prov = tx.prepare(
        "SELECT source_type, source_id, relation
         FROM belief_provenance WHERE belief_version_id = ?1",
    )?;
    let edges: Vec<(String, String, String)> = select_prov
        .query_map(params![absorbed_version_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(select_prov);

    let mut copied_prov_ids: Vec<String> = Vec::new();
    for (source_type, source_id, relation) in edges {
        let already: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM belief_provenance
                 WHERE belief_version_id = ?1 AND source_type = ?2
                   AND source_id = ?3 AND relation = ?4 LIMIT 1",
                params![keeper_version_id, source_type, source_id, relation],
                |r| r.get(0),
            )
            .optional()?;
        if already.is_some() {
            continue;
        }
        let prov_id = uuid::Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO belief_provenance
               (id, belief_version_id, source_type, source_id, relation, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                prov_id,
                keeper_version_id,
                source_type,
                source_id,
                relation,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        copied_prov_ids.push(prov_id);
    }

    // 2. New version of absorbed: status=corrected, marks the merge.
    let user_reason = reason.unwrap_or("");
    let merge_reason = if user_reason.is_empty() {
        format!("merged into {}", keeper_id)
    } else {
        format!("merged into {}: {}", keeper_id, user_reason)
    };
    let ledger = Ledger::new(tx);
    let new_absorbed_version = ledger.add_version(
        absorbed_id,
        NewVersion {
            statement: absorbed_statement.clone(),
            // Tombstone version — confidence is structural, nothing to carry.
            confidence: None,
            reason: Some(merge_reason),
            editor: Editor::User,
        },
        Some(Status::Corrected),
    )?;

    // 3. corrected_by edge from absorbed's new version → keeper belief.
    ledger.add_provenance(
        &new_absorbed_version.id,
        NewProvenance {
            source_type: SourceType::Belief,
            source_id: keeper_id.to_string(),
            relation: ProvenanceRelation::CorrectedBy,
        },
    )?;

    // 4. Pull absorbed's embedding (for undo), then drop it from the index.
    let absorbed_embedding: Option<Vec<u8>> = tx
        .query_row(
            "SELECT embedding FROM vec_beliefs WHERE belief_id = ?1",
            params![absorbed_id],
            |r| r.get(0),
        )
        .optional()?;
    tx.execute(
        "DELETE FROM vec_beliefs WHERE belief_id = ?1",
        params![absorbed_id],
    )?;

    // 5. Record the merge so it can be shown in the digest and reversed.
    tx.execute(
        "INSERT INTO belief_merges
           (id, keeper_id, keeper_statement, absorbed_id, absorbed_statement,
            absorbed_prior_status, absorbed_prior_version_id, tombstone_version_id,
            copied_prov_ids, absorbed_embedding, cosine, kind, created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        params![
            uuid::Uuid::new_v4().to_string(),
            keeper_id,
            keeper_statement,
            absorbed_id,
            absorbed_statement,
            absorbed_prior_status,
            absorbed_version_id,
            new_absorbed_version.id,
            serde_json::to_string(&copied_prov_ids).unwrap_or_else(|_| "[]".into()),
            absorbed_embedding,
            cosine,
            kind,
            chrono::Utc::now().to_rfc3339(),
        ],
    )?;

    Ok(())
}

#[tauri::command]
pub fn merge_beliefs(state: State<'_, AppState>, args: MergeBeliefsArgs) -> Result<(), String> {
    if args.keeper_id == args.absorbed_id {
        return Err("cannot merge a belief into itself".into());
    }
    state
        .db
        .with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            merge_in_tx(
                &tx,
                &args.keeper_id,
                &args.absorbed_id,
                args.reason.as_deref(),
                "manual",
                None,
            )?;
            tx.commit()?;
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    let meta = serde_json::json!({
        "keeper_id": &args.keeper_id,
        "absorbed_id": &args.absorbed_id,
        "kind": "manual",
        "reason": &args.reason,
    })
    .to_string();
    crate::audit::log_best_effort(
        &state.audit,
        "belief.merge",
        "user",
        &format!("merge:{}<-{}", args.keeper_id, args.absorbed_id),
        Some(&meta),
    );
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DismissMergeArgs {
    pub a_id: String,
    pub b_id: String,
}

/// Record a candidate pair as "not a duplicate" so it stops surfacing in the
/// review list and is never auto-merged. Idempotent — re-dismissing is a no-op.
#[tauri::command]
pub fn dismiss_merge_candidate(
    state: State<'_, AppState>,
    args: DismissMergeArgs,
) -> Result<(), String> {
    if args.a_id == args.b_id {
        return Err("cannot dismiss a belief against itself".into());
    }
    // Store ordered so the key matches compute_merge_candidates' pair_key.
    let (a, b) = if args.a_id < args.b_id {
        (args.a_id, args.b_id)
    } else {
        (args.b_id, args.a_id)
    };
    state
        .db
        .with_conn(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO belief_merge_dismissals
                   (belief_a_id, belief_b_id, created_at)
                 VALUES (?1, ?2, ?3)",
                params![a, b, chrono::Utc::now().to_rfc3339()],
            )?;
            Ok(())
        })
        .map_err(|e| e.to_string())
}

/// Decide which of two near-duplicate beliefs survives a merge. Prefer the
/// more-grounded trust class, then the more-reinforced, then the older (more
/// established) belief; ids break ties for determinism. Returns
/// `(keeper_id, absorbed_id)`.
pub(crate) fn pick_keeper(
    tx: &rusqlite::Transaction<'_>,
    id_a: &str,
    id_b: &str,
) -> rusqlite::Result<(String, String)> {
    fn trust_rank(tc: &str) -> i32 {
        match tc {
            "asserted" => 3,
            "inferred" => 2,
            "hypothesized" => 1,
            _ => 0, // summary / unknown
        }
    }
    // (trust_class, created_at, reinforced_count)
    let load = |id: &str| -> rusqlite::Result<(String, String, i64)> {
        tx.query_row(
            "SELECT b.trust_class, b.created_at,
                    (SELECT COUNT(*) FROM belief_provenance bp
                       JOIN belief_versions bv ON bv.id = bp.belief_version_id
                      WHERE bv.belief_id = b.id AND bp.relation = 'reinforced_by')
             FROM beliefs b WHERE b.id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
    };
    let (a_tc, a_created, a_reinf) = load(id_a)?;
    let (b_tc, b_created, b_reinf) = load(id_b)?;

    // true => keep A
    let keep_a = match trust_rank(&a_tc).cmp(&trust_rank(&b_tc)) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => match b_reinf.cmp(&a_reinf) {
            std::cmp::Ordering::Greater => false,
            std::cmp::Ordering::Less => true,
            // older (smaller created_at) wins; id breaks final ties.
            std::cmp::Ordering::Equal => (a_created, id_a) <= (b_created, id_b),
        },
    };
    if keep_a {
        Ok((id_a.to_string(), id_b.to_string()))
    } else {
        Ok((id_b.to_string(), id_a.to_string()))
    }
}

/// Merge a batch of candidate pairs in one transaction. Skips a pair if either
/// member was already absorbed earlier in the batch (so chains collapse safely
/// rather than touching a tombstoned belief). Returns the number merged.
pub(crate) fn batch_merge(
    tx: &rusqlite::Transaction<'_>,
    candidates: &[MergeCandidate],
    kind: &str,
    reason: &str,
) -> anyhow::Result<usize> {
    let mut absorbed: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut n = 0usize;
    for c in candidates {
        if absorbed.contains(&c.a_id) || absorbed.contains(&c.b_id) {
            continue;
        }
        let (keeper, victim) = pick_keeper(tx, &c.a_id, &c.b_id)?;
        merge_in_tx(tx, &keeper, &victim, Some(reason), kind, Some(c.cosine))?;
        absorbed.insert(victim);
        n += 1;
    }
    Ok(n)
}

/// Auto-merge near-identical beliefs (the "definite" tier, at or above the
/// dedup threshold) in the background. Leaves summaries alone. Best-effort:
/// returns the number merged. Reusable from the extraction worker.
pub(crate) fn run_auto_merge(db: &Db) -> anyhow::Result<usize> {
    let (suggest, dedup) = dedup_thresholds(db)?;
    db.with_conn(|conn| {
        let candidates = compute_merge_candidates(conn, suggest, dedup)?;
        let definite: Vec<MergeCandidate> = candidates
            .into_iter()
            .filter(|c| {
                c.tier == "definite"
                    && c.a_trust_class != "summary"
                    && c.b_trust_class != "summary"
            })
            .collect();
        if definite.is_empty() {
            return Ok(0);
        }
        let tx = conn.unchecked_transaction()?;
        let n = batch_merge(&tx, &definite, "auto", "auto-merged near-duplicate")?;
        tx.commit()?;
        Ok(n)
    })
}

#[tauri::command]
pub fn auto_merge_duplicates(state: State<'_, AppState>) -> Result<usize, String> {
    run_auto_merge(&state.db).map_err(|e| e.to_string())
}

/// One-click batch for the review tier: merge every currently-surfaced
/// candidate pair (anything at or above the suggest threshold). Returns the
/// number merged.
#[tauri::command]
pub fn merge_all_candidates(state: State<'_, AppState>) -> Result<usize, String> {
    let (suggest, dedup) = dedup_thresholds(&state.db).map_err(|e| e.to_string())?;
    state
        .db
        .with_conn(|conn| {
            let candidates = compute_merge_candidates(conn, suggest, dedup)?;
            let mergeable: Vec<MergeCandidate> = candidates
                .into_iter()
                .filter(|c| c.a_trust_class != "summary" && c.b_trust_class != "summary")
                .collect();
            if mergeable.is_empty() {
                return Ok(0);
            }
            let tx = conn.unchecked_transaction()?;
            let n = batch_merge(&tx, &mergeable, "manual", "merged duplicate")?;
            tx.commit()?;
            Ok(n)
        })
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeRecord {
    pub id: String,
    pub keeper_id: String,
    pub keeper_statement: String,
    pub absorbed_id: String,
    pub absorbed_statement: String,
    pub cosine: Option<f64>,
    pub kind: String,
    pub created_at: String,
}

/// Recent, not-yet-reverted merges — the digest the audit panel shows with an
/// Undo affordance.
#[tauri::command]
pub fn recent_merges(state: State<'_, AppState>, limit: Option<i64>) -> Result<Vec<MergeRecord>, String> {
    let lim = limit.unwrap_or(20).clamp(1, 200);
    state
        .db
        .with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, keeper_id, keeper_statement, absorbed_id, absorbed_statement,
                        cosine, kind, created_at
                 FROM belief_merges
                 WHERE reverted_at IS NULL
                 ORDER BY created_at DESC
                 LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![lim], |r| {
                Ok(MergeRecord {
                    id: r.get(0)?,
                    keeper_id: r.get(1)?,
                    keeper_statement: r.get(2)?,
                    absorbed_id: r.get(3)?,
                    absorbed_statement: r.get(4)?,
                    cosine: r.get(5)?,
                    kind: r.get(6)?,
                    created_at: r.get(7)?,
                })
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .map_err(|e| e.to_string())
}

/// Reverse a merge inside an open connection. Restores the absorbed belief's
/// prior status + version, drops the tombstone version (cascading its
/// corrected_by edge), removes the provenance edges that were copied into the
/// keeper, and re-indexes the absorbed embedding. Unknown / already-reverted
/// ids are a no-op. Factored out of the command so it can be unit-tested.
pub(crate) fn undo_merge_in_conn(conn: &rusqlite::Connection, merge_id: &str) -> anyhow::Result<()> {
    let row: Option<(String, String, String, String, Option<Vec<u8>>)> = conn
        .query_row(
            "SELECT absorbed_id, absorbed_prior_status, absorbed_prior_version_id,
                    tombstone_version_id, absorbed_embedding
             FROM belief_merges WHERE id = ?1 AND reverted_at IS NULL",
            params![merge_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let (absorbed_id, prior_status, prior_version_id, tombstone_version_id, embedding) = match row {
        Some(v) => v,
        None => return Ok(()), // already reverted or unknown — no-op
    };
    let copied: String = conn.query_row(
        "SELECT copied_prov_ids FROM belief_merges WHERE id = ?1",
        params![merge_id],
        |r| r.get(0),
    )?;
    let copied_ids: Vec<String> = serde_json::from_str(&copied).unwrap_or_default();

    let tx = conn.unchecked_transaction()?;

    // 1. Remove the provenance edges copied into the keeper.
    for pid in &copied_ids {
        tx.execute("DELETE FROM belief_provenance WHERE id = ?1", params![pid])?;
    }

    // 2. Restore the absorbed belief to its pre-merge head + status, then
    //    delete the tombstone version (FK-cascades its edges).
    tx.execute(
        "UPDATE beliefs SET current_version_id = ?1, status = ?2, updated_at = ?3
         WHERE id = ?4",
        params![
            prior_version_id,
            prior_status,
            chrono::Utc::now().to_rfc3339(),
            absorbed_id
        ],
    )?;
    tx.execute(
        "DELETE FROM belief_versions WHERE id = ?1",
        params![tombstone_version_id],
    )?;

    // 3. Re-index the absorbed embedding if we kept it.
    if let Some(blob) = embedding {
        tx.execute(
            "INSERT OR REPLACE INTO vec_beliefs (belief_id, embedding) VALUES (?1, ?2)",
            params![absorbed_id, blob],
        )?;
    }

    // 4. Mark the merge reverted.
    tx.execute(
        "UPDATE belief_merges SET reverted_at = ?1 WHERE id = ?2",
        params![chrono::Utc::now().to_rfc3339(), merge_id],
    )?;

    tx.commit()?;
    Ok(())
}

/// Reverse a merge: restore the absorbed belief's prior status + version, drop
/// the tombstone version (cascading its corrected_by edge), remove the
/// provenance edges that were copied into the keeper, and re-index the absorbed
/// belief's embedding so it returns to retrieval and the map.
#[tauri::command]
pub fn undo_merge(state: State<'_, AppState>, merge_id: String) -> Result<(), String> {
    state
        .db
        .with_conn(|conn| {
            undo_merge_in_conn(conn, &merge_id)?;
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    crate::audit::log_best_effort(
        &state.audit,
        "belief.merge.undo",
        "user",
        &format!("undo:{}", merge_id),
        Some(&format!(r#"{{"merge_id":"{merge_id}"}}"#)),
    );
    Ok(())
}
