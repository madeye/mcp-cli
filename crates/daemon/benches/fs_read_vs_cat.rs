use std::fs;
use std::hint::black_box;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use protocol::{methods, FsReadParams, FsReadResult, Request, Response};
use tempfile::TempDir;

const FILE_SIZES: &[usize] = &[4 * 1024, 64 * 1024, 256 * 1024];
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const WARMUP_ITERS: usize = 10;
const BENCH_ITERS: usize = 100;

struct BenchFixture {
    _tmp: TempDir,
    daemon: Child,
    socket: PathBuf,
    files: Vec<FileFixture>,
}

struct FileFixture {
    path: PathBuf,
    rel_path: String,
    size: usize,
}

struct Timing {
    total: Duration,
    iterations: usize,
}

impl Timing {
    fn avg_ns(&self) -> u128 {
        self.total.as_nanos() / self.iterations as u128
    }
}

impl BenchFixture {
    fn setup() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket = tmp.path().join("bench.sock");
        let root = tmp.path().join("root");
        fs::create_dir(&root).expect("create root");

        let files = FILE_SIZES
            .iter()
            .map(|&size| make_fixture_file(&root, size))
            .collect::<Vec<_>>();

        let daemon = spawn_daemon(&root, &socket);
        wait_for_socket(&socket, CONNECT_TIMEOUT);

        Self {
            _tmp: tmp,
            daemon,
            socket,
            files,
        }
    }
}

impl Drop for BenchFixture {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

struct RpcClient {
    stream: UnixStream,
    next_id: u64,
}

impl RpcClient {
    fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).expect("connect to daemon");
        Self { stream, next_id: 1 }
    }

    fn fs_read(&mut self, rel_path: &str) -> FsReadResult {
        let req = Request {
            id: self.next_id,
            method: methods::FS_READ.to_string(),
            params: serde_json::to_value(FsReadParams {
                path: rel_path.to_string(),
                offset: 0,
                length: None,
            })
            .expect("serialize fs.read params"),
        };
        self.next_id += 1;

        let payload = serde_json::to_vec(&req).expect("serialize request");
        let len = u32::try_from(payload.len()).expect("request fits into u32");
        self.stream
            .write_all(&len.to_be_bytes())
            .expect("write request length");
        self.stream.write_all(&payload).expect("write request");
        self.stream.flush().expect("flush request");

        let mut len_buf = [0u8; 4];
        self.stream
            .read_exact(&mut len_buf)
            .expect("read response length");
        let resp_len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; resp_len];
        self.stream
            .read_exact(&mut buf)
            .expect("read response payload");

        let resp: Response = serde_json::from_slice(&buf).expect("decode response");
        if let Some(err) = resp.error {
            panic!("daemon error {}: {}", err.code, err.message);
        }
        serde_json::from_value(resp.result.expect("response result"))
            .expect("decode fs.read result")
    }
}

fn main() {
    let fixture = BenchFixture::setup();

    println!(
        "fs.read vs cat benchmark ({WARMUP_ITERS} warm-up iters, {BENCH_ITERS} measured iters)"
    );
    println!(
        "{:<10} {:>14} {:>14} {:>10}",
        "size", "fs.read avg", "cat avg", "cat/fs"
    );

    for file in &fixture.files {
        let expected_len = fs::metadata(&file.path).expect("stat benchmark file").len() as usize;

        let mut client = RpcClient::connect(&fixture.socket);
        let warm = client.fs_read(&file.rel_path);
        assert_eq!(warm.bytes_read as usize, expected_len);
        assert_eq!(warm.content.len(), expected_len);

        warmup_fs_read(&mut client, file, WARMUP_ITERS);
        warmup_cat(file, WARMUP_ITERS);

        let fs_read = measure_fs_read(&mut client, file, BENCH_ITERS);
        let cat = measure_cat(file, BENCH_ITERS);
        let ratio = cat.avg_ns() as f64 / fs_read.avg_ns() as f64;

        println!(
            "{:<10} {:>14} {:>14} {:>10.2}x",
            human_size(file.size),
            fmt_ns(fs_read.avg_ns()),
            fmt_ns(cat.avg_ns()),
            ratio,
        );
    }
}

