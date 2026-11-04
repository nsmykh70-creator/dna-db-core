// SPDX-License-Identifier: AGPL-3.0-only

//! Shared CLI helpers: recursive directory sizing, advisory data-dir locking,
//! trusted-proxy auth validation and bootstrap password generation.

/// Recursively sum the byte size of every file under `dir`.
pub(crate) fn dir_size(dir: &std::path::Path) -> u64 {
    fn walk(p: &std::path::Path, acc: &mut u64) {
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                match e.file_type() {
                    Ok(ft) if ft.is_dir() => walk(&e.path(), acc),
                    Ok(_) => {
                        if let Ok(m) = e.metadata() {
                            *acc += m.len();
                        }
                    }
                    Err(_) => {}
                }
            }
        }
    }
    let mut acc = 0;
    walk(dir, &mut acc);
    acc
}

pub(crate) fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

pub(crate) fn build_proxy_auth(
    trusted_ips: &[std::net::IpAddr],
    user_header: &str,
    secret_header: &str,
    secret: Option<String>,
) -> Result<dna_db_core::admin::ProxyAuthConfig, String> {
    if !valid_header_name(user_header) {
        return Err("--proxy-auth-header must be a valid HTTP header name".into());
    }
    if !valid_header_name(secret_header) {
        return Err("--proxy-auth-secret-header must be a valid HTTP header name".into());
    }
    let secret = secret.filter(|value| !value.is_empty());
    if let Some(value) = &secret {
        if value.len() < 16 {
            return Err("--proxy-auth-secret must be at least 16 characters".into());
        }
    }
    Ok(dna_db_core::admin::ProxyAuthConfig {
        trusted_ips: trusted_ips.to_vec(),
        user_header: user_header.to_string(),
        secret_header: secret_header.to_string(),
        secret,
    })
}

/// Cryptographically random bootstrap password from the Os RNG. 24 symbols
/// from a 58-symbol unambiguous alphabet (no 0/O/1/l/I) — about 140 bits of
/// entropy, typed once and changed at first login.
pub(crate) fn generate_bootstrap_password() -> String {
    use rand::RngCore;
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut bytes = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect()
}

/// Exclusive advisory lock on `<dir>/.dna-db.lock`, held for the lifetime of
/// the returned guard file. The OS releases the flock when the process dies,
/// so a crashed server or doctor never leaves a stale lock behind.
pub(crate) fn acquire_dir_lock(dir: &std::path::Path, who: &str) -> Result<std::fs::File, String> {
    use fs2::FileExt;
    let lock_path = dir.join(".dna-db.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("cannot open lock file {}: {e}", lock_path.display()))?;
    file.try_lock_exclusive().map_err(|_| {
        format!(
            "{} is locked by another dna-db-core process (running {}\u{a0}— or a foreign \
             holder of {}: refusing concurrent access)",
            dir.display(),
            who,
            lock_path.display()
        )
    })?;
    Ok(file)
}
