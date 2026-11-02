// SPDX-License-Identifier: AGPL-3.0-only
//! Row-level locks for `SELECT … FOR UPDATE` — CMS_EDITION_RU.md §4 item 2.
//!
//! InnoDB semantics the CMS edition must reproduce for WooCommerce stock
//! paths (`wc_reduce_stock_levels`, HPOS order writes under concurrency):
//!
//! * `SELECT … FOR UPDATE` inside a transaction takes an exclusive lock on
//!   the returned rows and holds it until COMMIT / ROLLBACK / disconnect;
//! * a second transaction touching a held row **waits** (blocking acquire);
//!   if the wait exceeds `innodb_lock_wait_timeout` → MySQL error **1205**;
//! * `FOR UPDATE NOWAIT` on a held row fails immediately with **3572**
//!   (`ER_LOCK_NOWAIT`), never waits;
//! * when two transactions wait on each other's rows, the cycle is detected
//!   through the waits-for graph and one waiter gets **1213** (deadlock,
//!   SQLSTATE `40001`) — WooCommerce's retry loop branches on exactly this;
//! * same-session re-acquisition is reentrant (InnoDB X-lock on the same
//!   row inside one transaction is a no-op);
//! * in autocommit mode a statement's locks live for that statement only,
//!   so `UPDATE` outside a transaction *waits while* a row is held by an
//!   open transaction, but does not register a hold of its own.
//!
//! Lock identity is `(table, primary-key-value)`: every CMS table has a
//! single-column engine PK (the overlay's first `PRIMARY KEY` column), and
//! the compat gate resolves row keys with a `SELECT <pk> …` probe against
//! the same WHERE the client sent.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Poll interval while waiting for a row lock (same cadence as the named
/// lock registry — good enough for PHP connection churn, no condvar soup).
const POLL_EVERY: Duration = Duration::from_millis(25);

/// One held row lock.
#[derive(Debug, Clone)]
struct RowLockEntry {
    holder: u64,
    since: Instant,
}

/// Why an `acquire` gave up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowLockWait {
    /// Cycle in the waits-for graph — MySQL errno 1213 (`ER_LOCK_DEADLOCK`,
    /// SQLSTATE 40001). `chain` is the holder path detector → victim.
    Deadlock { chain: Vec<u64> },
    /// Waited past the deadline — MySQL errno 1205
    /// (`ER_LOCK_WAIT_TIMEOUT`, SQLSTATE HY000).
    Timeout { holder: u64 },
}

#[derive(Default)]
pub struct RowLockRegistry {
    /// (table, row key) → holder. Table names are lowercased (MySQL schema
    /// names are case-insensitive on the usual platforms).
    held: Mutex<HashMap<(String, String), RowLockEntry>>,
    /// session → keys this session is *currently waiting on* (waits-for
    /// graph edges; only present while a blocking acquire is in flight).
    waits: Mutex<HashMap<u64, HashSet<(String, String)>>>,
}

