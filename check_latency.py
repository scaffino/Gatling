#!/usr/bin/env python3
"""
Aggregate transaction finalization latency from Gatling logs.

Reads all files under logs/gatling named gatling_v{V}_i{I}_r{R}.log,
extracts finalization events, deduplicates per transaction per validator
(using only the first finalization event per validator), computes average
latency across validators for each transaction, then computes the overall
average latency per instance count, writes a CSV, and plots latency vs
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

try:
    from scipy.stats import ks_2samp
    SCIPY_AVAILABLE = True
except ImportError:
    SCIPY_AVAILABLE = False


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
        default=os.path.join("logs", "gatling-remote"),
        help="Directory containing gatling_v*_i*_r*.log files",
    )
    parser.add_argument(
        "--csv",
        dest="csv",
        default=os.path.join(
            "logs", "gatling-remote", "latency_vs_instances.csv"
        ),
        help="Output CSV path",
    )
    parser.add_argument(
        "--png",
        dest="png",
        default=os.path.join(
            "logs", "gatling-remote", "latency_vs_instances.png"
        ),
        help="Output PNG path",
    )
    return parser.parse_args()


def iter_gatling_files(dir_path: str) -> Iterable[Tuple[str, int, int, int]]:
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
        v = int(m.group(1))  # validator id
        i = int(m.group(2))  # instances count
        r = int(m.group(3))  # run index
        yield os.path.join(dir_path, name), i, v, r


def check_consistency(dir_path: str) -> None:
    """
    Performs basic consistency checks over discovered Gatling logs:
    - Files must follow expected naming:
      gatling_v{validator}_i{instances}_r{run}.log
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
    for (
        file_path,
        instances,
        validator,
        _run_idx,
    ) in iter_gatling_files(dir_path):
        any_file = True
        for tx_hash, latency_ms in parse_latency_occurrences(file_path):
            # Only keep the first occurrence of each transaction per file
            file_key = (instances, validator, tx_hash)
            if file_key not in seen_per_file:
                seen_per_file[file_key] = True
                by_instances[instances][tx_hash].append(latency_ms)
    if not any_file:
        print(
            (
                "Error: no gatling_v*_i*_r*.log files found in {0}"
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
    result: Dict[int, Tuple[float, float, float, float, float, int]] = {}
    for instances, tx_map in sorted(by_instances.items()):
        per_tx_averages: List[float] = []
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
                statistics.stdev(per_tx_averages) if len(per_tx_averages) >= 2
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
                f"{instances},{average_ms:.3f},{median_ms:.3f},"
                f"{std_ms:.3f},{min_ms:.3f},{max_ms:.3f},{n}\n"
            )


def prepare_plot_data(
    by_instances: Dict[int, Dict[str, List[int]]]
) -> Tuple[
    List[List[int]],
    List[int],
    List[Tuple[float, float, float, float, float]],
    List[float],
]:
    """
    Prepare data for plotting: extract latencies, calculate percentiles
    and means.
    Returns: (data_for_boxplot, instance_labels, percentile_data, mean_data)
    """
    data_for_boxplot = []
    instance_labels = []
    percentile_data = []
    mean_data = []
    try:
        import numpy as np
    except ImportError:
        print("Error: numpy not available")
        return [], [], [], []

    for instances in sorted(by_instances.keys()):
        all_latencies = []
        for tx_hash, validator_latencies in by_instances[instances].items():
            all_latencies.extend(validator_latencies)
        if all_latencies:
            data_for_boxplot.append(all_latencies)
            instance_labels.append(instances)
            # Calculate 10th, 50th (median), and 90th percentiles, plus min/max
            p10 = np.percentile(all_latencies, 10)
            p50 = np.percentile(all_latencies, 50)
            p90 = np.percentile(all_latencies, 90)
            p_min = np.min(all_latencies)
            p_max = np.max(all_latencies)
            mean_val = np.mean(all_latencies)
            percentile_data.append((p10, p50, p90, p_min, p_max))
            mean_data.append(mean_val)

    return data_for_boxplot, instance_labels, percentile_data, mean_data


def plot_boxplot_with_mean(
    by_instances: Dict[int, Dict[str, List[int]]], png_path: str
) -> None:
    """Create a box plot with mean overlay using seaborn."""
    try:
        import matplotlib.pyplot as plt
        import seaborn as sns
        from matplotlib.patches import Rectangle
    except Exception as e:
        print(
            (
                "Warning: matplotlib/seaborn/numpy not available; "
                "skipping plot: {0}"
            ).format(e)
        )
        return

    data_for_boxplot, instance_labels, percentile_data, mean_data = (
        prepare_plot_data(by_instances)
    )

    if not data_for_boxplot:
        print("Warning: no data to plot")
        return

    # Set seaborn style
    sns.set_style("whitegrid")
    plt.figure(figsize=(7, 4))

    # Manually create box plot with 10th-90th percentiles
    positions = range(1, len(instance_labels) + 1)
    box_width = 0.6

    for i, (pos, (p10, p50, p90, p_min, p_max), mean_val) in enumerate(
        zip(positions, percentile_data, mean_data)
    ):
        x_center = pos
        x_left = x_center - box_width / 2
        x_right = x_center + box_width / 2

        # Draw box (rectangle from p10 to p90)
        box_height = p90 - p10
        rect = Rectangle(
            (x_left, p10),
            box_width,
            box_height,
            facecolor=sns.color_palette("pastel")[0],
            alpha=0.7,
            edgecolor='black',
            linewidth=1
        )
        plt.gca().add_patch(rect)

        # Draw median line
        plt.plot(
            [x_left, x_right],
            [p50, p50],
            color='black',
            linewidth=1.5
        )

        # Draw whiskers (extend from box edges to min/max)
        whisker_width = box_width * 0.3
        # Lower whisker (from p10 to p_min)
        plt.plot(
            [x_center, x_center],
            [p_min, p10],
            color='black',
            linewidth=1
        )
        # Upper whisker (from p90 to p_max)
        plt.plot(
            [x_center, x_center],
            [p90, p_max],
            color='black',
            linewidth=1
        )

        # Draw caps (horizontal lines at whisker ends)
        # Lower cap at p_min
        plt.plot(
            [x_center - whisker_width / 2, x_center + whisker_width / 2],
            [p_min, p_min],
            color='black',
            linewidth=1
        )
        # Upper cap at p_max
        plt.plot(
            [x_center - whisker_width / 2, x_center + whisker_width / 2],
            [p_max, p_max],
            color='black',
            linewidth=1
        )

        # Overlay mean as a marker
        plt.plot(
            x_center,
            mean_val,
            marker='D',
            markersize=5,
            color='red',
            markeredgecolor='darkred',
            markeredgewidth=1,
            label='Mean' if i == 0 else ''
        )

    # Set x-axis labels and limits
    ticks = list(range(0, 12))
    plt.xticks(ticks, ticks)
    plt.xlim(0.5, 11.5)

    plt.xlabel("Number of instances")
    plt.ylabel("Latency (ms)")
    plt.yscale('log')
    plt.title(
        "Transaction latency distribution vs consensus instances (with mean)"
    )
    plt.legend(loc='best')
    plt.grid(True, linestyle=":", alpha=0.6, axis='y')
    os.makedirs(os.path.dirname(png_path), exist_ok=True)
    plt.tight_layout()
    plt.savefig(png_path, dpi=160)
    plt.close()


def plot_boxplot_with_scatter(
    by_instances: Dict[int, Dict[str, List[int]]], png_path: str
) -> None:
    """Create a box plot with scatter overlay using seaborn."""
    try:
        import matplotlib.pyplot as plt
        import seaborn as sns
        from matplotlib.patches import Rectangle
        import numpy as np
    except Exception as e:
        print(
            (
                "Warning: matplotlib/seaborn/numpy not available; "
                "skipping plot: {0}"
            ).format(e)
        )
        return

    data_for_boxplot, instance_labels, percentile_data, _ = (
        prepare_plot_data(by_instances)
    )

    if not data_for_boxplot:
        print("Warning: no data to plot")
        return

    # Set seaborn style
    sns.set_style("whitegrid")
    plt.figure(figsize=(7, 4))

    # Manually create box plot with 10th-90th percentiles
    positions = range(1, len(instance_labels) + 1)
    box_width = 0.6

    for i, (pos, (p10, p50, p90, p_min, p_max)) in enumerate(
        zip(positions, percentile_data)
    ):
        x_center = pos
        x_left = x_center - box_width / 2
        x_right = x_center + box_width / 2

        # Draw box (rectangle from p10 to p90)
        box_height = p90 - p10
        rect = Rectangle(
            (x_left, p10),
            box_width,
            box_height,
            facecolor=sns.color_palette("pastel")[0],
            alpha=0.7,
            edgecolor='black',
            linewidth=1
        )
        plt.gca().add_patch(rect)

        # Draw median line
        plt.plot(
            [x_left, x_right],
            [p50, p50],
            color='black',
            linewidth=1.5
        )

        # Draw whiskers (extend from box edges to min/max)
        whisker_width = box_width * 0.3
        # Lower whisker (from p10 to p_min)
        plt.plot(
            [x_center, x_center],
            [p_min, p10],
            color='black',
            linewidth=1
        )
        # Upper whisker (from p90 to p_max)
        plt.plot(
            [x_center, x_center],
            [p90, p_max],
            color='black',
            linewidth=1
        )

        # Draw caps (horizontal lines at whisker ends)
        # Lower cap at p_min
        plt.plot(
            [x_center - whisker_width / 2, x_center + whisker_width / 2],
            [p_min, p_min],
            color='black',
            linewidth=1
        )
        # Upper cap at p_max
        plt.plot(
            [x_center - whisker_width / 2, x_center + whisker_width / 2],
            [p_max, p_max],
            color='black',
            linewidth=1
        )

        # Overlay scatter plot of all data points
        x_scatter = np.random.normal(x_center, 0.05, len(data_for_boxplot[i]))
        plt.scatter(
            x_scatter,
            data_for_boxplot[i],
            alpha=0.3,
            s=3,
            color='darkblue',
            zorder=10
        )

    # Set x-axis labels and limits
    ticks = list(range(0, 12))
    plt.xticks(ticks, ticks)
    plt.xlim(0.5, 11.5)

    plt.xlabel("Number of instances")
    plt.ylabel("Latency (ms)")
    plt.yscale('log')
    plt.title(
        "Transaction latency distribution vs consensus instances "
        "(with scatter)"
    )
    plt.grid(True, linestyle=":", alpha=0.6, axis='y')
    os.makedirs(os.path.dirname(png_path), exist_ok=True)
    plt.tight_layout()
    plt.savefig(png_path, dpi=160)
    plt.close()


def plot_boxplot_with_mean_and_scatter(
    by_instances: Dict[int, Dict[str, List[int]]], png_path: str
) -> None:
    """Create a box plot with both mean and scatter overlay using seaborn."""
    try:
        import matplotlib.pyplot as plt
        import seaborn as sns
        from matplotlib.patches import Rectangle
        import numpy as np
    except Exception as e:
        print(
            (
                "Warning: matplotlib/seaborn/numpy not available; "
                "skipping plot: {0}"
            ).format(e)
        )
        return

    data_for_boxplot, instance_labels, percentile_data, mean_data = (
        prepare_plot_data(by_instances)
    )

    if not data_for_boxplot:
        print("Warning: no data to plot")
        return

    # Set seaborn style
    sns.set_style("whitegrid")
    plt.figure(figsize=(7, 4))

    # Manually create box plot with 10th-90th percentiles
    positions = range(1, len(instance_labels) + 1)
    box_width = 0.6

    for i, (pos, (p10, p50, p90, p_min, p_max), mean_val) in enumerate(
        zip(positions, percentile_data, mean_data)
    ):
        x_center = pos
        x_left = x_center - box_width / 2
        x_right = x_center + box_width / 2

        # Draw box (rectangle from p10 to p90)
        box_height = p90 - p10
        rect = Rectangle(
            (x_left, p10),
            box_width,
            box_height,
            facecolor=sns.color_palette("pastel")[0],
            alpha=0.7,
            edgecolor='black',
            linewidth=1
        )
        plt.gca().add_patch(rect)

        # Draw median line
        plt.plot(
            [x_left, x_right],
            [p50, p50],
            color='black',
            linewidth=1.5
        )

        # Draw whiskers (extend from box edges to min/max)
        whisker_width = box_width * 0.3
        # Lower whisker (from p10 to p_min)
        plt.plot(
            [x_center, x_center],
            [p_min, p10],
            color='black',
            linewidth=1
        )
        # Upper whisker (from p90 to p_max)
        plt.plot(
            [x_center, x_center],
            [p90, p_max],
            color='black',
            linewidth=1
        )

        # Draw caps (horizontal lines at whisker ends)
        # Lower cap at p_min
        plt.plot(
            [x_center - whisker_width / 2, x_center + whisker_width / 2],
            [p_min, p_min],
            color='black',
            linewidth=1
        )
        # Upper cap at p_max
        plt.plot(
            [x_center - whisker_width / 2, x_center + whisker_width / 2],
            [p_max, p_max],
            color='black',
            linewidth=1
        )

        # Overlay scatter plot of all data points
        x_scatter = np.random.normal(x_center, 0.05, len(data_for_boxplot[i]))
        plt.scatter(
            x_scatter,
            data_for_boxplot[i],
            alpha=0.3,
            s=3,
            color='darkblue',
            zorder=10
        )

        # Overlay mean as a marker
        plt.plot(
            x_center,
            mean_val,
            marker='D',
            markersize=5,
            color='red',
            markeredgecolor='darkred',
            markeredgewidth=1,
            label='Mean' if i == 0 else '',
            zorder=11
        )

    # Set x-axis labels and limits
    ticks = list(range(0, 12))
    plt.xticks(ticks, ticks)
    plt.xlim(0.5, 11.5)

    plt.xlabel("Number of instances")
    plt.ylabel("Latency (ms)")
    plt.yscale('log')
    plt.title(
        "Transaction latency distribution vs consensus instances "
        "(with mean and scatter)"
    )
    plt.legend(loc='best')
    plt.grid(True, linestyle=":", alpha=0.6, axis='y')
    os.makedirs(os.path.dirname(png_path), exist_ok=True)
    plt.tight_layout()
    plt.savefig(png_path, dpi=160)
    plt.close()


def prepare_data_for_statistical_test(
    by_instances: Dict[int, Dict[str, List[int]]]
) -> Dict[int, List[int]]:
    """
    Prepare latency data for statistical testing.
    Returns: instances -> list of all latency values
    """
    instance_data: Dict[int, List[int]] = {}
    for instances, tx_map in by_instances.items():
        all_latencies = []
        for tx_hash, validator_latencies in tx_map.items():
            all_latencies.extend(validator_latencies)
        instance_data[instances] = all_latencies
    return instance_data


def perform_ks_tests(
    by_instances: Dict[int, Dict[str, List[int]]]
) -> None:
    """
    Perform Kolmogorov-Smirnov tests for pairwise comparisons between
    all instance groups with Bonferroni correction for multiple comparisons.
    """
    if not SCIPY_AVAILABLE:
        print(
            "\nWarning: scipy not available. "
            "Skipping Kolmogorov-Smirnov tests."
            "\nInstall with: pip install scipy"
        )
        return

    instance_data = prepare_data_for_statistical_test(by_instances)
    sorted_instances = sorted(instance_data.keys())

    if len(sorted_instances) < 2:
        print("\nInsufficient groups for statistical testing.")
        return

    print("\n" + "=" * 80)
    print("KOLMOGOROV-SMIRNOV TESTS (Non-Significant Comparisons Only)")
    print("=" * 80)

    # Calculate number of comparisons for Bonferroni correction
    n_instances = len(sorted_instances)
    n_comparisons = n_instances * (n_instances - 1) // 2
    alpha = 0.05
    corrected_alpha = alpha / n_comparisons

    print(f"\nTotal number of pairwise comparisons: {n_comparisons}")
    print(f"Uncorrected alpha level: {alpha}")
    print(f"Bonferroni corrected alpha level: {corrected_alpha:.6f}")
    print(
        "\nShowing only NON-SIGNIFICANT comparisons "
        "(p-value >= corrected alpha)"
    )
    print(
        "\n" + "-" * 80
    )
    print(
        f"{'Instance 1':<12} {'Instance 2':<12} {'KS Statistic':<15} "
        f"{'p-value':<15} {'n1':<8} {'n2':<8}"
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
                        p_value_str = f"{p_value:.3e}"
                    elif p_value < 0.0001:
                        p_value_str = f"{p_value:.6e}"
                    else:
                        p_value_str = f"{p_value:.10f}"

                    print(
                        f"instances={inst1:<4}  instances={inst2:<4}  "
                        f"{ks_statistic:>14.6f}  {p_value_str:>15}  "
                        f"{n1:>7}  {n2:>7}"
                    )
            except Exception as e:
                print(
                    f"Error comparing instances={inst1} vs "
                    f"instances={inst2}: {e}"
                )

    print("-" * 80)

    # Summary of non-significant comparisons
    if non_significant_pairs:
        print("\n" + "=" * 80)
        print("SUMMARY OF NON-SIGNIFICANT COMPARISONS")
        print("=" * 80)
        print(
            f"{'Instance 1':<12} {'Instance 2':<12} {'KS Statistic':<15} "
            f"{'p-value':<15} {'n1':<8} {'n2':<8}"
        )
        print("-" * 80)
        for inst1, inst2, ks_stat, p_val, n1, n2 in non_significant_pairs:
            # Format p-value with better precision
            if p_val < 1e-10:
                p_val_str = f"{p_val:.3e}"
            elif p_val < 0.0001:
                p_val_str = f"{p_val:.6e}"
            else:
                p_val_str = f"{p_val:.10f}"
            print(
                f"instances={inst1:<4}  instances={inst2:<4}  "
                f"{ks_stat:>14.6f}  {p_val_str:>15}  {n1:>7}  {n2:>7}"
            )
        print(
            f"\nTotal non-significant pairs: "
            f"{len(non_significant_pairs)}/{n_comparisons}"
        )
        print(
            f"Total significant pairs: "
            f"{n_comparisons - len(non_significant_pairs)}/{n_comparisons}"
        )
    else:
        print(
            f"\nAll comparisons are significant after Bonferroni correction "
            f"({n_comparisons}/{n_comparisons} significant)"
        )

    # Additional statistics per group
    print("\n" + "=" * 80)
    print("GROUP SUMMARY STATISTICS")
    print("=" * 80)
    print(
        f"{'Instances':<12} {'n':<8} {'Mean (ms)':<12} {'Median (ms)':<12} "
        f"{'Std (ms)':<12} {'Min (ms)':<10} {'Max (ms)':<10}"
    )
    print("-" * 80)
    for inst in sorted_instances:
        data = instance_data[inst]
        mean_val = statistics.mean(data)
        median_val = statistics.median(data)
        std_val = statistics.stdev(data) if len(data) >= 2 else 0.0
        min_val = min(data)
        max_val = max(data)
        print(
            f"instances={inst:<4}  {len(data):>7}  {mean_val:>11.2f}  "
            f"{median_val:>11.2f}  {std_val:>11.2f}  {min_val:>9.2f}  "
            f"{max_val:>9.2f}"
        )

    print("\n" + "=" * 80 + "\n")


def main() -> None:
    args = parse_args()
    # Run consistency checks before aggregation to catch mismatches early
    check_consistency(args.dir)
    by_instances = aggregate_by_instances(args.dir)
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
    perform_ks_tests(by_instances)

    write_csv(stats, args.csv)
    # Generate three plots: one with mean overlay, one with scatter overlay,
    # and one with both mean and scatter
    base_path = args.png
    if base_path.endswith('.png'):
        base_path = base_path[:-4]
    plot_boxplot_with_mean(by_instances, f"{base_path}_mean.png")
    plot_boxplot_with_scatter(by_instances, f"{base_path}_scatter.png")
    plot_boxplot_with_mean_and_scatter(
        by_instances, f"{base_path}_mean_scatter.png"
    )


if __name__ == "__main__":
    main()
