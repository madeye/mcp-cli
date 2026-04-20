//! Multi-bridge contention test for M6.
//!
//! Spawns N bridge processes against one daemon (per-cwd socket
//! shared because they all chdir to the same project root) and
//! drives a small concurrent workload through each. Verifies:
//!
//!   1. all bridges connect successfully (the daemon's per-connection
//!      task model handles N parallel UDS clients without deadlock),
//!   2. every call returns a well-formed result (no per-bridge
//!      starvation, no cross-talk),
//!   3. the daemon survives all bridges disconnecting and idle-exits
//!      cleanly afterwards.
//!
//! This is a behavioural test, not a perf benchmark — we want to
//! catch lifecycle bugs that single-bridge tests miss, not measure
//! throughput.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const N_BRIDGES: usize = 4;
const CALLS_PER_BRIDGE: usize = 8;

#[test]
fn n_bridges_share_one_daemon_under_contention() {
    let daemon = daemon_binary_path();
    let bridge = PathBuf::from(env!("CARGO_BIN_EXE_mcp-cli-bridge"));

    let project = tempfile::tempdir().expect("project tempdir");
    let runtime = tempfile::tempdir().expect("runtime tempdir");

    // Seed a few files so each bridge has independent fs_read targets;
    // useful for catching cross-talk where bridge A's response shows
    // up on bridge B's stdout.
    for i in 0..N_BRIDGES {
        std::fs::write(
            project.path().join(format!("file_{i}.txt")),
            format!("payload from bridge {i}\n"),
        )
        .expect("seed file");
    }

    let socket = expected_socket_path(project.path(), runtime.path());

    // Spawn the bridges in a tight loop so they race on the daemon's
    // auto-spawn — exactly one wins the bind(2), the rest connect to
    // the same socket. This is the lifecycle path we want to exercise.
    let mut children: Vec<Child> = (0..N_BRIDGES)
        .map(|_| {
            Command::new(&bridge)
                .arg("--root")
                .arg(project.path())
                .arg("--daemon-bin")
                .arg(&daemon)
                .arg("--daemon-arg")
                .arg("--idle-timeout")
                .arg("--daemon-arg")
                .arg("3s")
                .arg("--daemon-arg")
                .arg("--no-prewarm")
                .env("XDG_RUNTIME_DIR", runtime.path())
                .env("RUST_LOG", "warn")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("spawn bridge")
        })
        .collect();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Drive each bridge from its own thread so we get genuine
        // parallel pressure on the daemon, not serialized round-robin.
        let (tx, rx) = mpsc::channel();
        let mut handles = Vec::with_capacity(N_BRIDGES);
        for (i, child) in children.iter_mut().enumerate() {
            let stdin = child.stdin.take().expect("bridge stdin");
            let stdout = child.stdout.take().expect("bridge stdout");
            let tx = tx.clone();
            handles.push(thread::spawn(move || {
                let outcome = drive_bridge(i, stdin, stdout);
                let _ = tx.send((i, outcome));
            }));
        }
        drop(tx);

        for h in handles {
            h.join().expect("worker thread panicked");
        }
        let mut outcomes: Vec<(usize, BridgeOutcome)> = rx.iter().collect();
        outcomes.sort_by_key(|(i, _)| *i);
        assert_eq!(outcomes.len(), N_BRIDGES, "lost a worker outcome");
        for (i, o) in &outcomes {
            assert_eq!(o.id, *i, "outcome id mismatch (cross-talk?)");
            assert_eq!(
                o.successful_calls, CALLS_PER_BRIDGE,
                "bridge {i} only got {} of {CALLS_PER_BRIDGE} calls through",
                o.successful_calls
            );
        }

        assert!(
            socket.exists(),
            "socket vanished mid-test at {}",
            socket.display()
        );
    }));

    for child in children.iter_mut() {
        drop(child.stdin.take());
    }
    for child in children.iter_mut() {
        let _ = wait_with_timeout(child, Duration::from_secs(5));
    }

    // After all bridges exit, the daemon should idle-exit (we passed
    // --idle-timeout 3s) and unlink the socket. Wait up to 12s.
    let deadline = Instant::now() + Duration::from_secs(12);
    while socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(150));
    }
    let still_there = socket.exists();

    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
    assert!(
        !still_there,
        "daemon socket {} did not disappear within idle-timeout budget",
        socket.display()
    );
}

struct BridgeOutcome {
    id: usize,
    successful_calls: usize,
}

fn drive_bridge<W: Write, R: std::io::Read + Send + 'static>(
    id: usize,
    mut stdin: W,
    stdout: R,
) -> BridgeOutcome {
    let mut reader = BufReader::new(stdout);

    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
    );
    let init = read_response(&mut reader, 1);
    assert!(init.get("result").is_some(), "[{id}] initialize: {init}");

    let mut successful = 0usize;
    let path = format!("file_{id}.txt");
    let expected = format!("payload from bridge {id}\n");

    for n in 0..CALLS_PER_BRIDGE {
        let req_id = (n as u64) + 100;
        send(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "method": "tools/call",
                "params": {"name": "fs_read", "arguments": {"path": path}},
            }),
        );
        let resp = read_response(&mut reader, req_id);
        let text = resp
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("[{id}/{n}] missing content text: {resp}"));
        let inner: Value =
            serde_json::from_str(text).unwrap_or_else(|e| panic!("[{id}/{n}] inner: {e}: {text}"));
        let content = inner.get("content").and_then(Value::as_str).unwrap_or("");
        assert_eq!(
            content, expected,
            "[{id}/{n}] cross-talk? expected {expected:?} got {content:?}"
        );
        successful += 1;
    }

    BridgeOutcome {
        id,
        successful_calls: successful,
    }
}

fn send<W: Write>(w: &mut W, msg: &Value) {
    let line = serde_json::to_string(msg).expect("serialize");
    w.write_all(line.as_bytes()).expect("write request");
    w.write_all(b"\n").expect("write newline");
    w.flush().expect("flush stdin");
}

fn read_response<R: BufRead>(r: &mut R, expected_id: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if Instant::now() > deadline {
            panic!("timed out waiting for response id={expected_id}");
        }
        let mut buf = String::new();
        let n = r.read_line(&mut buf).expect("read line");
        assert!(
            n > 0,
            "bridge stdout closed while waiting for id={expected_id}"
        );
        let v: Value = serde_json::from_str(buf.trim()).expect("parse response");
        if v.get("id") == Some(&json!(expected_id)) {
            return v;
        }
    }
}

fn daemon_binary_path() -> PathBuf {
    let bridge = PathBuf::from(env!("CARGO_BIN_EXE_mcp-cli-bridge"));
    let dir = bridge.parent().expect("bridge parent");
    let name = if cfg!(windows) {
        "mcp-cli-daemon.exe"
    } else {
        "mcp-cli-daemon"
    };
    let path = dir.join(name);
    if !path.exists() {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = Command::new(&cargo)
            .args(["build", "-p", "daemon"])
            .status()
            .expect("run cargo build -p daemon");
        assert!(status.success(), "cargo build -p daemon failed: {status}");
    }
    assert!(path.exists(), "daemon binary missing at {}", path.display());
    path
}

fn expected_socket_path(project_root: &Path, runtime_dir: &Path) -> PathBuf {
    let canonical = project_root
        .canonicalize()
        .expect("canonicalize project root");
    protocol::paths::socket_path_for_in(&canonical, Some(runtime_dir))
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
    let _ = child.kill();
    child.wait().ok()
}
