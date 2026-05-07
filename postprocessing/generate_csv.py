#!/usr/bin/env python3
"""
Aggregate transaction finalization latency from Gatling logs.

Reads all files under logs/gatling named gatling_i{N}_{id}.log,
where {N} is the consensus instance count and {id} is the validator identifier.
Extracts finalization events, deduplicates per transaction per validator
(using only the first finalization event per validator), computes average
latency across validators for each transaction, then computes the overall
statistics per instance count and writes:
  - stats.csv            : per-instance summary statistics + KS test results
  - latency_plot_data.csv: per-transaction average latencies (input for plots.py)
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

try:
    from scipy.stats import ks_2samp
    SCIPY_AVAILABLE = True
except ImportError:
    SCIPY_AVAILABLE = False


# ============================================================================
# CONFIGURATION
# ============================================================================

INPUT_DIR = "logs/gatling"
OUTPUT_DIR = None  # None → same as INPUT_DIR

OUTPUT_STATS_CSV_NAME = "stats.csv"
OUTPUT_PLOT_DATA_CSV_NAME = "latency_plot_data.csv"

# ============================================================================

# Matches: gatling_i{instances}_{validator_id}.log
FILENAME_RE = re.compile(r"^gatling_i(\d+)_([0-9a-z]+)\.log$")
LOG_TS_RE = re.compile(r"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z)")
FINALIZE_RE = re.compile(
    r"Transaction ([a-f0-9]+) \(timestamp: (\d+) ms\) is now final in block"
)
# New format: includes finalized_at captured by run_buffer before Gatling thread lag.
# Falls back to log timestamp for old log files that lack the field.
FINALIZED_AT_RE = re.compile(r"finalized_at: (\d+) ms")


def get_repo_root() -> str:
    script_dir = os.path.dirname(os.path.abspath(__file__))
    return os.path.dirname(script_dir)


def parse_args() -> argparse.Namespace:
    repo_root = get_repo_root()

    if os.path.isabs(INPUT_DIR):
        default_input_dir = INPUT_DIR
    else:
        default_input_dir = os.path.join(repo_root, INPUT_DIR)

    if OUTPUT_DIR is None:
        default_output_dir = default_input_dir
    elif os.path.isabs(OUTPUT_DIR):
        default_output_dir = OUTPUT_DIR
    else:
        default_output_dir = os.path.join(repo_root, OUTPUT_DIR)

    default_stats_csv = os.path.join(default_output_dir, OUTPUT_STATS_CSV_NAME)
    default_plot_data_csv = os.path.join(
        default_output_dir, OUTPUT_PLOT_DATA_CSV_NAME
    )

    parser = argparse.ArgumentParser(
        description="Aggregate Gatling latencies and write stats CSV"
    )
    parser.add_argument(
        "--dir",
        dest="dir",
        default=default_input_dir,
        help="Directory containing gatling_i*_*.log files (default: from config)",
    )
    parser.add_argument(
        "--stats-csv",
        dest="stats_csv",
        default=default_stats_csv,
        help="Output stats CSV path (default: from config)",
    )
    parser.add_argument(
        "--plot-data-csv",
        dest="plot_data_csv",
        default=default_plot_data_csv,
        help="Output CSV path for per-transaction plot data (default: from config)",
    )
    parser.add_argument(
        "--tx-window-start",
        dest="tx_window_start",
        type=int,
        default=None,
        help="Exclude transactions created before this Unix ms timestamp",
    )
    parser.add_argument(
        "--tx-window-end",
        dest="tx_window_end",
        type=int,
        default=None,
        help="Exclude transactions created after this Unix ms timestamp",
    )
    return parser.parse_args()


def iter_gatling_files(dir_path: str) -> Iterable[Tuple[str, int, str]]:
    """Yields (file_path, instances, validator_id)."""
    try:
        entries = sorted(os.listdir(dir_path))
    except FileNotFoundError:
        print("Error: directory not found: {0}".format(dir_path))
        sys.exit(1)

    for name in entries:
        m = FILENAME_RE.match(name)
        if not m:
            continue
        i = int(m.group(1))   # instance count
        v = m.group(2)        # validator id
        yield os.path.join(dir_path, name), i, v


def check_consistency(dir_path: str) -> None:
    any_file = False
    for _ in iter_gatling_files(dir_path):
        any_file = True
        break
    if not any_file:
        print(
            "Error: no gatling logs found matching expected pattern in {0}".format(
                dir_path
            )
        )
        sys.exit(1)


def parse_log_line(line: str) -> Optional[Tuple[int, str, int]]:
    """Returns (finalized_at_ms, tx_hash, tx_ts_ms) or None.

    Uses the embedded finalized_at field when present (written by run_buffer,
    immune to Gatling thread scheduling lag). Falls back to the log line's
    leading ISO timestamp for old log files that pre-date this field.
    """
    fin_m = FINALIZE_RE.search(line)
    if not fin_m:
        return None
    tx_hash = fin_m.group(1)
    tx_ts_ms = int(fin_m.group(2))

    at_m = FINALIZED_AT_RE.search(line)
    if at_m:
        finalized_at_ms = int(at_m.group(1))
    else:
        ts_m = LOG_TS_RE.search(line)
        if not ts_m:
            return None
        log_ts_str = ts_m.group(1).rstrip("Z")
        log_dt = datetime.strptime(log_ts_str, "%Y-%m-%dT%H:%M:%S.%f")
        base_seconds_ms = calendar.timegm(log_dt.timetuple()) * 1000
        micros_ms = int(log_dt.microsecond / 1000.0)
        finalized_at_ms = base_seconds_ms + micros_ms

    return finalized_at_ms, tx_hash, tx_ts_ms


def parse_latency_occurrences(
    path: str,
    tx_window_start: Optional[int] = None,
    tx_window_end: Optional[int] = None,
) -> Iterable[Tuple[str, int]]:
    """Yields (tx_hash, latency_ms) for each matching line in a file."""
    try:
        with open(path, "r") as f:
            for line in f:
                parsed = parse_log_line(line)
                if not parsed:
                    continue
                finalized_at_ms, tx_hash, tx_ts_ms = parsed
                if tx_window_start is not None and tx_ts_ms < tx_window_start:
                    continue
                if tx_window_end is not None and tx_ts_ms > tx_window_end:
                    continue
                yield tx_hash, finalized_at_ms - tx_ts_ms
    except IOError:
        return


def aggregate_by_instances(
    dir_path: str,
    tx_window_start: Optional[int] = None,
    tx_window_end: Optional[int] = None,
) -> Dict[int, Dict[str, List[int]]]:
    """
    Returns mapping: instances -> { tx_hash -> [latency_ms, ...] }
    For each transaction, collects the first finalization event from each
    validator file (one latency value per validator).
    """
    by_instances: Dict[int, Dict[str, List[int]]] = defaultdict(
        lambda: defaultdict(list)
    )
    seen_per_file: Dict[Tuple[int, str, str], bool] = {}
    any_file = False

    for file_path, instances, validator in iter_gatling_files(dir_path):
        any_file = True
        for tx_hash, latency_ms in parse_latency_occurrences(
            file_path, tx_window_start, tx_window_end
        ):
            file_key = (instances, validator, tx_hash)
            if file_key not in seen_per_file:
                seen_per_file[file_key] = True
                by_instances[instances][tx_hash].append(latency_ms)

    if not any_file:
        print(
            "Error: no gatling_i*_*.log files found in {0}".format(dir_path)
        )
        sys.exit(1)

    return by_instances


def compute_instance_stats(
    by_instances: Dict[int, Dict[str, List[int]]]
) -> Dict[int, Tuple[float, float, float, float, float, int]]:
    """
    For each instance count I:
    1. For each transaction, compute the average latency across all validators.
    2. Compute overall average, median, stddev, min, max across transactions.
    Returns: I -> (average_ms, median_ms, stddev_ms, min_ms, max_ms, num_txs).
    """
    result = {}
    for instances, tx_map in sorted(by_instances.items()):
        per_tx_averages = [
            statistics.mean(lats)
            for lats in tx_map.values()
            if lats
        ]
        if per_tx_averages:
            result[instances] = (
                statistics.mean(per_tx_averages),
                statistics.median(per_tx_averages),
                statistics.stdev(per_tx_averages) if len(per_tx_averages) >= 2 else 0.0,
                min(per_tx_averages),
                max(per_tx_averages),
                len(per_tx_averages),
            )
        else:
            result[instances] = (0.0, 0.0, 0.0, 0.0, 0.0, 0)
    return result


def write_stats_csv(
    stats: Dict[int, Tuple[float, float, float, float, float, int]],
    non_significant_pairs: List[Tuple[int, int, float, float, int, int]],
    stats_csv_path: str,
) -> None:
    os.makedirs(os.path.dirname(stats_csv_path), exist_ok=True)
    with open(stats_csv_path, "w") as f:
        # Section 1: Per-instance summary statistics
        f.write("=== PER-INSTANCE SUMMARY STATISTICS ===\n")
        f.write("instances,avg_ms,median_ms,stddev_ms,min_ms,max_ms,unique_txs\n")
        for instances in sorted(stats.keys()):
            average_ms, median_ms, std_ms, min_ms, max_ms, n = stats[instances]
            f.write("{0},{1:.3f},{2:.3f},{3:.3f},{4:.3f},{5:.3f},{6}\n".format(
                instances, average_ms, median_ms, std_ms, min_ms, max_ms, n
            ))
        f.write("\n")

        # Section 2: Kolmogorov-Smirnov test results (non-significant comparisons)
        if non_significant_pairs:
            f.write("=== KOLMOGOROV-SMIRNOV TESTS (Non-Significant Comparisons) ===\n")
            f.write("instance_1,instance_2,ks_statistic,p_value,n1,n2\n")
            for inst1, inst2, ks_stat, p_val, n1, n2 in non_significant_pairs:
                f.write("{0},{1},{2:.10f},{3:.10e},{4},{5}\n".format(
                    inst1, inst2, ks_stat, p_val, n1, n2
                ))
        else:
            f.write("=== KOLMOGOROV-SMIRNOV TESTS ===\n")
            f.write("No non-significant comparisons found.\n")


def write_plot_data_csv(
    by_instances: Dict[int, Dict[str, List[int]]],
    plot_data_csv_path: str,
) -> None:
    """Write per-transaction average latencies used for plotting."""
    os.makedirs(os.path.dirname(plot_data_csv_path), exist_ok=True)
    with open(plot_data_csv_path, "w") as f:
        f.write("instances,tx_hash,avg_latency_ms,num_validators\n")
        for instances in sorted(by_instances.keys()):
            tx_map = by_instances[instances]
            for tx_hash, validator_latencies in tx_map.items():
                if not validator_latencies:
                    continue
                avg_latency = statistics.mean(validator_latencies)
                f.write("{0},{1},{2:.6f},{3}\n".format(
                    instances, tx_hash, avg_latency, len(validator_latencies)
                ))


def prepare_data_for_statistical_test(
    by_instances: Dict[int, Dict[str, List[int]]]
) -> Dict[int, List[int]]:
    """Returns: instances -> list of all latency values (for KS testing)."""
    instance_data = {}
    for instances, tx_map in by_instances.items():
        all_latencies = []
        for validator_latencies in tx_map.values():
            all_latencies.extend(validator_latencies)
        instance_data[instances] = all_latencies
    return instance_data


def perform_ks_tests(
    by_instances: Dict[int, Dict[str, List[int]]]
) -> List[Tuple[int, int, float, float, int, int]]:
    """
    Pairwise Kolmogorov-Smirnov tests between all instance groups with
    Bonferroni correction. Returns the list of non-significant pairs.
    """
    non_significant_pairs: List[Tuple[int, int, float, float, int, int]] = []

    if not SCIPY_AVAILABLE:
        print(
            "\nWarning: scipy not available. Skipping Kolmogorov-Smirnov tests."
            "\nInstall with: pip install scipy"
        )
        return non_significant_pairs

    instance_data = prepare_data_for_statistical_test(by_instances)
    sorted_instances = sorted(instance_data.keys())

    if len(sorted_instances) < 2:
        print("\nInsufficient groups for statistical testing.")
        return non_significant_pairs

    n_comparisons = len(sorted_instances) * (len(sorted_instances) - 1) // 2
    alpha = 0.05
    corrected_alpha = alpha / n_comparisons

    print("\n" + "=" * 80)
    print("KOLMOGOROV-SMIRNOV TESTS (Non-Significant Comparisons Only)")
    print("=" * 80)
    print("\nTotal pairwise comparisons: {0}".format(n_comparisons))
    print("Bonferroni corrected alpha: {0:.6f}".format(corrected_alpha))
    print("\n" + "-" * 80)
    print("{0:<12} {1:<12} {2:<15} {3:<15} {4:<8} {5:<8}".format(
        'Instance 1', 'Instance 2', 'KS Statistic', 'p-value', 'n1', 'n2'
    ))
    print("-" * 80)

    for i, inst1 in enumerate(sorted_instances):
        for inst2 in sorted_instances[i + 1:]:
            data1 = instance_data[inst1]
            data2 = instance_data[inst2]
            try:
                ks_statistic, p_value = ks_2samp(data1, data2)
                if p_value >= corrected_alpha:
                    non_significant_pairs.append(
                        (inst1, inst2, ks_statistic, p_value, len(data1), len(data2))
                    )
                    p_str = (
                        "{0:.3e}".format(p_value) if p_value < 1e-10
                        else "{0:.6e}".format(p_value) if p_value < 0.0001
                        else "{0:.10f}".format(p_value)
                    )
                    print("instances={0:<4}  instances={1:<4}  {2:>14.6f}  {3:>15}  {4:>7}  {5:>7}".format(
                        inst1, inst2, ks_statistic, p_str, len(data1), len(data2)
                    ))
            except Exception as e:
                print("Error comparing instances={0} vs instances={1}: {2}".format(
                    inst1, inst2, e
                ))

    print("-" * 80)
    print("\nNon-significant pairs: {0}/{1}".format(
        len(non_significant_pairs), n_comparisons
    ))
    print("=" * 80 + "\n")

    return non_significant_pairs


def main() -> None:
    args = parse_args()
    check_consistency(args.dir)
    by_instances = aggregate_by_instances(
        args.dir, args.tx_window_start, args.tx_window_end
    )
    stats = compute_instance_stats(by_instances)

    for instances in sorted(stats.keys()):
        average_ms, median_ms, std_ms, min_ms, max_ms, n = stats[instances]
        print(
            "instances={0}: avg_ms={1:.3f}, median_ms={2:.3f}, "
            "stddev_ms={3:.3f}, min_ms={4:.3f}, max_ms={5:.3f}, "
            "unique_txs={6}".format(
                instances, average_ms, median_ms, std_ms, min_ms, max_ms, n
            )
        )

    non_significant_pairs = perform_ks_tests(by_instances)
    write_stats_csv(stats, non_significant_pairs, args.stats_csv)
    write_plot_data_csv(by_instances, args.plot_data_csv)


if __name__ == "__main__":
    main()
