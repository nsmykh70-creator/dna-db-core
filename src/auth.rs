// SPDX-License-Identifier: AGPL-3.0-only
//! Persistent multi-user authentication.
//!
//! Users live in `<data-dir>/users.json`. Passwords are Argon2id PHC strings
//! (memory-hard, per-user random salt); the plaintext password is never
//! written to disk or logged. The file is replaced atomically on every change.
//!
//! The bootstrap admin is created only when the file does not exist. The CLI
//! uses `admin` / `password` unless `--admin-pass` or `DNA_DB_ADMIN_PASS` is
//! supplied; the server prints a prominent warning until that password is
//! changed in the web panel.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

const USERS_FILE_VERSION: u32 = 1;
const MIN_PASSWORD_LEN: usize = 8;
const MAX_PASSWORD_LEN: usize = 256;
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserRecord {
    pub username: String,
    pub role: String,
    /// Argon2id PHC string (`$argon2id$v=19$...`).
    pub password_hash: String,
    pub disabled: bool,
    pub must_change: bool,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct UserView {
    pub username: String,
    pub role: String,
    pub disabled: bool,
    pub must_change: bool,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Serialize, Deserialize)]
struct UsersFile {
    version: u32,
    users: Vec<UserRecord>,
}

pub struct UserStore {
    path: Option<PathBuf>,
    users: RwLock<BTreeMap<String, UserRecord>>,
}

impl UserStore {
    /// In-memory store intended for tests and embedded use.
    pub fn in_memory(username: &str, password: &str) -> Result<Self, String> {
        let store = Self {
            path: None,
            users: RwLock::new(BTreeMap::new()),
        };
        store.seed_bootstrap(username, password)?;
        Ok(store)
    }

    /// Open the persistent user database, creating the bootstrap admin only
    /// when the file does not exist.
    pub fn open(path: &Path, bootstrap_user: &str, bootstrap_pass: &str) -> Result<Self, String> {
        if path.exists() {
            let bytes = std::fs::read(path)
                .map_err(|e| format!("cannot read users file {:?}: {e}", path))?;
            let file: UsersFile = serde_json::from_slice(&bytes)
                .map_err(|e| format!("invalid users file {:?}: {e}", path))?;
            if file.version != USERS_FILE_VERSION {
                return Err(format!(
                    "unsupported users file version {} (expected {})",
                    file.version, USERS_FILE_VERSION
                ));
            }
            let mut users = BTreeMap::new();
            for user in file.users {
                users.insert(user.username.clone(), user);
            }
            if users.is_empty() {
                return Err(format!(
                    "users file {:?} contains no users; refusing to silently reset credentials",
                    path
                ));
            }
            return Ok(Self {
                path: Some(path.to_path_buf()),
                users: RwLock::new(users),
            });
        }

        let store = Self {
            path: Some(path.to_path_buf()),
            users: RwLock::new(BTreeMap::new()),
        };
        store.seed_bootstrap(bootstrap_user, bootstrap_pass)?;
        store.persist()?;
        Ok(store)
    }

    fn seed_bootstrap(&self, username: &str, password: &str) -> Result<(), String> {
        validate_username(username)?;
        validate_password(password)?;
        let now = unix_now();
        let record = UserRecord {
            username: username.to_string(),
            role: "admin".into(),
            password_hash: hash_password(password)?,
            disabled: false,
            must_change: false,
            created_at: now,
            updated_at: now,
        };
        self.users.write().insert(username.to_string(), record);
        Ok(())
    }

