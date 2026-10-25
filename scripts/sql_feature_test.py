#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""SQL feature conformance test: DNA-DB vs MySQL vs PostgreSQL.

Runs the same SQL feature checks (DDL, WHERE/ORDER BY, JOIN, aggregates,
subqueries, secondary indexes, known limitations) against all three
systems, verifies every result against a Python reference model and writes
docs/sql_feature_results.json (source of docs/SQL_FEATURES*.md).

Usage:
    python scripts/sql_feature_test.py            # all three systems
    python scripts/sql_feature_test.py --skip-mysql --skip-postgres

Requires a built release binary (DNA-DB starts in a temporary data dir) and
running MySQL/PostgreSQL instances: see docs/SQL_FEATURES.md.
"""

import argparse
import copy
import json
import os
import shutil
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
PROJECT = os.path.dirname(HERE)
DOCS = os.path.join(PROJECT, "docs")
sys.path.insert(0, HERE)

from bench_compare import DnaDbClient, Server, free_port  # noqa: E402

# --------------------------------------------------------------------------
# Reference model — the same logical data is loaded into every system.
# --------------------------------------------------------------------------

USERS = [
    {"user_id": 1, "username": "alice", "balance": 100.0},
    {"user_id": 2, "username": "bob", "balance": 250.5},
    {"user_id": 3, "username": "carol", "balance": 50.0},
    {"user_id": 4, "username": "dave", "balance": 300.0},
    {"user_id": 5, "username": "erin", "balance": 150.0},
    {"user_id": 6, "username": "frank", "balance": 0.0},
    {"user_id": 10, "username": "nina", "balance": None},  # NULL row
]

ORDERS = [
    {"order_id": 101, "user_id": 1, "total_usd": 50.0, "status": "paid"},
    {"order_id": 102, "user_id": 1, "total_usd": 150.5, "status": "shipped"},
    {"order_id": 103, "user_id": 2, "total_usd": 400.0, "status": "paid"},
    {"order_id": 104, "user_id": 3, "total_usd": 600.0, "status": "pending"},
    {"order_id": 105, "user_id": 3, "total_usd": 100.0, "status": "paid"},
    {"order_id": 106, "user_id": 4, "total_usd": 250.0, "status": "shipped"},
    {"order_id": 107, "user_id": 5, "total_usd": 75.0, "status": "pending"},
    {"order_id": 108, "user_id": 5, "total_usd": 350.0, "status": "paid"},
]

BIG_ROWS = 1000
BIG = [{"order_id": i, "user_id": i % 500, "total_usd": i * 1.5}
       for i in range(1, BIG_ROWS + 1)]

DDL = [
    "CREATE TABLE users (user_id INT, username TEXT, balance FLOAT, "
    "PRIMARY KEY (user_id))",
    "CREATE TABLE orders (order_id INT, user_id INT, total_usd FLOAT, "
    "status TEXT, PRIMARY KEY (order_id), FOREIGN KEY (user_id) "
    "REFERENCES users(user_id))",
    "CREATE TABLE big_orders (order_id INT, user_id INT, total_usd FLOAT, "
    "PRIMARY KEY (order_id))",
]

INSERT_USERS = [
    "INSERT INTO users (user_id, username, balance) VALUES (%d, '%s', %s)"
    % (u["user_id"], u["username"],
       "NULL" if u["balance"] is None else repr(u["balance"]))
    for u in USERS
]
INSERT_ORDERS = [
    "INSERT INTO orders (order_id, user_id, total_usd, status) "
    "VALUES (%d, %d, %s, '%s')" % (o["order_id"], o["user_id"],
                                   repr(o["total_usd"]), o["status"])
    for o in ORDERS
]
INSERT_BIG = [
    "INSERT INTO big_orders (order_id, user_id, total_usd) "
    "VALUES (%d, %d, %s)" % (b["order_id"], b["user_id"],
                             repr(b["total_usd"]))
    for b in BIG
]

RESET = {
    "dnadb": [],   # fresh temp data dir on every run
    "mysql": ["DROP TABLE IF EXISTS users, orders, big_orders"],
    "postgres": ["DROP TABLE IF EXISTS big_orders",
                 "DROP TABLE IF EXISTS orders",
                 "DROP TABLE IF EXISTS users"],
}

DROP_INDEX_SQL = {
    "dnadb": "DROP INDEX idx_big_user",
    "mysql": "DROP INDEX idx_big_user ON big_orders",
    "postgres": "DROP INDEX idx_big_user",
}


# --------------------------------------------------------------------------
# Adapters
# --------------------------------------------------------------------------

class Res:
    def __init__(self, rows, err, plan=None):
        self.rows, self.err, self.plan = rows, err, plan


class DnaDb:
    key = "dnadb"
    name = "DNA-DB"

    def __init__(self, port):
        self.conn = DnaDbClient("127.0.0.1", port, "admin", "bench-pass")

    def exec(self, sql):
        raw = self.conn.sql(sql)
        if raw.startswith(b"ERR"):
            return Res(None, raw.decode("utf-8", "replace"))
        try:
            data = json.loads(raw)
        except Exception:
            return Res(None, "unparseable response: %r" % raw[:200])
        return Res(data.get("rows", []), None, data.get("plan"))

    def explain_used(self, sql):
        res = self.exec(sql)
        if res.err:
            return False, "query error: " + res.err
        plan = res.plan or ""
        return "IndexScan" in plan, plan

    def close(self):
        try:
            self.conn.close()
        except Exception:
            pass


class MySql:
    key = "mysql"
    name = "MySQL"

    def __init__(self):
        import pymysql
        from pymysql.cursors import DictCursor
        self.DictCursor = DictCursor
        self.conn = pymysql.connect(host="127.0.0.1", port=3306, user="root",
                                    password="bench", database="bench",
                                    autocommit=True, charset="utf8mb4")

    def exec(self, sql):
        cur = self.conn.cursor(self.DictCursor)
        try:
            cur.execute(sql)
            rows = list(cur.fetchall()) if cur.description else []
            return Res([dict(r) for r in rows], None)
        except Exception as e:
            return Res(None, str(e))
        finally:
            cur.close()

    def explain_used(self, sql):
        res = self.exec("EXPLAIN " + sql)
        if res.err:
            return False, "explain error: " + res.err
        if not res.rows:
            return False, "empty explain"
        r = res.rows[0]
        key, typ = r.get("key"), r.get("type")
        return bool(key) and typ != "ALL", "key=%s type=%s" % (key, typ)

    def close(self):
        self.conn.close()


class Pg:
    key = "postgres"
    name = "PostgreSQL"

    def __init__(self):
        import psycopg2
        self.conn = psycopg2.connect(host="127.0.0.1", port=5432, user="bench",
                                     password="bench", dbname="bench")
        self.conn.autocommit = True

    def exec(self, sql):
        cur = self.conn.cursor()
        try:
            cur.execute(sql)
            if cur.description is None:
                return Res([], None)
            cols = [d[0].lower() for d in cur.description]
            rows = [dict(zip(cols, r)) for r in cur.fetchall()]
            return Res(rows, None)
        except Exception as e:
            self.conn.rollback()
            return Res(None, str(e).strip().split("\n")[0])
        finally:
            cur.close()

    def explain_used(self, sql):
        res = self.exec("EXPLAIN " + sql)
        if res.err:
            return False, "explain error: " + res.err
        text = " ".join(str(r.get("query plan", "")) for r in res.rows)
        hits = ("Index Scan", "Index Only Scan", "Bitmap Index Scan")
        return any(h in text for h in hits), text

    def close(self):
        self.conn.close()


# --------------------------------------------------------------------------
# Result helpers
# --------------------------------------------------------------------------

def r_rows(expected, actual, note="", unordered=False):
    return "rows", {"expected": expected, "actual": actual, "note": note,
                     "unordered": unordered}


def r_err(message, note=""):
    return "error", {"error": message, "note": note}


def r_custom(status, detail):
    return "custom", {"status": status, "detail": detail}


def _norm(row):
    return {str(k).lower(): v for k, v in row.items()}


def _val_eq(a, b):
    if a is None or b is None:
        return a is None and b is None
    if isinstance(a, str) and isinstance(b, str):
        return a == b
    try:
        fa, fb = float(a), float(b)
    except (TypeError, ValueError):
        return a == b
    return abs(fa - fb) <= 1e-6 * max(1.0, abs(fb))


def _row_eq(a, b):
    na, nb = _norm(a), _norm(b)
    if set(na) != set(nb):
        return False
    return all(_val_eq(na[k], nb[k]) for k in na)


def rows_equal(expected, actual, unordered=False):
    if actual is None or len(expected) != len(actual):
        return False
    if not unordered:
        return all(_row_eq(e, a) for e, a in zip(expected, actual))
    pool = list(actual)
    for e in expected:
        for i, a in enumerate(pool):
            if _row_eq(e, a):
                pool.pop(i)
                break
        else:
            return False
    return not pool


def model_users(model):
    return sorted(model["users"], key=lambda r: r["user_id"])


def model_orders(model):
    return sorted(model["orders"], key=lambda r: r["order_id"])


def query(db, sql, expected, unordered=False, note=""):
    """Run SELECT-ish statement; compare with the reference expectation."""
    res = db.exec(sql)
    if res.err:
        return r_err(res.err, note)
    ev = note
    if res.plan:
        ev = (note + " | plan: " + res.plan) if note else "plan: " + res.plan
    return r_rows(expected, res.rows, ev, unordered=unordered)


# --------------------------------------------------------------------------
# Checks — executed in this exact order (later ones mutate data/model).
# --------------------------------------------------------------------------

def ck_create(db, model):
    """DDL round trip on a dedicated probe table (setup already loaded data)."""
    stmts = [
        "CREATE TABLE ddl_probe (id INT, label TEXT, PRIMARY KEY (id))",
        "INSERT INTO ddl_probe (id, label) VALUES (1, 'probe')",
        "SELECT id, label FROM ddl_probe ORDER BY id",
        "DROP TABLE ddl_probe",
    ]
    notes = []
    for i, stmt in enumerate(stmts):
        res = db.exec(stmt)
        if res.err:
            return r_err(res.err, "stmt: " + stmt)
        if res.plan:
            notes.append(res.plan)
        if i == 2 and not rows_equal([{"id": 1, "label": "probe"}],
                                     res.rows):
            return r_custom("mismatch", "probe table readback: %s"
                            % res.rows)
    return r_custom("ok", "; ".join(notes) or "CREATE/INSERT/SELECT/DROP")


def ck_pk_unique(db, model):
    res = db.exec("INSERT INTO users (user_id, username, balance) "
                  "VALUES (1, 'dup', 1.0)")
    if res.err:
        return r_custom("ok", "rejected: " + res.err)
    return r_custom("mismatch", "duplicate primary key was accepted")


def ck_fk_violation(db, model):
    res = db.exec("INSERT INTO orders (order_id, user_id, total_usd, status) "
                  "VALUES (901, 99, 1.0, 'paid')")
    if res.err:
        return r_custom("ok", "rejected: " + res.err)
    return r_custom("mismatch", "orphan foreign key row was accepted")


def ck_select_all(db, model):
    return query(db, "SELECT user_id, username, balance FROM users "
                     "ORDER BY user_id", model_users(model))


def ck_projection_alias(db, model):
    exp = [{"user_id": u["user_id"], "name": u["username"]}
           for u in model_users(model)]
    return query(db, "SELECT user_id, username AS name FROM users "
                     "ORDER BY user_id", exp)


def ck_where_in(db, model):
    exp = [o["order_id"] for o in model_orders(model)
           if o["status"] in ("paid", "shipped")]
    return query(db, "SELECT order_id FROM orders WHERE status IN "
                     "('paid','shipped') ORDER BY order_id",
                 [{"order_id": i} for i in exp])


def ck_where_and_or(db, model):
    exp = [o["order_id"] for o in model_orders(model)
           if o["status"] != "paid" and o["total_usd"] < 300]
    return query(db, "SELECT order_id FROM orders WHERE status != 'paid' "
                     "AND total_usd < 300 ORDER BY order_id",
                 [{"order_id": i} for i in exp])


def ck_order_limit_offset(db, model):
    ordered = sorted(model["orders"], key=lambda r: -r["total_usd"])
    exp = [{"order_id": o["order_id"], "total_usd": o["total_usd"]}
           for o in ordered[1:4]]
    return query(db, "SELECT order_id, total_usd FROM orders "
                     "ORDER BY total_usd DESC LIMIT 3 OFFSET 1", exp)


def ck_is_null(db, model):
    exp = [u["user_id"] for u in model_users(model) if u["balance"] is None]
    return query(db, "SELECT user_id FROM users WHERE balance IS NULL "
                     "ORDER BY user_id", [{"user_id": i} for i in exp])


def ck_is_not_null(db, model):
    exp = [u["user_id"] for u in model_users(model)
           if u["balance"] is not None]
    return query(db, "SELECT user_id FROM users WHERE balance IS NOT NULL "
                     "ORDER BY user_id", [{"user_id": i} for i in exp])


def ck_computed_projection(db, model):
    exp = [{"order_id": o["order_id"], "double_total": o["total_usd"] * 2}
           for o in model_orders(model)[:3]]
    return query(db, "SELECT order_id, total_usd * 2 AS double_total "
                     "FROM orders ORDER BY order_id LIMIT 3", exp)


def ck_qualified_order_by(db, model):
    exp = []
    users = {u["user_id"]: u["username"] for u in model_users(model)}
    for o in model_orders(model):
        exp.append({"username": users[o["user_id"]],
                    "order_id": o["order_id"]})
    return query(db, "SELECT username, order_id FROM users INNER JOIN orders "
                     "ON users.user_id = orders.user_id "
                     "ORDER BY orders.order_id", exp)


def ck_join_inner(db, model):
    exp = []
    users = {u["user_id"]: u["username"] for u in model_users(model)}
    for o in model_orders(model):
        exp.append({"username": users[o["user_id"]], "order_id": o["order_id"]})
    return query(db, "SELECT username, order_id FROM users "
                     "INNER JOIN orders ON users.user_id = orders.user_id "
                     "ORDER BY order_id", exp)


def ck_join_left(db, model):
    exp = []
    users = {u["user_id"]: u["username"] for u in model_users(model)}
    orders = {}
    for o in model_orders(model):
        orders.setdefault(o["user_id"], []).append(o["order_id"])
    for u in model_users(model):
        ids = orders.get(u["user_id"]) or [None]
        for oid in ids:
            exp.append({"username": u["username"], "order_id": oid})
    return query(db, "SELECT username, order_id FROM users LEFT JOIN orders "
                     "ON users.user_id = orders.user_id "
                     "ORDER BY username, order_id", exp)


def ck_right_join(db, model):
    exp = []
    users = {u["user_id"]: u["username"] for u in model_users(model)}
    for o in model_orders(model):
        exp.append({"username": users[o["user_id"]], "order_id": o["order_id"]})
    return query(db, "SELECT username, order_id FROM users RIGHT JOIN orders "
                     "ON users.user_id = orders.user_id ORDER BY order_id",
                 exp)


def ck_full_outer_join(db, model):
    exp, matched = [], set()
    orders_by_user = {}
    for o in model_orders(model):
        orders_by_user.setdefault(o["user_id"], []).append(o)
    for u in model_users(model):
        rows = orders_by_user.get(u["user_id"], [])
        if rows:
            for o in rows:
                exp.append({"username": u["username"],
                            "order_id": o["order_id"]})
                matched.add(o["order_id"])
        else:
            exp.append({"username": u["username"], "order_id": None})
    # orders without a matching user sort last (NULL user_id, NULLS LAST)
    for o in model_orders(model):
        if o["order_id"] not in matched:
            exp.append({"username": None, "order_id": o["order_id"]})
    return query(db, "SELECT username, order_id FROM users FULL OUTER JOIN "
                     "orders ON users.user_id = orders.user_id "
                     "ORDER BY users.user_id, orders.order_id", exp)


def ck_agg_functions(db, model):
    totals = [o["total_usd"] for o in model_orders(model)]
    exp = [{"n": len(totals), "s": sum(totals),
            "a": sum(totals) / len(totals),
            "mn": min(totals), "mx": max(totals)}]
    return query(db, "SELECT COUNT(*) AS n, SUM(total_usd) AS s, "
                     "AVG(total_usd) AS a, MIN(total_usd) AS mn, "
                     "MAX(total_usd) AS mx FROM orders", exp)


def ck_group_by(db, model):
    counts = {}
    for o in model_orders(model):
        counts[o["status"]] = counts.get(o["status"], 0) + 1
    exp = [{"status": s, "n": counts[s]} for s in sorted(counts)]
    return query(db, "SELECT status, COUNT(*) AS n FROM orders "
                     "GROUP BY status ORDER BY status", exp)


def ck_having(db, model):
    counts = {}
    for o in model_orders(model):
        counts[o["status"]] = counts.get(o["status"], 0) + 1
    exp = [{"status": s, "n": counts[s]} for s in sorted(counts)
           if counts[s] > 2]
    return query(db, "SELECT status, COUNT(*) AS n FROM orders "
                     "GROUP BY status HAVING COUNT(*) > 2 ORDER BY status",
                 exp)


def ck_order_by_aggregate(db, model):
    sums, counts = {}, {}
    for o in model_orders(model):
        sums[o["status"]] = sums.get(o["status"], 0.0) + o["total_usd"]
        counts[o["status"]] = counts.get(o["status"], 0) + 1
    exp = [{"status": s, "n": counts[s], "revenue": sums[s]}
           for s in sorted(sums, key=lambda s: -sums[s])]
    return query(db, "SELECT status, COUNT(*) AS n, SUM(total_usd) AS revenue "
                     "FROM orders GROUP BY status ORDER BY revenue DESC", exp)


def ck_agg_skips_null(db, model):
    vals = [u["balance"] for u in model_users(model)
            if u["balance"] is not None]
    exp = [{"a": sum(vals) / len(vals)}]
    return query(db, "SELECT AVG(balance) AS a FROM users", exp,
                 note="AVG must ignore the NULL row")


def ck_in_subquery(db, model):
    with_orders = {o["user_id"] for o in model_orders(model)}
    exp = [{"user_id": u["user_id"], "username": u["username"]}
           for u in model_users(model) if u["user_id"] in with_orders]
    return query(db, "SELECT user_id, username FROM users WHERE user_id IN "
                     "(SELECT user_id FROM orders) ORDER BY user_id", exp)


def ck_scalar_subquery(db, model):
    exp = [{"mx": max(o["total_usd"] for o in model_orders(model))}]
    return query(db, "SELECT (SELECT MAX(total_usd) FROM orders) AS mx", exp)


def ck_index_create(db, model):
    notes = []
    for stmt in ("CREATE INDEX idx_big_user ON big_orders (user_id)",
                 "CREATE INDEX idx_big_total ON big_orders (total_usd)"):
        res = db.exec(stmt)
        if res.err:
            return r_err(res.err)
        if res.plan:
            notes.append(res.plan)
    # Statistics must be fresh before judging the planner (standard practice:
    # PostgreSQL needs ANALYZE; InnoDB refreshes stats itself; DNA-DB has no
    # statistics subsystem — its planner works off index encodings).
    res = db.exec("ANALYZE big_orders")
    if res.err:
        notes.append("analyze skipped: " + res.err[:80])
    return r_custom("ok", "; ".join(notes) or "indexes created")


def ck_index_plan_equality(db, model):
    exp = [{"order_id": b["order_id"]} for b in model["big"]
           if b["user_id"] == 123]
    sql = "SELECT order_id FROM big_orders WHERE user_id = 123"
    res = db.exec(sql)
    if res.err:
        return r_err(res.err)
    used, detail = db.explain_used(sql)
    if not rows_equal(exp, res.rows, unordered=True):
        return r_custom("mismatch",
                        "wrong rows: expected %s got %s" % (exp, res.rows))
    if not used:
        return r_custom("mismatch", "index not used by planner (%s)" % detail)
    return r_custom("ok", "index used: %s | rows match: %d"
                    % (detail, len(exp)))


def ck_index_plan_range(db, model):
    exp = [{"order_id": b["order_id"]} for b in model["big"]
           if b["total_usd"] >= 1490]
    sql = "SELECT order_id FROM big_orders WHERE total_usd >= 1490"
    res = db.exec(sql)
    if res.err:
        return r_err(res.err)
    used, detail = db.explain_used(sql)
    if not rows_equal(exp, res.rows, unordered=True):
        return r_custom("mismatch",
                        "wrong rows: expected %d got %d"
                        % (len(exp), len(res.rows or [])))
    if not used:
        return r_custom("mismatch", "index not used by planner (%s)" % detail)
    return r_custom("ok", "index used: %s | rows match: %d"
                    % (detail, len(exp)))


def ck_index_dml(db, model):
    """UPDATE/DELETE must keep the secondary index in lockstep."""
    res = db.exec("UPDATE big_orders SET total_usd = 9999.5 "
                  "WHERE order_id = 123")
    if res.err:
        return r_err(res.err)
    res = db.exec("SELECT order_id FROM big_orders WHERE total_usd = 9999.5")
    if res.err:
        return r_err(res.err)
    if not rows_equal([{"order_id": 123}], res.rows, unordered=True):
        return r_custom("mismatch",
                        "after UPDATE by indexed column: %s" % res.rows)
    res = db.exec("DELETE FROM big_orders WHERE order_id = 456")
    if res.err:
        return r_err(res.err)
    res = db.exec("SELECT order_id FROM big_orders WHERE user_id = 456")
    if res.err:
        return r_err(res.err)
    if not rows_equal([{"order_id": 956}], res.rows, unordered=True):
        return r_custom("mismatch",
                        "after DELETE: rows for user_id=456: %s" % res.rows)
    return r_custom("ok", "index entries updated by UPDATE and DELETE")


def mutate_index_dml(model):
    for b in model["big"]:
        if b["order_id"] == 123:
            b["total_usd"] = 9999.5
    model["big"] = [b for b in model["big"] if b["order_id"] != 456]


def ck_index_drop(db, model):
    res = db.exec(DROP_INDEX_SQL[db.key])
    if res.err:
        return r_err(res.err)
    exp = [{"order_id": b["order_id"]} for b in model["big"]
           if b["user_id"] == 123]
    res2 = db.exec("SELECT order_id FROM big_orders WHERE user_id = 123")
    if res2.err:
        return r_err(res2.err)
    if not rows_equal(exp, res2.rows, unordered=True):
        return r_custom("mismatch",
                        "query after DROP INDEX: expected %s got %s"
                        % (exp, res2.rows))
    return r_custom("ok", "%s -> %d row(s) still correct"
                    % (DROP_INDEX_SQL[db.key], len(exp)))


def ck_transaction(db, model):
    """BEGIN ... ROLLBACK must erase the write (SQL transactions)."""
    for stmt in ("BEGIN",
                 "INSERT INTO users (user_id, username, balance) "
                 "VALUES (50, 'tx_user', 1.0)",
                 "ROLLBACK"):
        res = db.exec(stmt)
        if res.err:
            return r_err(res.err + " | stmt: " + stmt)
    return query(db, "SELECT username FROM users WHERE user_id = 50", [],
                 note="rolled-back row must not exist")


def ck_union(db, model):
    exp = [{"username": u["username"]} for u in model_users(model)]
    exp += [{"username": o["status"]} for o in model_orders(model)]
    # UNION de-duplicates: users/statuses are disjoint in this dataset
    seen, uniq = set(), []
    for r in exp:
        if r["username"] not in seen:
            seen.add(r["username"])
            uniq.append(r)
    return query(db, "SELECT username FROM users UNION SELECT status "
                     "FROM orders", uniq, unordered=True)


def ck_show_tables(db, model):
    if db.key == "postgres":
        sql = ("SELECT table_name FROM information_schema.tables "
               "WHERE table_schema = 'public'")
    else:
        sql = "SHOW TABLES"
    res = db.exec(sql)
    if res.err:
        return r_err(res.err)
    got = set()
    for row in res.rows:
        for v in row.values():
            got.add(str(v).lower())
    missing = {"users", "orders", "big_orders"} - got
    if missing:
        return r_custom("mismatch", "tables not listed: %s" % sorted(missing))
    return r_custom("ok", "%s -> %s" % (sql, sorted(got)))


def ck_explain_statement(db, model):
    res = db.exec("EXPLAIN SELECT order_id FROM orders WHERE status = 'paid'")
    if res.err:
        return r_err(res.err)
    if not res.rows:
        return r_custom("mismatch", "EXPLAIN returned no rows")
    return r_custom("ok", "EXPLAIN returns %d row(s)" % len(res.rows))


CHECKS = [
    # DDL / constraints
    dict(id="create_tables", group="DDL", title="CREATE TABLE + PK + FK",
         fn=ck_create, expect="ok"),
    dict(id="pk_unique", group="DDL",
         title="Duplicate primary key is rejected",
         fn=ck_pk_unique, expect="rows"),
    dict(id="fk_violation", group="DDL",
         title="Foreign key violation is rejected",
         fn=ck_fk_violation, expect="rows"),
    # projection / WHERE / sorting
    dict(id="select_order_by", group="SELECT / WHERE",
         title="SELECT with ORDER BY",
         fn=ck_select_all, expect="rows"),
    dict(id="projection_alias", group="SELECT / WHERE",
         title="Projection aliases (AS)",
         fn=ck_projection_alias, expect="rows"),
    dict(id="where_in_list", group="SELECT / WHERE",
         title="WHERE ... IN (list)",
         fn=ck_where_in, expect="rows"),
    dict(id="where_and_or", group="SELECT / WHERE",
         title="WHERE with AND / != / comparison",
         fn=ck_where_and_or, expect="rows"),
    dict(id="order_limit_offset", group="SELECT / WHERE",
         title="ORDER BY DESC + LIMIT/OFFSET",
         fn=ck_order_limit_offset, expect="rows"),
    dict(id="is_null", group="SELECT / WHERE", title="IS NULL",
         fn=ck_is_null, expect="rows"),
    dict(id="is_not_null", group="SELECT / WHERE", title="IS NOT NULL",
         fn=ck_is_not_null, expect="rows"),
    dict(id="computed_projection", group="SELECT / WHERE",
         title="Computed expression in SELECT (col * 2)",
         fn=ck_computed_projection, expect="rows"),
    dict(id="qualified_order_by", group="SELECT / WHERE",
         title="ORDER BY with qualified column (t.col)",
         fn=ck_qualified_order_by, expect="rows"),
    # joins
    dict(id="join_inner", group="JOIN", title="INNER JOIN",
         fn=ck_join_inner, expect="rows"),
    dict(id="join_left", group="JOIN",
         title="LEFT JOIN with NULL extension",
         fn=ck_join_left, expect="rows"),
    dict(id="join_right", group="JOIN", title="RIGHT JOIN",
         fn=ck_right_join, expect="rows"),
    dict(id="join_full", group="JOIN", title="FULL OUTER JOIN",
         fn=ck_full_outer_join, expect="rows"),
    # aggregates
    dict(id="agg_functions", group="Aggregates",
         title="COUNT / SUM / AVG / MIN / MAX",
         fn=ck_agg_functions, expect="rows"),
    dict(id="group_by", group="Aggregates", title="GROUP BY",
         fn=ck_group_by, expect="rows"),
    dict(id="having", group="Aggregates", title="HAVING on aggregate",
         fn=ck_having, expect="rows"),
    dict(id="order_by_aggregate", group="Aggregates",
         title="ORDER BY aggregate alias",
         fn=ck_order_by_aggregate, expect="rows"),
    dict(id="agg_null_skip", group="Aggregates",
         title="Aggregates ignore NULL values",
         fn=ck_agg_skips_null, expect="rows"),
    # subqueries
    dict(id="in_subquery", group="Subqueries",
         title="WHERE x IN (SELECT ...)",
         fn=ck_in_subquery, expect="rows"),
    dict(id="scalar_subquery", group="Subqueries",
         title="Scalar subquery in SELECT",
         fn=ck_scalar_subquery, expect="rows"),
    # secondary indexes (big table loaded before this group)
    dict(id="index_create", group="Indexes",
         title="CREATE INDEX on existing table",
         fn=ck_index_create, expect="ok"),
    dict(id="index_plan_equality", group="Indexes",
         title="Planner uses index for equality",
         fn=ck_index_plan_equality, expect="ok"),
    dict(id="index_plan_range", group="Indexes",
         title="Planner uses index for range (>=)",
         fn=ck_index_plan_range, expect="ok"),
    dict(id="index_dml", group="Indexes",
         title="Index kept correct by UPDATE / DELETE",
         fn=ck_index_dml, expect="ok", mutate=mutate_index_dml),
    dict(id="index_drop", group="Indexes", title="DROP INDEX",
         fn=ck_index_drop, expect="ok"),
    # transactions / set ops / introspection
    dict(id="transaction_rollback", group="Advanced / limits",
         title="BEGIN / INSERT / ROLLBACK",
         fn=ck_transaction, expect="rows"),
    dict(id="union", group="Advanced / limits", title="UNION",
         fn=ck_union, expect="rows"),
    dict(id="show_tables", group="Advanced / limits",
         title="Table introspection (dialect-specific)",
         fn=ck_show_tables, expect="ok"),
    dict(id="explain_statement", group="Advanced / limits",
         title="EXPLAIN <query>",
         fn=ck_explain_statement, expect="ok"),
]

# Per-system expectation: "rows"/"ok" = feature must work, "unsupported" =
# rejection is the documented behaviour for that system.
GAPS = {
    "is_not_null":            {"dnadb": "unsupported"},
    "computed_projection":    {"dnadb": "unsupported"},
    "qualified_order_by":     {"dnadb": "unsupported"},
    "join_right":             {"dnadb": "unsupported"},
    "join_full":              {"dnadb": "unsupported", "mysql": "unsupported"},
    "scalar_subquery":        {"dnadb": "unsupported"},
    "transaction_rollback":   {"dnadb": "unsupported"},
    "union":                  {"dnadb": "unsupported"},
    "show_tables":            {"dnadb": "unsupported"},
    "explain_statement":      {"dnadb": "unsupported"},
}

BIG_LOAD_SQL = INSERT_BIG


def load_big(db):
    """Fill big_orders (the table itself is created by DDL during setup)."""
    for stmt in BIG_LOAD_SQL:
        res = db.exec(stmt)
        if res.err:
            return res.err
    return None


def evaluate(spec, kind, payload, system):
    expect = GAPS.get(spec["id"], {}).get(system, spec["expect"])
    if kind == "error":
        if expect == "unsupported":
            return {"status": "unsupported",
                    "detail": payload["error"],
                    "note": payload.get("note", "")}
        return {"status": "error",
                "detail": payload["error"],
                "note": payload.get("note", "")}
    if kind == "rows":
        if payload["actual"] is None:
            return {"status": "error", "detail": "no result",
                    "note": payload.get("note", "")}
        ok = rows_equal(payload["expected"], payload["actual"],
                        unordered=payload.get("unordered", False))
        detail = ("%d row(s) match" % len(payload["expected"])) if ok else \
                 "expected %d row(s) %s, got %d row(s) %s" % (
                     len(payload["expected"]), payload["expected"][:30],
                     len(payload["actual"]), payload["actual"][:30])
        if not ok and expect == "unsupported":
            detail = "worked unexpectedly: " + detail
        return {"status": "ok" if ok else "mismatch",
                "detail": detail, "note": payload.get("note", "")}
    return {"status": payload["status"], "detail": payload["detail"],
            "note": ""}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--json-out", default=os.path.join(
        DOCS, "sql_feature_results.json"))
    ap.add_argument("--skip-mysql", action="store_true")
    ap.add_argument("--skip-postgres", action="store_true")
    ap.add_argument("--skip-dnadb", action="store_true")
    args = ap.parse_args()

    model = {"users": copy.deepcopy(USERS), "orders": copy.deepcopy(ORDERS),
             "big": copy.deepcopy(BIG)}

    systems, cleanup = {}, {}
    if not args.skip_dnadb:
        port = free_port()
        data_dir = tempfile.mkdtemp(prefix="dna_sqlfeat_")
        srv = Server(data_dir, port)
        srv.wait_ready()
        systems["dnadb"] = DnaDb(port)
        cleanup["dnadb"] = (srv, data_dir)
    if not args.skip_mysql:
        systems["mysql"] = MySql()
    if not args.skip_postgres:
        systems["postgres"] = Pg()

    setup_errors = {}
    for key, db in list(systems.items()):
        try:
            for stmt in RESET[key] + DDL:
                res = db.exec(stmt)
                if res.err:
                    raise RuntimeError("setup: " + res.err)
            for stmt in INSERT_USERS + INSERT_ORDERS:
                res = db.exec(stmt)
                if res.err:
                    raise RuntimeError("setup: " + res.err)
            err = load_big(db)
            if err:
                raise RuntimeError("big table load: " + err)
        except Exception as e:
            setup_errors[key] = str(e)
            db.close()
            del systems[key]

    # indexes are created by the dedicated check; big table must exist first
    # -> move index creation check execution order handled by CHECKS order.

    out_checks = []
    for spec in CHECKS:
        per = {}
        for key, db in systems.items():
            try:
                kind, payload = spec["fn"](db, model)
            except Exception as e:
                kind, payload = "error", {"error": repr(e), "note": ""}
            per[key] = evaluate(spec, kind, payload, key)
            per[key]["expect"] = GAPS.get(spec["id"], {}).get(
                key, spec["expect"])
        if "mutate" in spec:
            spec["mutate"](model)
        out_checks.append({"id": spec["id"], "group": spec["group"],
                           "title": spec["title"], "systems": per})
        line = "  %-24s" % spec["id"]
        for key in ("dnadb", "mysql", "postgres"):
            if key in per:
                line += " %s=%-11s" % (key[:4], per[key]["status"])
        print(line, flush=True)

    summary = {}
    for key in ("dnadb", "mysql", "postgres"):
        if key not in systems and key not in setup_errors:
            continue
        counts = {"ok": 0, "unsupported": 0, "mismatch": 0, "error": 0,
                  "n/a": 0}
        for c in out_checks:
            st = c["systems"].get(key, {}).get("status", "n/a")
            counts[st if st in counts else "n/a"] += 1
        summary[key] = counts

    results = {
        "meta": {
            "date": time.strftime("%Y-%m-%d %H:%M:%S"),
            "machine": __import__("platform").platform(),
            "systems": {
                "dnadb": "dna-db-core 3.0.0 (native, --sync strict)",
                "mysql": "MySQL 8.4.11 (docker mysql:8.4)",
                "postgres": "PostgreSQL 17.11 (docker postgres:17)",
            },
            "big_rows": BIG_ROWS,
            "setup_errors": setup_errors,
        },
        "checks": out_checks,
        "summary": summary,
    }
    os.makedirs(os.path.dirname(args.json_out), exist_ok=True)
    with open(args.json_out, "w", encoding="utf-8") as f:
        json.dump(results, f, indent=2, ensure_ascii=False)
    print("\nJSON:", args.json_out)
    print("summary:", json.dumps(summary))

    for key, db in systems.items():
        db.close()
    for key, (srv, data_dir) in cleanup.items():
        srv.stop()
        shutil.rmtree(data_dir, ignore_errors=True)


if __name__ == "__main__":
    main()
