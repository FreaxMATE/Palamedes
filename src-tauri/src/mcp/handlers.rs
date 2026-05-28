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
        CallToolResult, Content, Implementation, InitializeRequestParams, InitializeResult,
        ListResourcesResult, PaginatedRequestParams, ProtocolVersion, RawResource,
        ReadResourceRequestParams, ReadResourceResult, Resource, ResourceContents,
        ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::RwLock;

use crate::audit::AuditDb;
use crate::db::Db;
use crate::embeddings;
use crate::ledger::Ledger;
use crate::mcp::{audit, consent, proposals};
use crate::nebius::NebiusClient;

#[derive(Clone)]
pub struct PalamedesMcpHandler {
    pub db: Arc<Db>,
    pub client: NebiusClient,
    /// Tamper-evident hash chain (separate `audit.db`). Read by the
    /// `audit_trail` tool to expose the chain to external MCP clients.
    pub chain: Arc<AuditDb>,
    /// Set during `initialize` from the MCP `clientInfo.name`. Read by
    /// the write tools to attribute proposals to the right `mcp_clients`
    /// row. Per-session, so each MCP client gets its own handler.
    pub client_id: Arc<RwLock<Option<String>>>,
    // Populated by the `#[tool_router]` macro and consumed reflectively
    // by `rmcp`'s ServerHandler impl — never read directly from Rust.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl PalamedesMcpHandler {
    pub fn new(db: Arc<Db>, client: NebiusClient, chain: Arc<AuditDb>) -> Self {
        Self {
            db,
            client,
            chain,
            client_id: Arc::new(RwLock::new(None)),
            tool_router: Self::tool_router(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool input + output types
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ListBeliefsInput {
    /// Filter by trust_class: asserted | inferred | hypothesized | summary.
    pub trust_class: Option<String>,
    /// Filter by status: asserted | inferred | corrected | contested | expired | blocked.
    pub status: Option<String>,
    /// Filter by category, e.g. preference / fact / skill / plan / context.
    pub category: Option<String>,
    /// Max rows to return. Capped at 500 server-side.
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GetBeliefInput {
    /// The belief's UUID.
    pub id: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SearchBeliefsInput {
    /// Natural-language query. Embedded via the user's configured
    /// embedding model, then matched against the corpus by cosine.
    pub query: String,
    /// Top-K to return. Default 8, max 50.
    pub k: Option<i64>,
    /// Drop matches below this cosine. Default 0.35.
    pub min_cosine: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RecallTraceInput {
    /// The id of the chat turn (assistant message) to trace.
    pub turn_id: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct AuditTrailInput {
    /// Optional belief id — substring-matched against the metadata blob
    /// of every chain row. Returns only rows that mention this belief.
    pub belief_id: Option<String>,
    /// RFC-3339 lower bound on row timestamp.
    pub since: Option<String>,
    /// RFC-3339 upper bound on row timestamp.
    pub until: Option<String>,
    /// Max rows. Default 50, cap 500.
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ConsistencyCheckInput {
    /// The set of belief ids to check for internal contradictions.
    /// Returns pairs (a, b) where a's provenance points at b with a
    /// `contradicted_by` or `corrected_by` edge (or vice versa).
    pub belief_ids: Vec<String>,
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
    // No confidence field: Palamedes does not accept self-reported confidence
    // from any source (model or external AI). Confidence is derived
    // structurally from reinforcement + recency once the belief is accepted.
    /// Optional explanation of why you're proposing this. Shown in
    /// the audit inbox.
    pub reasoning: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReportOutcomeInput {
    /// The id of the belief whose outcome you observed.
    pub belief_id: String,
    /// What actually happened with respect to this belief.
    /// One of: `refuted` (the user contradicted it), `expired`
    /// (the claim is no longer applicable — e.g. a former preference),
    /// or `confirmed` (the user re-asserted it).
    ///
    /// `refuted` and `expired` land as correct proposals in the audit
    /// inbox. `confirmed` is rejected here — use `propose_belief` to
    /// suggest a new supporting claim instead, since auto-reinforcing
    /// from external AIs would let any caller inflate confidence.
    pub outcome: String,
    /// What you observed. At least 5 characters. Stored on the proposal.
    pub evidence: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CorrectPatternInput {
    /// Optional substring of `belief_versions.statement` — SQL LIKE
    /// pattern is built by wrapping in `%…%`.
    pub statement_like: Option<String>,
    /// Optional exact-match category.
    pub category: Option<String>,
    /// Optional exact-match status.
    pub status: Option<String>,
    /// Optional exact-match trust_class.
    pub trust_class: Option<String>,
    /// What status to suggest for the matched beliefs. One of:
    /// `contested`, `corrected`, `expired`.
    pub suggested_status: String,
    /// Shared reason for the bulk correction. Stamped on every proposal.
    /// At least 5 characters.
    pub reason: String,
    /// Cap on matches to land proposals for. Default 25, max 100 — bulk
    /// is for "we're decommissioning a feature, contest everything
    /// tagged `acme-app`", not "drop all preferences."
    pub limit: Option<i64>,
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
        let client_id = self.gate_read().await?;
        let input_json = serde_json::to_string(&input).ok();
        let limit = input.limit.unwrap_or(50).clamp(1, 500);
        let beliefs = self
            .db
            .with_conn(|conn| list_beliefs_sql(conn, &input, limit))
            .map_err(|e| internal(format!("list_beliefs failed: {e}")))?;
        let ids: Vec<String> = beliefs
            .iter()
            .filter_map(|v| v.get("id").and_then(|x| x.as_str()).map(String::from))
            .collect();
        self.log_read(&client_id, "list_beliefs", input_json.as_deref(), &ids);
        ok_json(&json!({ "beliefs": beliefs, "count": beliefs.len() }))
    }

    #[tool(
        description = "Fetch a single belief with all versions, provenance edges, and recent receipts."
    )]
    async fn get_belief(
        &self,
        Parameters(input): Parameters<GetBeliefInput>,
    ) -> Result<CallToolResult, McpError> {
        let client_id = self.gate_read().await?;
        let input_json = serde_json::to_string(&input).ok();
        let target_id = input.id.clone();
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
        self.log_read(&client_id, "get_belief", input_json.as_deref(), &[target_id]);
        ok_json(&detail)
    }

    #[tool(
        description = "Semantic search across the user's Belief Ledger. Returns top-K beliefs whose embeddings are closest to `query`."
    )]
    async fn search_beliefs(
        &self,
        Parameters(input): Parameters<SearchBeliefsInput>,
    ) -> Result<CallToolResult, McpError> {
        let client_id = self.gate_read().await?;
        let input_json = serde_json::to_string(&input).ok();
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

        let ids: Vec<String> = hits.iter().map(|h| h.belief_id.clone()).collect();
        self.log_read(&client_id, "search_beliefs", input_json.as_deref(), &ids);
        ok_json(&json!({ "matches": hydrated, "count": hydrated.len() }))
    }

    #[tool(
        description = "Show which beliefs grounded a specific assistant turn — id, statement, rank, and retrieval weight. Read-only."
    )]
    async fn recall_trace(
        &self,
        Parameters(input): Parameters<RecallTraceInput>,
    ) -> Result<CallToolResult, McpError> {
        let client_id = self.gate_read().await?;
        let input_json = serde_json::to_string(&input).ok();
        let turn_id = input.turn_id.clone();
        let receipts = self
            .db
            .with_conn(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT r.belief_id, r.belief_version_id, r.weight, r.rank,
                            bv.statement, b.category, b.status, b.trust_class
                     FROM recall_receipts r
                     JOIN belief_versions bv ON bv.id = r.belief_version_id
                     JOIN beliefs b ON b.id = r.belief_id
                     WHERE r.turn_id = ?1
                     ORDER BY r.rank ASC",
                )?;
                let rows = stmt.query_map(params![turn_id], |r| {
                    Ok(json!({
                        "belief_id": r.get::<_, String>(0)?,
                        "version_id": r.get::<_, String>(1)?,
                        "weight": r.get::<_, f64>(2)?,
                        "rank": r.get::<_, i64>(3)?,
                        "statement": r.get::<_, String>(4)?,
                        "category": r.get::<_, Option<String>>(5)?,
                        "status": r.get::<_, String>(6)?,
                        "trust_class": r.get::<_, String>(7)?,
                    }))
                })?;
                anyhow::Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })
            .map_err(|e| internal(format!("recall_trace failed: {e}")))?;
        let ids: Vec<String> = receipts
            .iter()
            .filter_map(|v| v.get("belief_id").and_then(|x| x.as_str()).map(String::from))
            .collect();
        self.log_read(&client_id, "recall_trace", input_json.as_deref(), &ids);
        ok_json(&json!({
            "turn_id": input.turn_id,
            "receipts": receipts,
            "count": receipts.len(),
        }))
    }

    #[tool(
        description = "Query the tamper-evident audit chain. Filter by belief id (substring of metadata) and/or RFC-3339 time bounds. Read-only."
    )]
    async fn audit_trail(
        &self,
        Parameters(input): Parameters<AuditTrailInput>,
    ) -> Result<CallToolResult, McpError> {
        let client_id = self.gate_read().await?;
        let input_json = serde_json::to_string(&input).ok();
        let limit = input.limit.unwrap_or(50).clamp(1, 500);
        let entries = self
            .chain
            .query(
                input.belief_id.as_deref(),
                input.since.as_deref(),
                input.until.as_deref(),
                limit,
            )
            .map_err(|e| internal(format!("audit_trail failed: {e}")))?;
        let head = self
            .chain
            .head()
            .map_err(|e| internal(format!("audit head failed: {e}")))?;
        let ids: Vec<String> = entries.iter().map(|e| e.seq.to_string()).collect();
        self.log_read(&client_id, "audit_trail", input_json.as_deref(), &ids);
        ok_json(&json!({
            "entries": entries,
            "head": head,
            "count": entries.len(),
        }))
    }

