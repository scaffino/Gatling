#!/usr/bin/env python3
"""
Generate latency plots (PDF) from precomputed CSV data.

Reads latency_plot_data.csv produced by generate_csv.py and generates
three PDF plots:
  - latency_vs_instances_mean.pdf         (box + mean overlay)
  - latency_vs_instances_scatter.pdf      (box + scatter overlay)
  - latency_vs_instances_mean_scatter.pdf (box + mean + scatter + fit curve)
"""

import argparse
import os
import csv
from collections import defaultdict
from typing import Dict, List, Tuple


def get_repo_root() -> str:
    script_dir = os.path.dirname(os.path.abspath(__file__))
    return os.path.dirname(script_dir)


def parse_args() -> argparse.Namespace:
    repo_root = get_repo_root()
    default_output_dir = os.path.join(repo_root, "logs", "gatling")
    default_plot_data_csv = os.path.join(default_output_dir, "latency_plot_data.csv")
    default_pdf = os.path.join(default_output_dir, "latency_vs_instances.pdf")

    parser = argparse.ArgumentParser(
        description="Generate latency vs instances PDF plots from precomputed CSV data"
    )
    parser.add_argument(
        "--plot-data-csv",
        dest="plot_data_csv",
        default=default_plot_data_csv,
        help="CSV file with per-transaction average latencies (default: latency_plot_data.csv)",
    )
    parser.add_argument(
        "--pdf",
        dest="pdf",
        default=default_pdf,
        help=(
            "Base PDF path for plots (default: latency_vs_instances.pdf). "
            "The script appends _mean, _scatter, _mean_scatter."
        ),
    )
    return parser.parse_args()


def load_plot_data(path: str) -> Dict[int, List[float]]:
    """
    Load per-transaction average latencies from latency_plot_data.csv.
    Returns: instances -> [avg_latency_ms, ...]
    """
    by_instances_avg: Dict[int, List[float]] = defaultdict(list)
    try:
        with open(path, newline="") as f:
            reader = csv.DictReader(f)
            for row in reader:
                inst = int(row["instances"])
                avg = float(row["avg_latency_ms"])
                by_instances_avg[inst].append(avg)
    except FileNotFoundError:
        print("Error: plot data CSV not found: {0}".format(path))
        return {}
    except KeyError as e:
        print("Error: missing expected column in plot data CSV: {0}".format(e))
        return {}
    return by_instances_avg


def prepare_plot_data_from_averages(
    by_instances_avg: Dict[int, List[float]]
) -> Tuple[
    List[List[float]],
    List[int],
    List[Tuple[float, float, float, float, float]],
    List[float],
]:
    data_for_boxplot: List[List[float]] = []
    instance_labels: List[int] = []
    percentile_data: List[Tuple[float, float, float, float, float]] = []
    mean_data: List[float] = []

    try:
        import numpy as np
    except ImportError:
        print("Error: numpy not available")
        return [], [], [], []

    excluded_instances = {11, 49}
    for instances in sorted(by_instances_avg.keys()):
        if instances in excluded_instances:
            continue
        per_tx_averages = by_instances_avg[instances]
        if not per_tx_averages:
            continue

        data_for_boxplot.append(per_tx_averages)
        instance_labels.append(instances)

        p10 = np.percentile(per_tx_averages, 10)
        p50 = np.percentile(per_tx_averages, 50)
        p90 = np.percentile(per_tx_averages, 90)
        p_min = np.min(per_tx_averages)
        p_max = np.max(per_tx_averages)
        mean_val = float(np.mean(per_tx_averages))
        percentile_data.append((p10, p50, p90, p_min, p_max))
        mean_data.append(mean_val)

    return data_for_boxplot, instance_labels, percentile_data, mean_data


