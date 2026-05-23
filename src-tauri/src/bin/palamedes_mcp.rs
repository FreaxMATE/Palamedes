//! `palamedes-mcp` — stdio MCP shim that proxies to the running Palamedes
//! Tauri app's HTTP+SSE server. Lets Claude Desktop (which only speaks
//! stdio) talk to Palamedes without bringing the whole rmcp server
//! surface into a second binary.
//!
//! Wire format:
//! - stdin: newline-delimited JSON-RPC messages, one per line
//! - stdout: same, transparent-pass-through of the server's responses
//! - stderr: human-readable diagnostics (logs / setup errors)
//!
//! For each stdin line we POST it to `$PALAMEDES_MCP_URL` with
//! `Authorization: Bearer $PALAMEDES_MCP_TOKEN`. The streamable-HTTP MCP
//! transport may return either:
//! - `application/json` — a single response object, emitted as one line
//! - `text/event-stream` — one or more SSE events; we strip the `data:`
//!   prefix and emit each event payload as its own line
//!
//! The `Mcp-Session-Id` header established on the first response is
//! cached and sent on every subsequent request, per the streamable HTTP
//! spec.
//!
//! IMPORTANT: per the MCP stdio convention, the only thing written to
//! stdout is well-formed JSON-RPC. Diagnostics and errors go to stderr.

use std::io::Write;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

const ACCEPT_HEADER: &str = "application/json, text/event-stream";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let url = std::env::var("PALAMEDES_MCP_URL")
        .context("PALAMEDES_MCP_URL not set — point this at e.g. http://127.0.0.1:5180/mcp")?;
    let token = std::env::var("PALAMEDES_MCP_TOKEN")
        .context("PALAMEDES_MCP_TOKEN not set — copy it from Palamedes Settings → Connections")?;

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;

    // Session id is established on the first initialize response and
    // must be echoed on every subsequent request.
    let session_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));

    while let Some(line) = reader.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let url = url.clone();
        let token = token.clone();
        let http = http.clone();
        let session = session_id.clone();
        let stdout = stdout.clone();

        // Sequential request handling — Claude Desktop's stdio loop is
        // single-threaded so serializing here matches its expectations
        // and keeps Mcp-Session-Id updates race-free.
        if let Err(e) = forward_one(&http, &url, &token, &session, &stdout, trimmed).await {
            eprintln!("palamedes-mcp: request failed: {e:#}");
        }
    }
    Ok(())
}

async fn forward_one(
    http: &reqwest::Client,
    url: &str,
    token: &str,
    session: &Arc<Mutex<Option<String>>>,
    stdout: &Arc<Mutex<tokio::io::Stdout>>,
    body: &str,
) -> Result<()> {
    let mut req = http
        .post(url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, ACCEPT_HEADER)
        .body(body.to_string());
    if let Some(id) = session.lock().await.clone() {
        req = req.header("Mcp-Session-Id", id);
    }
    let resp = req.send().await?;

    let status = resp.status();
    // Cache Mcp-Session-Id from this response, if the server set one.
    if let Some(id) = resp.headers().get("Mcp-Session-Id") {
        if let Ok(s) = id.to_str() {
            *session.lock().await = Some(s.to_string());
        }
    }
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "server returned {} {}: {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or(""),
            text
        ));
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    if content_type.starts_with("text/event-stream") {
        stream_sse(resp, stdout).await
    } else {
        let bytes = resp.bytes().await?;
        write_line(stdout, &bytes).await
    }
}

/// Parse an SSE byte stream and emit each `data:` event payload as a
/// stdout line. Other SSE fields (event:, id:, retry:) are ignored —
/// MCP only uses `data:`.
async fn stream_sse(resp: reqwest::Response, stdout: &Arc<Mutex<tokio::io::Stdout>>) -> Result<()> {
    let mut buf = String::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        let s = std::str::from_utf8(&bytes).unwrap_or("");
        buf.push_str(s);
        // Process complete events — SSE separates events with a blank line.
        while let Some(end) = find_event_boundary(&buf) {
            let event_block = buf[..end].to_string();
            buf.drain(..end + 2); // consume the \n\n delimiter
            if let Some(data) = collect_data_lines(&event_block) {
                write_line(stdout, data.as_bytes()).await?;
            }
        }
    }
    // Flush any trailing event without a terminator (rare but defensive).
    if let Some(data) = collect_data_lines(&buf) {
        if !data.is_empty() {
            write_line(stdout, data.as_bytes()).await?;
        }
    }
    Ok(())
}

fn find_event_boundary(s: &str) -> Option<usize> {
    s.find("\n\n")
}

/// Concatenate every `data:` field in an SSE event block per the spec
/// (multiple `data:` lines are joined with `\n`).
fn collect_data_lines(event_block: &str) -> Option<String> {
    let mut out = String::new();
    let mut found = false;
    for line in event_block.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            if found {
                out.push('\n');
            }
            out.push_str(rest.trim_start());
            found = true;
        }
    }
    if found {
        Some(out)
    } else {
        None
    }
}

async fn write_line(stdout: &Arc<Mutex<tokio::io::Stdout>>, bytes: &[u8]) -> Result<()> {
    let mut out = stdout.lock().await;
    out.write_all(bytes).await?;
    if !bytes.ends_with(b"\n") {
        out.write_all(b"\n").await?;
    }
    out.flush().await?;
    let _ = std::io::stderr().flush();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_event_with_single_data_line() {
        let block = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}";
        let got = collect_data_lines(block).unwrap();
        assert_eq!(got, "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}");
    }

    #[test]
    fn sse_event_with_multiple_data_lines_concatenates() {
        let block = "event: message\ndata: line1\ndata: line2\nid: 5";
        let got = collect_data_lines(block).unwrap();
        assert_eq!(got, "line1\nline2");
    }

    #[test]
    fn sse_event_without_data_is_skipped() {
        let block = "event: heartbeat\nid: 5";
        assert!(collect_data_lines(block).is_none());
    }

    #[test]
    fn boundary_detection_finds_double_newline() {
        let buf = "data: foo\n\ndata: bar\n\n";
        assert_eq!(find_event_boundary(buf), Some(9));
    }
}
