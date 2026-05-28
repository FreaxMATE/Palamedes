//! Chat pipeline — streaming chat, retrieval grounding, and the post-stream
//! ledger jobs that keep the belief corpus consistent.
//!
//! Surface (in approximate call order on a normal turn):
//! - **`send_message`** — persist the user turn, retrieve memories, render a
//!   grounded system prompt, stream the assistant reply, write recall receipts,
//!   and kick off background belief extraction.
//! - **`regenerate`** — same flow against an existing assistant turn, skipping
//!   extraction (the user prompt is unchanged so re-running would duplicate).
//! - **`cancel_stream`** — drop the cancellation token for an in-flight stream.
//!
//! Retrieval lives next door because every send/regenerate calls it:
//! - `retrieve_for_query` embeds the user prompt, fetches top-K beliefs above
//!   the user-configured cosine floor, and returns them for the system prompt.
//! - `render_memory_block` formats those beliefs as a prefix the model sees.
//! - `write_receipts` records *which* beliefs grounded *which* reply, so the
//!   audit panel can show "drew from" chips on every assistant message.
//!
//! Two adjacent background jobs round it out: `embed_unembedded_beliefs`
//! (idempotent embed backfill) and `regenerate_belief_labels` (cheap label
//! pass for the memory map). Both are belief-corpus housekeeping that benefits
//! from the same nebius client used by retrieval.

use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, State};
use tokio_util::sync::CancellationToken;

use crate::db::{Db, Message as DbMessage, RetrievedBelief};
use crate::embeddings;
use crate::nebius::{Message, NebiusClient, Role, StreamPiece};
use crate::{spawn_extraction, AppState, ExtractionSource};

// ---------- stream event payloads ----------

#[derive(Serialize, Clone)]
struct StreamChunk {
    stream_id: String,
    delta: String,
}

/// Reasoning chunks travel a separate event lane so the frontend can render
/// them in a collapsible "thinking" panel without contaminating the visible
/// content. Only emitted by reasoning models (Kimi K2.5, DeepSeek-V3.2).
#[derive(Serialize, Clone)]
struct StreamReasoning {
    stream_id: String,
    delta: String,
}

#[derive(Serialize, Clone)]
struct StreamDone {
    stream_id: String,
    message_id: String,
}

#[derive(Serialize, Clone)]
struct StreamError {
    stream_id: String,
    error: String,
}

// ---------- chat → nebius adapter ----------

fn to_nebius_messages(history: &[DbMessage], system_prompt: Option<String>) -> Vec<Message> {
    let mut messages: Vec<Message> = Vec::new();
    if let Some(sp) = system_prompt {
        if !sp.trim().is_empty() {
            messages.push(Message {
                role: Role::System,
                content: sp,
            });
        }
    }
    for m in history {
        let role = match m.role.as_str() {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "system" => Role::System,
            _ => Role::User,
        };
        messages.push(Message {
            role,
            content: m.content.clone(),
        });
    }
    messages
}

// ---------- streaming ----------

