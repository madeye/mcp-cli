use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use protocol::{Request, Response, RpcError};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;

use crate::backends::{BackendRegistry, TreeSitterBackend};
use crate::buffer_pool::BufferPool;
use crate::changelog::ChangeLog;
use crate::framing::{read_frame_into, write_frame, MAX_FRAME};
use crate::handlers;
use crate::metrics::ToolMetrics;
use crate::parse_cache::ParseCache;
use crate::prewarm;
use crate::search_cache::SearchCache;
use crate::watcher::{self, WatchHandle};

/// Cap on recycled frame buffers above which `BufferPool` drops storage
/// instead of recycling. 256 KiB covers every request payload we've
/// seen in practice (most are <4 KiB JSON); the cap exists so a stray
/// near-`MAX_FRAME` request can't bloat the pool's resident memory.
const FRAME_BUFFER_RECYCLE_CAP: usize = 256 * 1024;

// Compile-time guard: a recycle cap at or above MAX_FRAME would let the
// pool keep arbitrarily large buffers, defeating the cap. Both values
// are `const`s, so a release build can't ship with this misconfigured.
const _: () = assert!(
    FRAME_BUFFER_RECYCLE_CAP < MAX_FRAME as usize,
    "FRAME_BUFFER_RECYCLE_CAP must stay below MAX_FRAME or the cap is meaningless",
);

pub struct Daemon {
    pub root: PathBuf,
    pub changelog: Arc<ChangeLog>,
    pub search_cache: Arc<SearchCache>,
    pub backends: BackendRegistry,
    pub frame_pool: Arc<BufferPool>,
    pub metrics: Arc<ToolMetrics>,
}

pub struct Config {
    pub socket: PathBuf,
    pub root: PathBuf,
    pub changelog_capacity: usize,
    pub search_cache_capacity: usize,
    pub parse_cache_capacity: usize,
    pub prewarm_enabled: bool,
    pub idle_timeout: Option<Duration>,
}

pub async fn serve(cfg: Config) -> Result<()> {
    let socket_path = cfg.socket.clone();
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)
            .with_context(|| format!("removing stale socket {}", socket_path.display()))?;
    }
    protocol::paths::ensure_socket_parent(&socket_path)
        .with_context(|| format!("creating socket parent dir for {}", socket_path.display()))?;
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("binding {}", socket_path.display()))?;

    let changelog = Arc::new(ChangeLog::with_capacity(cfg.changelog_capacity));
    let search_cache = Arc::new(SearchCache::new(cfg.search_cache_capacity));
    let parse_cache = Arc::new(ParseCache::new(cfg.parse_cache_capacity));
    let _watch: WatchHandle =
        watcher::spawn(cfg.root.clone(), changelog.clone(), parse_cache.clone())?;
    if cfg.prewarm_enabled {
        prewarm::spawn(cfg.root.clone());
    }
    // Default backend stack: tree-sitter as the generalist fallback. Future
    // milestones register specialist backends (rust-analyzer, clangd) ahead
    // of this so they get first refusal on the languages they cover.
    let mut backends = BackendRegistry::new();
    backends.register(Arc::new(TreeSitterBackend::new(parse_cache.clone())));
    // Pool size = a small multiple of the expected concurrent-bridge
    // count; one entry per in-flight request frame is enough. Buffers
    // beyond FRAME_BUFFER_RECYCLE_CAP are dropped on return.
    let frame_pool = Arc::new(BufferPool::new(32, FRAME_BUFFER_RECYCLE_CAP));
    let metrics = Arc::new(ToolMetrics::new());
    let daemon = Arc::new(Daemon {
        root: cfg.root,
        changelog,
        search_cache,
        backends,
        frame_pool,
        metrics,
    });

    let idle = Arc::new(IdleTracker::new(cfg.idle_timeout));

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let idle_fired = idle.clone();
    let idle_signal = async move {
        if idle_fired.timeout.is_some() {
            idle_fired.wait_for_timeout().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(idle_signal);

    loop {
        tokio::select! {
            res = listener.accept() => {
                let (stream, _) = res?;
                let daemon = daemon.clone();
                let idle = idle.clone();
                idle.on_connect();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, daemon).await {
                        tracing::warn!(error = %e, "connection ended with error");
                    }
                    idle.on_disconnect();
                });
            }
            _ = &mut ctrl_c => {
                tracing::info!("shutting down (ctrl-c)");
                break;
            }
            _ = &mut idle_signal => {
                tracing::info!(
                    idle_timeout = ?idle.timeout,
                    "shutting down (idle timeout)",
                );
                break;
            }
        }
    }
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

/// Tracks the moment the last bridge disconnected so a background poll
/// can shut the daemon down after `timeout` with no activity. The poll
/// lives inside `wait_for_timeout` so there is exactly one waker, which
/// is fine for the "quiet, waiting to die" state.
struct IdleTracker {
    active: AtomicUsize,
    last_idle_at: Mutex<Option<Instant>>,
    timeout: Option<Duration>,
    tick: Notify,
}

