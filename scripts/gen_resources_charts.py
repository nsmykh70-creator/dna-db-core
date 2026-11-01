#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Charts for the resource benchmark: RAM & disk, DNA-DB vs MySQL vs PostgreSQL.

Reads  docs/resources_compare_linux.json  (produced by scripts/measure_resources.py)
Writes docs/charts/resources_ram.png      (idle vs peak RAM, sum PSS, MiB)
       docs/charts/resources_disk.png     (disk growth of the data directory, MiB)
       landing/assets/charts/ram_resources.png   (copies for the landing page)
       landing/assets/charts/disk_footprint.png

Usage: python scripts/gen_resources_charts.py [--json PATH] [--out DIR] [--landing DIR]
"""

import argparse
import json
import os
import shutil

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

HERE = os.path.dirname(os.path.abspath(__file__))
PROJECT = os.path.dirname(HERE)

COLORS = {"dnadb": "#22c55e", "mysql": "#f59e0b", "postgres": "#38bdf8"}
NAMES = {"dnadb": "DNA-DB", "mysql": "MySQL", "postgres": "PostgreSQL"}
ORDER = ("dnadb", "mysql", "postgres")


def load(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def style(ax):
    ax.grid(axis="y", alpha=0.3)
    ax.spines["top"].set_visible(False)
    ax.spines["right"].set_visible(False)
    ax.set_axisbelow(True)


def chart_ram(systems, out_dir, rows):
    """Grouped bars: idle vs peak memory (sum of process PSS), MiB."""
    idle = [systems[s]["idle_pss_mib"] for s in ORDER]
    peak = [systems[s]["peak_pss_mib"] for s in ORDER]

    fig, ax = plt.subplots(figsize=(8.2, 4.6), constrained_layout=True)
    width = 0.36
    xs = range(len(ORDER))
    b1 = ax.bar([x - width / 2 for x in xs], idle, width,
                label="Idle (after schema setup)", color=[COLORS[s] for s in ORDER],
                alpha=0.55)
    b2 = ax.bar([x + width / 2 for x in xs], peak, width,
                label="Peak (during load + reads)", color=[COLORS[s] for s in ORDER])
    for bars in (b1, b2):
        for rect in bars:
            v = rect.get_height()
            ax.text(rect.get_x() + rect.get_width() / 2, v, "%g" % v,
                    ha="center", va="bottom", fontsize=9.5, fontweight="bold")
    ax.set_xticks(list(xs))
    ax.set_xticklabels([NAMES[s] for s in ORDER], fontsize=11)
    ax.set_ylabel("Memory, MiB (lower is better)")
    ax.set_title("RAM: sum of DB process PSS — %s rows, defaults everywhere" %
                 "{:,}".format(rows).replace(",", " "), fontsize=12)
    style(ax)
    ax.legend(loc="upper left", fontsize=9.5, framealpha=0.9)
    path = os.path.join(out_dir, "resources_ram.png")
    fig.savefig(path, dpi=130)
    plt.close(fig)
    print("wrote", path)
    return path


def chart_disk(systems, out_dir, rows, value_bytes=160, key_len=12):
    """Bars: data-directory growth after a clean shutdown, MiB."""
    delta = [systems[s]["disk_delta_mib"] for s in ORDER]
    raw = rows * (value_bytes + key_len) / (1024 * 1024)

    fig, ax = plt.subplots(figsize=(8.2, 4.6), constrained_layout=True)
    xs = range(len(ORDER))
    bars = ax.bar(xs, delta, 0.55, color=[COLORS[s] for s in ORDER])
    for rect in bars:
        v = rect.get_height()
        ax.text(rect.get_x() + rect.get_width() / 2, v, "%g" % v,
                ha="center", va="bottom", fontsize=10, fontweight="bold")
    ax.axhline(raw, color="#64748b", linestyle="--", linewidth=1.2)
    ax.text(len(ORDER) - 0.52, raw, " raw payload %.1f MiB" % raw,
            va="bottom", ha="right", fontsize=9, color="#475569")
    ax.set_xticks(list(xs))
    ax.set_xticklabels([NAMES[s] for s in ORDER], fontsize=11)
    ax.set_ylabel("Disk growth, MiB (lower is better)")
    ax.set_title("Disk: data directory growth (clean shutdown), %s rows x 172 B" %
                 "{:,}".format(rows).replace(",", " "), fontsize=12)
    style(ax)
    path = os.path.join(out_dir, "resources_disk.png")
    fig.savefig(path, dpi=130)
    plt.close(fig)
    print("wrote", path)
    return path


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--json", default=os.path.join(
        PROJECT, "docs", "resources_compare_linux.json"))
    ap.add_argument("--out", default=os.path.join(PROJECT, "docs", "charts"))
    ap.add_argument("--landing", default=os.path.join(
        PROJECT, "landing", "assets", "charts"))
    args = ap.parse_args()

    data = load(args.json)
    systems = data["systems"]
    rows = data["meta"]["rows"]

    p1 = chart_ram(systems, args.out, rows)
    p2 = chart_disk(systems, args.out, rows)

    os.makedirs(args.landing, exist_ok=True)
    for src, name in ((p1, "ram_resources.png"), (p2, "disk_footprint.png")):
        dst = os.path.join(args.landing, name)
        shutil.copyfile(src, dst)
        print("wrote", dst)


if __name__ == "__main__":
    main()
