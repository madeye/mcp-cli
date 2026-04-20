//! Per-tool byte-savings counters surfaced via `metrics.gain`.
//!
//! Cheap to maintain (a few atomics per tool) and cheap to read.
//! The point is to put a number on the project's compaction wins
//! without forcing the agent to re-derive savings from response sizes.
//!
//! `record(tool, raw, compacted)` is called by handlers every time a
//! response is built — `raw` is the byte size of the structured
//! response we *would* have sent, `compacted` is the byte size of
//! what we actually sent. For non-compact responses both numbers are
//! the same and the savings ratio for that call is zero.

use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use protocol::{MetricsGainResult, ToolGainEntry};

/// Counters for a single tool name.
#[derive(Default, Debug)]
struct ToolCounter {
    calls: AtomicU64,
    raw_bytes: AtomicU64,
    compacted_bytes: AtomicU64,
}

#[derive(Default)]
pub struct ToolMetrics {
    // Small `Vec<(String, ToolCounter)>` rather than HashMap: there are
    // only a handful of distinct method names and lookup happens on a
    // hot path. Linear scan beats hashing for n < ~16.
    inner: Mutex<Vec<(String, ToolCounter)>>,
}

impl ToolMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, tool: &str, raw_bytes: u64, compacted_bytes: u64) {
        let guard = self.inner.lock();
        if let Some((_, counter)) = guard.iter().find(|(k, _)| k == tool) {
            counter.calls.fetch_add(1, Ordering::Relaxed);
            counter.raw_bytes.fetch_add(raw_bytes, Ordering::Relaxed);
            counter
                .compacted_bytes
                .fetch_add(compacted_bytes, Ordering::Relaxed);
            return;
        }
        // First call for this tool: drop the read lock and acquire a
        // mutating one. Tiny race window where two threads could both
        // insert; we resolve it by checking again under the new lock.
        drop(guard);
        let mut guard = self.inner.lock();
        if guard.iter().any(|(k, _)| k == tool) {
            // Lost the race — recurse; next iteration takes the fast path.
            drop(guard);
            return self.record(tool, raw_bytes, compacted_bytes);
        }
        let counter = ToolCounter {
            calls: AtomicU64::new(1),
            raw_bytes: AtomicU64::new(raw_bytes),
            compacted_bytes: AtomicU64::new(compacted_bytes),
        };
        guard.push((tool.to_string(), counter));
    }

    pub fn snapshot(&self) -> MetricsGainResult {
        let guard = self.inner.lock();
        let mut per_tool: Vec<ToolGainEntry> = guard
            .iter()
            .map(|(name, c)| ToolGainEntry {
                tool: name.clone(),
                calls: c.calls.load(Ordering::Relaxed),
                raw_bytes: c.raw_bytes.load(Ordering::Relaxed),
                compacted_bytes: c.compacted_bytes.load(Ordering::Relaxed),
            })
            .collect();
        // Heaviest tool first so a `metrics.gain` glance immediately
        // shows where the savings (or absence of savings) live.
        per_tool.sort_by(|a, b| b.raw_bytes.cmp(&a.raw_bytes).then(a.tool.cmp(&b.tool)));

        let total_raw_bytes: u64 = per_tool.iter().map(|e| e.raw_bytes).sum();
        let total_compacted_bytes: u64 = per_tool.iter().map(|e| e.compacted_bytes).sum();
        let savings_ratio = if total_raw_bytes == 0 {
            0.0
        } else {
            let r = 1.0 - (total_compacted_bytes as f64 / total_raw_bytes as f64);
            r.clamp(0.0, 1.0)
        };

        MetricsGainResult {
            per_tool,
            total_raw_bytes,
            total_compacted_bytes,
            savings_ratio,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_then_snapshot_round_trips() {
        let m = ToolMetrics::new();
        m.record("git.status", 1000, 200);
        m.record("git.status", 500, 100);
        m.record("search.grep", 2000, 500);

        let snap = m.snapshot();
        assert_eq!(snap.total_raw_bytes, 3500);
        assert_eq!(snap.total_compacted_bytes, 800);
        // 1.0 - 800/3500 ≈ 0.7714…
        assert!((snap.savings_ratio - (1.0 - 800.0 / 3500.0)).abs() < 1e-9);

        // search.grep is heavier (2000 raw bytes) so it sorts first.
        assert_eq!(snap.per_tool[0].tool, "search.grep");
        assert_eq!(snap.per_tool[0].calls, 1);
        assert_eq!(snap.per_tool[1].tool, "git.status");
        assert_eq!(snap.per_tool[1].calls, 2);
    }

    #[test]
    fn savings_ratio_is_zero_when_no_calls_recorded() {
        let m = ToolMetrics::new();
        let snap = m.snapshot();
        assert_eq!(snap.savings_ratio, 0.0);
        assert!(snap.per_tool.is_empty());
    }

    #[test]
    fn savings_ratio_is_clamped_for_pathological_input() {
        // compacted > raw (shouldn't happen in practice — handlers
        // record the *actual* serialized sizes — but a future bug
        // shouldn't make the ratio negative).
        let m = ToolMetrics::new();
        m.record("bug.tool", 100, 200);
        let snap = m.snapshot();
        assert_eq!(snap.savings_ratio, 0.0);
    }
}