#[tauri::command]
pub async fn send_message(
    app: AppHandle,
    state: State<'_, AppState>,
    stream_id: String,
    conversation_id: String,
    parent_id: Option<String>,
    user_message_id: String,
    assistant_message_id: String,
    user_content: String,
) -> Result<(), String> {
    let base_system_prompt = state
        .db
        .get_setting("system_prompt")
        .map_err(|e| e.to_string())?;
    let model = state
        .db
        .get_setting("model")
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| "moonshotai/Kimi-K2.5".to_string());

    // 1. Persist the user message as a child of `parent_id`, using the id from the frontend.
    let user_msg = state
        .db
        .insert_message_with_id(
            &user_message_id,
            &conversation_id,
            parent_id.as_deref(),
            "user",
            &user_content,
            None,
        )
        .map_err(|e| e.to_string())?;

    // Retrieve relevant memories. Best-effort: if embeddings aren't ready yet,
    // this returns an empty list and we fall through to a bare prompt.
    let retrieved = retrieve_for_query(&state.db, &state.client, &user_content).await;
    let memory_block = render_memory_block(&retrieved);
    let system_prompt = base_system_prompt.map(|sp| {
        let date_line = format!("Today's date: {}.", chrono::Utc::now().format("%Y-%m-%d"));
        match &memory_block {
            Some(mem) => format!("{}\n\n{}\n{}", sp, mem, date_line),
            None => format!("{}\n\n{}", sp, date_line),
        }
    });

    // 2. Build the history: the path from root to (and including) this new user msg.
    let history = state
        .db
        .get_path_to(&user_msg.id)
        .map_err(|e| e.to_string())?;

    // 3. Create an empty assistant message as a child of the user message, with the frontend-provided id.
    let assistant_msg = state
        .db
        .insert_message_with_id(
            &assistant_message_id,
            &conversation_id,
            Some(&user_msg.id),
            "assistant",
            "",
            Some(&model),
        )
        .map_err(|e| e.to_string())?;

    // 4. Register cancellation + kick off the stream.
    let cancel = CancellationToken::new();
    state
        .active_streams
        .lock()
        .unwrap()
        .insert(stream_id.clone(), cancel.clone());

    let messages = to_nebius_messages(&history, system_prompt);
    let app_clone = app.clone();
    let emit_id = stream_id.clone();
    let assistant_id = assistant_msg.id.clone();

    let accumulated = std::sync::Arc::new(Mutex::new(String::new()));
    let accumulated_clone = accumulated.clone();

    let result = state
        .client
        .stream_chat(&model, messages, cancel, |piece| match piece {
            StreamPiece::Content(delta) => {
                accumulated_clone.lock().unwrap().push_str(&delta);
                let _ = app_clone.emit(
                    "stream_chunk",
                    StreamChunk {
                        stream_id: emit_id.clone(),
                        delta,
                    },
                );
            }
            StreamPiece::Reasoning(delta) => {
                let _ = app_clone.emit(
                    "stream_reasoning",
                    StreamReasoning {
                        stream_id: emit_id.clone(),
                        delta,
                    },
                );
            }
        })
        .await;

    state.active_streams.lock().unwrap().remove(&stream_id);

    // 5. Save whatever we accumulated (even on cancel) and emit terminal event.
    let final_content = accumulated.lock().unwrap().clone();
    let _ = state.db.update_message_content(&assistant_id, &final_content);

    // 6. Persist recall receipts: which memories grounded this reply.
    if result.is_ok() && !final_content.trim().is_empty() {
        write_receipts(&state.db, &assistant_id, &retrieved);
    }

    // 7. Kick off belief extraction in the background (fire-and-forget).
    //    Only on success, only if we have meaningful content.
    if result.is_ok() && !final_content.trim().is_empty() {
        spawn_extraction(
            state.db.clone(),
            state.client.clone(),
            model.clone(),
            ExtractionSource::Turn(user_msg.id.clone()),
            user_content.clone(),
            final_content.clone(),
        );
    }

    match result {
        Ok(()) => {
            let _ = app.emit(
                "stream_done",
                StreamDone {
                    stream_id: stream_id.clone(),
                    message_id: assistant_id,
                },
            );
            Ok(())
        }
        Err(e) => {
            let msg = e.to_string();
            let _ = app.emit(
                "stream_error",
                StreamError {
                    stream_id: stream_id.clone(),
                    error: msg.clone(),
                },
            );
            Err(msg)
        }
    }
}

