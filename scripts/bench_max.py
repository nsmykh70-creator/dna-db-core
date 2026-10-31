#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Saturation benchmark: DNA-DB vs MySQL vs PostgreSQL.

This is deliberately separate from bench_compare.py: it answers a different
question.  bench_compare.py measures a fixed workload; this script increases
client concurrency until the observed aggregate throughput stops improving and
reports the best measured point before errors start.  It is not a claim about a
universal maximum: the result is bound to this host, dataset and query shape.

Measured phases per system:
  1. batch load of N rows (SQL path, durability on)
  2. point SELECT scaling: 1,2,4,8,16,32 client connections
  3. row UPDATE scaling: 1,2,4,8,16,32 client connections
  4. logical rows, on-disk bytes and bytes/row for the loaded table

DNA-DB is started with --sync strict.  --dnadb-tls/--dnadb-encryption select
the production transport/AEAD path; the default (legacy) is cleartext/no-AEAD
so the hardening overhead can be isolated.
"""

import argparse
import json
import os
import random
import shutil
import statistics
import tempfile
import threading
import time
from pathlib import Path

from bench_compare import (
    DnaDbClient,
    DnaDbAdapter,
    MySqlAdapter,
    PgAdapter,
    Server,
    free_port,
    find_binary,
)

PROJECT = str(Path(__file__).resolve().parent.parent)
PAYLOAD = "ACGT" * 40  # same 160-byte payload as bench_compare.py


def percentile(values, p):
    if not values:
        return 0.0
    values = sorted(values)
    idx = min(len(values) - 1, int(round((p / 100.0) * (len(values) - 1))))
    return values[idx] * 1000.0


def stats(lat, ops, wall, errors):
    return {
        "ops": ops,
        "secs": round(wall, 3),
        "ops_s": round(ops / wall) if wall else 0,
        "errors": errors,
        "p50_ms": round(percentile(lat, 50), 3),
        "p95_ms": round(percentile(lat, 95), 3),
        "p99_ms": round(percentile(lat, 99), 3),
    }


class DnaSqlBench:
    name = "DNA-DB"

    def __init__(self, adapter, data_dir, mode):
        self.adapter = adapter
        self.data_dir = data_dir
        self.mode = mode
        self.name = "DNA-DB (%s)" % mode

    def connect(self):
        return self.adapter.connect()

    def setup(self):
        c = self.connect()
        try:
            for ddl in (
                "CREATE TABLE maxkv (id INT, payload TEXT, PRIMARY KEY (id))",
                "CREATE TABLE maxupd (id INT, payload TEXT, PRIMARY KEY (id))",
            ):
                r = c.sql(ddl)
                if r.startswith(b"ERR"):
                    raise RuntimeError("DNA-DB DDL failed: %r" % r[:160])
        finally:
            c.close()

    def load_batch(self, start, count):
        values = ",".join("(%d,'%s')" % (i, PAYLOAD) for i in range(start, start + count))
        c = self.connect()
        try:
            r = c.sql("INSERT INTO maxkv (id,payload) VALUES %s" % values)
            if r.startswith(b"ERR"):
                raise RuntimeError("DNA-DB load failed: %r" % r[:200])
        finally:
            c.close()

    def read(self, conn, key):
        r = conn.sql("SELECT payload FROM maxkv WHERE id = %d" % key)
        if r.startswith(b"ERR"):
            raise RuntimeError(r[:160])
        data = json.loads(r)
        return data["rows"][0]["payload"]

    def update(self, conn, key, value):
        r = conn.sql("UPDATE maxkv SET payload = '%s' WHERE id = %d" % (value, key))
        if r.startswith(b"ERR"):
            raise RuntimeError(r[:160])

    def count(self):
        c = self.connect()
        try:
            data = json.loads(c.sql("SELECT COUNT(*) AS n FROM maxkv"))
            return int(data["rows"][0]["n"])
        finally:
            c.close()

    def storage_bytes(self):
        total = 0
        for root, _, files in os.walk(self.data_dir):
            for f in files:
                try:
                    total += os.path.getsize(os.path.join(root, f))
                except OSError:
                    pass
        return total


class MySqlBench:
    def __init__(self, adapter):
        self.adapter = adapter
        self.name = "MySQL"

    def connect(self):
        return self.adapter._connect()

    def setup(self):
        conn = self.connect()
        cur = conn.cursor()
        try:
            cur.execute("DROP TABLE IF EXISTS maxkv")
            cur.execute("DROP TABLE IF EXISTS maxupd")
            cur.execute("CREATE TABLE maxkv (id INT PRIMARY KEY, payload TEXT) "
                        "ENGINE=InnoDB")
            cur.execute("CREATE TABLE maxupd (id INT PRIMARY KEY, payload TEXT) "
                        "ENGINE=InnoDB")
        finally:
            cur.close()
            conn.close()

    def load_batch(self, start, count):
        values = ",".join("(%d,'%s')" % (i, PAYLOAD) for i in range(start, start + count))
        conn = self.connect()
        cur = conn.cursor()
        try:
            cur.execute("INSERT INTO maxkv (id,payload) VALUES %s" % values)
        finally:
            cur.close()
            conn.close()

    def read(self, conn, key):
        cur = conn.cursor()
        try:
            cur.execute("SELECT payload FROM maxkv WHERE id = %s", (key,))
            row = cur.fetchone()
            return row[0] if row else None
        finally:
            cur.close()

    def update(self, conn, key, value):
        cur = conn.cursor()
        try:
            cur.execute("UPDATE maxkv SET payload = %s WHERE id = %s", (value, key))
        finally:
            cur.close()

    def count(self):
        conn = self.connect()
        cur = conn.cursor()
        try:
            cur.execute("SELECT COUNT(*) FROM maxkv")
            return int(cur.fetchone()[0])
        finally:
            cur.close()
            conn.close()

    def storage_bytes(self):
        conn = self.connect()
        cur = conn.cursor()
        try:
            cur.execute("SELECT COALESCE(SUM(data_length + index_length),0) "
                        "FROM information_schema.tables "
                        "WHERE table_schema='bench' AND table_name='maxkv'")
            return int(cur.fetchone()[0])
        finally:
            cur.close()
            conn.close()


class PgBench:
    def __init__(self, adapter):
        self.adapter = adapter
        self.name = "PostgreSQL"

    def connect(self):
        return self.adapter._connect()

    def setup(self):
        conn = self.connect()
        cur = conn.cursor()
        try:
            cur.execute("DROP TABLE IF EXISTS maxkv")
            cur.execute("DROP TABLE IF EXISTS maxupd")
            cur.execute("CREATE TABLE maxkv (id INT PRIMARY KEY, payload TEXT)")
            cur.execute("CREATE TABLE maxupd (id INT PRIMARY KEY, payload TEXT)")
        finally:
            cur.close()
            conn.close()

    def load_batch(self, start, count):
        values = ",".join("(%d,'%s')" % (i, PAYLOAD) for i in range(start, start + count))
        conn = self.connect()
        cur = conn.cursor()
        try:
            cur.execute("INSERT INTO maxkv (id,payload) VALUES %s" % values)
        finally:
            cur.close()
            conn.close()

    def read(self, conn, key):
        cur = conn.cursor()
        try:
            cur.execute("SELECT payload FROM maxkv WHERE id = %s", (key,))
            row = cur.fetchone()
            return row[0] if row else None
        finally:
            cur.close()

    def update(self, conn, key, value):
        cur = conn.cursor()
        try:
            cur.execute("UPDATE maxkv SET payload = %s WHERE id = %s", (value, key))
        finally:
            cur.close()

    def count(self):
        conn = self.connect()
        cur = conn.cursor()
        try:
            cur.execute("SELECT COUNT(*) FROM maxkv")
            return int(cur.fetchone()[0])
        finally:
            cur.close()
            conn.close()

    def storage_bytes(self):
        conn = self.connect()
        cur = conn.cursor()
        try:
            cur.execute("SELECT pg_total_relation_size('maxkv')")
            return int(cur.fetchone()[0])
        finally:
            cur.close()
            conn.close()



class OracleBench:
    name = "Oracle Free 23ai"

    def __init__(self):
        import oracledb
        self.oracledb = oracledb

    def connect(self):
        c = self.oracledb.connect(user="demo", password="demo",
                                  dsn="127.0.0.1:1521/FREEPDB1")
        c.autocommit = True
        return c

    def setup(self):
        conn = self.connect()
        cur = conn.cursor()
        try:
            try:
                cur.execute("DROP TABLE maxkv PURGE")
            except Exception:
                pass
            cur.execute("CREATE TABLE maxkv (id NUMBER PRIMARY KEY, "
                        "payload VARCHAR2(160))")
        finally:
            cur.close()
            conn.close()

    def load_batch(self, start, count):
        values = ",".join("(%d,'%s')" % (i, PAYLOAD)
                          for i in range(start, start + count))
        conn = self.connect()
        cur = conn.cursor()
        try:
            cur.execute("INSERT INTO maxkv (id,payload) VALUES %s" % values)
        finally:
            cur.close()
            conn.close()

    def read(self, conn, key):
        cur = conn.cursor()
        try:
            cur.execute("SELECT payload FROM maxkv WHERE id = :1", (key,))
            row = cur.fetchone()
            return row[0] if row else None
        finally:
            cur.close()

    def update(self, conn, key, value):
        cur = conn.cursor()
        try:
            cur.execute("UPDATE maxkv SET payload = :1 WHERE id = :2",
                        (value[:160], key))
        finally:
            cur.close()

    def count(self):
        conn = self.connect()
        cur = conn.cursor()
        try:
            cur.execute("SELECT COUNT(*) FROM maxkv")
            return int(cur.fetchone()[0])
        finally:
            cur.close()
            conn.close()

    def storage_bytes(self):
        conn = self.connect()
        cur = conn.cursor()
        try:
            cur.execute("SELECT COALESCE(SUM(bytes),0) FROM user_segments "
                        "WHERE segment_name='MAXKV' OR segment_name IN "
                        "(SELECT index_name FROM user_indexes WHERE table_name='MAXKV')")
            return int(cur.fetchone()[0])
        finally:
            cur.close()
            conn.close()


def load_dataset(bench, rows, batch):
    print("  load %d rows, batch=%d ... " % (rows, batch), end="", flush=True)
    bench.setup()
    t0 = time.perf_counter()
    done = 0
    while done < rows:
        n = min(batch, rows - done)
        bench.load_batch(done, n)
        done += n
    wall = time.perf_counter() - t0
    got = bench.count()
    print("%d rows/s, %.1f s, verified=%s" % (rows / wall, wall, got == rows))
    return {
        "rows": rows,
        "secs": round(wall, 3),
        "rows_s": round(rows / wall),
        "verified": got == rows,
    }


def scale_clients(bench, rows, thread_counts, ops_per_thread, write, rng_seed):
    out = []
    for threads in thread_counts:
        keys = list(range(rows))
        lat = []
        errors = [0]
        lock = threading.Lock()

        def worker(tid):
            conn = bench.connect()
            rng = random.Random(rng_seed + tid)
            local = []
            err = 0
            try:
                for i in range(ops_per_thread):
                    if write:
                        # disjoint keys: no cross-client row-lock contention
                        key = keys[(tid * ops_per_thread + i) % rows]
                        val = "W%02d_%04d_%s" % (tid, i, PAYLOAD)
                        t = time.perf_counter()
                        try:
                            bench.update(conn, key, val)
                        except Exception:
                            err += 1
                        local.append(time.perf_counter() - t)
                    else:
                        key = keys[rng.randrange(rows)]
                        t = time.perf_counter()
                        try:
                            got = bench.read(conn, key)
                            if got != PAYLOAD:
                                err += 1
                        except Exception:
                            err += 1
                        local.append(time.perf_counter() - t)
            finally:
                conn.close()
            with lock:
                lat.extend(local)
                errors[0] += err

        t0 = time.perf_counter()
        ts = [threading.Thread(target=worker, args=(i,)) for i in range(threads)]
        for t in ts:
            t.start()
        for t in ts:
            t.join()
        wall = time.perf_counter() - t0
        ops = threads * ops_per_thread
        row = {"threads": threads, **stats(lat, ops, wall, errors[0])}
        out.append(row)
        print("    %2d clients: %6d ops/s, p50 %6.2f ms, p99 %7.2f ms, errors=%d"
              % (threads, row["ops_s"], row["p50_ms"], row["p99_ms"], row["errors"]))
    return out


def summarize(name, entry):
    reads = entry.get("read_scaling", [])
    writes = entry.get("write_scaling", [])
    best_r = max(reads, key=lambda x: x["ops_s"]) if reads else None
    best_w = max(writes, key=lambda x: x["ops_s"]) if writes else None
    entry["peak_read"] = best_r
    entry["peak_write"] = best_w
    entry["crash_observed"] = any(x["errors"] for x in reads + writes)
    print("\n  PEAK %s: read %s ops/s @ %s clients; write %s ops/s @ %s clients; errors=%s"
          % (name,
             best_r["ops_s"] if best_r else "n/a",
             best_r["threads"] if best_r else "n/a",
             best_w["ops_s"] if best_w else "n/a",
             best_w["threads"] if best_w else "n/a",
             entry["crash_observed"]))


def run_system(bench, rows, batch, thread_counts, read_ops, write_ops):
    print("\n=== %s ===" % bench.name)
    entry = {"name": bench.name}
    entry["load"] = load_dataset(bench, rows, batch)
    print("  point SELECT scaling (ops/client=%d)" % read_ops)
    entry["read_scaling"] = scale_clients(
        bench, rows, thread_counts, read_ops, False, 1234)
    print("  UPDATE scaling (ops/client=%d)" % write_ops)
    entry["write_scaling"] = scale_clients(
        bench, rows, thread_counts, write_ops, True, 5678)
    entry["storage_bytes"] = bench.storage_bytes()
    entry["bytes_per_row"] = round(entry["storage_bytes"] / rows, 2)
    print("  storage: %.1f MiB, %.2f bytes/row"
          % (entry["storage_bytes"] / 1048576.0, entry["bytes_per_row"]))
    summarize(bench.name, entry)
    return entry


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rows", type=int, default=1000000)
    ap.add_argument("--batch", type=int, default=200)
    ap.add_argument("--threads", default="1,2,4,8,16,32")
    ap.add_argument("--read-ops", type=int, default=1000,
                    help="point SELECTs per client")
    ap.add_argument("--write-ops", type=int, default=100,
                    help="UPDATEs per client")
    ap.add_argument("--systems", default="dnadb-production,dnadb-legacy,mysql,postgres",
                    help="comma-separated: dnadb-production,dnadb-legacy,mysql,postgres,oracle")
    ap.add_argument("--json-out", default=os.path.join(
        PROJECT, "docs", "bench_max_results.json"))
    args = ap.parse_args()
    thread_counts = [int(x) for x in args.threads.split(",") if x.strip()]
    systems = [x.strip() for x in args.systems.split(",") if x.strip()]
    results = {
        "meta": {
            "date": time.strftime("%Y-%m-%d %H:%M:%S"),
            "rows": args.rows,
            "batch": args.batch,
            "threads": thread_counts,
            "read_ops_per_client": args.read_ops,
            "write_ops_per_client": args.write_ops,
            "payload_bytes": len(PAYLOAD),
            "note": "Throughput saturation on this host; not a universal DB limit.",
        },
        "systems": {},
    }

    if "dnadb-production" in systems or "dnadb-legacy" in systems:
        for mode in ("production", "legacy"):
            key = "dnadb-" + mode
            if key not in systems:
                continue
            port = free_port()
            data_dir = tempfile.mkdtemp(prefix="dna_max_")
            production = mode == "production"
            srv = Server(data_dir, port, tls=production, encryption=production)
            try:
                srv.wait_ready()
                ad = DnaDbAdapter(port, cafile=srv.cert_path,
                                  mode="TLS+AEAD" if production else "legacy")
                bench = DnaSqlBench(ad, data_dir, "TLS+AEAD" if production else "legacy")
                results["systems"][key] = run_system(
                    bench, args.rows, args.batch, thread_counts,
                    args.read_ops, args.write_ops)
                ad.close()
            finally:
                srv.stop()
                shutil.rmtree(data_dir, ignore_errors=True)
            with open(args.json_out, "w", encoding="utf-8") as f:
                json.dump(results, f, indent=2, ensure_ascii=False)

    if "mysql" in systems:
        ad = MySqlAdapter()
        try:
            results["systems"]["mysql"] = run_system(
                MySqlBench(ad), args.rows, args.batch, thread_counts,
                args.read_ops, args.write_ops)
        finally:
            ad.close()

    if "postgres" in systems:
        ad = PgAdapter()
        try:
            results["systems"]["postgres"] = run_system(
                PgBench(ad), args.rows, args.batch, thread_counts,
                args.read_ops, args.write_ops)
        finally:
            ad.close()

    if "oracle" in systems:
        results["systems"]["oracle"] = run_system(
            OracleBench(), args.rows, args.batch, thread_counts,
            args.read_ops, args.write_ops)

    with open(args.json_out, "w", encoding="utf-8") as f:
        json.dump(results, f, indent=2, ensure_ascii=False)

    print("\n" + "=" * 86)
    print("%-26s %12s %10s %12s %10s" % (
        "system", "load rows/s", "read peak", "write peak", "bytes/row"))
    print("-" * 86)
    for entry in results["systems"].values():
        print("%-26s %12d %10s %12s %10.2f" % (
            entry["name"], entry["load"]["rows_s"],
            entry["peak_read"]["ops_s"] if entry.get("peak_read") else "n/a",
            entry["peak_write"]["ops_s"] if entry.get("peak_write") else "n/a",
            entry["bytes_per_row"]))
    print("=" * 86)
    print("JSON: %s" % args.json_out)


if __name__ == "__main__":
    main()
