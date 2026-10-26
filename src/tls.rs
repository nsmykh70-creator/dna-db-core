// SPDX-License-Identifier: AGPL-3.0-only
//! TLS termination for both the TCP protocol and the embedded HTTP panel.
//!
//! The production CLI enables TLS by default. When no certificate is supplied
//! it creates a self-signed localhost certificate under `<data-dir>/tls/` and
//! prints its path; operators with a public DNS name should always pass
//! `--tls-cert` / `--tls-key` (for example Let's Encrypt files) so clients can
//! verify the chain normally.
//!
//! The implementation uses rustls with the `ring` provider and TLS 1.2/1.3.
//! No OpenSSL or platform-specific TLS stack is required.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcgen::generate_simple_self_signed;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;

fn provider() {
    // Installing twice is harmless. The first call wins and keeps the
    // process-wide crypto provider deterministic.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Load a certificate/key pair, or generate a self-signed pair on first use.
pub fn load_or_generate(
    data_dir: &Path,
    cert_path: Option<&Path>,
    key_path: Option<&Path>,
) -> io::Result<Arc<ServerConfig>> {
    let explicit_pair = cert_path.is_some();
    let (cert_path, key_path) = match (cert_path, key_path) {
        (Some(cert), Some(key)) => (cert.to_path_buf(), key.to_path_buf()),
        (None, None) => {
            let dir = data_dir.join("tls");
            (dir.join("cert.pem"), dir.join("key.pem"))
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--tls-cert and --tls-key must be supplied together",
            ))
        }
    };

    if !cert_path.exists() || !key_path.exists() {
        if explicit_pair {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "explicit TLS pair not found: cert {:?} (exists={}), key {:?} (exists={})",
                    cert_path,
                    cert_path.exists(),
                    key_path,
                    key_path.exists()
                ),
            ));
        }
        if cert_path.exists() || key_path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "incomplete TLS pair: cert {:?} (exists={}), key {:?} (exists={})",
                    cert_path,
                    cert_path.exists(),
                    key_path,
                    key_path.exists()
                ),
            ));
        }
        generate_localhost_pair(&cert_path, &key_path)?;
    }

    let certs = load_certs(&cert_path)?;
    let key = load_key(&key_path)?;
    if certs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("no certificates found in {:?}", cert_path),
        ));
    }
    provider();
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    eprintln!(
        "[TLS] serving with certificate {:?} (self-signed pairs are for development; \
         use --tls-cert/--tls-key for public deployments)",
        cert_path
    );
    Ok(Arc::new(config))
}

fn generate_localhost_pair(cert_path: &Path, key_path: &Path) -> io::Result<()> {
    provider();
    let certified = generate_simple_self_signed(vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ])
    .map_err(|e| io::Error::other(format!("self-signed cert: {e}")))?;
    if let Some(parent) = cert_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(parent) = key_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_private(cert_path, certified.cert.pem().as_bytes())?;
    write_private(key_path, certified.key_pair.serialize_pem().as_bytes())?;
    eprintln!(
        "[TLS] generated self-signed certificate {:?} and key {:?}",
        cert_path, key_path
    );
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn load_certs(path: &Path) -> io::Result<Vec<CertificateDer<'static>>> {
    CertificateDer::pem_file_iter(path)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

fn load_key(path: &Path) -> io::Result<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_file(path)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// Create a rustls server connection over an accepted TCP stream.
pub fn accept(
    config: Arc<ServerConfig>,
    stream: std::net::TcpStream,
) -> io::Result<rustls::StreamOwned<rustls::ServerConnection, std::net::TcpStream>> {
    let connection =
        rustls::ServerConnection::new(config).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(rustls::StreamOwned::new(connection, stream))
}

/// Default TLS directory used by the CLI.
pub fn default_tls_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("tls")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_missing_pair_is_rejected() {
        let dir = std::env::temp_dir().join(format!(
            "dnadb_tls_missing_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let cert = dir.join("missing-cert.pem");
        let key = dir.join("missing-key.pem");
        assert!(load_or_generate(&dir, Some(&cert), Some(&key)).is_err());
        assert!(!cert.exists());
        assert!(!key.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generates_and_loads_local_pair() {
        let dir = std::env::temp_dir().join(format!(
            "dnadb_tls_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let config = load_or_generate(&dir, None, None).unwrap();
        assert!(config.alpn_protocols.is_empty());
        assert!(dir.join("tls/cert.pem").exists());
        assert!(dir.join("tls/key.pem").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
