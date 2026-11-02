#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Real-client smoke test for the DNA-DB MySQL wire listener.

Runs two independent MySQL client libraries against `dna-db-core mysqld`:
  * PyMySQL  — pure-Python client (text protocol, like wpdb/mysqli)
  * Connector/Py — Oracle's official client (uses COM_STMT_PREPARE/EXECUTE
    server-side prepared statements whenever params are supplied)

Every check mirrors what a real CMS (WordPress/WooCommerce) does on the wire.
Exit code 0 = all checks green.
"""
import sys

RESULTS = []


def check(name, cond, extra=""):
    RESULTS.append((name, bool(cond), extra))
    print(("PASS" if cond else "FAIL") + f"  {name}" + (f"  [{extra}]" if extra and not cond else ""))


def smoke_pymysql(port):
    import pymysql
    conn = pymysql.connect(
        host="127.0.0.1", port=port, user="wp", password="wp_pass",
        database="wordpress", charset="utf8mb4",
    )
    with conn.cursor() as cur:
        cur.execute("SELECT 1")
        check("pymysql: SELECT 1", cur.fetchone() == (1,))
        cur.execute("SELECT VERSION()")
        v = cur.fetchone()[0]
        check("pymysql: VERSION()=10.11.x-MariaDB", v.startswith("10.11.11-MariaDB"), v)
        cur.execute("SHOW TABLES LIKE 'wp_smoke'")
        have = cur.fetchall()
        if have:
            cur.execute("DROP TABLE wp_smoke")
            conn.commit()
        cur.execute(
            "CREATE TABLE wp_smoke (ID BIGINT UNSIGNED NOT NULL AUTO_INCREMENT, "
            "post_title VARCHAR(191) NOT NULL DEFAULT '', post_status VARCHAR(20) NOT NULL DEFAULT 'publish', "
            "PRIMARY KEY (ID), KEY post_status (post_status))")
        conn.commit()
        cur.execute(
            "INSERT INTO wp_smoke (post_title, post_status) VALUES (%s, %s), (%s, %s)",
            ("Hello DNA", "publish", "Draft post", "draft"))
        check("pymysql: multi-row INSERT affected=2", cur.rowcount == 2, cur.rowcount)
        cur.execute("SELECT LAST_INSERT_ID()")
        check("pymysql: LAST_INSERT_ID()=1", cur.fetchone()[0] == 1)
        cur.execute("INSERT INTO wp_smoke (post_title) VALUES (%s)", ("Third",))
        conn.commit()
        cur.execute("SELECT LAST_INSERT_ID()")
        check("pymysql: LAST_INSERT_ID()=3", cur.fetchone()[0] == 3)
        cur.execute("SELECT ID, post_title FROM wp_smoke WHERE post_status = %s ORDER BY ID", ("publish",))
        rows = cur.fetchall()
        # ID1 explicit 'publish'; ID3 takes the column DEFAULT 'publish'.
        check("pymysql: WHERE + ORDER BY", rows == ((1, "Hello DNA"), (3, "Third")), rows)
        cur.execute("SELECT CONCAT(post_title, ' #', ID) FROM wp_smoke WHERE ID = 1")
        check("pymysql: CONCAT(col, lit)", cur.fetchone()[0] == "Hello DNA #1")
        cur.execute("SELECT UPPER(post_title) FROM wp_smoke WHERE ID = 1")
        check("pymysql: UPPER(col)", cur.fetchone()[0] == "HELLO DNA")
        cur.execute("SELECT IFNULL(NULL, 'fallback'), COALESCE(NULL, 'x', 'y'), IF(1 > 0, 'yes', 'no')")
        row = cur.fetchone()
        check("pymysql: IFNULL/COALESCE/IF", row == ("fallback", "x", "yes"), row)
        cur.execute("UPDATE wp_smoke SET post_status = 'private' WHERE ID = 2")
        conn.commit()
        check("pymysql: UPDATE affected=1", cur.rowcount == 1, cur.rowcount)
        cur.execute("DELETE FROM wp_smoke WHERE ID = 3")
        conn.commit()
        check("pymysql: DELETE affected=1", cur.rowcount == 1, cur.rowcount)
        try:
            cur.execute("SELECT * FROM wp_missing_table")
            check("pymysql: missing table → 1146", False, "no error raised")
        except pymysql.err.ProgrammingError as e:
            check("pymysql: missing table → 1146", e.args[0] == 1146, e.args[0])
        cur.execute("SHOW FULL COLUMNS FROM wp_smoke")
        cols = [r[0] for r in cur.fetchall()]
        check("pymysql: SHOW FULL COLUMNS dbDelta shape", cols == ["ID", "post_title", "post_status"], cols)
        cur.execute("SHOW INDEX FROM wp_smoke")
        idx = cur.fetchall()
        check("pymysql: SHOW INDEX non-empty", len(idx) >= 2, idx)
        cur.execute("SET NAMES utf8mb4")
        cur.execute("SELECT CONCAT('днк', '-тест')")
        check("pymysql: utf8mb4 roundtrip", cur.fetchone()[0] == "днк-тест")
    conn.close()


def smoke_connector(port):
    import mysql.connector
    conn = mysql.connector.connect(
        host="127.0.0.1", port=port, user="wp", password="wp_pass",
        database="wordpress", ssl_disabled=True,
    )
    cur = conn.cursor(prepared=True)  # forces COM_STMT_PREPARE/EXECUTE
    cur.execute("SELECT ID, post_title FROM wp_smoke WHERE ID = %s", (1,))
    rows = cur.fetchall()
    check("connector: server-side prepared SELECT", rows == [(1, "Hello DNA")], rows)
    cur.execute("INSERT INTO wp_smoke (post_title, post_status) VALUES (%s, %s)", ("Via PS", "draft"))
    conn.commit()
    check("connector: prepared INSERT affected=1", cur.rowcount == 1, cur.rowcount)
    cur.execute("SELECT LAST_INSERT_ID()")
    lid = cur.fetchall()[0][0]
    check("connector: LAST_INSERT_ID()=4 after PS insert", lid == 4, lid)
    cur.execute("SELECT post_title FROM wp_smoke WHERE ID = %s", (4,))
    check("connector: read-back via PS", cur.fetchall()[0][0] == "Via PS")
    try:
        cur.execute("SELECT * FROM wp_missing_table")
        check("connector: missing table → 1146", False, "no error")
    except mysql.connector.Error as e:
        check("connector: missing table → 1146", e.errno == 1146, e.errno)
    cur.close()
    # ping via COM_PING
    check("connector: ping()", conn.is_connected())
    conn.close()


def main():
    port = int(sys.argv[1])
    smoke_pymysql(port)
    smoke_connector(port)
    failed = [r for r in RESULTS if not r[1]]
    print(f"\n{len(RESULTS) - len(failed)}/{len(RESULTS)} checks passed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
