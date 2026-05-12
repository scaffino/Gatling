#!/usr/bin/env python3
"""
Pool per-transaction latency data from multiple runs and compute aggregated statistics.

Reads all csv/latency_plot_data_no_crashes_r*.csv files, pools the per-transaction
avg_latency_ms values by instance count, and computes exact statistics from the
pooled distribution.  This is the statistically correct approach: percentiles
cannot be recovered from per-run summary statistics alone.

Outputs:
  csv/aggregated_no_crashes_stats.csv  — per-instance summary with percentiles
  (optional) csv/aggregated_no_crashes_plot_data.csv — pooled data for scatter plots

Usage:
  python3 postprocessing/aggregate_plot_data.py
  python3 postprocessing/aggregate_plot_data.py --exclude-instances 30 40 50 60
  python3 postprocessing/aggregate_plot_data.py --output-plot-data csv/aggregated_no_crashes_plot_data.csv
"""

import argparse
import csv
import glob
import os
from collections import defaultdict
from typing import Dict, List, Set

try:
    import numpy as np
    _NUMPY_OK = True
except ImportError:
    _NUMPY_OK = False


# ============================================================================
# CONFIGURATION
# ============================================================================

# Default K values to exclude from the output.  K=60 is excluded because the
# user is not interested in it.
DEFAULT_EXCLUDED_INSTANCES: Set[int] = {60}

# ============================================================================


def get_repo_root() -> str:
    return os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def parse_args() -> argparse.Namespace:
    repo_root = get_repo_root()
    csv_dir = os.path.join(repo_root, "csv")

    parser = argparse.ArgumentParser(
        description="Aggregate latency data from multiple no-crashes runs into one stats CSV"
    )
    parser.add_argument(
        "--csv-dir",
        default=csv_dir,
        help="Directory containing latency_plot_data_no_crashes_r*.csv (default: csv/)",
    )
    parser.add_argument(
        "--output-stats",
        default=os.path.join(csv_dir, "aggregated_no_crashes_stats.csv"),
        help="Output path for aggregated stats CSV",
    )
    parser.add_argument(
        "--output-plot-data",
        default=None,
        metavar="PATH",
        help="If set, also write a pooled latency_plot_data CSV to this path "
             "(large file; needed only for scatter plots)",
    )
    parser.add_argument(
        "--exclude-instances",
        type=int,
        nargs="*",
        default=sorted(DEFAULT_EXCLUDED_INSTANCES),
        metavar="K",
        help="Instance counts to exclude (default: 60)",
    )
    return parser.parse_args()


def load_all_runs(
    csv_dir: str, excluded: Set[int]
) -> Dict[int, List[float]]:
    """
    Stream all run CSV files and pool latencies by instance count.
    Returns {instances: [avg_latency_ms, ...]}.
    """
    pattern = os.path.join(csv_dir, "latency_plot_data_no_crashes_r*.csv")
    files = sorted(glob.glob(pattern))
    if not files:
        raise FileNotFoundError("No files matching: {}".format(pattern))

    by_instances: Dict[int, List[float]] = defaultdict(list)
    run_counts: Dict[int, Set[str]] = defaultdict(set)  # k -> set of filenames

    for path in files:
        run_name = os.path.basename(path)
        print("Reading {} ...".format(run_name))
        n = 0
        with open(path, newline="") as f:
            reader = csv.DictReader(f)
            for row in reader:
                k = int(row["instances"])
                if k in excluded:
                    continue
                by_instances[k].append(float(row["avg_latency_ms"]))
                run_counts[k].add(run_name)
                n += 1
        print("  loaded {:,} rows (excluding K in {{{}}})".format(
            n, ", ".join(str(k) for k in sorted(excluded))
        ))

    print()
    print("Runs per instance count:")
    for k in sorted(run_counts):
        print("  K={:>3}: {} runs  ({:,} txs total)".format(
            k, len(run_counts[k]), len(by_instances[k])
        ))
    print()

    return by_instances


def _percentile(values: List[float], p: float) -> float:
    """Compute exact percentile using numpy if available, otherwise linear interpolation."""
    if _NUMPY_OK:
        return float(np.percentile(values, p))
    # Pure-Python fallback: linear interpolation
    n = len(values)
    s = sorted(values)
    idx = (p / 100.0) * (n - 1)
    lo = int(idx)
    hi = lo + 1
    if hi >= n:
        return s[-1]
    frac = idx - lo
    return s[lo] + frac * (s[hi] - s[lo])


def compute_stats(latencies: List[float]) -> dict:
    """Compute summary statistics from a list of per-transaction latencies."""
    n = len(latencies)
    if _NUMPY_OK:
        arr = np.array(latencies)
        mean = float(np.mean(arr))
        stddev = float(np.std(arr, ddof=1)) if n > 1 else 0.0
        return {
            "n_tx": n,
            "mean_ms": mean,
            "stddev_ms": stddev,
            "p5_ms": float(np.percentile(arr, 5)),
            "p10_ms": float(np.percentile(arr, 10)),
            "p50_ms": float(np.percentile(arr, 50)),
            "p90_ms": float(np.percentile(arr, 90)),
            "p95_ms": float(np.percentile(arr, 95)),
            "min_ms": float(np.min(arr)),
            "max_ms": float(np.max(arr)),
        }
    else:
        mean = sum(latencies) / n
        var = sum((x - mean) ** 2 for x in latencies) / (n - 1) if n > 1 else 0.0
        stddev = var ** 0.5
        return {
            "n_tx": n,
            "mean_ms": mean,
            "stddev_ms": stddev,
            "p5_ms": _percentile(latencies, 5),
            "p10_ms": _percentile(latencies, 10),
            "p50_ms": _percentile(latencies, 50),
            "p90_ms": _percentile(latencies, 90),
            "p95_ms": _percentile(latencies, 95),
            "min_ms": min(latencies),
            "max_ms": max(latencies),
        }


def write_stats_csv(by_instances: Dict[int, List[float]], path: str) -> None:
    fieldnames = [
        "instances", "n_tx",
        "mean_ms", "stddev_ms",
        "p5_ms", "p10_ms", "p50_ms", "p90_ms", "p95_ms",
        "min_ms", "max_ms",
    ]
    with open(path, "w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=fieldnames)
        writer.writeheader()
        for k in sorted(by_instances.keys()):
            stats = compute_stats(by_instances[k])
            row = {"instances": k}
            row.update(stats)
            writer.writerow(row)
    print("Aggregated stats written to: {}".format(path))


def write_plot_data_csv(by_instances: Dict[int, List[float]], path: str) -> None:
    """Write pooled data in the same format as latency_plot_data.csv (for scatter plots)."""
    with open(path, "w", newline="") as f:
        writer = csv.writer(f)
        writer.writerow(["instances", "avg_latency_ms"])
        for k in sorted(by_instances.keys()):
            for v in by_instances[k]:
                writer.writerow([k, "{:.6f}".format(v)])
    print("Pooled plot data written to: {}".format(path))


def main() -> None:
    if not _NUMPY_OK:
        print("Warning: numpy not available; using pure-Python fallback (slower for large datasets)")

    args = parse_args()
    excluded = set(args.exclude_instances)

    by_instances = load_all_runs(args.csv_dir, excluded)
    write_stats_csv(by_instances, args.output_stats)

    if args.output_plot_data:
        write_plot_data_csv(by_instances, args.output_plot_data)


if __name__ == "__main__":
    main()
