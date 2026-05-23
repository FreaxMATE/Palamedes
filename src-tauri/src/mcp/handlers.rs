//! MCP tool handlers — the public RPC surface that external AIs see.
//!
//! Five tools:
//! - `list_beliefs`, `get_belief`, `search_beliefs` — read-only.
//! - `propose_belief`, `correct_belief` — write paths that land rows in
//!   `belief_proposals` (NEVER auto-assert).
//!
//! The handler struct is constructed once per MCP session by the
//! `StreamableHttpService` factory in [`super::server`]. Shared state
//! (`Db`, `NebiusClient`) is cheap to clone (Arc inside). Per-session
//! state — the `client_id` we identified during `initialize` — is held
//! in an `Arc<RwLock<Option<String>>>` so the tool methods can read it.
//!
//! Consent gating (read tools require `consent_read=1`; write tools
//! require `consent_write=1`) is wired on Day 6. Today's handlers
//! resolve the client_id and call straight through.

use std::sync::Arc;

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, InitializeRequestParam, InitializeResult,
        ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::RwLock;

use crate::db::Db;
use crate::embeddings;
use crate::ledger::Ledger;
use crate::mcp::{consent, proposals};
use crate::nebius::NebiusClient;

#[derive(Clone)]
pub struct PalamedesMcpHandler {
    pub db: Arc<Db>,
    pub client: NebiusClient,
    /// Set during `initialize` from the MCP `clientInfo.name`. Read by
    /// the write tools to attribute proposals to the right `mcp_clients`
    /// row. Per-session, so each MCP client gets its own handler.
    pub client_id: Arc<RwLock<Option<String>>>,
    tool_router: ToolRouter<Self>,
}

