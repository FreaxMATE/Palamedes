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
        migrate_provenance_check_constraint(&conn)?;
        migrate_belief_versions_confidence_nullable(&conn)?;
        // Numbered-migration runner. Records the baseline as v1 on first
        // run; from v2 onward, schema changes ship as files under
        // ../migrations/ and entries in `MIGRATIONS` in migrations.rs.
        crate::migrations::run(&conn)?;
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
        // MCP server (Phase C). Off by default — local-first contract.
        // Token is generated on first enable; we don't seed one so a
        // disabled server has no credential lying around.
        conn.execute(
            "INSERT OR IGNORE INTO settings (key, value) VALUES (?1, ?2)",
            params!["mcp_server_enabled", "false"],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO settings (key, value) VALUES (?1, ?2)",
            params!["mcp_server_port", "5180"],
        )?;
        // Per-MCP-client read budget over a sliding 24h window. The audit
        // module reads this on every call; see src/mcp/audit.rs. Default
        // 1000 — high enough for a normal agent session, low enough that
        // a malicious client can't enumerate the whole ledger overnight.
        conn.execute(
            "INSERT OR IGNORE INTO settings (key, value) VALUES (?1, ?2)",
            params![
                "mcp_read_limit_per_day",
                crate::mcp::audit::DEFAULT_READ_LIMIT_PER_DAY.to_string()
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
        // Detect a vec_beliefs dim mismatch (the configured EMBEDDING_DIM
        // changed from what was last embedded). Old behavior: silently
        // DROP the table — that destroyed every existing embedding without
        // warning. New behavior: preserve the old vectors in a regular
        // vec_beliefs_legacy_<dim> table, then drop + recreate the virtual
        // table at the new dim. The UI sees the pending-swap state via
        // `get_embedding_swap_state` and surfaces it; the existing
        // `embed_unembedded_beliefs` background loop will re-embed the
        // user's beliefs at the new dim.
        let test_blob = crate::embeddings::vec_to_blob(&vec![0.0_f32; crate::embeddings::EMBEDDING_DIM])
            .expect("EMBEDDING_DIM must be valid");
        let probe = conn.execute(
            "INSERT INTO vec_beliefs (belief_id, embedding) VALUES (?1, ?2)",
            params!["__dim_probe__", test_blob],
        );
        match probe {
            Ok(_) => {
                let _ = conn.execute(
                    "DELETE FROM vec_beliefs WHERE belief_id = '__dim_probe__'",
                    [],
                );
            }
            Err(_) => {
                archive_and_recreate_vec_beliefs(&conn)?;
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

    /// `with_conn` variant for callers that need a `&mut Connection` —
    /// e.g. anything that starts a `conn.transaction()`. Same lock
    /// semantics as `with_conn`.
    pub fn with_conn_mut<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T>,
    {
        let mut conn = self.conn.lock().unwrap();
        f(&mut conn)
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
    /// Currently unused at consumer sites — kept on the row so future debug /
    /// inspection paths have the human-readable claim without re-querying.
    #[allow(dead_code)]
    pub statement: String,
    pub status: String,
    pub distance: f64,
}

/// Parse the column dim out of a `vec_beliefs` CREATE statement.
/// Looks for `FLOAT[N]` (the syntax sqlite-vec writes for vec0 columns)
/// and returns `N`. Returns `None` if the table doesn't exist or the
/// statement doesn't include a recognisable dim.
fn introspect_vec_beliefs_dim(conn: &Connection) -> Option<usize> {
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='vec_beliefs'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()?;
    // sqlite-vec stores something like `embedding FLOAT[4096]`. Pull the
    // first integer inside square brackets.
    let open = sql.find('[')?;
    let close = sql[open..].find(']')?;
    let inside = &sql[open + 1..open + close];
    inside.trim().parse::<usize>().ok()
}

/// Handle a vec_beliefs dim mismatch detected at startup. Preserves the
/// old embeddings in a `vec_beliefs_legacy_<dim>` regular table so the
/// user can roll back, drops the old virtual table, and recreates
/// `vec_beliefs` at the new dim by re-running the schema bundle. Records
/// pending-swap state in `settings` so the UI can prompt the user.
///
/// After this runs, `vec_beliefs` is empty at the new dim. The existing
/// `embed_unembedded_beliefs` background pass will refill it from the
/// `beliefs` table on its own.
fn archive_and_recreate_vec_beliefs(conn: &Connection) -> Result<()> {
    let old_dim = introspect_vec_beliefs_dim(conn).unwrap_or(0);
    let new_dim = crate::embeddings::EMBEDDING_DIM;
    let belief_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM beliefs", [], |r| r.get(0))
        .unwrap_or(0);
    let now = Utc::now().to_rfc3339();
    let legacy_table = format!("vec_beliefs_legacy_{}", old_dim);

    // Archive — best-effort. Skip silently if the source table is in a
    // shape we can't copy from (e.g. corruption). The user keeps the
    // belief rows themselves regardless; only the cached vectors are at
    // risk, and they were going to be wiped under the old behavior too.
    if old_dim > 0 {
        let create_legacy = format!(
            "CREATE TABLE IF NOT EXISTS {} (
                 belief_id TEXT PRIMARY KEY,
                 embedding BLOB
             )",
            legacy_table
        );
        let _ = conn.execute(&create_legacy, []);
        let copy = format!(
            "INSERT OR IGNORE INTO {} (belief_id, embedding)
             SELECT belief_id, embedding FROM vec_beliefs",
            legacy_table
        );
        let _ = conn.execute(&copy, []);
    }

    conn.execute("DROP TABLE IF EXISTS vec_beliefs", [])?;
    conn.execute_batch(SCHEMA)?;

    let upsert = |k: &str, v: &str| -> Result<()> {
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
            params![k, v],
        )?;
        Ok(())
    };
    upsert("embedding_swap_pending", "1")?;
    upsert("embedding_swap_old_dim", &old_dim.to_string())?;
    upsert("embedding_swap_new_dim", &new_dim.to_string())?;
    upsert("embedding_swap_belief_count", &belief_count.to_string())?;
    upsert("embedding_swap_legacy_table", &legacy_table)?;
    upsert("embedding_swap_detected_at", &now)?;

    eprintln!(
        "[palamedes] embedding dim swap: old={} new={}. {} belief(s) need re-embedding. \
         Old vectors archived to {}.",
        old_dim, new_dim, belief_count, legacy_table
    );
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingSwapState {
    pub old_dim: usize,
    pub new_dim: usize,
    pub belief_count: i64,
    pub legacy_table: String,
    pub detected_at: String,
}

impl Db {
    /// `Some(state)` while a previously-detected dim swap is awaiting user
    /// acknowledgement; `None` once dismissed via
    /// [`Self::confirm_embedding_swap`] or [`Self::dismiss_embedding_swap`].
    pub fn get_embedding_swap_state(&self) -> Result<Option<EmbeddingSwapState>> {
        let conn = self.conn.lock().unwrap();
        let pending: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key='embedding_swap_pending'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if pending.as_deref() != Some("1") {
            return Ok(None);
        }
        let read = |k: &str| -> Result<String> {
            conn.query_row(
                "SELECT value FROM settings WHERE key=?1",
                params![k],
                |r| r.get::<_, String>(0),
            )
            .map_err(Into::into)
        };
        Ok(Some(EmbeddingSwapState {
            old_dim: read("embedding_swap_old_dim")?.parse().unwrap_or(0),
            new_dim: read("embedding_swap_new_dim")?.parse().unwrap_or(0),
            belief_count: read("embedding_swap_belief_count")?.parse().unwrap_or(0),
            legacy_table: read("embedding_swap_legacy_table")?,
            detected_at: read("embedding_swap_detected_at")?,
        }))
    }

    /// User accepted the swap: drop the legacy archive and clear the
    /// pending-state flags. (The empty `vec_beliefs` is already in place;
    /// the background `embed_unembedded_beliefs` pass refills it.)
    pub fn confirm_embedding_swap(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let legacy: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key='embedding_swap_legacy_table'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(table) = legacy {
            // Cheap sanity check before constructing dynamic SQL — the
            // legacy table name is generated by us, but a paranoid check
            // costs nothing.
            if table.starts_with("vec_beliefs_legacy_")
                && table[19..].chars().all(|c| c.is_ascii_digit())
            {
                let drop_sql = format!("DROP TABLE IF EXISTS {}", table);
                conn.execute(&drop_sql, [])?;
            }
        }
        clear_swap_flags(&conn)?;
        Ok(())
    }

    /// User dismissed the warning without re-embedding. Keeps the legacy
    /// archive on disk for manual recovery; just clears the flag so the
    /// banner doesn't keep popping up.
    pub fn dismiss_embedding_swap(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        clear_swap_flags(&conn)?;
        Ok(())
    }
}

fn clear_swap_flags(conn: &Connection) -> Result<()> {
    for k in [
        "embedding_swap_pending",
        "embedding_swap_old_dim",
        "embedding_swap_new_dim",
        "embedding_swap_belief_count",
        "embedding_swap_legacy_table",
        "embedding_swap_detected_at",
    ] {
        conn.execute("DELETE FROM settings WHERE key = ?1", params![k])?;
    }
    Ok(())
}

/// Phase C migration: extend `belief_provenance.source_type` CHECK to accept
/// `'proposal'` and `'mcp_client'`. SQLite can't ALTER a CHECK constraint, so
/// we rename-table-dance on existing DBs. Idempotent: if the constraint
/// already includes `'mcp_client'`, this is a no-op. Returns whether the
/// migration ran.
fn migrate_provenance_check_constraint(conn: &Connection) -> Result<bool> {
    let existing_sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='belief_provenance'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    let Some(sql) = existing_sql else {
        return Ok(false); // fresh DB: schema.sql already wrote the new CHECK
    };
    if sql.contains("'mcp_client'") {
        return Ok(false);
    }
    conn.execute_batch(
        r#"
        BEGIN;
        CREATE TABLE belief_provenance_new (
            id                TEXT PRIMARY KEY,
            belief_version_id TEXT NOT NULL REFERENCES belief_versions(id) ON DELETE CASCADE,
            source_type       TEXT NOT NULL CHECK (source_type IN
                                ('turn','artifact','belief','proposal','mcp_client')),
            source_id         TEXT NOT NULL,
            relation          TEXT NOT NULL CHECK (relation IN
                                ('extracted_from','reinforced_by','contradicted_by',
                                 'corrected_by','summarizes')),
            created_at        TEXT NOT NULL
        );
        INSERT INTO belief_provenance_new
            (id, belief_version_id, source_type, source_id, relation, created_at)
        SELECT id, belief_version_id, source_type, source_id, relation, created_at
        FROM belief_provenance;
        DROP TABLE belief_provenance;
        ALTER TABLE belief_provenance_new RENAME TO belief_provenance;
        CREATE INDEX IF NOT EXISTS idx_provenance_version
            ON belief_provenance(belief_version_id);
        CREATE INDEX IF NOT EXISTS idx_provenance_source
            ON belief_provenance(source_type, source_id);
        COMMIT;
        "#,
    )?;
    Ok(true)
}

/// Drop the `NOT NULL` constraint on `belief_versions.confidence` for existing
/// DBs. Confidence is no longer stored for leaf beliefs (it is derived
/// structurally at read time — see `confidence.rs`), so new versions insert
/// NULL. SQLite can't ALTER a column's nullability in place, so we rebuild the
/// table with the rename-table dance.
///
/// `belief_versions` is FK-referenced by `belief_provenance` (ON DELETE CASCADE)
/// and `recall_receipts`, so we MUST disable foreign keys for the rebuild —
/// otherwise dropping the table would cascade-delete every provenance row and
/// violate the receipts FK. `PRAGMA foreign_keys` is a no-op inside a
/// transaction, so it is toggled outside the BEGIN/COMMIT.
fn migrate_belief_versions_confidence_nullable(conn: &Connection) -> Result<bool> {
    let existing_sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='belief_versions'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    let Some(sql) = existing_sql else {
        return Ok(false); // fresh DB: schema.sql already wrote the nullable column
    };
    if sql.contains("confidence IS NULL") {
        return Ok(false); // already migrated
    }

    conn.execute_batch("PRAGMA foreign_keys=OFF;")?;
    let rebuild = conn.execute_batch(
        r#"
        BEGIN;
        CREATE TABLE belief_versions_new (
            id          TEXT PRIMARY KEY,
            belief_id   TEXT NOT NULL REFERENCES beliefs(id) ON DELETE CASCADE,
            version_num INTEGER NOT NULL,
            statement   TEXT    NOT NULL,
            confidence  REAL    CHECK (confidence IS NULL OR (confidence >= 0.0 AND confidence <= 1.0)),
            reason      TEXT,
            editor      TEXT    NOT NULL CHECK (editor IN ('user','ai','system')),
            created_at  TEXT    NOT NULL,
            UNIQUE (belief_id, version_num)
        );
        INSERT INTO belief_versions_new
            (id, belief_id, version_num, statement, confidence, reason, editor, created_at)
        SELECT id, belief_id, version_num, statement, confidence, reason, editor, created_at
        FROM belief_versions;
        DROP TABLE belief_versions;
        ALTER TABLE belief_versions_new RENAME TO belief_versions;
        CREATE INDEX IF NOT EXISTS idx_belief_versions_belief ON belief_versions(belief_id);
        COMMIT;
        "#,
    );
    // Restore foreign-key enforcement regardless of whether the rebuild
    // succeeded, then surface any error.
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    rebuild?;
    Ok(true)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The pre-Phase-C belief_provenance CREATE statement, copied verbatim
    /// from schema.sql before this commit. Lets us simulate an existing
    /// dogfood DB and prove the rename-table dance migrates rows safely.
    /// Includes a stub `belief_versions` because the new table's FK target
    /// is validated when foreign_keys is on (which is the schema default).
    const OLD_BELIEF_PROVENANCE_SCHEMA: &str = r#"
        CREATE TABLE belief_versions (id TEXT PRIMARY KEY);
        CREATE TABLE belief_provenance (
            id                TEXT PRIMARY KEY,
            belief_version_id TEXT NOT NULL,
            source_type       TEXT NOT NULL CHECK (source_type IN ('turn','artifact','belief')),
            source_id         TEXT NOT NULL,
            relation          TEXT NOT NULL CHECK (relation IN
                                ('extracted_from','reinforced_by','contradicted_by',
                                 'corrected_by','summarizes')),
            created_at        TEXT NOT NULL
        );
        CREATE INDEX idx_provenance_version ON belief_provenance(belief_version_id);
        CREATE INDEX idx_provenance_source  ON belief_provenance(source_type, source_id);
    "#;

    #[test]
    fn migration_preserves_rows_and_accepts_new_source_types() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(OLD_BELIEF_PROVENANCE_SCHEMA).unwrap();

        // Seed belief_versions with the ids we'll reference. In a real
        // dogfood DB these always exist; the test fixture has to model that.
        for i in 0..3 {
            conn.execute(
                "INSERT INTO belief_versions (id) VALUES (?1)",
                params![format!("v{}", i)],
            )
            .unwrap();
        }

        // Seed with rows that use all three legacy source_types — exactly
        // what a real dogfood DB looks like.
        for (i, (src_type, relation)) in [
            ("turn", "extracted_from"),
            ("artifact", "extracted_from"),
            ("belief", "summarizes"),
        ]
        .iter()
        .enumerate()
        {
            conn.execute(
                "INSERT INTO belief_provenance
                   (id, belief_version_id, source_type, source_id, relation, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    format!("p{}", i),
                    format!("v{}", i),
                    src_type,
                    format!("s{}", i),
                    relation,
                    "2026-05-23T00:00:00Z"
                ],
            )
            .unwrap();
        }

        // Confirm pre-migration CHECK rejects the new types.
        let pre_reject = conn.execute(
            "INSERT INTO belief_provenance
               (id, belief_version_id, source_type, source_id, relation, created_at)
             VALUES ('px', 'vx', 'mcp_client', 'sx', 'extracted_from', '2026-05-23T00:00:00Z')",
            [],
        );
        assert!(pre_reject.is_err(), "old CHECK should reject mcp_client");

        // Migrate.
        let ran = migrate_provenance_check_constraint(&conn).unwrap();
        assert!(ran, "migration should have run on the legacy schema");

        // All three original rows survived.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM belief_provenance", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);

        // New CHECK now accepts mcp_client + proposal.
        for v in ["vx", "vy"] {
            conn.execute("INSERT INTO belief_versions (id) VALUES (?1)", params![v])
                .unwrap();
        }
        conn.execute(
            "INSERT INTO belief_provenance
               (id, belief_version_id, source_type, source_id, relation, created_at)
             VALUES ('px', 'vx', 'mcp_client', 'sx', 'extracted_from', '2026-05-23T00:00:00Z')",
            [],
        )
        .expect("mcp_client should now be allowed");
        conn.execute(
            "INSERT INTO belief_provenance
               (id, belief_version_id, source_type, source_id, relation, created_at)
             VALUES ('py', 'vy', 'proposal', 'sy', 'extracted_from', '2026-05-23T00:00:00Z')",
            [],
        )
        .expect("proposal should now be allowed");

        // Indexes recreated.
        let idx_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='index' AND tbl_name='belief_provenance'
                   AND name IN ('idx_provenance_version','idx_provenance_source')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(idx_count, 2, "both provenance indexes should be present");
    }

    #[test]
    fn migration_is_idempotent_on_already_migrated_db() {
        // register_vec_extension MUST run before open_in_memory — sqlite-vec
        // is loaded via sqlite3_auto_extension which only fires on new
        // connections. Calling it after open() is a no-op (Once-guarded).
        crate::embeddings::register_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        // Apply the new schema directly (simulating a fresh DB).
        conn.execute_batch(SCHEMA).unwrap();
        let ran = migrate_provenance_check_constraint(&conn).unwrap();
        assert!(!ran, "migration should no-op on a fresh DB");
    }

    #[test]
    fn migration_is_noop_on_empty_db() {
        let conn = Connection::open_in_memory().unwrap();
        let ran = migrate_provenance_check_constraint(&conn).unwrap();
        assert!(!ran, "migration should no-op when table doesn't exist");
    }

    /// A pre-this-commit belief_versions with `confidence REAL NOT NULL`, plus
    /// the FK-referencing belief_provenance with ON DELETE CASCADE — exactly the
    /// shape that makes the rebuild dangerous if foreign keys aren't disabled.
    const OLD_BELIEF_VERSIONS_SCHEMA: &str = r#"
        CREATE TABLE beliefs (id TEXT PRIMARY KEY);
        CREATE TABLE belief_versions (
            id          TEXT PRIMARY KEY,
            belief_id   TEXT NOT NULL REFERENCES beliefs(id) ON DELETE CASCADE,
            version_num INTEGER NOT NULL,
            statement   TEXT    NOT NULL,
            confidence  REAL    NOT NULL CHECK (confidence >= 0.0 AND confidence <= 1.0),
            reason      TEXT,
            editor      TEXT    NOT NULL CHECK (editor IN ('user','ai','system')),
            created_at  TEXT    NOT NULL,
            UNIQUE (belief_id, version_num)
        );
        CREATE TABLE belief_provenance (
            id                TEXT PRIMARY KEY,
            belief_version_id TEXT NOT NULL REFERENCES belief_versions(id) ON DELETE CASCADE,
            source_type       TEXT NOT NULL,
            source_id         TEXT NOT NULL,
            relation          TEXT NOT NULL,
            created_at        TEXT NOT NULL
        );
    "#;

    #[test]
    fn confidence_migration_makes_column_nullable_without_nuking_provenance() {
        let conn = Connection::open_in_memory().unwrap();
        // Foreign keys ON — this is what makes the naive DROP dangerous.
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(OLD_BELIEF_VERSIONS_SCHEMA).unwrap();

        conn.execute("INSERT INTO beliefs (id) VALUES ('b1')", []).unwrap();
        conn.execute(
            "INSERT INTO belief_versions
               (id, belief_id, version_num, statement, confidence, reason, editor, created_at)
             VALUES ('v1','b1',1,'stmt',0.9,'why','ai','2026-05-26T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO belief_provenance
               (id, belief_version_id, source_type, source_id, relation, created_at)
             VALUES ('p1','v1','turn','m1','extracted_from','2026-05-26T00:00:00Z')",
            [],
        )
        .unwrap();

        // Pre-migration: NULL confidence is rejected by the NOT NULL constraint.
        let pre = conn.execute(
            "INSERT INTO belief_versions
               (id, belief_id, version_num, statement, confidence, reason, editor, created_at)
             VALUES ('v2','b1',2,'stmt2',NULL,NULL,'ai','2026-05-26T00:00:00Z')",
            [],
        );
        assert!(pre.is_err(), "old NOT NULL should reject a NULL confidence");

        let ran = migrate_belief_versions_confidence_nullable(&conn).unwrap();
        assert!(ran, "migration should run on the legacy schema");

        // The provenance row survived — foreign keys were OFF during the drop,
        // so ON DELETE CASCADE did NOT fire.
        let prov: i64 = conn
            .query_row("SELECT COUNT(*) FROM belief_provenance", [], |r| r.get(0))
            .unwrap();
        assert_eq!(prov, 1, "provenance must survive the rebuild");
        let bv: i64 = conn
            .query_row("SELECT COUNT(*) FROM belief_versions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(bv, 1, "existing version rows must be preserved");

        // Post-migration: NULL confidence is now accepted.
        conn.execute(
            "INSERT INTO belief_versions
               (id, belief_id, version_num, statement, confidence, reason, editor, created_at)
             VALUES ('v2','b1',2,'stmt2',NULL,NULL,'ai','2026-05-26T00:00:00Z')",
            [],
        )
        .expect("NULL confidence should now be allowed");

        // Idempotent.
        let again = migrate_belief_versions_confidence_nullable(&conn).unwrap();
        assert!(!again, "migration should no-op once already applied");
    }

    #[test]
    fn confidence_migration_noops_on_fresh_schema() {
        crate::embeddings::register_vec_extension();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        let ran = migrate_belief_versions_confidence_nullable(&conn).unwrap();
        assert!(!ran, "fresh schema is already nullable");
    }
}
