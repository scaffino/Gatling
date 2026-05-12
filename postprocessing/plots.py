#!/usr/bin/env python3
"""
Generate latency plots (PDF) from precomputed CSV data.

Reads latency_plot_data.csv produced by generate_csv.py and generates
two PDF plots:
  - latency_vs_instances_mean.pdf         (box + mean overlay + fit curve)
  - latency_vs_instances_mean_scatter.pdf (box + mean + scatter + fit curve)
"""

import argparse
import os
import csv
from collections import defaultdict
from typing import Dict, List, Tuple, Optional

try:
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    import matplotlib.patches as mpatches
    from matplotlib.lines import Line2D
    import numpy as np
    import seaborn as sns
    _DEPS_OK = True
    _DEPS_ERR = ""
except ImportError as _e:
    _DEPS_OK = False
    _DEPS_ERR = str(_e)

EXCLUDED_INSTANCES = {11, 49}
# K=1 is plotted but excluded from the fit: its latency is dominated by
# nullification timeouts, not by the L(K)=3Δ+Q/K queuing model.
FIT_EXCLUDED_INSTANCES = EXCLUDED_INSTANCES | {1}

# Queuing coefficient Q in L(K) = 3Δ + Q/K.
# With gossip enabled:  Q = 3000/2          = 1500  ms  (any validator picks up the tx)
# With gossip disabled: Q = V * 3000/2      = 15000 ms  (tx waits for its own validator to lead)
#   where V = 10 validators, so the inter-leadership period is V * 3000/K ms.
QUEUING_COEFF = 15000


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
        "--stats-csv",
        dest="stats_csv",
        default=None,
        help=(
            "Aggregated stats CSV produced by aggregate_plot_data.py "
            "(instances,n_tx,mean_ms,stddev_ms,p5_ms,p10_ms,p50_ms,p90_ms,p95_ms,min_ms,max_ms). "
            "When provided, skips loading the large plot-data CSV and generates "
            "the _mean plot directly from pre-computed percentiles."
        ),
    )
    parser.add_argument(
        "--pdf",
        dest="pdf",
        default=default_pdf,
        help=(
            "Base PDF path for plots (default: latency_vs_instances.pdf). "
            "The script appends _mean, _mean_scatter."
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


def load_stats_csv(path: str) -> Dict[int, dict]:
    """
    Load aggregated stats CSV produced by aggregate_plot_data.py.
    Returns: instances -> {mean_ms, stddev_ms, p10_ms, p50_ms, p90_ms, min_ms, max_ms, ...}
    """
    by_instances: Dict[int, dict] = {}
    try:
        with open(path, newline="") as f:
            reader = csv.DictReader(f)
            for row in reader:
                k = int(row["instances"])
                by_instances[k] = {col: float(row[col]) for col in row if col != "instances"}
    except FileNotFoundError:
        print("Error: stats CSV not found: {0}".format(path))
        return {}
    except KeyError as e:
        print("Error: missing expected column in stats CSV: {0}".format(e))
        return {}
    return by_instances


def prepare_plot_data_from_stats(
    stats: Dict[int, dict]
) -> Tuple[List[int], List[Tuple[float, float, float, float, float]], List[float]]:
    """Extract (instance_labels, percentile_data, mean_data) from an aggregated stats dict."""
    instance_labels: List[int] = []
    percentile_data: List[Tuple[float, float, float, float, float]] = []
    mean_data: List[float] = []

    for k in sorted(stats.keys()):
        if k in EXCLUDED_INSTANCES:
            continue
        s = stats[k]
        instance_labels.append(k)
        percentile_data.append((s["p10_ms"], s["p50_ms"], s["p90_ms"], s["min_ms"], s["max_ms"]))
        mean_data.append(s["mean_ms"])

    return instance_labels, percentile_data, mean_data


def plot_boxplot_with_mean_from_stats(stats: Dict[int, dict], pdf_path: str) -> None:
    """
    Generate the _mean plot directly from pre-computed aggregated statistics.
    Equivalent to plot_boxplot_with_mean but skips loading the large plot-data CSV.
    """
    if not _DEPS_OK:
        print("Warning: dependencies not available; skipping plot: {0}".format(_DEPS_ERR))
        return

    instance_labels, percentile_data, mean_data = prepare_plot_data_from_stats(stats)
    if not instance_labels:
        print("Warning: no data to plot")
        return

    sns.set_style("ticks")
    fig, ax = plt.subplots(figsize=(7, 4))
    positions = list(range(1, len(instance_labels) + 1))

    _draw_boxes(ax, positions, percentile_data, mean_data=mean_data)

    k_arr = np.array(instance_labels, dtype=float)
    m_arr = np.array(mean_data, dtype=float)
    fit_mask = np.array([k not in FIT_EXCLUDED_INSTANCES for k in instance_labels])
    delta = float((m_arr[fit_mask] - float(QUEUING_COEFF) / k_arr[fit_mask]).mean() / 3.0)
    fit_values = 3.0 * delta + (float(QUEUING_COEFF) / k_arr)

    ax.plot(positions, fit_values, color="green", linewidth=1.0, linestyle="--", zorder=12)
    ax.axhline(3.0 * delta, color="magenta", linewidth=1.0, linestyle="--")

    ax.legend(handles=[
        _box_patch_handle(),
        Line2D([0], [0], color="black", linewidth=1.5, label="Median"),
        Line2D([0], [0], marker="D", color="red", linestyle="None",
               markeredgecolor="darkred", markeredgewidth=1, markersize=5, label="Mean"),
        Line2D([0], [0], color="green", linewidth=1.0, linestyle="--",
               label="Fit: Δ = {0:.0f} ms".format(delta)),
        Line2D([0], [0], color="magenta", linewidth=1.0, linestyle="--",
               label="Confirmation time 3Δ = {0:.0f} ms".format(3.0 * delta)),
    ], loc="best")

    _apply_axes_style(ax, instance_labels, positions)
    os.makedirs(os.path.dirname(pdf_path), exist_ok=True)
    fig.tight_layout()
    fig.savefig(pdf_path)
    print("Fitted Δ for L(K) = 3Δ + {0}/K (K≥10): {1:.1f} ms  (3Δ = {2:.0f} ms)".format(
        QUEUING_COEFF, delta, 3.0 * delta))
    plt.close(fig)


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

    for instances in sorted(by_instances_avg.keys()):
        if instances in EXCLUDED_INSTANCES:
            continue
        per_tx_averages = by_instances_avg[instances]
        if not per_tx_averages:
            continue

        data_for_boxplot.append(per_tx_averages)
        instance_labels.append(instances)

        p10 = float(np.percentile(per_tx_averages, 10))
        p50 = float(np.percentile(per_tx_averages, 50))
        p90 = float(np.percentile(per_tx_averages, 90))
        p_min = float(np.min(per_tx_averages))
        p_max = float(np.max(per_tx_averages))
        mean_val = float(np.mean(per_tx_averages))
        percentile_data.append((p10, p50, p90, p_min, p_max))
        mean_data.append(mean_val)

    return data_for_boxplot, instance_labels, percentile_data, mean_data


def _draw_boxes(
    ax,
    positions: List[int],
    percentile_data: List[Tuple[float, float, float, float, float]],
    data_for_boxplot: Optional[List[List[float]]] = None,
    mean_data: Optional[List[float]] = None,
    box_width: float = 0.6,
) -> None:
    """Draw custom boxes, whiskers, and optionally scatter and mean markers."""
    box_color = sns.color_palette("pastel")[0]
    w_half = box_width * 0.3 / 2

    for i, (pos, (p10, p50, p90, p_min, p_max)) in enumerate(zip(positions, percentile_data)):
        x_l = pos - box_width / 2
        x_r = pos + box_width / 2

        ax.add_patch(mpatches.Rectangle(
            (x_l, p10), box_width, p90 - p10,
            facecolor=box_color, alpha=0.7, edgecolor="black", linewidth=1,
        ))
        ax.plot([x_l, x_r], [p50, p50], color="black", linewidth=1.5)
        ax.plot([pos, pos], [p_min, p10], color="black", linewidth=1)
        ax.plot([pos, pos], [p90, p_max], color="black", linewidth=1)
        ax.plot([pos - w_half, pos + w_half], [p_min, p_min], color="black", linewidth=1)
        ax.plot([pos - w_half, pos + w_half], [p_max, p_max], color="black", linewidth=1)

        if data_for_boxplot is not None:
            x_jitter = np.random.normal(pos, 0.05, len(data_for_boxplot[i]))
            ax.scatter(x_jitter, data_for_boxplot[i], alpha=0.3, s=3, color="darkblue", zorder=10)

        if mean_data is not None:
            ax.plot(pos, mean_data[i], marker="D", markersize=5, linestyle="None",
                    color="red", markeredgecolor="darkred", markeredgewidth=1, zorder=11)


def _apply_axes_style(ax, instance_labels: List[int], positions: List[int]) -> None:
    n = len(instance_labels)
    ax.set_xticks(list(range(1, n + 1)))
    ax.set_xticklabels(instance_labels)
    ax.set_xlim(0.5, n + 0.5)
    ax.set_xlabel("K, number of parallel component protocol instances")
    ax.set_ylabel("Transaction Latency (ms)")
    ax.set_ylim(bottom=0)
    ax.grid(True, linestyle=":", alpha=0.6, axis="y")
    for spine in ax.spines.values():
        spine.set_visible(False)


def _box_patch_handle() -> mpatches.Patch:
    return mpatches.Patch(
        facecolor=sns.color_palette("pastel")[0], alpha=0.7, edgecolor="black", label="P10–P90"
    )


def plot_boxplot_with_mean(
    by_instances_avg: Dict[int, List[float]], pdf_path: str
) -> None:
    if not _DEPS_OK:
        print("Warning: dependencies not available; skipping plot: {0}".format(_DEPS_ERR))
        return

    data_for_boxplot, instance_labels, percentile_data, mean_data = \
        prepare_plot_data_from_averages(by_instances_avg)
    if not data_for_boxplot:
        print("Warning: no data to plot")
        return

    sns.set_style("ticks")
    fig, ax = plt.subplots(figsize=(7, 4))
    positions = list(range(1, len(instance_labels) + 1))

    _draw_boxes(ax, positions, percentile_data, mean_data=mean_data)

    # Fit L(K) = 3Δ + 1500/K over K values not in FIT_EXCLUDED_INSTANCES.
    # Closed-form least squares (model is linear in Δ): Δ = mean(yᵢ − 1500/Kᵢ) / 3
    k_arr = np.array(instance_labels, dtype=float)
    m_arr = np.array(mean_data, dtype=float)
    fit_mask = np.array([k not in FIT_EXCLUDED_INSTANCES for k in instance_labels])
    delta = float((m_arr[fit_mask] - float(QUEUING_COEFF) / k_arr[fit_mask]).mean() / 3.0)
    fit_values = 3.0 * delta + (float(QUEUING_COEFF) / k_arr)

    ax.plot(positions, fit_values, color="green", linewidth=1.0, linestyle="--", zorder=12)
    ax.axhline(3.0 * delta, color="magenta", linewidth=1.0, linestyle="--")

    ax.legend(handles=[
        _box_patch_handle(),
        Line2D([0], [0], color="black", linewidth=1.5, label="Median"),
        Line2D([0], [0], marker="D", color="red", linestyle="None",
               markeredgecolor="darkred", markeredgewidth=1, markersize=5, label="Mean"),
        Line2D([0], [0], color="green", linewidth=1.0, linestyle="--",
               label="Fit: Δ = {0:.0f} ms".format(delta)),
        Line2D([0], [0], color="magenta", linewidth=1.0, linestyle="--",
               label="Confirmation time 3Δ = {0:.0f} ms".format(3.0 * delta)),
    ], loc="best")

    _apply_axes_style(ax, instance_labels, positions)
    os.makedirs(os.path.dirname(pdf_path), exist_ok=True)
    fig.tight_layout()
    fig.savefig(pdf_path)
    print("Fitted Δ for L(K) = 3Δ + {0}/K (K≥10): {1:.1f} ms  (3Δ = {2:.0f} ms)".format(
    QUEUING_COEFF, delta, 3.0 * delta))
    plt.close(fig)


def plot_boxplot_with_mean_and_scatter(
    by_instances_avg: Dict[int, List[float]], pdf_path: str
) -> None:
    if not _DEPS_OK:
        print("Warning: dependencies not available; skipping plot: {0}".format(_DEPS_ERR))
        return

    data_for_boxplot, instance_labels, percentile_data, mean_data = \
        prepare_plot_data_from_averages(by_instances_avg)
    if not data_for_boxplot:
        print("Warning: no data to plot")
        return

    sns.set_style("ticks")
    fig, ax = plt.subplots(figsize=(7, 4))
    positions = list(range(1, len(instance_labels) + 1))

    _draw_boxes(ax, positions, percentile_data,
                data_for_boxplot=data_for_boxplot, mean_data=mean_data)

    k_arr = np.array(instance_labels, dtype=float)
    m_arr = np.array(mean_data, dtype=float)
    fit_mask = np.array([k not in FIT_EXCLUDED_INSTANCES for k in instance_labels])
    delta = float((m_arr[fit_mask] - float(QUEUING_COEFF) / k_arr[fit_mask]).mean() / 3.0)
    fit_values = 3.0 * delta + (float(QUEUING_COEFF) / k_arr)

    ax.plot(positions, fit_values, color="green", linewidth=1.0, linestyle="--", zorder=12)
    ax.axhline(3.0 * delta, color="magenta", linewidth=1.0, linestyle="--")

    ax.legend(handles=[
        _box_patch_handle(),
        Line2D([0], [0], color="black", linewidth=1.5, label="Median"),
        Line2D([0], [0], marker="D", color="red", linestyle="None",
               markeredgecolor="darkred", markeredgewidth=1, markersize=5, label="Mean"),
        Line2D([0], [0], marker="o", color="darkblue", linestyle="None",
               alpha=0.4, markersize=4, label="Per-tx avg"),
        Line2D([0], [0], color="green", linewidth=1.0, linestyle="--",
               label="Fit: Δ = {0:.0f} ms".format(delta)),
        Line2D([0], [0], color="magenta", linewidth=1.0, linestyle="--",
               label="Confirmation time 3Δ = {0:.0f} ms".format(3.0 * delta)),
    ], loc="best")

    _apply_axes_style(ax, instance_labels, positions)
    os.makedirs(os.path.dirname(pdf_path), exist_ok=True)
    fig.tight_layout()
    fig.savefig(pdf_path)
    print("Fitted Δ for L(K) = 3Δ + {0}/K (K≥10): {1:.1f} ms  (3Δ = {2:.0f} ms)".format(
    QUEUING_COEFF, delta, 3.0 * delta))
    plt.close(fig)


def main() -> None:
    args = parse_args()

    base_path = args.pdf
    if base_path.endswith(".pdf"):
        base_path = base_path[:-4]

    if args.stats_csv:
        stats = load_stats_csv(args.stats_csv)
        if not stats:
            return
        plot_boxplot_with_mean_from_stats(stats, "{0}_mean.pdf".format(base_path))
        # Scatter plot requires raw per-transaction data; supply --plot-data-csv as well
        # to also generate _mean_scatter.
        if os.path.exists(args.plot_data_csv):
            by_instances_avg = load_plot_data(args.plot_data_csv)
            if by_instances_avg:
                plot_boxplot_with_mean_and_scatter(
                    by_instances_avg, "{0}_mean_scatter.pdf".format(base_path)
                )
    else:
        by_instances_avg = load_plot_data(args.plot_data_csv)
        if not by_instances_avg:
            return
        plot_boxplot_with_mean(by_instances_avg, "{0}_mean.pdf".format(base_path))
        plot_boxplot_with_mean_and_scatter(
            by_instances_avg, "{0}_mean_scatter.pdf".format(base_path)
        )


if __name__ == "__main__":
    main()