impl PalamedesMcpHandler {
    pub fn new(db: Arc<Db>, client: NebiusClient) -> Self {
        Self {
            db,
            client,
            client_id: Arc::new(RwLock::new(None)),
            tool_router: Self::tool_router(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool input + output types
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ListBeliefsInput {
    /// Filter by trust_class: asserted | inferred | hypothesized | summary.
    pub trust_class: Option<String>,
    /// Filter by status: asserted | inferred | corrected | contested | expired | blocked.
    pub status: Option<String>,
    /// Filter by category, e.g. preference / fact / skill / plan / context.
    pub category: Option<String>,
    /// Drop beliefs whose current_version.confidence < this value.
    pub min_confidence: Option<f64>,
    /// Max rows to return. Capped at 500 server-side.
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetBeliefInput {
    /// The belief's UUID.
    pub id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchBeliefsInput {
    /// Natural-language query. Embedded via the user's configured
    /// embedding model, then matched against the corpus by cosine.
    pub query: String,
    /// Top-K to return. Default 8, max 50.
    pub k: Option<i64>,
    /// Drop matches below this cosine. Default 0.35.
    pub min_cosine: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProposeBeliefInput {
    /// The belief you want the user to add. Required.
    pub statement: String,
    /// Free-form identifier for where this came from — e.g.
    /// "inferred from chat", a URL, your tool's name.
    pub source: String,
    /// Suggested category. The user can override on accept.
    pub category: Option<String>,
    /// Your confidence in [0, 1]. The user can override on accept.
    pub confidence: Option<f64>,
    /// Optional explanation of why you're proposing this. Shown in
    /// the audit inbox.
    pub reasoning: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CorrectBeliefInput {
    /// The id of the belief you think is wrong.
    pub belief_id: String,
    /// One of: contested, corrected, expired.
    pub suggested_status: String,
    /// Why you think this belief is wrong. At least 5 characters.
    pub reason: String,
}

#[derive(Debug, Serialize)]
struct ProposalAck {
    proposal_id: String,
    status: &'static str,
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[tool_router]
impl PalamedesMcpHandler {
    #[tool(
        description = "Query the user's Belief Ledger. Read-only. Returns up to `limit` rows, newest first."
    )]
    async fn list_beliefs(
        &self,
        Parameters(input): Parameters<ListBeliefsInput>,
    ) -> Result<CallToolResult, McpError> {
        self.require_consent(false).await?;
        let limit = input.limit.unwrap_or(50).clamp(1, 500);
        let beliefs = self
            .db
            .with_conn(|conn| list_beliefs_sql(conn, &input, limit))
            .map_err(|e| internal(format!("list_beliefs failed: {e}")))?;
        ok_json(&json!({ "beliefs": beliefs, "count": beliefs.len() }))
    }

    #[tool(
        description = "Fetch a single belief with all versions, provenance edges, and recent receipts."
    )]
    async fn get_belief(
        &self,
        Parameters(input): Parameters<GetBeliefInput>,
    ) -> Result<CallToolResult, McpError> {
        self.require_consent(false).await?;
        let detail = self
            .db
            .with_conn(|conn| {
                let ledger = Ledger::new(conn);
                let Some(belief) = ledger.get_belief(&input.id)? else {
                    return anyhow::Ok(serde_json::Value::Null);
                };
                let versions = ledger.get_versions(&input.id)?;
                let mut prov_by_version: serde_json::Map<String, serde_json::Value> =
                    Default::default();
                for v in &versions {
                    let p = ledger.get_provenance(&v.id)?;
                    prov_by_version
                        .insert(v.id.clone(), serde_json::to_value(p).unwrap_or_default());
                }
                Ok(json!({
                    "belief": belief,
                    "versions": versions,
                    "provenance_by_version_id": prov_by_version,
                }))
            })
            .map_err(|e| internal(format!("get_belief failed: {e}")))?;
        if detail.is_null() {
            return Err(McpError::invalid_params(
                format!("no belief with id {}", input.id),
                None,
            ));
        }
        ok_json(&detail)
    }

    #[tool(
        description = "Semantic search across the user's Belief Ledger. Returns top-K beliefs whose embeddings are closest to `query`."
    )]
    async fn search_beliefs(
        &self,
        Parameters(input): Parameters<SearchBeliefsInput>,
    ) -> Result<CallToolResult, McpError> {
        self.require_consent(false).await?;
        if input.query.trim().is_empty() {
            return Err(McpError::invalid_params("query must be non-empty", None));
        }
        let k = input.k.unwrap_or(8).clamp(1, 50) as usize;
        let min_cosine = input
            .min_cosine
            .unwrap_or(embeddings::DEFAULT_RETRIEVAL_MIN_COSINE);

        let model = self
            .db
            .get_setting("embedding_model")
            .ok()
            .flatten()
            .unwrap_or_else(|| embeddings::DEFAULT_EMBEDDING_MODEL.to_string());

        let mut q = self
            .client
            .embed_query(&model, input.query.trim())
            .await
            .map_err(|e| internal(format!("embed_query failed: {e}")))?;
        if q.len() != embeddings::EMBEDDING_DIM {
            return Err(internal(format!(
                "embedding dim mismatch: expected {}, got {}",
                embeddings::EMBEDDING_DIM,
                q.len()
            )));
        }
        embeddings::normalize(&mut q);

        let mut hits = self
            .db
            .retrieve_top_k(&q, k)
            .map_err(|e| internal(format!("retrieve_top_k failed: {e}")))?;
        hits.retain(|r| embeddings::cosine_from_l2(r.distance) >= min_cosine);

        // Hydrate hits with the current statement + confidence so the
        // caller doesn't have to round-trip a follow-up get_belief.
        let hydrated = self
            .db
            .with_conn(|conn| {
                let ledger = Ledger::new(conn);
                let mut out = Vec::with_capacity(hits.len());
                for h in &hits {
                    let belief = ledger.get_belief(&h.belief_id)?;
                    let version = ledger.get_version(&h.version_id)?;
                    out.push(json!({
                        "belief_id": h.belief_id,
                        "version_id": h.version_id,
                        "cosine": embeddings::cosine_from_l2(h.distance),
                        "belief": belief,
                        "version": version,
                    }));
                }
                anyhow::Ok(out)
            })
            .map_err(|e| internal(format!("hydrate failed: {e}")))?;

        ok_json(&json!({ "matches": hydrated, "count": hydrated.len() }))
    }

    #[tool(
        description = "Propose a new belief about the user. Lands in the audit inbox for human review. NEVER auto-asserted."
    )]
    async fn propose_belief(
        &self,
        Parameters(input): Parameters<ProposeBeliefInput>,
    ) -> Result<CallToolResult, McpError> {
        self.require_consent(true).await?;
        let client_id = self
            .require_client_id()
            .await
            .ok_or_else(|| internal("MCP initialize did not establish a client_id"))?;
        let proposal = self
            .db
            .with_conn(|conn| {
                proposals::insert_propose(
                    conn,
                    &client_id,
                    &proposals::ProposeInput {
                        statement: input.statement,
                        source: input.source,
                        category: input.category,
                        confidence: input.confidence,
                        reasoning: input.reasoning,
                    },
                )
            })
            .map_err(|e| McpError::invalid_params(format!("{e}"), None))?;
        ok_json(&ProposalAck {
            proposal_id: proposal.id,
            status: "pending",
        })
    }

    #[tool(
        description = "Flag an existing belief as wrong. Lands in the audit inbox for the user to adjudicate. NEVER auto-applied."
    )]
    async fn correct_belief(
        &self,
        Parameters(input): Parameters<CorrectBeliefInput>,
    ) -> Result<CallToolResult, McpError> {
        self.require_consent(true).await?;
        let client_id = self
            .require_client_id()
            .await
            .ok_or_else(|| internal("MCP initialize did not establish a client_id"))?;
        let proposal = self
            .db
            .with_conn(|conn| {
                proposals::insert_correct(
                    conn,
                    &client_id,
                    &proposals::CorrectInput {
                        belief_id: input.belief_id,
                        suggested_status: input.suggested_status,
                        reason: input.reason,
                    },
                )
            })
            .map_err(|e| McpError::invalid_params(format!("{e}"), None))?;
        ok_json(&ProposalAck {
            proposal_id: proposal.id,
            status: "pending",
        })
    }
}

