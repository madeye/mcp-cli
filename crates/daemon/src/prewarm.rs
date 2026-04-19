//! Startup pre-warm: walk the tree in the background so the OS page cache is
//! hot before the first `fs.read` or `search.grep` lands.
//!
//! We deliberately keep this simple: respect `.gitignore`, skip `.git/`, cap
//! the per-file bytes we touch (we want to prime readahead, not actually fault
//! in a 2 GiB blob), and never block daemon startup. Failures are logged and
//! swallowed — a half-warmed cache is still better than none.

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::time::Instant;

use ignore::WalkBuilder;

/// Cap per-file bytes we pull in. One or two pages is enough to trigger the
/// kernel's readahead heuristics; beyond that we'd just be wasting memory on
/// files the agent may never touch.
const MAX_BYTES_PER_FILE: usize = 64 * 1024;

pub fn spawn(root: PathBuf) {
    std::thread::Builder::new()
        .name("prewarm".into())
        .spawn(move || run(root))
        .map(|_| ())
        .unwrap_or_else(|e| tracing::warn!(error = %e, "prewarm thread failed to spawn"));
}

fn run(root: PathBuf) {
    let start = Instant::now();
    let mut files = 0u64;
    let mut bytes = 0u64;

    let mut buf = [0u8; 8 * 1024];
    for entry in WalkBuilder::new(&root)
        .standard_filters(true)
        .hidden(false)
        .build()
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        let Ok(mut file) = File::open(path) else {
            continue;
        };
        let mut remaining = MAX_BYTES_PER_FILE;
        while remaining > 0 {
            let to_read = remaining.min(buf.len());
            match file.read(&mut buf[..to_read]) {
                Ok(0) => break,
                Ok(n) => {
                    bytes += n as u64;
                    remaining -= n;
                }
                Err(_) => break,
            }
        }
        files += 1;
    }

    tracing::info!(
        files,
        bytes,
        elapsed_ms = start.elapsed().as_millis() as u64,
        "prewarm done",
    );
}
