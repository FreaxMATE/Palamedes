use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

const SCHEMA: &str = include_str!("../schema.sql");

fn truncate_title(s: &str) -> String {
    let single_line: String = s.lines().next().unwrap_or("").to_string();
    if single_line.chars().count() <= 40 {
        single_line
    } else {
        let truncated: String = single_line.chars().take(40).collect();
        format!("{}…", truncated)
    }
}

const DEFAULT_SYSTEM_PROMPT: &str = concat!(
    "You are Palamedes, a personal thinking tool.\n\n",
    "Tone and style:\n",
    "- Measured and analytical. Not breathless, not flattering.\n",
    "- Start with the substance, not with a greeting or acknowledgment.\n",
    "- Use adjectives sparingly. Do not say \"excellent\", \"impressive\", \"strong\", ",
    "\"sharply targeted\", or similar praise unless you can point to a specific, ",
    "verifiable reason.\n",
    "- When you disagree or see a problem, say so plainly and explain why.\n",
    "- When uncertain, say \"I'm not sure\" or \"I don't know\" rather than ",
    "guessing with confidence.\n\n",
    "Epistemic discipline:\n",
    "- Never claim something is a typo, error, or mistake without verifying it ",
    "against facts you actually have. If you can't verify, ask instead of asserting.\n",
    "- Distinguish what the user stated from what you inferred and what you are ",
    "speculating. Use phrasing like \"You wrote...\", \"I'm inferring...\", ",
    "\"A possibility is...\".\n",
    "- Cite specifics when making judgments. A concrete observation beats a ",
    "generic compliment.\n\n",
    "Format:\n",
    "- Default to prose. Use lists only when structure genuinely helps.\n",
    "- Keep responses proportional to the question. Do not pad."
);

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
        // Migration: add `label` column to existing `beliefs` rows. SQLite
        // has no `IF NOT EXISTS` for ADD COLUMN, so swallow the duplicate-
        // column error.
        if let Err(e) = conn.execute("ALTER TABLE beliefs ADD COLUMN label TEXT", []) {
            let msg = e.to_string();
            if !msg.contains("duplicate column name") {
                return Err(e.into());
            }
        }
        conn.execute(
            "INSERT OR IGNORE INTO settings (key, value) VALUES (?1, ?2)",
            params!["system_prompt", DEFAULT_SYSTEM_PROMPT],
        )?;
        // Migrate: if the user still has the legacy default prompt, upgrade it.
        // Any user-edited prompt is left untouched.
        const LEGACY_DEFAULT: &str =
            "You are Palamedes, a thoughtful personal assistant. Be concise, direct, and honest.";
        conn.execute(
            "UPDATE settings SET value = ?1
             WHERE key = 'system_prompt' AND value = ?2",
            params![DEFAULT_SYSTEM_PROMPT, LEGACY_DEFAULT],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO settings (key, value) VALUES (?1, ?2)",
            params!["model", "moonshotai/Kimi-K2.5"],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO settings (key, value) VALUES (?1, ?2)",
            params!["embedding_model", crate::embeddings::DEFAULT_EMBEDDING_MODEL],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO settings (key, value) VALUES (?1, ?2)",
            params![
                "dedup_cosine_threshold",
                crate::embeddings::DEFAULT_DEDUP_COSINE_THRESHOLD.to_string()
            ],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO settings (key, value) VALUES (?1, ?2)",
            params![
                "dedup_suggest_threshold",
                crate::embeddings::DEFAULT_SUGGEST_COSINE_THRESHOLD.to_string()
            ],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO settings (key, value) VALUES (?1, ?2)",
            params![
                "retrieval_min_cosine",
                crate::embeddings::DEFAULT_RETRIEVAL_MIN_COSINE.to_string()
            ],
        )?;
        // Migrate the previous default (Qwen3-Embedding-0.6B was a guess that
        // turned out not to be hosted on Nebius) to the actual SOTA option.
        // Any user-picked model is left alone.
        conn.execute(
            "UPDATE settings SET value = ?1
             WHERE key = 'embedding_model' AND value = 'Qwen/Qwen3-Embedding-0.6B'",
            params![crate::embeddings::DEFAULT_EMBEDDING_MODEL],
        )?;
        // Auto-drop vec_beliefs if the configured EMBEDDING_DIM has changed
        // from the existing table's dim. Probes by attempting a 1-dim insert
        // and dropping if the schema rejects it for a different reason than
        // dim mismatch... actually simpler: try inserting a zero-vector of
        // EMBEDDING_DIM and drop+recreate on dim error. Since we wrap with
        // IF NOT EXISTS, the schema's CREATE will then recreate at the new dim.
        let test_blob = crate::embeddings::vec_to_blob(&vec![0.0_f32; crate::embeddings::EMBEDDING_DIM])
            .expect("EMBEDDING_DIM must be valid");
        let probe = conn.execute(
            "INSERT INTO vec_beliefs (belief_id, embedding) VALUES (?1, ?2)",
            params!["__dim_probe__", test_blob],
        );
        match probe {
            Ok(_) => {
                // Probe succeeded — table is at the right dim. Clean up.
                let _ = conn.execute(
                    "DELETE FROM vec_beliefs WHERE belief_id = '__dim_probe__'",
                    [],
                );
            }
            Err(_) => {
                // Either dim mismatch or some other write error. Drop and let
                // the schema recreate fresh; the user will need to re-embed.
                conn.execute("DROP TABLE IF EXISTS vec_beliefs", [])?;
                conn.execute_batch(SCHEMA)?;
            }
        }
        // Migrate the previous default from DeepSeek back to Kimi after the
        // 2026-05-06 dogfood showed Kimi K2.5 produces materially better
        // chat completions and stops fabricating company names. A user-
        // picked model (anything other than the old default) is left alone.
        conn.execute(
            "UPDATE settings SET value = 'moonshotai/Kimi-K2.5'
             WHERE key = 'model' AND value = 'deepseek-ai/DeepSeek-V3.2'",
            [],
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

    /// Wipe every conversation + message, leaving the belief ledger,
    /// embeddings, and settings intact. Cascades clear `recall_receipts`
    /// and `extraction_log` via FK ON DELETE CASCADE; beliefs whose
    /// provenance pointed at deleted turns keep dangling source_ids
    /// (audit UI tolerates missing previews).
    pub fn wipe_chats(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM messages", [])?;
        tx.execute("DELETE FROM conversations", [])?;
        tx.commit()?;
        Ok(())
    }

    /// Nuke everything except `settings`: chats, beliefs, artifacts,
    /// embeddings, position cache, blocklist, logs. Settings (model,
    /// system prompt, embedding model, dedup thresholds) survive.
    pub fn wipe_all_data(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM recall_receipts", [])?;
        tx.execute("DELETE FROM extraction_log", [])?;
        tx.execute("DELETE FROM belief_provenance", [])?;
        tx.execute("DELETE FROM belief_versions", [])?;
        tx.execute("DELETE FROM belief_blocklist", [])?;
        tx.execute("DELETE FROM belief_positions", [])?;
        tx.execute("DELETE FROM vec_beliefs", [])?;
        tx.execute("DELETE FROM beliefs", [])?;
        tx.execute("DELETE FROM artifacts", [])?;
        tx.execute("DELETE FROM messages", [])?;
        tx.execute("DELETE FROM conversations", [])?;
        tx.commit()?;
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

    pub fn insert_message_with_id(
        &self,
        id: &str,
        conversation_id: &str,
        parent_id: Option<&str>,
        role: &str,
        content: &str,
        model: Option<&str>,
    ) -> Result<Message> {
        let now = Utc::now().to_rfc3339();
        let branch_title = if role == "user" {
            Some(truncate_title(content))
        } else {
            None
        };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO messages (id, conversation_id, parent_id, role, content,
                                   branch_title, created_at, model,
                                   tokens_in, tokens_out, cost_micro_usd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL, NULL)",
            params![id, conversation_id, parent_id, role, content, branch_title, now, model],
        )?;
        conn.execute(
            "UPDATE conversations SET updated_at = ?2, current_leaf_id = ?3 WHERE id = ?1",
            params![conversation_id, now, id],
        )?;
        Ok(Message {
            id: id.to_string(),
            conversation_id: conversation_id.to_string(),
            parent_id: parent_id.map(String::from),
            role: role.to_string(),
            content: content.to_string(),
            branch_title,
            created_at: now,
            model: model.map(String::from),
            tokens_in: None,
            tokens_out: None,
            cost_micro_usd: None,
        })
    }

    pub fn set_branch_title(&self, message_id: &str, title: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE messages SET branch_title = ?2 WHERE id = ?1",
            params![message_id, title],
        )?;
        Ok(())
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

    /// Persist the short keyword label generated for a belief by the LLM.
    /// Idempotent: pass the same label twice, no-op.
    pub fn set_belief_label(&self, belief_id: &str, label: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE beliefs SET label = ?1 WHERE id = ?2",
            params![label, belief_id],
        )?;
        Ok(())
    }

    /// Beliefs with no label yet — used by the regenerate-labels backfill.
    /// Returns (id, statement) so the caller can do one LLM call per row.
    pub fn list_unlabeled_beliefs(&self) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT b.id, bv.statement
             FROM beliefs b
             JOIN belief_versions bv ON bv.id = b.current_version_id
             WHERE (b.label IS NULL OR b.label = '')
               AND b.status NOT IN ('blocked','expired')
             ORDER BY b.created_at ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Collect active blocklist hints for the extraction prompt:
    ///   - every `belief_blocklist.pattern` (when set)
    ///   - for every `belief_blocklist.belief_id`, the current statement of that belief
    pub fn blocklist_hints(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut out: Vec<String> = Vec::new();

        let mut patterns_stmt = conn.prepare(
            "SELECT pattern FROM belief_blocklist WHERE pattern IS NOT NULL",
        )?;
        for row in patterns_stmt.query_map([], |r| r.get::<_, String>(0))? {
            out.push(row?);
        }

        let mut statements_stmt = conn.prepare(
            "SELECT bv.statement
             FROM belief_blocklist bl
             JOIN beliefs b           ON b.id = bl.belief_id
             JOIN belief_versions bv  ON bv.id = b.current_version_id
             WHERE bl.belief_id IS NOT NULL",
        )?;
        for row in statements_stmt.query_map([], |r| r.get::<_, String>(0))? {
            out.push(row?);
        }

        Ok(out)
    }

    pub fn log_extraction(
        &self,
        turn_id: Option<&str>,
        status: &str,
        model: Option<&str>,
        raw_output: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO extraction_log (id, turn_id, status, model, raw_output, error, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                uuid::Uuid::new_v4().to_string(),
                turn_id,
                status,
                model,
                raw_output,
                error,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Expose the connection for cross-module work (e.g. the Ledger). Callers
    /// must hold the lock for the duration of their use. Short critical sections.
    pub fn with_conn<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T>,
    {
        let conn = self.conn.lock().unwrap();
        f(&conn)
    }

    /// Upsert a single belief's embedding into the vec0 virtual table. The
    /// caller is responsible for L2-normalizing the vector beforehand.
    pub fn upsert_embedding(&self, belief_id: &str, vec: &[f32]) -> Result<()> {
        let blob = crate::embeddings::vec_to_blob(vec)?;
        let conn = self.conn.lock().unwrap();
        // vec0 doesn't support ON CONFLICT — delete-then-insert is the idiomatic
        // upsert. Cheap because belief_id is the PK.
        conn.execute("DELETE FROM vec_beliefs WHERE belief_id = ?1", params![belief_id])?;
        conn.execute(
            "INSERT INTO vec_beliefs (belief_id, embedding) VALUES (?1, ?2)",
            params![belief_id, blob],
        )?;
        Ok(())
    }

    /// Belief ids that have at least one version but no row in vec_beliefs.
    /// Used by the backfill job. Excludes blocked/expired beliefs since we
    /// will not retrieve those anyway.
    pub fn list_unembedded_beliefs(&self) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT b.id, bv.statement
             FROM beliefs b
             JOIN belief_versions bv ON bv.id = b.current_version_id
             WHERE b.status NOT IN ('expired','blocked')
               AND NOT EXISTS (SELECT 1 FROM vec_beliefs v WHERE v.belief_id = b.id)
             ORDER BY b.created_at ASC",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Top-K nearest beliefs by cosine distance (we store and query
    /// L2-normalized vectors, so sqlite-vec's L2 ranks identically to cosine).
    /// Filters out expired/blocked statuses.
    ///
    /// Returns rows of (belief_id, current_version_id, statement, trust_class,
    /// status, level, distance), ordered by distance ascending.
    pub fn retrieve_top_k(
        &self,
        query_vec: &[f32],
        k: usize,
    ) -> Result<Vec<RetrievedBelief>> {
        let blob = crate::embeddings::vec_to_blob(query_vec)?;
        let conn = self.conn.lock().unwrap();
        // Over-fetch from the vec table because the post-join filter may
        // remove blocked/expired rows. 3× is plenty for K up to ~30.
        let oversample = (k * 3).max(8);
        let mut stmt = conn.prepare(
            "SELECT v.belief_id, v.distance,
                    b.current_version_id, bv.statement, b.trust_class, b.status, b.level
             FROM (
                 SELECT belief_id, distance
                 FROM vec_beliefs
                 WHERE embedding MATCH ?1 AND k = ?2
                 ORDER BY distance
             ) v
             JOIN beliefs b ON b.id = v.belief_id
             JOIN belief_versions bv ON bv.id = b.current_version_id
             WHERE b.status NOT IN ('expired','blocked')
             ORDER BY v.distance ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![blob, oversample, k], |r| {
            Ok(RetrievedBelief {
                belief_id: r.get(0)?,
                distance: r.get(1)?,
                version_id: r.get(2)?,
                statement: r.get(3)?,
                trust_class: r.get(4)?,
                status: r.get(5)?,
                level: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub id: String,
    pub kind: String,
    pub title: Option<String>,
    pub content: Option<String>,
    pub source_url: Option<String>,
    pub created_at: String,
}

impl Db {
    pub fn insert_artifact(
        &self,
        kind: &str,
        title: Option<&str>,
        content: Option<&str>,
        source_url: Option<&str>,
    ) -> Result<Artifact> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO artifacts (id, kind, title, content, source_url, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, kind, title, content, source_url, now],
        )?;
        Ok(Artifact {
            id,
            kind: kind.to_string(),
            title: title.map(String::from),
            content: content.map(String::from),
            source_url: source_url.map(String::from),
            created_at: now,
        })
    }

    pub fn list_artifacts(&self) -> Result<Vec<Artifact>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, kind, title, content, source_url, created_at
             FROM artifacts ORDER BY created_at DESC LIMIT 50",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Artifact {
                id: r.get(0)?,
                kind: r.get(1)?,
                title: r.get(2)?,
                content: r.get(3)?,
                source_url: r.get(4)?,
                created_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievedBelief {
    pub belief_id: String,
    pub version_id: String,
    pub statement: String,
    pub trust_class: String,
    pub status: String,
    pub level: i32,
    pub distance: f64,
}

#[derive(Debug, Clone)]
pub struct DedupCandidate {
    pub belief_id: String,
    pub version_id: String,
    pub statement: String,
    pub status: String,
    pub distance: f64,
}

/// Find the single nearest existing belief (by L2 distance) to `query_vec`.
/// Unlike `Db::retrieve_top_k`, this does *not* filter by status — the caller
/// needs to see blocked/expired matches in order to drop blocked-near-dupes
/// silently rather than reinserting them.
pub fn find_top_dedup_match(
    conn: &Connection,
    query_vec: &[f32],
) -> Result<Option<DedupCandidate>> {
    let blob = crate::embeddings::vec_to_blob(query_vec)?;
    let row = conn
        .query_row(
            "SELECT v.belief_id, v.distance, b.current_version_id, bv.statement, b.status
             FROM (
                 SELECT belief_id, distance
                 FROM vec_beliefs
                 WHERE embedding MATCH ?1 AND k = 1
                 ORDER BY distance
             ) v
             JOIN beliefs b           ON b.id = v.belief_id
             JOIN belief_versions bv  ON bv.id = b.current_version_id
             LIMIT 1",
            params![blob],
            |r| {
                Ok(DedupCandidate {
                    belief_id: r.get(0)?,
                    distance: r.get(1)?,
                    version_id: r.get(2)?,
                    statement: r.get(3)?,
                    status: r.get(4)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}
