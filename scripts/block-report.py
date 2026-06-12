#!/usr/bin/env python3
"""
Read block-timing.py JSON output and produce a percentile table + chart.

Usage:
    python3 scripts/block-report.py blocks.json
    python3 scripts/block-report.py blocks.json --out report.png
"""

import argparse
import json
import sys

import numpy as np
import matplotlib.pyplot as plt
import matplotlib.ticker as mticker

PERCENTILES = [50, 80, 95, 99]


# ── helpers ──────────────────────────────────────────────────────────────────

def parse_db_size_bytes(s: str | None) -> float | None:
    if not s:
        return None
    units = {"K": 1024, "M": 1024 ** 2, "G": 1024 ** 3, "T": 1024 ** 4}
    suffix = s[-1].upper()
    if suffix in units:
        try:
            return float(s[:-1]) * units[suffix]
        except ValueError:
            return None
    try:
        return float(s)
    except ValueError:
        return None


def percentiles(values: list[float]) -> list[float | None]:
    if len(values) < 2:
        return [None] * len(PERCENTILES)
    return [float(np.percentile(values, p)) for p in PERCENTILES]


def fmt_ms(v: float | None, decimals: int = 3) -> str:
    return "n/a" if v is None else f"{v:.{decimals}f} ms"


def fmt_s(v: float | None) -> str:
    return "n/a" if v is None else f"{v:.3f} s"


def fmt_count(v: float | None) -> str:
    return "n/a" if v is None else f"{v:.1f}"


def print_table(rows: list[tuple[str, list[float | None], callable]]) -> None:
    C0, CW = 24, 18
    header = f"{'metric':<{C0}}" + "".join(f"{'p' + str(p):>{CW}}" for p in PERCENTILES)
    sep = "─" * (C0 + CW * len(PERCENTILES))
    print(header)
    print(sep)
    for label, pcts, fmt in rows:
        print(f"{label:<{C0}}" + "".join(f"{fmt(v):>{CW}}" for v in pcts))
    print()


# ── main ─────────────────────────────────────────────────────────────────────

