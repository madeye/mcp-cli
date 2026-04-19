use std::collections::{HashMap, VecDeque};

use parking_lot::Mutex;
use protocol::{ChangeEntry, ChangeKind};

const DEFAULT_CAPACITY: usize = 4096;

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
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

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
    pub fn changes_since(&self, since: u64) -> (u64, Vec<ChangeEntry>, bool) {
        let g = self.inner.lock();
        let current = g.version;

        // If `since` is older than anything we still hold, the client has gaps.
        // overflow_high_water is the version of the most recently evicted event.
        let overflowed = since < g.overflow_high_water;

        // Coalesce by path: keep the highest-version event per path.
        let mut last: HashMap<&str, &ChangeEntry> = HashMap::new();
        for e in g.entries.iter().filter(|e| e.version > since) {
            last.insert(e.path.as_str(), e);
        }
        let mut out: Vec<ChangeEntry> = last.into_values().cloned().collect();
        out.sort_by_key(|e| e.version);
        (current, out, overflowed)
    }
}