    fn persist(&self) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let users: Vec<UserRecord> = self.users.read().values().cloned().collect();
        let payload = serde_json::to_vec_pretty(&UsersFile {
            version: USERS_FILE_VERSION,
            users,
        })
        .map_err(|e| format!("cannot serialise users file: {e}"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create users directory: {e}"))?;
        }
        let tmp = path.with_extension(format!(
            "json.tmp.{}.{}.{}",
            std::process::id(),
            TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&tmp)
            .map_err(|e| format!("cannot write users file {:?}: {e}", tmp))?;
        file.write_all(&payload)
            .map_err(|e| format!("cannot write users file {:?}: {e}", tmp))?;
        file.sync_all()
            .map_err(|e| format!("cannot fsync users file {:?}: {e}", tmp))?;
        std::fs::rename(&tmp, path)
            .map_err(|e| format!("cannot replace users file {:?}: {e}", path))?;
        let _ = File::open(path).and_then(|f| f.sync_all());
        Ok(())
    }

    pub fn verify(&self, username: &str, password: &str) -> Option<UserView> {
        let record = self.users.read().get(username).cloned();
        // Always run Argon2, even for an unknown user, so response timing does
        // not reveal whether an account exists.
        let hash = record
            .as_ref()
            .map(|record| record.password_hash.clone())
            .unwrap_or_else(|| dummy_password_hash().to_string());
        let parsed = PasswordHash::new(&hash).ok()?;
        let password_ok = Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok();
        let record = record?;
        if record.disabled || !password_ok {
            return None;
        }
        Some(record.view())
    }

    pub fn change_password(
        &self,
        username: &str,
        old_password: &str,
        new_password: &str,
    ) -> Result<(), String> {
        if self.verify(username, old_password).is_none() {
            return Err("old password is incorrect".into());
        }
        validate_password(new_password)?;
        let hash = hash_password(new_password)?;
        {
            let mut users = self.users.write();
            let record = users
                .get_mut(username)
                .ok_or_else(|| "user not found".to_string())?;
            record.password_hash = hash;
            record.updated_at = unix_now();
            record.must_change = false;
        }
        self.persist()
    }

    pub fn reset_password(&self, username: &str, new_password: &str) -> Result<(), String> {
        validate_password(new_password)?;
        let hash = hash_password(new_password)?;
        {
            let mut users = self.users.write();
            let record = users
                .get_mut(username)
                .ok_or_else(|| "user not found".to_string())?;
            record.password_hash = hash;
            record.updated_at = unix_now();
            record.must_change = true;
        }
        self.persist()
    }

    pub fn create_user(
        &self,
        username: &str,
        password: &str,
        role: &str,
        must_change: bool,
    ) -> Result<(), String> {
        validate_username(username)?;
        validate_password(password)?;
        validate_role(role)?;
        if self.users.read().contains_key(username) {
            return Err("user already exists".into());
        }
        let hash = hash_password(password)?;
        let now = unix_now();
        {
            let mut users = self.users.write();
            if users.contains_key(username) {
                return Err("user already exists".into());
            }
            users.insert(
                username.to_string(),
                UserRecord {
                    username: username.to_string(),
                    role: role.to_string(),
                    password_hash: hash,
                    disabled: false,
                    must_change,
                    created_at: now,
                    updated_at: now,
                },
            );
        }
        self.persist()
    }

    pub fn set_disabled(&self, username: &str, disabled: bool) -> Result<(), String> {
        {
            let mut users = self.users.write();
            if disabled && is_last_active_admin(&users, username) {
                return Err("refusing to disable the last active administrator".into());
            }
            let record = users
                .get_mut(username)
                .ok_or_else(|| "user not found".to_string())?;
            record.disabled = disabled;
            record.updated_at = unix_now();
        }
        self.persist()
    }

    pub fn delete_user(&self, username: &str) -> Result<(), String> {
        {
            let mut users = self.users.write();
            if is_last_active_admin(&users, username) {
                return Err("refusing to delete the last active administrator".into());
            }
            if users.remove(username).is_none() {
                return Err("user not found".into());
            }
        }
        self.persist()
    }

    pub fn list(&self) -> Vec<UserView> {
        self.users.read().values().map(UserRecord::view).collect()
    }

    pub fn is_admin(&self, username: &str) -> bool {
        self.users
            .read()
            .get(username)
            .is_some_and(|user| !user.disabled && user.role == "admin")
    }

    pub fn exists(&self, username: &str) -> bool {
        self.users.read().contains_key(username)
    }
}

impl UserRecord {
    fn view(&self) -> UserView {
        UserView {
            username: self.username.clone(),
            role: self.role.clone(),
            disabled: self.disabled,
            must_change: self.must_change,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

fn is_last_active_admin(users: &BTreeMap<String, UserRecord>, username: &str) -> bool {
    let Some(target) = users.get(username) else {
        return false;
    };
    if target.role != "admin" || target.disabled {
        return false;
    }
    !users
        .values()
        .any(|u| u.role == "admin" && !u.disabled && u.username != username)
}

fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| format!("cannot hash password: {e}"))
}

fn dummy_password_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        hash_password("dna-db-timing-equalisation-dummy")
            .expect("OS CSPRNG must be available for the dummy password hash")
    })
}

fn validate_username(username: &str) -> Result<(), String> {
    if username.is_empty() || username.len() > 64 {
        return Err("username must contain 1..64 characters".into());
    }
    if !username
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err("username may contain only letters, digits, '_', '-' and '.'".into());
    }
    Ok(())
}

fn validate_password(password: &str) -> Result<(), String> {
    if password.len() < MIN_PASSWORD_LEN {
        return Err(format!(
            "password must contain at least {MIN_PASSWORD_LEN} characters"
        ));
    }
    if password.len() > MAX_PASSWORD_LEN {
        return Err(format!(
            "password must contain at most {MAX_PASSWORD_LEN} characters"
        ));
    }
    Ok(())
}

fn validate_role(role: &str) -> Result<(), String> {
    match role {
        "admin" | "operator" | "viewer" => Ok(()),
        _ => Err("role must be one of: admin, operator, viewer".into()),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn user_store_path(data_dir: &Path) -> PathBuf {
    data_dir.join("users.json")
}

/// Share one store between the TCP server and the embedded web panel.
pub type SharedUserStore = Arc<UserStore>;

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dnadb_auth_{}_{}_{}",
            name,
            std::process::id(),
            unix_now()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bootstrap_persists_and_verifies_argon2id() {
        let dir = temp("bootstrap");
        let path = dir.join("users.json");
        let store = UserStore::open(&path, "admin", "password").unwrap();
        assert!(store.verify("admin", "password").is_some());
        assert!(store.verify("admin", "wrong").is_none());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("$argon2id$"));
        assert!(!text.contains("password\""));
        drop(store);
        let reopened = UserStore::open(&path, "ignored", "ignored-password").unwrap();
        assert!(reopened.verify("admin", "password").is_some());
    }

    #[test]
    fn password_change_and_reset() {
        let dir = temp("change");
        let store = UserStore::open(&dir.join("users.json"), "admin", "password").unwrap();
        assert!(store
            .change_password("admin", "wrong", "new-password-1")
            .is_err());
        store
            .change_password("admin", "password", "new-password-1")
            .unwrap();
        assert!(store.verify("admin", "new-password-1").is_some());
        store
            .create_user("viewer", "viewer-pass", "viewer", false)
            .unwrap();
        store.reset_password("viewer", "viewer-pass-2").unwrap();
        assert!(store.verify("viewer", "viewer-pass-2").is_some());
        assert!(!store.is_admin("viewer"));
    }

    #[test]
    fn last_admin_is_protected() {
        let store = UserStore::in_memory("admin", "password").unwrap();
        assert!(store.delete_user("admin").is_err());
        assert!(store.set_disabled("admin", true).is_err());
        store
            .create_user("admin2", "password2", "admin", false)
            .unwrap();
        assert!(store.delete_user("admin").is_ok());
    }
}