def main() -> None:
    parser = argparse.ArgumentParser(description="Analyze block-timing.py output")
    parser.add_argument("input", help="JSON file produced by block-timing.py")
    parser.add_argument("--out", default=None, help="Save chart to file instead of displaying")
    args = parser.parse_args()

    with open(args.input) as f:
        records: list[dict] = json.load(f)

    if not records:
        print("No records.", file=sys.stderr)
        sys.exit(1)

    records.sort(key=lambda r: r["block_number"])

    # ── compute operations per block (entity_count diff) ──────────────────────
    ops_per_block: list[float | None] = [None]
    for i in range(1, len(records)):
        prev = records[i - 1].get("entity_count")
        curr = records[i].get("entity_count")
        ops_per_block.append(
            float(abs(curr - prev)) if prev is not None and curr is not None else None
        )

    block_times_ms = [
        r["block_time_between_s"] * 1000
        for r in records
        if r.get("block_time_between_s") is not None
    ]
    evm_times_ms = [
        r["evm_execution_time_ms"]
        for r in records
        if r.get("evm_execution_time_ms") is not None
    ]
    flush_times_ms = [
        r["block_flushing_time_ms"]
        for r in records
        if r.get("block_flushing_time_ms") is not None
    ]
    ops_values = [v for v in ops_per_block if v is not None]

    # ── percentile table ──────────────────────────────────────────────────────
    print_table([
        ("block_time_between",   percentiles(block_times_ms), fmt_ms),
        ("evm_execution_time",   percentiles(evm_times_ms),   fmt_ms),
        ("flush_time",           percentiles(flush_times_ms), fmt_ms),
        ("operations_per_block", percentiles(ops_values),     fmt_count),
    ])

    # ── chart data: group blocks by 100 ───────────────────────────────────────
    def group(n: int) -> int:
        return (n // 100) * 100

    groups: dict[int, dict] = {}
    for r in records:
        g = group(r["block_number"])
        bucket = groups.setdefault(g, {"bt": [], "ds": [], "ec": [], "evm": []})
        if r.get("block_time_between_s") is not None:
            bucket["bt"].append(r["block_time_between_s"])
        db_bytes = parse_db_size_bytes(r.get("db_size"))
        if db_bytes is not None:
            bucket["ds"].append(db_bytes / 1024 ** 3)          # → GB
        if r.get("entity_count") is not None:
            bucket["ec"].append(r["entity_count"])
        if r.get("evm_execution_time_ms") is not None:
            bucket["evm"].append(r["evm_execution_time_ms"])

    sorted_keys = sorted(groups)
    x_labels = [f"{g}–{g + 99}" for g in sorted_keys]
    x = np.arange(len(sorted_keys))

    avg_bt   = [np.mean(groups[g]["bt"])  if groups[g]["bt"]  else None for g in sorted_keys]
    avg_ds   = [np.mean(groups[g]["ds"])  if groups[g]["ds"]  else None for g in sorted_keys]
    last_ec  = [groups[g]["ec"][-1]       if groups[g]["ec"]  else None for g in sorted_keys]
    p95_evm  = [float(np.percentile(groups[g]["evm"], 95)) if len(groups[g]["evm"]) >= 2 else None
                for g in sorted_keys]

    w = max(10, len(sorted_keys) * 1.4)
    fig, (top, bot) = plt.subplots(2, 1, figsize=(w, 10), sharex=True)

    # ── chart 1: avg block time + DB size ─────────────────────────────────────
    ax1b = top.twinx()

    bt_vals = [v if v is not None else 0.0         for v in avg_bt]
    ds_vals = [v if v is not None else float("nan") for v in avg_ds]

    top.bar(x, bt_vals, width=0.5, alpha=0.65, color="steelblue", label="avg block time (s)")
    ax1b.plot(x, ds_vals, color="tomato", marker="o", linewidth=2, label="avg DB size (GB)")

    top.set_ylabel("Avg time between blocks (s)", color="steelblue")
    ax1b.set_ylabel("Avg DB size (GB)", color="tomato")
    top.tick_params(axis="y", labelcolor="steelblue")
    ax1b.tick_params(axis="y", labelcolor="tomato")
    top.yaxis.set_major_formatter(mticker.FormatStrFormatter("%.2f"))
    ax1b.yaxis.set_major_formatter(mticker.FormatStrFormatter("%.3f"))

    h1, l1 = top.get_legend_handles_labels()
    h2, l2 = ax1b.get_legend_handles_labels()
    top.legend(h1 + h2, l1 + l2, loc="upper left")
    top.set_title("Avg block time & DB growth")

    # ── chart 2: entity count + p95 EVM execution time ────────────────────────
    ax2b = bot.twinx()

    ec_vals  = [v if v is not None else float("nan") for v in last_ec]
    evm_vals = [v if v is not None else float("nan") for v in p95_evm]

    bot.plot(x, ec_vals,  color="mediumseagreen", marker="s", linewidth=2, label="entity count (end of group)")
    ax2b.plot(x, evm_vals, color="darkorange",    marker="^", linewidth=2, label="p95 EVM exec time (ms)")

    bot.set_xlabel("Block group")
    bot.set_ylabel("Entity count", color="mediumseagreen")
    ax2b.set_ylabel("p95 EVM execution time (ms)", color="darkorange")
    bot.tick_params(axis="y", labelcolor="mediumseagreen")
    ax2b.tick_params(axis="y", labelcolor="darkorange")

    h3, l3 = bot.get_legend_handles_labels()
    h4, l4 = ax2b.get_legend_handles_labels()
    bot.legend(h3 + h4, l3 + l4, loc="upper left")
    bot.set_title("Entity count & p95 EVM execution time")

    plt.xticks(x, x_labels, rotation=45, ha="right")

    total = len(records)
    bn_min = records[0]["block_number"]
    bn_max = records[-1]["block_number"]
    fig.suptitle(f"Blocks {bn_min}–{bn_max} ({total} samples)", fontsize=13, y=1.01)
    plt.tight_layout()

    if args.out:
        plt.savefig(args.out, dpi=150, bbox_inches="tight")
        print(f"Charts saved to {args.out}")
    else:
        plt.show()


if __name__ == "__main__":
    main()
