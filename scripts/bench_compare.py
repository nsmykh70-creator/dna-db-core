#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Сравнительный бенчмарк: DNA-DB vs MySQL vs PostgreSQL.

Одинаковая нагрузка на всех трёх системах с локального клиента (Python),
локальный TCP, durability по умолчанию во всех системах:
  - DNA-DB    — serve --sync strict   (fsync WAL на каждый ack)
  - MySQL 8.4 — innodb_flush_log_at_trx_commit=1 (InnoDB, по умолчанию)
  - PG 17     — synchronous_commit=on (по умолчанию)

Фазы (одинаковые ключи/значения/последовательность RNG для всех систем):
  1. load       — N одиночных вставок (KV, autocommit/ack на каждую)
  2. point-read — R случайных чтений по ключу (p50/p95/p99, ops/s)
  3. mixed      — T потоков, 80% чтения / 20% записи, агрегированная ops/s
  4. sql        — реляционная мини-нагрузка: INSERT + SELECT WHERE pk

Результаты: docs/bench_compare_results.json + таблица в stdout.
"""

import argparse
import json
import os
import random
import shutil
import socket
import statistics
import struct
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
PROJECT = os.path.dirname(HERE)

# ---------------------------------------------------------------------------
# DNA-DB binary protocol client (docs/PROTOCOL.md)
# ---------------------------------------------------------------------------

class DnaDbClient:
    def __init__(self, host, port, user, password):
        self.sock = socket.create_connection((host, port), timeout=120)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self._send(b"\x01" + user.encode() + b":" + password.encode())
        if self._recv_exact(2) != b"OK":
            raise RuntimeError("DNA-DB auth failed")

    def _send(self, payload):
        self.sock.sendall(struct.pack("<I", len(payload)) + payload)

    def _recv_exact(self, n):
        buf = b""
        while len(buf) < n:
            chunk = self.sock.recv(n - len(buf))
            if not chunk:
                raise RuntimeError("connection closed")
            buf += chunk
        return buf

    def _recv_frame(self):
        (length,) = struct.unpack("<I", self._recv_exact(4))
        return self._recv_exact(length) if length else b""

    def put(self, key, value):
        payload = b"\x03" + struct.pack("<H", len(key)) + key + value
        self._send(payload)
        if self._recv_exact(2) != b"OK":
            raise RuntimeError("PUT failed")

    def get(self, key):
        self._send(b"\x02" + key)
        return self._recv_frame()

    def sql(self, query):
        self._send(b"\x11" + query.encode())
        return self._recv_frame()

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


# ---------------------------------------------------------------------------
# Server lifecycle
# ---------------------------------------------------------------------------

def find_binary():
    name = "dna-db-core.exe" if os.name == "nt" else "dna-db-core"
    for cand in (os.path.join(PROJECT, "target", "release", name),
                 os.path.join(PROJECT, "prebuilt", "linux-x86_64", "dna-db-core")):
        if os.path.exists(cand):
            return cand
    raise RuntimeError("dna-db-core binary not found; run cargo build --release")


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


class Server:
    def __init__(self, data_dir, port):
        self.port = port
        self.log_path = os.path.join(tempfile.gettempdir(), "dna_bench_srv.log")
        self.log = open(self.log_path, "ab")
        self.proc = subprocess.Popen(
            [find_binary(), "serve",
             "--data-dir", data_dir,
             "--port", str(port),
             "--bind", "127.0.0.1",
             "--admin-pass", "bench-pass",
             "--sync", "strict",
             "--no-scrub"],
            stdout=self.log, stderr=self.log, cwd=PROJECT)

    def wait_ready(self, timeout=30):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError("DNA-DB died on startup, see %s" % self.log_path)
            try:
                DnaDbClient("127.0.0.1", self.port, "admin", "bench-pass").close()
                return
            except OSError:
                time.sleep(0.05)
        raise RuntimeError("DNA-DB did not become ready")

    def stop(self):
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.log.close()


# ---------------------------------------------------------------------------
# Stats helpers
# ---------------------------------------------------------------------------

def pct(sorted_vals, q):
    if not sorted_vals:
        return 0.0
    idx = min(len(sorted_vals) - 1, int(round(q * (len(sorted_vals) - 1))))
    return sorted_vals[idx]


def latency_stats(lat_s):
    s = sorted(lat_s)
    return {
        "ops": len(s),
        "avg_ms": round(statistics.fmean(s) * 1000, 3) if s else 0.0,
        "p50_ms": round(pct(s, 0.50) * 1000, 3),
        "p95_ms": round(pct(s, 0.95) * 1000, 3),
        "p99_ms": round(pct(s, 0.99) * 1000, 3),
    }


def make_dataset(n):
    """Deterministic keys/values shared by all systems."""
    rng = random.Random(42)
    keys = [("chr1:%07d" % i).encode() for i in range(n)]
    value = ("ACTG" * 40).encode()  # 160 bytes
    return rng, keys, value


# ---------------------------------------------------------------------------
# Adapters: общий интерфейс — setup, load, point_read, mixed, sql
# ---------------------------------------------------------------------------

class DnaDbAdapter:
    name = "DNA-DB v3.0"
    version = "dna-db-core 3.0.0 (strict fsync)"

    def __init__(self, port):
        self.port = port

    def connect(self):
        return DnaDbClient("127.0.0.1", self.port, "admin", "bench-pass")

    def setup(self):
        # fresh server, fresh storage — only the SQL demo table needs creating
        c = self.connect()
        resp = c.sql("CREATE TABLE bench_users (user_id INT, uname TEXT, "
                     "balance FLOAT, PRIMARY KEY (user_id))")
        if resp.startswith(b"ERR"):
            raise RuntimeError("dnadb CREATE TABLE failed: %r" % resp[:200])
        c.close()

    def load(self, keys, value, rng):
        c = self.connect()
        lat = []
        t0 = time.perf_counter()
        for k in keys:
            t = time.perf_counter()
            c.put(k, value)
            lat.append(time.perf_counter() - t)
        total = time.perf_counter() - t0
        c.close()
        return {"ops": len(keys), "secs": round(total, 3),
                "ops_s": round(len(keys) / total),
                **latency_stats(lat)}

    def point_read(self, keys, n_reads, rng, value):
        c = self.connect()
        lat, errors = [], 0
        t0 = time.perf_counter()
        for _ in range(n_reads):
            k = keys[rng.randrange(len(keys))]
            t = time.perf_counter()
            got = c.get(k)
            lat.append(time.perf_counter() - t)
            if got != value:
                errors += 1
        total = time.perf_counter() - t0
        c.close()
        return {"ops": n_reads, "secs": round(total, 3),
                "ops_s": round(n_reads / total), "errors": errors,
                **latency_stats(lat)}

    def mixed(self, keys, value, threads, ops_per_thread, rng_seed):
        results = []
        def worker(seed):
            rng = random.Random(seed)
            c = self.connect()
            lat = []
            for i in range(ops_per_thread):
                k = keys[rng.randrange(len(keys))]
                t = time.perf_counter()
                if rng.random() < 0.8:
                    c.get(k)
                else:
                    c.put(k, value)
                lat.append(time.perf_counter() - t)
            c.close()
            results.extend(lat)
        t0 = time.perf_counter()
        ts = [threading.Thread(target=worker, args=(rng_seed + i,))
              for i in range(threads)]
        for t in ts:
            t.start()
        for t in ts:
            t.join()
        total = time.perf_counter() - t0
        ops = threads * ops_per_thread
        return {"ops": ops, "secs": round(total, 3),
                "ops_s": round(ops / total), "threads": threads,
                **latency_stats(results)}

    def sql_load(self, rows):
        c = self.connect()
        lat = []
        t0 = time.perf_counter()
        for i in range(rows):
            q = ("INSERT INTO bench_users (user_id, uname, balance) "
                 "VALUES (%d, 'user_%d', %.2f)" % (i, i, (i * 3.7) % 10000 / 100))
            t = time.perf_counter()
            resp = c.sql(q)
            lat.append(time.perf_counter() - t)
            if resp.startswith(b"ERR"):
                raise RuntimeError("dnadb SQL INSERT failed: %r" % resp[:200])
        total = time.perf_counter() - t0
        c.close()
        return {"ops": rows, "secs": round(total, 3),
                "ops_s": round(rows / total), **latency_stats(lat)}

    def sql_point_query(self, rows, n_q, rng):
        c = self.connect()
        lat, errors = [], 0
        t0 = time.perf_counter()
        for _ in range(n_q):
            i = rng.randrange(rows)
            q = ("SELECT uname, balance FROM bench_users WHERE user_id = %d" % i)
            t = time.perf_counter()
            resp = c.sql(q)
            lat.append(time.perf_counter() - t)
            if resp.startswith(b"ERR"):
                errors += 1
                continue
            try:
                data = json.loads(resp)
                expect = (i * 3.7) % 10000 / 100
                if not data.get("rows"):
                    errors += 1
                else:
                    got = float(data["rows"][0].get("balance", -1))
                    if abs(got - expect) > 0.01:
                        errors += 1
            except Exception:
                errors += 1
        total = time.perf_counter() - t0
        c.close()
        return {"ops": n_q, "secs": round(total, 3),
                "ops_s": round(n_q / total), "errors": errors,
                **latency_stats(lat)}

    def close(self):
        pass


class MySqlAdapter:
    name = "MySQL"
    version = "MySQL 8.4.11 (InnoDB, flush_log_at_trx_commit=1)"

    def __init__(self):
        import pymysql
        self.pymysql = pymysql
        self.conn = pymysql.connect(host="127.0.0.1", port=3306, user="root",
                                    password="bench", database="bench",
                                    autocommit=True)

    def _cursor(self):
        return self.conn.cursor()

    def load(self, keys, value, rng):
        lat = []
        cur = self._cursor()
        t0 = time.perf_counter()
        for k in keys:
            t = time.perf_counter()
            cur.execute("INSERT INTO kvbench (k, v) VALUES (%s, %s)", (k, value))
            lat.append(time.perf_counter() - t)
        total = time.perf_counter() - t0
        cur.close()
        return {"ops": len(keys), "secs": round(total, 3),
                "ops_s": round(len(keys) / total), **latency_stats(lat)}

    def point_read(self, keys, n_reads, rng, value):
        lat, errors = [], 0
        cur = self._cursor()
        t0 = time.perf_counter()
        for _ in range(n_reads):
            k = keys[rng.randrange(len(keys))]
            t = time.perf_counter()
            cur.execute("SELECT v FROM kvbench WHERE k = %s", (k,))
            row = cur.fetchone()
            lat.append(time.perf_counter() - t)
            if row is None or row[0] != value:
                errors += 1
        total = time.perf_counter() - t0
        cur.close()
        return {"ops": n_reads, "secs": round(total, 3),
                "ops_s": round(n_reads / total), "errors": errors,
                **latency_stats(lat)}

    def mixed(self, keys, value, threads, ops_per_thread, rng_seed):
        results = []
        def worker(seed):
            rng = random.Random(seed)
            conn = self.pymysql.connect(host="127.0.0.1", port=3306, user="root",
                                        password="bench", database="bench",
                                        autocommit=True)
            cur = conn.cursor()
            lat = []
            for i in range(ops_per_thread):
                k = keys[rng.randrange(len(keys))]
                t = time.perf_counter()
                if rng.random() < 0.8:
                    cur.execute("SELECT v FROM kvbench WHERE k = %s", (k,))
                    cur.fetchone()
                else:
                    cur.execute("INSERT INTO kvbench (k, v) VALUES (%s, %s) "
                                "ON DUPLICATE KEY UPDATE v = VALUES(v)", (k, value))
                lat.append(time.perf_counter() - t)
            cur.close()
            conn.close()
            results.extend(lat)
        t0 = time.perf_counter()
        ts = [threading.Thread(target=worker, args=(rng_seed + i,))
              for i in range(threads)]
        for t in ts:
            t.start()
        for t in ts:
            t.join()
        total = time.perf_counter() - t0
        ops = threads * ops_per_thread
        return {"ops": ops, "secs": round(total, 3),
                "ops_s": round(ops / total), "threads": threads,
                **latency_stats(results)}

    def sql_load(self, rows):
        lat = []
        cur = self._cursor()
        t0 = time.perf_counter()
        for i in range(rows):
            t = time.perf_counter()
            cur.execute("INSERT INTO bench_users (user_id, uname, balance) "
                        "VALUES (%s, %s, %s)",
                        (i, "user_%d" % i, (i * 3.7) % 10000 / 100))
            lat.append(time.perf_counter() - t)
        total = time.perf_counter() - t0
        cur.close()
        return {"ops": rows, "secs": round(total, 3),
                "ops_s": round(rows / total), **latency_stats(lat)}

    def sql_point_query(self, rows, n_q, rng):
        lat, errors = [], 0
        cur = self._cursor()
        t0 = time.perf_counter()
        for _ in range(n_q):
            i = rng.randrange(rows)
            t = time.perf_counter()
            cur.execute("SELECT uname, balance FROM bench_users WHERE user_id = %s",
                        (i,))
            row = cur.fetchone()
            lat.append(time.perf_counter() - t)
            if row is None:
                errors += 1
                continue
            expect = (i * 3.7) % 10000 / 100
            if row[0] != "user_%d" % i or abs(float(row[1]) - expect) > 0.01:
                errors += 1
        total = time.perf_counter() - t0
        cur.close()
        return {"ops": n_q, "secs": round(total, 3),
                "ops_s": round(n_q / total), "errors": errors,
                **latency_stats(lat)}

    def setup(self):
        cur = self._cursor()
        cur.execute("DROP TABLE IF EXISTS kvbench")
        cur.execute("DROP TABLE IF EXISTS bench_users")
        cur.execute("CREATE TABLE kvbench (k VARCHAR(64) CHARACTER SET ascii "
                    "PRIMARY KEY, v VARBINARY(512)) ENGINE=InnoDB")
        cur.execute("CREATE TABLE bench_users (user_id INT PRIMARY KEY, "
                    "uname VARCHAR(64), balance DOUBLE) ENGINE=InnoDB")
        cur.close()

    def close(self):
        self.conn.close()


class PgAdapter:
    name = "PostgreSQL"
    version = "PostgreSQL 17.11 (synchronous_commit=on)"

    def __init__(self):
        import psycopg2
        self.psycopg2 = psycopg2
        self.conn = psycopg2.connect(host="127.0.0.1", port=5432, user="bench",
                                     password="bench", dbname="bench")
        self.conn.autocommit = True

    def load(self, keys, value, rng):
        lat = []
        cur = self.conn.cursor()
        t0 = time.perf_counter()
        for k in keys:
            t = time.perf_counter()
            cur.execute("INSERT INTO kvbench (k, v) VALUES (%s, %s)",
                        (k.decode(), value))
            lat.append(time.perf_counter() - t)
        total = time.perf_counter() - t0
        cur.close()
        return {"ops": len(keys), "secs": round(total, 3),
                "ops_s": round(len(keys) / total), **latency_stats(lat)}

    def point_read(self, keys, n_reads, rng, value):
        lat, errors = [], 0
        cur = self.conn.cursor()
        t0 = time.perf_counter()
        for _ in range(n_reads):
            k = keys[rng.randrange(len(keys))].decode()
            t = time.perf_counter()
            cur.execute("SELECT v FROM kvbench WHERE k = %s", (k,))
            row = cur.fetchone()
            lat.append(time.perf_counter() - t)
            if row is None or bytes(row[0]) != value:
                errors += 1
        total = time.perf_counter() - t0
        cur.close()
        return {"ops": n_reads, "secs": round(total, 3),
                "ops_s": round(n_reads / total), "errors": errors,
                **latency_stats(lat)}

    def mixed(self, keys, value, threads, ops_per_thread, rng_seed):
        results = []
        def worker(seed):
            rng = random.Random(seed)
            conn = self.psycopg2.connect(host="127.0.0.1", port=5432, user="bench",
                                         password="bench", dbname="bench")
            conn.autocommit = True
            cur = conn.cursor()
            lat = []
            for i in range(ops_per_thread):
                k = keys[rng.randrange(len(keys))].decode()
                t = time.perf_counter()
                if rng.random() < 0.8:
                    cur.execute("SELECT v FROM kvbench WHERE k = %s", (k,))
                    cur.fetchone()
                else:
                    cur.execute("INSERT INTO kvbench (k, v) VALUES (%s, %s) "
                                "ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v",
                                (k, value))
                lat.append(time.perf_counter() - t)
            cur.close()
            conn.close()
            results.extend(lat)
        t0 = time.perf_counter()
        ts = [threading.Thread(target=worker, args=(rng_seed + i,))
              for i in range(threads)]
        for t in ts:
            t.start()
        for t in ts:
            t.join()
        total = time.perf_counter() - t0
        ops = threads * ops_per_thread
        return {"ops": ops, "secs": round(total, 3),
                "ops_s": round(ops / total), "threads": threads,
                **latency_stats(results)}

    def sql_load(self, rows):
        lat = []
        cur = self.conn.cursor()
        t0 = time.perf_counter()
        for i in range(rows):
            t = time.perf_counter()
            cur.execute("INSERT INTO bench_users (user_id, uname, balance) "
                        "VALUES (%s, %s, %s)",
                        (i, "user_%d" % i, (i * 3.7) % 10000 / 100))
            lat.append(time.perf_counter() - t)
        total = time.perf_counter() - t0
        cur.close()
        return {"ops": rows, "secs": round(total, 3),
                "ops_s": round(rows / total), **latency_stats(lat)}

    def sql_point_query(self, rows, n_q, rng):
        lat, errors = [], 0
        cur = self.conn.cursor()
        t0 = time.perf_counter()
        for _ in range(n_q):
            i = rng.randrange(rows)
            t = time.perf_counter()
            cur.execute("SELECT uname, balance FROM bench_users WHERE user_id = %s",
                        (i,))
            row = cur.fetchone()
            lat.append(time.perf_counter() - t)
            if row is None:
                errors += 1
                continue
            expect = (i * 3.7) % 10000 / 100
            if row[0] != "user_%d" % i or abs(float(row[1]) - expect) > 0.01:
                errors += 1
        total = time.perf_counter() - t0
        cur.close()
        return {"ops": n_q, "secs": round(total, 3),
                "ops_s": round(n_q / total), "errors": errors,
                **latency_stats(lat)}

    def setup(self):
        cur = self.conn.cursor()
        cur.execute("DROP TABLE IF EXISTS kvbench")
        cur.execute("DROP TABLE IF EXISTS bench_users")
        cur.execute("CREATE TABLE kvbench (k TEXT PRIMARY KEY, v BYTEA)")
        cur.execute("CREATE TABLE bench_users (user_id INT PRIMARY KEY, "
                    "uname TEXT, balance DOUBLE PRECISION)")
        cur.close()

    def close(self):
        self.conn.close()


# ---------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------

def run_adapter(adapter, args, out):
    print("\n=== %s ===" % adapter.name)
    adapter.setup()
    rng, keys, value = make_dataset(args.load)

    print("  load %d ... " % args.load, end="", flush=True)
    r = adapter.load(keys, value, rng)
    out["load"] = r
    print("%d ops/s, avg %.2f ms" % (r["ops_s"], r["avg_ms"]))

    print("  point-read %d ... " % args.reads, end="", flush=True)
    rng2, _, _ = make_dataset(args.load)  # same key order, fresh RNG
    r = adapter.point_read(keys, args.reads, rng2, value)
    out["point_read"] = r
    print("%d ops/s, p50 %.2f / p95 %.2f / p99 %.2f ms, errors=%d"
          % (r["ops_s"], r["p50_ms"], r["p95_ms"], r["p99_ms"], r.get("errors", 0)))

    print("  mixed %d threads x %d ops (80/20) ... "
          % (args.threads, args.mixed_ops), end="", flush=True)
    r = adapter.mixed(keys, value, args.threads, args.mixed_ops, seed_base)
    out["mixed"] = r
    print("%d ops/s, p50 %.2f / p99 %.2f ms"
          % (r["ops_s"], r["p50_ms"], r["p99_ms"]))

    print("  sql load %d rows ... " % args.sql_rows, end="", flush=True)
    r = adapter.sql_load(args.sql_rows)
    out["sql_load"] = r
    print("%d ops/s, avg %.2f ms" % (r["ops_s"], r["avg_ms"]))

    print("  sql SELECT WHERE pk x %d ... " % args.sql_queries, end="", flush=True)
    rng3 = random.Random(42)
    r = adapter.sql_point_query(args.sql_rows, args.sql_queries, rng3)
    out["sql_point_query"] = r
    print("%d ops/s, p50 %.2f / p95 %.2f / p99 %.2f ms, errors=%d"
          % (r["ops_s"], r["p50_ms"], r["p95_ms"], r["p99_ms"], r.get("errors", 0)))


def main():
    global seed_base
    ap = argparse.ArgumentParser(description="DNA-DB vs MySQL vs PostgreSQL")
    ap.add_argument("--load", type=int, default=10000)
    ap.add_argument("--reads", type=int, default=4000)
    ap.add_argument("--threads", type=int, default=8)
    ap.add_argument("--mixed-ops", type=int, default=500)
    ap.add_argument("--sql-rows", type=int, default=3000)
    ap.add_argument("--sql-queries", type=int, default=1500)
    ap.add_argument("--json-out", default=os.path.join(
        PROJECT, "docs", "bench_compare_results.json"))
    ap.add_argument("--skip-dnadb", action="store_true")
    ap.add_argument("--skip-sql", action="store_true")
    args = ap.parse_args()
    seed_base = 777

    results = {
        "meta": {
            "date": time.strftime("%Y-%m-%d %H:%M:%S"),
            "machine": "%s | %s" % (os.name, platform_string()),
            "params": {"load": args.load, "reads": args.reads,
                       "threads": args.threads, "mixed_ops": args.mixed_ops,
                       "sql_rows": args.sql_rows, "sql_queries": args.sql_queries},
            "durability": {
                "dnadb": "--sync strict (fsync WAL per ack)",
                "mysql": "innodb_flush_log_at_trx_commit=1 (default)",
                "postgres": "synchronous_commit=on (default)",
            },
        },
        "systems": {},
    }

    # DNA-DB (собственный сервер)
    if not args.skip_dnadb:
        port = free_port()
        data_dir = tempfile.mkdtemp(prefix="dna_bench_")
        srv = Server(data_dir, port)
        try:
            srv.wait_ready()
            ad = DnaDbAdapter(port)
            results["systems"]["dnadb"] = {"name": ad.name, "version": ad.version}
            run_adapter(ad, args, results["systems"]["dnadb"])
            ad.close()
            dump_json(args.json_out, results)
        finally:
            srv.stop()
            shutil.rmtree(data_dir, ignore_errors=True)

    # MySQL
    ad = MySqlAdapter()
    results["systems"]["mysql"] = {"name": ad.name, "version": ad.version}
    run_adapter(ad, args, results["systems"]["mysql"])
    ad.close()
    dump_json(args.json_out, results)

    # PostgreSQL
    ad = PgAdapter()
    results["systems"]["postgres"] = {"name": ad.name, "version": ad.version}
    run_adapter(ad, args, results["systems"]["postgres"])
    ad.close()

    dump_json(args.json_out, results)
    print("\nJSON report: %s" % args.json_out)

    # Сводная таблица
    print("\n" + "=" * 78)
    print("%-28s %10s %10s %10s %10s" %
          ("system / metric", "load/s", "read/s", "mixed/s", "sql sel/s"))
    print("-" * 78)
    for key, label in (("dnadb", "DNA-DB"), ("mysql", "MySQL"),
                       ("postgres", "PostgreSQL")):
        s = results["systems"].get(key)
        if not s:
            continue
        print("%-28s %10d %10d %10d %10d" % (
            label,
            s.get("load", {}).get("ops_s", 0),
            s.get("point_read", {}).get("ops_s", 0),
            s.get("mixed", {}).get("ops_s", 0),
            s.get("sql_point_query", {}).get("ops_s", 0)))
    print("=" * 78)


def dump_json(path, results):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        json.dump(results, f, indent=2, ensure_ascii=False)


def platform_string():
    try:
        import platform
        return platform.platform()
    except Exception:
        return "unknown"


if __name__ == "__main__":
    main()