impl PalamedesMcpHandler {
    async fn require_client_id(&self) -> Option<String> {
        self.client_id.read().await.clone()
    }

    /// Block this tool call unless the connecting MCP client is consented
    /// for the appropriate bucket (read vs write). Returns a clear error
    /// the calling AI can surface to the user — they then open Palamedes
    /// Settings → Connections and approve.
    async fn require_consent(&self, need_write: bool) -> Result<(), McpError> {
        let client_id = self.require_client_id().await.ok_or_else(|| {
            internal("MCP session has no client_id — initialize did not run yet")
        })?;
        let client = self
            .db
            .with_conn(|conn| consent::get_client_by_id(conn, &client_id))
            .map_err(|e| internal(format!("client lookup failed: {e}")))?
            .ok_or_else(|| internal(format!("mcp_clients row {client_id} vanished")))?;
        if client.revoked_at.is_some() {
            return Err(McpError::invalid_request(
                "this MCP client is revoked. Re-approve it in Palamedes Settings → Connections.",
                None,
            ));
        }
        let ok = if need_write { client.can_write() } else { client.can_read() };
        if !ok {
            let msg = if client.is_pending() {
                "consent pending — approve this client in Palamedes Settings → Connections"
            } else if need_write {
                "write consent denied — grant write access in Palamedes Settings → Connections"
            } else {
                "read consent denied — grant read access in Palamedes Settings → Connections"
            };
            return Err(McpError::invalid_request(msg, None));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ServerHandler: initialize hook tracks the connecting MCP client.
// ---------------------------------------------------------------------------

#[tool_handler]
impl ServerHandler for PalamedesMcpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "This server exposes the user's Belief Ledger over MCP. \
                 Read tools (list_beliefs, get_belief, search_beliefs) return rows from \
                 a versioned, audited corpus. Write tools (propose_belief, correct_belief) \
                 NEVER auto-assert — they land in an audit inbox the user reviews."
                    .to_string(),
            )
    }

