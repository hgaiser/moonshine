#!/usr/bin/env python3
"""Parse moonshine video pipeline latency logs and produce per-stage statistics + histograms.

Usage:
    # Collect logs from a streaming session:
    RUST_LOG=moonshine::session::stream::video=debug cargo run --release -- config.toml 2>&1 | tee session.log

    # Parse and analyze:
    python3 tools/parse_latency_logs.py session.log

    # With histogram plots (requires matplotlib):
    python3 tools/parse_latency_logs.py session.log --plot

    # Export CSV for external analysis:
    python3 tools/parse_latency_logs.py session.log --csv latency_data.csv
"""

import argparse
import csv
import re
import sys
from collections import defaultdict
from pathlib import Path

# Matches the per-frame latency breakdown log line from pipeline/mod.rs.
# The tracing crate puts the message first, then structured fields after (or vice versa).
# Support both orderings.
# Example format 1 (fields after message):
#   ... Frame latency breakdown channel_wait_us=42 import_us=15 convert_us=200 encode_us=1500 packetize_us=300 total_us=2057
# Example format 2 (fields before message):
#   ... channel_wait_us=42 import_us=15 convert_us=200 encode_us=1500 packetize_us=300 total_us=2057 Frame latency breakdown
FRAME_LATENCY_RE = re.compile(
    r"channel_wait_us=(\d+)\s+"
    r"import_us=(\d+)\s+"
    r"convert_us=(\d+)\s+"
    r"encode_us=(\d+)\s+"
    r"packetize_us=(\d+)\s+"
    r"total_us=(\d+)"
)

# Matches the spike warning line.
SPIKE_RE = re.compile(
    r"SPIKE: frame latency exceeds 4ms"
)

# Matches the periodic summary line.
SUMMARY_RE = re.compile(
    r"frames=(\d+)\s+"
    r"total_p50_us=(\d+)\s+"
    r"total_p95_us=(\d+)\s+"
    r"total_p99_us=(\d+)"
)

STAGE_NAMES = ["channel_wait", "import", "convert", "encode", "packetize", "total"]

# ANSI escape code pattern for stripping terminal colors from log output.
ANSI_ESCAPE_RE = re.compile(r"\x1b\[[0-9;]*m")


def parse_log_file(path):
    """Parse a log file and return a list of frame latency samples."""
    samples = []
    spike_count = 0

    with open(path) as f:
        for line in f:
            # Strip ANSI color codes that tracing_subscriber emits to terminals.
            line = ANSI_ESCAPE_RE.sub("", line)
            m = FRAME_LATENCY_RE.search(line)
            if m:
                sample = {
                    "channel_wait": int(m.group(1)),
                    "import": int(m.group(2)),
                    "convert": int(m.group(3)),
                    "encode": int(m.group(4)),
                    "packetize": int(m.group(5)),
                    "total": int(m.group(6)),
                }
                samples.append(sample)

            if SPIKE_RE.search(line):
                spike_count += 1

    return samples, spike_count


def percentile(sorted_values, p):
    """Return the p-th percentile from a sorted list."""
    if not sorted_values:
        return 0
    idx = int(len(sorted_values) * p / 100)
    idx = min(idx, len(sorted_values) - 1)
    return sorted_values[idx]


def compute_statistics(samples):
    """Compute per-stage statistics from samples."""
    stats = {}
    for stage in STAGE_NAMES:
        values = sorted(s[stage] for s in samples)
        if not values:
            continue
        stats[stage] = {
            "min": values[0],
            "p50": percentile(values, 50),
            "p75": percentile(values, 75),
            "p90": percentile(values, 90),
            "p95": percentile(values, 95),
            "p99": percentile(values, 99),
            "max": values[-1],
            "mean": sum(values) / len(values),
            "count": len(values),
        }
    return stats


