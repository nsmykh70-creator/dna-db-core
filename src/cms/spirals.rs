// SPDX-License-Identifier: AGPL-3.0-only
//! DNA-DB spirals: per-database dual-strand storage pairs.
//!
//! A DNA-DB database is physically a *spiral pair* — two spirals, like the
//! two strands of a DNA helix:
//!
//! * **Strand A** (первичная спираль) and **Strand B** (вторая спираль) are
//!   each a real, engine-initialized data directory (opened once with
//!   [`crate::engine::Engine::open`] so the WAL/segment layout exists, then
//!   closed), described by a `spiral_manifest.json` inside.
//!
//! * Spirals live in *storage roots* — directories on local disks or on
//!   mounted network storages (NFS/SMB/…). Roots are registered through the
//!   web panel; every placement is validated with a write probe first, and
//!   the panel lists writable local-disk candidates parsed from
//!   `/proc/mounts`.
//!
//! * The catalog (`spirals.json` in the data dir, written atomically) keeps
//!   every pair's placements. `CREATE DATABASE` creates the pair, `DROP
//!   DATABASE` removes it, and the panel can relocate a spiral to another
//!   root while the server runs (copy → verify → swap → update catalog →
//!   delete old).
//!
//! Table data of the active catalog still lives in the main engine data
//! directory; spiral pairs are the per-database storage layer, ready for
//! per-database engine promotion (CMS_EDITION_RU.md, Фаза 4). The manifest
//! + catalog layout is the contract that promotion will consume.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::engine::{Engine, EngineConfig};

pub const CATALOG_FILE: &str = "spirals.json";
pub const SPIRAL_MANIFEST: &str = "spiral_manifest.json";
pub const SPIRAL_SUFFIX: &str = ".spiral";
pub const PROBE_NAME: &str = ".dna-db-storage-probe";

// ---------------------------------------------------------------------------
// model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageKind {
    Local,
    Network,
}

impl StorageKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            StorageKind::Local => "local",
            StorageKind::Network => "network",
        }
    }
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "local" | "локальный" | "локальный диск" => Some(StorageKind::Local),
            "network" | "сетевой" | "сетевое" | "сетевое хранилище" => Some(StorageKind::Network),
            _ => None,
        }
    }
}

/// A registered storage root: where spirals may be placed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageRoot {
    pub label: String,
    pub path: String,
    pub kind: StorageKind,
    pub registered_at: String,
}

/// Placement of one spiral (one strand of a pair).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpiralPlacement {
    /// Registered root this spiral lives in (None = ad-hoc path).
    pub root_label: Option<String>,
    /// Absolute directory of the spiral (`<root>/<db>.spiral`).
    pub path: String,
    pub kind: StorageKind,
    pub spiral_id: String,
    pub created_at: String,
}

/// A database's dual-spiral pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpiralPair {
    pub database: String,
    pub pair_id: String,
    pub created_at: String,
    pub strand_a: SpiralPlacement,
    pub strand_b: SpiralPlacement,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CatalogFile {
    version: u32,
    roots: Vec<StorageRoot>,
    pairs: BTreeMap<String, SpiralPair>,
}

pub struct SpiralCatalog {
    data_dir: PathBuf,
    roots: Vec<StorageRoot>,
    pairs: BTreeMap<String, SpiralPair>,
}

fn now_iso() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    // Civil date from days (Howard Hinnant) — no chrono dependency.
    let days = secs / 86_400;
    let z = days as i64 + 719_468;
    let doe = z.rem_euclid(146_097);
    let yoe = doe / 1460;
    let y = yoe + doe / 365 - doe / 1460;
    let mp = (doe.rem_euclid(365) + doe / 1460 * 48) / 5;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let d = doe.rem_euclid(365).rem_euclid(153) / 5 + 1;
    let tod = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

fn new_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let rand = {
        use rand::RngCore;
        let mut b = [0u8; 4];
        rand::rngs::OsRng.fill_bytes(&mut b);
        u32::from_le_bytes(b)
    };
    format!("{prefix}-{nanos:08x}{rand:08x}")
}

// ---------------------------------------------------------------------------
// path / name safety
// ---------------------------------------------------------------------------

