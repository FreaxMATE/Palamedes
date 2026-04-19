mod db;
mod nebius;

use db::{Conversation, Db, Message as DbMessage};
use nebius::{Message, NebiusClient, Role};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
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
    db: Db,
    active_streams: Mutex<HashMap<String, CancellationToken>>,
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

// ---------- streaming ----------

#[tauri::command]
async fn send_message(
    app: AppHandle,
    state: State<'_, AppState>,
    stream_id: String,
    conversation_id: String,
    parent_id: Option<String>,
    user_content: String,
) -> Result<(), String> {
    let system_prompt = state
        .db
        .get_setting("system_prompt")
        .map_err(|e| e.to_string())?;
    let model = state.client.model().to_string();

    // 1. Persist the user message as a child of `parent_id`.
    let user_msg = state
        .db
        .insert_message(
            &conversation_id,
            parent_id.as_deref(),
            "user",
            &user_content,
            None,
        )
        .map_err(|e| e.to_string())?;

    // 2. Build the history: the path from root to (and including) this new user msg.
    let history = state
        .db
        .get_path_to(&user_msg.id)
        .map_err(|e| e.to_string())?;

    // 3. Create an empty assistant message as a child of the user message.
    let assistant_msg = state
        .db
        .insert_message(
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
        .stream_chat(messages, cancel, |delta| {
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
fn cancel_stream(state: State<'_, AppState>, stream_id: String) -> Result<(), String> {
    if let Some(token) = state.active_streams.lock().unwrap().remove(&stream_id) {
        token.cancel();
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = dotenvy::dotenv();

    let client = NebiusClient::from_env()
        .expect("failed to init Nebius client — is NEBIUS_API_KEY set in .env?");

    tauri::Builder::default()
        .setup(move |app| {
            let data_dir = app
                .path()
                .app_data_dir()
                .expect("no app data dir");
            let db_path = data_dir.join("palamedes.db");
            let db = Db::open(&db_path).expect("failed to open SQLite db");

            app.manage(AppState {
                client,
                db,
                active_streams: Mutex::new(HashMap::new()),
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
            cancel_stream,
            list_conversations,
            create_conversation,
            delete_conversation,
            rename_conversation,
            get_messages,
            set_current_leaf,
            deepest_descendant,
            get_setting,
            set_setting,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
