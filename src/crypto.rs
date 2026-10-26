// SPDX-License-Identifier: AGPL-3.0-only
//! Encryption-at-rest primitives.
//!
//! DNA-DB encrypts the *payload* of every logical record with
//! XChaCha20-Poly1305 before it reaches the WAL, mirror or an SST segment.
//! The storage keys (which are part of the LSM ordering/index) stay visible;
//! everything an application stores as a value is authenticated ciphertext.
//!
//! The envelope is self-describing, so an existing plaintext database can be
//! opened with encryption enabled and old rows remain readable. New writes
//! are always encrypted. This makes rolling deployment possible without an
//! offline rewrite, while `doctor` still validates the on-disk checksums.
//!
//! Key material comes from (in order):
//!   * `DNA_DB_ENCRYPTION_KEY` — 64 hex characters (32 bytes), suitable for
//!     a container secret / systemd `LoadCredential` / KMS-injected env;
//!   * `--encryption-key-file PATH` — 32 raw bytes or 64 hex characters;
//!   * `<data-dir>/.dna-db.key` — generated on first start with a loud warning.
//!
//! For production the key must live outside the data directory (or in a KMS /
//! secret manager). The generated file exists for local development and for
//! single-host deployments where the data volume and the key volume have
//! different access controls.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};

const VALUE_MAGIC: &[u8; 5] = b"DNAE1";
const BLOB_MAGIC: &[u8; 5] = b"DNAB1";
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
const KEY_LEN: usize = 32;

#[derive(Clone)]
pub struct Crypto {
    key: [u8; KEY_LEN],
}

impl Crypto {
    pub fn from_key(key: [u8; KEY_LEN]) -> Self {
        Self { key }
    }

    /// Load the master key, creating a development key file when none exists.
    pub fn load_or_create(data_dir: &Path, key_file: Option<&Path>) -> io::Result<Arc<Self>> {
        if let Ok(value) = std::env::var("DNA_DB_ENCRYPTION_KEY") {
            if !value.trim().is_empty() {
                let key = parse_key_text(value.trim()).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("DNA_DB_ENCRYPTION_KEY: {e}"),
                    )
                })?;
                return Ok(Arc::new(Self::from_key(key)));
            }
        }

        let path = key_file
            .map(Path::to_path_buf)
            .unwrap_or_else(|| data_dir.join(".dna-db.key"));
        if path.exists() {
            let key = read_key_file(&path)?;
            return Ok(Arc::new(Self::from_key(key)));
        }

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let mut key = [0u8; KEY_LEN];
        OsRng.fill_bytes(&mut key);
        write_key_file(&path, &key)?;
        eprintln!(
            "[CRYPTO] generated development encryption key at {:?}; \
             move it outside the data directory (or set DNA_DB_ENCRYPTION_KEY) \
             for production",
            path
        );
        Ok(Arc::new(Self::from_key(key)))
    }

    /// Load a key only when a key source already exists.
    ///
    /// Used by offline tools (`doctor`, `sql2dnaql`): they must not silently
    /// create a new key for an existing database.
    pub fn load_existing(
        data_dir: &Path,
        key_file: Option<&Path>,
    ) -> io::Result<Option<Arc<Self>>> {
        if let Ok(value) = std::env::var("DNA_DB_ENCRYPTION_KEY") {
            if !value.trim().is_empty() {
                let key = parse_key_text(value.trim()).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("DNA_DB_ENCRYPTION_KEY: {e}"),
                    )
                })?;
                return Ok(Some(Arc::new(Self::from_key(key))));
            }
        }
        let path = key_file
            .map(Path::to_path_buf)
            .unwrap_or_else(|| data_dir.join(".dna-db.key"));
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(Arc::new(Self::from_key(read_key_file(&path)?))))
    }

    pub fn fingerprint(&self) -> String {
        let digest = Sha256::digest(self.key);
        hex_encode(&digest[..8])
    }

    /// Authenticated value envelope bound to the record key.
    pub fn encrypt_value(&self, key: &[u8], plaintext: &[u8]) -> io::Result<Vec<u8>> {
        let mut nonce = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);
        let cipher = XChaCha20Poly1305::new((&self.key).into());
        let mut aad = Vec::with_capacity(16 + key.len());
        aad.extend_from_slice(b"dna-db/value/v1");
        aad.extend_from_slice(key);
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "value encryption failed"))?;
        let mut out = Vec::with_capacity(VALUE_MAGIC.len() + NONCE_LEN + ciphertext.len());
        out.extend_from_slice(VALUE_MAGIC);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Decrypt a stored value. Plaintext legacy values are returned unchanged;
    /// a malformed envelope is a hard error (never silently downgraded).
    pub fn decrypt_value(&self, key: &[u8], stored: &[u8]) -> io::Result<Vec<u8>> {
        if !stored.starts_with(VALUE_MAGIC) {
            return Ok(stored.to_vec());
        }
        if stored.len() < VALUE_MAGIC.len() + NONCE_LEN + TAG_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "encrypted value envelope is truncated",
            ));
        }
        let nonce_start = VALUE_MAGIC.len();
        let nonce = &stored[nonce_start..nonce_start + NONCE_LEN];
        let ciphertext = &stored[nonce_start + NONCE_LEN..];
        let mut aad = Vec::with_capacity(16 + key.len());
        aad.extend_from_slice(b"dna-db/value/v1");
        aad.extend_from_slice(key);
        let cipher = XChaCha20Poly1305::new((&self.key).into());
        cipher
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "value authentication failed (wrong key or tampered record)",
                )
            })
    }

    /// Encrypt an auxiliary blob (for example a Bloom sidecar).
    pub fn encrypt_blob(&self, context: &str, plaintext: &[u8]) -> io::Result<Vec<u8>> {
        let mut nonce = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);
        let cipher = XChaCha20Poly1305::new((&self.key).into());
        let mut aad = Vec::with_capacity(16 + context.len());
        aad.extend_from_slice(b"dna-db/blob/v1");
        aad.extend_from_slice(context.as_bytes());
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "blob encryption failed"))?;
        let mut out = Vec::with_capacity(BLOB_MAGIC.len() + NONCE_LEN + ciphertext.len());
        out.extend_from_slice(BLOB_MAGIC);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Decrypt an auxiliary blob; plaintext legacy blobs pass through.
    pub fn decrypt_blob(&self, context: &str, stored: &[u8]) -> io::Result<Vec<u8>> {
        if !stored.starts_with(BLOB_MAGIC) {
            return Ok(stored.to_vec());
        }
        if stored.len() < BLOB_MAGIC.len() + NONCE_LEN + TAG_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "encrypted blob envelope is truncated",
            ));
        }
        let nonce_start = BLOB_MAGIC.len();
        let nonce = &stored[nonce_start..nonce_start + NONCE_LEN];
        let ciphertext = &stored[nonce_start + NONCE_LEN..];
        let mut aad = Vec::with_capacity(16 + context.len());
        aad.extend_from_slice(b"dna-db/blob/v1");
        aad.extend_from_slice(context.as_bytes());
        let cipher = XChaCha20Poly1305::new((&self.key).into());
        cipher
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "blob authentication failed (wrong key or tampered sidecar)",
                )
            })
    }
}

