#!/usr/bin/env python3
"""
Generate latency plots from precomputed CSV data.

This script reads the per-transaction average latency data produced by
postprocessing/compute_latency.py (latency_plot_data.csv) and recreates the
same plots that used to be generated directly by the compute script:

- latency_vs_instances_mean.png      (box + mean overlay)
- latency_vs_instances_scatter.png   (box + scatter overlay)
- latency_vs_instances_mean_scatter.png (box + mean + scatter)

The plotting style, excluded instance counts, and axis labels are intended
to match the original implementation.
"""

import argparse
import os
import csv
from collections import defaultdict
from typing import Dict, Iterable, List, Tuple


def get_repo_root() -> str:
    """
    Get the repository root directory (one level up from postprocessing
    folder).
    """
    script_dir = os.path.dirname(os.path.abspath(__file__))
    return os.path.dirname(script_dir)


def parse_args() -> argparse.Namespace:
    repo_root = get_repo_root()

    # Default locations mirror compute_latency.py outputs
    default_output_dir = os.path.join(repo_root, "logs", "gatling-best")
    default_plot_data_csv = os.path.join(
        default_output_dir, "latency_plot_data.csv"
    )
    default_png = os.path.join(default_output_dir, "latency_vs_instances.png")

    parser = argparse.ArgumentParser(
        description=(
            "Generate latency vs instances plots from precomputed CSV data"
        )
    )
    parser.add_argument(
        "--plot-data-csv",
        dest="plot_data_csv",
        default=default_plot_data_csv,
        help=(
            "CSV file with per-transaction average latencies used for plotting "
            "(default: latency_plot_data.csv in the same directory as PNG)"
        ),
    )
    parser.add_argument(
        "--png",
        dest="png",
        default=default_png,
        help=(
            "Base PNG path for plots (default: latency_vs_instances.png). "
            "The script will append _mean, _scatter, _mean_scatter."
        ),
    )
    return parser.parse_args()


