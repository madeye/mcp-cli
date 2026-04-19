use std::collections::{HashMap, VecDeque};

use parking_lot::Mutex;
use protocol::{ChangeEntry, ChangeKind};

pub struct ChangeLog {
    inner: Mutex<Inner>,
    capacity: usize,
}

struct Inner {
    /// Monotonic cursor. Always the version of the most-recently recorded event,
    /// or 0 if nothing has been recorded yet.
    version: u64,
    /// Bounded ring of raw events, oldest first.
    entries: VecDeque<ChangeEntry>,
    /// Total events ever evicted; clients whose `since` predates this must
    /// re-scan because we no longer have the intermediate history.
    overflow_high_water: u64,
}

impl ChangeLog {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                version: 0,
                entries: VecDeque::with_capacity(capacity),
                overflow_high_water: 0,
            }),
            capacity,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn record(&self, path: String, kind: ChangeKind) {
        let mut g = self.inner.lock();
        g.version += 1;
        let version = g.version;
        if g.entries.len() == self.capacity {
            if let Some(evicted) = g.entries.pop_front() {
                g.overflow_high_water = evicted.version;
            }
        }
        g.entries.push_back(ChangeEntry {
            path,
            kind,
            version,
        });
    }

    pub fn snapshot(&self) -> (u64, u64) {
        let g = self.inner.lock();
        let oldest_retained = g.entries.front().map(|e| e.version).unwrap_or(g.version);
        (g.version, oldest_retained)
    }

    /// Return changes newer than `since`, coalesced per path.
    /// `overflowed = true` means we've evicted events that the client missed.
    ///
    /// Coalescing rules:
    /// * For each path, keep the most-recent event as the representative.
    /// * Drop paths whose first event in the window is `Created` and whose
    ///   last event is `Removed`: the file briefly existed and is gone again,
    ///   and the client never saw it — a `Removed` edge here would be noise.
    pub fn changes_since(&self, since: u64) -> (u64, Vec<ChangeEntry>, bool) {
        let g = self.inner.lock();
        let current = g.version;

        // If `since` is older than anything we still hold, the client has gaps.
        // overflow_high_water is the version of the most recently evicted event.
        let overflowed = since < g.overflow_high_water;

        let mut first: HashMap<&str, &ChangeEntry> = HashMap::new();
        let mut last: HashMap<&str, &ChangeEntry> = HashMap::new();
        for e in g.entries.iter().filter(|e| e.version > since) {
            first.entry(e.path.as_str()).or_insert(e);
            last.insert(e.path.as_str(), e);
        }
        let mut out: Vec<ChangeEntry> = last
            .iter()
            .filter_map(|(path, last_e)| {
                let first_e = first.get(path)?;
                if first_e.kind == ChangeKind::Created && last_e.kind == ChangeKind::Removed {
                    return None;
                }
                Some((*last_e).clone())
            })
            .collect();
        out.sort_by_key(|e| e.version);
        (current, out, overflowed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_snapshot_reports_zero() {
        let log = ChangeLog::with_capacity(64);
        let (version, oldest) = log.snapshot();
        assert_eq!(version, 0);
        assert_eq!(oldest, 0);
    }

    #[test]
    fn records_in_version_order() {
        let log = ChangeLog::with_capacity(64);
        log.record("a.rs".into(), ChangeKind::Created);
        log.record("b.rs".into(), ChangeKind::Modified);
        log.record("c.rs".into(), ChangeKind::Removed);

        let (version, changes, overflowed) = log.changes_since(0);
        assert_eq!(version, 3);
        assert!(!overflowed);
        let versions: Vec<u64> = changes.iter().map(|c| c.version).collect();
        assert_eq!(versions, vec![1, 2, 3]);
    }

    #[test]
    fn coalesces_per_path_to_latest() {
        let log = ChangeLog::with_capacity(64);
        log.record("a.rs".into(), ChangeKind::Created);
        log.record("a.rs".into(), ChangeKind::Modified);
        log.record("a.rs".into(), ChangeKind::Modified);

        let (_, changes, _) = log.changes_since(0);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Modified);
        assert_eq!(changes[0].version, 3);
    }

    #[test]
    fn suppresses_created_then_removed_within_window() {
        let log = ChangeLog::with_capacity(64);
        log.record("tmp.txt".into(), ChangeKind::Created);
        log.record("tmp.txt".into(), ChangeKind::Modified);
        log.record("tmp.txt".into(), ChangeKind::Removed);
        log.record("keep.rs".into(), ChangeKind::Modified);

        let (_, changes, _) = log.changes_since(0);
        let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, vec!["keep.rs"]);
    }

    #[test]
    fn removed_without_create_still_reported() {
        let log = ChangeLog::with_capacity(64);
        // The client's cursor is 0 but the file existed before that — from the
        // client's perspective a lone `Removed` is meaningful and must survive.
        log.record("existing.rs".into(), ChangeKind::Removed);

        let (_, changes, _) = log.changes_since(0);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Removed);
    }

    #[test]
    fn since_filter_excludes_older() {
        let log = ChangeLog::with_capacity(64);
        log.record("a.rs".into(), ChangeKind::Modified); // v=1
        log.record("b.rs".into(), ChangeKind::Modified); // v=2
        log.record("c.rs".into(), ChangeKind::Modified); // v=3

        let (_, changes, _) = log.changes_since(1);
        let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, vec!["b.rs", "c.rs"]);
    }

    #[test]
    fn overflow_trips_when_since_predates_evicted() {
        let log = ChangeLog::with_capacity(2);
        log.record("a".into(), ChangeKind::Created); // v=1
        log.record("b".into(), ChangeKind::Created); // v=2
        log.record("c".into(), ChangeKind::Created); // v=3 evicts v=1

        let (version, _, overflowed) = log.changes_since(0);
        assert_eq!(version, 3);
        assert!(overflowed, "since=0 is older than the evicted v=1");

        let (_, _, overflowed) = log.changes_since(1);
        assert!(
            !overflowed,
            "since=1 matches the evicted watermark but isn't older",
        );
    }

    #[test]
    fn capacity_bounded_ring() {
        let log = ChangeLog::with_capacity(2);
        for i in 0..10 {
            log.record(format!("f{i}.rs"), ChangeKind::Modified);
        }
        let (version, oldest) = log.snapshot();
        assert_eq!(version, 10);
        // Only the newest two entries remain.
        assert_eq!(oldest, 9);
    }
}
