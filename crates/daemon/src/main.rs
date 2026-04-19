mod changelog;
mod framing;
mod handlers;
mod server;
mod watcher;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "mcp-cli-daemon", about = "Sidecar daemon for MCP-CLI")]
struct Args {
    /// Unix domain socket path the daemon listens on.
    #[arg(long, default_value = "/tmp/mcp-cli.sock")]
    socket: PathBuf,

    /// Project root the daemon serves (defaults to current dir).
    #[arg(long)]
    root: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    let root = match args.root {
        Some(r) => r,
        None => std::env::current_dir()?,
    };
    let root = root.canonicalize()?;

    tracing::info!(socket = %args.socket.display(), root = %root.display(), "starting daemon");
    server::serve(args.socket, root).await
}