impl RowLockRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Blocking acquire of an exclusive row lock.
    ///
    /// Returns `Ok(true)` when the lock was newly taken, `Ok(false)` when it
    /// was already held by this session (reentrant no-op). On conflict the
    /// call blocks up to `timeout` (deadlock → immediate `RowLockWait::Deadlock`,
    /// expiry → `RowLockWait::Timeout`); a zero timeout acts as NOWAIT.
    pub fn acquire(
        &self,
        table: &str,
        key: &str,
        session: u64,
        timeout: Duration,
    ) -> Result<bool, RowLockWait> {
        let id = (table.to_ascii_lowercase(), key.to_string());
        // Fast path: free or already ours.
        {
            let held = self.held.lock().expect("row locks poisoned");
            match held.get(&id) {
                None => {
                    drop(held);
                    self.held.lock().expect("row locks poisoned").insert(
                        id.clone(),
                        RowLockEntry { holder: session, since: Instant::now() },
                    );
                    return Ok(true);
                }
                Some(e) if e.holder == session => return Ok(false),
                Some(_) => {}
            }
        }
        // Contended: register the wait edge, then poll with deadlock checks.
        self.waits
            .lock()
            .expect("row waits poisoned")
            .entry(session)
            .or_default()
            .insert(id.clone());
        let deadline = Instant::now() + timeout;
        let res = loop {
            {
                let held = self.held.lock().expect("row locks poisoned");
                match held.get(&id) {
                    None => {
                        drop(held);
                        self.held.lock().expect("row locks poisoned").insert(
                            id.clone(),
                            RowLockEntry { holder: session, since: Instant::now() },
                        );
                        break Ok(true);
                    }
                    Some(e) if e.holder == session => break Ok(false),
                    Some(e) => {
                        if self.detects_deadlock(session, &held) {
                            let chain = self.wait_chain(session, &id, &held);
                            break Err(RowLockWait::Deadlock { chain });
                        }
                        if Instant::now() >= deadline {
                            break Err(RowLockWait::Timeout { holder: e.holder });
                        }
                    }
                }
            }
            std::thread::sleep(POLL_EVERY);
        };
        self.waits
            .lock()
            .expect("row waits poisoned")
            .entry(session)
            .or_default()
            .remove(&id);
        res
    }

    /// Non-blocking probe: `Some(holder)` when another session holds the row.
    pub fn probe(&self, table: &str, key: &str, session: u64) -> Option<u64> {
        let id = (table.to_ascii_lowercase(), key.to_string());
        let held = self.held.lock().expect("row locks poisoned");
        held.get(&id)
            .filter(|e| e.holder != session)
            .map(|e| e.holder)
    }

    /// Blocks (polling) until `key` is free of *foreign* holds or `timeout`
    /// expires. Autocommit DML uses this: it must serialize against an open
    /// transaction's `FOR UPDATE` hold, but registers no hold of its own.
    pub fn wait_until_free(
        &self,
        table: &str,
        key: &str,
        session: u64,
        timeout: Duration,
    ) -> Result<(), RowLockWait> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.probe(table, key, session) {
                None => return Ok(()),
                Some(holder) => {
                    if Instant::now() >= deadline {
                        return Err(RowLockWait::Timeout { holder });
                    }
                }
            }
            std::thread::sleep(POLL_EVERY);
        }
    }

    /// Releases every row lock held by `session` (COMMIT / ROLLBACK /
    /// disconnect). Returns the number of locks freed.
    pub fn release_session(&self, session: u64) -> usize {
        let mut held = self.held.lock().expect("row locks poisoned");
        let before = held.len();
        held.retain(|_, e| e.holder != session);
        before - held.len()
    }

    /// Number of currently held row locks (diagnostics / tests).
    pub fn held_len(&self) -> usize {
        self.held.lock().expect("row locks poisoned").len()
    }

    /// How long the row has been held (diagnostics — `SHOW PROCESSLIST`
    /// analogue; the named-lock registry exposes the same for GET_LOCK).
    pub fn held_since(&self, table: &str, key: &str) -> Option<Duration> {
        let id = (table.to_ascii_lowercase(), key.to_string());
        self.held
            .lock()
            .expect("row locks poisoned")
            .get(&id)
            .map(|e| e.since.elapsed())
    }

    /// True when waiting for `waiter`'s wanted keys would close a cycle:
    /// following (waiter → key → holder → keys they wait on …) reaches
    /// `waiter` again.
    fn detects_deadlock(
        &self,
        waiter: u64,
        held: &HashMap<(String, String), RowLockEntry>,
    ) -> bool {
        let waits = self.waits.lock().expect("row waits poisoned");
        // DFS over sessions: from every holder of the waiter's wanted keys.
        let mut stack: Vec<u64> = vec![waiter];
        let mut seen: HashSet<u64> = HashSet::new();
        while let Some(s) = stack.pop() {
            if !seen.insert(s) {
                continue;
            }
            // Keys s waits for → their holders get pushed.
            if let Some(keys) = waits.get(&s) {
                for k in keys {
                    if let Some(e) = held.get(k) {
                        if e.holder == waiter {
                            return true;
                        }
                        stack.push(e.holder);
                    }
                }
            }
        }
        false
    }

    /// Holder path waiter → … (diagnostics for the 1213 message; MySQL
    /// reports just the two sessions). `wanted` is the key `waiter` asked
    /// for; `held` is the already-locked lock map (no re-lock — the caller
    /// holds the mutex, and `Mutex` is not reentrant).
    fn wait_chain(
        &self,
        waiter: u64,
        wanted: &(String, String),
        held: &HashMap<(String, String), RowLockEntry>,
    ) -> Vec<u64> {
        let waits = self.waits.lock().expect("row waits poisoned");
        let mut chain = vec![waiter];
        let mut cur = waiter;
        for _ in 0..16 {
            let next = waits
                .get(&cur)
                .and_then(|keys| {
                    keys.iter()
                        .find(|k| *k == wanted || held.contains_key(*k))
                })
                .and_then(|k| held.get(k))
                .map(|e| e.holder);
            match next {
                Some(h) if h != waiter => {
                    chain.push(h);
                    cur = h;
                }
                _ => break,
            }
        }
        chain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_release_reentrant() {
        let r = RowLockRegistry::new();
        assert_eq!(r.acquire("wp_options", "1", 7, Duration::from_millis(10)).unwrap(), true);
        // Reentrant: same session, same row → no-op.
        assert_eq!(r.acquire("wp_options", "1", 7, Duration::from_millis(10)).unwrap(), false);
        assert_eq!(r.held_len(), 1);
        assert_eq!(r.release_session(7), 1);
        assert_eq!(r.held_len(), 0);
        // Free again.
        assert_eq!(r.acquire("wp_options", "1", 8, Duration::from_millis(10)).unwrap(), true);
    }

    #[test]
    fn timeout_when_held_too_long() {
        let r = RowLockRegistry::new();
        r.acquire("t", "k", 1, Duration::from_millis(5)).unwrap();
        let err = r
            .acquire("t", "k", 2, Duration::from_millis(80))
            .unwrap_err();
        assert_eq!(err, RowLockWait::Timeout { holder: 1 });
        // Holder untouched by the failed waiter.
        assert_eq!(r.held_len(), 1);
        r.release_session(1);
        assert_eq!(r.held_len(), 0);
    }

    #[test]
    fn zero_timeout_is_nowait() {
        let r = RowLockRegistry::new();
        r.acquire("t", "k", 1, Duration::from_millis(5)).unwrap();
        let err = r.acquire("t", "k", 2, Duration::ZERO).unwrap_err();
        assert_eq!(err, RowLockWait::Timeout { holder: 1 });
    }

    #[test]
    fn deadlock_cycle_detected_immediately() {
        let r = std::sync::Arc::new(RowLockRegistry::new());
        // s1 holds A; s2 holds B.
        r.acquire("t", "a", 1, Duration::from_millis(5)).unwrap();
        r.acquire("t", "b", 2, Duration::from_millis(5)).unwrap();
        // s1 starts waiting for B (thread, so the main flow proceeds).
        let r3 = r.clone();
        let waiter = std::thread::spawn(move || {
            r3.acquire("t", "b", 1, Duration::from_millis(2_000))
        });
        // Give the waiter time to register its wait edge.
        std::thread::sleep(Duration::from_millis(150));
        // s2 asks for A → cycle (2 → B → s1 → A → s2) → 1213 immediately.
        let start = Instant::now();
        let err = r.acquire("t", "a", 2, Duration::from_millis(2_000)).unwrap_err();
        assert!(
            matches!(err, RowLockWait::Deadlock { .. }),
            "expected deadlock, got {err:?}"
        );
        assert!(start.elapsed() < Duration::from_millis(1_000), "must fail fast");
        // The victim (s2) "dies" — InnoDB rolls it back, releasing B; the
        // innocent waiter then completes.
        r.release_session(2);
        let got = waiter.join().unwrap();
        assert!(got.is_ok(), "waiter should have acquired B after victim died");
    }

    #[test]
    fn wait_until_free_respects_foreign_only() {
        let r = RowLockRegistry::new();
        r.acquire("t", "k", 1, Duration::from_millis(5)).unwrap();
        // Own hold is not foreign — probe returns None for the holder.
        assert_eq!(r.probe("t", "k", 1), None);
        assert_eq!(r.probe("t", "k", 2), Some(1));
        assert!(r.wait_until_free("t", "k", 1, Duration::from_millis(10)).is_ok());
        let err = r.wait_until_free("t", "k", 2, Duration::from_millis(60)).unwrap_err();
        assert_eq!(err, RowLockWait::Timeout { holder: 1 });
    }

    #[test]
    fn release_session_frees_only_own() {
        let r = RowLockRegistry::new();
        r.acquire("t", "a", 1, Duration::from_millis(5)).unwrap();
        r.acquire("t", "b", 1, Duration::from_millis(5)).unwrap();
        r.acquire("t", "c", 2, Duration::from_millis(5)).unwrap();
        assert_eq!(r.release_session(1), 2);
        assert_eq!(r.held_len(), 1);
        assert_eq!(r.release_session(1), 0, "second release is a no-op");
    }
}