    #[tool(
        description = "Given a set of belief ids, return pairs that contradict or supersede each other (via provenance edges). Read-only."
    )]
    async fn consistency_check(
        &self,
        Parameters(input): Parameters<ConsistencyCheckInput>,
    ) -> Result<CallToolResult, McpError> {
        let client_id = self.gate_read().await?;
        let input_json = serde_json::to_string(&input).ok();
        if input.belief_ids.is_empty() {
            return Err(McpError::invalid_params(
                "belief_ids must be non-empty",
                None,
            ));
        }
        if input.belief_ids.len() > 500 {
            return Err(McpError::invalid_params(
                "belief_ids capped at 500 per call",
                None,
            ));
        }
        let ids = input.belief_ids.clone();
        let pairs = self
            .db
            .with_conn(|conn| {
                // Find provenance rows where the version belongs to one
                // of the input beliefs AND the source is also in the set.
                let placeholders = std::iter::repeat("?")
                    .take(ids.len())
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!(
                    "SELECT DISTINCT bv.belief_id AS a, bp.source_id AS b, bp.relation
                     FROM belief_provenance bp
                     JOIN belief_versions bv ON bv.id = bp.belief_version_id
                     WHERE bp.source_type = 'belief'
                       AND bp.relation IN ('contradicted_by','corrected_by')
                       AND bv.belief_id IN ({placeholders})
                       AND bp.source_id IN ({placeholders})
                       AND bv.belief_id <> bp.source_id"
                );
                let mut stmt = conn.prepare(&sql)?;
                let binds: Vec<&dyn rusqlite::ToSql> = ids
                    .iter()
                    .chain(ids.iter())
                    .map(|s| s as &dyn rusqlite::ToSql)
                    .collect();
                let rows = stmt.query_map(binds.as_slice(), |r| {
                    Ok(json!({
                        "a": r.get::<_, String>(0)?,
                        "b": r.get::<_, String>(1)?,
                        "relation": r.get::<_, String>(2)?,
                    }))
                })?;
                anyhow::Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })
            .map_err(|e| internal(format!("consistency_check failed: {e}")))?;
        self.log_read(
            &client_id,
            "consistency_check",
            input_json.as_deref(),
            &input.belief_ids,
        );
        ok_json(&json!({
            "contradictions": pairs,
            "count": pairs.len(),
            "checked": input.belief_ids.len(),
        }))
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

    #[tool(
        description = "Report an outcome for an existing belief — refuted or expired lands as a correct proposal in the audit inbox. NEVER auto-applied."
    )]
    async fn report_outcome(
        &self,
        Parameters(input): Parameters<ReportOutcomeInput>,
    ) -> Result<CallToolResult, McpError> {
        self.require_consent(true).await?;
        let client_id = self
            .require_client_id()
            .await
            .ok_or_else(|| internal("MCP initialize did not establish a client_id"))?;
        let suggested_status = match input.outcome.as_str() {
            "refuted" => "contested",
            "expired" => "expired",
            "confirmed" => {
                return Err(McpError::invalid_params(
                    "outcome=confirmed isn't accepted — confidence is structural and \
                     can't be auto-reinforced from external AIs. Use propose_belief to \
                     suggest a new supporting claim instead.",
                    None,
                ));
            }
            other => {
                return Err(McpError::invalid_params(
                    format!("unknown outcome '{other}'; expected refuted | expired | confirmed"),
                    None,
                ));
            }
        };
        let proposal = self
            .db
            .with_conn(|conn| {
                proposals::insert_correct(
                    conn,
                    &client_id,
                    &proposals::CorrectInput {
                        belief_id: input.belief_id,
                        suggested_status: suggested_status.into(),
                        reason: input.evidence,
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
        description = "Bulk-correct beliefs matching a predicate — one correct proposal per match. NEVER auto-applied."
    )]
    async fn correct_pattern(
        &self,
        Parameters(input): Parameters<CorrectPatternInput>,
    ) -> Result<CallToolResult, McpError> {
        self.require_consent(true).await?;
        let client_id = self
            .require_client_id()
            .await
            .ok_or_else(|| internal("MCP initialize did not establish a client_id"))?;
        if input.reason.trim().len() < 5 {
            return Err(McpError::invalid_params(
                "reason must be at least 5 chars",
                None,
            ));
        }
        if !matches!(
            input.suggested_status.as_str(),
            "contested" | "corrected" | "expired"
        ) {
            return Err(McpError::invalid_params(
                "suggested_status must be one of: contested, corrected, expired",
                None,
            ));
        }
        // At least one filter — guard against "correct everything in the ledger."
        if input.statement_like.is_none()
            && input.category.is_none()
            && input.status.is_none()
            && input.trust_class.is_none()
        {
            return Err(McpError::invalid_params(
                "at least one of statement_like / category / status / trust_class is required",
                None,
            ));
        }
        let limit = input.limit.unwrap_or(25).clamp(1, 100);
        let suggested_status = input.suggested_status.clone();
        let reason = input.reason.trim().to_string();
        let acks = self
            .db
            .with_conn(|conn| {
                let mut sql = String::from(
                    "SELECT b.id
                     FROM beliefs b
                     LEFT JOIN belief_versions bv ON bv.id = b.current_version_id
                     WHERE 1=1",
                );
                let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
                if let Some(stmt_like) = &input.statement_like {
                    sql.push_str(" AND bv.statement LIKE ?");
                    binds.push(Box::new(format!("%{stmt_like}%")));
                }
                if let Some(cat) = &input.category {
                    sql.push_str(" AND b.category = ?");
                    binds.push(Box::new(cat.clone()));
                }
                if let Some(st) = &input.status {
                    sql.push_str(" AND b.status = ?");
                    binds.push(Box::new(st.clone()));
                }
                if let Some(tc) = &input.trust_class {
                    sql.push_str(" AND b.trust_class = ?");
                    binds.push(Box::new(tc.clone()));
                }
                sql.push_str(" ORDER BY b.updated_at DESC LIMIT ?");
                binds.push(Box::new(limit));
                let mut stmt = conn.prepare(&sql)?;
                let refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
                let ids: Vec<String> = stmt
                    .query_map(refs.as_slice(), |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;

                let mut out = Vec::with_capacity(ids.len());
                for belief_id in ids {
                    let proposal = proposals::insert_correct(
                        conn,
                        &client_id,
                        &proposals::CorrectInput {
                            belief_id: belief_id.clone(),
                            suggested_status: suggested_status.clone(),
                            reason: reason.clone(),
                        },
                    )?;
                    out.push(json!({
                        "belief_id": belief_id,
                        "proposal_id": proposal.id,
                    }));
                }
                anyhow::Ok(out)
            })
            .map_err(|e| McpError::invalid_params(format!("{e}"), None))?;
        ok_json(&json!({
            "proposals": acks,
            "count": acks.len(),
            "limit": limit,
        }))
    }
}

impl PalamedesMcpHandler {
    async fn require_client_id(&self) -> Option<String> {
        self.client_id.read().await.clone()
    }

    /// Combined gate for read tools: enforces consent, resolves the
    /// client_id, then checks the per-client 24h read budget. Returns the
    /// resolved client_id so the caller can attach it to the audit log
    /// entry it writes after the work completes.
    async fn gate_read(&self) -> Result<String, McpError> {
        self.require_consent(false).await?;
        let client_id = self
            .require_client_id()
            .await
            .ok_or_else(|| internal("MCP initialize did not establish a client_id"))?;
        self.db
            .with_conn(|conn| audit::check_rate_limit(conn, &client_id))
            .map_err(|e| McpError::invalid_request(format!("{e}"), None))?;
        Ok(client_id)
    }

    fn resource_recent(&self) -> anyhow::Result<serde_json::Value> {
        let beliefs = self.db.with_conn(|conn| {
            list_beliefs_sql(conn, &ListBeliefsInput::default(), 50)
        })?;
        Ok(json!({ "beliefs": beliefs, "count": beliefs.len() }))
    }

    fn resource_contradictions(&self) -> anyhow::Result<serde_json::Value> {
        let input = ListBeliefsInput {
            status: Some("contested".into()),
            ..Default::default()
        };
        let contested = self.db.with_conn(|conn| list_beliefs_sql(conn, &input, 200))?;
        let input = ListBeliefsInput {
            status: Some("corrected".into()),
            ..Default::default()
        };
        let corrected = self.db.with_conn(|conn| list_beliefs_sql(conn, &input, 200))?;
        let input = ListBeliefsInput {
            status: Some("expired".into()),
            ..Default::default()
        };
        let expired = self.db.with_conn(|conn| list_beliefs_sql(conn, &input, 200))?;
        Ok(json!({
            "contested": contested,
            "corrected": corrected,
            "expired": expired,
            "total": contested.len() + corrected.len() + expired.len(),
        }))
    }

    fn resource_audit_head(&self) -> anyhow::Result<serde_json::Value> {
        let head = self.chain.head()?;
        Ok(json!({ "head": head }))
    }

    /// Append a row to the read audit log. Best-effort — a failure to log
    /// does not fail the tool call (the caller already has the data).
    fn log_read(&self, client_id: &str, tool: &str, input_json: Option<&str>, ids: &[String]) {
        let _ = self
            .db
            .with_conn(|conn| audit::log_read(conn, client_id, tool, input_json, ids));
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
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::from_build_env())
        .with_protocol_version(ProtocolVersion::V_2024_11_05)
        .with_instructions(
            "This server exposes the user's Belief Ledger over MCP. \
                 Read tools (list_beliefs, get_belief, search_beliefs, recall_trace, \
                 audit_trail, consistency_check) return rows from a versioned, audited \
                 corpus. Write tools (propose_belief, correct_belief, report_outcome, \
                 correct_pattern) NEVER auto-assert — they land in an audit inbox the \
                 user reviews. Resources: palamedes://recent, palamedes://contradictions, \
                 palamedes://audit-head."
                .to_string(),
        )
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let make = |uri: &str, name: &str, description: &str| {
            let mut raw = RawResource::new(uri, name);
            raw.description = Some(description.into());
            raw.mime_type = Some("application/json".into());
            Resource::new(raw, None)
        };
        Ok(ListResourcesResult::with_all_items(vec![
            make(
                "palamedes://recent",
                "recent_beliefs",
                "The 50 most recently updated beliefs.",
            ),
            make(
                "palamedes://contradictions",
                "contradictions",
                "Beliefs in contested / corrected / expired state — the user's \
                 'things I've ruled out' set.",
            ),
            make(
                "palamedes://audit-head",
                "audit_head",
                "Latest entry of the tamper-evident hash chain. Use this as an \
                 external attestation anchor.",
            ),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let client_id = self.gate_read().await?;
        let uri = request.uri.clone();
        let body = match uri.as_str() {
            "palamedes://recent" => self.resource_recent().map_err(|e| {
                internal(format!("recent resource failed: {e}"))
            })?,
            "palamedes://contradictions" => self.resource_contradictions().map_err(|e| {
                internal(format!("contradictions resource failed: {e}"))
            })?,
            "palamedes://audit-head" => self.resource_audit_head().map_err(|e| {
                internal(format!("audit-head resource failed: {e}"))
            })?,
            other => {
                return Err(McpError::invalid_params(
                    format!("unknown resource uri '{other}'"),
                    None,
                ));
            }
        };
        let text = serde_json::to_string_pretty(&body)
            .map_err(|e| internal(format!("serialize failed: {e}")))?;
        self.log_read(&client_id, &format!("resource:{uri}"), None, &[]);
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(text, uri).with_mime_type("application/json"),
        ]))
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
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
        let chain = Arc::new(AuditDb::open_in_memory().unwrap());
        (PalamedesMcpHandler::new(db, client, chain), path)
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

    // -----------------------------------------------------------------
    // Week-3 helpers: consenting handler + ledger seeds.
    // -----------------------------------------------------------------

    use crate::ledger::{Editor, Ledger, NewBelief, NewVersion, Scope, Status, TrustClass};

    async fn handler_with_consent(write: bool) -> (PalamedesMcpHandler, std::path::PathBuf) {
        let (handler, path) = make_handler();
        let cid = handler
            .db
            .with_conn(|conn| {
                let (c, _) = consent::upsert_client(conn, "test-client", None, None)?;
                consent::set_consent(conn, &c.id, true, write)?;
                anyhow::Ok(c.id)
            })
            .unwrap();
        *handler.client_id.write().await = Some(cid);
        (handler, path)
    }

    fn seed_belief(db: &Db, statement: &str, status: Status, category: Option<&str>) -> String {
        db.with_conn(|conn| {
            let ledger = Ledger::new(conn);
            let (b, _) = ledger.insert_belief(NewBelief {
                subject: "user".into(),
                category: category.map(String::from),
                status,
                trust_class: TrustClass::Inferred,
                scope: Scope::Global,
                scope_ref_id: None,
                level: 0,
                parent_summary_id: None,
                initial_version: NewVersion {
                    statement: statement.into(),
                    confidence: None,
                    reason: None,
                    editor: Editor::User,
                },
            })?;
            anyhow::Ok(b.id)
        })
        .unwrap()
    }

    #[tokio::test]
    async fn recall_trace_returns_receipts_for_turn() {
        let (handler, path) = handler_with_consent(false).await;
        let belief_id = seed_belief(&handler.db, "User prefers Vim", Status::Inferred, Some("preference"));

        // Seed a turn + receipt manually.
        handler
            .db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO conversations (id, title, created_at, updated_at)
                     VALUES ('conv1', 'test', datetime('now'), datetime('now'))",
                    [],
                )?;
                conn.execute(
                    "INSERT INTO messages (id, conversation_id, role, content, created_at)
                     VALUES ('turn1', 'conv1', 'assistant', 'hi', datetime('now'))",
                    [],
                )?;
                let vid: String = conn.query_row(
                    "SELECT current_version_id FROM beliefs WHERE id = ?1",
                    [&belief_id],
                    |r| r.get(0),
                )?;
                conn.execute(
                    "INSERT INTO recall_receipts (id, turn_id, belief_id, belief_version_id, weight, rank)
                     VALUES ('r1', 'turn1', ?1, ?2, 0.83, 0)",
                    rusqlite::params![belief_id, vid],
                )?;
                anyhow::Ok(())
            })
            .unwrap();

        let result = handler
            .recall_trace(Parameters(RecallTraceInput {
                turn_id: "turn1".into(),
            }))
            .await
            .unwrap();
        let body = serde_json::from_str::<serde_json::Value>(
            result.content[0].as_text().unwrap().text.as_str(),
        )
        .unwrap();
        assert_eq!(body["count"], 1);
        assert_eq!(body["receipts"][0]["belief_id"], belief_id);
        assert_eq!(body["receipts"][0]["statement"], "User prefers Vim");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn audit_trail_filters_by_belief_substring() {
        let (handler, path) = handler_with_consent(false).await;
        handler
            .chain
            .log("test_op", "user", "payload-a", Some(r#"{"belief_id":"abc-123"}"#))
            .unwrap();
        handler
            .chain
            .log("test_op", "user", "payload-b", Some(r#"{"belief_id":"def-456"}"#))
            .unwrap();

        let result = handler
            .audit_trail(Parameters(AuditTrailInput {
                belief_id: Some("abc-123".into()),
                since: None,
                until: None,
                limit: None,
            }))
            .await
            .unwrap();
        let body = serde_json::from_str::<serde_json::Value>(
            result.content[0].as_text().unwrap().text.as_str(),
        )
        .unwrap();
        assert_eq!(body["count"], 1);
        assert!(body["head"]["seq"].as_i64().unwrap() >= 2);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn consistency_check_finds_contradicted_pairs() {
        let (handler, path) = handler_with_consent(false).await;
        let a = seed_belief(&handler.db, "User likes A", Status::Inferred, None);
        let b = seed_belief(&handler.db, "User likes B", Status::Contested, None);

        handler
            .db
            .with_conn(|conn| {
                let ledger = Ledger::new(conn);
                let b_belief = ledger.get_belief(&b)?.unwrap();
                ledger.add_provenance(
                    b_belief.current_version_id.as_deref().unwrap(),
                    crate::ledger::NewProvenance {
                        source_type: crate::ledger::SourceType::Belief,
                        source_id: a.clone(),
                        relation: crate::ledger::ProvenanceRelation::ContradictedBy,
                    },
                )?;
                anyhow::Ok(())
            })
            .unwrap();

        let result = handler
            .consistency_check(Parameters(ConsistencyCheckInput {
                belief_ids: vec![a.clone(), b.clone()],
            }))
            .await
            .unwrap();
        let body = serde_json::from_str::<serde_json::Value>(
            result.content[0].as_text().unwrap().text.as_str(),
        )
        .unwrap();
        assert_eq!(body["count"], 1);
        assert_eq!(body["contradictions"][0]["a"], b);
        assert_eq!(body["contradictions"][0]["b"], a);
        assert_eq!(body["contradictions"][0]["relation"], "contradicted_by");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn report_outcome_refuted_lands_correct_proposal() {
        let (handler, path) = handler_with_consent(true).await;
        let bid = seed_belief(&handler.db, "User uses Emacs", Status::Inferred, None);

        let result = handler
            .report_outcome(Parameters(ReportOutcomeInput {
                belief_id: bid.clone(),
                outcome: "refuted".into(),
                evidence: "user said Vim, not Emacs".into(),
            }))
            .await
            .unwrap();
        let body = serde_json::from_str::<serde_json::Value>(
            result.content[0].as_text().unwrap().text.as_str(),
        )
        .unwrap();
        let proposal_id = body["proposal_id"].as_str().unwrap();
        let proposal = handler
            .db
            .with_conn(|conn| proposals::get_proposal(conn, proposal_id))
            .unwrap()
            .unwrap();
        assert_eq!(proposal.kind, proposals::ProposalKind::Correct);
        assert_eq!(proposal.suggested_status.as_deref(), Some("contested"));
        assert_eq!(proposal.target_belief_id.as_deref(), Some(bid.as_str()));

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn report_outcome_confirmed_is_rejected() {
        let (handler, path) = handler_with_consent(true).await;
        let bid = seed_belief(&handler.db, "User uses Vim", Status::Inferred, None);
        let err = handler
            .report_outcome(Parameters(ReportOutcomeInput {
                belief_id: bid,
                outcome: "confirmed".into(),
                evidence: "user said Vim again".into(),
            }))
            .await;
        assert!(err.is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn correct_pattern_requires_a_filter() {
        let (handler, path) = handler_with_consent(true).await;
        let err = handler
            .correct_pattern(Parameters(CorrectPatternInput {
                statement_like: None,
                category: None,
                status: None,
                trust_class: None,
                suggested_status: "contested".into(),
                reason: "no scope".into(),
                limit: None,
            }))
            .await;
        assert!(err.is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn correct_pattern_lands_one_proposal_per_match() {
        let (handler, path) = handler_with_consent(true).await;
        seed_belief(&handler.db, "Pref one", Status::Inferred, Some("preference"));
        seed_belief(&handler.db, "Pref two", Status::Inferred, Some("preference"));
        seed_belief(&handler.db, "Fact one", Status::Inferred, Some("fact"));

        let result = handler
            .correct_pattern(Parameters(CorrectPatternInput {
                statement_like: None,
                category: Some("preference".into()),
                status: None,
                trust_class: None,
                suggested_status: "contested".into(),
                reason: "decommissioning preference feature".into(),
                limit: Some(50),
            }))
            .await
            .unwrap();
        let body = serde_json::from_str::<serde_json::Value>(
            result.content[0].as_text().unwrap().text.as_str(),
        )
        .unwrap();
        assert_eq!(body["count"], 2, "should propose one correction per preference belief");

        let pending = handler
            .db
            .with_conn(|conn| {
                proposals::list_proposals(conn, Some(proposals::ProposalStatus::Pending))
            })
            .unwrap();
        assert_eq!(pending.len(), 2);
        for p in &pending {
            assert_eq!(p.kind, proposals::ProposalKind::Correct);
            assert_eq!(p.suggested_status.as_deref(), Some("contested"));
        }

        let _ = std::fs::remove_file(&path);
    }

    // Resource handlers go through `RequestContext` which rmcp doesn't
    // expose a constructor for outside the crate. We test the inner
    // builders directly here; the public `read_resource` path is covered
    // end-to-end by the server.rs HTTP smoke test once we layer an MCP
    // session over it.

    #[tokio::test]
    async fn resource_recent_serializes_beliefs() {
        let (handler, path) = handler_with_consent(false).await;
        seed_belief(&handler.db, "B1", Status::Inferred, None);
        seed_belief(&handler.db, "B2", Status::Asserted, None);
        let body = handler.resource_recent().unwrap();
        assert_eq!(body["count"], 2);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn resource_contradictions_groups_by_status() {
        let (handler, path) = handler_with_consent(false).await;
        seed_belief(&handler.db, "X", Status::Contested, None);
        seed_belief(&handler.db, "Y", Status::Corrected, None);
        seed_belief(&handler.db, "Z", Status::Asserted, None);
        let body = handler.resource_contradictions().unwrap();
        assert_eq!(body["contested"].as_array().unwrap().len(), 1);
        assert_eq!(body["corrected"].as_array().unwrap().len(), 1);
        assert_eq!(body["expired"].as_array().unwrap().len(), 0);
        assert_eq!(body["total"], 2);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn resource_audit_head_returns_null_when_empty() {
        let (handler, path) = handler_with_consent(false).await;
        let body = handler.resource_audit_head().unwrap();
        assert!(body["head"].is_null());
        // After a write, head() returns Some.
        handler.chain.log("op", "user", "x", None).unwrap();
        let body = handler.resource_audit_head().unwrap();
        assert!(body["head"].is_object());
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
                bv.statement, bv.confidence,
                (SELECT COUNT(*) FROM belief_provenance bp
                  JOIN belief_versions bv2 ON bv2.id = bp.belief_version_id
                  WHERE bv2.belief_id = b.id AND bp.relation = 'reinforced_by') AS reinforced_count,
                (SELECT COUNT(*) FROM belief_provenance bp
                  JOIN belief_versions bv2 ON bv2.id = bp.belief_version_id
                  WHERE bv2.belief_id = b.id AND bp.relation = 'contradicted_by') AS contradicted_count,
                b.num_times, b.last_observed_at
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
    sql.push_str(" ORDER BY b.updated_at DESC LIMIT ?");
    binds.push(Box::new(limit));

    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |r| {
        let trust_class: String = r.get(5)?;
        let status: String = r.get(4)?;
        let created_at: String = r.get(10)?;
        let last_reinforced_at: Option<String> = r.get(12)?;
        let stored: Option<f64> = r.get(14)?;
        let reinforced_count: i64 = r.get(15)?;
        let contradicted_count: i64 = r.get(16)?;
        let num_times: i64 = r.get(17)?;
        let last_observed_at: Option<String> = r.get(18)?;
        // Structural confidence — never self-reported. See confidence.rs.
        let eff = crate::confidence::effective_for(
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
        Ok(json!({
            "id": r.get::<_, String>(0)?,
            "subject": r.get::<_, String>(1)?,
            "category": r.get::<_, Option<String>>(2)?,
            "current_version_id": r.get::<_, Option<String>>(3)?,
            "status": status,
            "trust_class": trust_class,
            "scope": r.get::<_, String>(6)?,
            "scope_ref_id": r.get::<_, Option<String>>(7)?,
            "level": r.get::<_, i32>(8)?,
            "parent_summary_id": r.get::<_, Option<String>>(9)?,
            "created_at": created_at,
            "updated_at": r.get::<_, String>(11)?,
            "last_reinforced_at": last_reinforced_at,
            "reinforced_count": reinforced_count,
            "contradicted_count": contradicted_count,
            "num_times": num_times,
            "statement": r.get::<_, Option<String>>(13)?,
            "confidence": eff.score,
            "confidence_bucket": eff.bucket.as_str(),
        }))
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}
