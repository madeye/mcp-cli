//! Per-file tree-sitter parse cache, invalidated by mtime + size.
//!
//! Unlike `search.grep`, per-file parses are independent of unrelated
//! tree edits: a change to `a.rs` does not invalidate a cached `b.rs`
//! tree. We therefore don't hang this off the `ChangeLog` version
//! (which would flush everything on every edit). Instead each entry
//! carries the `(mtime_ns, size)` it was parsed at; a stat on access
//! decides hit vs re-parse. `fs.changes` / watcher events can still
//! invalidate proactively by calling `evict`, but correctness doesn't
//! rely on the watcher running.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use parking_lot::Mutex;
use tree_sitter::Tree;

use crate::languages::Language;

/// What the watcher / snapshot code sees for a cached file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    mtime_ns: u128,
    size: u64,
}

impl FileStamp {
    fn of(path: &Path) -> Option<Self> {
        let md = std::fs::metadata(path).ok()?;
        let mtime = md.modified().ok()?;
        let mtime_ns = mtime.duration_since(UNIX_EPOCH).ok()?.as_nanos();
        Some(Self {
            mtime_ns,
            size: md.len(),
        })
    }
}

struct Entry {
    path: PathBuf,
    stamp: FileStamp,
    language: Language,
    /// Parsed source bytes, owned so callers can run queries against them
    /// without re-reading from disk.
    source: Arc<Vec<u8>>,
    tree: Arc<Tree>,
}

pub struct ParseCache {
    inner: Mutex<Inner>,
    capacity: usize,
}

struct Inner {
    /// Oldest at front, most-recently-used at back.
    entries: VecDeque<Entry>,
}

/// What `get_or_parse` returns: the parsed tree plus the source bytes
/// it was parsed from. Queries must be run against these bytes, not
/// against a fresh read — the two can diverge if the file is rewritten
/// between the parse and the query.
#[derive(Clone)]
pub struct ParsedFile {
    pub language: Language,
    pub source: Arc<Vec<u8>>,
    pub tree: Arc<Tree>,
}

impl ParseCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: VecDeque::new(),
            }),
            capacity,
        }
    }

    /// Look up a cached parse, or read+parse the file and store the result.
    /// Returns `Ok(None)` if the file is not a supported language.
    pub fn get_or_parse(&self, path: &Path) -> std::io::Result<Option<ParsedFile>> {
        let language = match Language::detect(path) {
            Some(l) => l,
            None => return Ok(None),
        };

        let stamp = FileStamp::of(path)
            .ok_or_else(|| std::io::Error::other(format!("stat {}", path.display())))?;

        if self.capacity > 0 {
            let mut g = self.inner.lock();
            if let Some(pos) = g.entries.iter().position(|e| e.path == path) {
                let entry = &g.entries[pos];
                if entry.stamp == stamp && entry.language == language {
                    let parsed = ParsedFile {
                        language: entry.language,
                        source: entry.source.clone(),
                        tree: entry.tree.clone(),
                    };
                    let e = g.entries.remove(pos).expect("just indexed");
                    g.entries.push_back(e);
                    return Ok(Some(parsed));
                }
                // Stale entry — drop it before we re-parse.
                g.entries.remove(pos);
            }
        }

        let source = std::fs::read(path)?;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&language.ts_language())
            .map_err(|e| std::io::Error::other(format!("set_language {}: {e}", language.name())))?;
        let tree = parser
            .parse(&source, None)
            .ok_or_else(|| std::io::Error::other("tree-sitter parse returned None"))?;

        let parsed = ParsedFile {
            language,
            source: Arc::new(source),
            tree: Arc::new(tree),
        };

        if self.capacity > 0 {
            let mut g = self.inner.lock();
            while g.entries.len() >= self.capacity {
                g.entries.pop_front();
            }
            g.entries.push_back(Entry {
                path: path.to_path_buf(),
                stamp,
                language,
                source: parsed.source.clone(),
                tree: parsed.tree.clone(),
            });
        }
        Ok(Some(parsed))
    }

    /// Drop any cached parse for `path`. Call this from the watcher when
    /// a change event lands — correctness doesn't require it (mtime is
    /// the source of truth), but it lets memory release earlier.
    pub fn evict(&self, path: &Path) {
        if self.capacity == 0 {
            return;
        }
        let mut g = self.inner.lock();
        if let Some(pos) = g.entries.iter().position(|e| e.path == path) {
            g.entries.remove(pos);
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.lock().entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::thread::sleep;
    use std::time::Duration;

    fn write(path: &Path, body: &str) {
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.sync_all().unwrap();
    }

    #[test]
    fn miss_then_hit() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.rs");
        write(&path, "fn a() {}\n");
        let cache = ParseCache::new(4);

        let first = cache.get_or_parse(&path).unwrap().expect("supported");
        assert_eq!(cache.len(), 1);

        let second = cache.get_or_parse(&path).unwrap().expect("supported");
        assert_eq!(cache.len(), 1);
        // Same Arc — the hit path must not re-parse.
        assert!(Arc::ptr_eq(&first.tree, &second.tree));
    }

    #[test]
    fn reparses_on_mtime_change() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.rs");
        write(&path, "fn a() {}\n");
        let cache = ParseCache::new(4);
        let first = cache.get_or_parse(&path).unwrap().expect("supported");

        // Ensure a fresh mtime (some filesystems round to the second).
        sleep(Duration::from_millis(20));
        write(&path, "fn a() {}\nfn b() {}\n");

        let second = cache.get_or_parse(&path).unwrap().expect("supported");
        assert!(!Arc::ptr_eq(&first.tree, &second.tree));
        assert!(!Arc::ptr_eq(&first.source, &second.source));
    }

    #[test]
    fn unsupported_extension_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("notes.md");
        write(&path, "hi\n");
        let cache = ParseCache::new(4);
        assert!(cache.get_or_parse(&path).unwrap().is_none());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn capacity_evicts_oldest() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ParseCache::new(2);
        for name in ["a.rs", "b.rs", "c.rs"] {
            let p = tmp.path().join(name);
            write(&p, "fn x() {}\n");
            let _ = cache.get_or_parse(&p).unwrap();
        }
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn evict_is_idempotent_and_noop_for_unknown_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.rs");
        write(&path, "fn a() {}\n");
        let cache = ParseCache::new(4);
        cache.get_or_parse(&path).unwrap();
        cache.evict(&path);
        cache.evict(&path);
        cache.evict(&tmp.path().join("nope.rs"));
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn zero_capacity_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.rs");
        write(&path, "fn a() {}\n");
        let cache = ParseCache::new(0);
        let first = cache.get_or_parse(&path).unwrap().expect("supported");
        let second = cache.get_or_parse(&path).unwrap().expect("supported");
        assert_eq!(cache.len(), 0);
        // Each call re-parses when caching is off.
        assert!(!Arc::ptr_eq(&first.tree, &second.tree));
    }
}
