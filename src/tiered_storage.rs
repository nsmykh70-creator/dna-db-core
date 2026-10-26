// SPDX-License-Identifier: AGPL-3.0-only
//! Tiered Hybrid Storage — REAL file-based tier management.
//!
//! Tier layout on disk:
//!   <data_dir>/            — hot tier: WAL + active SSTables (NVMe-class)
//!   <data_dir>/cold/       — cold tier: aged-out segments (S3/HDD-class in
//!                            production; local dir in v2.1)
//!
//! demote_oldest() moves the oldest segments to the cold dir (get() searches
//! both tiers transparently via Engine's segment list; cold segments are
//! opened read-only from their new location). promote() brings them back.
//! This replaces the previous stub that only flipped an enum with a
//! hardcoded 64MB counter.

use parking_lot::RwLock;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageTier {
    Hot,
    Cold,
}

pub struct TieredStorageManager {
    pub hot_dir: PathBuf,
    pub cold_dir: PathBuf,
    /// Segment id -> tier
    tier_map: RwLock<std::collections::BTreeMap<u64, StorageTier>>,
    demotions: std::sync::atomic::AtomicU64,
    promotions: std::sync::atomic::AtomicU64,
}

impl TieredStorageManager {
    pub fn new(data_dir: impl AsRef<Path>) -> std::io::Result<Self> {
        let hot_dir = data_dir.as_ref().to_path_buf();
        let cold_dir = data_dir.as_ref().join("cold");
        fs::create_dir_all(&cold_dir)?;
        Ok(Self {
            hot_dir,
            cold_dir,
            tier_map: RwLock::new(std::collections::BTreeMap::new()),
            demotions: std::sync::atomic::AtomicU64::new(0),
            promotions: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Move the N oldest segments (lowest ids) to the cold tier.
    /// Returns the number of segments demoted. Files are renamed atomically.
    pub fn demote_oldest(&self, n: usize) -> std::io::Result<usize> {
        let mut moved = 0usize;
        let mut ids: Vec<u64> = Vec::new();
        for entry in fs::read_dir(&self.hot_dir)?.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "sst").unwrap_or(false) {
                if let Some(id) = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    ids.push(id);
                }
            }
        }
        ids.sort_unstable();

        for id in ids.into_iter().take(n) {
            let sst = self.hot_dir.join(format!("{:06}.sst", id));
            let bloom = self.hot_dir.join(format!("{:06}.bloom", id));
            if !sst.exists() {
                continue;
            }
            fs::rename(&sst, self.cold_dir.join(format!("{:06}.sst", id)))?;
            if bloom.exists() {
                let _ = fs::rename(&bloom, self.cold_dir.join(format!("{:06}.bloom", id)));
            }
            self.tier_map.write().insert(id, StorageTier::Cold);
            self.demotions
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            moved += 1;
        }
        Ok(moved)
    }

    /// Bring a segment back to the hot tier.
    pub fn promote(&self, segment_id: u64) -> std::io::Result<bool> {
        let src = self.cold_dir.join(format!("{:06}.sst", segment_id));
        if !src.exists() {
            return Ok(false);
        }
        fs::rename(&src, self.hot_dir.join(format!("{:06}.sst", segment_id)))?;
        let bloom_src = self.cold_dir.join(format!("{:06}.bloom", segment_id));
        if bloom_src.exists() {
            let _ = fs::rename(
                &bloom_src,
                self.hot_dir.join(format!("{:06}.bloom", segment_id)),
            );
        }
        self.tier_map.write().insert(segment_id, StorageTier::Hot);
        self.promotions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(true)
    }

    /// Absolute path of a segment in whichever tier it currently lives.
    /// Segments never touched by demote/promote are assumed hot.
    pub fn locate(&self, segment_id: u64) -> Option<PathBuf> {
        let tier = self.tier_of(segment_id);
        let (dir, name) = match tier {
            StorageTier::Hot => (self.hot_dir.clone(), format!("{:06}.sst", segment_id)),
            StorageTier::Cold => (self.cold_dir.clone(), format!("{:06}.sst", segment_id)),
        };
        let p = dir.join(name);
        p.exists().then_some(p)
    }

    pub fn tier_of(&self, segment_id: u64) -> StorageTier {
        *self
            .tier_map
            .read()
            .get(&segment_id)
            .unwrap_or(&StorageTier::Hot)
    }

    /// Bytes used per tier (scans the dirs — real numbers).
    pub fn tier_bytes(&self) -> std::io::Result<(u64, u64)> {
        let dir_bytes = |dir: &Path| -> std::io::Result<u64> {
            let mut total = 0u64;
            for entry in fs::read_dir(dir)?.flatten() {
                if entry.path().is_file() {
                    total += entry.metadata()?.len();
                }
            }
            Ok(total)
        };
        Ok((dir_bytes(&self.hot_dir)?, dir_bytes(&self.cold_dir)?))
    }

    pub fn stats(&self) -> (u64, u64) {
        (
            self.demotions.load(std::sync::atomic::Ordering::Relaxed),
            self.promotions.load(std::sync::atomic::Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dna_tier_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn fake_segment(dir: &Path, id: u64, size: usize) -> PathBuf {
        let p = dir.join(format!("{:06}.sst", id));
        fs::write(&p, vec![0xAB; size]).unwrap();
        p
    }

    #[test]
    fn demote_and_locate() {
        let dir = tempdir("demote");
        let mgr = TieredStorageManager::new(&dir).unwrap();
        fake_segment(&dir, 1, 100);
        fake_segment(&dir, 2, 200);
        fake_segment(&dir, 3, 300);

        let moved = mgr.demote_oldest(2).unwrap();
        assert_eq!(moved, 2);
        assert_eq!(mgr.tier_of(1), StorageTier::Cold);
        assert_eq!(mgr.tier_of(2), StorageTier::Cold);
        assert_eq!(mgr.tier_of(3), StorageTier::Hot);

        let cold_path = mgr.locate(1).unwrap();
        assert!(cold_path.starts_with(dir.join("cold")));
        let hot_path = mgr.locate(3).unwrap();
        assert!(hot_path.starts_with(&dir) && !hot_path.starts_with(dir.join("cold")));

        let (hot_b, cold_b) = mgr.tier_bytes().unwrap();
        assert_eq!(hot_b, 300);
        assert_eq!(cold_b, 300);
    }

    #[test]
    fn promote_back() {
        let dir = tempdir("promote");
        let mgr = TieredStorageManager::new(&dir).unwrap();
        fake_segment(&dir, 7, 50);
        mgr.demote_oldest(1).unwrap();
        assert_eq!(mgr.tier_of(7), StorageTier::Cold);
        assert!(mgr.promote(7).unwrap());
        assert_eq!(mgr.tier_of(7), StorageTier::Hot);
        assert!(mgr.locate(7).unwrap().starts_with(&dir));
        assert!(!mgr.promote(99).unwrap());
        let (d, p) = mgr.stats();
        assert_eq!((d, p), (1, 1));
    }
}
