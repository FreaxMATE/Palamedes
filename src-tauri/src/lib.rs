mod db;
mod embeddings;
mod extraction;
mod ledger;
mod nebius;
mod recap;
mod summarization;

use db::{Artifact, Conversation, Db, Message as DbMessage, RetrievedBelief};
use extraction::TurnContext;
use ledger::{Editor, Ledger, NewBelief, NewProvenance, NewVersion, ProvenanceRelation, Scope, SourceType, Status, TrustClass};
use nebius::{Message, NebiusClient, Role};
use rusqlite::params;
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

        match client.extract_beliefs(&model, ctx).await {
            Ok(outcome) => {
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

                let mut newly_inserted: Vec<(String, String)> = Vec::new();
                let write_result = db.with_conn(|conn| {
                    // All-or-nothing: a partial write would leave orphan beliefs
                    // without provenance, polluting the audit view.
                    let tx = conn.unchecked_transaction()?;
                    let ledger = Ledger::new(&tx);
                    for d in &outcome.drafts {
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
                        newly_inserted.push((belief.id.clone(), d.statement.clone()));
                    }
                    tx.commit()?;
                    Ok(())
                });

                match write_result {
                    Ok(()) => {
                        let _ = db.log_extraction(
                            source.turn_id_for_log(),
                            "ok",
                            Some(&model),
                            Some(&outcome.raw_response),
                            None,
                        );
                        // Embed each new belief. Failures are logged but do not
                        // affect the ledger — the backfill command can recover.
                        embed_beliefs(&db, &client, &newly_inserted).await;
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
            }
            Err(e) => {
                let _ = db.log_extraction(
                    source.turn_id_for_log(),
                    "failed",
                    Some(&model),
                    None,
                    Some(&e.to_string()),
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
        .unwrap_or_else(|| "deepseek-ai/DeepSeek-V3.2".to_string());

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
        .stream_chat(&model, messages, cancel, |delta| {
            accumulated_clone.lock().unwrap().push_str(delta);
            let _ = app_clone.emit(
                "stream_chunk",
                StreamChunk {
                    stream_id: emit_id.clone(),
                    delta: delta.to_string(),
                },
            );
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
        .unwrap_or_else(|| "deepseek-ai/DeepSeek-V3.2".to_string());

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
        .stream_chat(&model, messages, cancel, |delta| {
            accumulated_clone.lock().unwrap().push_str(delta);
            let _ = app_clone.emit(
                "stream_chunk",
                StreamChunk {
                    stream_id: emit_id.clone(),
                    delta: delta.to_string(),
                },
            );
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
/// configured embedding model, normalizes, queries vec_beliefs. Failures
/// (e.g. no embeddings yet) silently return an empty list — retrieval is
/// best-effort.
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
    db.retrieve_top_k(&q, RETRIEVAL_K).unwrap_or_default()
}

/// Write recall_receipts rows linking a turn to the beliefs that grounded it.
fn write_receipts(db: &Db, turn_id: &str, retrieved: &[RetrievedBelief]) {
    if retrieved.is_empty() {
        return;
    }
    let _ = db.with_conn(|conn| {
        let tx = conn.unchecked_transaction()?;
        for (i, r) in retrieved.iter().enumerate() {
            let weight = 1.0 - (r.distance / 2.0).min(1.0); // L2 of unit vectors ∈ [0, 2]
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

/// Embed each belief and write into vec_beliefs. Per-row failures are logged
/// to extraction_log so they're visible in the audit but never propagate.
async fn embed_beliefs(
    db: &Arc<Db>,
    client: &NebiusClient,
    beliefs: &[(String, String)],
) {
    if beliefs.is_empty() {
        return;
    }
    let model = db
        .get_setting("embedding_model")
        .ok()
        .flatten()
        .unwrap_or_else(|| embeddings::DEFAULT_EMBEDDING_MODEL.to_string());

    for (id, statement) in beliefs {
        match client.embed(&model, statement).await {
            Ok(mut v) => {
                if v.len() != embeddings::EMBEDDING_DIM {
                    let _ = db.log_extraction(
                        None,
                        "failed",
                        Some(&model),
                        None,
                        Some(&format!(
                            "embedding dim mismatch for belief {}: got {}, expected {}",
                            id,
                            v.len(),
                            embeddings::EMBEDDING_DIM
                        )),
                    );
                    continue;
                }
                embeddings::normalize(&mut v);
                if let Err(e) = db.upsert_embedding(id, &v) {
                    let _ = db.log_extraction(
                        None,
                        "failed",
                        Some(&model),
                        None,
                        Some(&format!("embedding write failed for {}: {}", id, e)),
                    );
                }
            }
            Err(e) => {
                let _ = db.log_extraction(
                    None,
                    "failed",
                    Some(&model),
                    None,
                    Some(&format!("embed call failed for {}: {}", id, e)),
                );
            }
        }
    }
}

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
                    created_at: r.get(10)?,
                    updated_at: r.get(11)?,
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
                        created_at: r.get(10)?,
                        updated_at: r.get(11)?,
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
        .unwrap_or_else(|| "deepseek-ai/DeepSeek-V3.2".to_string());

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
#[tauri::command]
fn generate_recap(
    state: State<'_, AppState>,
    date_iso: Option<String>,
) -> Result<RecapResponse, String> {
    let date = date_iso.unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d").to_string());

    // SQLite stores RFC3339 with timezone. Compare on the date prefix; for
    // the user's mental model "today" is local-time-today, but we accept the
    // small fudge factor near midnight rather than building a full tz join.
    let data = state
        .db
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
                date: date.clone(),
                user_messages_today,
                assistant_messages_today,
                conversations,
                beliefs,
            })
        })
        .map_err(|e| e.to_string())?;

    let markdown = recap::render_markdown(&data);

    let dir = state.data_dir.join("recaps");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{}.md", date));
    std::fs::write(&path, &markdown).map_err(|e| e.to_string())?;

    Ok(RecapResponse {
        date,
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
    use summarization::SummarizationContext;

    let model = state
        .db
        .get_setting("model")
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| "deepseek-ai/DeepSeek-V3.2".to_string());

    // Collect candidate (category -> Vec<(id, statement)>) groups with >= 2 entries.
    let groups: Vec<(String, Vec<(String, String)>)> = state
        .db
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

        let outcome = match state
            .client
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

        let category_for_log = category.clone();
        let summaries = outcome.summaries.clone();
        let write_result = state.db.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let ledger = Ledger::new(&tx);
            let mut written = 0;
            let mut covered = 0;
            let mut overlaps = 0;
            let mut hallucinated = 0;
            let mut claimed: std::collections::HashSet<String> =
                std::collections::HashSet::new();

            for s in &summaries {
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
                if valid_children.is_empty() {
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

                written += 1;
                covered += valid_children.len() as i32;
            }

            tx.commit()?;
            Ok((written, covered, overlaps, hallucinated))
        });

        match write_result {
            Ok((written, covered, overlaps, hallucinated)) => {
                report.summaries_created += written;
                report.beliefs_covered += covered;
                report.overlaps_dropped += overlaps;
                report.hallucinations_dropped += hallucinated;
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

            app.manage(AppState {
                client,
                db,
                active_streams: Mutex::new(HashMap::new()),
                data_dir,
            });

            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            send_message,
            regenerate,
            cancel_stream,
            list_conversations,
            create_conversation,
            delete_conversation,
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
            get_receipts_for_turn,
            generate_recap,
            list_recaps,
            capture_note,
            list_artifacts,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