#[tauri::command]
pub async fn regenerate(
    app: AppHandle,
    state: State<'_, AppState>,
    stream_id: String,
    assistant_message_id: String,
    new_assistant_message_id: String,
) -> Result<(), String> {
    // Find the assistant message → its parent is the user msg whose history we reuse.
    let path = state
        .db
        .get_path_to(&assistant_message_id)
        .map_err(|e| e.to_string())?;
    let assistant = path
        .last()
        .cloned()
        .ok_or_else(|| "assistant message not found".to_string())?;
    if assistant.role != "assistant" {
        return Err("can only regenerate assistant messages".into());
    }
    let parent_id = assistant
        .parent_id
        .clone()
        .ok_or_else(|| "assistant has no parent".to_string())?;
    let conversation_id = assistant.conversation_id.clone();

    let base_system_prompt = state
        .db
        .get_setting("system_prompt")
        .map_err(|e| e.to_string())?;
    let model = state
        .db
        .get_setting("model")
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| "moonshotai/Kimi-K2.5".to_string());

    // History = path from root to (and including) the user msg parent.
    let history = state.db.get_path_to(&parent_id).map_err(|e| e.to_string())?;

    // Re-retrieve memories against the original user prompt so the new reply
    // gets the same grounding as a fresh send_message would.
    let user_query = history
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.clone())
        .unwrap_or_default();
    let retrieved = retrieve_for_query(&state.db, &state.client, &user_query).await;
    let memory_block = render_memory_block(&retrieved);
    let system_prompt = base_system_prompt.map(|sp| {
        let date_line = format!("Today's date: {}.", chrono::Utc::now().format("%Y-%m-%d"));
        match &memory_block {
            Some(mem) => format!("{}\n\n{}\n{}", sp, mem, date_line),
            None => format!("{}\n\n{}", sp, date_line),
        }
    });

    // New assistant sibling.
    let new_assistant = state
        .db
        .insert_message_with_id(
            &new_assistant_message_id,
            &conversation_id,
            Some(&parent_id),
            "assistant",
            "",
            Some(&model),
        )
        .map_err(|e| e.to_string())?;

    let cancel = CancellationToken::new();
    state
        .active_streams
        .lock()
        .unwrap()
        .insert(stream_id.clone(), cancel.clone());

    let messages = to_nebius_messages(&history, system_prompt);
    let app_clone = app.clone();
    let emit_id = stream_id.clone();
    let new_assistant_id = new_assistant.id.clone();
    let accumulated = std::sync::Arc::new(Mutex::new(String::new()));
    let accumulated_clone = accumulated.clone();

    let result = state
        .client
        .stream_chat(&model, messages, cancel, |piece| match piece {
            StreamPiece::Content(delta) => {
                accumulated_clone.lock().unwrap().push_str(&delta);
                let _ = app_clone.emit(
                    "stream_chunk",
                    StreamChunk {
                        stream_id: emit_id.clone(),
                        delta,
                    },
                );
            }
            StreamPiece::Reasoning(delta) => {
                let _ = app_clone.emit(
                    "stream_reasoning",
                    StreamReasoning {
                        stream_id: emit_id.clone(),
                        delta,
                    },
                );
            }
        })
        .await;

    state.active_streams.lock().unwrap().remove(&stream_id);

    let final_content = accumulated.lock().unwrap().clone();
    let _ = state
        .db
        .update_message_content(&new_assistant_id, &final_content);

    if result.is_ok() && !final_content.trim().is_empty() {
        write_receipts(&state.db, &new_assistant_id, &retrieved);
    }

    // No belief extraction on regenerate: the user turn is identical to the one
    // already extracted from, so re-running would just duplicate beliefs.

    match result {
        Ok(()) => {
            let _ = app.emit(
                "stream_done",
                StreamDone {
                    stream_id: stream_id.clone(),
                    message_id: new_assistant_id,
                },
            );
            Ok(())
        }
        Err(e) => {
            let msg = e.to_string();
            let _ = app.emit(
                "stream_error",
                StreamError {
                    stream_id: stream_id.clone(),
                    error: msg.clone(),
                },
            );
            Err(msg)
        }
    }
}

#[tauri::command]
pub fn cancel_stream(state: State<'_, AppState>, stream_id: String) -> Result<(), String> {
    if let Some(token) = state.active_streams.lock().unwrap().remove(&stream_id) {
        token.cancel();
    }
    Ok(())
}

// ---------- retrieval ----------

const RETRIEVAL_K: usize = 10;

