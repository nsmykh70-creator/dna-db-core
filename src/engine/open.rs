// SPDX-License-Identifier: AGPL-3.0-only
//! `Engine::open` — full recovery: segment load (both formats), closed-WAL
//! repair from parity, WAL replay into the memtable, repair-hook install
//! and scrubber start.

use super::*;

impl Engine {
    /// Open (or create) an engine. Performs full recovery:
    /// loads segments (both formats), repairs closed WALs from parity,
    /// replays WAL entries into the memtable, installs the repair hook and
    /// starts the scrubber when configured.
    pub fn open(config: EngineConfig) -> Result<Arc<Self>, std::io::Error> {
        if config.erasure_k == 0
            || config.erasure_m == 0
            || config.erasure_k + config.erasure_m > 256
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid erasure geometry: k, m >= 1 and k + m <= 256 required",
            ));
        }
        std::fs::create_dir_all(&config.dir_path)?;

        let wal_path = config.dir_path.join("active.wal");
        let primary_wal_missing = !wal_path.exists();
        if primary_wal_missing {
            let wal_file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&wal_path)?;
            wal_file.sync_all()?;
            // Best-effort directory fsync: opening a directory handle via
            // `File::open` is not supported on Windows (PermissionDenied), so a
            // failure here must not abort engine startup (the WAL file itself is
            // fsynced above, which is the strict durability guarantee we need).
            let _ = File::open(&config.dir_path).and_then(|f| f.sync_all());
        }

        let mirror = match config.mirror_disk_path.as_ref() {
            Some(path) => Some(Arc::new(DualStrandMirror::new(&config.dir_path, path)?)),
            None => None,
        };
        let mirrored_segment_ids = match &mirror {
            Some(mirror) => Some(mirror.repair_segment_set()?),
            None => None,
        };

        // 1. Load existing segments (newest first). Both formats are read;
        //    writing always produces DNASST02.
        let mut readers = Vec::new();
        let mut max_seg_id = 0u64;
        let mut legacy_found = 0usize;
        let mut total_quarantined = 0usize;
        let mut total_repairable = 0usize;
        let segment_paths: Vec<PathBuf> = if let Some(segment_ids) = mirrored_segment_ids {
            segment_ids
                .into_iter()
                .map(|id| config.dir_path.join(format!("{id:06}.sst")))
                .collect()
        } else {
            std::fs::read_dir(&config.dir_path)?
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|extension| extension == "sst"))
                .collect()
        };
        for path in segment_paths {
            let stem = path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid SST filename")
            })?;
            let seg_id: u64 = stem.parse().map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid SST segment id")
            })?;
            let reader = SegmentFile::open(&path)?;
            if reader.format_tag() == "DNASST01" {
                legacy_found += 1;
            }
            // Quarantine-mode validation: stripes beyond parity stay
            // localised (honest per-key errors), the segment keeps serving.
            let (quarantined, repairable) = reader.validate_for_open()?;
            total_quarantined += quarantined;
            total_repairable += repairable;
            max_seg_id = max_seg_id.max(seg_id);
            readers.push((seg_id, Arc::new(reader)));
        }
        if total_quarantined > 0 {
            eprintln!(
                "[ENGINE] {} stripe(s) quarantined and {} stripe(s) repairable across segments; \
                 run `dna-db-core doctor --data-dir {} --fix`, restore the mirror, or accept \
                 honest errors for the affected keys",
                total_quarantined,
                total_repairable,
                config.dir_path.display()
            );
        }
        if legacy_found > 0 {
            eprintln!(
                "[ENGINE] {} legacy DNASST01 segment(s) detected; they are readable but not \
                 parity-protected. Run `dna-db-core doctor --data-dir {} --migrate` to upgrade.",
                legacy_found,
                config.dir_path.display()
            );
        }
        readers.sort_by(|a, b| b.0.cmp(&a.0));
        let segments: Vec<Arc<SegmentFile>> = readers.into_iter().map(|(_, r)| r).collect();

        // 2. Closed WAL segments: RS repair from parity sidecars, then frame
        //    recovery. Cascade: beyond parity → Strand B copy → parity again.
        // Closed WAL segments: first restore any B-only closed WAL that is
        // missing from A, then run parity/frame recovery on the primary set.
        if let Some(mirror) = &mirror {
            match mirror.heal_primary_closed_wals(&config.dir_path) {
                Ok(n) if n > 0 => {
                    eprintln!("[WAL] restored {n} B-only closed segment(s) to Strand A")
                }
                Ok(_) => {}
                Err(e) => eprintln!("[WAL] Strand A closed-WAL restore failed: {e}"),
            }
        }
        let mut closed_wals = std::fs::read_dir(&config.dir_path)?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("wal_") && n.ends_with(".wal"))
                    .unwrap_or(false)
            })
            .collect::<Vec<PathBuf>>();
        closed_wals.sort();
        let mut max_wal_seq = 0u64;
        let mut wal_entries: Vec<WalEntry> = Vec::new();
        for path in &closed_wals {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let seq = name
                .trim_start_matches("wal_")
                .trim_end_matches(".wal")
                .parse::<u64>()
                .unwrap_or(0);
            max_wal_seq = max_wal_seq.max(seq);

            let len_before = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            let mut rs_repaired = 0usize;
            let mut parity_ok = true;
            match WalParity::repair(path) {
                Ok(n) => rs_repaired = n,
                Err(e) => {
                    parity_ok = false;
                    eprintln!("[WAL] parity repair failed for {name}: {e}");
                }
            }
            // Frame recovery on Strand A: stops at the first torn frame and
            // truncates the file to the last valid offset.
            let mut entries = WalReader::recover(path)?;
            let mut a_valid_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            let a_lost_frames = a_valid_len < len_before;
            // B→A cascade: A lost frames (RS exhausted / sidecar gone) or its
            // sidecar is unusable — take B's copy only when it holds strictly
            // more valid bytes (downgrade guard: frames A already recovered
            // are never discarded for a worse B copy).
            let mut restored_from_b = false;
            if (a_lost_frames || !parity_ok) && mirror.is_some() {
                match mirror.as_ref().unwrap().restore_closed_wal_better(
                    &name,
                    &config.dir_path,
                    a_valid_len,
                ) {
                    Ok(Some(b_valid_len)) => {
                        eprintln!(
                            "[WAL] {name}: Strand A held {a_valid_len} valid byte(s), \
                             Strand B {b_valid_len}; restored from Strand B"
                        );
                        entries = WalReader::recover(path)?;
                        a_valid_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                        restored_from_b = true;
                    }
                    Ok(None) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        eprintln!("[WAL] {name}: damaged on Strand A, no copy exists on Strand B");
                    }
                    Err(e) => eprintln!("[WAL] mirror cascade failed for {name}: {e}"),
                }
            }
            if rs_repaired > 0 || a_lost_frames {
                eprintln!(
                    "[WAL] closed segment {name}: {} block(s) restored by parity, \
                     {} valid byte(s) kept",
                    rs_repaired, a_valid_len
                );
            }
            // The parity sidecar is corrupt or stale after a cascade restore
            // — rebuild it from the validated file.
            if restored_from_b || !parity_ok {
                if let Err(e) = WalParity::protect(path, config.erasure_k, config.erasure_m) {
                    eprintln!("[WAL] parity re-protect failed for {name}: {e}");
                }
            }
            wal_entries.extend(entries);
            // Keep B byte-identical after an A-side repair.
            if (rs_repaired > 0 || a_lost_frames || restored_from_b) && mirror.is_some() {
                let _ = mirror
                    .as_ref()
                    .unwrap()
                    .sync_closed_wal(&name, &config.dir_path);
            }
        }

        // Closed-WAL cascade, A→B half: rebuild damaged/missing Strand B
        // copies from the now-validated Strand A files (B→A half ran above
        // where parity was insufficient). Both spirals heal each other.
        if let Some(mirror) = &mirror {
            match mirror.heal_backup_closed_wals(&config.dir_path) {
                Ok(n) if n > 0 => eprintln!(
                    "[WAL] rebuilt {n} damaged closed segment(s) on Strand B from Strand A"
                ),
                Ok(_) => {}
                Err(e) => eprintln!("[WAL] Strand B closed-WAL healing failed: {e}"),
            }
        }

        // 3. Validate the ACTIVE WAL and restore a damaged Strand A log from B.
        let wal_len_before = std::fs::metadata(&wal_path)
            .map(|metadata| metadata.len())
            .ok();
        let active_entries = WalReader::recover(&wal_path)?;
        let wal_len_after = std::fs::metadata(&wal_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let primary_wal_damaged = (primary_wal_missing
            && mirror
                .as_ref()
                .map(|m| m.backup_wal_size() > 0 || m.backup_wal_is_damaged())
                .unwrap_or(false))
            || wal_len_before
                .map(|before| wal_len_after < before)
                .unwrap_or(false);
        let active_entries = if let Some(mirror) = &mirror {
            mirror.repair_wal(&wal_path, primary_wal_damaged)?
        } else {
            active_entries
        };
        wal_entries.extend(active_entries);

        let wal = WalWriter::open(&wal_path, config.sync_mode)?;
        let memtable = Arc::new(MemTable::new(config.memtable_capacity));

        // Repair journal + hook (installed into every block-format reader).
        let journal = RepairJournal::open(&config.dir_path).ok().map(Arc::new);
        let stats = Arc::new(EngineStats::new());
        let hook: Arc<EngineRepairHook> = Arc::new(EngineRepairHook {
            policy: config.repair_policy,
            stats: stats.clone(),
            journal,
            mirror: mirror.clone(),
        });

        let mut engine = Arc::new(Self {
            mutation_gate: RwLock::new(()),
            active_memtable: RwLock::new(memtable.clone()),
            immutable_memtable: RwLock::new(None),
            wal: RwLock::new(wal),
            wal_path: wal_path.clone(),
            mirror: mirror.clone(),
            segments: RwLock::new(segments),
            next_segment_id: AtomicU64::new(max_seg_id + 1),
            wal_seq: AtomicU64::new(max_wal_seq + 1),
            config: EngineConfig {
                memtable_capacity: config.memtable_capacity,
                ..config
            },
            stats: stats.clone(),
            flush_lock: Mutex::new(()),
            repair_hook: Some(hook.clone()),
            scrubber: None,
        });

        // Install the hook into every loaded DNASST02 reader.
        for seg in engine.segments.read().iter() {
            seg.set_hook(hook.clone());
        }

        // Start the background scrubber when configured.
        if let Some(scrub_config) = engine.config.scrub.clone() {
            let hook2 = hook.clone();
            let stats2 = stats.clone();
            let segments_ref: std::sync::Weak<Engine> = Arc::downgrade(&engine);
            let list = move || -> Vec<PathBuf> {
                match segments_ref.upgrade() {
                    Some(engine) => engine
                        .segments
                        .read()
                        .iter()
                        .map(|s| s.path().to_path_buf())
                        .collect(),
                    None => Vec::new(),
                }
            };
            let scrubber = Scrubber::spawn(scrub_config, stats2, hook2, list);
            // No other Arc<Engine> clone exists yet, so exclusive access is fine.
            match Arc::get_mut(&mut engine) {
                Some(e) => e.scrubber = Some(scrubber),
                // Extremely unlikely (scrubber already grabbed a strong ref);
                // stop the thread instead of leaking an unstoppable handle.
                None => scrubber.stop(),
            }
        }

        let replay_count = wal_entries.len();
        for entry in wal_entries {
            // Replay into the ENGINE's current active memtable (not a stale
            // local Arc): when the memtable fills up mid-replay we flush and
            // the engine swaps in a fresh one. Without the in-loop capacity
            // check, replaying a huge WAL buffered the ENTIRE log in memory
            // before the single end-of-loop check — an OOM on restart.
            {
                let mem = engine.active_memtable.read();
                match &entry {
                    WalEntry {
                        op: WalOp::Put,
                        key,
                        value: Some(v),
                    } => {
                        mem.put(key.clone(), v.clone());
                    }
                    WalEntry {
                        op: WalOp::Delete,
                        key,
                        ..
                    } => {
                        mem.delete(key.clone());
                    }
                    WalEntry {
                        op: WalOp::Put,
                        key,
                        value: None,
                    } => {
                        // batch delete payload quirk: treat as delete
                        let _ = key;
                    }
                }
            }
            if engine.active_memtable.read().is_full() {
                engine.ensure_capacity()?;
            }
        }
        engine
            .stats
            .wal_replayed_entries
            .store(replay_count as u64, Ordering::Relaxed);

        Ok(engine)
    }

}
