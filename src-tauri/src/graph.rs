//! Memory map — 2D projection of the embedded belief corpus.
//!
//! Exposes everything the renderer needs to draw the starfield: a snapshot of
//! beliefs + typed edges (hierarchy, summarizes, reinforced_by, …), on-demand
//! kNN / co-recall edges, raw embedding vectors for client-side UMAP, and the
//! position cache (write path).
//!
//! All command bodies stay close to the DB so the renderer never has to
//! reconstruct shape it already has — `GraphBelief` carries the same coarse
//! confidence bucket the audit panel uses, so map opacity and ledger sort
//! order can't drift apart.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tauri::State;

use crate::confidence;
use crate::embeddings;
use crate::AppState;

/// What the renderer needs per belief: enough to draw + tooltip + click-through.
/// Position is None when the belief hasn't been projected yet (new since the
/// last UMAP run); the frontend collects those and triggers a re-projection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphBelief {
    pub id: String,
    pub statement: String,
    pub label: Option<String>,
    pub category: Option<String>,
    pub status: String,
    pub trust_class: String,
    pub level: i32,
    pub parent_summary_id: Option<String>,
    /// Structural score [0,1] — drives map opacity / cluster elevation.
    pub confidence: f64,
    /// Coarse bucket shown in hover/detail instead of a false-precise number.
    pub confidence_bucket: String,
    pub reinforced_count: i64,
    pub created_at: String,
    pub last_reinforced_at: Option<String>,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub has_embedding: bool,
}

/// Typed edge between two beliefs. `kind` matches the belief_provenance
/// relation vocabulary plus three synthetic kinds (`hierarchy`, `knn`,
/// `co_recall`) derived from other tables.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    pub source_id: String,
    pub target_id: String,
    pub kind: String,
    pub weight: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphSnapshot {
    pub beliefs: Vec<GraphBelief>,
    pub edges: Vec<GraphEdge>,
    pub projection_version: i64,
}

#[tauri::command]
pub fn get_graph_snapshot(state: State<'_, AppState>) -> Result<GraphSnapshot, String> {
    state
        .db
        .with_conn(|conn| {
            let projection_version: i64 = conn
                .query_row(
                    "SELECT COALESCE(MAX(projection_version), 0) FROM belief_positions",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);

            let mut stmt = conn.prepare(
                "SELECT b.id, bv.statement, b.label, b.category, b.status, b.trust_class,
                        b.level, b.parent_summary_id, bv.confidence,
                        (SELECT COUNT(*) FROM belief_provenance bp
                          JOIN belief_versions bv2 ON bv2.id = bp.belief_version_id
                          WHERE bv2.belief_id = b.id AND bp.relation = 'reinforced_by') AS reinforced_count,
                        (SELECT COUNT(*) FROM belief_provenance bp
                          JOIN belief_versions bv2 ON bv2.id = bp.belief_version_id
                          WHERE bv2.belief_id = b.id AND bp.relation = 'contradicted_by') AS contradicted_count,
                        b.num_times, b.created_at, b.last_reinforced_at, b.last_observed_at,
                        p.x, p.y,
                        EXISTS(SELECT 1 FROM vec_beliefs v WHERE v.belief_id = b.id) AS has_embedding
                 FROM beliefs b
                 JOIN belief_versions bv ON bv.id = b.current_version_id
                 LEFT JOIN belief_positions p ON p.belief_id = b.id
                 ORDER BY b.created_at ASC",
            )?;
            let rows = stmt.query_map([], |r| {
                let trust_class: String = r.get(5)?;
                let status: String = r.get(4)?;
                let stored: Option<f64> = r.get(8)?;
                let reinforced_count: i64 = r.get(9)?;
                let contradicted_count: i64 = r.get(10)?;
                let num_times: i64 = r.get(11)?;
                let created_at: String = r.get(12)?;
                let last_reinforced_at: Option<String> = r.get(13)?;
                let last_observed_at: Option<String> = r.get(14)?;
                // Map opacity / cluster elevation now reflect *structural*
                // confidence, not a self-reported number.
                let eff = confidence::effective_for(
                    &trust_class,
                    reinforced_count,
                    contradicted_count,
                    num_times,
                    status == "corrected",
                    &created_at,
                    last_reinforced_at.as_deref(),
                    last_observed_at.as_deref(),
                    stored,
                );
                Ok(GraphBelief {
                    id: r.get(0)?,
                    statement: r.get(1)?,
                    label: r.get(2)?,
                    category: r.get(3)?,
                    status,
                    trust_class,
                    level: r.get(6)?,
                    parent_summary_id: r.get(7)?,
                    confidence: eff.score,
                    confidence_bucket: eff.bucket.as_str().to_string(),
                    reinforced_count,
                    created_at,
                    last_reinforced_at,
                    x: r.get(15)?,
                    y: r.get(16)?,
                    has_embedding: r.get::<_, i64>(17)? != 0,
                })
            })?;
            let beliefs = rows.collect::<Result<Vec<_>, _>>()?;

            // Build the visible-id set so we don't emit edges to beliefs the
            // frontend won't draw (corrected/blocked/expired filter above).
            let visible: std::collections::HashSet<String> =
                beliefs.iter().map(|b| b.id.clone()).collect();
            let mut edges: Vec<GraphEdge> = Vec::new();

            // Hierarchy edges: leaf -> parent_summary_id (level 0 -> 1+).
            // Drawn child→parent so the renderer can curve them upward.
            let mut h_stmt = conn.prepare(
                "SELECT id, parent_summary_id
                 FROM beliefs
                 WHERE parent_summary_id IS NOT NULL",
            )?;
            let h_rows = h_stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in h_rows {
                let (child, parent) = row?;
                if visible.contains(&child) && visible.contains(&parent) {
                    edges.push(GraphEdge {
                        source_id: child,
                        target_id: parent,
                        kind: "hierarchy".to_string(),
                        weight: 1.0,
                    });
                }
            }

            // Provenance edges: belief→belief edges from the current version
            // of each belief. We collapse duplicates by (source, target, kind)
            // and weight by row count (matters for reinforced_by).
            let mut p_stmt = conn.prepare(
                "SELECT bv.belief_id  AS source_id,
                        bp.source_id  AS target_id,
                        bp.relation
                 FROM belief_provenance bp
                 JOIN belief_versions bv ON bv.id = bp.belief_version_id
                 JOIN beliefs b ON b.id = bv.belief_id
                 WHERE bp.source_type = 'belief'
                   AND bv.id = b.current_version_id",
            )?;
            let p_rows = p_stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            let mut seen: HashMap<(String, String, String), f64> = HashMap::new();
            for row in p_rows {
                let (src, tgt, rel) = row?;
                if !visible.contains(&src) || !visible.contains(&tgt) {
                    continue;
                }
                *seen.entry((src, tgt, rel)).or_insert(0.0) += 1.0;
            }
            for ((src, tgt, rel), w) in seen {
                edges.push(GraphEdge {
                    source_id: src,
                    target_id: tgt,
                    kind: rel,
                    weight: w,
                });
            }

            Ok(GraphSnapshot {
                beliefs,
                edges,
                projection_version,
            })
        })
        .map_err(|e| e.to_string())
}