def load_plot_data(path: str) -> Dict[int, List[float]]:
    """
    Load per-transaction average latencies from the CSV produced by
    compute_latency.py (write_plot_data_csv).

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
        print(f"Error: plot data CSV not found: {path}")
        return {}
    except KeyError as e:
        print(f"Error: missing expected column in plot data CSV: {e}")
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
    """
    Prepare data for plotting from per-transaction average latencies.

    This mirrors the original prepare_plot_data implementation that worked
    from the in-memory by_instances structure.
    """
    data_for_boxplot: List[List[float]] = []
    instance_labels: List[int] = []
    percentile_data: List[Tuple[float, float, float, float, float]] = []
    mean_data: List[float] = []

    try:
        import numpy as np
    except ImportError:
        print("Error: numpy not available")
        return [], [], [], []

    # Exclude instance counts 11 and 49 from plots, as in the original script
    excluded_instances = {11, 49}
    for instances in sorted(by_instances_avg.keys()):
        if instances in excluded_instances:
            continue
        per_tx_averages = by_instances_avg[instances]
        if not per_tx_averages:
            continue

        data_for_boxplot.append(per_tx_averages)
        instance_labels.append(instances)

        # Calculate 10th, 50th (median), and 90th percentiles, plus min/max
        p10 = np.percentile(per_tx_averages, 10)
        p50 = np.percentile(per_tx_averages, 50)
        p90 = np.percentile(per_tx_averages, 90)
        p_min = np.min(per_tx_averages)
        p_max = np.max(per_tx_averages)
        mean_val = np.mean(per_tx_averages)
        percentile_data.append((p10, p50, p90, p_min, p_max))
        mean_data.append(mean_val)

    return data_for_boxplot, instance_labels, percentile_data, mean_data


def plot_boxplot_with_mean(
    by_instances_avg: Dict[int, List[float]], png_path: str
) -> None:
    """Create a box plot with mean overlay using seaborn."""
    try:
        import matplotlib
        matplotlib.use("Agg")  # Use non-interactive backend
        import matplotlib.pyplot as plt
        import seaborn as sns
        from matplotlib.patches import Rectangle
        from matplotlib.lines import Line2D
    except Exception as e:
        print(
            (
                "Warning: matplotlib/seaborn/numpy not available; "
                f"skipping plot: {e}"
            )
        )
        return

    (
        data_for_boxplot,
        instance_labels,
        percentile_data,
        mean_data,
    ) = prepare_plot_data_from_averages(by_instances_avg)

    if not data_for_boxplot:
        print("Warning: no data to plot")
        return

    # Set seaborn style (use "ticks" to avoid vertical grid lines)
    sns.set_style("ticks")
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
            edgecolor="black",
            linewidth=1,
        )
        plt.gca().add_patch(rect)

        # Draw median line
        plt.plot(
            [x_left, x_right],
            [p50, p50],
            color="black",
            linewidth=1.5,
            label="Median" if i == 0 else "",
        )

        # Draw whiskers (extend from box edges to min/max)
        whisker_width = box_width * 0.3
        # Lower whisker (from p10 to p_min)
        plt.plot(
            [x_center, x_center],
            [p_min, p10],
            color="black",
            linewidth=1,
        )
        # Upper whisker (from p90 to p_max)
        plt.plot(
            [x_center, x_center],
            [p90, p_max],
            color="black",
            linewidth=1,
        )

        # Draw caps (horizontal lines at whisker ends)
        # Lower cap at p_min
        plt.plot(
            [x_center - whisker_width / 2, x_center + whisker_width / 2],
            [p_min, p_min],
            color="black",
            linewidth=1,
        )
        # Upper cap at p_max
        plt.plot(
            [x_center - whisker_width / 2, x_center + whisker_width / 2],
            [p_max, p_max],
            color="black",
            linewidth=1,
        )

        # Overlay mean as a marker
        plt.plot(
            x_center,
            mean_val,
            marker="D",
            markersize=5,
            color="red",
            markeredgecolor="darkred",
            markeredgewidth=1,
            label="Mean" if i == 0 else "",
        )

    # Set x-axis from actual instance counts
    n = len(instance_labels)
    positions = list(range(1, n + 1))
    plt.xticks(positions, instance_labels)
    plt.xlim(0.5, n + 0.5)

    plt.xlabel("K, number of parallel component protocol instances")
    plt.ylabel("Transaction Latency (ms)")
    # Start y-axis at 0 (linear scale)
    plt.ylim(bottom=0)
    handles, labels = plt.gca().get_legend_handles_labels()
    handles.append(
        Line2D([0], [0], color="black", linewidth=1.5, label="Median")
    )
    labels.append("Median")
    plt.legend(handles, labels, loc="best")
    plt.grid(True, linestyle=":", alpha=0.6, axis="y")
    # Remove all axis spines
    ax = plt.gca()
    ax.spines["top"].set_visible(False)
    ax.spines["right"].set_visible(False)
    ax.spines["bottom"].set_visible(False)
    ax.spines["left"].set_visible(False)
    os.makedirs(os.path.dirname(png_path), exist_ok=True)
    plt.tight_layout()
    plt.savefig(png_path, dpi=160)
    # Also save as PDF
    pdf_path = png_path.replace(".png", ".pdf")
    plt.savefig(pdf_path)
    plt.close()


def plot_boxplot_with_scatter(
    by_instances_avg: Dict[int, List[float]], png_path: str
) -> None:
    """Create a box plot with scatter overlay using seaborn."""
    try:
        import matplotlib
        matplotlib.use("Agg")  # Use non-interactive backend
        import matplotlib.pyplot as plt
        import seaborn as sns
        from matplotlib.patches import Rectangle
        import numpy as np
    except Exception as e:
        print(
            (
                "Warning: matplotlib/seaborn/numpy not available; "
                f"skipping plot: {e}"
            )
        )
        return

    (
        data_for_boxplot,
        instance_labels,
        percentile_data,
        _,
    ) = prepare_plot_data_from_averages(by_instances_avg)

    if not data_for_boxplot:
        print("Warning: no data to plot")
        return

    # Set seaborn style (use "ticks" to avoid vertical grid lines)
    sns.set_style("ticks")
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
            edgecolor="black",
            linewidth=1,
        )
        plt.gca().add_patch(rect)

        # Draw median line
        plt.plot(
            [x_left, x_right],
            [p50, p50],
            color="black",
            linewidth=1.5,
        )

        # Draw whiskers (extend from box edges to min/max)
        whisker_width = box_width * 0.3
        # Lower whisker (from p10 to p_min)
        plt.plot(
            [x_center, x_center],
            [p_min, p10],
            color="black",
            linewidth=1,
        )
        # Upper whisker (from p90 to p_max)
        plt.plot(
            [x_center, x_center],
            [p90, p_max],
            color="black",
            linewidth=1,
        )

        # Draw caps (horizontal lines at whisker ends)
        # Lower cap at p_min
        plt.plot(
            [x_center - whisker_width / 2, x_center + whisker_width / 2],
            [p_min, p_min],
            color="black",
            linewidth=1,
        )
        # Upper cap at p_max
        plt.plot(
            [x_center - whisker_width / 2, x_center + whisker_width / 2],
            [p_max, p_max],
            color="black",
            linewidth=1,
        )

        # Overlay scatter plot of all data points
        x_scatter = np.random.normal(x_center, 0.05, len(data_for_boxplot[i]))
        plt.scatter(
            x_scatter,
            data_for_boxplot[i],
            alpha=0.3,
            s=3,
            color="darkblue",
            zorder=10,
        )

    # Set x-axis from actual instance counts
    n = len(instance_labels)
    positions = list(range(1, n + 1))
    plt.xticks(positions, instance_labels)
    plt.xlim(0.5, n + 0.5)

    plt.xlabel("K, number of parallel component protocol instances")
    plt.ylabel("Transaction Latency (ms)")
    # Start y-axis at 0 (linear scale)
    plt.ylim(bottom=0)
    plt.grid(True, linestyle=":", alpha=0.6, axis="y")
    # Remove all axis spines
    ax = plt.gca()
    ax.spines["top"].set_visible(False)
    ax.spines["right"].set_visible(False)
    ax.spines["bottom"].set_visible(False)
    ax.spines["left"].set_visible(False)
    os.makedirs(os.path.dirname(png_path), exist_ok=True)
    plt.tight_layout()
    plt.savefig(png_path, dpi=160)
    # Also save as PDF
    pdf_path = png_path.replace(".png", ".pdf")
    plt.savefig(pdf_path)
    plt.close()


def plot_boxplot_with_mean_and_scatter(
    by_instances_avg: Dict[int, List[float]], png_path: str
) -> None:
    """Create a box plot with both mean and scatter overlay using seaborn."""
    try:
        import matplotlib
        matplotlib.use("Agg")  # Use non-interactive backend
        import matplotlib.pyplot as plt
        import seaborn as sns
        from matplotlib.patches import Rectangle
        import numpy as np
    except Exception as e:
        print(
            (
                "Warning: matplotlib/seaborn/numpy not available; "
                f"skipping plot: {e}"
            )
        )
        return

    (
        data_for_boxplot,
        instance_labels,
        percentile_data,
        mean_data,
    ) = prepare_plot_data_from_averages(by_instances_avg)

    if not data_for_boxplot:
        print("Warning: no data to plot")
        return

    # Set seaborn style (use "ticks" to avoid vertical grid lines)
    sns.set_style("ticks")
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
            edgecolor="black",
            linewidth=1,
        )
        plt.gca().add_patch(rect)

        # Draw median line
        plt.plot(
            [x_left, x_right],
            [p50, p50],
            color="black",
            linewidth=1.5,
        )

        # Draw whiskers (extend from box edges to min/max)
        whisker_width = box_width * 0.3
        # Lower whisker (from p10 to p_min)
        plt.plot(
            [x_center, x_center],
            [p_min, p10],
            color="black",
            linewidth=1,
        )
        # Upper whisker (from p90 to p_max)
        plt.plot(
            [x_center, x_center],
            [p90, p_max],
            color="black",
            linewidth=1,
        )

        # Draw caps (horizontal lines at whisker ends)
        # Lower cap at p_min
        plt.plot(
            [x_center - whisker_width / 2, x_center + whisker_width / 2],
            [p_min, p_min],
            color="black",
            linewidth=1,
        )
        # Upper cap at p_max
        plt.plot(
            [x_center - whisker_width / 2, x_center + whisker_width / 2],
            [p_max, p_max],
            color="black",
            linewidth=1,
        )

        # Overlay scatter plot of all data points
        x_scatter = np.random.normal(x_center, 0.05, len(data_for_boxplot[i]))
        plt.scatter(
            x_scatter,
            data_for_boxplot[i],
            alpha=0.3,
            s=3,
            color="darkblue",
            zorder=10,
        )

        # Overlay mean as a marker
        plt.plot(
            x_center,
            mean_val,
            marker="D",
            markersize=5,
            linestyle="None",
            color="red",
            markeredgecolor="darkred",
            markeredgewidth=1,
            label="Mean" if i == 0 else "",
            zorder=11,
        )

    # Fit x for f(K) = 3x + 1500/K against mean_data (least squares)
    k_values = np.array(instance_labels, dtype=float)
    mean_values = np.array(mean_data, dtype=float)
    x_fit = (mean_values - (1500.0 / k_values)).mean() / 3.0
    fit_values = 3.0 * x_fit + (1500.0 / k_values)

    # Set x-axis from actual instance counts
    n = len(instance_labels)
    positions = list(range(1, n + 1))
    plt.xticks(positions, instance_labels)
    plt.xlim(0.5, n + 0.5)

    # Overlay fit curve at each K position
    plt.plot(
        positions,
        fit_values,
        color="green",
        linewidth=1.0,
        linestyle="--",
        label="Fit: Δ=112",
        zorder=12,
    )
    plt.axhline(
        336,
        color="magenta",
        linewidth=1.0,
        linestyle="--",
        label="Confirmation time 3Δ",
    )

    plt.xlabel("K, number of parallel component protocol instances")
    plt.ylabel("Transaction Latency (ms)")
    # Start y-axis at 0 (linear scale)
    plt.ylim(bottom=0)
    plt.legend(loc="best")
    plt.grid(True, linestyle=":", alpha=0.6, axis="y")
    # Remove all axis spines
    ax = plt.gca()
    ax.spines["top"].set_visible(False)
    ax.spines["right"].set_visible(False)
    ax.spines["bottom"].set_visible(False)
    ax.spines["left"].set_visible(False)
    os.makedirs(os.path.dirname(png_path), exist_ok=True)
    plt.tight_layout()
    plt.savefig(png_path, dpi=160)
    # Also save as PDF
    pdf_path = png_path.replace(".png", ".pdf")
    plt.savefig(pdf_path)
    print(f"Fitted x for f(K)=3x+1500/K: {x_fit:.6f}")
    plt.close()


def main() -> None:
    args = parse_args()
    by_instances_avg = load_plot_data(args.plot_data_csv)
    if not by_instances_avg:
        return

    base_path = args.png
    if base_path.endswith(".png"):
        base_path = base_path[:-4]

    # Generate three plots: one with mean overlay, one with scatter overlay,
    # and one with both mean and scatter
    plot_boxplot_with_mean(
        by_instances_avg, f"{base_path}_mean.png"
    )
    plot_boxplot_with_scatter(
        by_instances_avg, f"{base_path}_scatter.png"
    )
    plot_boxplot_with_mean_and_scatter(
        by_instances_avg, f"{base_path}_mean_scatter.png"
    )


if __name__ == "__main__":
    main()