impl IdleTracker {
    fn new(timeout: Option<Duration>) -> Self {
        // Start "idle" — a daemon nobody ever connects to should also
        // exit after the timeout.
        Self {
            active: AtomicUsize::new(0),
            last_idle_at: Mutex::new(Some(Instant::now())),
            timeout,
            tick: Notify::new(),
        }
    }

    fn on_connect(&self) {
        self.active.fetch_add(1, Ordering::SeqCst);
        *self.last_idle_at.lock() = None;
        self.tick.notify_waiters();
    }

    fn on_disconnect(&self) {
        let prev = self.active.fetch_sub(1, Ordering::SeqCst);
        if prev == 1 {
            *self.last_idle_at.lock() = Some(Instant::now());
            self.tick.notify_waiters();
        }
    }

    async fn wait_for_timeout(&self) {
        let Some(timeout) = self.timeout else {
            return;
        };
        loop {
            let wait = {
                let guard = self.last_idle_at.lock();
                match *guard {
                    None => None, // connection open; wait for disconnect
                    Some(since) => {
                        let elapsed = since.elapsed();
                        if elapsed >= timeout {
                            return;
                        }
                        Some(timeout - elapsed)
                    }
                }
            };
            match wait {
                None => self.tick.notified().await,
                Some(remaining) => {
                    tokio::select! {
                        _ = tokio::time::sleep(remaining) => {}
                        _ = self.tick.notified() => {}
                    }
                }
            }
        }
    }
}

async fn handle_conn(mut stream: UnixStream, daemon: Arc<Daemon>) -> Result<()> {
    let (read_half, mut write_half) = stream.split();
    let mut reader = tokio::io::BufReader::new(read_half);

    loop {
        // Read + deserialize inside an inner scope so the pooled frame
        // buffer is returned to the pool before we spend any time
        // serializing a response (success *or* error path). That keeps
        // a concurrent connection from waiting on our write to recycle
        // its next read buffer.
        let parsed: Result<Request, serde_json::Error> = {
            let mut frame = daemon.frame_pool.acquire();
            match read_frame_into(&mut reader, &mut frame).await? {
                None => return Ok(()),
                Some(()) => serde_json::from_slice(&frame),
            }
        };

        let req = match parsed {
            Ok(r) => r,
            Err(e) => {
                let resp = Response {
                    id: 0,
                    result: None,
                    error: Some(RpcError::new(-32700, e.to_string())),
                };
                let payload = serde_json::to_vec(&resp)?;
                write_frame(&mut write_half, &payload).await?;
                continue;
            }
        };

        let resp = dispatch(&daemon, req).await;
        let payload = serde_json::to_vec(&resp)?;
        write_frame(&mut write_half, &payload).await?;
    }
}

async fn dispatch(daemon: &Daemon, req: Request) -> Response {
    let id = req.id;
    let method = req.method.clone();
    // Time every dispatch (including errors and unknown methods) so a
    // sudden latency spike on a misbehaving client shows up too. The
    // metrics RPCs themselves get instrumented — there's no recursion
    // hazard since `record_latency` doesn't touch the dispatch path.
    let start = std::time::Instant::now();

    let result = match method.as_str() {
        protocol::methods::PING => {
            Ok(serde_json::json!({"ok": true, "version": protocol::PROTOCOL_VERSION}))
        }
        protocol::methods::FS_READ => handlers::fs_read(daemon, req.params),
        protocol::methods::FS_SNAPSHOT => handlers::fs_snapshot(daemon, req.params),
        protocol::methods::FS_CHANGES => handlers::fs_changes(daemon, req.params),
        protocol::methods::FS_SCAN => handlers::fs_scan(daemon, req.params),
        protocol::methods::GIT_STATUS => handlers::git_status(daemon, req.params),
        protocol::methods::SEARCH_GREP => handlers::search_grep(daemon, req.params),
        protocol::methods::CODE_OUTLINE => handlers::code_outline(daemon, req.params),
        protocol::methods::CODE_SYMBOLS => handlers::code_symbols(daemon, req.params),
        protocol::methods::METRICS_GAIN => handlers::metrics_gain(daemon, req.params),
        protocol::methods::METRICS_TOOL_LATENCY => {
            handlers::metrics_tool_latency(daemon, req.params)
        }
        other => Err(RpcError::new(-32601, format!("unknown method: {other}"))),
    };

    let elapsed_us = start.elapsed().as_micros() as u64;
    daemon.metrics.record_latency(&method, elapsed_us);

    match result {
        Ok(value) => Response {
            id,
            result: Some(value),
            error: None,
        },
        Err(err) => Response {
            id,
            result: None,
            error: Some(err),
        },
    }
}