/// One row per turn that retrieved this belief — used by the receipts
/// spotlight ("show me turns where this belief was cited").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnForBelief {
    pub turn_id: String,
    pub conversation_id: String,
    pub conversation_title: String,
    pub preview: String,
    pub created_at: String,
    pub weight: f64,
    pub rank: i64,
}

#[tauri::command]
pub fn get_turns_for_belief(
    state: State<'_, AppState>,
    belief_id: String,
) -> Result<Vec<TurnForBelief>, String> {
    state
        .db
        .with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT m.id, m.conversation_id, c.title, m.content, m.created_at,
                        r.weight, r.rank
                 FROM recall_receipts r
                 JOIN messages m       ON m.id = r.turn_id
                 JOIN conversations c  ON c.id = m.conversation_id
                 WHERE r.belief_id = ?1
                 ORDER BY m.created_at DESC
                 LIMIT 100",
            )?;
            let rows = stmt.query_map(params![belief_id], |r| {
                let content: String = r.get(3)?;
                let preview: String = content.chars().take(120).collect();
                Ok(TurnForBelief {
                    turn_id: r.get(0)?,
                    conversation_id: r.get(1)?,
                    conversation_title: r.get(2)?,
                    preview,
                    created_at: r.get(4)?,
                    weight: r.get(5)?,
                    rank: r.get(6)?,
                })
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .map_err(|e| e.to_string())
}

/// Heavier signals fetched on demand: top-k semantic neighbors for every
/// embedded belief, and pairs of beliefs cited together in the same turn
/// (`recall_receipts`). Off the cold path so the map opens fast.
#[tauri::command]
pub fn get_graph_edges_extended(
    state: State<'_, AppState>,
    knn_k: Option<usize>,
) -> Result<Vec<GraphEdge>, String> {
    let k = knn_k.unwrap_or(3).clamp(1, 8);
    state
        .db
        .with_conn(|conn| {
            let mut edges: Vec<GraphEdge> = Vec::new();

            // Co-recall edges: beliefs cited together in the same turn. Now
            // served from the materialized `belief_co_recall` table that the
            // chat pipeline maintains incrementally; no more O(receipts²)
            // self-join on every map open.
            let mut cr_stmt = conn.prepare(
                "SELECT cr.belief_a_id, cr.belief_b_id, cr.weight
                 FROM belief_co_recall cr
                 JOIN beliefs b1 ON b1.id = cr.belief_a_id
                 JOIN beliefs b2 ON b2.id = cr.belief_b_id
                 WHERE b1.status NOT IN ('blocked')
                   AND b2.status NOT IN ('blocked')",
            )?;
            let cr_rows = cr_stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)? as f64,
                ))
            })?;
            for row in cr_rows {
                let (a, b, w) = row?;
                edges.push(GraphEdge {
                    source_id: a,
                    target_id: b,
                    kind: "co_recall".to_string(),
                    weight: w,
                });
            }

            // kNN edges: for each embedded belief, the top-k nearest neighbors
            // by L2 over the normalized vectors (rank-equivalent to cosine).
            // We over-fetch k+1 (drop self) and skip self-loops by id.
            let mut ids_stmt = conn.prepare(
                "SELECT b.id FROM beliefs b
                 JOIN vec_beliefs v ON v.belief_id = b.id
                 WHERE b.status NOT IN ('blocked')",
            )?;
            let ids: Vec<String> = ids_stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;

            let mut emb_stmt = conn.prepare(
                "SELECT embedding FROM vec_beliefs WHERE belief_id = ?1",
            )?;
            let mut nn_stmt = conn.prepare(
                "SELECT v.belief_id, v.distance
                 FROM (
                     SELECT belief_id, distance
                     FROM vec_beliefs
                     WHERE embedding MATCH ?1 AND k = ?2
                     ORDER BY distance
                 ) v
                 JOIN beliefs b ON b.id = v.belief_id
                 WHERE b.status NOT IN ('blocked')
                 ORDER BY v.distance ASC",
            )?;

            let mut knn_seen: std::collections::HashSet<(String, String)> =
                std::collections::HashSet::new();

            for src in &ids {
                let blob: Option<Vec<u8>> = emb_stmt
                    .query_row(params![src], |r| r.get::<_, Vec<u8>>(0))
                    .optional()?;
                let Some(query_blob) = blob else { continue };

                // Pull k+2 to leave room for self + status filtering.
                let oversample = (k as i64) + 2;
                let nn_rows = nn_stmt.query_map(params![query_blob, oversample], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
                })?;

                let mut taken = 0usize;
                for row in nn_rows {
                    let (tgt, dist) = row?;
                    if &tgt == src {
                        continue;
                    }
                    // Symmetric undirected edge — store with min id first to dedupe.
                    let pair = if src < &tgt {
                        (src.clone(), tgt.clone())
                    } else {
                        (tgt.clone(), src.clone())
                    };
                    if knn_seen.contains(&pair) {
                        continue;
                    }
                    knn_seen.insert(pair.clone());

                    // Convert L2 on unit vectors to cosine ∈ [-1, 1] for weight.
                    let cosine = embeddings::cosine_from_l2(dist);
                    edges.push(GraphEdge {
                        source_id: pair.0,
                        target_id: pair.1,
                        kind: "knn".to_string(),
                        weight: cosine,
                    });
                    taken += 1;
                    if taken >= k {
                        break;
                    }
                }
            }

            Ok(edges)
        })
        .map_err(|e| e.to_string())
}

