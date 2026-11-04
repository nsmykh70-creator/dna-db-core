// SPDX-License-Identifier: AGPL-3.0-only
//! Server-side security manager: brute-force lockout, attempt history,
//! persistent security journal. Split from the former server.rs monolith —
//! no behaviour change.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::auth::{UserStore, UserView};

// ---------------------------------------------------------------------------
// SecurityManager: Argon2id users, persistent per-IP + per-username lockout
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct AuthenticatedUser {
    pub username: String,
    pub role: String,
    pub allowed_tables: Option<Vec<String>>,
    pub must_change: bool,
}

#[derive(Debug, Clone)]
pub enum AuthError {
    InvalidCredentials,
    Locked(String),
    Internal(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::InvalidCredentials => write!(f, "invalid credentials"),
            AuthError::Locked(msg) => write!(f, "{msg}"),
            AuthError::Internal(msg) => write!(f, "{msg}"),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedAttempt {
    failures: usize,
    last_unix: u64,
}

pub struct SecurityManager {
    pub users: Arc<UserStore>,
    failed_attempts: Mutex<HashMap<String, PersistedAttempt>>,
    state_path: Option<PathBuf>,
    global_failures: Mutex<VecDeque<Instant>>,
    /// Serialise Argon2 verification. Each hash needs ~19 MiB; without this
    /// gate 128 pre-auth connections could reserve several gigabytes.
    auth_gate: Mutex<()>,
    /// Serialise atomic replacement of `security_state.json` so concurrent
    /// failures cannot rename an older snapshot over a newer one.
    state_write: Mutex<()>,
    pub lockout_duration: Duration,
    pub max_failed_attempts: usize,
}

const GLOBAL_FAILURE_WINDOW: Duration = Duration::from_secs(60);
const GLOBAL_FAILURE_LIMIT: usize = 2_000;
pub(super) const PRE_AUTH_TIMEOUT: Duration = Duration::from_secs(10);
/// Absolute wall-clock budget for the whole pre-auth phase. The per-read
/// timeout alone allowed a slowloris client to hold a connection forever by
/// drip-feeding one byte every 9 seconds; with this deadline an
/// unauthenticated connection is dropped after 30 s no matter how slowly it
/// sends.
pub(super) const PRE_AUTH_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const AUTHENTICATED_TIMEOUT: Duration = Duration::from_secs(300);
static TEMP_FILE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Unauthenticated clients may only send an AUTH frame. Capping it at 64 KiB
/// removes the 128 x 16 MiB pre-auth allocation amplification.
pub const PRE_AUTH_MAX_FRAME_SIZE: usize = 64 * 1024;

impl SecurityManager {
    /// In-memory manager for unit tests and embedded callers.
    pub fn new(admin_user: &str, admin_pass: &str) -> Self {
        let users = UserStore::in_memory(admin_user, admin_pass)
            .expect("invalid test bootstrap credentials");
        Self::with_store(Arc::new(users), None)
    }

    /// Open the persistent user database and lockout state under `data_dir`.
    pub fn open(data_dir: &Path, admin_user: &str, admin_pass: &str) -> io::Result<Self> {
        Self::open_with_policy(data_dir, admin_user, admin_pass, false)
    }

    /// Same as `open`, but a freshly seeded bootstrap admin is created with
    /// `must_change = true` when requested (see `UserStore::open_with_policy`).
    pub fn open_with_policy(
        data_dir: &Path,
        admin_user: &str,
        admin_pass: &str,
        bootstrap_must_change: bool,
    ) -> io::Result<Self> {
        let users = UserStore::open_with_policy(
            &crate::auth::user_store_path(data_dir),
            admin_user,
            admin_pass,
            bootstrap_must_change,
        )
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let state_path = data_dir.join("security_state.json");
        Ok(Self::with_store(Arc::new(users), Some(state_path)))
    }

    pub fn with_store(users: Arc<UserStore>, state_path: Option<PathBuf>) -> Self {
        let failed_attempts = state_path
            .as_ref()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| {
                serde_json::from_slice::<HashMap<String, PersistedAttempt>>(&bytes).ok()
            })
            .unwrap_or_default();
        Self {
            users,
            failed_attempts: Mutex::new(failed_attempts),
            state_path,
            global_failures: Mutex::new(VecDeque::new()),
            auth_gate: Mutex::new(()),
            state_write: Mutex::new(()),
            lockout_duration: Duration::from_secs(60),
            max_failed_attempts: 5,
        }
    }

    pub fn verify_credentials(
        &self,
        ip: &str,
        user: &str,
        pass: &str,
    ) -> Result<AuthenticatedUser, AuthError> {
        {
            let mut global = self.global_failures.lock();
            let now = Instant::now();
            while global
                .front()
                .is_some_and(|t| now.duration_since(*t) > GLOBAL_FAILURE_WINDOW)
            {
                global.pop_front();
            }
            if global.len() >= GLOBAL_FAILURE_LIMIT {
                return Err(AuthError::Locked(
                    "authentication temporarily throttled; retry in a minute".into(),
                ));
            }
        }

        let ip_key = format!("ip:{ip}");
        let user_key = format!("user:{}", user.to_ascii_lowercase());
        if let Some(msg) = self
            .lock_message(&ip_key)
            .or_else(|| self.lock_message(&user_key))
        {
            return Err(AuthError::Locked(msg));
        }

        let verified = {
            let _gate = self.auth_gate.lock();
            self.users.verify(user, pass)
        };
        match verified {
            Some(view) => {
                {
                    let mut attempts = self.failed_attempts.lock();
                    attempts.remove(&ip_key);
                    attempts.remove(&user_key);
                }
                self.save_state();
                Ok(AuthenticatedUser {
                    username: view.username,
                    role: view.role,
                    allowed_tables: view.allowed_tables,
                    must_change: view.must_change,
                })
            }
            None => {
                self.record_failure(&ip_key);
                self.record_failure(&user_key);
                self.global_failures.lock().push_back(Instant::now());
                if self.lock_message(&ip_key).is_some() || self.lock_message(&user_key).is_some() {
                    Err(AuthError::Locked(
                        "too many failed attempts; account/IP temporarily locked".into(),
                    ))
                } else {
                    Err(AuthError::InvalidCredentials)
                }
            }
        }
    }

    /// Resolve an identity already authenticated by a trusted reverse proxy.
    /// No password or lockout accounting is involved: the proxy is the
    /// authentication boundary and must strip client-supplied headers.
    pub fn verify_trusted_identity(&self, username: &str) -> Result<AuthenticatedUser, AuthError> {
        let view = self
            .users
            .identity(username)
            .ok_or(AuthError::InvalidCredentials)?;
        Ok(AuthenticatedUser {
            username: view.username,
            role: view.role,
            allowed_tables: view.allowed_tables,
            must_change: false,
        })
    }

    fn lock_message(&self, key: &str) -> Option<String> {
        let mut attempts = self.failed_attempts.lock();
        let entry = attempts.get(key)?;
        if entry.failures < self.max_failed_attempts {
            return None;
        }
        let now = unix_now();
        let elapsed = now.saturating_sub(entry.last_unix);
        if elapsed < self.lockout_duration.as_secs() {
            return Some(format!(
                "temporarily locked after {} failed attempts; retry in {}s",
                entry.failures,
                self.lockout_duration.as_secs().saturating_sub(elapsed)
            ));
        }
        attempts.remove(key);
        None
    }

    fn record_failure(&self, key: &str) {
        let now = unix_now();
        let mut attempts = self.failed_attempts.lock();
        if attempts.len() > 20_000 {
            attempts.retain(|_, entry| now.saturating_sub(entry.last_unix) < 86_400);
        }
        let entry = attempts.entry(key.to_string()).or_insert(PersistedAttempt {
            failures: 0,
            last_unix: now,
        });
        entry.failures = entry.failures.saturating_add(1);
        entry.last_unix = now;
        drop(attempts);
        self.save_state();
    }

    fn save_state(&self) {
        let Some(path) = &self.state_path else {
            return;
        };
        let _write_guard = self.state_write.lock();
        let state = self.failed_attempts.lock().clone();
        let Ok(payload) = serde_json::to_vec(&state) else {
            return;
        };
        let tmp = path.with_extension(format!(
            "json.tmp.{}.{}.{}",
            std::process::id(),
            TEMP_FILE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        if std::fs::write(&tmp, payload).is_err() {
            return;
        }
        let _ = std::fs::rename(&tmp, path);
    }

    pub fn list_users(&self) -> Vec<UserView> {
        self.users.list()
    }

    pub fn user_is_admin(&self, username: &str) -> bool {
        self.users.is_admin(username)
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
