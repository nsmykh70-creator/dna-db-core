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
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

// ---------------------------------------------------------------------------
// v3.9 — Cloud object storage tier (AWS S3 / GCS-S3-JSON / MinIO)
// ---------------------------------------------------------------------------

/// Minimal S3-compatible object client with AWS Signature V4. Speaks plain
/// HTTP (MinIO, tests, local emulators) or HTTPS via rustls (AWS/GCS).
/// Implemented on purpose without new dependencies: HMAC-SHA256 is two
/// SHA-256 passes (RFC 2104), everything else is string canonicalisation.
pub struct S3Client {
    pub endpoint: String, // host[:port], no scheme
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    pub https: bool,
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    h.finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// RFC 2104 HMAC-SHA256 without an `hmac` crate dependency.
fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut k = [0u8; 64];
    if key.len() > 64 {
        let mut h = Sha256::new();
        h.update(key);
        k[..32].copy_from_slice(&h.finalize());
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(data);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(inner);
    outer.finalize().into()
}

fn hmac_hex(key: &[u8], data: &[u8]) -> String {
    hmac_sha256(key, data).iter().map(|b| format!("{b:02x}")).collect()
}

impl S3Client {
    pub fn from_env() -> Option<Self> {
        let endpoint = std::env::var("DNA_DB_S3_ENDPOINT").ok()?;
        Some(Self {
            endpoint,
            bucket: std::env::var("DNA_DB_S3_BUCKET").ok()?,
            region: std::env::var("DNA_DB_S3_REGION").unwrap_or_else(|_| "us-east-1".into()),
            access_key: std::env::var("DNA_DB_S3_ACCESS_KEY").ok()?,
            secret_key: std::env::var("DNA_DB_S3_SECRET_KEY").ok()?,
            https: std::env::var("DNA_DB_S3_HTTPS")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
        })
    }

    /// SigV4 authorization header for a request.
    fn authorize(
        &self,
        method: &str,
        object_key: &str,
        payload_sha256: &str,
        amz_date: &str,
    ) -> String {
        let host = &self.endpoint;
        let canonical_uri = format!("/{}/{}", self.bucket, object_key);
        let canonical_request = format!(
            "{method}\n{uri}\n\nhost:{host}\nx-amz-content-sha256:{payload_sha256}\nx-amz-date:{amz_date}\n\nhost;x-amz-content-sha256;x-amz-date\n{payload_sha256}",
            method = method,
            uri = canonical_uri,
            payload_sha256 = payload_sha256,
            amz_date = amz_date,
        );
        let scope = format!(
            "{}/{}\n",
            &amz_date[..8],
            format!("{}/s3/aws4_request", self.region)
        );
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}{}",
            sha256_hex(canonical_request.as_bytes())
        );
        let k_date = hmac_sha256(
            format!("AWS4{}", self.secret_key).as_bytes(),
            amz_date[..8].as_bytes(),
        );
        let k_region = hmac_sha256(&k_date, self.region.as_bytes());
        let k_service = hmac_sha256(&k_region, b"s3");
        let k_signing = hmac_sha256(&k_service, b"aws4_request");
        let signature = hmac_hex(&k_signing, string_to_sign.as_bytes());
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{}/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={}",
            self.access_key,
            &amz_date[..8],
            signature
        )
    }

    fn request(
        &self,
        method: &str,
        object_key: &str,
        body: Option<&[u8]>,
    ) -> Result<Vec<u8>, String> {
        let payload = body.unwrap_or(&[]);
        let payload_sha = sha256_hex(payload);
        let amz_date = chrono_amz_date();
        let auth = self.authorize(method, object_key, &payload_sha, &amz_date);
        let host = self.endpoint.clone();
        let path = format!("/{}/{}", self.bucket, object_key);
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nx-amz-content-sha256: {payload_sha}\r\nx-amz-date: {amz_date}\r\nAuthorization: {auth}\r\nConnection: close\r\n"
        );
        if let Some(b) = body {
            req.push_str(&format!("Content-Length: {}\r\n", b.len()));
        }
        req.push_str("\r\n");

        let mut stream: Box<dyn ReadWrite> = if self.https {
            let server = rustls_pki_types::ServerName::try_from(host.clone())
                .map_err(|e| format!("bad S3 host: {e:?}"))?;
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            let cfg = Arc::new(
                rustls::ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            );
            let conn = rustls::ClientConnection::new(cfg, server)
                .map_err(|e| format!("tls: {e}"))?;
            Box::new(rustls::StreamOwned::new(
                conn,
                std::net::TcpStream::connect(&host).map_err(|e| format!("connect: {e}"))?,
            ))
        } else {
            Box::new(
                std::net::TcpStream::connect(&host).map_err(|e| format!("connect: {e}"))?,
            )
        };
        stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
        if let Some(b) = body {
            stream.write_all(b).map_err(|e| e.to_string())?;
        }
        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&resp);
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(|| "bad HTTP response from S3 endpoint".to_string())?;
        let body_start = resp
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|i| i + 4)
            .unwrap_or(resp.len());
        if (200..300).contains(&status) {
            Ok(resp[body_start..].to_vec())
        } else {
            Err(format!(
                "S3 {method} {} -> HTTP {} : {}",
                object_key,
                status,
                String::from_utf8_lossy(&resp[body_start..resp.len().min(body_start + 200)])
            ))
        }
    }

    pub fn put_object(&self, key: &str, data: &[u8]) -> Result<(), String> {
        self.request("PUT", key, Some(data)).map(|_| ())
    }

    pub fn get_object(&self, key: &str) -> Result<Vec<u8>, String> {
        self.request("GET", key, None)
    }

    pub fn delete_object(&self, key: &str) -> Result<(), String> {
        self.request("DELETE", key, None).map(|_| ())
    }
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}


