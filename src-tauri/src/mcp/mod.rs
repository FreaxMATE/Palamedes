//! MCP belief-schema server (Phase C).
//!
//! Exposes the Belief Ledger to external AIs (Claude Desktop, Cursor,
//! Witsy, Open WebUI) over MCP — read tools list/get/search the corpus;
//! write tools `propose_belief` / `correct_belief` land in an audit
//! inbox the user reviews. External writes never auto-assert; the
//! audit panel is the only gate.
//!
//! Wire layout:
//! - `proposals` — CRUD on `belief_proposals` + accept materialization
//! - `consent`   — per-client consent state, lookup + decisions
//! - `audit`     — read-tool audit log + per-client rate limit
//! - `handlers`  — the 5 rmcp tool handlers (Day 3-4)
//! - `server`    — axum HTTP+SSE host, bearer-token middleware (Day 3-5)
//!
//! See `.plans/PHASE_C.md` for the full design + timeline.

pub mod audit;
pub mod consent;
pub mod handlers;
pub mod proposals;
pub mod server;
