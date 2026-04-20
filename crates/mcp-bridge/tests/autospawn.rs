//! End-to-end smoke test for the M3 drop-in install track.
//!
//! Starts the bridge binary in a fresh tempdir cwd, drives it with an
//! MCP `initialize` + `tools/call fs_read`, and verifies that:
//!   1. the bridge auto-spawned a daemon pointed at the right root,
//!   2. the socket landed at the protocol-derived per-cwd path, and
//!   3. `fs_read` returns the file content written pre-spawn.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

#[test]
fn bridge_autospawns_daemon_and_serves_fs_read() {
    let daemon = daemon_binary_path();
    let bridge = PathBuf::from(env!("CARGO_BIN_EXE_mcp-cli-bridge"));

    // Dedicated tempdirs for the project and the socket parent — the
    // socket lives under XDG_RUNTIME_DIR so we point that at a tempdir
    // to avoid colliding with real daemons running in the user's
    // session.
    let project = tempfile::tempdir().expect("project tempdir");
    let runtime = tempfile::tempdir().expect("runtime tempdir");
    let hello = project.path().join("hello.txt");
    std::fs::write(&hello, b"hello world\n").expect("seed file");

    let mut child = Command::new(&bridge)
        .arg("--root")
        .arg(project.path())
        .arg("--daemon-bin")
        .arg(&daemon)
        .arg("--daemon-arg")
        .arg("--idle-timeout")
        .arg("--daemon-arg")
        .arg("2s")
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
        run_exchange(&mut child, project.path());
        // The bridge has just received a real response, so the daemon
        // was definitely spawned — the socket must exist at the path
        // we derived. If this fires, the bridge and protocol::paths
        // disagree about where to put the socket.
        assert!(
            socket.exists(),
            "expected daemon socket at {} after successful fs_read",
            socket.display()
        );
    }));

    // Always tear down: close stdin so the bridge exits; wait up to 5s.
    drop(child.stdin.take());
    let _ = wait_with_timeout(&mut child, Duration::from_secs(5));

    // Wait for the daemon to idle-exit (we passed --idle-timeout 2s) so
    // the socket file is cleaned up before the next run, then assert
    // it actually went away — a daemon that never exits would otherwise
    // leave this test silently passing.
    let deadline = Instant::now() + Duration::from_secs(8);
    while socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
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