fn parse_key_text(text: &str) -> Result<[u8; KEY_LEN], String> {
    if text.len() == 64 {
        let bytes = hex_decode(text)?;
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&bytes);
        return Ok(key);
    }
    // Allow a 32-byte raw key supplied through an environment variable. Use
    // the file path instead when the key contains arbitrary bytes.
    if text.len() == KEY_LEN {
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(text.as_bytes());
        return Ok(key);
    }
    Err("expected 64 hex characters (or 32 raw bytes)".into())
}

fn read_key_file(path: &Path) -> io::Result<[u8; KEY_LEN]> {
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    while bytes.last().is_some_and(|b| b.is_ascii_whitespace()) {
        bytes.pop();
    }
    if bytes.len() == KEY_LEN {
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&bytes);
        return Ok(key);
    }
    if bytes.len() == 64 {
        let decoded = hex_decode(&String::from_utf8_lossy(&bytes)).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid encryption key file {:?}: {e}", path),
            )
        })?;
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&decoded);
        return Ok(key);
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "invalid encryption key file {:?}: expected 32 raw bytes or 64 hex characters",
            path
        ),
    ))
}

fn write_key_file(path: &Path, key: &[u8; KEY_LEN]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let text = hex_encode(key) + "\n";
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

pub fn hex_decode(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err("hex string has odd length".into());
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_nibble(bytes[i]).ok_or_else(|| "invalid hex character".to_string())?;
        let lo = hex_nibble(bytes[i + 1]).ok_or_else(|| "invalid hex character".to_string())?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Default key path used by the CLI and offline tools.
pub fn default_key_path(data_dir: &Path) -> PathBuf {
    data_dir.join(".dna-db.key")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crypto() -> Crypto {
        Crypto::from_key([7u8; KEY_LEN])
    }

    #[test]
    fn round_trip_value_and_key_binding() {
        let c = crypto();
        let encrypted = c.encrypt_value(b"row:1", b"secret payload").unwrap();
        assert!(encrypted.starts_with(VALUE_MAGIC));
        assert_ne!(encrypted, b"secret payload");
        assert_eq!(
            c.decrypt_value(b"row:1", &encrypted).unwrap(),
            b"secret payload"
        );
        assert!(c.decrypt_value(b"row:2", &encrypted).is_err());
        assert_eq!(c.decrypt_value(b"row:1", b"legacy").unwrap(), b"legacy");
    }

    #[test]
    fn round_trip_blob() {
        let c = crypto();
        let encrypted = c.encrypt_blob("bloom:000001.sst", b"filter bytes").unwrap();
        assert_eq!(
            c.decrypt_blob("bloom:000001.sst", &encrypted).unwrap(),
            b"filter bytes"
        );
        assert!(c.decrypt_blob("bloom:000002.sst", &encrypted).is_err());
    }

    #[test]
    fn hex_helpers() {
        assert_eq!(hex_encode(&[0, 15, 255]), "000fff");
        assert_eq!(hex_decode("000fff").unwrap(), vec![0, 15, 255]);
        assert!(hex_decode("xyz").is_err());
    }
}
