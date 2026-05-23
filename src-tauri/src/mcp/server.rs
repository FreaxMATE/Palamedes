//! MCP HTTP+SSE server lifecycle.
//!
//! Hosts `PalamedesMcpHandler` behind rmcp's `StreamableHttpService`,
//! bound to `127.0.0.1:<port>`. A bearer token (stored in `settings`)
//! gates the `/mcp` endpoint via an axum middleware layer — anything
//! without a valid `Authorization: Bearer <token>` header gets 401.
//!
//! Concurrency: one server per app. Start/stop is idempotent;
//! double-start returns the existing handle, double-stop is a no-op.

use std::sync::Arc;

use axum::{
    extract::State as AxumState,
    http::{HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::Response,
    Router,
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::db::Db;
use crate::mcp::handlers::PalamedesMcpHandler;
use crate::nebius::NebiusClient;

/// Live MCP server. Drop / `shutdown()` to stop.
pub struct McpServerHandle {
    pub port: u16,
    pub token: String,
    cancel: CancellationToken,
    join: Option<JoinHandle<()>>,
}

impl McpServerHandle {
    /// Cancel the server task and wait for it to drain. Idempotent —
    /// calling twice is harmless.
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(handle) = self.join.take() {
            let _ = handle.await;
        }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }
}

/// Start the MCP server on `127.0.0.1:port`. Requires `Authorization:
/// Bearer <token>` on every request.
pub async fn start(
    db: Arc<Db>,
    client: NebiusClient,
    port: u16,
    token: String,
) -> anyhow::Result<McpServerHandle> {
    let cancel = CancellationToken::new();

    // Factory: one handler per session. Db (Arc) and NebiusClient are
    // cheap to clone — they share state. The handler's own client_id
    // RwLock is fresh per session.
    let db_factory = db.clone();
    let client_factory = client.clone();
    let factory = move || {
        Ok(PalamedesMcpHandler::new(
            db_factory.clone(),
            client_factory.clone(),
        ))
    };

    let mcp_service = StreamableHttpService::new(
        factory,
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default().with_cancellation_token(cancel.child_token()),
    );

    let router = Router::new()
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn_with_state(
            BearerAuthState {
                token: token.clone(),
            },
            bearer_auth,
        ));

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let bound_port = listener.local_addr()?.port();
    let cancel_for_task = cancel.child_token();
    let join = tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { cancel_for_task.cancelled().await })
            .await;
    });

    Ok(McpServerHandle {
        port: bound_port,
        token,
        cancel,
        join: Some(join),
    })
}

#[derive(Clone)]
struct BearerAuthState {
    token: String,
}

/// Reject any request whose `Authorization` header doesn't match
/// `Bearer <token>`. Constant-time compare so timing leaks don't help
/// guessing.
async fn bearer_auth(
    AxumState(state): AxumState<BearerAuthState>,
    headers: HeaderMap,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let provided = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    if !constant_time_eq(provided.as_bytes(), state.token.as_bytes()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(request).await)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Generate a fresh bearer token. 64 hex chars (256 bits) from two
/// concatenated UUIDv4s — avoids pulling in a separate RNG crate.
pub fn generate_token() -> String {
    let a = uuid::Uuid::new_v4().simple().to_string();
    let b = uuid::Uuid::new_v4().simple().to_string();
    format!("{a}{b}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: server boots on port 0 (auto-assigned), the bearer
    /// middleware 401s unauthenticated requests, and an MCP initialize
    /// with the right token succeeds.
    #[tokio::test]
    async fn server_starts_and_gates_on_bearer_token() {
        crate::embeddings::register_vec_extension();
        let path = std::env::temp_dir()
            .join(format!("palamedes-mcp-test-{}.db", uuid::Uuid::new_v4()));
        let db = Arc::new(Db::open(&path).unwrap());
        let client = NebiusClient::from_api_key("test-key".into());
        let token = "test-bearer-token-1234".to_string();

        let handle = start(db, client, 0, token.clone()).await.unwrap();
        let url = handle.url();

        let http = reqwest::Client::new();

        // No Authorization header → 401.
        let init_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "smoke-test-client", "version": "0.0.1" }
            }
        });
        let r_anon = http
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .json(&init_body)
            .send()
            .await
            .unwrap();
        assert_eq!(r_anon.status(), reqwest::StatusCode::UNAUTHORIZED);

        // Wrong token → 401.
        let r_bad = http
            .post(&url)
            .header("Authorization", "Bearer not-the-real-token")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .json(&init_body)
            .send()
            .await
            .unwrap();
        assert_eq!(r_bad.status(), reqwest::StatusCode::UNAUTHORIZED);

        // Correct token → 2xx, and the client row appears in mcp_clients.
        let r_ok = http
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .json(&init_body)
            .send()
            .await
            .unwrap();
        assert!(
            r_ok.status().is_success(),
            "initialize failed: {} body={:?}",
            r_ok.status(),
            r_ok.text().await.ok()
        );

        handle.shutdown().await;
        let _ = std::fs::remove_file(&path);
    }
}
