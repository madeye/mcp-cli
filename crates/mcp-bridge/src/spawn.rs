//! Auto-spawn the daemon when the bridge cannot reach it.
//!
//! Called from `mcp::run` on `ENOENT`/`ECONNREFUSED`. The bridge forks
//! and execs `mcp-cli-daemon`, detaches it via `setsid`, redirects its
//! stdio to a per-root log file, and then retry-connects with short
//! backoff. A concurrent bridge invocation for the same project root
//! will race to spawn — one wins the `bind(2)` and the others silently
//! connect to the winner's socket.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::net::UnixStream;
use tokio::time::sleep;

/// Total budget for the retry-connect loop after we fire off the spawn.
const CONNECT_BUDGET: Duration = Duration::from_millis(2_000);
/// Starting backoff between connection attempts. Doubles up to ~320ms.
const INITIAL_BACKOFF: Duration = Duration::from_millis(25);
const MAX_BACKOFF: Duration = Duration::from_millis(320);

pub struct SpawnArgs<'a> {
    pub socket: &'a Path,
    pub root: &'a Path,
    pub daemon_bin: Option<&'a Path>,
    pub extra_args: &'a [String],
}

/// Spawn the daemon for `root` listening on `socket`, then connect.
pub async fn spawn_and_connect(args: SpawnArgs<'_>) -> Result<UnixStream> {
    let daemon = resolve_daemon_bin(args.daemon_bin)?;
    let log_path = daemon_log_path(args.socket);
    tracing::info!(
        daemon = %daemon.display(),
        root = %args.root.display(),
        socket = %args.socket.display(),
        log = %log_path.display(),
        "auto-spawning daemon",
    );
    spawn_detached(&daemon, args.root, args.socket, &log_path, args.extra_args)
        .with_context(|| format!("spawning {}", daemon.display()))?;
    retry_connect(args.socket, CONNECT_BUDGET).await
}

/// Retry-connect after a spawn. Exposed so the same backoff policy can
/// be used on transient reconnects (future M5 work). Only retries on
/// connect errors that plausibly clear up once the daemon finishes
/// binding — a `PermissionDenied` or `InvalidInput` means the caller's
/// environment is wrong and no amount of waiting will help.
pub async fn retry_connect(socket: &Path, budget: Duration) -> Result<UnixStream> {
    let deadline = std::time::Instant::now() + budget;
    let mut backoff = INITIAL_BACKOFF;
    loop {
        match UnixStream::connect(socket).await {
            Ok(s) => return Ok(s),
            Err(e) if !is_connect_retryable(&e) => {
                return Err(anyhow::Error::from(e)
                    .context(format!("connect {} (non-retryable)", socket.display())));
            }
            Err(e) => {
                if std::time::Instant::now() >= deadline {
                    return Err(anyhow!(
                        "daemon did not accept connections within {:?}: {}",
                        budget,
                        e,
                    ));
                }
                sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

pub fn is_connect_retryable(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

fn resolve_daemon_bin(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    if let Ok(self_exe) = std::env::current_exe() {
        if let Some(parent) = self_exe.parent() {
            let candidate = parent.join(bin_name("mcp-cli-daemon"));
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    // Fall back to $PATH resolution by execvp.
    Ok(PathBuf::from(bin_name("mcp-cli-daemon")))
}

fn bin_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

/// Per-socket log file. Lives next to the socket so it's cleaned up
/// implicitly when the runtime dir is purged.
fn daemon_log_path(socket: &Path) -> PathBuf {
    let mut p = socket.to_path_buf();
    p.set_extension("log");
    p
}

#[cfg(unix)]
fn spawn_detached(
    daemon: &Path,
    root: &Path,
    socket: &Path,
    log: &Path,
    extra_args: &[String],
) -> Result<()> {
    use std::os::unix::process::CommandExt;

    protocol::paths::ensure_socket_parent(socket).with_context(|| {
        format!(
            "creating socket parent dir for {} (check filesystem permissions)",
            socket.display()
        )
    })?;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("opening daemon log {}", log.display()))?;
    let stderr = log_file.try_clone().context("cloning log fd for stderr")?;

    let mut cmd = std::process::Command::new(daemon);
    cmd.arg("--root")
        .arg(root)
        .arg("--socket")
        .arg(socket)
        .args(extra_args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(stderr));

    // SAFETY: setsid is async-signal-safe and fine to call between fork
    // and exec; the closure only touches kernel state, no allocations.
    unsafe {
        cmd.pre_exec(|| {
            // Detach from controlling tty / process group so the daemon
            // survives the bridge exiting.
            if libc_setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    cmd.spawn()
        .with_context(|| format!("spawning daemon {}", daemon.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn spawn_detached(
    daemon: &Path,
    root: &Path,
    socket: &Path,
    log: &Path,
    extra_args: &[String],
) -> Result<()> {
    protocol::paths::ensure_socket_parent(socket).with_context(|| {
        format!(
            "creating socket parent dir for {} (check filesystem permissions)",
            socket.display()
        )
    })?;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("opening daemon log {}", log.display()))?;
    let stderr = log_file.try_clone().context("cloning log fd for stderr")?;
    std::process::Command::new(daemon)
        .arg("--root")
        .arg(root)
        .arg("--socket")
        .arg(socket)
        .args(extra_args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(stderr))
        .spawn()
        .with_context(|| format!("spawning daemon {}", daemon.display()))?;
    Ok(())
}

#[cfg(unix)]
extern "C" {
    fn setsid() -> i32;
}

#[cfg(unix)]
fn libc_setsid() -> i32 {
    // Own tiny wrapper so we don't take a libc dep just for this.
    unsafe { setsid() }
}
