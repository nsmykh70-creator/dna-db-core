#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Charts for the resource benchmark: RAM & disk, DNA-DB vs MySQL vs PostgreSQL.

Reads  docs/resources_compare_linux.json      (100 000 rows, produced by
       scripts/measure_resources.py) and docs/resources_compare_linux_1m.json
       (1 000 000 rows).
Writes docs/charts/resources_ram.png          (idle vs peak RAM, sum PSS, MiB)
       docs/charts/resources_disk.png         (disk growth of the data directory, MiB)
       landing/assets/charts/ram_resources.png   (copies for the landing page)
       landing/assets/charts/disk_footprint.png
For every non-default scale <rows> the files get a suffix: resources_ram_1m.png,
ram_resources_1m.png, ... Pass --json once per scale.

Usage: python scripts/gen_resources_charts.py [--json PATH]... [--out DIR] [--landing DIR]
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


def scale_suffix(rows):
    """File-name suffix for a dataset scale: legacy names for 100k, _1m for 1M."""
    if rows == 100000:
        return ""
    if rows == 1000000:
        return "_1m"
    return "_%d" % rows


def load(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def style(ax):
    ax.grid(axis="y", alpha=0.3)
    ax.spines["top"].set_visible(False)
    ax.spines["right"].set_visible(False)
    ax.set_axisbelow(True)


def chart_ram(systems, out_dir, rows, suffix=""):
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
    path = os.path.join(out_dir, "resources_ram%s.png" % suffix)
    fig.savefig(path, dpi=130)
    plt.close(fig)
    print("wrote", path)
    return path


def chart_disk(systems, out_dir, rows, suffix="", value_bytes=160, key_len=12):
    """Bars: data-directory growth after a clean shutdown, MiB.

    When the JSON carries `disk_after_compact_mib` for DNA-DB (v4.1 offline
    compaction), a lighter fourth bar shows the post-compaction footprint.
    """
    delta = [systems[s]["disk_delta_mib"] for s in ORDER]
    raw = rows * (value_bytes + key_len) / (1024 * 1024)

    compact = systems["dnadb"].get("disk_after_compact_mib")
    entries = [("DNA-DB", delta[0], COLORS["dnadb"], False)]
    if compact is not None:
        entries.append(("DNA-DB\nafter compaction", compact, "#86efac", True))
    entries.append(("MySQL", delta[1], COLORS["mysql"], False))
    entries.append(("PostgreSQL", delta[2], COLORS["postgres"], False))

    fig, ax = plt.subplots(figsize=(8.2, 4.6), constrained_layout=True)
    xs = range(len(entries))
    bars = ax.bar(xs, [e[1] for e in entries], 0.55,
                  color=[e[2] for e in entries])
    for (label, v, color, faded), rect in zip(entries, bars):
        txt = "%g" % v
        if faded and compact:
            txt += "  (−%g MiB)" % (delta[0] - compact)
        ax.text(rect.get_x() + rect.get_width() / 2, v, txt,
                ha="center", va="bottom", fontsize=10, fontweight="bold")
    ax.axhline(raw, color="#64748b", linestyle="--", linewidth=1.2)
    ax.text(len(entries) - 0.52, raw, " raw payload %.1f MiB" % raw,
            va="bottom", ha="right", fontsize=9, color="#475569")
    ax.set_xticks(list(xs))
    ax.set_xticklabels([e[0] for e in entries], fontsize=10.5)
    ax.set_ylabel("Disk growth, MiB (lower is better)")
    ax.set_title("Disk: data directory growth (clean shutdown), %s rows x 172 B" %
                 "{:,}".format(rows).replace(",", " "), fontsize=12)
    style(ax)
    path = os.path.join(out_dir, "resources_disk%s.png" % suffix)
    fig.savefig(path, dpi=130)
    plt.close(fig)
    print("wrote", path)
    return path


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--json", action="append", dest="jsons", default=None,
                    help="resources JSON; repeat for each scale (100k, 1M)")
    ap.add_argument("--out", default=os.path.join(PROJECT, "docs", "charts"))
    ap.add_argument("--landing", default=os.path.join(
        PROJECT, "landing", "assets", "charts"))
    args = ap.parse_args()

    jsons = args.jsons or [os.path.join(
        PROJECT, "docs", "resources_compare_linux.json")]

    os.makedirs(args.out, exist_ok=True)
    os.makedirs(args.landing, exist_ok=True)
    for path in jsons:
        data = load(path)
        systems = data["systems"]
        rows = data["meta"]["rows"]
        # default scale keeps the legacy file names, larger scales get _1m etc.
        suffix = scale_suffix(rows)

        p1 = chart_ram(systems, args.out, rows, suffix)
        p2 = chart_disk(systems, args.out, rows, suffix)

        for src, name in ((p1, "ram_resources%s.png" % suffix),
                          (p2, "disk_footprint%s.png" % suffix)):
            dst = os.path.join(args.landing, name)
            shutil.copyfile(src, dst)
            print("wrote", dst)


if __name__ == "__main__":
    main()
