mod db;
mod embeddings;
mod extraction;
mod ledger;
mod mcp;
mod nebius;
mod recap;
mod summarization;

use db::{find_top_dedup_match, Artifact, Conversation, Db, Message as DbMessage, RetrievedBelief};
use extraction::TurnContext;
use ledger::{Editor, Ledger, NewBelief, NewProvenance, NewVersion, ProvenanceRelation, Scope, SourceType, Status, TrustClass};
use nebius::{Message, NebiusClient, Role, StreamPiece};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

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
enum ExtractionSource {
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
/// to `extraction_log`.
fn spawn_extraction(
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

                // Genuinely new belief.
                let trust_class = match d.trust_class.as_str() {
                    "asserted" => TrustClass::Asserted,
                    _ => TrustClass::Inferred,
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
                        confidence: d.confidence,
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

fn to_nebius_messages(history: &[DbMessage], system_prompt: Option<String>) -> Vec<Message> {
    let mut out = Vec::with_capacity(history.len() + 1);
    if let Some(sp) = system_prompt {
        out.push(Message {
            role: Role::System,
            content: sp,
        });
    }
    for m in history {
        let role = match m.role.as_str() {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "system" => Role::System,
            _ => Role::User,
        };
        out.push(Message {
            role,
            content: m.content.clone(),
        });
    }
    out
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

// ---------- streaming ----------

#[tauri::command]
async fn send_message(
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
async fn regenerate(
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
fn cancel_stream(state: State<'_, AppState>, stream_id: String) -> Result<(), String> {
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

/// Write recall_receipts rows linking a turn to the beliefs that grounded it.
fn write_receipts(db: &Db, turn_id: &str, retrieved: &[RetrievedBelief]) {
    if retrieved.is_empty() {
        return;
    }
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
async fn embed_unembedded_beliefs(state: State<'_, AppState>) -> Result<EmbedReport, String> {
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
async fn regenerate_belief_labels(state: State<'_, AppState>) -> Result<LabelBackfillReport, String> {
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

// ---------- audit (Belief Ledger) ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditBelief {
    pub id: String,
    pub statement: String,
    pub confidence: f64,
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
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionItem {
    pub version_num: i32,
    pub statement: String,
    pub confidence: f64,
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
                        b.created_at, b.updated_at
                 FROM beliefs b
                 JOIN belief_versions bv ON bv.id = b.current_version_id
                 ORDER BY b.updated_at DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(AuditBelief {
                    id: r.get(0)?,
                    statement: r.get(1)?,
                    confidence: r.get(2)?,
                    category: r.get(3)?,
                    status: r.get(4)?,
                    trust_class: r.get(5)?,
                    level: r.get(6)?,
                    parent_summary_id: r.get(7)?,
                    provenance_count: r.get(8)?,
                    version_count: r.get(9)?,
                    reinforced_count: r.get(10)?,
                    created_at: r.get(11)?,
                    updated_at: r.get(12)?,
                })
            })?;
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
                        b.created_at, b.updated_at
                 FROM beliefs b
                 JOIN belief_versions bv ON bv.id = b.current_version_id
                 WHERE b.id = ?1",
                params![id],
                |r| {
                    Ok(AuditBelief {
                        id: r.get(0)?,
                        statement: r.get(1)?,
                        confidence: r.get(2)?,
                        category: r.get(3)?,
                        status: r.get(4)?,
                        trust_class: r.get(5)?,
                        level: r.get(6)?,
                        parent_summary_id: r.get(7)?,
                        provenance_count: r.get(8)?,
                        version_count: r.get(9)?,
                        reinforced_count: r.get(10)?,
                        created_at: r.get(11)?,
                        updated_at: r.get(12)?,
                    })
                },
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
                        confidence: r.get(3)?,
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
                    v.provenance.push(ProvenanceItem {
                        source_type,
                        source_id,
                        relation,
                        preview,
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
    pub new_confidence: Option<f64>,
    pub reason: Option<String>,
    pub blocklist_pattern: Option<String>,
}

#[tauri::command]
fn update_belief(state: State<'_, AppState>, args: UpdateBeliefArgs) -> Result<(), String> {
    state
        .db
        .with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;

            // Load current version so optional fields default to existing values.
            let (cur_statement, cur_confidence): (String, f64) = tx.query_row(
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
                    confidence: args.new_confidence.unwrap_or(cur_confidence),
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
                "SELECT bv.statement, b.category, bv.confidence, b.status, b.trust_class
                 FROM beliefs b
                 JOIN belief_versions bv ON bv.id = b.current_version_id
                 WHERE substr(b.created_at, 1, 10) = ?1
                 ORDER BY
                    CASE b.status WHEN 'inferred' THEN 0 ELSE 1 END,
                    bv.confidence DESC",
            )?;
            let beliefs: Vec<recap::BeliefRow> = beliefs_stmt
                .query_map(params![date], |r| {
                    Ok(recap::BeliefRow {
                        statement: r.get(0)?,
                        category: r.get(1)?,
                        confidence: r.get(2)?,
                        status: r.get(3)?,
                        trust_class: r.get(4)?,
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

// ---------- memory map (2D projection of embeddings) ----------

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
    pub confidence: f64,
    pub reinforced_count: i64,
    pub created_at: String,
    pub last_reinforced_at: Option<String>,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub has_embedding: bool,
}

/// Typed edge between two beliefs. `kind` matches the belief_provenance
/// relation vocabulary plus two synthetic kinds (`hierarchy`, `knn`,
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
fn get_graph_snapshot(state: State<'_, AppState>) -> Result<GraphSnapshot, String> {
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
                        b.created_at, b.last_reinforced_at,
                        p.x, p.y,
                        EXISTS(SELECT 1 FROM vec_beliefs v WHERE v.belief_id = b.id) AS has_embedding
                 FROM beliefs b
                 JOIN belief_versions bv ON bv.id = b.current_version_id
                 LEFT JOIN belief_positions p ON p.belief_id = b.id
                 ORDER BY b.created_at ASC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(GraphBelief {
                    id: r.get(0)?,
                    statement: r.get(1)?,
                    label: r.get(2)?,
                    category: r.get(3)?,
                    status: r.get(4)?,
                    trust_class: r.get(5)?,
                    level: r.get(6)?,
                    parent_summary_id: r.get(7)?,
                    confidence: r.get(8)?,
                    reinforced_count: r.get(9)?,
                    created_at: r.get(10)?,
                    last_reinforced_at: r.get(11)?,
                    x: r.get(12)?,
                    y: r.get(13)?,
                    has_embedding: r.get::<_, i64>(14)? != 0,
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
fn get_turns_for_belief(
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
fn get_graph_edges_extended(
    state: State<'_, AppState>,
    knn_k: Option<usize>,
) -> Result<Vec<GraphEdge>, String> {
    let k = knn_k.unwrap_or(3).clamp(1, 8);
    state
        .db
        .with_conn(|conn| {
            let mut edges: Vec<GraphEdge> = Vec::new();

            // Co-recall edges: beliefs that appear together in the same turn's
            // recall_receipts. Symmetric, so emit (a < b) once.
            let mut cr_stmt = conn.prepare(
                "SELECT r1.belief_id, r2.belief_id, COUNT(*) AS w
                 FROM recall_receipts r1
                 JOIN recall_receipts r2
                   ON r1.turn_id = r2.turn_id
                  AND r1.belief_id < r2.belief_id
                 JOIN beliefs b1 ON b1.id = r1.belief_id
                 JOIN beliefs b2 ON b2.id = r2.belief_id
                 WHERE b1.status NOT IN ('blocked')
                   AND b2.status NOT IN ('blocked')
                 GROUP BY r1.belief_id, r2.belief_id",
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
fn get_belief_embeddings(
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
fn save_belief_positions(
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

// ---------- merge candidates ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeCandidate {
    pub a_id: String,
    pub a_statement: String,
    pub a_status: String,
    pub a_trust_class: String,
    pub a_confidence: f64,
    pub b_id: String,
    pub b_statement: String,
    pub b_status: String,
    pub b_trust_class: String,
    pub b_confidence: f64,
    pub cosine: f64,
    /// "definite" (>= dedup_cosine_threshold) or "likely" (>= suggest threshold).
    pub tier: String,
}

/// Sweep all embedded beliefs (excluding blocked/expired) and surface pairs
/// whose cosine similarity is at or above the suggest threshold. The dedup
/// auto-merge threshold marks one tier; pairs below that are surfaced as
/// "review me" candidates. O(N) MATCH queries; fine for personal corpora.
#[tauri::command]
fn list_merge_candidates(state: State<'_, AppState>) -> Result<Vec<MergeCandidate>, String> {
    let dedup_threshold: f64 = state
        .db
        .get_setting("dedup_cosine_threshold")
        .map_err(|e| e.to_string())?
        .and_then(|s| s.parse().ok())
        .unwrap_or(embeddings::DEFAULT_DEDUP_COSINE_THRESHOLD);
    let suggest_threshold: f64 = state
        .db
        .get_setting("dedup_suggest_threshold")
        .map_err(|e| e.to_string())?
        .and_then(|s| s.parse().ok())
        .unwrap_or(embeddings::DEFAULT_SUGGEST_COSINE_THRESHOLD);

    state
        .db
        .with_conn(|conn| {
            // First: ids of every belief that has an embedding AND is in
            // active status (excludes blocked/expired/corrected). Corrected
            // beliefs were previously merged or marked wrong; don't resurface.
            let mut id_stmt = conn.prepare(
                "SELECT v.belief_id
                 FROM vec_beliefs v
                 JOIN beliefs b ON b.id = v.belief_id
                 WHERE b.status NOT IN ('blocked','expired','corrected')",
            )?;
            let ids: Vec<String> = id_stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;

            // For each belief, fetch its embedding blob and find its top-5
            // nearest neighbors. Dedup pairs (a, b) by ordering id strings.
            let mut emb_stmt = conn.prepare(
                "SELECT embedding FROM vec_beliefs WHERE belief_id = ?1",
            )?;
            let mut nn_stmt = conn.prepare(
                "SELECT v.belief_id, v.distance, bv.statement, b.status, b.trust_class, bv.confidence
                 FROM (
                     SELECT belief_id, distance
                     FROM vec_beliefs
                     WHERE embedding MATCH ?1 AND k = 6
                     ORDER BY distance
                 ) v
                 JOIN beliefs b ON b.id = v.belief_id
                 JOIN belief_versions bv ON bv.id = b.current_version_id
                 WHERE b.status NOT IN ('blocked','expired','corrected')",
            )?;

            // Cache the (statement, status, trust_class, confidence) for each id we touch.
            type Meta = (String, String, String, f64);
            let mut meta: std::collections::HashMap<String, Meta> = std::collections::HashMap::new();
            let mut load_meta = |id: &str| -> rusqlite::Result<Option<Meta>> {
                if let Some(m) = meta.get(id) {
                    return Ok(Some(m.clone()));
                }
                let r: Option<Meta> = conn
                    .query_row(
                        "SELECT bv.statement, b.status, b.trust_class, bv.confidence
                         FROM beliefs b JOIN belief_versions bv ON bv.id = b.current_version_id
                         WHERE b.id = ?1",
                        params![id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
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
                        a_confidence: a_meta.3,
                        b_id: id_b,
                        b_statement: b_meta.0,
                        b_status: b_meta.1,
                        b_trust_class: b_meta.2,
                        b_confidence: b_meta.3,
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
        })
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

/// Absorb `absorbed_id` into `keeper_id`. Mechanics:
/// 1. Copy every provenance edge on absorbed's current version to keeper's
///    current version (so keeper inherits all the source citations).
/// 2. Write a new version of absorbed with status='corrected', editor='user',
///    reason "merged into <keeper_id>: <user reason>".
/// 3. Add a `corrected_by` provenance edge from absorbed's new version to
///    the keeper belief.
/// 4. Delete absorbed's row from vec_beliefs so it stops surfacing in
///    retrieval and merge-candidate sweeps.
#[tauri::command]
fn merge_beliefs(state: State<'_, AppState>, args: MergeBeliefsArgs) -> Result<(), String> {
    if args.keeper_id == args.absorbed_id {
        return Err("cannot merge a belief into itself".into());
    }
    state
        .db
        .with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;

            let keeper_version_id: String = tx.query_row(
                "SELECT current_version_id FROM beliefs WHERE id = ?1",
                params![args.keeper_id],
                |r| r.get(0),
            )?;
            let absorbed_version_id: String = tx.query_row(
                "SELECT current_version_id FROM beliefs WHERE id = ?1",
                params![args.absorbed_id],
                |r| r.get(0),
            )?;

            // 1. Copy provenance edges from absorbed → keeper. Skip
            // self-references and any duplicate (same source_type+source_id+relation).
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

            for (source_type, source_id, relation) in edges {
                let already: Option<i64> = tx
                    .query_row(
                        "SELECT 1 FROM belief_provenance
                         WHERE belief_version_id = ?1
                           AND source_type = ?2
                           AND source_id = ?3
                           AND relation = ?4
                         LIMIT 1",
                        params![keeper_version_id, source_type, source_id, relation],
                        |r| r.get(0),
                    )
                    .optional()?;
                if already.is_some() {
                    continue;
                }
                tx.execute(
                    "INSERT INTO belief_provenance
                       (id, belief_version_id, source_type, source_id, relation, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        uuid::Uuid::new_v4().to_string(),
                        keeper_version_id,
                        source_type,
                        source_id,
                        relation,
                        chrono::Utc::now().to_rfc3339(),
                    ],
                )?;
            }

            // 2. New version of absorbed: status=corrected, marks the merge.
            let user_reason = args.reason.as_deref().unwrap_or("");
            let merge_reason = if user_reason.is_empty() {
                format!("merged into {}", args.keeper_id)
            } else {
                format!("merged into {}: {}", args.keeper_id, user_reason)
            };
            let ledger = Ledger::new(&tx);
            let absorbed_keeper_statement: String = tx.query_row(
                "SELECT bv.statement FROM beliefs b
                 JOIN belief_versions bv ON bv.id = b.current_version_id
                 WHERE b.id = ?1",
                params![args.absorbed_id],
                |r| r.get(0),
            )?;
            let absorbed_confidence: f64 = tx.query_row(
                "SELECT bv.confidence FROM beliefs b
                 JOIN belief_versions bv ON bv.id = b.current_version_id
                 WHERE b.id = ?1",
                params![args.absorbed_id],
                |r| r.get(0),
            )?;
            let new_absorbed_version = ledger.add_version(
                &args.absorbed_id,
                NewVersion {
                    statement: absorbed_keeper_statement,
                    confidence: absorbed_confidence,
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
                    source_id: args.keeper_id.clone(),
                    relation: ProvenanceRelation::CorrectedBy,
                },
            )?;

            // 4. Drop absorbed from the vector index.
            tx.execute(
                "DELETE FROM vec_beliefs WHERE belief_id = ?1",
                params![args.absorbed_id],
            )?;

            tx.commit()?;
            Ok(())
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
                        confidence: s.confidence,
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
    confidence: Option<f64>,
    trust_class: Option<String>,
) -> Result<String, String> {
    let tc = match trust_class.as_deref() {
        None => None,
        Some(s) => Some(ledger::TrustClass::from_str(s).map_err(|e| e.to_string())?),
    };
    let override_ = if statement.is_some() || category.is_some() || confidence.is_some() || tc.is_some() {
        Some(mcp::proposals::AcceptOverride {
            statement,
            category,
            confidence,
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
