mod daemon_client;
mod mcp;
mod spawn;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "mcp-cli-bridge",
    about = "MCP stdio bridge that fronts the sidecar daemon"
)]
struct Args {
    /// Project root. Defaults to the current working directory — the
    /// agent spawns the bridge from inside the project, so cwd is the
    /// right root without per-agent config.
    #[arg(long)]
    root: Option<PathBuf>,

    /// Override the UDS path. Defaults to a deterministic per-root
    /// location under `$XDG_RUNTIME_DIR/mcp-cli/` or `/tmp/`.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Path to the daemon binary. Defaults to `mcp-cli-daemon` resolved
    /// next to the bridge binary (then falling back to $PATH).
    #[arg(long)]
    daemon_bin: Option<PathBuf>,

    /// Do not auto-spawn a daemon if the socket is unreachable. Intended
    /// for tests and managed deployments where an external supervisor
    /// owns daemon lifecycle.
    #[arg(long, default_value_t = false)]
    no_autospawn: bool,

    /// Extra CLI argument to forward to the spawned daemon. Repeatable.
    /// The bridge already passes --root and --socket; use this to set
    /// flags like `--idle-timeout 5m` or `--no-prewarm`. Values may
    /// start with `--` (e.g. `--daemon-arg=--idle-timeout`).
    #[arg(long = "daemon-arg", value_name = "ARG", allow_hyphen_values = true)]
    daemon_args: Vec<String>,
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
    let root = match args.root {
        Some(r) => r,
        None => std::env::current_dir().context("resolving current directory")?,
    };
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalize root {}", root.display()))?;
    let socket = args
        .socket
        .unwrap_or_else(|| protocol::paths::socket_path_for(&root));

    mcp::run(mcp::RunConfig {
        socket,
        root,
        daemon_bin: args.daemon_bin,
        autospawn: !args.no_autospawn,
        daemon_extra_args: args.daemon_args,
    })
    .await
}
