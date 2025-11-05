#!/usr/bin/env python3
"""
Aggregate transaction finalization latency from Gatling logs.

Reads all files under logs/gatling named gatling_v{V}_i{I}_r{R}.log,
extracts finalization events, deduplicates per transaction per validator
(using only the first finalization event per validator), computes median
latency across validators for each transaction, then computes the overall
median latency per instance count, writes a CSV, and plots latency vs
instances.
"""

import argparse
import calendar
import os
import re
import statistics
import sys
from collections import defaultdict
from datetime import datetime
from typing import Dict, Iterable, List, Optional, Tuple


FILENAME_RE = re.compile(r"^gatling_v(\d+)_i(\d+)_r(\d+)\.log$")
LOG_TS_RE = re.compile(r"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z)")
FINALIZE_RE = re.compile(
    r"Transaction ([a-f0-9]+) \(timestamp: (\d+) ms\) is now final in block"
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Aggregate Gatling latencies and plot vs instances"
    )
    parser.add_argument(
        "--dir",
        dest="dir",
        default=os.path.join("logs", "gatling"),
        help="Directory containing gatling_v*_i*_r*.log files",
    )
    parser.add_argument(
        "--csv",
        dest="csv",
        default=os.path.join("logs", "gatling", "latency_by_instances.csv"),
        help="Output CSV path",
    )
    parser.add_argument(
        "--png",
        dest="png",
        default=os.path.join("logs", "gatling", "latency_by_instances.png"),
        help="Output PNG path",
    )
    return parser.parse_args()


def iter_gatling_files(dir_path: str) -> Iterable[Tuple[str, int, int]]:
    try:
        entries = sorted(os.listdir(dir_path))
    except FileNotFoundError:
        print("Error: directory not found: {0}".format(dir_path))
        sys.exit(1)

    for name in entries:
        m = FILENAME_RE.match(name)
        if not m:
            # Skip non-matching files silently
            continue
        v = int(m.group(1))
        i = int(m.group(2))
        _r = int(m.group(3))
        yield os.path.join(dir_path, name), i, v


def parse_log_line(line: str) -> Optional[Tuple[int, str, int]]:
    """
    Returns tuple (log_ts_ms, tx_hash, tx_ts_ms) or None
    """
    ts_m = LOG_TS_RE.search(line)
    fin_m = FINALIZE_RE.search(line)
    if not ts_m or not fin_m:
        return None

    log_ts_str = ts_m.group(1).rstrip("Z")
    log_dt = datetime.strptime(log_ts_str, "%Y-%m-%dT%H:%M:%S.%f")
    log_ts_ms = (
        calendar.timegm(log_dt.timetuple()) * 1000 + int(log_dt.microsecond / 1000.0)
    )
    tx_hash = fin_m.group(1)
    tx_ts_ms = int(fin_m.group(2))
    return log_ts_ms, tx_hash, tx_ts_ms


def parse_latency_occurrences(path: str) -> Iterable[Tuple[str, int]]:
    """
    Yields (tx_hash, latency_ms) for each matching line in a file.
    """
    try:
        with open(path, "r") as f:
            for line in f:
                parsed = parse_log_line(line)
                if not parsed:
                    continue
                log_ts_ms, tx_hash, tx_ts_ms = parsed
                yield tx_hash, log_ts_ms - tx_ts_ms
    except IOError:
        # Skip unreadable files
        return


def aggregate_by_instances(dir_path: str) -> Dict[int, Dict[str, List[int]]]:
    """
    Returns mapping: instances -> { tx_hash -> [latency_ms, ...] }
    For each transaction, collects the first finalization event from each
    validator file (one latency value per validator).
    """
    by_instances: Dict[int, Dict[str, List[int]]] = defaultdict(
        lambda: defaultdict(list)
    )
    # Track first occurrence per transaction per validator file
    seen_per_file: Dict[Tuple[int, int, str], bool] = {}
    any_file = False
    for file_path, instances, validator in iter_gatling_files(dir_path):
        any_file = True
        for tx_hash, latency_ms in parse_latency_occurrences(file_path):
            # Only keep the first occurrence of each transaction per file
            file_key = (instances, validator, tx_hash)
            if file_key not in seen_per_file:
                seen_per_file[file_key] = True
                by_instances[instances][tx_hash].append(latency_ms)
    if not any_file:
        print(
            "Error: no gatling_v*_i*_r*.log files found in {0}".format(dir_path)
        )
        sys.exit(1)
    return by_instances


def compute_instance_stats(
    by_instances: Dict[int, Dict[str, List[int]]]
) -> Dict[int, Tuple[float, int]]:
    """
    For each instance count I:
    1. For each transaction, compute median of latency values from all
       validators (each validator contributes its first finalization event)
    2. Compute overall median across all unique transactions
    Returns mapping: I -> (median_latency_ms, num_unique_transactions).
    """
    result: Dict[int, Tuple[float, int]] = {}
    for instances, tx_map in sorted(by_instances.items()):
        per_tx_medians: List[float] = []
        for tx_hash, validator_latencies in tx_map.items():
            if not validator_latencies:
                continue
            # Median latency across all validators for this transaction
            tx_median = statistics.median(validator_latencies)
            per_tx_medians.append(tx_median)
        if per_tx_medians:
            overall_median = statistics.median(per_tx_medians)
            result[instances] = (overall_median, len(per_tx_medians))
        else:
            result[instances] = (0.0, 0)
    return result


def write_csv(stats: Dict[int, Tuple[float, int]], csv_path: str) -> None:
    os.makedirs(os.path.dirname(csv_path), exist_ok=True)
    with open(csv_path, "w") as f:
        f.write("instances,median_latency_ms,num_transactions\n")
        for instances in sorted(stats.keys()):
            median_ms, n = stats[instances]
            f.write(f"{instances},{median_ms:.3f},{n}\n")


def plot_png(stats: Dict[int, Tuple[float, int]], png_path: str) -> None:
    try:
        import matplotlib.pyplot as plt
    except Exception as e:
        print("Warning: matplotlib not available; skipping plot: {0}".format(e))
        return
    xs = sorted(stats.keys())
    ys = [stats[i][0] for i in xs]
    plt.figure(figsize=(7, 4))
    plt.plot(xs, ys, marker="o")
    plt.xlabel("Number of instances")
    plt.ylabel("Median latency (ms)")
    plt.title("Median transaction latency vs consensus instances")
    # Force integer ticks from 0 to 11 on the x-axis
    ticks = list(range(0, 12))
    plt.xticks(ticks, ticks)
    plt.xlim(0, 11)
    plt.grid(True, linestyle=":", alpha=0.6)
    os.makedirs(os.path.dirname(png_path), exist_ok=True)
    plt.tight_layout()
    plt.savefig(png_path, dpi=160)
    plt.close()


def main() -> None:
    args = parse_args()
    by_instances = aggregate_by_instances(args.dir)
    stats = compute_instance_stats(by_instances)

    # Console summary
    for instances in sorted(stats.keys()):
        median_ms, n = stats[instances]
        print(
            "instances={0}: median_latency_ms={1:.3f}, "
            "unique_transactions={2}".format(instances, median_ms, n)
        )

    write_csv(stats, args.csv)
    plot_png(stats, args.png)


if __name__ == "__main__":
    main()
