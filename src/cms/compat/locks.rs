// SPDX-License-Identifier: AGPL-3.0-only
//! Row-write coordination: SELECT … FOR UPDATE row locks, write-target
//! probes and MySQL lock-wait error mapping.

use std::collections::BTreeMap;
use std::time::Duration;

use super::*;
use crate::cms::errno;
use crate::cms::row_locks::RowLockWait;
use crate::relational::Value;

impl CmsSqlGate {
    /// Row-lock wait budget of the session (`innodb_lock_wait_timeout`);
    /// `NOWAIT` means zero — a held row errors immediately.
    pub(crate) fn lock_timeout_of(&self, session: u64, nowait: bool) -> Duration {
        if nowait {
            return Duration::ZERO;
        }
        let secs = self
            .with_session(session, |s| s.lock_wait_timeout)
            .unwrap_or(50.0);
        Duration::from_secs_f64(secs.clamp(0.0, 3600.0))
    }

    /// Locks the rows a `SELECT … FOR UPDATE` returned. Resolves the PK of
    /// the main table from the overlay metadata — from the result rows when
    /// the projection carries it (`SELECT *`, Action Scheduler), otherwise
    /// by re-querying the same FROM/WHERE for `SELECT <pk>`. Returns the
    /// number of rows locked; with `skip_locked`, foreign-held rows are
    /// dropped from the result instead of waited on (queue semantics).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn lock_select_rows(
        &self,
        session: u64,
        sql: &str,
        tail_pos: usize,
        nowait: bool,
        skip_locked: bool,
        rows: &mut Vec<BTreeMap<String, Value>>,
    ) -> Result<usize, CmsError> {
        if rows.is_empty() {
            return Ok(0);
        }
        let Some(from_pos) = top_level_find_ci(sql, "FROM") else {
            return Ok(0);
        };
        if from_pos >= tail_pos {
            return Ok(0);
        }
        let tail = &sql[from_pos..tail_pos];
        // Aggregates change the row identity — InnoDB locks the scanned
        // rows; we conservatively skip (documented divergence).
        if top_level_find_ci(tail, "GROUP").is_some() {
            return Ok(0);
        }
        let Some(table) = first_table_ident(&tail[4..]) else {
            return Ok(0);
        };
        let Some(pk) = self
            .table_meta(&table)
            .and_then(|m| m.primary_key.first().cloned())
        else {
            return Ok(0);
        };
        let in_tx = self.tx_active(session);
        // SKIP LOCKED never waits: try once, drop on foreign hold.
        let timeout = self.lock_timeout_of(session, nowait || skip_locked);
        // Fast path: the projection already carries the PK.
        let projected = rows[0].contains_key(&pk);
        let keys: Vec<String> = if projected {
            rows.iter()
                .filter_map(|r| r.get(&pk).and_then(value_key))
                .collect()
        } else {
            let key_sql = format!("SELECT {} {}", pk, tail);
            match self.run_with_tx(session, &key_sql) {
                Ok(r) => r
                    .rows
                    .iter()
                    .filter_map(|row| row.get(&pk).and_then(value_key))
                    .collect(),
                // Best-effort: a key query the engine cannot parse must
                // never fail the client statement.
                Err(_) => return Ok(0),
            }
        };
        let mut locked = 0usize;
        let mut skipped: Vec<usize> = Vec::new();
        for (idx, key) in keys.iter().enumerate() {
            if skip_locked && self.row_locks.probe(&table, key, session).is_some() {
                skipped.push(idx);
                continue;
            }
            match self.row_locks.acquire(&table, key, session, timeout) {
                Ok(_) => {
                    if in_tx {
                        locked += 1;
                    }
                }
                Err(w) => {
                    if skip_locked {
                        skipped.push(idx);
                        continue;
                    }
                    if !in_tx {
                        self.row_locks.release_session(session);
                    }
                    return Err(row_lock_error(w, nowait));
                }
            }
        }
        if !in_tx {
            // Autocommit: the statement's locks end with the statement.
            self.row_locks.release_session(session);
        }
        // SKIP LOCKED with a projected PK: excluded rows leave the result
        // (InnoDB returns only the rows it actually locked).
        if projected && !skipped.is_empty() {
            let mut i = 0usize;
            rows.retain(|_| {
                let keep = !skipped.contains(&i);
                i += 1;
                keep
            });
        }
        Ok(locked)
    }

    /// Textual probe plan for an UPDATE/DELETE: (main table, `SELECT <pk>`
    /// query over the same WHERE). `None` when the target set cannot be
    /// derived (no WHERE, multi-table DELETE, unknown table, subquery FROM).
    pub(crate) fn write_target_probe(&self, sql: &str) -> Option<(String, String)> {
        let up = sql.to_ascii_uppercase();
        if up.starts_with("UPDATE") {
            let set_pos = top_level_find_ci(sql, "SET")?;
            let table = first_table_ident(&sql[6..set_pos])?;
            let pk = self
                .table_meta(&table)
                .and_then(|m| m.primary_key.first().cloned())?;
            let where_pos = top_level_find_ci(sql, "WHERE")?;
            Some((
                table.clone(),
                format!("SELECT {} FROM {} {}", pk, table, &sql[where_pos..]),
            ))
        } else if up.starts_with("DELETE ") {
            let from_pos = top_level_find_ci(sql, "FROM")?;
            // Multi-table DELETE (`DELETE a, b FROM …`) — not targeted here.
            if sql[..from_pos].contains(',') {
                return None;
            }
            let table = first_table_ident(&sql[from_pos + 4..])?;
            let pk = self
                .table_meta(&table)
                .and_then(|m| m.primary_key.first().cloned())?;
            Some((table, format!("SELECT {} {}", pk, &sql[from_pos..])))
        } else {
            None
        }
    }

    /// InnoDB write-write conflict: while an open transaction holds a
    /// target row, the DML waits. Inside a transaction the DML *takes* the
    /// locks (X-locks held until COMMIT) so opposite-order updates deadlock
    /// with errno 1213 exactly like InnoDB.
    pub(crate) fn wait_write_targets(
        &self,
        session: u64,
        table: &str,
        key_sql: &str,
    ) -> Result<(), CmsError> {
        let rows = match self.run_with_tx(session, key_sql) {
            Ok(r) => r.rows,
            Err(_) => return Ok(()),
        };
        let Some(pk) = self
            .table_meta(table)
            .and_then(|m| m.primary_key.first().cloned())
        else {
            return Ok(());
        };
        let timeout = self.lock_timeout_of(session, false);
        let in_tx = self.tx_active(session);
        for row in &rows {
            let Some(v) = row.get(&pk) else { continue };
            let Some(key) = value_key(v) else { continue };
            let res = if in_tx {
                self.row_locks
                    .acquire(table, &key, session, timeout)
                    .map(|_| ())
            } else {
                self.row_locks
                    .wait_until_free(table, &key, session, timeout)
            };
            res.map_err(|w| row_lock_error(w, false))?;
        }
        Ok(())
    }
}

/// Maps a lock-wait outcome to the MySQL server error CMS code branches on:
/// deadlock → 1213 (SQLSTATE 40001), NOWAIT → 3572, wait expiry → 1205.
pub(crate) fn row_lock_error(wait: RowLockWait, nowait: bool) -> CmsError {
    match wait {
        RowLockWait::Deadlock { chain } => CmsError {
            errno: errno::ER_LOCK_DEADLOCK,
            message: format!(
                "Deadlock found when trying to get lock; try restarting transaction (session chain {:?})",
                chain
            ),
        },
        RowLockWait::Timeout { holder } if nowait => CmsError {
            errno: errno::ER_LOCK_NOWAIT,
            message: format!(
                "Statement aborted because lock(s) could not be acquired immediately and NOWAIT is set (held by session {})",
                holder
            ),
        },
        RowLockWait::Timeout { holder } => CmsError {
            errno: errno::ER_LOCK_WAIT_TIMEOUT,
            message: format!(
                "Lock wait timeout exceeded; try restarting transaction (row held by session {})",
                holder
            ),
        },
    }
}