fn run_exchange(child: &mut Child, project_root: &Path) {
    let mut stdin = child.stdin.take().expect("bridge stdin");
    let stdout = child.stdout.take().expect("bridge stdout");
    let mut reader = BufReader::new(stdout);

    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
    );
    let init = read_response(&mut reader, 1);
    assert!(
        init.get("result").is_some(),
        "initialize returned no result: {init}"
    );

    send(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    );
    let list = read_response(&mut reader, 2);
    let tools = list
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .expect("tools/list returned a tools array");
    assert!(
        tools
            .iter()
            .any(|t| t.get("name") == Some(&json!("fs_read"))),
        "tools/list missing fs_read: {list}"
    );

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "fs_read", "arguments": {"path": "hello.txt"}},
        }),
    );
    let call = read_response(&mut reader, 3);
    let text = call
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("tools/call missing content text: {call}"));
    let inner: Value =
        serde_json::from_str(text).unwrap_or_else(|e| panic!("inner JSON parse: {e}: {text}"));
    assert_eq!(
        inner.get("content").and_then(Value::as_str),
        Some("hello world\n"),
        "unexpected fs_read content: {inner}"
    );
    let _ = project_root; // path arg is returned verbatim, not re-canonicalized
    let returned_path = inner.get("path").and_then(Value::as_str).unwrap_or("");
    assert!(
        returned_path.ends_with("hello.txt"),
        "unexpected fs_read path: {inner}"
    );

    // Batch read: exercise fs_read_batch end-to-end, including per-item
    // error handling when one of the paths doesn't exist. The second
    // request points at a sibling file we seed here, the third is
    // a deliberately-missing path so we can verify error isolation.
    std::fs::write(project_root.join("other.txt"), b"second file\n").expect("seed other");
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "fs_read_batch",
                "arguments": {
                    "requests": [
                        {"path": "hello.txt"},
                        {"path": "other.txt"},
                        {"path": "does-not-exist.txt"},
                    ]
                },
            },
        }),
    );
    let batch = read_response(&mut reader, 4);
    let btext = batch
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("fs_read_batch missing content text: {batch}"));
    let binner: Value =
        serde_json::from_str(btext).unwrap_or_else(|e| panic!("batch JSON parse: {e}: {btext}"));
    let responses = binner
        .get("responses")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("fs_read_batch missing responses[]: {binner}"));
    assert_eq!(responses.len(), 3, "expected 3 response entries: {binner}");

    let first_content = responses[0]
        .pointer("/result/content")
        .and_then(Value::as_str);
    assert_eq!(first_content, Some("hello world\n"), "entry 0: {binner}");

    let second_content = responses[1]
        .pointer("/result/content")
        .and_then(Value::as_str);
    assert_eq!(second_content, Some("second file\n"), "entry 1: {binner}");

    // Third request: missing file → error entry, no crash, other
    // responses still delivered.
    assert!(
        responses[2].get("error").is_some(),
        "entry 2 should carry an error object: {binner}"
    );
    assert!(
        responses[2].get("result").is_none() || responses[2].get("result") == Some(&Value::Null),
        "entry 2 should not carry a result: {binner}"
    );

    // code_symbols_batch: hits the daemon's tree-sitter backend over
    // two real source files, plus one path that doesn't exist to
    // exercise per-item error isolation.
    std::fs::write(project_root.join("a.rs"), b"fn alpha() {}\nstruct Beta;\n").expect("seed a.rs");
    std::fs::write(project_root.join("b.rs"), b"fn gamma() {}\n").expect("seed b.rs");
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "code_symbols_batch",
                "arguments": {
                    "requests": [
                        {"path": "a.rs"},
                        {"path": "b.rs"},
                        {"path": "missing.rs"},
                    ]
                },
            },
        }),
    );
    let symbatch = read_response(&mut reader, 6);
    let symtext = symbatch
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("code_symbols_batch missing content text: {symbatch}"));
    let syminner: Value = serde_json::from_str(symtext).expect("parse code_symbols_batch result");
    let symresps = syminner
        .get("responses")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("code_symbols_batch missing responses[]: {syminner}"));
    assert_eq!(symresps.len(), 3, "expected 3 entries: {syminner}");

    let names_a: Vec<&str> = symresps[0]
        .pointer("/result/names")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    assert!(
        names_a.contains(&"alpha") && names_a.contains(&"Beta"),
        "a.rs should expose alpha + Beta: {syminner}"
    );

    let names_b: Vec<&str> = symresps[1]
        .pointer("/result/names")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    assert!(
        names_b.contains(&"gamma"),
        "b.rs should expose gamma: {syminner}"
    );

    assert!(
        symresps[2].get("error").is_some(),
        "missing.rs entry must carry error: {syminner}"
    );

    // search_grep with context=2 — the match line should come back
    // with up to 2 lines before and 2 lines after attached to its
    // `context` array. Seed a small multi-line file so the expected
    // surround is deterministic.
    std::fs::write(
        project_root.join("ctx.txt"),
        b"alpha\nbeta\nNEEDLE here\ngamma\ndelta\n",
    )
    .expect("seed ctx");
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "search_grep",
                "arguments": {
                    "pattern": "NEEDLE",
                    "glob": "ctx.txt",
                    "context": 2,
                },
            },
        }),
    );
    let search = read_response(&mut reader, 5);
    let stext = search
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("search_grep missing content text: {search}"));
    let sinner: Value = serde_json::from_str(stext).expect("parse search result");
    let hits = sinner
        .get("hits")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("search_grep missing hits[]: {sinner}"));
    assert_eq!(hits.len(), 1, "expected one match: {sinner}");
    let hit = &hits[0];
    assert_eq!(hit.pointer("/line_number"), Some(&json!(3)));
    assert_eq!(hit.pointer("/line"), Some(&json!("NEEDLE here")));
    let context = hit
        .get("context")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("hit missing context[]: {hit}"));
    // Expect the two lines before (alpha, beta) and two after
    // (gamma, delta) in file order.
    let got: Vec<(u64, &str)> = context
        .iter()
        .map(|c| {
            (
                c.get("line_number").and_then(Value::as_u64).unwrap_or(0),
                c.get("line").and_then(Value::as_str).unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![(1, "alpha"), (2, "beta"), (4, "gamma"), (5, "delta"),],
        "unexpected context window: {hit}"
    );

    // Give the ring buffer a beat to register our connection as active
    // so the idle timer doesn't win the race against test teardown.
    std::thread::sleep(Duration::from_millis(50));
}

fn send<W: Write>(w: &mut W, msg: &Value) {
    let line = serde_json::to_string(msg).expect("serialize");
    w.write_all(line.as_bytes()).expect("write request");
    w.write_all(b"\n").expect("write newline");
    w.flush().expect("flush stdin");
}

fn read_response<R: BufRead>(r: &mut R, expected_id: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
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
    let dir = bridge
        .parent()
        .expect("bridge binary should live in a directory");
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
        assert!(
            status.success(),
            "cargo build -p daemon failed with {status}"
        );
    }
    assert!(
        path.exists(),
        "daemon binary missing at {} after build",
        path.display()
    );
    path
}

fn expected_socket_path(project_root: &Path, runtime_dir: &Path) -> PathBuf {
    let canonical = project_root
        .canonicalize()
        .expect("canonicalize project root");
    // Derive using the runtime-dir override helper so we don't mutate
    // the test process's env — tests run in parallel and XDG_RUNTIME_DIR
    // races would make this flaky.
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