/// Return raw embedding vectors for the requested belief ids. Used by the
/// frontend before running UMAP. We ship vectors as Vec<f32> (JSON arrays);
/// for ~1000 beliefs at 4096-dim that's ~16MB JSON over Tauri IPC, which is
/// fine for an on-demand projection trigger.
#[tauri::command]
pub fn get_belief_embeddings(
    state: State<'_, AppState>,
    belief_ids: Vec<String>,
) -> Result<Vec<(String, Vec<f32>)>, String> {
    if belief_ids.is_empty() {
        return Ok(Vec::new());
    }
    state
        .db
        .with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT embedding FROM vec_beliefs WHERE belief_id = ?1",
            )?;
            let mut out: Vec<(String, Vec<f32>)> = Vec::with_capacity(belief_ids.len());
            for id in &belief_ids {
                let blob: Option<Vec<u8>> = stmt
                    .query_row(params![id], |r| r.get::<_, Vec<u8>>(0))
                    .optional()?;
                let Some(bytes) = blob else { continue };
                if bytes.len() != embeddings::EMBEDDING_DIM * 4 {
                    continue;
                }
                let mut v: Vec<f32> = Vec::with_capacity(embeddings::EMBEDDING_DIM);
                for chunk in bytes.chunks_exact(4) {
                    let mut buf = [0u8; 4];
                    buf.copy_from_slice(chunk);
                    v.push(f32::from_le_bytes(buf));
                }
                out.push((id.clone(), v));
            }
            Ok(out)
        })
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Deserialize)]
pub struct PositionUpdate {
    pub belief_id: String,
    pub x: f64,
    pub y: f64,
}

/// Replace the position cache with a new projection. The version bumps
/// monotonically so old rows that didn't make it into this projection
/// remain identifiable as stale (though we just delete them outright here).
#[tauri::command]
pub fn save_belief_positions(
    state: State<'_, AppState>,
    positions: Vec<PositionUpdate>,
) -> Result<i64, String> {
    state
        .db
        .with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let next_version: i64 = tx
                .query_row(
                    "SELECT COALESCE(MAX(projection_version), 0) + 1 FROM belief_positions",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(1);
            // Wipe existing positions; we do full re-projections, not partial.
            tx.execute("DELETE FROM belief_positions", [])?;
            let now = chrono::Utc::now().to_rfc3339();
            for p in &positions {
                tx.execute(
                    "INSERT INTO belief_positions
                       (belief_id, x, y, projection_version, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![p.belief_id, p.x, p.y, next_version, now],
                )?;
            }
            tx.commit()?;
            Ok(next_version)
        })
        .map_err(|e| e.to_string())
}
