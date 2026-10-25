// SPDX-License-Identifier: AGPL-3.0-only
//! Engine Statistics — real atomic counters (no fabrication).
//!
//! Counters are monotonically increasing since engine open. Latency is
//! measured as a moving view of total time spent in put/get calls divided
//! by op count (cheap, honest average, no fake µs floors).
//!
//! v3.0 adds the Repairosome counters: lesions_found / lesions_repaired /
//! lesions_unrecoverable / bytes_repaired / mirror_rescues and the scrubber
//! progress gauges (scrub_runs, scrub_blocks_scanned vs scrub_blocks_total,
//! scrub_lesions, scrub_last_unix).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[derive(Default)]
pub struct EngineStats {
    pub puts: AtomicU64,
    pub gets: AtomicU64,
    pub deletes: AtomicU64,
    pub bytes_written: AtomicU64,
    pub bytes_read: AtomicU64,
    pub flush_count: AtomicU64,
    pub compaction_count: AtomicU64,
    pub wal_replayed_entries: AtomicU64,
    pub put_time_nanos: AtomicU64,
    pub get_time_nanos: AtomicU64,
    // --- Repairosome (v3.0) ---
    /// Damaged shard blocks detected (any pipeline: read, scrub, doctor).
    pub lesions_found: AtomicU64,
    /// Damaged shard blocks successfully reconstructed.
    pub lesions_repaired: AtomicU64,
    /// Damaged shard blocks beyond parity rescue (honest failures).
    pub lesions_unrecoverable: AtomicU64,
    /// Bytes rewritten by in-place repair.
    pub bytes_repaired: AtomicU64,
    /// Times Strand B restored a segment that parity could not fix.
    pub mirror_rescues: AtomicU64,
    /// Completed WAL rotations.
    pub wal_rotations: AtomicU64,
    // --- Scrubber gauges ---
    pub scrub_runs: AtomicU64,
    pub scrub_blocks_scanned: AtomicU64,
    /// Total blocks of the last full pass (progress denominator).
    pub scrub_blocks_total: AtomicU64,
    /// Lesions found by the scrubber itself.
    pub scrub_lesions: AtomicU64,
    /// Unix time of the last completed scrub pass (0 = never).
    pub scrub_last_unix: AtomicU64,
    started_at: std::sync::Mutex<Option<Instant>>,
}

impl EngineStats {
    pub fn new() -> Self {
        Self {
            started_at: std::sync::Mutex::new(Some(Instant::now())),
            ..Default::default()
        }
    }

    pub fn record_put(&self, bytes: usize, nanos: u64) {
        self.puts.fetch_add(1, Ordering::Relaxed);
        self.bytes_written.fetch_add(bytes as u64, Ordering::Relaxed);
        self.put_time_nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    pub fn record_get(&self, bytes: usize, nanos: u64) {
        self.gets.fetch_add(1, Ordering::Relaxed);
        self.bytes_read.fetch_add(bytes as u64, Ordering::Relaxed);
        self.get_time_nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    pub fn record_delete(&self, nanos: u64) {
        self.deletes.fetch_add(1, Ordering::Relaxed);
        self.put_time_nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started_at
            .lock()
            .map(|g| g.map(|t| t.elapsed().as_secs()).unwrap_or(0))
            .unwrap_or(0)
    }

    pub fn avg_put_us(&self) -> f64 {
        let puts = self.puts.load(Ordering::Relaxed);
        if puts == 0 {
            return 0.0;
        }
        self.put_time_nanos.load(Ordering::Relaxed) as f64 / puts as f64 / 1000.0
    }

    pub fn avg_get_us(&self) -> f64 {
        let gets = self.gets.load(Ordering::Relaxed);
        if gets == 0 {
            return 0.0;
        }
        self.get_time_nanos.load(Ordering::Relaxed) as f64 / gets as f64 / 1000.0
    }

    /// Scrubber progress of the current/last pass, 0.0..=1.0.
    pub fn scrub_progress(&self) -> f64 {
        let total = self.scrub_blocks_total.load(Ordering::Relaxed);
        if total == 0 {
            return 0.0;
        }
        let scanned = self.scrub_blocks_scanned.load(Ordering::Relaxed).min(total);
        scanned as f64 / total as f64
    }

    /// Snapshot as a display string (used by server STATUS / shell).
    pub fn summary(&self) -> String {
        format!(
            "puts={} gets={} deletes={} bytes_written={} bytes_read={} flushes={} compactions={} uptime_secs={} avg_put_us={:.2} avg_get_us={:.2} lesions_found={} lesions_repaired={} lesions_unrecoverable={} bytes_repaired={} mirror_rescues={} wal_rotations={} scrub_runs={} scrub_progress={:.2}",
            self.puts.load(Ordering::Relaxed),
            self.gets.load(Ordering::Relaxed),
            self.deletes.load(Ordering::Relaxed),
            self.bytes_written.load(Ordering::Relaxed),
            self.bytes_read.load(Ordering::Relaxed),
            self.flush_count.load(Ordering::Relaxed),
            self.compaction_count.load(Ordering::Relaxed),
            self.uptime_secs(),
            self.avg_put_us(),
            self.avg_get_us(),
            self.lesions_found.load(Ordering::Relaxed),
            self.lesions_repaired.load(Ordering::Relaxed),
            self.lesions_unrecoverable.load(Ordering::Relaxed),
            self.bytes_repaired.load(Ordering::Relaxed),
            self.mirror_rescues.load(Ordering::Relaxed),
            self.wal_rotations.load(Ordering::Relaxed),
            self.scrub_runs.load(Ordering::Relaxed),
            self.scrub_progress(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate() {
        let s = EngineStats::new();
        s.record_put(100, 500);
        s.record_put(200, 1500);
        assert_eq!(s.puts.load(Ordering::Relaxed), 2);
        assert_eq!(s.bytes_written.load(Ordering::Relaxed), 300);
        assert!((s.avg_put_us() - 1.0).abs() < 0.01); // (500+1500)/2 = 1000ns = 1us
        assert_eq!(s.flush_count.load(Ordering::Relaxed), 0);
        let text = s.summary();
        assert!(text.contains("puts=2"));
    }

    #[test]
    fn repair_counters() {
        let s = EngineStats::new();
        s.lesions_found.fetch_add(5, Ordering::Relaxed);
        s.lesions_repaired.fetch_add(4, Ordering::Relaxed);
        s.lesions_unrecoverable.fetch_add(1, Ordering::Relaxed);
        s.bytes_repaired.fetch_add(4 * 4096, Ordering::Relaxed);
        s.mirror_rescues.fetch_add(1, Ordering::Relaxed);
        s.scrub_blocks_total.store(10, Ordering::Relaxed);
        s.scrub_blocks_scanned.store(5, Ordering::Relaxed);
        assert!((s.scrub_progress() - 0.5).abs() < 1e-9);
        let text = s.summary();
        assert!(text.contains("lesions_found=5"));
        assert!(text.contains("lesions_repaired=4"));
        assert!(text.contains("lesions_unrecoverable=1"));
        assert!(text.contains("mirror_rescues=1"));
        assert!(text.contains("scrub_progress=0.50"));
    }

    #[test]
    fn scrub_progress_zero_when_no_total() {
        let s = EngineStats::new();
        assert_eq!(s.scrub_progress(), 0.0);
    }
}
