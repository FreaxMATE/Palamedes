mod nebius;

use nebius::{Message, NebiusClient, Role};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, State};
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
}

#[derive(Serialize, Clone)]
struct StreamError {
    stream_id: String,
    error: String,
}

pub struct AppState {
    client: NebiusClient,
    active_streams: Mutex<HashMap<String, CancellationToken>>,
}

fn to_nebius_messages(history: Vec<ChatMessage>, system_prompt: Option<String>) -> Vec<Message> {
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
            content: m.content,
        });
    }
    out
}

#[tauri::command]
async fn send_message(
    app: AppHandle,
    state: State<'_, AppState>,
    stream_id: String,
    history: Vec<ChatMessage>,
    system_prompt: Option<String>,
) -> Result<String, String> {
    let cancel = CancellationToken::new();
    state
        .active_streams
        .lock()
        .unwrap()
        .insert(stream_id.clone(), cancel.clone());

    let messages = to_nebius_messages(history, system_prompt);
    let app_clone = app.clone();
    let emit_id = stream_id.clone();

    let result = state
        .client
        .stream_chat(messages, cancel, |delta| {
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

    match result {
        Ok(()) => {
            let _ = app.emit(
                "stream_done",
                StreamDone {
                    stream_id: stream_id.clone(),
                },
            );
            Ok(stream_id)
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
    let state = AppState {
        client,
        active_streams: Mutex::new(HashMap::new()),
    };

    tauri::Builder::default()
        .manage(state)
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![send_message, cancel_stream])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