    async fn initialize(
        &self,
        request: InitializeRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        // Upsert the client row + remember its id for this session. Read
        // and write tool gating (Day 6) will key off this id.
        let name = request.client_info.name.clone();
        let version = Some(request.client_info.version.clone());
        let client_info_json = serde_json::to_string(&request.client_info).ok();
        let (client, _newly_created) = self
            .db
            .with_conn(|conn| {
                consent::upsert_client(conn, &name, version.as_deref(), client_info_json.as_deref())
            })
            .map_err(|e| internal(format!("upsert_client failed: {e}")))?;
        *self.client_id.write().await = Some(client.id.clone());
        // TODO(Day 6): if consent_read or consent_write is None, emit a
        // 'mcp:client-pending' Tauri event and hold pending tool calls
        // until the user resolves consent.
        Ok(self.get_info())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ok_json<T: Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let s = serde_json::to_string_pretty(value)
        .map_err(|e| internal(format!("serialize failed: {e}")))?;
    Ok(CallToolResult::success(vec![Content::text(s)]))
}

fn internal(msg: impl Into<String>) -> McpError {
    McpError::internal_error(msg.into(), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_handler() -> (PalamedesMcpHandler, std::path::PathBuf) {
        crate::embeddings::register_vec_extension();
        let path = std::env::temp_dir()
            .join(format!("palamedes-handlers-test-{}.db", uuid::Uuid::new_v4()));
        let db = Arc::new(Db::open(&path).unwrap());
        let client = NebiusClient::from_api_key("test-key".into());
        (PalamedesMcpHandler::new(db, client), path)
    }

    #[tokio::test]
    async fn require_consent_rejects_pending_then_admits_granted() {
        let (handler, path) = make_handler();
        let mcp_client_id = handler
            .db
            .with_conn(|conn| {
                let (c, _) = consent::upsert_client(conn, "test-client", None, None)?;
                anyhow::Ok(c.id)
            })
            .unwrap();
        *handler.client_id.write().await = Some(mcp_client_id.clone());

        // Pending consent → reads rejected.
        assert!(handler.require_consent(false).await.is_err());
        assert!(handler.require_consent(true).await.is_err());

        // Grant read only.
        handler
            .db
            .with_conn(|conn| consent::set_consent(conn, &mcp_client_id, true, false))
            .unwrap();
        assert!(handler.require_consent(false).await.is_ok());
        assert!(handler.require_consent(true).await.is_err());

        // Grant both.
        handler
            .db
            .with_conn(|conn| consent::set_consent(conn, &mcp_client_id, true, true))
            .unwrap();
        assert!(handler.require_consent(false).await.is_ok());
        assert!(handler.require_consent(true).await.is_ok());

        // Revoke clears both.
        handler
            .db
            .with_conn(|conn| consent::revoke_client(conn, &mcp_client_id))
            .unwrap();
        assert!(handler.require_consent(false).await.is_err());
        assert!(handler.require_consent(true).await.is_err());

        let _ = std::fs::remove_file(&path);
    }
}

/// `list_beliefs` SQL with optional filters. Done inline rather than as
/// a `Db` method since the filter shape is MCP-specific.
fn list_beliefs_sql(
    conn: &rusqlite::Connection,
    input: &ListBeliefsInput,
    limit: i64,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut sql = String::from(
        "SELECT b.id, b.subject, b.category, b.current_version_id, b.status, b.trust_class,
                b.scope, b.scope_ref_id, b.level, b.parent_summary_id,
                b.created_at, b.updated_at, b.last_reinforced_at,
                bv.statement, bv.confidence
         FROM beliefs b
         LEFT JOIN belief_versions bv ON bv.id = b.current_version_id
         WHERE 1=1",
    );
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(tc) = input.trust_class.as_deref() {
        sql.push_str(" AND b.trust_class = ?");
        binds.push(Box::new(tc.to_string()));
    }
    if let Some(st) = input.status.as_deref() {
        sql.push_str(" AND b.status = ?");
        binds.push(Box::new(st.to_string()));
    }
    if let Some(cat) = input.category.as_deref() {
        sql.push_str(" AND b.category = ?");
        binds.push(Box::new(cat.to_string()));
    }
    if let Some(min_c) = input.min_confidence {
        sql.push_str(" AND COALESCE(bv.confidence, 0) >= ?");
        binds.push(Box::new(min_c));
    }
    sql.push_str(" ORDER BY b.updated_at DESC LIMIT ?");
    binds.push(Box::new(limit));

    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |r| {
        Ok(json!({
            "id": r.get::<_, String>(0)?,
            "subject": r.get::<_, String>(1)?,
            "category": r.get::<_, Option<String>>(2)?,
            "current_version_id": r.get::<_, Option<String>>(3)?,
            "status": r.get::<_, String>(4)?,
            "trust_class": r.get::<_, String>(5)?,
            "scope": r.get::<_, String>(6)?,
            "scope_ref_id": r.get::<_, Option<String>>(7)?,
            "level": r.get::<_, i32>(8)?,
            "parent_summary_id": r.get::<_, Option<String>>(9)?,
            "created_at": r.get::<_, String>(10)?,
            "updated_at": r.get::<_, String>(11)?,
            "last_reinforced_at": r.get::<_, Option<String>>(12)?,
            "statement": r.get::<_, Option<String>>(13)?,
            "confidence": r.get::<_, Option<f64>>(14)?,
        }))
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}