/// Format retrieved beliefs as a memory block to prepend to the system prompt.
/// Returns None if there are no retrievals — caller falls back to bare prompt.
fn render_memory_block(retrieved: &[RetrievedBelief]) -> Option<String> {
    if retrieved.is_empty() {
        return None;
    }
    let mut lines = String::from(
        "Memories that may be relevant to the current turn (use only if applicable; \
do not parrot them; the user can audit and correct any of these):\n",
    );
    for r in retrieved {
        let badge = match r.trust_class.as_str() {
            "asserted" => "🔒 asserted",
            "inferred" => "🧠 inferred",
            "hypothesized" => "❓ hypothesized",
            "summary" => "Σ summary",
            other => other,
        };
        lines.push_str(&format!("- [{}] {}\n", badge, r.statement));
    }
    Some(lines)
}

/// Retrieve top-K beliefs for a query string. Embeds the query against the
/// configured embedding model, normalizes, queries vec_beliefs, and drops
/// matches below the user-configured cosine threshold so off-topic turns
/// don't drag random memories into the system prompt. Failures (e.g. no
/// embeddings yet) silently return an empty list — retrieval is best-effort.
async fn retrieve_for_query(
    db: &Arc<Db>,
    client: &NebiusClient,
    query: &str,
) -> Vec<RetrievedBelief> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    let model = match db.get_setting("embedding_model").ok().flatten() {
        Some(m) => m,
        None => embeddings::DEFAULT_EMBEDDING_MODEL.to_string(),
    };
    let mut q = match client.embed_query(&model, query).await {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    if q.len() != embeddings::EMBEDDING_DIM {
        return Vec::new();
    }
    embeddings::normalize(&mut q);

    let min_cosine: f64 = db
        .get_setting("retrieval_min_cosine")
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(embeddings::DEFAULT_RETRIEVAL_MIN_COSINE);

    let mut hits = db.retrieve_top_k(&q, RETRIEVAL_K).unwrap_or_default();
    hits.retain(|r| embeddings::cosine_from_l2(r.distance) >= min_cosine);
    hits
}

/// Write recall_receipts rows linking a turn to the beliefs that grounded it,
/// and increment the materialized `belief_co_recall` table for each unordered
/// pair of beliefs co-recalled in this same turn. The co_recall table is what
/// the memory-map "two beliefs cited together" edges read from — keeping it up
/// to date incrementally avoids the O(receipts²) self-join on every map open.
fn write_receipts(db: &Db, turn_id: &str, retrieved: &[RetrievedBelief]) {
    if retrieved.is_empty() {
        return;
    }
    let now = chrono::Utc::now().to_rfc3339();
    let _ = db.with_conn(|conn| {
        let tx = conn.unchecked_transaction()?;
        for (i, r) in retrieved.iter().enumerate() {
            // Cosine similarity from L2 distance of normalized vectors.
            let weight = embeddings::cosine_from_l2(r.distance).max(0.0);
            tx.execute(
                "INSERT INTO recall_receipts (id, turn_id, belief_id, belief_version_id, weight, rank)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    uuid::Uuid::new_v4().to_string(),
                    turn_id,
                    r.belief_id,
                    r.version_id,
                    weight,
                    (i as i64) + 1,
                ],
            )?;
        }

        // Upsert one row per unordered pair of distinct beliefs in this turn.
        // The PRIMARY KEY check on (a < b) is enforced by the canonical-order
        // CHECK constraint added in migration 0004.
        for i in 0..retrieved.len() {
            for j in (i + 1)..retrieved.len() {
                let (a, b) = if retrieved[i].belief_id < retrieved[j].belief_id {
                    (&retrieved[i].belief_id, &retrieved[j].belief_id)
                } else {
                    (&retrieved[j].belief_id, &retrieved[i].belief_id)
                };
                if a == b {
                    continue; // can't happen given the distinct-id guarantee
                }
                tx.execute(
                    "INSERT INTO belief_co_recall (belief_a_id, belief_b_id, weight, last_seen)
                     VALUES (?1, ?2, 1, ?3)
                     ON CONFLICT (belief_a_id, belief_b_id) DO UPDATE
                       SET weight = weight + 1,
                           last_seen = excluded.last_seen",
                    params![a, b, now],
                )?;
            }
        }

        tx.commit()?;
        Ok(())
    });
}