pub fn resolve_within(root: &Path, candidate: &str) -> std::result::Result<PathBuf, RpcError> {
    let path = Path::new(candidate);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let canon = joined
        .canonicalize()
        .map_err(|e| RpcError::new(-32001, format!("canonicalize {candidate}: {e}")))?;
    if !canon.starts_with(root) {
        return Err(RpcError::new(
            -32002,
            format!("path escapes root: {}", canon.display()),
        ));
    }
    Ok(canon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        (tmp, root)
    }

    #[test]
    fn accepts_relative_file_inside_root() {
        let (_tmp, root) = setup();
        fs::write(root.join("a.txt"), b"hello").unwrap();
        let resolved = resolve_within(&root, "a.txt").expect("inside root");
        assert_eq!(resolved, root.join("a.txt"));
    }

    #[test]
    fn accepts_absolute_path_inside_root() {
        let (_tmp, root) = setup();
        fs::write(root.join("a.txt"), b"hello").unwrap();
        let abs = root.join("a.txt");
        let resolved = resolve_within(&root, abs.to_str().unwrap()).expect("inside root");
        assert_eq!(resolved, abs);
    }

    #[test]
    fn rejects_parent_traversal() {
        let (_tmp, root) = setup();
        let sub = root.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(root.parent().unwrap().join("outside.txt"), b"x").ok();

        // Build a path that canonicalizes to a sibling of root.
        let err = resolve_within(&sub, "../../outside.txt").expect_err("should be rejected");
        assert_eq!(err.code, -32002);
    }

    #[test]
    fn rejects_absolute_path_outside_root() {
        let (_tmp, root) = setup();
        // Point at a real path that is guaranteed to exist but is not under root.
        let err = resolve_within(&root, "/tmp").expect_err("should be rejected");
        // Either canonicalize succeeded (wrong-tree) or failed; both are errors.
        assert!(err.code == -32001 || err.code == -32002);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escaping_root() {
        use std::os::unix::fs::symlink;
        let (_tmp, root) = setup();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("secret.txt");
        fs::write(&outside_file, b"nope").unwrap();
        symlink(&outside_file, root.join("link")).unwrap();

        let err =
            resolve_within(&root, "link").expect_err("symlink escaping root must be rejected");
        assert_eq!(err.code, -32002);
    }

    #[test]
    fn nonexistent_path_is_error() {
        let (_tmp, root) = setup();
        let err = resolve_within(&root, "does-not-exist").expect_err("nonexistent");
        assert_eq!(err.code, -32001);
    }

    #[test]
    fn idle_tracker_none_means_no_wait_target() {
        let t = IdleTracker::new(None);
        // last_idle_at is seeded so if we ever call wait_for_timeout it
        // returns immediately for None (short-circuited).
        assert!(t.last_idle_at.lock().is_some());
    }

    #[test]
    fn idle_tracker_connect_clears_idle_timestamp() {
        let t = IdleTracker::new(Some(Duration::from_secs(60)));
        assert!(t.last_idle_at.lock().is_some());
        t.on_connect();
        assert!(t.last_idle_at.lock().is_none());
        assert_eq!(t.active.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn idle_tracker_last_disconnect_sets_timestamp() {
        let t = IdleTracker::new(Some(Duration::from_secs(60)));
        t.on_connect();
        t.on_connect();
        t.on_disconnect();
        assert!(t.last_idle_at.lock().is_none()); // one still active
        t.on_disconnect();
        assert!(t.last_idle_at.lock().is_some());
        assert_eq!(t.active.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn idle_tracker_waits_for_timeout() {
        let t = Arc::new(IdleTracker::new(Some(Duration::from_millis(50))));
        let t2 = t.clone();
        let handle = tokio::spawn(async move { t2.wait_for_timeout().await });
        // The tracker starts already-idle, so the timer should fire
        // without any connection ever arriving.
        tokio::time::timeout(Duration::from_millis(500), handle)
            .await
            .expect("idle timer fired in time")
            .unwrap();
    }

    #[tokio::test]
    async fn idle_tracker_resets_on_connect() {
        let t = Arc::new(IdleTracker::new(Some(Duration::from_millis(80))));
        let t2 = t.clone();
        let wait = tokio::spawn(async move { t2.wait_for_timeout().await });
        // Connect before the initial idle timer fires; the timer must
        // now wait on notification of a later disconnect rather than
        // firing when the original 80ms elapses.
        tokio::time::sleep(Duration::from_millis(20)).await;
        t.on_connect();
        // Sleep past the original deadline to prove the timer didn't
        // fire on the pre-connect timestamp.
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(
            !wait.is_finished(),
            "idle timer fired while a connection was open"
        );
        // Disconnect and let it fire within the timeout.
        t.on_disconnect();
        tokio::time::timeout(Duration::from_millis(500), wait)
            .await
            .expect("idle timer fired after disconnect")
            .unwrap();
    }
}
