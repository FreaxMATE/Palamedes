//! Headless MCP server � binds the Belief Ledger HTTP endpoint without GTK.
//!
//! Usage:
//!   cd src-tauri && cargo run --bin palamedes-mcp-serve
//!
//! Honors `PALAMEDES_DATA_DIR` (defaults to the Tauri app data dir on Linux).

use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let data_dir = std::env::var("PALAMEDES_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|_| -> anyhow::Result<PathBuf> {
            let home = std::env::var("HOME")?;
            Ok(PathBuf::from(home).join(".local/share/dev.palamedes.app"))
        })?;
    let handle = palamedes_lib::headless_mcp_serve(data_dir).await?;
    eprintln!("Palamedes MCP: {}", handle.url());
    eprintln!("Press Ctrl+C to stop.");
    tokio::signal::ctrl_c().await?;
    handle.shutdown().await;
    Ok(())
}
