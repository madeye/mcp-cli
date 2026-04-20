mod changelog;
mod framing;
mod handlers;
mod languages;
mod outline;
mod parse_cache;
mod prewarm;
mod search_cache;
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

    /// Capacity (in events) of the in-memory ChangeLog ring buffer.
    /// Larger values tolerate slower clients at the cost of memory.
    #[arg(long, default_value_t = 4096)]
    changelog_capacity: usize,

    /// Capacity (in entries) of the `search.grep` LRU. The cache is flushed
    /// whenever the ChangeLog version advances, so this just caps memory for
    /// repeat queries within a single quiescent window. Set to 0 to disable.
    #[arg(long, default_value_t = 64)]
    search_cache_capacity: usize,

    /// Capacity (in files) of the per-file tree-sitter parse cache used by
    /// `code.outline` / `code.symbols`. Entries are validated by mtime +
    /// size on every access. Set to 0 to disable caching (each call
    /// re-parses from disk).
    #[arg(long, default_value_t = 256)]
    parse_cache_capacity: usize,

    /// Skip the startup pre-warm walk that pages source files into the OS
    /// cache. The walk runs once, in the background, and does not block
    /// incoming requests; this flag exists for benchmarks and tests where
    /// warm-cache behaviour should be controlled explicitly.
    #[arg(long, default_value_t = false)]
    no_prewarm: bool,
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

    if args.changelog_capacity == 0 {
        anyhow::bail!("--changelog-capacity must be > 0");
    }

    tracing::info!(
        socket = %args.socket.display(),
        root = %root.display(),
        changelog_capacity = args.changelog_capacity,
        search_cache_capacity = args.search_cache_capacity,
        parse_cache_capacity = args.parse_cache_capacity,
        prewarm = !args.no_prewarm,
        "starting daemon",
    );
    server::serve(
        args.socket,
        root,
        args.changelog_capacity,
        args.search_cache_capacity,
        args.parse_cache_capacity,
        !args.no_prewarm,
    )
    .await
}