fn chrono_amz_date() -> String {
    // UTC YYYYmmddTHHMMSSZ without pulling a time crate: seconds since epoch
    // are converted with the standard civil-from-days algorithm.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // civil_from_days (Howard Hinnant's algorithm)
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        y, mth, d, h, m, s
    )
}

impl TieredStorageManager {
    /// v3.9: demote a segment into object storage. Returns the S3 key.
    pub fn demote_segment_to_s3(
        &self,
        s3: &S3Client,
        segment_id: u64,
    ) -> Result<String, String> {
        let local = self.hot_dir.join(format!("{:06}.sst", segment_id));
        let data = std::fs::read(&local).map_err(|e| format!("read {}: {e}", local.display()))?;
        let key = format!("cold/{:06}.sst", segment_id);
        s3.put_object(&key, &data)?;
        // Upload the Bloom sidecar when present (cheap membership on restore).
        let bloom = self.hot_dir.join(format!("{:06}.bloom", segment_id));
        if bloom.exists() {
            let bd = std::fs::read(&bloom).map_err(|e| e.to_string())?;
            let _ = s3.put_object(&format!("cold/{:06}.bloom", segment_id), &bd);
        }
        // Only remove the local copy after the remote copy is complete.
        std::fs::remove_file(&local).map_err(|e| e.to_string())?;
        self.tier_map
            .write()
            .insert(segment_id, StorageTier::Cold);
        self.demotions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(key)
    }

    /// v3.9: fetch a segment back from object storage into the hot tier.
    pub fn promote_segment_from_s3(
        &self,
        s3: &S3Client,
        segment_id: u64,
    ) -> Result<bool, String> {
        let key = format!("cold/{:06}.sst", segment_id);
        let data = match s3.get_object(&key) {
            Ok(d) => d,
            Err(_) => return Ok(false),
        };
        let dst = self.hot_dir.join(format!("{:06}.sst", segment_id));
        std::fs::write(&dst, &data).map_err(|e| e.to_string())?;
        if let Ok(bd) = s3.get_object(&format!("cold/{:06}.bloom", segment_id)) {
            let _ = std::fs::write(self.hot_dir.join(format!("{:06}.bloom", segment_id)), bd);
        }
        self.tier_map
            .write()
            .insert(segment_id, StorageTier::Hot);
        self.promotions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(true)
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
