#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Generate PNG comparison charts (DNA-DB vs MySQL vs PostgreSQL).

Reads  docs/bench_compare_results.json  (produced by scripts/bench_compare.py)
Writes docs/charts/*.png  (used by docs/DB_COMPARISON.md and DB_COMPARISON_RU.md)

Usage: python scripts/gen_charts.py [--json PATH] [--out DIR]
"""

import argparse
import json
import os

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

HERE = os.path.dirname(os.path.abspath(__file__))
PROJECT = os.path.dirname(HERE)
DOCS = os.path.join(PROJECT, "docs")

COLORS = {"dnadb": "#22c55e", "mysql": "#f59e0b", "postgres": "#38bdf8"}
NAMES = {"dnadb": "DNA-DB", "mysql": "MySQL", "postgres": "PostgreSQL"}
ORDER = ("dnadb", "mysql", "postgres")


def load(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def save(fig, out_dir, name):
    os.makedirs(out_dir, exist_ok=True)
    path = os.path.join(out_dir, name)
    fig.tight_layout()
    fig.savefig(path, dpi=130)
    plt.close(fig)
    print("wrote", path)


def chart_throughput(systems, out_dir):
    """Grouped bars: ops/s for every phase, one group per phase."""
    phases = [
        ("load", "Load\n(single-thread insert)"),
        ("point_read", "Point read\n(single-thread)"),
        ("mixed", "Mixed 80/20\n(8 threads)"),
        ("sql_load", "SQL INSERT\n(single-thread)"),
        ("sql_point_query", "SQL SELECT by PK\n(single-thread)"),
    ]
    present = [k for k, _ in phases if any(k in systems[s] for s in ORDER)]
    n = len(present)
    width = 0.8 / len(ORDER)

    fig, ax = plt.subplots(figsize=(9.5, 4.8))
    for i, key in enumerate(ORDER):
        vals = [systems[key].get(p, {}).get("ops_s", 0) for p in present]
        xs = [j + i * width for j in range(n)]
        ax.bar(xs, vals, width * 0.94, label=NAMES[key], color=COLORS[key])
        for x, v in zip(xs, vals):
            ax.text(x, v, f"{v:,.0f}", ha="center", va="bottom", fontsize=8)

    ax.set_xticks([j + width for j in range(n)])
    ax.set_xticklabels([dict(phases)[p] for p in present], fontsize=8.5)
    ax.set_ylabel("ops/s (higher is better)")
    ax.set_title("Throughput by workload phase")
    ax.grid(axis="y", alpha=0.3)
    ax.legend(ncols=len(ORDER))
    ax.set_ylim(0, max(s.get(p, {}).get("ops_s", 0)
                        for s in systems.values() for p in present) * 1.18)
    save(fig, out_dir, "throughput.png")


def chart_latency(systems, out_dir, phase, title, fname):
    """Grouped bars: p50/p95/p99 latency in ms for one phase."""
    percentiles = [("p50_ms", "p50"), ("p95_ms", "p95"), ("p99_ms", "p99")]
    present = [k for k, _ in percentiles
               if any(phase in systems[s] and systems[s][phase].get(k) is not None
                      for s in ORDER)]
    width = 0.8 / len(percentiles)

    fig, ax = plt.subplots(figsize=(8.2, 4.4))
    for i, (metric, label) in enumerate(percentiles):
        if metric not in present:
            continue
        vals = [systems[key].get(phase, {}).get(metric, 0) for key in ORDER]
        xs = [j + i * width for j in range(len(ORDER))]
        bars = ax.bar(xs, vals, width * 0.94, label=label, alpha=0.9)
        for x, v in zip(xs, vals):
            ax.text(x, v, f"{v:.2f}", ha="center", va="bottom", fontsize=8)

    ax.set_xticks([j + width for j in range(len(ORDER))])
    ax.set_xticklabels([NAMES[k] for k in ORDER])
    ax.set_ylabel("milliseconds (lower is better)")
    ax.set_title(title + " — latency, ms")
    ax.grid(axis="y", alpha=0.3)
    ax.legend(ncols=len(present))
    top = max(systems[key].get(phase, {}).get(m, 0)
              for key in ORDER for m, _ in percentiles) or 1
    ax.set_ylim(0, top * 1.2)
    save(fig, out_dir, fname)


def chart_errors(systems, out_dir):
    """Verification errors: every system must show 0."""
    phases = ["point_read", "sql_point_query"]
    labels = ["Point read", "SQL SELECT by PK"]
    fig, ax = plt.subplots(figsize=(7.0, 3.6))
    width = 0.8 / len(ORDER)
    for i, key in enumerate(ORDER):
        vals = [systems[key].get(p, {}).get("errors", 0) for p in phases]
        xs = [j + i * width for j in range(len(phases))]
        ax.bar(xs, vals, width * 0.94, label=NAMES[key], color=COLORS[key])
        for x, v in zip(xs, vals):
            ax.text(x, v, str(v), ha="center", va="bottom", fontsize=9)
    ax.set_xticks([j + width for j in range(len(phases))])
    ax.set_xticklabels(labels)
    ax.set_ylabel("mismatched responses")
    ax.set_title("Correctness: value mismatches during benchmark (0 = all correct)")
    ax.grid(axis="y", alpha=0.3)
    ax.legend(ncols=len(ORDER))
    ax.set_ylim(0, 1)
    save(fig, out_dir, "correctness.png")


def chart_sql_features(out_dir, path):
    """Stacked bars: supported / documented gap / wrong result per system."""
    data = load(path)
    names = {"dnadb": "DNA-DB", "mysql": "MySQL", "postgres": "PostgreSQL"}
    colors = {"ok": "#22c55e", "unsupported": "#94a3b8", "bad": "#ef4444"}
    labels = {"ok": "supported & correct",
              "unsupported": "not supported (documented gap)",
              "bad": "wrong result / unexpected error"}
    keys = [k for k in ("dnadb", "mysql", "postgres")
            if k in data["summary"]]

    fig, ax = plt.subplots(figsize=(9.0, 3.6))
    ys = range(len(keys))
    left = [0] * len(keys)
    for bucket in ("ok", "unsupported", "bad"):
        vals = []
        for k in keys:
            s = data["summary"][k]
            if bucket == "ok":
                v = s.get("ok", 0)
            elif bucket == "unsupported":
                v = s.get("unsupported", 0)
            else:
                v = s.get("mismatch", 0) + s.get("error", 0)
            vals.append(v)
        ax.barh(list(ys), vals, left=left, color=colors[bucket],
                label=labels[bucket])
        for y, v, l in zip(ys, vals, left):
            if v:
                ax.text(l + v / 2, y, str(v), ha="center", va="center",
                        color="white" if bucket != "ok" else "#052e16",
                        fontsize=9, fontweight="bold")
        left = [l + v for l, v in zip(left, vals)]

    ax.set_yticks(list(ys))
    ax.set_yticklabels([names[k] for k in keys])
    ax.invert_yaxis()
    ax.set_xlabel("feature checks passed")
    ax.set_title("SQL feature conformance (%d checks per system)"
                 % len(data["checks"]))
    ax.grid(axis="x", alpha=0.3)
    ax.legend(loc="lower right", fontsize=8, ncols=1)
    ax.set_xlim(0, max(left) * 1.08)
    save(fig, out_dir, "sql_features.png")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--json", default=os.path.join(DOCS, "bench_compare_results.json"))
    ap.add_argument("--out", default=os.path.join(DOCS, "charts"))
    ap.add_argument("--sql-json", default=os.path.join(
        DOCS, "sql_feature_results.json"))
    args = ap.parse_args()

    data = load(args.json)
    systems = data["systems"]
    chart_throughput(systems, args.out)
    chart_latency(systems, args.out, "point_read",
                  "Point read (KV)", "latency_point_read.png")
    chart_latency(systems, args.out, "sql_point_query",
                  "SQL SELECT WHERE pk", "latency_sql_select.png")
    chart_errors(systems, args.out)
    if args.sql_json and os.path.exists(args.sql_json):
        chart_sql_features(args.out, args.sql_json)


if __name__ == "__main__":
    main()
