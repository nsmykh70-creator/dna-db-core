// SPDX-License-Identifier: AGPL-3.0-only
//! Named advisory locks — the engine-side registry for `GET_LOCK()` /
//! `RELEASE_LOCK()` / `IS_FREE_LOCK()` / `IS_USED_LOCK()`.
//!
//! MySQL semantics being reproduced:
//! - one exclusive holder per name, *per server*, not per table;
//! - re-entrant within the same connection (second `GET_LOCK` on the same
//!   name from the same session succeeds immediately and does not nest
//!   releases — one `RELEASE_LOCK` frees it);
//! - `GET_LOCK(name, timeout)` returns 1 on success, 0 on timeout;
//! - `RELEASE_LOCK(name)` returns 1 if released, 0 if held by someone else,
//!   NULL if the lock does not exist (modelled as [`ReleaseResult`]);
//! - all locks of a session are freed when the connection closes
//!   ([`LockRegistry::release_all`]) — critical for PHP-FPM pools where
//!   connections churn constantly.
//!
//! WooCommerce stock handling and many cron/queue plugins gate their work
//! through these functions, so they must be real server-side locks, not
//! no-ops.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

#[derive(Debug, Clone)]
struct LockEntry {
    holder: u64,
    #[allow(dead_code)] // depth tracked for future refcount introspection
    depth: u32,
    acquired_at: Instant,
}

/// Result of `RELEASE_LOCK()` as MySQL reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseResult {
    /// 1 — the lock existed and was held (and freed) by this session.
    Released,
    /// 0 — the lock exists but belongs to another session.
    NotOwner,
    /// NULL — no such lock was ever acquired (or already freed).
    NotExists,
}

/// Registry of named advisory locks. Cheap to clone via `Arc`.
#[derive(Debug, Default)]
pub struct LockRegistry {
    inner: Mutex<HashMap<String, LockEntry>>,
}

impl LockRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// `GET_LOCK(name, timeout)` — try to acquire, waiting up to `timeout`
    /// if the name is held by another session. Re-entrant for the holder.
    /// Returns `true` (MySQL: 1) on success, `false` (0) on timeout.
    pub fn get_lock(&self, name: &str, session: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let mut map = self.inner.lock();
                match map.get_mut(name) {
                    // Free — take it.
                    None => {
                        map.insert(
                            name.to_string(),
                            LockEntry {
                                holder: session,
                                depth: 1,
                                acquired_at: Instant::now(),
                            },
                        );
                        return true;
                    }
                    // Held by us — re-entrant success (no nesting on release).
                    Some(entry) if entry.holder == session => return true,
                    // Held by someone else — fall through to the wait below.
                    Some(_) => {}
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            // Skeleton wait strategy: short polling sleep. The production
            // CMS compat layer replaces this with a condvar (or an async
            // waker in the wire server) so waiters wake on release.
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// `RELEASE_LOCK(name)` — see [`ReleaseResult`] for MySQL's 1/0/NULL.
    pub fn release_lock(&self, name: &str, session: u64) -> ReleaseResult {
        let mut map = self.inner.lock();
        match map.get(name) {
            None => ReleaseResult::NotExists,
            Some(entry) if entry.holder != session => ReleaseResult::NotOwner,
            Some(_) => {
                map.remove(name);
                ReleaseResult::Released
            }
        }
    }

    /// Frees every lock held by `session` (connection close / COM_QUIT).
    /// Returns the number of locks released.
    pub fn release_all(&self, session: u64) -> usize {
        let mut map = self.inner.lock();
        let before = map.len();
        map.retain(|_, entry| entry.holder != session);
        before - map.len()
    }

    /// `IS_FREE_LOCK(name)` — `true` (1) when nobody holds it.
    pub fn is_free_lock(&self, name: &str) -> bool {
        !self.inner.lock().contains_key(name)
    }

    /// `IS_USED_LOCK(name)` — holder session id, or `None` when free.
    pub fn is_used_lock(&self, name: &str) -> Option<u64> {
        self.inner.lock().get(name).map(|e| e.holder)
    }

    /// How long `name` has been held, for diagnostics / slow-lock reports.
    /// `None` when the lock is free. Exposed so the admin panel can later
    /// render a "who holds which lock, since when" view — something MySQL
    /// only offers via performance_schema digging.
    pub fn held_since(&self, name: &str) -> Option<Duration> {
        self.inner.lock().get(name).map(|e| e.acquired_at.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn acquire_reentrant_release() {
        let reg = LockRegistry::new();
        assert!(reg.get_lock("wc-stock", 10, Duration::ZERO));
        // Re-entrant for the same session.
        assert!(reg.get_lock("wc-stock", 10, Duration::ZERO));
        assert_eq!(reg.is_used_lock("wc-stock"), Some(10));
        assert!(!reg.is_free_lock("wc-stock"));

        // Single release is enough despite the re-entrant acquire.
        assert_eq!(reg.release_lock("wc-stock", 10), ReleaseResult::Released);
        assert!(reg.is_free_lock("wc-stock"));
        // NULL semantics: releasing a non-existent lock.
        assert_eq!(reg.release_lock("wc-stock", 10), ReleaseResult::NotExists);
    }

    #[test]
    fn other_session_times_out() {
        let reg = LockRegistry::new();
        assert!(reg.get_lock("cron-run", 1, Duration::ZERO));
        // Session 2 must time out (zero timeout → immediate 0).
        assert!(!reg.get_lock("cron-run", 2, Duration::ZERO));
        // And not steal it via release.
        assert_eq!(reg.release_lock("cron-run", 2), ReleaseResult::NotOwner);
        assert_eq!(reg.is_used_lock("cron-run"), Some(1));
    }

    #[test]
    fn waits_for_release_within_timeout() {
        let reg = Arc::new(LockRegistry::new());
        assert!(reg.get_lock("queue", 1, Duration::ZERO));

        let reg2 = Arc::clone(&reg);
        let waiter = std::thread::spawn(move || {
            // 2 s budget; the holder frees after 100 ms → must succeed.
            reg2.get_lock("queue", 2, Duration::from_secs(2))
        });
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(reg.release_lock("queue", 1), ReleaseResult::Released);
        assert!(waiter.join().unwrap(), "waiter should acquire after release");
        assert_eq!(reg.is_used_lock("queue"), Some(2));
    }

    #[test]
    fn release_all_frees_only_own_session() {
        let reg = LockRegistry::new();
        assert!(reg.get_lock("a", 1, Duration::ZERO));
        assert!(reg.get_lock("b", 1, Duration::ZERO));
        assert!(reg.get_lock("c", 2, Duration::ZERO));

        assert_eq!(reg.release_all(1), 2);
        assert!(reg.is_free_lock("a"));
        assert!(reg.is_free_lock("b"));
        assert_eq!(reg.is_used_lock("c"), Some(2));
    }

    #[test]
    fn held_since_reports_duration_or_none() {
        let reg = LockRegistry::new();
        assert_eq!(reg.held_since("diag"), None);
        assert!(reg.get_lock("diag", 7, Duration::ZERO));
        let d = reg.held_since("diag").expect("lock is held");
        // Held for a non-negative, near-zero interval.
        assert!(d < Duration::from_secs(1));
        reg.release_lock("diag", 7);
        assert_eq!(reg.held_since("diag"), None);
    }
}
