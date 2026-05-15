//! `clam-mcp` binary entry point. Loads config + wallet, then serves an MCP
//! `ServerHandler` over stdio.
//!
//! `stdout` is reserved for MCP frames; all diagnostics go to `stderr` via
//! `tracing-subscriber`.

mod server;

use anyhow::{Context, Result};
use clam_core::{ClamX402Client, Config, Ledger, Wallet};
use rmcp::{transport::stdio, ServiceExt};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    init_tracing();

    let config = Config::from_env().context("loading config from environment")?;
    config
        .ensure_dirs()
        .context("creating config directories")?;

    tracing::info!(
        network = config.network.as_str(),
        rpc_url = %config.rpc_url,
        facilitator_url = %config.facilitator_url,
        keypair_path = %config.keypair_path.display(),
        ledger_path = %config.ledger_path.display(),
        "clam-mcp starting"
    );

    let wallet = Wallet::from_config(&config).with_context(|| {
        format!(
            "loading keypair at {} (generate one with `solana-keygen new --outfile {}`)",
            config.keypair_path.display(),
            config.keypair_path.display()
        )
    })?;

    tracing::info!(address = %wallet.pubkey(), "wallet loaded");

    let ledger = Ledger::new(config.ledger_path.clone());
    let x402 = ClamX402Client::new(wallet.clone(), config.clone(), ledger.clone());
    let server = server::ClamServer::new(wallet, ledger, x402);

    let service = server
        .serve(stdio())
        .await
        .context("serving MCP over stdio")?;
    let reason = service.waiting().await?;
    tracing::info!(?reason, "clam-mcp exiting");
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_env("CLAM_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_ansi(false);
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .try_init();
}
