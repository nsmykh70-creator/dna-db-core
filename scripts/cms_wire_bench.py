#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""CMS wire-протокол бенчмарк: DNA-DB mysqld vs MariaDB, одинаковый SQL через
pymysql (text protocol, как wpdb). Метрики: ops/sec по фазам + RSS демона.

Использование:
  python3 scripts/cms_wire_bench.py --dna-port 13307 --mysql-port 13306 \
      --rows 8000 --out-json /tmp/bench.json --out-md /tmp/bench.md
"""

import argparse
import json
import platform
import time

import pymysql


# ---------------------------------------------------------------------------
# Утилиты
# ---------------------------------------------------------------------------

def connect(port, password):
    return pymysql.connect(
        host="127.0.0.1",
        port=port,
        user="bench",
        password=password,
        database="bench",
        autocommit=True,
    )


def version_of(port):
    """Строка VERSION() движка (для метаданных отчёта)."""
    try:
        conn = connect(port, "benchpass")
        with conn.cursor() as cur:
            cur.execute("SELECT VERSION()")
            v = cur.fetchone()[0]
        conn.close()
        return str(v)
    except Exception as e:  # движок мог быть остановлен к моменту записи отчёта
        return "unknown (%s)" % e


def rss_mb(pid):
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) / 1024.0
    except Exception:
        pass
    return 0.0


class RssSampler:
    """Фоновый сэмплер RSS: максимум за фазу."""

    def __init__(self, pids, interval=0.05):
        import threading

        self.pids = pids
        self.interval = interval
        self.stop = threading.Event()
        self.max_rss = {name: 0.0 for name in pids}
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()

    def _sample_once(self):
        for name, pid in self.pids.items():
            v = rss_mb(pid)
            if v > self.max_rss[name]:
                self.max_rss[name] = v

    def _run(self):
        while not self.stop.is_set():
            self._sample_once()
            time.sleep(self.interval)

    def phase(self):
        """Сброс максимума перед новой фазой + синхронный замер baseline:
        без него быстрая фаза (< интервала сэмплирования) осталась бы с 0."""
        for name in self.max_rss:
            self.max_rss[name] = 0.0
        self._sample_once()

    def snapshot(self):
        return dict(self.max_rss)

    def stop_now(self):
        self.stop.set()


def timed(fn):
    t0 = time.perf_counter()
    fn()
    dt = time.perf_counter() - t0
    return dt


# ---------------------------------------------------------------------------
# Схема и фазы (одинаковы для обоих движков)
# ---------------------------------------------------------------------------

SCHEMA = (
    "CREATE TABLE bench_posts ( "
    " ID bigint(20) unsigned NOT NULL AUTO_INCREMENT, "
    " post_author bigint(20) unsigned NOT NULL DEFAULT 0, "
    " post_title text NOT NULL, "
    " post_status varchar(20) NOT NULL DEFAULT 'publish', "
    " post_name varchar(200) NOT NULL DEFAULT '', "
    " post_modified datetime NOT NULL DEFAULT '2026-01-01 00:00:00', "
    " menu_order int(11) NOT NULL DEFAULT 0, "
    " PRIMARY KEY (ID), "
    " KEY post_name (post_name(191)) "
    ")",

    "CREATE TABLE bench_options ( "
    " option_id bigint(20) unsigned NOT NULL AUTO_INCREMENT, "
    " option_name varchar(191) NOT NULL DEFAULT '', "
    " option_value longtext NOT NULL, "
    " autoload varchar(20) NOT NULL DEFAULT 'yes', "
    " PRIMARY KEY (option_id), "
    " UNIQUE KEY option_name (option_name) "
    ")",
)


def phase_inserts(conn, n):
    """Построчные INSERT в autocommit — худший случай для LSM."""
    for i in range(n):
        with conn.cursor() as cur:
            cur.execute(
                "INSERT INTO bench_posts (post_author, post_title, post_status, post_name) "
                "VALUES (%s, %s, 'publish', %s)",
                (1, f"Пост номер {i}", f"post-{i}"),
            )


def phase_inserts_multirow(conn, n, batch=100):
    """Пакетные INSERT по 100 строк."""
    for start in range(0, n, batch):
        rows = ", ".join(
            f"(1, 'Пост {start + j}', 'publish', 'mr-{start + j}')" for j in range(batch)
        )
        with conn.cursor() as cur:
            cur.execute(
                "INSERT INTO bench_posts (post_author, post_title, post_status, post_name) "
                f"VALUES {rows}"
            )


def phase_point_selects(conn, n):
    """Точечные чтения по PK (id = монотонный случайный)."""
    for i in range(1, n + 1):
        with conn.cursor() as cur:
            cur.execute(
                "SELECT ID, post_title, post_status FROM bench_posts WHERE ID = %s", (i,)
            )
            cur.fetchall()


def phase_index_selects(conn, n):
    """Чтение по вторичному индексу post_name."""
    for i in range(1, n + 1):
        with conn.cursor() as cur:
            cur.execute(
                "SELECT ID, post_title FROM bench_posts WHERE post_name = %s",
                (f"post-{i}",),
            )
            cur.fetchall()


def phase_updates(conn, n):
    """UPDATE по PK — переписывание значения (Woo-паттерн stock)."""
    for i in range(1, n + 1):
        with conn.cursor() as cur:
            cur.execute(
                "UPDATE bench_posts SET menu_order = %s WHERE ID = %s", (i % 100, i)
            )


def phase_wp_mix(conn, n):
    """WordPress-микс: чтение опции + запись опции + INSERT meta-подобной строки."""
    for i in range(n):
        with conn.cursor() as cur:
            cur.execute(
                "SELECT option_value FROM bench_options WHERE option_name = %s",
                (f"opt_{i % 200}",),
            )
            cur.fetchall()
            cur.execute(
                "INSERT INTO bench_options (option_name, option_value, autoload) "
                "VALUES (%s, %s, 'yes') ON DUPLICATE KEY UPDATE option_value = VALUES(option_value)",
                (f"opt_{i % 200}", f"значение {i}"),
            )


PHASES = [
    ("insert_single", phase_inserts),
    ("insert_multirow", phase_inserts_multirow),
    ("select_pk", phase_point_selects),
    ("select_index", phase_index_selects),
    ("update_pk", phase_updates),
    ("wp_mix", phase_wp_mix),
]


def run_engine(name, port, password, rows, sampler):
    conn = connect(port, password)
    with conn.cursor() as cur:
        for ddl in SCHEMA:
            cur.execute(ddl)
    results = {}
    for phase_name, fn in PHASES:
        sampler.phase()
        t0 = time.perf_counter()
        fn(conn, rows)
        dt = time.perf_counter() - t0
        rss = sampler.snapshot()
        # RSS строго своего демона: max по всем процессам смешал бы колонки
        # (во время фаз DNA-DB в максимум попадал бы простаивающий MariaDB).
        own_rss = rss.get(name, 0.0)
        results[phase_name] = {
            "seconds": round(dt, 3),
            "ops_per_sec": round(rows / dt, 1),
            "rss_max_mb": round(own_rss, 1),
        }
        print(f"  [{name}] {phase_name}: {rows / dt:8.1f} ops/s  ({dt:5.2f}s)  rss={own_rss:.0f}MB")
    conn.close()
    return results


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dna-port", type=int, default=13307)
    ap.add_argument("--mysql-port", type=int, default=13306)
    ap.add_argument("--rows", type=int, default=8000)
    ap.add_argument("--dna-pid", type=int, default=0)
    ap.add_argument("--mysql-pid", type=int, default=0)
    ap.add_argument("--out-json", default="/tmp/cms_wire_bench.json")
    ap.add_argument("--out-md", default="/tmp/cms_wire_bench.md")
    args = ap.parse_args()

    pids = {}
    if args.dna_pid:
        pids["dna"] = args.dna_pid
    if args.mysql_pid:
        pids["mysql"] = args.mysql_pid
    if not pids:
        raise SystemExit("укажите --dna-pid и/или --mysql-pid: без pid демона RSS не измерить")
    sampler = RssSampler(pids)

    print(f"rows={args.rows}")
    print("== DNA-DB (mysql wire :%d) ==" % args.dna_port)
    dna = run_engine("dna", args.dna_port, "benchpass", args.rows, sampler)
    print("== MariaDB (:%d) ==" % args.mysql_port)
    mysql = run_engine("mysql", args.mysql_port, "benchpass", args.rows, sampler)
    sampler.stop_now()

    meta = {
        "date": time.strftime("%Y-%m-%d %H:%M:%S"),
        "platform": platform.platform(),
        "rows": args.rows,
        "method": ("одинаковый SQL через pymysql (text protocol, как wpdb); "
                   "ops/s = строк/сек по фазе; RSS = пик VmRSS демона за фазу "
                   "(сэмплер 50 мс + синхронный baseline, замер строго своего процесса)"),
        "versions": {"dna": version_of(args.dna_port), "mysql": version_of(args.mysql_port)},
    }
    out = {"meta": meta, "rows": args.rows, "dna": dna, "mysql": mysql}
    with open(args.out_json, "w") as f:
        json.dump(out, f, ensure_ascii=False, indent=2)

    # Markdown-сводка
    lines = [
        "# CMS wire-бенчмарк: DNA-DB mysqld vs MariaDB",
        "",
        "- Дата: %s, платформа: `%s`" % (meta["date"], meta["platform"]),
        "- Объём: %s строк, одинаковый SQL через pymysql (text protocol, как wpdb)" %
        "{:,}".format(args.rows).replace(",", " "),
        "- Версии: DNA-DB CMS Edition (`%s` — wire-совместимая строка версии), "
        "MariaDB `%s`" % (meta["versions"]["dna"], meta["versions"]["mysql"]),
        "- Методика: %s" % meta["method"],
        "",
        "| Фаза | DNA-DB ops/s | MariaDB ops/s | DNA × MariaDB |",
        "|---|---|---|---|",
    ]
    for phase_name, _ in PHASES:
        d = dna[phase_name]["ops_per_sec"]
        m = mysql[phase_name]["ops_per_sec"]
        ratio = d / m if m else 0
        lines.append(
            f"| {phase_name} | {d:,.0f} | {m:,.0f} | ×{ratio:.2f} |".replace(",", " ")
        )
    lines.append("")
    lines.append("| Фаза | RSS DNA-DB, МБ | RSS MariaDB, МБ |")
    lines.append("|---|---|---|")
    for phase_name, _ in PHASES:
        lines.append(
            f"| {phase_name} | {dna[phase_name]['rss_max_mb']:.0f} | {mysql[phase_name]['rss_max_mb']:.0f} |"
        )
    with open(args.out_md, "w") as f:
        f.write("\n".join(lines) + "\n")
    print("\n".join(lines))


if __name__ == "__main__":
    main()
