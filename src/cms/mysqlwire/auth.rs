// SPDX-License-Identifier: AGPL-3.0-only
//! Authentication: MySqlAuth account matching and the MySQL
//! native_password scramble (SHA1) verify path.

use super::*;

// ---------------------------------------------------------------------------
// Authentication policy
// ---------------------------------------------------------------------------

/// How the wire listener decides who may connect.
#[derive(Clone)]
pub enum MySqlAuth {
    /// Accept every user/password pair. Default for quick-start deployments;
    /// the server prints a prominent warning. Behaves like MySQL's
    /// `--skip-grant-tables`.
    Trust,
    /// Accept exactly one user — the CMS `DB_USER`.
    Single { name: String, password: String },
    /// Accept a fixed table of users (hosting-style multi-tenant).
    Users(HashMap<String, String>),
}

/// mysql_native_password client scramble:
/// `SHA1(password) XOR SHA1(salt || SHA1(SHA1(password)))`.
pub fn native_password_token(password: &str, salt: &[u8]) -> Vec<u8> {
    let stage1 = Sha1::digest(password.as_bytes());
    let stage2 = Sha1::digest(&stage1);
    let mut h = Sha1::new();
    h.update(salt);
    h.update(&stage2);
    let scramble = h.finalize();
    stage1
        .iter()
        .zip(scramble.iter())
        .map(|(a, b)| a ^ b)
        .collect()
}

/// Server-side check of the same scramble without ever seeing the plaintext:
/// recover `SHA1(password)` from the token, hash it again, compare with the
/// double hash recomputed from the stored password.
pub(crate) fn verify_native_password(token: &[u8], password: &str, salt: &[u8]) -> bool {
    if token.len() != 20 {
        return false;
    }
    let stage1 = Sha1::digest(password.as_bytes());
    let stage2 = Sha1::digest(&stage1);
    let mut h = Sha1::new();
    h.update(salt);
    h.update(&stage2);
    let scramble = h.finalize();
    let recovered: Vec<u8> = token
        .iter()
        .zip(scramble.iter())
        .map(|(a, b)| a ^ b)
        .collect();
    Sha1::digest(&recovered).as_slice() == stage2.as_slice()
}

pub(crate) fn check_password(token: &[u8], stored: &str, salt: &[u8]) -> bool {
    if stored.is_empty() {
        // Empty stored password: MySQL expects the client to send no token.
        return token.is_empty();
    }
    verify_native_password(token, stored, salt)
}

impl MySqlAuth {
    pub(super) fn allows(&self, user: &str, token: &[u8], salt: &[u8]) -> bool {
        match self {
            MySqlAuth::Trust => true,
            MySqlAuth::Single { name, password } => {
                user == name && check_password(token, password, salt)
            }
            MySqlAuth::Users(map) => map
                .get(user)
                .map(|pw| check_password(token, pw, salt))
                .unwrap_or(false),
        }
    }
}

