#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Differential comparison harness: WordPress/WooCommerce SQL corpus on
MySQL (MariaDB) vs DNA-DB CMS Edition (pgwire + cms::compat gate).

Both engines receive the SAME fixture DDL (generated from the Rust WP
preset), the SAME seed data and the SAME corpus statements
(docs/cms/wp_queries_corpus.txt). Results are normalized and diffed;
the output is a Markdown report with per-query verdicts.

Usage:
  python3 cms/harness/run_comparison.py \
      --mysql-port 13306 --mysql-db wptest \
      --dna-port 15432 \
      --fixtures cms/harness/fixtures_wp.sql \
      --corpus docs/cms/wp_queries_corpus.txt \
      --out /tmp/cms_mysql_comparison_report.md
"""

import argparse
import re
import socket
import struct
import sys

import pymysql

# ---------------------------------------------------------------------------
# PostgreSQL wire (simple protocol) client for DNA-DB
# ---------------------------------------------------------------------------

def _recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("closed")
        buf += chunk
    return buf


def _recv_message(sock):
    tag = _recv_exact(sock, 1)
    (length,) = struct.unpack("!i", _recv_exact(sock, 4))
    body = _recv_exact(sock, length - 4) if length > 4 else b""
    return tag, body


class DnaClient:
    """One DNA-DB pgwire connection = one MySQL-style session (LAST_INSERT_ID,
    named locks and the BEGIN..COMMIT buffer live on this socket)."""

    def __init__(self, host, port):
        self.sock = socket.create_connection((host, port), timeout=30)
        payload = struct.pack("!i", 196608) + b"user\0wp\0database\0wordpress\0\0"
        self.sock.sendall(struct.pack("!i", len(payload) + 4) + payload)
        tag, body = _recv_message(self.sock)
        if tag == b"N":
            tag, body = _recv_message(self.sock)
        while True:
            if tag == b"R":
                (code,) = struct.unpack("!i", body[:4])
                if code != 0:
                    raise RuntimeError(f"auth code {code} not supported")
            elif tag == b"Z":
                break
            tag, body = _recv_message(self.sock)

    def query(self, sql):
        """Returns dict(ok, columns, rows, tag, errno, sqlstate, message)."""
        body = sql.encode() + b"\0"
        self.sock.sendall(b"Q" + struct.pack("!i", len(body) + 4) + body)
        columns, rows = [], []
        tag_final, error_body = None, None
        while True:
            tag, mbody = _recv_message(self.sock)
            if tag == b"T":
                (n,) = struct.unpack("!h", mbody[:2])
                off = 2
                for _ in range(n):
                    end = mbody.index(b"\0", off)
                    columns.append(mbody[off:end].decode())
                    off = end + 1 + 4 + 2 + 4 + 2 + 4 + 2
            elif tag == b"D":
                (n,) = struct.unpack("!h", mbody[:2])
                off = 2
                row = []
                for _ in range(n):
                    (ln,) = struct.unpack("!i", mbody[off:off + 4])
                    off += 4
                    if ln == -1:
                        row.append(None)
                    else:
                        row.append(mbody[off:off + ln].decode())
                        off += ln
                rows.append(row)
            elif tag == b"C":
                tag_final = mbody.rstrip(b"\0").decode()
            elif tag == b"E":
                error_body = mbody
            elif tag == b"Z":
                if error_body is not None:
                    sqlstate, message = "", error_body.decode(errors="replace")
                    for part in error_body.split(b"\0"):
                        if part.startswith(b"C"):
                            sqlstate = part[1:].decode()
                        elif part.startswith(b"M"):
                            message = part[1:].decode()
                    m = re.search(r"\[MySQL errno (\d+)\]", message)
                    return {"ok": False, "columns": [], "rows": [],
                            "tag": None, "errno": int(m.group(1)) if m else 0,
                            "sqlstate": sqlstate, "message": message}
                return {"ok": True, "columns": columns, "rows": rows,
                        "tag": tag_final, "errno": 0, "sqlstate": "",
                        "message": ""}

    def close(self):
        try:
            self.sock.sendall(b"X" + struct.pack("!i", 4))
            self.sock.close()
        except OSError:
            pass


# ---------------------------------------------------------------------------
# MySQL client
# ---------------------------------------------------------------------------

class MySqlClient:
    def __init__(self, host, port, db):
        self.conn = pymysql.connect(host=host, port=port, user="root",
                                    database=db, autocommit=True,
                                    charset="utf8mb4")
        cur = self.conn.cursor()
        # WordPress-hosting reality: relaxed sql_mode (zero dates etc.).
        cur.execute("SET sql_mode = ''")
        cur.execute("SET NAMES utf8mb4")
        cur.close()

    def query(self, sql):
        cur = self.conn.cursor()
        try:
            cur.execute(sql)
            if cur.description:
                columns = [d[0] for d in cur.description]
                rows = [tuple(r) for r in cur.fetchall()]
                return {"ok": True, "columns": columns, "rows": rows,
                        "tag": "SELECT %d" % len(rows), "errno": 0,
                        "sqlstate": "", "message": ""}
            return {"ok": True, "columns": [], "rows": [], "tag": "OK",
                    "errno": 0, "sqlstate": "",
                    "affected": max(cur.rowcount, 0), "message": ""}
        except pymysql.err.MySQLError as e:
            errno = e.args[0] if e.args and isinstance(e.args[0], int) else 0
            msg = e.args[1] if len(e.args) > 1 else str(e)
            return {"ok": False, "columns": [], "rows": [], "tag": None,
                    "errno": errno, "sqlstate": "", "message": str(msg)}
        finally:
            cur.close()

    def close(self):
        self.conn.close()


# ---------------------------------------------------------------------------
# Corpus parsing
# ---------------------------------------------------------------------------

def _strip_comment(line):
    return re.sub(r"\s+--.*$", "", line)


def parse_corpus(path):
    entries, cur = [], None
    for raw in open(path, encoding="utf-8"):
        line = raw.rstrip("\n")
        stripped = line.strip()
        if stripped.startswith("#") or not stripped:
            if cur:
                entries.append(cur)
                cur = None
            continue
        if line.startswith("[") and "]" in line:
            if cur:
                entries.append(cur)
            tag = line[1:line.index("]")]
            cur = {"tag": tag, "sql": _strip_comment(line[line.index("]") + 1:]).strip()}
        elif cur is not None:
            cur["sql"] += " " + _strip_comment(stripped)
    if cur:
        entries.append(cur)
    out = []
    for e in entries:
        sql = " ".join(e["sql"].split())
        if sql:
            out.append((e["tag"], sql))
    return out


# ---------------------------------------------------------------------------
# Fixtures + seed data (identical SQL to BOTH engines)
# ---------------------------------------------------------------------------

WOO_DDL = [
    """CREATE TABLE wp_wc_product_meta_lookup (
        product_id bigint(20) NOT NULL default 0,
        sku varchar(100) default '',
        virtual tinyint(1) default 0,
        downloadable tinyint(1) default 0,
        total_sales decimal(10,4) NOT NULL default 0,
        regular_price decimal(19,4) default NULL,
        sale_price decimal(19,4) default NULL,
        stock_quantity double NOT NULL default 0,
        PRIMARY KEY  (product_id),
        KEY sku (sku)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4""",
    """CREATE TABLE wp_wc_orders (
        id bigint(20) unsigned NOT NULL auto_increment,
        status varchar(20) NOT NULL default '',
        currency varchar(10) NOT NULL default '',
        customer_id bigint(20) unsigned default 0,
        date_created_gmt datetime default NULL,
        PRIMARY KEY  (id),
        KEY status (status),
        KEY customer_id (customer_id)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4""",
    # HPOS-легаси позиции заказа (Woo 11.2.1 dbDelta shape) — регрессия E2E
    # 2026-11-02: wpdb->insert() без формата шлёт order_id СТРОКОЙ в bigint
    # unsigned; MariaDB приводит к типу колонки при записи — мы обязаны тоже,
    # иначе ключ вторичного индекса не совпадает с Int-литералом.
    """CREATE TABLE wp_woocommerce_order_items (
        order_item_id bigint(20) unsigned NOT NULL auto_increment,
        order_item_name text NOT NULL,
        order_item_type varchar(200) NOT NULL default '',
        order_id bigint(20) unsigned NOT NULL default 0,
        PRIMARY KEY  (order_item_id),
        KEY order_id (order_id)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4""",
    """CREATE TABLE wp_woocommerce_order_itemmeta (
        meta_id bigint(20) unsigned NOT NULL auto_increment,
        order_item_id bigint(20) unsigned NOT NULL default 0,
        meta_key varchar(255) default NULL,
        meta_value longtext,
        PRIMARY KEY  (meta_id),
        KEY order_item_id (order_item_id),
        KEY meta_key (meta_key(191))
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4""",
]

SEEDS = [
    "INSERT INTO wp_options (option_name, option_value, autoload) VALUES "
    "('siteurl','https://example.com','yes'),('home','https://example.com','yes'),"
    "('blogname','Test Blog','yes'),('template','twentytwentyfour','yes'),"
    "('stylesheet','twentytwentyfour','yes'),('foo','bar','yes'),"
    "('_transient_woo_x','1','no'),('_transient_timeout_woo_x','9999999999','no')",

    "INSERT INTO wp_posts (ID, post_author, post_date, post_date_gmt, post_content, "
    "post_title, post_excerpt, post_status, comment_status, ping_status, post_password, "
    "post_name, to_ping, pinged, post_modified, post_modified_gmt, post_content_filtered, "
    "post_parent, guid, menu_order, post_type, post_mime_type, comment_count) VALUES "
    "(42, 1, '2026-01-15 10:00:00', '2026-01-15 10:00:00', '<p>hello world</p>', "
    "'Hello world!', '', 'publish', 'open', 'open', '', 'hello-world', '', '', "
    "'2026-01-15 10:00:00', '2026-01-15 10:00:00', '', 0, 'https://example.com/?p=42', "
    "0, 'post', '', 0)",

    "INSERT INTO wp_postmeta (post_id, meta_key, meta_value) VALUES "
    "(42, '_edit_lock', '1762000000:1'), (42, '_thumbnail_id', '7'), "
    "(42, 'color', 'red'), (42, 'size', 'xl'), (42, 'price', '150'), "
    "(7, '_wp_attached_file', '2026/01/img.jpg'), (43, 'color', 'blue')",

    "INSERT INTO wp_terms (term_id, name, slug, term_group) VALUES (4, 'News', 'news', 0)",
    "INSERT INTO wp_term_taxonomy (term_taxonomy_id, term_id, taxonomy, description, parent, count) "
    "VALUES (7, 4, 'category', '', 0, 1)",
    "INSERT INTO wp_term_relationships (object_id, term_taxonomy_id, term_order) VALUES (42, 7, 0)",

    "INSERT INTO wp_users (ID, user_login, user_pass, user_nicename, user_email, user_url, "
    "user_registered, user_activation_key, user_status, display_name) VALUES "
    "(1, 'admin', '$P$hash', 'admin', 'admin@example.com', '', '2026-01-01 00:00:00', '', 0, 'Admin')",

    "INSERT INTO wp_usermeta (user_id, meta_key, meta_value) VALUES "
    "(1, 'wp_capabilities', 'a:1:{s:13:\"administrator\";b:1;}'), (1, 'nickname', 'Admin')",

    "INSERT INTO wp_comments (comment_ID, comment_post_ID, comment_author, comment_date_gmt, comment_approved) "
    "VALUES (5, 42, 'Bob', '2026-01-16 09:00:00', '1'), (6, 42, 'Eve', '2026-01-17 09:00:00', '0')",

    "INSERT INTO wp_wc_product_meta_lookup (product_id, sku, total_sales, regular_price, stock_quantity) "
    "VALUES (42, 'WOO-42', 15.0000, 19.9900, 100)",
    "INSERT INTO wp_wc_orders (status, currency, customer_id, date_created_gmt) VALUES "
    "('processing', 'USD', 1, '2026-01-20 12:00:00'), "
    "('wc-completed', 'USD', 1, '2026-01-21 12:00:00'), "
    "('wc-completed', 'USD', 2, '2026-01-22 12:00:00')",

    # HPOS: заказ 42 из сидов — позиции в форме Woo CRUD (wpdb->insert без
    # формата → все значения СТРОКИ) и в форме wc_add_order_item (%s,%s,%d).
    "INSERT INTO wp_woocommerce_order_items (order_item_name, order_item_type, order_id) VALUES "
    "('Тестовый товар DNA', 'line_item', '42'), ('Прямая вставка', 'line_item', 42)",
    "INSERT INTO wp_woocommerce_order_itemmeta (order_item_id, meta_key, meta_value) VALUES "
    "(1, '_product_id', '42'), (1, '_qty', '2'), (2, '_product_id', '42'), (2, '_qty', '1')",
]

ALL_TABLES = [
    "wp_comments", "wp_commentmeta", "wp_links", "wp_options", "wp_postmeta",
    "wp_posts", "wp_terms", "wp_termmeta", "wp_term_relationships",
    "wp_term_taxonomy", "wp_usermeta", "wp_users",
    "wp_wc_product_meta_lookup", "wp_wc_orders",
    "wp_woocommerce_order_items", "wp_woocommerce_order_itemmeta",
]


def load_fixtures(path):
    stmts, buf = [], []
    for raw in open(path, encoding="utf-8"):
        line = raw.rstrip("\n")
        s = line.strip()
        if s.startswith("--") or not s:
            continue
        buf.append(line)
        if s.endswith(";"):
            stmts.append(" ".join(" ".join(buf).split())[:-1])
            buf = []
    if buf:
        stmts.append(" ".join(" ".join(buf).split()))
    return stmts


# ---------------------------------------------------------------------------
# Comparison
# ---------------------------------------------------------------------------

def norm_cell(v):
    return None if v is None else str(v)


def vals_equal(a, b):
    if a is None or b is None:
        return a is None and b is None
    if a == b:
        return True
    try:
        fa, fb = float(a), float(b)
        return abs(fa - fb) <= 1e-9 * max(1.0, abs(fa), abs(fb))
    except ValueError:
        return False


def canon_row(cols, row):
    cells = [norm_cell(c) for c in row]
    return tuple(zip([c.lower() for c in cols], cells))


def run(client, sql, skip_if_table_exists=None):
    if skip_if_table_exists:
        exists = client.query("SHOW TABLES LIKE '%s'" % skip_if_table_exists)
        if exists["ok"] and exists["rows"]:
            return {"ok": True, "columns": [], "rows": [], "skipped": True,
                    "errno": 0, "sqlstate": "", "message": "table exists"}
    r = client.query(sql)
    r.setdefault("skipped", False)
    return r


def compare(tag, sql, m, d):
    verdict, notes = "", []
    upper_head = sql.upper().lstrip()

    # Session-control / SET statements: both engines accept them; there is
    # nothing meaningful to diff (affected counts are client-side conventions).
    if upper_head.startswith(("SET ", "BEGIN", "START TRANSACTION", "COMMIT",
                              "ROLLBACK", "USE ", "CREATE", "ALTER", "DROP",
                              "TRUNCATE")):
        if m["ok"] and d["ok"]:
            return "MATCH (ctl)", []
        if not m["ok"] and not d["ok"]:
            if m["errno"] == d["errno"] and m["errno"] != 0:
                return "BOTH-ERR (same errno %d)" % m["errno"], []
            return "DIFF-ERR", ["mysql errno %s: %s" % (m["errno"], m["message"][:110]),
                                "dna errno %s: %s" % (d["errno"], d["message"][:110])]
        return "ERR-MYSQL" if not m["ok"] else "ERR-DNA", []

    if m.get("skipped") and d.get("skipped"):
        return "skip (table exists both)", []
    if m.get("skipped") or d.get("skipped"):
        return "SKIP-MISMATCH", ["one side has the table, the other does not"]

    if not m["ok"] and not d["ok"]:
        if m["errno"] == d["errno"] and m["errno"] != 0:
            return "BOTH-ERR (same errno %d)" % m["errno"], []
        return "DIFF-ERR", ["mysql errno %s: %s" % (m["errno"], m["message"][:110]),
                           "dna errno %s: %s" % (d["errno"], d["message"][:110])]
    if not m["ok"]:
        return "ERR-MYSQL", ["dna ok; mysql errno %s: %s" % (m["errno"], m["message"][:140])]
    if not d["ok"]:
        return "ERR-DNA", ["mysql ok; dna errno %s (%s): %s"
                           % (d["errno"], d["sqlstate"], d["message"][:140])]

    # Both OK.
    is_select = bool(m["columns"]) or bool(d["columns"])
    if not is_select:
        ma = m.get("affected")
        da = None
        if d.get("tag"):
            t = d["tag"]
            if t.startswith("INSERT"):
                try:
                    da = int(t.split()[-1])
                except ValueError:
                    da = None
            elif t.startswith(("UPDATE", "DELETE")):
                try:
                    da = int(t.split()[-1])
                except ValueError:
                    da = None
        if ma is None and da is None:
            return "MATCH (ddl)", []
        if ma == da:
            return "MATCH (affected=%s)" % ma, []
        return "DIFF-AFFECTED", ["mysql affected=%s, dna affected=%s" % (ma, da)]

    mc, dc = m["columns"], d["columns"]
    ordered = "ORDER BY" in sql.upper()
    mrows = [[norm_cell(c) for c in r] for r in m["rows"]]
    drows = [[norm_cell(c) for c in r] for r in d["rows"]]

    # Column alignment: when both sides expose the SAME column-name set
    # (WordPress reads by name; DNA returns BTreeMap order for SELECT *),
    # align values by column name instead of position.
    mnames = [c.lower() for c in mc]
    dnames = [c.lower() for c in dc]
    name_aligned = (sorted(mnames) == sorted(dnames) and len(set(mnames)) == len(mnames)
                    and bool(mnames))
    if name_aligned:
        mrows = [[dict(zip(mnames, r)).get(n) for n in mnames] for r in mrows]
        drows = [[dict(zip(dnames, r)).get(n) for n in mnames] for r in drows]
    if not ordered:
        mrows.sort(key=lambda r: repr(r))
        drows.sort(key=lambda r: repr(r))
    notes_cols = ""
    if mnames != dnames:
        notes_cols = " (column names differ: %s vs %s)" % (mc[:4], dc[:4])

    if len(mrows) != len(drows):
        return "DIFF-ROWS" + notes_cols, ["mysql %d rows, dna %d rows" % (len(mrows), len(drows))]

    # Column alignment: same count — align by position; if names differ,
    # align by lower-cased name where possible (SHOW * output).
    for i, (mr, dr) in enumerate(zip(mrows, drows)):
        if len(mr) != len(dr):
            return "DIFF-ARITY" + notes_cols, ["row %d: %d vs %d cells" % (i, len(mr), len(dr))]
        for j, (a, b) in enumerate(zip(mr, dr)):
            colname_l = (mc[j] if j < len(mc) else "").lower()
            if colname_l == "cardinality":
                continue  # MySQL index estimate — server-specific by design
            if not vals_equal(a, b):
                colname = mc[j] if j < len(mc) else "?"
                return "DIFF-VALUE" + notes_cols, [
                    "row %d col %s: mysql=%r dna=%r" % (i, colname, a, b)]
    verdict = "MATCH (%d rows%s)" % (len(mrows), ", ordered" if ordered else "")
    if notes_cols:
        verdict += notes_cols
    return verdict, []


def shorten(s, n=72):
    s = " ".join(s.split())
    return s if len(s) <= n else s[:n - 1] + "…"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--mysql-host", default="127.0.0.1")
    ap.add_argument("--mysql-port", type=int, default=13306)
    ap.add_argument("--mysql-db", default="wptest")
    ap.add_argument("--dna-host", default="127.0.0.1")
    ap.add_argument("--dna-port", type=int, default=15432)
    ap.add_argument("--fixtures", default="cms/harness/fixtures_wp.sql")
    ap.add_argument("--corpus", default="docs/cms/wp_queries_corpus.txt")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    mysql = MySqlClient(args.mysql_host, args.mysql_port, args.mysql_db)
    dna = DnaClient(args.dna_host, args.dna_port)
    print("[1] clients connected (MariaDB %d, DNA-DB pgwire %d)"
          % (args.mysql_port, args.dna_port))

    # Phase 0: reset schema on both sides.
    for t in ALL_TABLES:
        mysql.query("DROP TABLE IF EXISTS %s" % t)
        dna.query("DROP TABLE IF EXISTS %s" % t)
    print("[2] schema reset on both sides")

    # Phase 1: fixture DDL (identical text).
    fixtures = load_fixtures(args.fixtures) + WOO_DDL
    setup_errors = []
    for stmt in fixtures:
        r = mysql.query(stmt)
        if not r["ok"]:
            setup_errors.append(("mysql", stmt[:60], r["message"][:110]))
        r = dna.query(stmt)
        if not r["ok"]:
            setup_errors.append(("dna", stmt[:60], r["message"][:110]))
    # Phase 2: seed data.
    for stmt in SEEDS:
        r = mysql.query(stmt)
        if not r["ok"]:
            setup_errors.append(("mysql-seed", stmt[:60], r["message"][:110]))
        r = dna.query(stmt)
        if not r["ok"]:
            setup_errors.append(("dna-seed", stmt[:60], r["message"][:110]))
    if setup_errors:
        print("[!] setup problems:")
        for side, stmt, msg in setup_errors:
            print("    %s: %s -> %s" % (side, stmt, msg))
    else:
        print("[3] fixtures + seeds applied cleanly on both sides")

    # Phase 3: corpus.
    corpus = parse_corpus(args.corpus)
    print("[4] corpus entries: %d" % len(corpus))

    results = []
    for i, (tag, sql) in enumerate(corpus, 1):
        table = None
        m_create = re.match(r"CREATE TABLE (\w+)", sql, re.I)
        if m_create:
            table = m_create.group(1)
        m = run(mysql, sql, table)
        d = run(dna, sql, table)
        verdict, notes = compare(tag, sql, m, d)
        results.append({"n": i, "tag": tag, "sql": sql, "verdict": verdict,
                        "notes": notes, "mysql": m, "dna": d})
        print("    [%02d %-8s] %s -> %s" % (i, tag, shorten(sql), verdict))

    # Report.
    buckets = {}
    for r in results:
        key = r["verdict"].split(" ")[0]
        buckets[key] = buckets.get(key, 0) + 1
    total = len(results)
    lines = []
    lines.append("# DNA-DB CMS Edition — дифференциальные тесты против MySQL (MariaDB 10.11.11)")
    lines.append("")
    lines.append("Методика: один и тот же корпус SQL WordPress/WooCommerce (`docs/cms/wp_queries_corpus.txt`),")
    lines.append("одни и те же фикстуры (12 таблиц WP-пресета + 2 таблицы WooCommerce/HPOS-lookup) и одни и те же")
    lines.append("данные подаются в MariaDB 10.11.11 (sql_mode='', как на WP-хостингах) и в DNA-DB CMS Edition")
    lines.append("(ветка `cms-edition`, `--features cms`, MySQL-dialect gate через pgwire, порт %d)." % args.dna_port)
    lines.append("Сравниваются: успех/ошибка, errno, колонки и значения строк (без ORDER BY — с сортировкой).")
    lines.append("")
    lines.append("## Сводка (%d запросов)" % total)
    lines.append("")
    lines.append("| Вердикт | Запросов |")
    lines.append("|---|---|")
    for k in sorted(buckets, key=lambda x: -buckets[x]):
        lines.append("| %s | %d |" % (k, buckets[k]))
    lines.append("")
    lines.append("## Детали по запросам")
    lines.append("")
    lines.append("| # | Метка | Запрос | Вердикт |")
    lines.append("|---|---|---|---|")
    for r in results:
        lines.append("| %d | %s | `%s` | %s |" % (r["n"], r["tag"], shorten(r["sql"], 64), r["verdict"]))
    lines.append("")
    lines.append("## Расхождения и ошибки")
    lines.append("")
    for r in results:
        if r["verdict"].startswith("MATCH") or r["verdict"].startswith("skip"):
            continue
        lines.append("### #%d %s — %s" % (r["n"], r["tag"], r["verdict"]))
        lines.append("")
        lines.append("```sql")
        lines.append(" ".join(r["sql"].split()))
        lines.append("```")
        for n in r["notes"]:
            lines.append("- %s" % n)
        if not r["mysql"]["ok"]:
            lines.append("- MySQL error: [%s] %s" % (r["mysql"]["errno"], r["mysql"]["message"]))
        if not r["dna"]["ok"]:
            lines.append("- DNA error: [%s/%s] %s" % (r["dna"]["errno"], r["dna"]["sqlstate"], r["dna"]["message"]))
        lines.append("")
    report = "\n".join(lines)
    with open(args.out, "w", encoding="utf-8") as f:
        f.write(report)
    print("[5] report written: %s" % args.out)
    print("    summary: %s" % ", ".join("%s=%d" % (k, v) for k, v in sorted(buckets.items(), key=lambda x: -x[1])))

    mysql.close()
    dna.close()


if __name__ == "__main__":
    main()