def print_statistics(stats, spike_count, num_samples):
    """Print a formatted statistics table."""
    print(f"\n{'=' * 80}")
    print(f"  Moonshine Video Pipeline Latency Analysis")
    print(f"  {num_samples} frames analyzed, {spike_count} spikes (>4ms)")
    print(f"{'=' * 80}\n")

    header = f"{'Stage':<15} {'Min':>7} {'P50':>7} {'P75':>7} {'P90':>7} {'P95':>7} {'P99':>7} {'Max':>7} {'Mean':>8}"
    print(header)
    print("-" * len(header))

    for stage in STAGE_NAMES:
        if stage not in stats:
            continue
        s = stats[stage]
        print(
            f"{stage:<15} {s['min']:>6}µ {s['p50']:>6}µ {s['p75']:>6}µ "
            f"{s['p90']:>6}µ {s['p95']:>6}µ {s['p99']:>6}µ {s['max']:>6}µ {s['mean']:>7.0f}µ"
        )

    print()

    # Identify top-3 contributors to total latency by mean.
    contributors = []
    for stage in STAGE_NAMES:
        if stage == "total" or stage not in stats:
            continue
        contributors.append((stage, stats[stage]["mean"]))
    contributors.sort(key=lambda x: x[1], reverse=True)

    total_mean = stats.get("total", {}).get("mean", 1)
    print("Top-3 contributors to frame latency (by mean):")
    for i, (stage, mean_us) in enumerate(contributors[:3], 1):
        pct = mean_us / total_mean * 100 if total_mean > 0 else 0
        print(f"  {i}. {stage}: {mean_us:.0f}µs ({pct:.1f}% of total)")

    print()

    # Check targets from the performance plan.
    print("Target compliance:")
    total = stats.get("total", {})
    channel = stats.get("channel_wait", {})

    if total:
        p50_ok = "PASS" if total["p50"] < 5000 else "FAIL"
        p99_ok = "PASS" if total["p99"] < 10000 else "FAIL"
        print(f"  Median total < 5ms (T5-T1): {total['p50']}µs [{p50_ok}]")
        print(f"  P99 total < 10ms:           {total['p99']}µs [{p99_ok}]")

    if channel:
        ch_ok = "PASS" if channel["p50"] < 1000 else "FAIL"
        print(f"  Channel wait (T1-T0) < 1ms: {channel['p50']}µs [{ch_ok}]")

    print()


def export_csv(samples, path):
    """Export raw samples to CSV."""
    with open(path, "w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=STAGE_NAMES)
        writer.writeheader()
        for sample in samples:
            writer.writerow(sample)
    print(f"Exported {len(samples)} samples to {path}")


def plot_histograms(samples, stats):
    """Plot per-stage latency histograms. Requires matplotlib."""
    try:
        import matplotlib.pyplot as plt
    except ImportError:
        print("matplotlib is required for plotting. Install with: pip install matplotlib")
        sys.exit(1)

    fig, axes = plt.subplots(2, 3, figsize=(16, 10))
    fig.suptitle("Moonshine Video Pipeline Latency Histograms", fontsize=14)

    for ax, stage in zip(axes.flat, STAGE_NAMES):
        values = [s[stage] for s in samples]
        if not values:
            continue

        ax.hist(values, bins=50, edgecolor="black", alpha=0.7)
        ax.set_title(stage)
        ax.set_xlabel("Latency (µs)")
        ax.set_ylabel("Frame count")

        s = stats[stage]
        ax.axvline(s["p50"], color="green", linestyle="--", label=f"P50={s['p50']}µs")
        ax.axvline(s["p95"], color="orange", linestyle="--", label=f"P95={s['p95']}µs")
        ax.axvline(s["p99"], color="red", linestyle="--", label=f"P99={s['p99']}µs")
        ax.legend(fontsize=8)

    plt.tight_layout()
    output_path = "latency_histograms.png"
    plt.savefig(output_path, dpi=150)
    print(f"Saved histogram plot to {output_path}")
    plt.close()


def main():
    parser = argparse.ArgumentParser(
        description="Parse moonshine video pipeline latency logs."
    )
    parser.add_argument("logfile", help="Path to the log file to analyze.")
    parser.add_argument(
        "--plot",
        action="store_true",
        help="Generate histogram plots (requires matplotlib).",
    )
    parser.add_argument(
        "--csv",
        metavar="PATH",
        help="Export raw latency data to a CSV file.",
    )
    args = parser.parse_args()

    if not Path(args.logfile).exists():
        print(f"Error: {args.logfile} not found.", file=sys.stderr)
        sys.exit(1)

    samples, spike_count = parse_log_file(args.logfile)

    if not samples:
        print("No frame latency samples found in the log file.")
        print("Make sure moonshine was run with:")
        print("  RUST_LOG=moonshine::session::stream::video=debug")
        sys.exit(1)

    stats = compute_statistics(samples)
    print_statistics(stats, spike_count, len(samples))

    if args.csv:
        export_csv(samples, args.csv)

    if args.plot:
        plot_histograms(samples, stats)


if __name__ == "__main__":
    main()
