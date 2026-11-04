// SPDX-License-Identifier: AGPL-3.0-only
//! Row locks: SELECT ... FOR UPDATE, deadlock 1213, SKIP LOCKED.

use super::*;

// ---------------- row locks: SELECT … FOR UPDATE (§4 item 2) ----------------

/// Small stock table for lock scenarios (Woo `_stock` meta shape).
fn stock_gate(name: &str) -> CmsSqlGate {
    let g = gate(name);
    g.execute(
            g.open_session(),
            "CREATE TABLE wp_stock_meta (meta_id bigint(20) unsigned NOT NULL auto_increment, post_id bigint(20) unsigned NOT NULL default 0, meta_key varchar(255) NOT NULL default '', meta_value longtext, PRIMARY KEY (meta_id))",
        )
        .unwrap();
    g
}

#[test]
fn for_update_blocks_second_session_until_commit() {
    let g = stock_gate("fu_block");
    let s1 = g.open_session();
    let s2 = g.open_session();
    g.execute(
        s1,
        "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (7, '_stock', '10')",
    )
    .unwrap();

    // s1: read-and-lock inside a transaction.
    g.execute(s1, "BEGIN").unwrap();
    let r = g
            .execute(s1, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 7 AND meta_key = '_stock' FOR UPDATE")
            .unwrap();
    assert_eq!(v(&r.rows[0], "meta_value"), "10");
    assert_eq!(g.row_locks.held_len(), 1, "one row lock held inside tx");

    // s2: the same FOR UPDATE waits → 1205 with a tiny session timeout.
    g.execute(s2, "SET innodb_lock_wait_timeout = 0.15")
        .unwrap();
    g.execute(s2, "BEGIN").unwrap();
    let err = g
            .execute(s2, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 7 AND meta_key = '_stock' FOR UPDATE")
            .unwrap_err();
    assert_eq!(
        err.errno,
        errno::ER_LOCK_WAIT_TIMEOUT,
        "1205 expected, got {:?}",
        err
    );
    // s2's tx survived the failed statement (statement-level rollback).
    assert!(g.tx_active(s2));

    // NOWAIT on a held row → 3572 immediately.
    g.execute(s1, "SET innodb_lock_wait_timeout = 5").unwrap();
    let err = g
            .execute(s2, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 7 AND meta_key = '_stock' FOR UPDATE NOWAIT")
            .unwrap_err();
    assert_eq!(
        err.errno,
        errno::ER_LOCK_NOWAIT,
        "3572 expected, got {:?}",
        err
    );

    // COMMIT releases; the waiter then succeeds (its tx still open).
    g.execute(s1, "COMMIT").unwrap();
    assert_eq!(g.row_locks.held_len(), 0, "commit releases row locks");
    let r = g
            .execute(s2, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 7 AND meta_key = '_stock' FOR UPDATE")
            .unwrap();
    assert_eq!(v(&r.rows[0], "meta_value"), "10");
    g.execute(s2, "ROLLBACK").unwrap();
    assert_eq!(g.row_locks.held_len(), 0);
}

#[test]
fn for_update_reentrant_same_session_and_row() {
    let g = stock_gate("fu_reentrant");
    let s = g.open_session();
    g.execute(
        s,
        "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (1, '_stock', '5')",
    )
    .unwrap();
    g.execute(s, "BEGIN").unwrap();
    g.execute(
        s,
        "SELECT meta_value FROM wp_stock_meta WHERE post_id = 1 FOR UPDATE",
    )
    .unwrap();
    // Same transaction locks the row again — no self-deadlock.
    let r = g.execute(s, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 1 AND meta_key = '_stock' FOR UPDATE").unwrap();
    assert_eq!(v(&r.rows[0], "meta_value"), "5");
    g.execute(s, "COMMIT").unwrap();
    assert_eq!(g.row_locks.held_len(), 0);
}

#[test]
fn opposite_order_updates_deadlock_1213() {
    // InnoDB pattern: T1 locks A then wants B; T2 locks B then wants A →
    // one of them gets 1213 and WooCommerce retries.
    let g = stock_gate("fu_deadlock");
    let s1 = g.open_session();
    let s2 = g.open_session();
    g.execute(
        s1,
        "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (100, '_stock', '1')",
    )
    .unwrap();
    g.execute(
        s1,
        "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (200, '_stock', '2')",
    )
    .unwrap();

    g.execute(s1, "SET innodb_lock_wait_timeout = 3").unwrap();
    g.execute(s2, "SET innodb_lock_wait_timeout = 3").unwrap();

    // T1 holds row A (post_id=100); T2 holds row B (post_id=200).
    g.execute(s1, "BEGIN").unwrap();
    g.execute(
        s1,
        "SELECT meta_value FROM wp_stock_meta WHERE post_id = 100 FOR UPDATE",
    )
    .unwrap();
    g.execute(s2, "BEGIN").unwrap();
    g.execute(
        s2,
        "SELECT meta_value FROM wp_stock_meta WHERE post_id = 200 FOR UPDATE",
    )
    .unwrap();

    // T1 starts waiting for B on a background thread…
    let g2 = std::sync::Arc::new(g);
    let g3 = g2.clone();
    let t1 = std::thread::spawn(move || {
        g3.execute(
            s1,
            "SELECT meta_value FROM wp_stock_meta WHERE post_id = 200 FOR UPDATE",
        )
    });
    std::thread::sleep(std::time::Duration::from_millis(150));

    // …then T2 asks for A: cycle (s2→B→s1→A→s2) → 1213 immediately.
    let err = g2
        .execute(
            s2,
            "SELECT meta_value FROM wp_stock_meta WHERE post_id = 100 FOR UPDATE",
        )
        .unwrap_err();
    assert_eq!(
        err.errno,
        errno::ER_LOCK_DEADLOCK,
        "1213 expected, got {:?}",
        err
    );
    assert_eq!(err.sqlstate(), "40001");

    // Victim "rolled back its statement" (tx survives per MySQL); after
    // T2 rolls back fully, T1's wait completes.
    g2.execute(s2, "ROLLBACK").unwrap();
    let r = t1
        .join()
        .unwrap()
        .expect("T1 must acquire row B after T2 died");
    assert_eq!(v(&r.rows[0], "meta_value"), "2");
    g2.execute(s1, "COMMIT").unwrap();
}

#[test]
fn autocommit_update_waits_for_held_row() {
    // Woo path: order A reduced stock in a tx (FOR UPDATE); order B's
    // autocommit UPDATE of the same row must wait, not stomp.
    let g = stock_gate("fu_autocommit");
    let s1 = g.open_session();
    let s2 = g.open_session();
    g.execute(
        s1,
        "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (77, '_stock', '10')",
    )
    .unwrap();

    g.execute(s1, "BEGIN").unwrap();
    g.execute(s1, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 77 AND meta_key = '_stock' FOR UPDATE").unwrap();

    // Autocommit UPDATE blocked by the open tx → 1205 (short timeout).
    g.execute(s2, "SET innodb_lock_wait_timeout = 0.15")
        .unwrap();
    let err = g
        .execute(
            s2,
            "UPDATE wp_stock_meta SET meta_value = '8' WHERE post_id = 77 AND meta_key = '_stock'",
        )
        .unwrap_err();
    assert_eq!(
        err.errno,
        errno::ER_LOCK_WAIT_TIMEOUT,
        "1205 expected, got {:?}",
        err
    );
    // The row still holds the old value.
    let r = g
        .execute(
            s1,
            "SELECT meta_value FROM wp_stock_meta WHERE post_id = 77",
        )
        .unwrap();
    assert_eq!(v(&r.rows[0], "meta_value"), "10");

    // Commit, then the UPDATE goes through.
    g.execute(s1, "COMMIT").unwrap();
    let r = g
        .execute(
            s2,
            "UPDATE wp_stock_meta SET meta_value = '8' WHERE post_id = 77 AND meta_key = '_stock'",
        )
        .unwrap();
    assert_eq!(r.affected_rows, 1);
    let r = g
        .execute(
            s2,
            "SELECT meta_value FROM wp_stock_meta WHERE post_id = 77",
        )
        .unwrap();
    assert_eq!(v(&r.rows[0], "meta_value"), "8");
}

#[test]
fn update_in_tx_takes_hold_until_commit() {
    let g = stock_gate("fu_upd_tx");
    let s1 = g.open_session();
    let s2 = g.open_session();
    g.execute(
        s1,
        "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (9, '_stock', '4')",
    )
    .unwrap();

    g.execute(s1, "BEGIN").unwrap();
    g.execute(
        s1,
        "UPDATE wp_stock_meta SET meta_value = '3' WHERE post_id = 9 AND meta_key = '_stock'",
    )
    .unwrap();
    assert!(g.row_locks.held_len() >= 1, "UPDATE in tx holds an X lock");

    g.execute(s2, "SET innodb_lock_wait_timeout = 0.15")
        .unwrap();
    g.execute(s2, "BEGIN").unwrap();
    let err = g
        .execute(
            s2,
            "UPDATE wp_stock_meta SET meta_value = '2' WHERE post_id = 9 AND meta_key = '_stock'",
        )
        .unwrap_err();
    assert_eq!(
        err.errno,
        errno::ER_LOCK_WAIT_TIMEOUT,
        "1205 expected, got {:?}",
        err
    );
    g.execute(s2, "ROLLBACK").unwrap();

    g.execute(s1, "COMMIT").unwrap();
    assert_eq!(g.row_locks.held_len(), 0);
    let r = g
        .execute(s2, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 9")
        .unwrap();
    assert_eq!(
        v(&r.rows[0], "meta_value"),
        "3",
        "first tx's write committed"
    );
}

#[test]
fn delete_blocked_by_held_row_and_locking_tail_skips_literals() {
    let g = stock_gate("fu_delete");
    let s1 = g.open_session();
    let s2 = g.open_session();
    g.execute(
        s1,
        "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (5, '_stock', '1')",
    )
    .unwrap();

    g.execute(s1, "BEGIN").unwrap();
    g.execute(
        s1,
        "SELECT meta_value FROM wp_stock_meta WHERE post_id = 5 FOR UPDATE",
    )
    .unwrap();

    // A literal 'for update' inside a string must NOT take the lock path.
    let r = g
        .execute(
            s2,
            "SELECT meta_value FROM wp_stock_meta WHERE meta_value = 'wait for update'",
        )
        .unwrap();
    assert_eq!(
        r.rows.len(),
        0,
        "literal containing 'for update' is not a locking tail"
    );

    g.execute(s2, "SET innodb_lock_wait_timeout = 0.15")
        .unwrap();
    let err = g
        .execute(s2, "DELETE FROM wp_stock_meta WHERE post_id = 5")
        .unwrap_err();
    assert_eq!(err.errno, errno::ER_LOCK_WAIT_TIMEOUT, "DELETE waits too");

    g.execute(s1, "COMMIT").unwrap();
    let r = g
        .execute(s2, "DELETE FROM wp_stock_meta WHERE post_id = 5")
        .unwrap();
    assert_eq!(r.affected_rows, 1);
}

#[test]
fn close_session_releases_row_locks() {
    let g = stock_gate("fu_close");
    let s1 = g.open_session();
    let s2 = g.open_session();
    g.execute(
        s1,
        "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (3, '_stock', '9')",
    )
    .unwrap();
    g.execute(s1, "BEGIN").unwrap();
    g.execute(
        s1,
        "SELECT meta_value FROM wp_stock_meta WHERE post_id = 3 FOR UPDATE",
    )
    .unwrap();
    assert_eq!(g.row_locks.held_len(), 1);
    // PHP-FPM worker dies mid-transaction → disconnect rolls the tx back.
    g.close_session(s1);
    assert_eq!(g.row_locks.held_len(), 0, "disconnect releases row locks");
    let r = g
        .execute(s2, "SELECT meta_value FROM wp_stock_meta WHERE post_id = 3")
        .unwrap();
    assert_eq!(v(&r.rows[0], "meta_value"), "9");
}

#[test]
fn skip_locked_claims_only_free_rows() {
    // Action Scheduler: two queue workers claim pending jobs with
    // FOR UPDATE SKIP LOCKED — each must get a DIFFERENT row.
    let g = stock_gate("fu_skip");
    let s1 = g.open_session();
    let s2 = g.open_session();
    g.execute(
        s1,
        "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (11, '_job', 'a')",
    )
    .unwrap();
    g.execute(
        s1,
        "INSERT INTO wp_stock_meta (post_id, meta_key, meta_value) VALUES (12, '_job', 'b')",
    )
    .unwrap();

    // Worker 1 claims job 11 (projection carries the PK → exact rows).
    g.execute(s1, "BEGIN").unwrap();
    let r = g
        .execute(
            s1,
            "SELECT meta_id, post_id FROM wp_stock_meta WHERE post_id = 11 FOR UPDATE",
        )
        .unwrap();
    assert_eq!(v(&r.rows[0], "post_id"), "11");

    // Worker 2 asks for both jobs, skipping held ones → only 12 left.
    g.execute(s2, "BEGIN").unwrap();
    let r = g
            .execute(s2, "SELECT meta_id, post_id FROM wp_stock_meta WHERE post_id IN (11, 12) ORDER BY meta_id FOR UPDATE SKIP LOCKED")
            .unwrap();
    assert_eq!(r.rows.len(), 1, "held job 11 excluded by SKIP LOCKED");
    assert_eq!(v(&r.rows[0], "post_id"), "12");

    // Plain FOR UPDATE on the same set still blocks (0.15s → 1205).
    let s3 = g.open_session();
    g.execute(s3, "SET innodb_lock_wait_timeout = 0.15")
        .unwrap();
    let err = g
            .execute(s3, "SELECT meta_id FROM wp_stock_meta WHERE post_id IN (11, 12) ORDER BY meta_id FOR UPDATE")
            .unwrap_err();
    assert_eq!(err.errno, errno::ER_LOCK_WAIT_TIMEOUT);

    g.execute(s1, "COMMIT").unwrap();
    g.execute(s2, "ROLLBACK").unwrap();
    assert_eq!(g.row_locks.held_len(), 0);
}
