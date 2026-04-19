mod daemon_client;
mod mcp;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "mcp-cli-bridge",
    about = "MCP stdio bridge that fronts the sidecar daemon"
)]
struct Args {
    /// UDS path of the running daemon.
    #[arg(long, default_value = "/tmp/mcp-cli.sock")]
    socket: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    mcp::run(args.socket).await
}
