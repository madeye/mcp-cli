mod backends;
mod buffer_pool;
mod changelog;
mod compact;
mod framing;
mod handlers;
mod languages;
mod metrics;
mod outline;
mod parse_cache;
mod prewarm;
mod search_cache;
mod server;
mod watcher;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

// mimalloc gives the daemon a meaningful win on the tree-sitter /
// hashmap-heavy hot paths and matters more once future arenas and
// pooled buffers stack on top of it. Only the daemon needs it — the
// bridge and installer are short-lived processes. Opt out with
// `--no-default-features` for valgrind / heaptrack / ASan runs.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Debug, Parser)]
#[command(name = "mcp-cli-daemon", about = "Sidecar daemon for MCP-CLI")]
struct Args {
    /// Unix domain socket path the daemon listens on. Defaults to a
    /// deterministic per-root path so the bridge can find it without
    /// any configuration.
    #[arg(long)]
    socket: Option<PathBuf>,

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

    /// Shut down cleanly when no bridge has been connected for this long.
    /// Accepts human-friendly durations like `30m`, `1h`, `10s`, or `0`
    /// to disable. Pairs with the bridge's auto-spawn so idle daemons
    /// don't linger after the agent session ends.
    #[arg(long, default_value = "30m")]
    idle_timeout: String,

    /// Enable Linux io_uring I/O mode. Rejected on non-Linux hosts.
    #[arg(long, default_value_t = false)]
    io_uring: bool,

    /// Size the Tokio runtime explicitly to one worker per logical CPU.
    #[arg(long, default_value_t = true)]
    thread_per_core: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let worker_threads = if args.thread_per_core {
        num_cpus::get().max(1)
    } else {
        1
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .build()?;
    rt.block_on(run(args, worker_threads))
}

async fn run(args: Args, worker_threads: usize) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let root = match args.root {
        Some(r) => r,
        None => std::env::current_dir()?,
    };
    let root = root.canonicalize()?;
    let socket = args
        .socket
        .unwrap_or_else(|| protocol::paths::socket_path_for(&root));

    if args.changelog_capacity == 0 {
        anyhow::bail!("--changelog-capacity must be > 0");
    }
    let idle_timeout = parse_idle_timeout(&args.idle_timeout).context("parsing --idle-timeout")?;

    tracing::info!(
        socket = %socket.display(),
        root = %root.display(),
        changelog_capacity = args.changelog_capacity,
        search_cache_capacity = args.search_cache_capacity,
        parse_cache_capacity = args.parse_cache_capacity,
        prewarm = !args.no_prewarm,
        idle_timeout = ?idle_timeout,
        io_uring = args.io_uring,
        worker_threads,
        "starting daemon",
    );
    server::serve(server::Config {
        socket,
        root,
        changelog_capacity: args.changelog_capacity,
        search_cache_capacity: args.search_cache_capacity,
        parse_cache_capacity: args.parse_cache_capacity,
        prewarm_enabled: !args.no_prewarm,
        idle_timeout,
        io_uring_enabled: args.io_uring,
    })
    .await
}

fn parse_idle_timeout(raw: &str) -> Result<Option<Duration>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "0" {
        return Ok(None);
    }
    let d = humantime::parse_duration(trimmed)
        .with_context(|| format!("invalid duration: {trimmed}"))?;
    if d.is_zero() {
        Ok(None)
    } else {
        Ok(Some(d))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_disables_idle_timeout() {
        assert_eq!(parse_idle_timeout("0").unwrap(), None);
        assert_eq!(parse_idle_timeout("").unwrap(), None);
    }

    #[test]
    fn parses_humantime_duration() {
        assert_eq!(
            parse_idle_timeout("30m").unwrap(),
            Some(Duration::from_secs(30 * 60))
        );
        assert_eq!(
            parse_idle_timeout("1h").unwrap(),
            Some(Duration::from_secs(3600))
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_idle_timeout("abc").is_err());
    }
}
