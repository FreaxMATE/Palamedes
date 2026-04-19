use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS conversations (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    current_leaf_id TEXT
);

CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    parent_id TEXT REFERENCES messages(id),
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    branch_title TEXT,
    created_at TEXT NOT NULL,
    model TEXT,
    tokens_in INTEGER,
    tokens_out INTEGER,
    cost_micro_usd INTEGER
);

CREATE INDEX IF NOT EXISTS idx_messages_conv ON messages(conversation_id);
CREATE INDEX IF NOT EXISTS idx_messages_parent ON messages(parent_id);

CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

const DEFAULT_SYSTEM_PROMPT: &str =
    "You are Palamedes, a thoughtful personal assistant. Be concise, direct, and honest.";

pub struct Db {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub current_leaf_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub conversation_id: String,
    pub parent_id: Option<String>,
    pub role: String,
    pub content: String,
    pub branch_title: Option<String>,
    pub created_at: String,
    pub model: Option<String>,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    pub cost_micro_usd: Option<i64>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        conn.execute(
            "INSERT OR IGNORE INTO settings (key, value) VALUES (?1, ?2)",
            params!["system_prompt", DEFAULT_SYSTEM_PROMPT],
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn list_conversations(&self) -> Result<Vec<Conversation>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, title, created_at, updated_at, current_leaf_id
             FROM conversations ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Conversation {
                id: r.get(0)?,
                title: r.get(1)?,
                created_at: r.get(2)?,
                updated_at: r.get(3)?,
                current_leaf_id: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn create_conversation(&self, title: &str) -> Result<Conversation> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO conversations (id, title, created_at, updated_at, current_leaf_id)
             VALUES (?1, ?2, ?3, ?3, NULL)",
            params![id, title, now],
        )?;
        Ok(Conversation {
            id,
            title: title.to_string(),
            created_at: now.clone(),
            updated_at: now,
            current_leaf_id: None,
        })
    }

    pub fn delete_conversation(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM messages WHERE conversation_id = ?1", params![id])?;
        conn.execute("DELETE FROM conversations WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn rename_conversation(&self, id: &str, title: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE conversations SET title = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, title, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn get_messages(&self, conversation_id: &str) -> Result<Vec<Message>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, conversation_id, parent_id, role, content, branch_title,
                    created_at, model, tokens_in, tokens_out, cost_micro_usd
             FROM messages WHERE conversation_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![conversation_id], |r| {
            Ok(Message {
                id: r.get(0)?,
                conversation_id: r.get(1)?,
                parent_id: r.get(2)?,
                role: r.get(3)?,
                content: r.get(4)?,
                branch_title: r.get(5)?,
                created_at: r.get(6)?,
                model: r.get(7)?,
                tokens_in: r.get(8)?,
                tokens_out: r.get(9)?,
                cost_micro_usd: r.get(10)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn insert_message(
        &self,
        conversation_id: &str,
        parent_id: Option<&str>,
        role: &str,
        content: &str,
        model: Option<&str>,
    ) -> Result<Message> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO messages (id, conversation_id, parent_id, role, content,
                                   branch_title, created_at, model,
                                   tokens_in, tokens_out, cost_micro_usd)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, NULL, NULL, NULL)",
            params![id, conversation_id, parent_id, role, content, now, model],
        )?;
        conn.execute(
            "UPDATE conversations SET updated_at = ?2, current_leaf_id = ?3 WHERE id = ?1",
            params![conversation_id, now, id],
        )?;
        Ok(Message {
            id,
            conversation_id: conversation_id.to_string(),
            parent_id: parent_id.map(String::from),
            role: role.to_string(),
            content: content.to_string(),
            branch_title: None,
            created_at: now,
            model: model.map(String::from),
            tokens_in: None,
            tokens_out: None,
            cost_micro_usd: None,
        })
    }

    pub fn update_message_content(&self, id: &str, content: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE messages SET content = ?2 WHERE id = ?1",
            params![id, content],
        )?;
        Ok(())
    }

    /// Walk from `leaf_id` up to the root via parent_id, return root-first.
    pub fn get_path_to(&self, leaf_id: &str) -> Result<Vec<Message>> {
        let conn = self.conn.lock().unwrap();
        let mut path: Vec<Message> = Vec::new();
        let mut cursor: Option<String> = Some(leaf_id.to_string());
        while let Some(id) = cursor {
            let msg: Option<Message> = conn
                .query_row(
                    "SELECT id, conversation_id, parent_id, role, content, branch_title,
                            created_at, model, tokens_in, tokens_out, cost_micro_usd
                     FROM messages WHERE id = ?1",
                    params![id],
                    |r| {
                        Ok(Message {
                            id: r.get(0)?,
                            conversation_id: r.get(1)?,
                            parent_id: r.get(2)?,
                            role: r.get(3)?,
                            content: r.get(4)?,
                            branch_title: r.get(5)?,
                            created_at: r.get(6)?,
                            model: r.get(7)?,
                            tokens_in: r.get(8)?,
                            tokens_out: r.get(9)?,
                            cost_micro_usd: r.get(10)?,
                        })
                    },
                )
                .optional()?;
            match msg {
                Some(m) => {
                    cursor = m.parent_id.clone();
                    path.push(m);
                }
                None => break,
            }
        }
        path.reverse();
        Ok(path)
    }

    pub fn set_current_leaf(&self, conversation_id: &str, leaf_id: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE conversations SET current_leaf_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![conversation_id, leaf_id, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Find the deepest descendant of `message_id` by always following the most-recently-created child.
    /// Used when switching to a sibling: we jump to its newest tip rather than its subtree root.
    pub fn deepest_descendant(&self, message_id: &str) -> Result<String> {
        let conn = self.conn.lock().unwrap();
        let mut current = message_id.to_string();
        loop {
            let child: Option<String> = conn
                .query_row(
                    "SELECT id FROM messages WHERE parent_id = ?1
                     ORDER BY created_at DESC LIMIT 1",
                    params![current],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            match child {
                Some(c) => current = c,
                None => break,
            }
        }
        Ok(current)
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let value = conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(value)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}
