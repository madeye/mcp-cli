//! Tiny LRU for `search.grep` results, invalidated on ChangeLog version bumps.
//!
//! The daemon's whole point is that the file tree in RAM is authoritative. A
//! `search.grep` answer is valid exactly as long as no file under the watched
//! root has changed; once the ChangeLog advances, every cached hit could be
//! stale, so we drop the cache wholesale rather than try to reason about which
//! entries a particular file touched.

use std::collections::VecDeque;

use parking_lot::Mutex;
use protocol::SearchGrepResult;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SearchKey {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    pub max_results: Option<usize>,
    pub case_insensitive: bool,
    /// Context-lines count requested. Part of the key because the
    /// cached response shape differs (hits grow a `context` array)
    /// — a cache hit for `context=0` must not silently satisfy a
    /// `context=5` request.
    pub context: u32,
}

pub struct SearchCache {
    inner: Mutex<Inner>,
    capacity: usize,
}

struct Inner {
    /// Version the cached entries are consistent with. When the daemon's
    /// current ChangeLog version differs, we flush.
    version: u64,
    /// Oldest at front, most-recently-used at back.
    entries: VecDeque<(SearchKey, SearchGrepResult)>,
}

impl SearchCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                version: 0,
                entries: VecDeque::new(),
            }),
            capacity,
        }
    }

    pub fn get(&self, version: u64, key: &SearchKey) -> Option<SearchGrepResult> {
        if self.capacity == 0 {
            return None;
        }
        let mut g = self.inner.lock();
        if g.version != version {
            g.version = version;
            g.entries.clear();
            return None;
        }
        let pos = g.entries.iter().position(|(k, _)| k == key)?;
        let entry = g.entries.remove(pos).expect("position just returned Some");
        let value = entry.1.clone();
        g.entries.push_back(entry);
        Some(value)
    }

    pub fn insert(&self, version: u64, key: SearchKey, value: SearchGrepResult) {
        if self.capacity == 0 {
            return;
        }
        let mut g = self.inner.lock();
        if g.version != version {
            g.version = version;
            g.entries.clear();
        }
        if let Some(pos) = g.entries.iter().position(|(k, _)| k == &key) {
            g.entries.remove(pos);
        }
        while g.entries.len() >= self.capacity {
            g.entries.pop_front();
        }
        g.entries.push_back((key, value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::SearchHit;

    fn hit(path: &str, line: u64) -> SearchHit {
        SearchHit {
            path: path.into(),
            line_number: line,
            line: format!("line {line}"),
            context: Vec::new(),
        }
    }

    fn key(pattern: &str) -> SearchKey {
        SearchKey {
            pattern: pattern.into(),
            path: None,
            glob: None,
            max_results: None,
            case_insensitive: false,
            context: 0,
        }
    }

    fn result(paths: &[&str]) -> SearchGrepResult {
        SearchGrepResult {
            hits: paths.iter().map(|p| hit(p, 1)).collect(),
            truncated: false,
            compact: None,
        }
    }

    #[test]
    fn miss_on_empty_cache() {
        let c = SearchCache::new(4);
        assert!(c.get(1, &key("foo")).is_none());
    }

    #[test]
    fn hit_after_insert() {
        let c = SearchCache::new(4);
        c.insert(1, key("foo"), result(&["a.rs"]));
        let got = c.get(1, &key("foo")).expect("hit");
        assert_eq!(got.hits.len(), 1);
        assert_eq!(got.hits[0].path, "a.rs");
    }

    #[test]
    fn version_bump_flushes() {
        let c = SearchCache::new(4);
        c.insert(1, key("foo"), result(&["a.rs"]));
        assert!(c.get(2, &key("foo")).is_none());
        // And after the flush the new version's cache is empty:
        assert!(c.get(2, &key("foo")).is_none());
    }

    #[test]
    fn capacity_evicts_oldest() {
        let c = SearchCache::new(2);
        c.insert(1, key("a"), result(&["1"]));
        c.insert(1, key("b"), result(&["2"]));
        c.insert(1, key("c"), result(&["3"]));
        assert!(c.get(1, &key("a")).is_none(), "oldest should be evicted");
        assert!(c.get(1, &key("b")).is_some());
        assert!(c.get(1, &key("c")).is_some());
    }

    #[test]
    fn hit_bumps_to_mru() {
        let c = SearchCache::new(2);
        c.insert(1, key("a"), result(&["1"]));
        c.insert(1, key("b"), result(&["2"]));
        // Touch "a" so it becomes MRU; inserting "c" should now evict "b".
        let _ = c.get(1, &key("a"));
        c.insert(1, key("c"), result(&["3"]));
        assert!(c.get(1, &key("a")).is_some());
        assert!(c.get(1, &key("b")).is_none());
        assert!(c.get(1, &key("c")).is_some());
    }

    #[test]
    fn reinsert_dedupes() {
        let c = SearchCache::new(2);
        c.insert(1, key("a"), result(&["old"]));
        c.insert(1, key("a"), result(&["new"]));
        let got = c.get(1, &key("a")).unwrap();
        assert_eq!(got.hits[0].path, "new");
        // Dedup should keep room for one more entry, not evict.
        c.insert(1, key("b"), result(&["b"]));
        assert!(c.get(1, &key("a")).is_some());
        assert!(c.get(1, &key("b")).is_some());
    }

    #[test]
    fn zero_capacity_disabled() {
        let c = SearchCache::new(0);
        c.insert(1, key("a"), result(&["1"]));
        assert!(c.get(1, &key("a")).is_none());
    }
}