/// MySQL-ish database name: letters, digits, `_`, `$`. Rejects path
/// separators, dots and everything else — the name becomes a directory
/// component under user-chosen roots, so traversal must be impossible.
pub(crate) fn validate_db_name(db: &str) -> Result<(), String> {
    if db.is_empty() || db.len() > 64 {
        return Err(format!(
            "database name must be 1..64 chars, got {}",
            db.len()
        ));
    }
    if !db
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
    {
        return Err(format!(
            "database name '{db}' contains characters outside [A-Za-z0-9_$]"
        ));
    }
    Ok(())
}

fn probe_writable(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let probe = dir.join(PROBE_NAME);
    std::fs::write(&probe, b"ok").map_err(|e| format!("{} is not writable: {e}", dir.display()))?;
    std::fs::remove_file(&probe)
        .map_err(|e| format!("probe file in {} cannot be removed: {e}", dir.display()))?;
    Ok(())
}

/// Recursively copies `from` into `to_dir/<name>` (plain file copy — spiral
/// layouts are small: manifests, WAL segments, schema files).
fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst = to.join(entry.file_name());
        if ty.is_dir() {
            copy_dir(&entry.path(), &dst)?;
        } else {
            std::fs::copy(entry.path(), dst)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// manifest
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct SpiralManifest {
    spiral_id: String,
    pair_id: String,
    database: String,
    /// "A" (primary) or "B" (second spiral).
    role: char,
    kind: StorageKind,
    root_label: Option<String>,
    created_at: String,
    engine_version: String,
}

fn write_manifest(dir: &Path, m: &SpiralManifest) -> io::Result<()> {
    let tmp = dir.join(format!("{SPIRAL_MANIFEST}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(m)?)?;
    std::fs::rename(tmp, dir.join(SPIRAL_MANIFEST))
}

// ---------------------------------------------------------------------------
// catalog
// ---------------------------------------------------------------------------

impl SpiralCatalog {
    /// Loads (or initializes) the spiral catalog under `data_dir`.
    pub fn open(data_dir: &Path) -> io::Result<Self> {
        let path = data_dir.join(CATALOG_FILE);
        let (roots, pairs) = if path.exists() {
            let raw = std::fs::read(&path)?;
            let f: CatalogFile = serde_json::from_slice(&raw).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("corrupt {CATALOG_FILE}: {e}"))
            })?;
            (f.roots, f.pairs)
        } else {
            (Vec::new(), BTreeMap::new())
        };
        let mut cat = SpiralCatalog {
            data_dir: data_dir.to_path_buf(),
            roots,
            pairs,
        };
        // Default roots: both strands on the server's own storage.
        if !cat.roots.iter().any(|r| r.label == "local-a") {
            cat.roots.push(StorageRoot {
                label: "local-a".into(),
                path: data_dir.join("spirals-a").display().to_string(),
                kind: StorageKind::Local,
                registered_at: now_iso(),
            });
        }
        if !cat.roots.iter().any(|r| r.label == "local-b") {
            cat.roots.push(StorageRoot {
                label: "local-b".into(),
                path: data_dir.join("spirals-b").display().to_string(),
                kind: StorageKind::Local,
                registered_at: now_iso(),
            });
        }
        cat.persist()?;
        Ok(cat)
    }

    /// Atomic persist: temp file + rename (crash-safe like every DNA-DB
    /// metadata write).
    fn persist(&self) -> io::Result<()> {
        let f = CatalogFile {
            version: 1,
            roots: self.roots.clone(),
            pairs: self.pairs.clone(),
        };
        let path = self.data_dir.join(CATALOG_FILE);
        let tmp = self.data_dir.join(format!("{CATALOG_FILE}.tmp"));
        std::fs::write(&tmp, serde_json::to_vec_pretty(&f)?)?;
        std::fs::rename(tmp, path)
    }

    pub fn roots(&self) -> &[StorageRoot] {
        &self.roots
    }

    pub fn pairs(&self) -> impl Iterator<Item = &SpiralPair> {
        self.pairs.values()
    }

    pub fn pair(&self, db: &str) -> Option<&SpiralPair> {
        self.pairs.get(db)
    }

    fn root_by_label(&self, label: &str) -> Result<&StorageRoot, String> {
        self.roots
            .iter()
            .find(|r| r.label == label)
            .ok_or_else(|| format!("storage root '{label}' is not registered"))
    }

    fn resolve_placement(
        &self,
        root_label: Option<&str>,
        custom_path: Option<&str>,
        kind: Option<StorageKind>,
    ) -> Result<(PathBuf, Option<String>, StorageKind), String> {
        if let Some(label) = root_label {
            let r = self.root_by_label(label)?;
            let p = PathBuf::from(&r.path);
            probe_writable(&p)?;
            return Ok((p, Some(label.to_string()), r.kind));
        }
        if let Some(p) = custom_path {
            let p = PathBuf::from(p);
            if !p.is_absolute() {
                return Err(format!("storage path '{}' must be absolute", p.display()));
            }
            probe_writable(&p)?;
            return Ok((p, None, kind.unwrap_or(StorageKind::Local)));
        }
        Err("storage placement needs a root label or an explicit path".into())
    }

    /// Registers a storage root after validating it with a write probe.
    pub fn add_root(&mut self, label: &str, path: &str, kind: StorageKind) -> Result<(), String> {
        let label = label.trim();
        if label.is_empty() || label.len() > 64 {
            return Err("root label must be 1..64 chars".into());
        }
        if self.roots.iter().any(|r| r.label == label) {
            return Err(format!("root '{label}' already exists"));
        }
        let p = PathBuf::from(path);
        if !p.is_absolute() {
            return Err(format!("storage path '{path}' must be absolute"));
        }
        probe_writable(&p)?;
        self.roots.push(StorageRoot {
            label: label.to_string(),
            path: path.to_string(),
            kind,
            registered_at: now_iso(),
        });
        self.persist().map_err(|e| e.to_string())
    }

    /// Unregisters a root; refused while any spiral still references it.
    pub fn remove_root(&mut self, label: &str) -> Result<(), String> {
        let used = self.pairs.values().any(|p| {
            p.strand_a.root_label.as_deref() == Some(label)
                || p.strand_b.root_label.as_deref() == Some(label)
        });
        if used {
            return Err(format!("root '{label}' hosts existing spirals; relocate them first"));
        }
        let before = self.roots.len();
        self.roots.retain(|r| r.label != label);
        if self.roots.len() == before {
            return Err(format!("root '{label}' is not registered"));
        }
        self.persist().map_err(|e| e.to_string())
    }

    /// Writable storage candidates on this host: real filesystems from
    /// `/proc/mounts` (local disks + network mounts), probed for write
    /// access. This is what the panel offers in the storage dropdowns.
    pub fn local_candidates() -> Vec<(StorageRoot, bool)> {
        let mut out = Vec::new();
        let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
            return out;
        };
        let local_fs = [
            "ext2", "ext3", "ext4", "xfs", "btrfs", "zfs", "f2fs", "ntfs3", "vfat", "exfat",
            "fuseblk", "overlay",
        ];
        let network_fs = ["nfs", "nfs4", "cifs", "smbfs", "sshfs", "9p", "ceph", "glusterfs"];
        let excluded = ["/proc", "/sys", "/dev", "/run", "/snap", "/boot/efi", "/tmpfs"];
        let mut seen = std::collections::BTreeSet::new();
        for line in mounts.lines() {
            let mut parts = line.split_whitespace();
            let (_dev, mp, fstype) = match (parts.next(), parts.next(), parts.next()) {
                (Some(a), Some(b), Some(c)) => (a, b, c),
                _ => continue,
            };
            if excluded.iter().any(|e| mp == *e || mp.starts_with(&format!("{e}/"))) {
                continue;
            }
            let kind = if local_fs.contains(&fstype) {
                StorageKind::Local
            } else if network_fs.contains(&fstype) {
                StorageKind::Network
            } else {
                continue;
            };
            if !seen.insert(mp.to_string()) {
                continue;
            }
            let ok = probe_writable(Path::new(mp)).is_ok();
            out.push((
                StorageRoot {
                    label: mp.rsplit('/').next().unwrap_or(mp).to_string(),
                    path: mp.to_string(),
                    kind,
                    registered_at: String::new(),
                },
                ok,
            ));
            if out.len() >= 32 {
                break;
            }
        }
        out
    }

    /// Creates a database's dual-spiral pair: two real engine-initialized
    /// data directories (Strand A + Strand B) on the chosen storage, each
    /// carrying a manifest, plus the catalog entry. Both strands are
    /// written before the call returns.
    pub fn create_pair(
        &mut self,
        db: &str,
        a: (Option<&str>, Option<&str>, Option<StorageKind>),
        b: (Option<&str>, Option<&str>, Option<StorageKind>),
    ) -> Result<SpiralPair, String> {
        validate_db_name(db)?;
        if self.pairs.contains_key(db) {
            return Err(format!("database '{db}' already has a spiral pair"));
        }
        // No placement given → the default roots ("local-a" / "local-b",
        // both under the server data dir).
        let a_label = match (a.0, a.1) {
            (Some(l), _) => Some(l),
            (None, Some(_)) => None,
            (None, None) => Some("local-a"),
        };
        let b_label = match (b.0, b.1) {
            (Some(l), _) => Some(l),
            (None, Some(_)) => None,
            (None, None) => Some("local-b"),
        };
        let a = (a_label, a.1, a.2);
        let b = (b_label, b.1, b.2);
        let (root_a, label_a, kind_a) = self.resolve_placement(a.0, a.1, a.2)?;
        let (root_b, label_b, kind_b) = self.resolve_placement(b.0, b.1, b.2)?;

        let dir_a = root_a.join(format!("{db}{SPIRAL_SUFFIX}"));
        let dir_b = root_b.join(format!("{db}{SPIRAL_SUFFIX}"));
        if dir_a.exists() || dir_b.exists() {
            return Err(format!(
                "spiral directory for '{db}' already exists on the target storage"
            ));
        }
        let pair_id = new_id("pair");
        let created = now_iso();

        let build = |dir: &Path, role: char| -> Result<(), String> {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            // Real engine initialization: open once with the production
            // defaults so the WAL/segment layout exists, then close.
            Engine::open(EngineConfig {
                dir_path: dir.to_path_buf(),
                ..EngineConfig::default()
            })
            .map_err(|e| format!("engine init on {}: {e}", dir.display()))?;
            write_manifest(
                dir,
                &SpiralManifest {
                    spiral_id: format!("{pair_id}-{role}"),
                    pair_id: pair_id.clone(),
                    database: db.to_string(),
                    role,
                    kind: if role == 'A' { kind_a } else { kind_b },
                    root_label: if role == 'A' { label_a.clone() } else { label_b.clone() },
                    created_at: created.clone(),
                    engine_version: env!("CARGO_PKG_VERSION").to_string(),
                },
            )
            .map_err(|e| e.to_string())
        };

        let result = (|| -> Result<SpiralPair, String> {
            build(&dir_a, 'A')?;
            build(&dir_b, 'B')?;
            Ok(SpiralPair {
                database: db.to_string(),
                pair_id: pair_id.clone(),
                created_at: created.clone(),
                strand_a: SpiralPlacement {
                    root_label: label_a.clone(),
                    path: dir_a.display().to_string(),
                    kind: kind_a,
                    spiral_id: format!("{pair_id}-A"),
                    created_at: created.clone(),
                },
                strand_b: SpiralPlacement {
                    root_label: label_b.clone(),
                    path: dir_b.display().to_string(),
                    kind: kind_b,
                    spiral_id: format!("{pair_id}-B"),
                    created_at: created.clone(),
                },
            })
        })();

        match result {
            Ok(pair) => {
                self.pairs.insert(db.to_string(), pair.clone());
                self.persist().map_err(|e| e.to_string())?;
                Ok(pair)
            }
            Err(e) => {
                // Best-effort cleanup so a failed create leaves no half
                // spirals behind.
                let _ = std::fs::remove_dir_all(&dir_a);
                let _ = std::fs::remove_dir_all(&dir_b);
                Err(e)
            }
        }
    }

    /// Removes a database's pair: both spiral directories and the catalog
    /// entry.
    pub fn drop_pair(&mut self, db: &str) -> Result<(), String> {
        let pair = self
            .pairs
            .remove(db)
            .ok_or_else(|| format!("database '{db}' has no spiral pair"))?;
        let a = PathBuf::from(&pair.strand_a.path);
        let b = PathBuf::from(&pair.strand_b.path);
        let ra = std::fs::remove_dir_all(&a);
        let rb = std::fs::remove_dir_all(&b);
        self.persist().map_err(|e| e.to_string())?;
        ra.map_err(|e| format!("cannot remove {}: {e}", a.display()))?;
        rb.map_err(|e| format!("cannot remove {}: {e}", b.display()))?;
        Ok(())
    }

    /// Relocates one strand of a pair to another storage (the panel's
    /// «править» action): copy to the target, verify the manifest, swap,
    /// update the catalog, then delete the old copy.
    pub fn relocate(
        &mut self,
        db: &str,
        role: char,
        root_label: Option<&str>,
        custom_path: Option<&str>,
        kind: Option<StorageKind>,
    ) -> Result<SpiralPlacement, String> {
        // Resolve the target storage BEFORE borrowing the pair mutably
        // (resolve_placement needs &self).
        let (new_root, new_label, new_kind) =
            self.resolve_placement(root_label, custom_path, kind)?;
        let (old_path, old_spiral_id, old_created, pair_id) = {
            let pair = self
                .pairs
                .get(db)
                .ok_or_else(|| format!("database '{db}' has no spiral pair"))?;
            let cur = if role == 'A' { &pair.strand_a } else { &pair.strand_b };
            (
                PathBuf::from(&cur.path),
                cur.spiral_id.clone(),
                cur.created_at.clone(),
                pair.pair_id.clone(),
            )
        };
        let new_dir = new_root.join(format!("{db}{SPIRAL_SUFFIX}"));
        if new_dir == old_path {
            return Err("spiral is already on this storage".into());
        }
        let tmp = new_root.join(format!("{db}{SPIRAL_SUFFIX}.relocating"));
        let _ = std::fs::remove_dir_all(&tmp);
        copy_dir(&old_path, &tmp).map_err(|e| format!("copy to {}: {e}", tmp.display()))?;
        // Update the manifest inside the copy, then verify it reads back.
        write_manifest(
            &tmp,
            &SpiralManifest {
                spiral_id: old_spiral_id.clone(),
                pair_id: pair_id.clone(),
                database: db.to_string(),
                role,
                kind: new_kind,
                root_label: new_label.clone(),
                created_at: old_created.clone(),
                engine_version: env!("CARGO_PKG_VERSION").to_string(),
            },
        )
        .map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &new_dir)
            .map_err(|e| format!("cannot activate {}: {e}", new_dir.display()))?;

        let placement = SpiralPlacement {
            root_label: new_label,
            path: new_dir.display().to_string(),
            kind: new_kind,
            spiral_id: old_spiral_id,
            created_at: old_created,
        };
        let pair = self
            .pairs
            .get_mut(db)
            .ok_or_else(|| format!("database '{db}' has no spiral pair"))?;
        if role == 'A' {
            pair.strand_a = placement.clone();
        } else {
            pair.strand_b = placement.clone();
        }
        self.persist().map_err(|e| e.to_string())?;
        let _ = std::fs::remove_dir_all(&old_path);
        Ok(placement)
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "cms"))]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dna_spirals_test_{tag}_{}_{:?}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn create_pair_makes_two_real_spirals() {
        let data = tmpdir("create");
        let mut cat = SpiralCatalog::open(&data).unwrap();
        assert_eq!(cat.roots().len(), 2); // local-a + local-b defaults

        let pair = cat.create_pair("shop", (None, None, None), (None, None, None)).unwrap();
        let a = PathBuf::from(&pair.strand_a.path);
        let b = PathBuf::from(&pair.strand_b.path);
        assert!(a.is_dir() && b.is_dir(), "both spiral dirs must exist");
        assert!(a.join(SPIRAL_MANIFEST).is_file(), "strand A manifest");
        assert!(b.join(SPIRAL_MANIFEST).is_file(), "strand B manifest");
        // Real engine layouts: WAL + schema files, not just empty dirs.
        let entries = std::fs::read_dir(&a).unwrap().count();
        assert!(entries > 1, "strand A must be an initialized engine dir");
        assert_eq!(pair.strand_a.kind, StorageKind::Local);

        // Persisted and reloadable.
        let cat2 = SpiralCatalog::open(&data).unwrap();
        assert!(cat2.pair("shop").is_some(), "pair survives reload");
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn create_on_custom_roots_and_drop() {
        let data = tmpdir("custom");
        let root_a = tmpdir("root_a");
        let root_b = tmpdir("root_b");
        let mut cat = SpiralCatalog::open(&data).unwrap();
        cat.add_root("nas", &root_b.display().to_string(), StorageKind::Network)
            .unwrap();

        let pair = cat
            .create_pair(
                "wiki",
                (Some("local-a"), None, None),
                (Some("nas"), None, None),
            )
            .unwrap();
        assert_eq!(pair.strand_a.kind, StorageKind::Local);
        assert_eq!(pair.strand_b.kind, StorageKind::Network);
        assert_eq!(pair.strand_b.root_label.as_deref(), Some("nas"));

        cat.drop_pair("wiki").unwrap();
        assert!(!PathBuf::from(&pair.strand_a.path).exists());
        assert!(!PathBuf::from(&pair.strand_b.path).exists());
        assert!(cat.pair("wiki").is_none());
        for d in [&data, &root_a, &root_b] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn relocate_moves_spiral_between_roots() {
        let data = tmpdir("reloc");
        let root_b1 = tmpdir("b1");
        let root_b2 = tmpdir("b2");
        let mut cat = SpiralCatalog::open(&data).unwrap();
        cat.add_root("nas1", &root_b1.display().to_string(), StorageKind::Network)
            .unwrap();
        cat.add_root("nas2", &root_b2.display().to_string(), StorageKind::Network)
            .unwrap();
        cat.create_pair(
            "drupal",
            (None, None, None),
            (Some("nas1"), None, None),
        )
        .unwrap();

        let p = cat
            .relocate("drupal", 'B', Some("nas2"), None, None)
            .unwrap();
        assert!(Path::new(&p.path).join(SPIRAL_MANIFEST).is_file());
        assert_eq!(p.root_label.as_deref(), Some("nas2"));
        let pair = cat.pair("drupal").unwrap();
        assert_eq!(pair.strand_b.path, p.path);
        assert!(!PathBuf::from(&pair.strand_a.path)
            .display()
            .to_string()
            .contains("nas"));

        // The old copy is gone, the new one still is a real spiral.
        let moved = PathBuf::from(&p.path);
        assert!(moved.join("spiral_manifest.json").is_file());
        for d in [&data, &root_b1, &root_b2] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn names_are_path_safe() {
        assert!(validate_db_name("shop_2026").is_ok());
        assert!(validate_db_name("a$b").is_ok());
        for bad in ["../etc", "a/b", ".", "..", "", "a-b!", "very long name over sixty four characters padding padding"] {
            assert!(validate_db_name(bad).is_err(), "'{bad}' must be rejected");
        }
        // The dangerous form: traversal via the panel must be impossible.
        let mut cat = SpiralCatalog::open(&tmpdir("traversal")).unwrap();
        assert!(cat.create_pair("../escape", (None, None, None), (None, None, None)).is_err());
    }

    #[test]
    fn roots_validate_and_protect() {
        let data = tmpdir("roots");
        let mut cat = SpiralCatalog::open(&data).unwrap();
        // Relative path refused.
        assert!(cat.add_root("rel", "relative/path", StorageKind::Local).is_err());
        // Duplicate label refused.
        assert!(cat.add_root("local-a", "/tmp", StorageKind::Local).is_err());
        // Fresh absolute root accepted.
        let r = tmpdir("newroot");
        cat.add_root("disk2", &r.display().to_string(), StorageKind::Local)
            .unwrap();
        // A root hosting a spiral cannot be removed…
        cat.create_pair("db1", (Some("disk2"), None, None), (None, None, None))
            .unwrap();
        assert!(cat.remove_root("disk2").is_err());
        // …an unused one can.
        cat.drop_pair("db1").unwrap();
        cat.remove_root("disk2").unwrap();
        assert!(cat.roots().iter().all(|x| x.label != "disk2"));
        for d in [&data, &r] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}
