//! Per-tool counters surfaced via `metrics.gain` (byte savings) and
//! `metrics.tool_latency` (per-call wall-clock).
//!
//! Cheap to maintain (a few atomics per tool) and cheap to read. The
//! point is to put numbers on the project's claims — token/byte
//! savings *and* the M5 fork/exec-replacement latency story —
//! without forcing the caller to re-derive them from response sizes.
//!
//! `record(tool, raw, compacted)` is called by handlers when a
//! response is built. `record_latency(tool, duration_us)` is called
//! by `dispatch` for every method, so latency tracking is automatic
//! and doesn't require per-handler instrumentation.

use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use protocol::{MetricsGainResult, MetricsToolLatencyResult, ToolGainEntry, ToolLatencyEntry};

/// Counters for a single tool name. Bytes track compaction wins;
/// latency tracks per-call wall-clock so we can prove that the
/// fork/exec we save isn't being eaten by daemon-side regression.
#[derive(Default, Debug)]
struct ToolCounter {
    calls: AtomicU64,
    raw_bytes: AtomicU64,
    compacted_bytes: AtomicU64,
    latency_calls: AtomicU64,
    latency_sum_us: AtomicU64,
    latency_max_us: AtomicU64,
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
            ..Default::default()
        };
        guard.push((tool.to_string(), counter));
    }

    /// Record a single dispatched call's wall-clock latency. Called from
    /// `server::dispatch` regardless of compact/raw mode — the byte
    /// counters track compaction wins, this tracks per-call cost.
    pub fn record_latency(&self, tool: &str, duration_us: u64) {
        let guard = self.inner.lock();
        if let Some((_, counter)) = guard.iter().find(|(k, _)| k == tool) {
            counter.latency_calls.fetch_add(1, Ordering::Relaxed);
            counter
                .latency_sum_us
                .fetch_add(duration_us, Ordering::Relaxed);
            update_max(&counter.latency_max_us, duration_us);
            return;
        }
        // First call for this tool: insert under a mutating lock.
        drop(guard);
        let mut guard = self.inner.lock();
        if guard.iter().any(|(k, _)| k == tool) {
            drop(guard);
            return self.record_latency(tool, duration_us);
        }
        let counter = ToolCounter {
            latency_calls: AtomicU64::new(1),
            latency_sum_us: AtomicU64::new(duration_us),
            latency_max_us: AtomicU64::new(duration_us),
            ..Default::default()
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

    /// Per-tool latency snapshot. Tools with zero observed calls are
    /// dropped so the response only carries tools the agent actually
    /// hit. Sorted by `latency_sum_us` desc so the heaviest tool is
    /// first — same idea as `snapshot()` for byte-savings.
    pub fn snapshot_latency(&self) -> MetricsToolLatencyResult {
        let guard = self.inner.lock();
        let mut per_tool: Vec<ToolLatencyEntry> = guard
            .iter()
            .filter_map(|(name, c)| {
                let calls = c.latency_calls.load(Ordering::Relaxed);
                if calls == 0 {
                    return None;
                }
                let sum = c.latency_sum_us.load(Ordering::Relaxed);
                Some(ToolLatencyEntry {
                    tool: name.clone(),
                    calls,
                    latency_sum_us: sum,
                    mean_us: sum / calls,
                    max_us: c.latency_max_us.load(Ordering::Relaxed),
                })
            })
            .collect();
        per_tool.sort_by(|a, b| {
            b.latency_sum_us
                .cmp(&a.latency_sum_us)
                .then(a.tool.cmp(&b.tool))
        });
        MetricsToolLatencyResult { per_tool }
    }
}

/// Atomically bump `target` to `candidate` if `candidate` is larger.
/// Tiny CAS loop — uncontended in steady state, bounded retries even
/// under contention since the value only ever moves up.
fn update_max(target: &AtomicU64, candidate: u64) {
    let mut current = target.load(Ordering::Relaxed);
    while candidate > current {
        match target.compare_exchange_weak(current, candidate, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => return,
            Err(observed) => current = observed,
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

    #[test]
    fn latency_snapshot_tracks_calls_sum_mean_max() {
        let m = ToolMetrics::new();
        m.record_latency("git.status", 100);
        m.record_latency("git.status", 300);
        m.record_latency("git.status", 200);
        m.record_latency("search.grep", 5000);

        let snap = m.snapshot_latency();
        // search.grep has the largest sum so it sorts first.
        assert_eq!(snap.per_tool[0].tool, "search.grep");
        assert_eq!(snap.per_tool[0].calls, 1);
        assert_eq!(snap.per_tool[0].mean_us, 5000);
        assert_eq!(snap.per_tool[0].max_us, 5000);

        let g = &snap.per_tool[1];
        assert_eq!(g.tool, "git.status");
        assert_eq!(g.calls, 3);
        assert_eq!(g.latency_sum_us, 600);
        assert_eq!(g.mean_us, 200);
        assert_eq!(g.max_us, 300);
    }

    #[test]
    fn latency_snapshot_drops_tools_with_no_latency_observations() {
        let m = ToolMetrics::new();
        m.record("git.status", 100, 50); // bytes only, no latency
        let snap = m.snapshot_latency();
        assert!(snap.per_tool.is_empty());
    }

    #[test]
    fn latency_max_only_moves_upward() {
        let m = ToolMetrics::new();
        m.record_latency("fs.read", 500);
        m.record_latency("fs.read", 100);
        m.record_latency("fs.read", 300);
        let snap = m.snapshot_latency();
        assert_eq!(snap.per_tool[0].max_us, 500);
    }
}
