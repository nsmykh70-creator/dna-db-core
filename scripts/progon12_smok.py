#!/usr/bin/env python3
# Прогон №12 смок: раздел «Расхождения и ошибки» устранён — живая проверка
# через MySQL wire (pymysql). Все проверки должны дать MariaDB-поведение.
import os
import sys
import time

import pymysql

HOST = "127.0.0.1"
PORT = int(os.environ.get("SMOK_PORT", "13307"))
USER = os.environ.get("SMOK_USER", "root")
PASS = os.environ.get("SMOK_PASS", "dnatest")
DB = "smok12"

ok = 0
fail = 0


def check(name, cond, detail=""):
    global ok, fail
    if cond:
        ok += 1
        print(f"  PASS  {name}")
    else:
        fail += 1
        print(f"  FAIL  {name}  {detail}")


def wait_port():
    import socket
    deadline = time.time() + 30
    while time.time() < deadline:
        try:
            with socket.create_connection((HOST, PORT), timeout=1):
                return True
        except OSError:
            time.sleep(0.3)
    return False


def main():
    if not wait_port():
        print("SMOK_FAIL: server port never opened")
        return 2
    conn = pymysql.connect(
        host=HOST, port=PORT, user=USER, password=PASS, database=DB,
        autocommit=True,
    )
    cur = conn.cursor()

    # 1) CREATE + декорированная таблица (decimal + on update)
    cur.execute("DROP TABLE IF EXISTS items")
    cur.execute(
        "CREATE TABLE items ("
        " id bigint(20) unsigned NOT NULL AUTO_INCREMENT,"
        " name varchar(50) NOT NULL DEFAULT '',"
        " price decimal(10,2) NOT NULL DEFAULT '0.00',"
        " modified datetime NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,"
        " PRIMARY KEY (id)) ENGINE=InnoDB"
    )
    cur.execute("INSERT INTO items (name, price) VALUES ('кружка', 250.00)")
    cur.execute("INSERT INTO items (name, price) VALUES ('tea', 0.10)")

    # 2) Паритет ошибок 1062: текст = MariaDB
    cur.execute("CREATE TABLE opts (id bigint(20) unsigned NOT NULL AUTO_INCREMENT,"
                " name varchar(32) NOT NULL DEFAULT '', PRIMARY KEY (id),"
                " UNIQUE KEY name (name)) ENGINE=InnoDB")
    cur.execute("INSERT INTO opts (name) VALUES ('foo')")
    try:
        cur.execute("INSERT INTO opts (name) VALUES ('foo')")
        check("1062 raised", False, "no error")
    except pymysql.err.IntegrityError as e:
        check("1062 errno", e.args[0] == 1062, repr(e.args))
        check("1062 text", e.args[1] == "Duplicate entry 'foo' for key 'name'", repr(e.args))

    # 3) 1091: DROP COLUMN / DROP INDEX несуществующих
    try:
        cur.execute("ALTER TABLE opts DROP COLUMN old_col")
        check("1091 col raised", False, "no error")
    except pymysql.err.OperationalError as e:
        check("1091 col text", e.args[0] == 1091 and
              e.args[1] == "Can't DROP COLUMN `old_col`; check that it exists", repr(e.args))
    try:
        cur.execute("DROP INDEX no_idx ON opts")
        check("1091 idx raised", False, "no error")
    except pymysql.err.OperationalError as e:
        check("1091 idx text", e.args[0] == 1091 and
              e.args[1] == "Can't DROP `no_idx`; check that column/key exists", repr(e.args))

    # 4) SHOW TABLE STATUS Rows = живой счёт
    cur.execute("SHOW TABLE STATUS LIKE 'items'")
    row = cur.fetchone()
    cols = [d[0] for d in cur.description]
    rows_n = dict(zip(cols, row))["Rows"]
    check("TABLE STATUS Rows=2", rows_n == 2, f"got {rows_n!r}")

    # 5) ON UPDATE CURRENT_TIMESTAMP: явное назначение держится
    cur.execute("UPDATE items SET name='x', modified='2020-01-01 00:00:00' WHERE id = 1")
    cur.execute("SELECT modified FROM items WHERE id = 1")
    check("on-update explicit wins", str(cur.fetchone()[0]) == "2020-01-01 00:00:00",
          "pinned value changed")
    # …а без явного — бампится
    cur.execute("UPDATE items SET name='y' WHERE id = 1")
    cur.execute("SELECT modified FROM items WHERE id = 1")
    bumped = str(cur.fetchone()[0])
    check("on-update auto-bump", bumped != "2020-01-01 00:00:00", f"got {bumped!r}")

    # 6) SQL_CALC_FOUND_ROWS: FOUND_ROWS() = счёт без LIMIT
    cur.execute("SELECT SQL_CALC_FOUND_ROWS id FROM items ORDER BY id LIMIT 1")
    check("page limited", len(cur.fetchall()) == 1)
    cur.execute("SELECT FOUND_ROWS()")
    check("FOUND_ROWS unlimited", str(cur.fetchone()[0]) == "2")

    # 7) SUM(выражение) — агрегированная строка со шкалой
    cur.execute("SELECT SUM(price * 2) FROM items")
    row = cur.fetchone()
    check("SUM(expr) single row", len(row) == 1)
    check("SUM(expr) scale", str(row[0]) == "500.20", f"got {row[0]!r}")

    # 8) MODIFY: retype с приведением данных + SHOW FULL COLUMNS следует
    cur.execute("ALTER TABLE items MODIFY COLUMN price decimal(12,4) NOT NULL DEFAULT 0.0000")
    cur.execute("SELECT price FROM items WHERE id = 2")
    check("MODIFY retype value", str(cur.fetchone()[0]) == "0.1000", "scale not applied")
    cur.execute("SHOW FULL COLUMNS FROM items")
    cols = [d[0] for d in cur.description]
    types = {dict(zip(cols, r))["Field"]: dict(zip(cols, r))["Type"] for r in cur.fetchall()}
    check("MODIFY overlay Type", types.get("price") == "decimal(12,4)", f"got {types.get('price')!r}")

    # 9) CHANGE: rename + unique enforcement + 1054 на старое имя
    cur.execute("ALTER TABLE opts CHANGE COLUMN name opt_name varchar(64) NOT NULL DEFAULT ''")
    cur.execute("SELECT opt_name FROM opts WHERE id = 1")
    check("CHANGE rename read", str(cur.fetchone()[0]) == "foo")
    try:
        cur.execute("SELECT name FROM opts WHERE id = 1")
        check("old col 1054", False, "old column still readable")
    except pymysql.err.OperationalError as e:
        check("old col 1054", e.args[0] == 1054 and
              "Unknown column 'name'" in e.args[1], repr(e.args))
    try:
        cur.execute("INSERT INTO opts (opt_name) VALUES ('foo')")
        check("unique follows rename", False, "duplicate accepted")
    except pymysql.err.IntegrityError as e:
        check("unique follows rename", e.args[0] == 1062 and
              e.args[1] == "Duplicate entry 'foo' for key 'name'", repr(e.args))

    # 10) SHOW CREATE TABLE воспроизводит ON UPDATE (roundtrip)
    cur.execute("SHOW CREATE TABLE items")
    ddl = list(cur.fetchone())[1]
    check("SHOW CREATE keeps ON UPDATE", "ON UPDATE CURRENT_TIMESTAMP" in ddl)

    cur.close()
    conn.close()
    print(f"SMOK12: ok={ok} fail={fail}")
    if fail == 0:
        print("PROGON12_SMOK_VERDICT: OK")
        return 0
    print("PROGON12_SMOK_VERDICT: FAILED")
    return 1


if __name__ == "__main__":
    sys.exit(main())