def plot_boxplot_with_mean(
    by_instances_avg: Dict[int, List[float]], pdf_path: str
) -> None:
    try:
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
        import seaborn as sns
        from matplotlib.lines import Line2D
        from matplotlib.patches import Rectangle
    except Exception as e:
        print("Warning: matplotlib/seaborn/numpy not available; skipping plot: {0}".format(e))
        return

    data_for_boxplot, instance_labels, percentile_data, mean_data = \
        prepare_plot_data_from_averages(by_instances_avg)
    if not data_for_boxplot:
        print("Warning: no data to plot")
        return

    sns.set_style("ticks")
    plt.figure(figsize=(7, 4))
    positions = range(1, len(instance_labels) + 1)
    box_width = 0.6

    for i, (pos, (p10, p50, p90, p_min, p_max), mean_val) in enumerate(
        zip(positions, percentile_data, mean_data)
    ):
        x_center = pos
        x_left = x_center - box_width / 2
        x_right = x_center + box_width / 2
        whisker_width = box_width * 0.3

        plt.gca().add_patch(Rectangle(
            (x_left, p10), box_width, p90 - p10,
            facecolor=sns.color_palette("pastel")[0],
            alpha=0.7, edgecolor="black", linewidth=1,
        ))
        plt.plot([x_left, x_right], [p50, p50], color="black", linewidth=1.5,
                 label="Median" if i == 0 else "")
        plt.plot([x_center, x_center], [p_min, p10], color="black", linewidth=1)
        plt.plot([x_center, x_center], [p90, p_max], color="black", linewidth=1)
        plt.plot([x_center - whisker_width / 2, x_center + whisker_width / 2],
                 [p_min, p_min], color="black", linewidth=1)
        plt.plot([x_center - whisker_width / 2, x_center + whisker_width / 2],
                 [p_max, p_max], color="black", linewidth=1)
        plt.plot(x_center, mean_val, marker="D", markersize=5, color="red",
                 markeredgecolor="darkred", markeredgewidth=1,
                 label="Mean" if i == 0 else "")

    n = len(instance_labels)
    plt.xticks(list(range(1, n + 1)), instance_labels)
    plt.xlim(0.5, n + 0.5)
    plt.xlabel("K, number of parallel component protocol instances")
    plt.ylabel("Transaction Latency (ms)")
    plt.ylim(bottom=0)
    handles, labels = plt.gca().get_legend_handles_labels()
    handles.append(Line2D([0], [0], color="black", linewidth=1.5, label="Median"))
    labels.append("Median")
    plt.legend(handles, labels, loc="best")
    plt.grid(True, linestyle=":", alpha=0.6, axis="y")
    ax = plt.gca()
    for spine in ax.spines.values():
        spine.set_visible(False)
    os.makedirs(os.path.dirname(pdf_path), exist_ok=True)
    plt.tight_layout()
    plt.savefig(pdf_path)
    plt.close()


def plot_boxplot_with_scatter(
    by_instances_avg: Dict[int, List[float]], pdf_path: str
) -> None:
    try:
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
        import seaborn as sns
        import numpy as np
        from matplotlib.patches import Rectangle
    except Exception as e:
        print("Warning: matplotlib/seaborn/numpy not available; skipping plot: {0}".format(e))
        return

    data_for_boxplot, instance_labels, percentile_data, _ = \
        prepare_plot_data_from_averages(by_instances_avg)
    if not data_for_boxplot:
        print("Warning: no data to plot")
        return

    sns.set_style("ticks")
    plt.figure(figsize=(7, 4))
    positions = range(1, len(instance_labels) + 1)
    box_width = 0.6

    for i, (pos, (p10, p50, p90, p_min, p_max)) in enumerate(
        zip(positions, percentile_data)
    ):
        x_center = pos
        x_left = x_center - box_width / 2
        x_right = x_center + box_width / 2
        whisker_width = box_width * 0.3

        plt.gca().add_patch(Rectangle(
            (x_left, p10), box_width, p90 - p10,
            facecolor=sns.color_palette("pastel")[0],
            alpha=0.7, edgecolor="black", linewidth=1,
        ))
        plt.plot([x_left, x_right], [p50, p50], color="black", linewidth=1.5)
        plt.plot([x_center, x_center], [p_min, p10], color="black", linewidth=1)
        plt.plot([x_center, x_center], [p90, p_max], color="black", linewidth=1)
        plt.plot([x_center - whisker_width / 2, x_center + whisker_width / 2],
                 [p_min, p_min], color="black", linewidth=1)
        plt.plot([x_center - whisker_width / 2, x_center + whisker_width / 2],
                 [p_max, p_max], color="black", linewidth=1)
        x_scatter = np.random.normal(x_center, 0.05, len(data_for_boxplot[i]))
        plt.scatter(x_scatter, data_for_boxplot[i], alpha=0.3, s=3,
                    color="darkblue", zorder=10)

    n = len(instance_labels)
    plt.xticks(list(range(1, n + 1)), instance_labels)
    plt.xlim(0.5, n + 0.5)
    plt.xlabel("K, number of parallel component protocol instances")
    plt.ylabel("Transaction Latency (ms)")
    plt.ylim(bottom=0)
    plt.grid(True, linestyle=":", alpha=0.6, axis="y")
    ax = plt.gca()
    for spine in ax.spines.values():
        spine.set_visible(False)
    os.makedirs(os.path.dirname(pdf_path), exist_ok=True)
    plt.tight_layout()
    plt.savefig(pdf_path)
    plt.close()


