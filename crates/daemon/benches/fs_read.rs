/// Criterion benchmark: `fs.read` via daemon UDS vs. `cat` fork+exec.
///
/// Strategy: build the daemon binary first (`cargo build -p daemon`),
/// then spawn it into a tempdir.  Each Criterion iteration either:
///   - sends one `fs.read` RPC over the UDS and reads the response, or
///   - spawns `cat <path>` and drains stdout.
///
/// Wall-clock numbers expose the per-call kernel overhead difference
/// (daemon: 1 syscall pair vs. cat: fork+exec+read+wait).
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};

// ---------------------------------------------------------------------------
// Framing helpers (synchronous — bench doesn't need async)
// ---------------------------------------------------------------------------

fn write_frame(stream: &mut UnixStream, payload: &[u8]) -> std::io::Result<()> {
    let len = u32::try_from(payload.len()).expect("frame too large");
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(payload)?;
    stream.flush()
}

fn read_frame(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Daemon lifecycle
// ---------------------------------------------------------------------------

struct DaemonGuard {
    child: Child,
    socket: PathBuf,
    _tmpdir: tempfile::TempDir,
}

impl DaemonGuard {
    fn spawn(root: &Path) -> Self {
        let tmpdir = tempfile::TempDir::new().expect("tmpdir");
        let socket = tmpdir.path().join("bench.sock");

        // Locate the daemon binary built by `cargo bench` (always release-like
        // profile in the same target dir).
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let target_dir = manifest_dir
            .ancestors()
            .nth(2) // workspace root
            .expect("workspace root")
            .join("target");

        // `cargo bench` uses the `bench` profile (optimized). The binary lives
        // in target/release when built with --release, or target/debug for
        // dev. Try release first, fall back to debug.
        let daemon_bin = ["release", "debug"]
            .iter()
            .map(|profile| target_dir.join(profile).join("mcp-cli-daemon"))
            .find(|p| p.exists())
            .expect("mcp-cli-daemon binary not found; run `cargo build -p daemon --release` first");

        let child = Command::new(&daemon_bin)
            .arg("--root")
            .arg(root)
            .arg("--socket")
            .arg(&socket)
            .arg("--no-prewarm")
            .arg("--idle-timeout")
            .arg("0") // don't auto-exit during bench
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn daemon");

        // Wait for the socket to appear (up to 5 s).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if socket.exists() {
                // Extra small sleep so the daemon finishes bind+listen.
                std::thread::sleep(Duration::from_millis(20));
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("daemon socket did not appear within 5 s");
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        Self {
            child,
            socket,
            _tmpdir: tmpdir,
        }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// RPC helpers
// ---------------------------------------------------------------------------

fn connect(socket: &Path) -> UnixStream {
    UnixStream::connect(socket).expect("connect to daemon")
}

/// Send an `fs.read` RPC and return the raw response bytes. We don't
/// parse the JSON — we just want wall-clock round-trip time.
fn daemon_fs_read(stream: &mut UnixStream, path: &str) -> Vec<u8> {
    let req = format!(
        r#"{{"id":1,"method":"fs.read","params":{{"path":{path_json}}}}}"#,
        path_json = serde_json::to_string(path).unwrap()
    );
    write_frame(stream, req.as_bytes()).expect("write_frame");
    read_frame(stream).expect("read_frame")
}

/// Spawn `cat <path>` and drain its stdout. Returns total bytes read.
fn cat_read(path: &Path) -> usize {
    let output = Command::new("cat").arg(path).output().expect("cat");
    output.stdout.len()
}

// ---------------------------------------------------------------------------
// Benchmark groups
// ---------------------------------------------------------------------------

fn bench_fs_read_daemon(c: &mut Criterion) {
    // Use handlers.rs as the target file — substantial but not huge (~22 KiB).
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/handlers.rs");
    assert!(target.exists(), "handlers.rs must exist");
    let target_str = target.to_str().unwrap().to_owned();

    let daemon = DaemonGuard::spawn(target.parent().unwrap());

    // Warm-up: open a connection, fire a few reads so the OS page cache and
    // daemon-side mmap are hot before Criterion starts timing.
    let mut stream = connect(&daemon.socket);
    for _ in 0..5 {
        daemon_fs_read(&mut stream, &target_str);
    }

    // Bench A: fs.read via daemon (persistent connection, no spawn per call).
    let mut group = c.benchmark_group("fs_read");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(10));

    // Reuse one connection for the whole bench group — this is the steady-
    // state profile (agent keeps the UDS open across many calls).
    group.bench_function("daemon_uds", |b| {
        b.iter(|| {
            let bytes = daemon_fs_read(&mut stream, &target_str);
            criterion::black_box(bytes);
        });
    });

    // Bench B: cat fork+exec per call — the baseline an agent would use
    // without the daemon.
    group.bench_function("cat_fork_exec", |b| {
        b.iter(|| {
            let n = cat_read(&target);
            criterion::black_box(n);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_fs_read_daemon);
criterion_main!(benches);
