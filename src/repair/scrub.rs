// SPDX-License-Identifier: AGPL-3.0-only
//! Background scrubber: the enzyme scanners of the Repairosome.
//! Rate-limited full-database sweep that opens every segment and lets the
//! on-read repair hook heal lesions. Split from the former repair.rs
//! monolith — no behaviour change.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::EngineRepairHook;
use crate::segment_block::{phys_slot, BLOCK_SIZE};
use crate::stats::EngineStats;

// ---------------------------------------------------------------------------
// Scrubber — the background enzyme scanner
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ScrubConfig {
    /// Interval between full scrub passes.
    pub interval: Duration,
    /// Upper bound on scrubber read bandwidth.
    pub rate_limit_bytes_per_sec: u64,
}

impl Default for ScrubConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(24 * 3600),
            rate_limit_bytes_per_sec: 32 * 1024 * 1024,
        }
    }
}

/// Handle to stop the scrubber and read its progress.
pub struct Scrubber {
    stop: Arc<std::sync::atomic::AtomicBool>,
    wake: Arc<std::sync::atomic::AtomicBool>,
    pub progress: Arc<EngineStats>,
}

impl Scrubber {
    /// Spawn the background scrubber. `segments` is re-snapshotted via
    /// `list_segments` on every pass, so compaction-superseded files are
    /// never scanned (coordination contract, P3.3 / P4.4).
    pub fn spawn<F>(
        config: ScrubConfig,
        stats: Arc<EngineStats>,
        hook: Arc<EngineRepairHook>,
        list_segments: F,
    ) -> Self
    where
        F: Fn() -> Vec<PathBuf> + Send + 'static,
    {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let wake = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wake2 = wake.clone();
        let progress = stats.clone();
        std::thread::Builder::new()
            .name("dna-scrubber".into())
            .spawn(move || {
                loop {
                    // Sleep in small slices so shutdown stays responsive.
                    let mut waited = Duration::ZERO;
                    while waited < config.interval {
                        if stop2.load(Ordering::Relaxed) {
                            return;
                        }
                        if wake2.swap(false, Ordering::Relaxed) {
                            break; // immediate pass requested
                        }
                        let slice = Duration::from_secs(1).min(config.interval - waited);
                        std::thread::sleep(slice);
                        waited += slice;
                    }
                    if stop2.load(Ordering::Relaxed) {
                        return;
                    }
                    // Active tier first: engine stores segments newest-first;
                    // the list closure already returns that order.
                    let segments = list_segments();
                    stats.scrub_runs.fetch_add(1, Ordering::Relaxed);
                    // Pre-count blocks so progress is meaningful during the
                    // pass. A segment that cannot be opened is skipped (doctor
                    // reports it); it contributes zero.
                    let mut total_blocks = 0u64;
                    for seg in &segments {
                        if let Ok(reader) = crate::segment_block::BlockSegmentReader::open(seg) {
                            total_blocks += reader.block_count() as u64;
                        }
                    }
                    stats.scrub_blocks_scanned.store(0, Ordering::Relaxed);
                    stats
                        .scrub_blocks_total
                        .store(total_blocks, Ordering::Relaxed);
                    for seg in &segments {
                        if stop2.load(Ordering::Relaxed) {
                            return;
                        }
                        scrub_one_segment(seg, &hook, &stats, config.rate_limit_bytes_per_sec);
                    }
                    stats.scrub_last_unix.store(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0),
                        Ordering::Relaxed,
                    );
                }
            })
            .expect("spawn scrubber");
        Self {
            stop,
            wake,
            progress,
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Ask the scrubber to start a pass now instead of waiting for its timer.
    pub fn wake(&self) {
        self.wake.store(true, Ordering::Relaxed);
    }
}

/// Verify every block of one segment (rate-limited), repairing lesions.
/// Returns blocks scanned. Superseded (deleted) files are skipped gracefully.
fn scrub_one_segment(
    path: &Path,
    hook: &Arc<EngineRepairHook>,
    stats: &Arc<EngineStats>,
    rate_limit: u64,
) -> u64 {
    // If compaction deleted this file between snapshot and now — skip.
    if !path.exists() {
        return 0;
    }
    let reader = match crate::segment_block::BlockSegmentReader::open(path) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            eprintln!(
                "[SCRUB] cannot open {:?}: {} (needs doctor)",
                path.file_name().unwrap_or_default(),
                e
            );
            return 0;
        }
    };
    reader.set_hook(hook.clone());
    let blocks = reader.block_count() as u64;
    stats
        .scrub_blocks_total
        .fetch_add(blocks, Ordering::Relaxed);

    // Rate limit: pace the sweep at `rate_limit` bytes/sec.
    let per_block_delay = if rate_limit == 0 {
        Duration::ZERO
    } else {
        Duration::from_nanos((BLOCK_SIZE as u64) * 1_000_000_000 / rate_limit)
    };

    let seed = reader.interleave_seed();
    for region in [false, true] {
        let stripes = reader.stripe_count(region);
        for stripe in 0..stripes {
            // Coordination: file may vanish mid-sweep (compaction). Re-check.
            if !path.exists() {
                return blocks;
            }
            for idx in 0..(reader.erasure_geometry().0 + reader.erasure_geometry().1) {
                let slot = phys_slot(stripe, idx, stripes, seed);
                match reader.verify_block(region, slot) {
                    Ok(true) => {
                        stats.scrub_lesions.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(false) => {}
                    Err(e) => {
                        // Over-parity damage: counted by the hook, honest error.
                        eprintln!(
                            "[SCRUB] {:?} stripe {}: {}",
                            path.file_name().unwrap_or_default(),
                            stripe,
                            e
                        );
                    }
                }
                if !per_block_delay.is_zero() {
                    std::thread::sleep(per_block_delay);
                }
                stats.scrub_blocks_scanned.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    blocks
}

// ---------------------------------------------------------------------------
// Doctor — offline scan / fix / migrate
// ---------------------------------------------------------------------------

/// Verdict for one segment in a doctor report.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SegmentVerdict {
    pub file: String,
    pub format: String,
    pub records: u64,
    pub blocks_total: usize,
    pub damaged_blocks: usize,
    pub affected_stripes: usize,
    pub unrecoverable_stripes: usize,
    /// HEALTHY | REPAIRABLE | UNREPAIRABLE | STRUCTURAL_DAMAGE
    pub verdict: &'static str,
    pub detail: Option<String>,
}