def plot_boxplot_with_mean_and_scatter(
    by_instances_avg: Dict[int, List[float]], pdf_path: str
) -> None:
    try:
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
        import seaborn as sns
        import numpy as np
        from matplotlib.patches import Rectangle
    except Exception as e:
        print("Warning: matplotlib/seaborn/numpy not available; skipping plot: {0}".format(e))
        return

    data_for_boxplot, instance_labels, percentile_data, mean_data = \
        prepare_plot_data_from_averages(by_instances_avg)
    if not data_for_boxplot:
        print("Warning: no data to plot")
        return

    sns.set_style("ticks")
    plt.figure(figsize=(7, 4))
    positions = range(1, len(instance_labels) + 1)
    box_width = 0.6

    for i, (pos, (p10, p50, p90, p_min, p_max), mean_val) in enumerate(
        zip(positions, percentile_data, mean_data)
    ):
        x_center = pos
        x_left = x_center - box_width / 2
        x_right = x_center + box_width / 2
        whisker_width = box_width * 0.3

        plt.gca().add_patch(Rectangle(
            (x_left, p10), box_width, p90 - p10,
            facecolor=sns.color_palette("pastel")[0],
            alpha=0.7, edgecolor="black", linewidth=1,
        ))
        plt.plot([x_left, x_right], [p50, p50], color="black", linewidth=1.5)
        plt.plot([x_center, x_center], [p_min, p10], color="black", linewidth=1)
        plt.plot([x_center, x_center], [p90, p_max], color="black", linewidth=1)
        plt.plot([x_center - whisker_width / 2, x_center + whisker_width / 2],
                 [p_min, p_min], color="black", linewidth=1)
        plt.plot([x_center - whisker_width / 2, x_center + whisker_width / 2],
                 [p_max, p_max], color="black", linewidth=1)
        x_scatter = np.random.normal(x_center, 0.05, len(data_for_boxplot[i]))
        plt.scatter(x_scatter, data_for_boxplot[i], alpha=0.3, s=3,
                    color="darkblue", zorder=10)
        plt.plot(x_center, mean_val, marker="D", markersize=5, linestyle="None",
                 color="red", markeredgecolor="darkred", markeredgewidth=1,
                 label="Mean" if i == 0 else "", zorder=11)

    # Fit x for f(K) = 3x + 1500/K against mean_data (least squares)
    k_values = np.array(instance_labels, dtype=float)
    mean_values = np.array(mean_data, dtype=float)
    x_fit = float((mean_values - (1500.0 / k_values)).mean() / 3.0)
    fit_values = 3.0 * x_fit + (1500.0 / k_values)

    n = len(instance_labels)
    pos_list = list(range(1, n + 1))
    plt.xticks(pos_list, instance_labels)
    plt.xlim(0.5, n + 0.5)
    plt.plot(pos_list, fit_values, color="green", linewidth=1.0,
             linestyle="--", label="Fit: Δ=112", zorder=12)
    plt.axhline(336, color="magenta", linewidth=1.0, linestyle="--",
                label="Confirmation time 3Δ")
    plt.xlabel("K, number of parallel component protocol instances")
    plt.ylabel("Transaction Latency (ms)")
    plt.ylim(bottom=0)
    plt.legend(loc="best")
    plt.grid(True, linestyle=":", alpha=0.6, axis="y")
    ax = plt.gca()
    for spine in ax.spines.values():
        spine.set_visible(False)
    os.makedirs(os.path.dirname(pdf_path), exist_ok=True)
    plt.tight_layout()
    plt.savefig(pdf_path)
    print("Fitted x for f(K)=3x+1500/K: {0:.6f}".format(x_fit))
    plt.close()


def main() -> None:
    args = parse_args()
    by_instances_avg = load_plot_data(args.plot_data_csv)
    if not by_instances_avg:
        return

    base_path = args.pdf
    if base_path.endswith(".pdf"):
        base_path = base_path[:-4]

    plot_boxplot_with_mean(by_instances_avg, "{0}_mean.pdf".format(base_path))
    plot_boxplot_with_scatter(by_instances_avg, "{0}_scatter.pdf".format(base_path))
    plot_boxplot_with_mean_and_scatter(
        by_instances_avg, "{0}_mean_scatter.pdf".format(base_path)
    )


if __name__ == "__main__":
    main()