fn measure_fs_read(client: &mut RpcClient, file: &FileFixture, iterations: usize) -> Timing {
    let start = Instant::now();
    for _ in 0..iterations {
        let result = client.fs_read(&file.rel_path);
        black_box(result.bytes_read);
        black_box(result.content.len());
    }
    Timing {
        total: start.elapsed(),
        iterations,
    }
}

fn measure_cat(file: &FileFixture, iterations: usize) -> Timing {
    let start = Instant::now();
    for _ in 0..iterations {
        let output = Command::new("cat")
            .arg(&file.path)
            .output()
            .expect("spawn cat");
        assert!(output.status.success(), "cat exited unsuccessfully");
        black_box(output.stdout.len());
    }
    Timing {
        total: start.elapsed(),
        iterations,
    }
}

fn warmup_fs_read(client: &mut RpcClient, file: &FileFixture, iterations: usize) {
    for _ in 0..iterations {
        let result = client.fs_read(&file.rel_path);
        black_box(result.content.len());
    }
}

fn warmup_cat(file: &FileFixture, iterations: usize) {
    for _ in 0..iterations {
        let output = Command::new("cat")
            .arg(&file.path)
            .output()
            .expect("spawn cat");
        assert!(output.status.success(), "cat exited unsuccessfully");
        black_box(output.stdout.len());
    }
}

fn make_fixture_file(root: &Path, size: usize) -> FileFixture {
    let name = format!("fixture-{}.txt", size);
    let path = root.join(&name);
    let content = make_ascii_content(size);
    fs::write(&path, content).expect("write fixture file");
    FileFixture {
        path,
        rel_path: name,
        size,
    }
}

fn make_ascii_content(size: usize) -> Vec<u8> {
    let pattern = b"mcp-cli benchmark fixture line 0123456789 abcdefghijklmnopqrstuvwxyz\n";
    let mut out = Vec::with_capacity(size);
    while out.len() < size {
        let remaining = size - out.len();
        let take = remaining.min(pattern.len());
        out.extend_from_slice(&pattern[..take]);
    }
    out
}

fn spawn_daemon(root: &Path, socket: &Path) -> Child {
    Command::new(daemon_bin())
        .arg("--root")
        .arg(root)
        .arg("--socket")
        .arg(socket)
        .arg("--idle-timeout")
        .arg("0")
        .arg("--no-prewarm")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn benchmark daemon")
}

fn daemon_bin() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_mcp-cli-daemon") {
        return PathBuf::from(path);
    }

    let current = std::env::current_exe().expect("current_exe");
    let target_release = current
        .parent()
        .and_then(Path::parent)
        .expect("target/release");
    let candidate = target_release.join("mcp-cli-daemon");
    assert!(
        candidate.exists(),
        "mcp-cli-daemon not found at {}",
        candidate.display()
    );
    candidate
}

fn wait_for_socket(socket: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        match UnixStream::connect(socket) {
            Ok(_) => return,
            Err(err) if Instant::now() < deadline => {
                let _ = err;
                sleep(Duration::from_millis(25));
            }
            Err(err) => panic!(
                "daemon socket {} not ready within {:?}: {}",
                socket.display(),
                timeout,
                err
            ),
        }
    }
}

fn human_size(size: usize) -> String {
    match size / 1024 {
        kib => format!("{kib} KiB"),
    }
}

fn fmt_ns(ns: u128) -> String {
    if ns >= 1_000_000 {
        format!("{:.2} ms", ns as f64 / 1_000_000.0)
    } else if ns >= 1_000 {
        format!("{:.2} us", ns as f64 / 1_000.0)
    } else {
        format!("{ns} ns")
    }
}