// ---------- embeddings ----------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EmbedReport {
    pub embedded: i32,
    pub failed: i32,
    /// The first error message encountered, if any. Surfaced in the UI toast
    /// so misconfigured embedding models are diagnosable without diving into
    /// the SQLite logs.
    pub first_error: Option<String>,
    /// Embedding model that was used for this run.
    pub model: String,
}

#[tauri::command]
pub async fn embed_unembedded_beliefs(state: State<'_, AppState>) -> Result<EmbedReport, String> {
    let pending = state
        .db
        .list_unembedded_beliefs()
        .map_err(|e| e.to_string())?;

    let model = state
        .db
        .get_setting("embedding_model")
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| embeddings::DEFAULT_EMBEDDING_MODEL.to_string());

    let mut report = EmbedReport {
        model: model.clone(),
        ..Default::default()
    };

    if pending.is_empty() {
        return Ok(report);
    }

    for (id, statement) in pending {
        match state.client.embed(&model, &statement).await {
            Ok(mut v) => {
                if v.len() != embeddings::EMBEDDING_DIM {
                    report.failed += 1;
                    if report.first_error.is_none() {
                        report.first_error = Some(format!(
                            "dim mismatch: got {}, expected {}",
                            v.len(),
                            embeddings::EMBEDDING_DIM
                        ));
                    }
                    continue;
                }
                embeddings::normalize(&mut v);
                match state.db.upsert_embedding(&id, &v) {
                    Ok(()) => report.embedded += 1,
                    Err(e) => {
                        report.failed += 1;
                        if report.first_error.is_none() {
                            report.first_error = Some(format!("upsert failed: {}", e));
                        }
                    }
                }
            }
            Err(e) => {
                report.failed += 1;
                if report.first_error.is_none() {
                    report.first_error = Some(format!("{}", e));
                }
            }
        }
    }
    Ok(report)
}

// ---------- label backfill ----------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LabelBackfillReport {
    pub labeled: i32,
    pub failed: i32,
    pub first_error: Option<String>,
    pub model: String,
}

/// Generate a 1–4 word label for every unlabeled belief. Cheap LLM call per
/// belief; runs them in batches of 8 in parallel to keep wall-clock low.
#[tauri::command]
pub async fn regenerate_belief_labels(state: State<'_, AppState>) -> Result<LabelBackfillReport, String> {
    let pending = state
        .db
        .list_unlabeled_beliefs()
        .map_err(|e| e.to_string())?;

    // The label model is hardcoded inside `extract_label` (a small,
    // non-thinking instruct model). The chat-model setting is irrelevant
    // here — labels can't run through reasoning models. We still report
    // the chat model name for backwards-compat with the UI.
    let model = state
        .db
        .get_setting("model")
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| "moonshotai/Kimi-K2.5".to_string());

    let mut report = LabelBackfillReport {
        model,
        ..Default::default()
    };

    if pending.is_empty() {
        return Ok(report);
    }

    const BATCH: usize = 8;
    for chunk in pending.chunks(BATCH) {
        let futures = chunk.iter().map(|(id, statement)| {
            let c = state.client.clone();
            let db = state.db.clone();
            let id = id.clone();
            let statement = statement.clone();
            async move {
                let label = c
                    .extract_label(&statement)
                    .await
                    .map_err(|e| e.to_string())?;
                db.set_belief_label(&id, &label)
                    .map_err(|e| e.to_string())?;
                Ok(())
            }
        });
        let results = futures::future::join_all(futures).await;
        for r in results {
            match r {
                Ok(()) => report.labeled += 1,
                Err(e) => {
                    report.failed += 1;
                    if report.first_error.is_none() {
                        report.first_error = Some(e);
                    }
                }
            }
        }
    }

    Ok(report)
}
