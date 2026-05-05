#!/usr/bin/env python3
"""
Aggregate transaction finalization latency from Gatling logs.

Reads all files under logs/gatling named gatling_{hex}_i{I}_r{R}.log,
where {hex} is a hexadecimal validator identifier.
Extracts finalization events, deduplicates per transaction per validator
(using only the first finalization event per validator), computes average
latency across validators for each transaction, then computes the overall
average latency per instance count and writes CSV files with summary
statistics and per-transaction plot data. Plotting is handled separately
by postprocessing/plots.py.

This script is designed to be run from the postprocessing folder.
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
# CONFIGURATION: Easily change input/output directories and file names here
# ============================================================================

# Input directory: where to read gatling log files from
# This can be a relative path (from repo root) or absolute path
# Default: "logs/gatling" (relative to repo root)
INPUT_DIR = "logs/gatling"

# Output directory: where to write output files
# This can be a relative path (from repo root) or absolute path
# Default: same as INPUT_DIR
OUTPUT_DIR = None  # None means use INPUT_DIR

# Output file names (will be placed in OUTPUT_DIR) f
OUTPUT_CSV_NAME = "latency_vs_instances.csv"
OUTPUT_PNG_NAME = "latency_vs_instances.png"
OUTPUT_STATS_CSV_NAME = "stats.csv"
# Per-transaction plot data (used to generate figures)
OUTPUT_PLOT_DATA_CSV_NAME = "latency_plot_data.csv"

# ============================================================================
# END CONFIGURATION
# ============================================================================

# Match validator ID as hex string (no 'v' prefix in actual files)
# Files are named: gatling_{hex}_i{instances}_r{run}.log
FILENAME_RE = re.compile(r"^gatling_([0-9a-f]+)_i(\d+)_r(\d+)\.log$")
LOG_TS_RE = re.compile(r"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z)")
FINALIZE_RE = re.compile(
    r"Transaction ([a-f0-9]+) \(timestamp: (\d+) ms\) is now final in block"
)


def get_repo_root() -> str:
    """
    Get the repository root directory (one level up from postprocessing
    folder).
    """
    script_dir = os.path.dirname(os.path.abspath(__file__))
    return os.path.dirname(script_dir)


def parse_args() -> argparse.Namespace:
    repo_root = get_repo_root()

    # Resolve input directory: if relative, make it absolute relative to
    # repo root
    if os.path.isabs(INPUT_DIR):
        default_input_dir = INPUT_DIR
    else:
        default_input_dir = os.path.join(repo_root, INPUT_DIR)

    # Resolve output directory: if None, use input directory; if relative,
    # make it absolute
    if OUTPUT_DIR is None:
        default_output_dir = default_input_dir
    elif os.path.isabs(OUTPUT_DIR):
        default_output_dir = OUTPUT_DIR
    else:
        default_output_dir = os.path.join(repo_root, OUTPUT_DIR)

    # Build default output file paths
    default_csv = os.path.join(default_output_dir, OUTPUT_CSV_NAME)
    default_png = os.path.join(default_output_dir, OUTPUT_PNG_NAME)
    default_stats_csv = os.path.join(
        default_output_dir, OUTPUT_STATS_CSV_NAME
    )
    default_plot_data_csv = os.path.join(
        default_output_dir, OUTPUT_PLOT_DATA_CSV_NAME
    )

    parser = argparse.ArgumentParser(
        description="Aggregate Gatling latencies and plot vs instances"
    )
    parser.add_argument(
        "--dir",
        dest="dir",
        default=default_input_dir,
        help=(
            "Directory containing gatling_*_i*_r*.log files "
            "(default: from config)"
        ),
    )
    parser.add_argument(
        "--csv",
        dest="csv",
        default=default_csv,
        help="Output CSV path (default: from config)",
    )
    parser.add_argument(
        "--png",
        dest="png",
        default=default_png,
        help="Output PNG path (default: from config)",
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
        help=(
            "Output CSV path for per-transaction plot data "
            "(default: from config)"
        ),
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


def iter_gatling_files(dir_path: str) -> Iterable[Tuple[str, int, str, int]]:
    """
    Yields (file_path, instances, validator_id, run_index).
    validator_id is a hexadecimal string identifier.
    """
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
        v = m.group(1)  # validator id (string: numeric, hex, or location name)
        i = int(m.group(2))  # instances count
        r = int(m.group(3))  # run index
        yield os.path.join(dir_path, name), i, v, r


def check_consistency(dir_path: str) -> None:
    """
    Performs basic consistency checks over discovered Gatling logs:
    - Files must follow expected naming:
      gatling_{hex}_i{instances}_r{run}.log
      where {hex} is a hexadecimal validator identifier
    Exits with non-zero status if no matching files are found.
    """
    any_file = False
    for _, _, _, _ in iter_gatling_files(dir_path):
        any_file = True
        break

    if not any_file:
        print(
            (
                "Error: no gatling logs found matching expected pattern in "
                "{0}"
            ).format(dir_path)
        )
        sys.exit(1)


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
    base_seconds_ms = calendar.timegm(log_dt.timetuple()) * 1000
    micros_ms = int(log_dt.microsecond / 1000.0)
    log_ts_ms = base_seconds_ms + micros_ms
    tx_hash = fin_m.group(1)
    tx_ts_ms = int(fin_m.group(2))
    return log_ts_ms, tx_hash, tx_ts_ms


def parse_latency_occurrences(
    path: str,
    tx_window_start: Optional[int] = None,
    tx_window_end: Optional[int] = None,
) -> Iterable[Tuple[str, int]]:
    """
    Yields (tx_hash, latency_ms) for each matching line in a file.
    If tx_window_start or tx_window_end are provided (Unix ms), only
    transactions whose creation timestamp falls within the window are included.
    """
    try:
        with open(path, "r") as f:
            for line in f:
                parsed = parse_log_line(line)
                if not parsed:
                    continue
                log_ts_ms, tx_hash, tx_ts_ms = parsed
                if tx_window_start is not None and tx_ts_ms < tx_window_start:
                    continue
                if tx_window_end is not None and tx_ts_ms > tx_window_end:
                    continue
                yield tx_hash, log_ts_ms - tx_ts_ms
    except IOError:
        # Skip unreadable files
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
    by_instances = defaultdict(
        lambda: defaultdict(list)
    )
    # Track first occurrence per transaction per validator file
    # validator is a hexadecimal string identifier
    seen_per_file = {}
    any_file = False
    for (
        file_path,
        instances,
        validator,
        _run_idx,
    ) in iter_gatling_files(dir_path):
        any_file = True
        for tx_hash, latency_ms in parse_latency_occurrences(
            file_path, tx_window_start, tx_window_end
        ):
            # Only keep the first occurrence of each transaction per file
            file_key = (instances, validator, tx_hash)
            if file_key not in seen_per_file:
                seen_per_file[file_key] = True
                by_instances[instances][tx_hash].append(latency_ms)
    if not any_file:
        print(
            (
                "Error: no gatling_*_i*_r*.log files found in {0}"
            ).format(dir_path)
        )
        sys.exit(1)
    return by_instances


def compute_instance_stats(
    by_instances: Dict[int, Dict[str, List[int]]]
) -> Dict[int, Tuple[float, float, float, float, float, int]]:
    """
    For each instance count I:
    1. For each transaction, compute average of latency values from all
       validators (each validator contributes its first finalization event)
    2. Compute overall average across all unique transactions
    3. Compute median across per-transaction averages
    4. Compute standard deviation across per-transaction averages
    5. Compute min and max across per-transaction averages
    Returns mapping:
    I -> (average_latency_ms, median_latency_ms, stddev_latency_ms,
          min_latency_ms, max_latency_ms, num_unique_transactions).
    """
    result = {}
    for instances, tx_map in sorted(by_instances.items()):
        per_tx_averages = []
        for tx_hash, validator_latencies in tx_map.items():
            if not validator_latencies:
                continue
            # For each tx, compute the average latency across all vals
            # who reported it
            tx_average = statistics.mean(validator_latencies)
            per_tx_averages.append(tx_average)
        # per_tx_averages is the list of mean latencies for each unique tx
        # at the given number of instances
        if per_tx_averages:
            overall_average = statistics.mean(per_tx_averages)
            overall_median = statistics.median(per_tx_averages)
            overall_std = (
                statistics.stdev(per_tx_averages)
                if len(per_tx_averages) >= 2
                else 0.0
            )
            overall_min = min(per_tx_averages)
            overall_max = max(per_tx_averages)
            result[instances] = (
                overall_average,
                overall_median,
                overall_std,
                overall_min,
                overall_max,
                len(per_tx_averages),
            )
        else:
            result[instances] = (0.0, 0.0, 0.0, 0.0, 0.0, 0)
    return result


def write_csv(
    stats: Dict[int, Tuple[float, float, float, float, float, int]],
    csv_path: str,
) -> None:
    os.makedirs(os.path.dirname(csv_path), exist_ok=True)
    with open(csv_path, "w") as f:
        f.write(
            (
                "instances,average_latency_ms,median_latency_ms,"
                "stddev_latency_ms,min_latency_ms,max_latency_ms,"
                "num_transactions\n"
            )
        )
        for instances in sorted(stats.keys()):
            average_ms, median_ms, std_ms, min_ms, max_ms, n = stats[instances]
            f.write(
                "{0},{1:.3f},{2:.3f},"
                "{3:.3f},{4:.3f},{5:.3f},{6}\n".format(
                    instances, average_ms, median_ms, std_ms, min_ms, max_ms, n
                )
            )


def write_stats_csv(
    stats: Dict[int, Tuple[float, float, float, float, float, int]],
    ks_results: Tuple[
        List[Tuple[int, int, float, float, int, int]],
        Dict[int, Tuple[float, float, float, float, float, int]]
    ],
    stats_csv_path: str,
) -> None:
    """
    Write all terminal output statistics to a structured CSV file.
    """
    non_significant_pairs, group_stats = ks_results
    os.makedirs(os.path.dirname(stats_csv_path), exist_ok=True)

    with open(stats_csv_path, "w") as f:
        # Section 1: Per-instance summary statistics
        f.write("=== PER-INSTANCE SUMMARY STATISTICS (this is what we want) ===\n")
        f.write(
            "instances,avg_ms,median_ms,stddev_ms,min_ms,max_ms,unique_txs\n"
        )
        for instances in sorted(stats.keys()):
            average_ms, median_ms, std_ms, min_ms, max_ms, n = stats[instances]
            f.write(
                "{0},{1:.3f},{2:.3f},"
                "{3:.3f},{4:.3f},{5:.3f},{6}\n".format(
                    instances, average_ms, median_ms, std_ms, min_ms, max_ms, n
                )
            )
        f.write("\n")

        # Section 2: Kolmogorov-Smirnov test results
        # (non-significant comparisons)
        if non_significant_pairs:
            f.write(
                "=== KOLMOGOROV-SMIRNOV TESTS "
                "(Non-Significant Comparisons) ===\n"
            )
            f.write("instance_1,instance_2,ks_statistic,p_value,n1,n2\n")
            for inst1, inst2, ks_stat, p_val, n1, n2 in non_significant_pairs:
                f.write("{0},{1},{2:.10f},{3:.10e},{4},{5}\n".format(
                    inst1, inst2, ks_stat, p_val, n1, n2
                ))
        else:
            f.write("=== KOLMOGOROV-SMIRNOV TESTS ===\n")
            f.write("No non-significant comparisons found.\n")
        f.write("\n")

        # Section 3: Group summary statistics
        if group_stats:
            f.write("=== GROUP SUMMARY STATISTICS ===\n")
            f.write("instances,n,mean_ms,median_ms,std_ms,min_ms,max_ms\n")
            for inst in sorted(group_stats.keys()):
                (mean_val, median_val, std_val, min_val, max_val, n) = (
                    group_stats[inst]
                )
                f.write(
                    "{0},{1},{2:.2f},{3:.2f},"
                    "{4:.2f},{5:.2f},{6:.2f}\n".format(
                        inst, n, mean_val, median_val, std_val,
                        min_val, max_val
                    )
                )
        else:
            f.write("=== GROUP SUMMARY STATISTICS ===\n")
            f.write("No group statistics available.\n")


def write_plot_data_csv(
    by_instances: Dict[int, Dict[str, List[int]]],
    plot_data_csv_path: str,
) -> None:
    """
    Write the per-transaction average latencies used for plotting.

    Each row corresponds to one transaction at a given instance count:
    instances,tx_hash,avg_latency_ms,num_validators
    """
    os.makedirs(os.path.dirname(plot_data_csv_path), exist_ok=True)

    with open(plot_data_csv_path, "w") as f:
        f.write(
            "instances,tx_hash,avg_latency_ms,num_validators\n"
        )
        for instances in sorted(by_instances.keys()):
            tx_map = by_instances[instances]
            for tx_hash, validator_latencies in tx_map.items():
                if not validator_latencies:
                    continue
                avg_latency = statistics.mean(validator_latencies)
                f.write(
                    "{0},{1},{2:.6f},{3}\n".format(
                        instances, tx_hash, avg_latency, len(validator_latencies)
                    )
                )


def prepare_data_for_statistical_test(
    by_instances: Dict[int, Dict[str, List[int]]]
) -> Dict[int, List[int]]:
    """
    Prepare latency data for statistical testing.
    Returns: instances -> list of all latency values
    """
    instance_data = {}
    for instances, tx_map in by_instances.items():
        all_latencies = []
        for tx_hash, validator_latencies in tx_map.items():
            all_latencies.extend(validator_latencies)
        instance_data[instances] = all_latencies
    return instance_data


def perform_ks_tests(
    by_instances: Dict[int, Dict[str, List[int]]]
) -> Tuple[
    List[Tuple[int, int, float, float, int, int]],
    Dict[int, Tuple[float, float, float, float, float, int]]
]:
    """
    Perform Kolmogorov-Smirnov tests for pairwise comparisons between
    all instance groups with Bonferroni correction for multiple comparisons.
    Returns: (non_significant_pairs, group_stats)
    where non_significant_pairs is list of
    (inst1, inst2, ks_stat, p_value, n1, n2)
    and group_stats is dict of instances -> (mean, median, std, min, max, n)
    """
    non_significant_pairs = []
    group_stats = {}

    instance_data = prepare_data_for_statistical_test(by_instances)
    sorted_instances = sorted(instance_data.keys())

    # Compute group stats regardless of scipy availability
    for inst in sorted_instances:
        data = instance_data[inst]
        mean_val = statistics.mean(data)
        median_val = statistics.median(data)
        std_val = statistics.stdev(data) if len(data) >= 2 else 0.0
        min_val = min(data)
        max_val = max(data)
        group_stats[inst] = (
            mean_val, median_val, std_val, min_val, max_val, len(data)
        )

    if not SCIPY_AVAILABLE:
        print(
            "\nWarning: scipy not available. "
            "Skipping Kolmogorov-Smirnov tests."
            "\nInstall with: pip install scipy"
        )
        return non_significant_pairs, group_stats

    if len(sorted_instances) < 2:
        print("\nInsufficient groups for statistical testing.")
        return non_significant_pairs, group_stats

    print("\n" + "=" * 80)
    print("KOLMOGOROV-SMIRNOV TESTS (Non-Significant Comparisons Only)")
    print("=" * 80)

    # Calculate number of comparisons for Bonferroni correction
    n_instances = len(sorted_instances)
    n_comparisons = n_instances * (n_instances - 1) // 2
    alpha = 0.05
    corrected_alpha = alpha / n_comparisons

    print("\nTotal number of pairwise comparisons: {0}".format(n_comparisons))
    print("Uncorrected alpha level: {0}".format(alpha))
    print("Bonferroni corrected alpha level: {0:.6f}".format(corrected_alpha))
    print(
        "\nShowing only NON-SIGNIFICANT comparisons "
        "(p-value >= corrected alpha)"
    )
    print(
        "\n" + "-" * 80
    )
    print(
        "{0:<12} {1:<12} {2:<15} "
        "{3:<15} {4:<8} {5:<8}".format(
            'Instance 1', 'Instance 2', 'KS Statistic',
            'p-value', 'n1', 'n2'
        )
    )
    print("-" * 80)

    non_significant_pairs = []
    all_comparisons = []

    for i, inst1 in enumerate(sorted_instances):
        for inst2 in sorted_instances[i + 1:]:
            data1 = instance_data[inst1]
            data2 = instance_data[inst2]
            n1 = len(data1)
            n2 = len(data2)

            try:
                # Perform Kolmogorov-Smirnov test
                ks_statistic, p_value = ks_2samp(data1, data2)

                # Check significance after Bonferroni correction
                is_significant = p_value < corrected_alpha

                all_comparisons.append(
                    (
                        inst1, inst2, ks_statistic, p_value,
                        is_significant, n1, n2
                    )
                )

                # Only collect and print non-significant pairs
                if not is_significant:
                    non_significant_pairs.append(
                        (inst1, inst2, ks_statistic, p_value, n1, n2)
                    )

                    # Format output with better precision for p-values
                    # Use scientific notation for very small p-values
                    if p_value < 1e-10:
                        p_value_str = "{0:.3e}".format(p_value)
                    elif p_value < 0.0001:
                        p_value_str = "{0:.6e}".format(p_value)
                    else:
                        p_value_str = "{0:.10f}".format(p_value)

                    print(
                        "instances={0:<4}  instances={1:<4}  "
                        "{2:>14.6f}  {3:>15}  "
                        "{4:>7}  {5:>7}".format(
                            inst1, inst2, ks_statistic, p_value_str, n1, n2
                        )
                    )
            except Exception as e:
                print(
                    "Error comparing instances={0} vs "
                    "instances={1}: {2}".format(inst1, inst2, e)
                )

    print("-" * 80)

    # Summary of non-significant comparisons
    if non_significant_pairs:
        print("\n" + "=" * 80)
        print("SUMMARY OF NON-SIGNIFICANT COMPARISONS")
        print("=" * 80)
        print(
            "{0:<12} {1:<12} {2:<15} "
            "{3:<15} {4:<8} {5:<8}".format(
                'Instance 1', 'Instance 2', 'KS Statistic',
                'p-value', 'n1', 'n2'
            )
        )
        print("-" * 80)
        for inst1, inst2, ks_stat, p_val, n1, n2 in non_significant_pairs:
            # Format p-value with better precision
            if p_val < 1e-10:
                p_val_str = "{0:.3e}".format(p_val)
            elif p_val < 0.0001:
                p_val_str = "{0:.6e}".format(p_val)
            else:
                p_val_str = "{0:.10f}".format(p_val)
            print(
                "instances={0:<4}  instances={1:<4}  "
                "{2:>14.6f}  {3:>15}  {4:>7}  {5:>7}".format(
                    inst1, inst2, ks_stat, p_val_str, n1, n2
                )
            )
        print(
            "\nTotal non-significant pairs: "
            "{0}/{1}".format(len(non_significant_pairs), n_comparisons)
        )
        print(
            "Total significant pairs: "
            "{0}/{1}".format(
                n_comparisons - len(non_significant_pairs), n_comparisons
            )
        )
    else:
        print(
            "\nAll comparisons are significant after Bonferroni correction "
            "({0}/{1} significant)".format(n_comparisons, n_comparisons)
        )

    # Additional statistics per group (already computed above, just print them)
    print("\n" + "=" * 80)
    print("GROUP SUMMARY STATISTICS")
    print("=" * 80)
    print(
        "{0:<12} {1:<8} {2:<12} {3:<12} "
        "{4:<12} {5:<10} {6:<10}".format(
            'Instances', 'n', 'Mean (ms)', 'Median (ms)',
            'Std (ms)', 'Min (ms)', 'Max (ms)'
        )
    )
    print("-" * 80)
    for inst in sorted_instances:
        mean_val, median_val, std_val, min_val, max_val, n = group_stats[inst]
        print(
            "instances={0:<4}  {1:>7}  {2:>11.2f}  "
            "{3:>11.2f}  {4:>11.2f}  {5:>9.2f}  "
            "{6:>9.2f}".format(
                inst, n, mean_val, median_val, std_val, min_val, max_val
            )
        )

    print("\n" + "=" * 80 + "\n")
    return non_significant_pairs, group_stats


def main() -> None:
    args = parse_args()
    # Run consistency checks before aggregation to catch mismatches early
    check_consistency(args.dir)
    by_instances = aggregate_by_instances(
        args.dir, args.tx_window_start, args.tx_window_end
    )
    stats = compute_instance_stats(by_instances)

    # Console summary
    for instances in sorted(stats.keys()):
        average_ms, median_ms, std_ms, min_ms, max_ms, n = stats[instances]
        print(
            "instances={0}: avg_ms={1:.3f}, "
            "median_ms={2:.3f}, stddev_ms={3:.3f}, "
            "min_ms={4:.3f}, max_ms={5:.3f}, "
            "unique_txs={6}".format(
                instances, average_ms, median_ms, std_ms, min_ms, max_ms, n
            )
        )

    # Perform Kolmogorov-Smirnov tests with multiple comparison correction
    ks_results = perform_ks_tests(by_instances)

    write_csv(stats, args.csv)
    write_stats_csv(stats, ks_results, args.stats_csv)
    # Write per-transaction plot data so figures can be regenerated by
    # postprocessing/plots.py
    write_plot_data_csv(by_instances, args.plot_data_csv)


if __name__ == "__main__":
    main()
