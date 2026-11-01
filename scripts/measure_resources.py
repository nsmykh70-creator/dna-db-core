#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Измерение потребления RAM и места на диске: DNA-DB vs MySQL vs PostgreSQL.

Одинаковая нагрузка и методика для всех трёх систем, настройки по умолчанию:

  1. Сервер запущен с настройками по умолчанию, схема создана.
  2. RAM в покое: сумма PSS/RSS всех процессов СУБД сразу после setup
     (PSS — proportional set size из /proc/*/smaps_rollup: общая память
     делится между процессами пропорционально, поэтому PostgreSQL
     (shared_buffers) учитывается честно; для одиночного процесса
     DNA-DB PSS == RSS).
  3. Нагрузка: та же детерминированная KV-нагрузка, что в bench_compare.py —
     N одиночных вставок (autocommit/ack на каждую, durability по умолчанию)
     + R точечных чтений. Фоновый поток каждые 50 мс суммирует PSS всех
     процессов СУБД; пик суммарного PSS фиксируется как потребление под нагрузкой.
  4. Корректная остановка сервера (все буферы сброшены на диск).
  5. Диск: размер каталога данных после остановки МИНУС размер каталога
     до создания базы (после чистого старта сервера). Для MySQL в каталог
     попадают и binlog, для PostgreSQL — WAL, для DNA-DB — WAL и сегменты:
     сравнивается полный прирост занятого места каталогом каждой СУБД.

Результаты: docs/resources_compare_linux.json + таблица в stdout.
Скрипт должен запускаться с правами root (sudo): нужны чтение
/proc/<pid>/smaps_rollup чужих процессов и остановка системных служб.
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
if HERE not in sys.path:
    sys.path.insert(0, HERE)

import bench_compare as bc  # noqa: E402


# ---------------------------------------------------------------------------
# /proc helpers
# ---------------------------------------------------------------------------

def read_pid_mem_kb(pid):
    """(pss_kb, rss_kb) of one process; (0, 0) if it vanished."""
    pss = rss = 0
    try:
        with open("/proc/%d/smaps_rollup" % pid) as f:
            for line in f:
                if line.startswith("Pss:"):
                    pss = int(line.split()[1])
                elif line.startswith("Rss:"):
                    rss = int(line.split()[1])
    except OSError:
        pass
    return pss, rss


def du_bytes(path):
    r = subprocess.run(["du", "-sb", path], capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError("du failed on %s: %s" % (path, r.stderr.strip()))
    return int(r.stdout.split()[0])


def mib(b):
    return round(b / (1024 * 1024), 1)


class ProcSet:
    """Discovers the server processes of one system."""

    def __init__(self, system, server_proc=None):
        self.system = system
        self.server_proc = server_proc

    def pids(self):
        if self.system == "dnadb":
            return [self.server_proc.pid] if self.server_proc else []
        if self.system == "mysql":
            r = subprocess.run(["pgrep", "-x", "mysqld"],
                               capture_output=True, text=True)
            return [int(x) for x in r.stdout.split()]
        # postgres: postmaster + backends ("postgres: ...")
        r = subprocess.run(["pgrep", "-f", "postgres"],
                           capture_output=True, text=True)
        out = []
        for x in r.stdout.split():
            try:
                with open("/proc/%s/cmdline" % x, "rb") as f:
                    if b"postgres" in f.read():
                        out.append(int(x))
            except OSError:
                pass
        return out


class MemSampler:
    """Samples the summed PSS/RSS of a ProcSet in the background."""

    def __init__(self, procset, interval=0.05):
        self.procset = procset
        self.interval = interval
        self.peak_pss = 0
        self.peak_rss = 0
        self.cur_pss = 0
        self.cur_rss = 0
        self._stop = threading.Event()
        self._t = None

    def _loop(self):
        while not self._stop.is_set():
            pss = rss = 0
            for pid in self.procset.pids():
                p, r = read_pid_mem_kb(pid)
                pss += p
                rss += r
            self.cur_pss, self.cur_rss = pss, rss
            self.peak_pss = max(self.peak_pss, pss)
            self.peak_rss = max(self.peak_rss, rss)
            self._stop.wait(self.interval)

    def start(self):
        self._t = threading.Thread(target=self._loop, daemon=True)
        self._t.start()

    def settle(self, secs=2.0):
        """Let the sampler run and return the CURRENT (idle) sums."""
        time.sleep(secs)
        return self.cur_pss, self.cur_rss

    def stop(self):
        self._stop.set()
        if self._t:
            self._t.join(timeout=2)


# ---------------------------------------------------------------------------
# Per-system measurement
# ---------------------------------------------------------------------------

def stop_service(name):
    subprocess.run(["systemctl", "stop", name], check=False)
    time.sleep(2)


def check_count(n_expected, n_actual, what):
    if n_expected != n_actual:
        raise RuntimeError("row count mismatch for %s: %d != %d"
                           % (what, n_actual, n_expected))


def check_stats(stats, what):
    if stats.get("errors"):
        raise RuntimeError("%s reported %d errors" % (what, stats["errors"]))


def measure_dnadb(out, rows, reads):
    data_dir = tempfile.mkdtemp(prefix="dnadb_res_", dir=os.path.expanduser("~"))
    port = bc.free_port()
    server = bc.Server(data_dir, port)
    server.wait_ready()
    procset = ProcSet("dnadb", server.proc)
    sampler = MemSampler(procset)
    sampler.start()
    idle_pss, idle_rss = sampler.settle(2.0)

    adapter = bc.DnaDbAdapter(port)
    t0 = time.perf_counter()
    adapter.setup()
    setup_secs = time.perf_counter() - t0
    rng, keys, value = bc.make_dataset(rows)
    load = adapter.load(keys, value, rng)
    read = adapter.point_read(keys, reads, rng, value)

    # spot-check: 100 random keys must return the exact value
    for _ in range(100):
        k = keys[rng.randrange(rows)]
        c = adapter.connect()
        v = c.get(k)
        c.close()
        if v != value:
            raise RuntimeError("DNA-DB spot-check failed for %r" % k)

    check_stats(load, "dnadb.load")
    check_stats(read, "dnadb.point_read")
    sampler.stop()
    peak_pss, peak_rss = sampler.peak_pss, sampler.peak_rss
    server.stop()
    time.sleep(1)
    # an empty data dir held 0 bytes before the server started
    disk_after = du_bytes(data_dir)
    out["dnadb"] = {
        "version": adapter.version,
        "idle_pss_mib": mib(idle_pss * 1024),
        "peak_pss_mib": mib(peak_pss * 1024),
        "idle_rss_mib": mib(idle_rss * 1024),
        "peak_rss_mib": mib(peak_rss * 1024),
        "disk_baseline_mib": 0.0,
        "disk_after_mib": mib(disk_after),
        "disk_delta_mib": mib(disk_after),
        "setup_secs": round(setup_secs, 2),
        "load_secs": load["secs"], "load_ops_s": load["ops_s"],
        "read_ops_s": read["ops_s"],
    }
    print("  [dnadb] idle %.1f MiB | peak %.1f MiB | disk %.1f MiB"
          % (out["dnadb"]["idle_pss_mib"], out["dnadb"]["peak_pss_mib"],
             out["dnadb"]["disk_delta_mib"]))
    return data_dir


def measure_mysql(out, rows, reads):
    import pymysql
    # root/bench configured by the CI install step; no bench database yet
    conn = pymysql.connect(host="127.0.0.1", port=3306, user="root",
                           password="bench", autocommit=True)
    cur = conn.cursor()
    cur.execute("SELECT VERSION()")
    version = "MySQL %s" % cur.fetchone()[0]
    cur.execute("CREATE DATABASE IF NOT EXISTS bench")
    cur.close()
    conn.close()

    data_dir = "/var/lib/mysql"
    disk_base = du_bytes(data_dir)
    procset = ProcSet("mysql")
    sampler = MemSampler(procset)
    sampler.start()
    idle_pss, idle_rss = sampler.settle(2.0)

    adapter = bc.MySqlAdapter()
    t0 = time.perf_counter()
    adapter.setup()
    setup_secs = time.perf_counter() - t0
    rng, keys, value = bc.make_dataset(rows)
    load = adapter.load(keys, value, rng)
    read = adapter.point_read(keys, reads, rng, value)
    check_count(rows, adapter._count("kvbench"), "mysql.kvbench")
    check_stats(load, "mysql.load")
    check_stats(read, "mysql.point_read")
    sampler.stop()
    peak_pss, peak_rss = sampler.peak_pss, sampler.peak_rss
    adapter.close()

    stop_service("mysql")
    disk_after = du_bytes(data_dir)
    out["mysql"] = {
        "version": version,
        "idle_pss_mib": mib(idle_pss * 1024),
        "peak_pss_mib": mib(peak_pss * 1024),
        "idle_rss_mib": mib(idle_rss * 1024),
        "peak_rss_mib": mib(peak_rss * 1024),
        "disk_baseline_mib": mib(disk_base),
        "disk_after_mib": mib(disk_after),
        "disk_delta_mib": mib(disk_after - disk_base),
        "setup_secs": round(setup_secs, 2),
        "load_secs": load["secs"], "load_ops_s": load["ops_s"],
        "read_ops_s": read["ops_s"],
    }
    print("  [mysql] idle %.1f MiB | peak %.1f MiB | disk +%.1f MiB"
          % (out["mysql"]["idle_pss_mib"], out["mysql"]["peak_pss_mib"],
             out["mysql"]["disk_delta_mib"]))


def psql(sql):
    r = subprocess.run(["sudo", "-u", "postgres", "psql", "-tAc", sql],
                       capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError("psql failed: %s" % r.stderr.strip())
    return r.stdout.strip()


def measure_postgres(out, rows, reads):
    # postgres peer-auth via unix socket (TCP has no password for 'postgres')
    version = psql("SELECT version()").split(",")[0]
    if psql("SELECT 1 FROM pg_database WHERE datname='bench'") != "1":
        r = subprocess.run(["sudo", "-u", "postgres", "psql", "-c",
                            "CREATE DATABASE bench OWNER bench"],
                           capture_output=True, text=True)
        if r.returncode != 0:
            raise RuntimeError("CREATE DATABASE failed: %s" % r.stderr.strip())

    data_dir = None
    for cand in sorted(os.listdir("/var/lib/postgresql")):
        p = os.path.join("/var/lib/postgresql", cand, "main")
        if os.path.isdir(p):
            data_dir = p
    if data_dir is None:
        raise RuntimeError("PostgreSQL data directory not found")

    disk_base = du_bytes(data_dir)
    procset = ProcSet("postgres")
    sampler = MemSampler(procset)
    sampler.start()
    idle_pss, idle_rss = sampler.settle(2.0)

    adapter = bc.PgAdapter()
    t0 = time.perf_counter()
    adapter.setup()
    setup_secs = time.perf_counter() - t0
    rng, keys, value = bc.make_dataset(rows)
    load = adapter.load(keys, value, rng)
    read = adapter.point_read(keys, reads, rng, value)
    check_count(rows, adapter._count("kvbench"), "pg.kvbench")
    check_stats(load, "pg.load")
    check_stats(read, "pg.point_read")
    sampler.stop()
    peak_pss, peak_rss = sampler.peak_pss, sampler.peak_rss
    adapter.close()

    stop_service("postgresql")
    disk_after = du_bytes(data_dir)
    out["postgres"] = {
        "version": version,
        "idle_pss_mib": mib(idle_pss * 1024),
        "peak_pss_mib": mib(peak_pss * 1024),
        "idle_rss_mib": mib(idle_rss * 1024),
        "peak_rss_mib": mib(peak_rss * 1024),
        "disk_baseline_mib": mib(disk_base),
        "disk_after_mib": mib(disk_after),
        "disk_delta_mib": mib(disk_after - disk_base),
        "setup_secs": round(setup_secs, 2),
        "load_secs": load["secs"], "load_ops_s": load["ops_s"],
        "read_ops_s": read["ops_s"],
    }
    print("  [postgres] idle %.1f MiB | peak %.1f MiB | disk +%.1f MiB"
          % (out["postgres"]["idle_pss_mib"], out["postgres"]["peak_pss_mib"],
             out["postgres"]["disk_delta_mib"]))


# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--rows", type=int, default=100_000)
    ap.add_argument("--reads", type=int, default=4_000)
    ap.add_argument("--json-out", default=os.path.join(
        bc.PROJECT, "docs", "resources_compare_linux.json"))
    args = ap.parse_args()

    if os.name != "nt" and os.geteuid() != 0:
        print("ЗАПУСТИТЕ С sudo: нужны /proc/*/smaps_rollup и systemctl", file=sys.stderr)
        return 2

    out = {"meta": {
        "date": time.strftime("%Y-%m-%d %H:%M:%S"),
        "machine": bc.platform_string(),
        "rows": args.rows,
        "value_bytes": 160,
        "reads": args.reads,
        "method": ("defaults everywhere; idle = sum of process PSS after setup; "
                   "peak = max sum of PSS sampled every 50 ms during load+reads; "
                   "disk = data directory growth (clean shutdown) incl. WAL/binlog; "
                   "same deterministic dataset as bench_compare.py (seed 42)"),
    }}
    out["systems"] = {}
    order = []
    for system, fn in (("dnadb", measure_dnadb),
                       ("mysql", measure_mysql),
                       ("postgres", measure_postgres)):
        print("== %s ==" % system)
        fn(out["systems"], args.rows, args.reads)
        order.append(system)
    out["meta"]["order"] = order

    with open(args.json_out, "w") as f:
        json.dump(out, f, indent=2, ensure_ascii=False)
    print("\nOK ->", args.json_out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
