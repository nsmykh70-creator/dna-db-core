// SPDX-License-Identifier: AGPL-3.0-only
//! Per-table AUTO_INCREMENT sequences for the CMS edition.
//!
//! MySQL semantics being reproduced:
//! - the next generated id is one above the highest value ever generated or
//!   explicitly inserted for the table (monotonic, never re-used while the
//!   process lives);
//! - an explicit INSERT with a higher id moves the counter forward
//!   (`bump_to`), so later generated ids never collide;
//! - after a restart MySQL 8.0 re-derives the counter from the table; we do
//!   the same by lazily seeding from `MAX(pk)` on first use (the `seed`
//!   closure is supplied by the caller and runs at most once per table).
//!
//! Restart caveat (documented deviation): ids freed by DELETE of the max row
//! can be re-issued after a restart, exactly like MySQL 5.7's in-memory
//! counter (MySQL 8.0 persists the counter; see CMS_EDITION_RU.md §4).

use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, Default)]
pub struct AutoIncrementRegistry {
    inner: Mutex<HashMap<String, u64>>,
}

impl AutoIncrementRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Next generated value for `table`, seeding from `seed()` when the
    /// table is seen for the first time in this process. `seed` must return
    /// the current MAX(pk) (0 for an empty table).
    pub fn next(&self, table: &str, seed: impl FnOnce() -> Result<u64, String>) -> Result<u64, String> {
        let mut map = self.inner.lock().expect("sequence registry poisoned");
        let slot = match map.get_mut(table) {
            Some(v) => v,
            None => {
                let s = seed()?;
                map.insert(table.to_string(), s);
                map.get_mut(table).expect("just inserted")
            }
        };
        *slot += 1;
        Ok(*slot)
    }

    /// An explicit INSERT with id `v` happened: keep the counter above it.
    pub fn bump_to(&self, table: &str, v: u64) {
        let mut map = self.inner.lock().expect("sequence registry poisoned");
        let slot = map.entry(table.to_string()).or_insert(0);
        if v > *slot {
            *slot = v;
        }
    }

    /// Current counter value without generating (`SHOW TABLE STATUS` /
    /// diagnostics). Seeds lazily the same way as [`Self::next`].
    pub fn current(&self, table: &str, seed: impl FnOnce() -> Result<u64, String>) -> Result<u64, String> {
        let mut map = self.inner.lock().expect("sequence registry poisoned");
        if let Some(v) = map.get(table) {
            return Ok(*v);
        }
        let s = seed()?;
        map.insert(table.to_string(), s);
        Ok(s)
    }

    /// Forget the counter (TRUNCATE / DROP TABLE bookkeeping).
    pub fn reset(&self, table: &str) {
        self.inner.lock().expect("sequence registry poisoned").remove(table);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn seeds_once_then_monotonic() {
        let reg = AutoIncrementRegistry::new();
        let calls = AtomicUsize::new(0);
        let seed = || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(10u64) // MAX(pk) = 10 in storage
        };
        assert_eq!(reg.next("wp_posts", seed).unwrap(), 11);
        assert_eq!(reg.next("wp_posts", seed).unwrap(), 12);
        assert_eq!(reg.next("wp_posts", seed).unwrap(), 13);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "seed must run exactly once");
        // Current does not advance.
        assert_eq!(reg.current("wp_posts", seed).unwrap(), 13);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn explicit_higher_id_bumps_counter() {
        let reg = AutoIncrementRegistry::new();
        assert_eq!(reg.next("t", || Ok(0)).unwrap(), 1);
        // INSERT with explicit id = 100.
        reg.bump_to("t", 100);
        assert_eq!(reg.next("t", || Ok(0)).unwrap(), 101);
        // A lower explicit id does not rewind the counter.
        reg.bump_to("t", 50);
        assert_eq!(reg.next("t", || Ok(0)).unwrap(), 102);
    }

    #[test]
    fn tables_are_independent_and_resettable() {
        let reg = AutoIncrementRegistry::new();
        assert_eq!(reg.next("a", || Ok(5)).unwrap(), 6);
        assert_eq!(reg.next("b", || Ok(0)).unwrap(), 1);
        reg.reset("a");
        assert_eq!(reg.next("a", || Ok(0)).unwrap(), 1);
    }

    #[test]
    fn seed_error_propagates() {
        let reg = AutoIncrementRegistry::new();
        let err = reg.next("t", || Err("storage boom".into())).unwrap_err();
        assert_eq!(err, "storage boom");
    }
}
