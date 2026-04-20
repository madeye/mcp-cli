//! Reconnect-on-daemon-dead regression test (M5).
//!
//! Spawns the bridge so it auto-spawns a daemon, drives one tools/call
//! to confirm the connection works, kills the daemon out from under it
//! (`pkill -f <unique-socket-path>`), then drives a second tools/call.
//! Pre-M5 the bridge would surface broken-pipe / ConnectionRefused to
//! the MCP client; post-M5 it must transparently reconnect via the
//! same auto-spawn path used at startup.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

#[test]
fn bridge_reconnects_after_daemon_killed() {
    let daemon = daemon_binary_path();
    let bridge = PathBuf::from(env!("CARGO_BIN_EXE_mcp-cli-bridge"));

    let project = tempfile::tempdir().expect("project tempdir");
    let runtime = tempfile::tempdir().expect("runtime tempdir");
    let hello = project.path().join("hello.txt");
    std::fs::write(&hello, b"hello world\n").expect("seed file");

    // Long idle timeout so the only way the daemon dies during this test
    // is the explicit pkill we issue below.
    let mut child = Command::new(&bridge)
        .arg("--root")
        .arg(project.path())
        .arg("--daemon-bin")
        .arg(&daemon)
        .arg("--daemon-arg")
        .arg("--idle-timeout")
        .arg("--daemon-arg")
        .arg("0")
        .arg("--daemon-arg")
        .arg("--no-prewarm")
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("RUST_LOG", "warn")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn bridge");

    let socket = expected_socket_path(project.path(), runtime.path());

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut stdin = child.stdin.take().expect("bridge stdin");
        let stdout = child.stdout.take().expect("bridge stdout");
        let mut reader = BufReader::new(stdout);

        send(
            &mut stdin,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        );
        let init = read_response(&mut reader, 1);
        assert!(init.get("result").is_some(), "initialize: {init}");

        // First fs_read — daemon was just spawned by the bridge.
        let first = call_fs_read(&mut stdin, &mut reader, 2);
        assert_eq!(first, "hello world\n", "first fs_read content");
        assert!(
            socket.exists(),
            "socket missing after first call at {}",
            socket.display()
        );

        // Kill the daemon out from under the bridge. We pkill by socket
        // path because that's a unique substring of the daemon's argv
        // (no risk of nuking unrelated daemons running in parallel CI).
        kill_daemon_for_socket(&socket);
        // Wait until a new connect attempt would fail — the kernel takes
        // a beat to tear down the listener after the process dies.
        wait_until_socket_dead(&socket, Duration::from_secs(5));

        // Second fs_read — the bridge's existing UnixStream is stale.
        // The reconnect path must spin up a fresh daemon and serve.
        let second = call_fs_read(&mut stdin, &mut reader, 3);
        assert_eq!(
            second, "hello world\n",
            "second fs_read after reconnect should succeed"
        );
        assert!(
            socket.exists(),
            "socket missing after reconnect at {}",
            socket.display()
        );
    }));

    drop(child.stdin.take());
    let _ = wait_with_timeout(&mut child, Duration::from_secs(5));
    // Best-effort cleanup of the post-reconnect daemon so subsequent test
    // runs in the same XDG_RUNTIME_DIR don't see a lingering socket.
    kill_daemon_for_socket(&socket);

    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn kill_daemon_for_socket(socket: &Path) {
    let _ = Command::new("pkill")
        .arg("-f")
        .arg(socket.to_string_lossy().as_ref())
        .status();
}

/// Block until a fresh `connect(socket)` returns ECONNREFUSED or NotFound,
/// proving the previous listener is gone. Polling avoids guessing how
/// long the OS takes to reap a killed process's UDS state.
fn wait_until_socket_dead(socket: &Path, budget: Duration) {
    use std::os::unix::net::UnixStream;
    let deadline = Instant::now() + budget;
    loop {
        match UnixStream::connect(socket) {
            Ok(_) => {
                if Instant::now() >= deadline {
                    panic!(
                        "daemon at {} still accepting connections after {:?}",
                        socket.display(),
                        budget
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return,
        }
    }
}

fn call_fs_read<R: BufRead, W: Write>(stdin: &mut W, reader: &mut R, id: u64) -> String {
    send(
        stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": "fs_read", "arguments": {"path": "hello.txt"}},
        }),
    );
    let resp = read_response(reader, id);
    let text = resp
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("tools/call missing content text at id={id}: {resp}"));
    let inner: Value =
        serde_json::from_str(text).unwrap_or_else(|e| panic!("inner JSON parse: {e}: {text}"));
    inner
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing content field at id={id}: {inner}"))
        .to_string()
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
    let dir = bridge.parent().expect("bridge binary parent");
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
