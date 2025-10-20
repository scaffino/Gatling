#!/usr/bin/env python3
"""
Script to analyze proposal timing gaps from validator log files.
Extracts Unix timestamps from proposal log lines and computes average
gap between them.
"""

import re
import sys
from collections import defaultdict


def extract_timestamps_from_file(filepath):
    """
    Extract Unix timestamps from validator log file, grouped by
    consensus instance.

    Looks for lines matching pattern:
    "[consensus_X] Validator Y proposed block Z (view W) with N
    transactions at Unix timestamp TIMESTAMP ms"

    Args:
        filepath: Path to the log file

    Returns:
        Dictionary mapping instance_id -> list of timestamps in ms
    """
    timestamps_by_instance = defaultdict(list)

    # Pattern to match the proposal log line
    # and extract instance ID and timestamp
    pattern = (r"\[consensus_(\d+)\] Validator \d+ proposed block \d+ "
               r"\(view \d+\) with \d+ transactions at Unix timestamp "
               r"(\d+) ms")

    try:
        with open(filepath, 'r') as f:
            for line in f:
                match = re.search(pattern, line)
                if match:
                    instance_id = int(match.group(1))
                    timestamp = int(match.group(2))
                    timestamps_by_instance[instance_id].append(timestamp)
    except IOError:
        print("Warning: File not found: {}".format(filepath),
              file=sys.stderr)
    except Exception as e:
        print("Error reading {}: {}".format(filepath, e), file=sys.stderr)

    return dict(timestamps_by_instance)


def compute_gaps(timestamps):
    """
    Compute gaps between consecutive timestamps.

    Args:
        timestamps: List of timestamps in milliseconds

    Returns:
        List of gaps (differences between consecutive timestamps)
    """
    if len(timestamps) < 2:
        return []

    gaps = []
    for i in range(1, len(timestamps)):
        gap = timestamps[i] - timestamps[i-1]
        gaps.append(gap)

    return gaps


def compute_median(values):
    """
    Compute median of a list of values.

    Args:
        values: List of numeric values

    Returns:
        Median value as float
    """
    if not values:
        return 0.0

    sorted_values = sorted(values)
    n = len(sorted_values)

    if n % 2 == 0:
        # Even number of elements: average of two middle values
        return (sorted_values[n//2 - 1] + sorted_values[n//2]) / 2.0
    else:
        # Odd number of elements: middle value
        return float(sorted_values[n//2])


def compute_statistics(gaps):
    """
    Compute statistics for gaps.

    Returns:
        Tuple of (average, median, min, max) gap values
    """
    if not gaps:
        return 0.0, 0.0, 0, 0

    avg = sum(gaps) / float(len(gaps))
    median = compute_median(gaps)
    min_gap = min(gaps)
    max_gap = max(gaps)

    return avg, median, min_gap, max_gap


def main():
    """Main function to process log files and compute statistics."""

    # Default log files to analyze
    log_files = [
        "validator0.log",
        "validator1.log",
        "validator2.log",
        "validator3.log",
    ]

    # If command line arguments provided, use those instead
    if len(sys.argv) > 1:
        log_files = sys.argv[1:]

    print("Analyzing Validator Proposal Timing Gaps")
    print("=" * 60)
    print()

    # Collect timestamps grouped by instance (combining all log files)
    all_instances = defaultdict(list)
    all_timestamps = []

    for log_file in log_files:
        print("Processing: {}".format(log_file))
        timestamps_by_instance = extract_timestamps_from_file(log_file)

        if timestamps_by_instance:
            total_in_file = 0
            for instance_id, timestamps in timestamps_by_instance.items():
                # Combine proposals from all validators for each instance
                all_instances[instance_id].extend(timestamps)
                all_timestamps.extend(timestamps)
                total_in_file += len(timestamps)
            print("  Found {} proposals across {} instance(s)".format(
                total_in_file, len(timestamps_by_instance)))
        else:
            print("  No proposal timestamps found")

    print()
    print("=" * 60)
    print()

    if not all_timestamps:
        print("No timestamps found in any file!")
        return

    # Sort timestamps for each instance
    for instance_id in all_instances:
        all_instances[instance_id].sort()

    # Sort all timestamps (across all instances)
    all_timestamps.sort()

    print("Total timestamps collected: {}".format(len(all_timestamps)))
    print("Total instances found: {}".format(len(all_instances)))
    print("Instance IDs: {}".format(sorted(all_instances.keys())))
    print("Time range: {} ms to {} ms".format(
        all_timestamps[0], all_timestamps[-1]))
    duration_sec = (all_timestamps[-1] - all_timestamps[0]) / 1000.0
    print("Total duration: {:.2f} seconds".format(duration_sec))
    print()

    # ==================================================================
    # 1) WITHIN-INSTANCE STATISTICS (across all validators)
    # ==================================================================
    print("=" * 60)
    print("1) AVERAGE TIME BETWEEN PROPOSALS WITHIN SAME INSTANCE")
    print("   (combining all validators per instance)")
    print("=" * 60)
    print()

    within_instance_gaps_all = []

    for instance_id in sorted(all_instances.keys()):
        timestamps = all_instances[instance_id]
        if len(timestamps) >= 2:
            gaps = compute_gaps(timestamps)
            within_instance_gaps_all.extend(gaps)
            avg_gap, median_gap, min_gap, max_gap = compute_statistics(gaps)

            print("Instance {}:".format(instance_id))
            print("  Total proposals (all validators): {}".format(
                len(timestamps)))
            print("  Number of gaps: {}".format(len(gaps)))
            print("  Average gap: {:.2f} ms "
                  "({:.4f} seconds)".format(avg_gap, avg_gap/1000.0))
            print("  Median gap: {:.2f} ms "
                  "({:.4f} seconds)".format(median_gap, median_gap/1000.0))
            print("  Min gap: {} ms "
                  "({:.4f} seconds)".format(min_gap, min_gap/1000.0))
            print("  Max gap: {} ms "
                  "({:.4f} seconds)".format(max_gap, max_gap/1000.0))
            print()

    if within_instance_gaps_all:
        stats = compute_statistics(within_instance_gaps_all)
        avg_within, median_within, min_within, max_within = stats
        print("SUMMARY - Within-Instance Statistics:")
        num_gaps = len(within_instance_gaps_all)
        print("  Total gaps (within instances): {}".format(num_gaps))
        print("  Average gap: {:.2f} ms "
              "({:.4f} seconds)".format(avg_within, avg_within/1000.0))
        print("  Median gap: {:.2f} ms "
              "({:.4f} seconds)".format(median_within, median_within/1000.0))
        print("  Min gap: {} ms "
              "({:.4f} seconds)".format(min_within, min_within/1000.0))
        print("  Max gap: {} ms "
              "({:.4f} seconds)".format(max_within, max_within/1000.0))

    print()

    # ==================================================================
    # 2) ACROSS ALL INSTANCES STATISTICS (all validators, all instances)
    # ==================================================================
    print("=" * 60)
    print("2) AVERAGE TIME BETWEEN PROPOSALS ACROSS ALL INSTANCES")
    print("   (combining all validators and all instances)")
    print("=" * 60)
    print()

    # Compute gaps across all proposals (sorted chronologically)
    all_gaps = compute_gaps(all_timestamps)

    if not all_gaps:
        print("Not enough timestamps to compute gaps!")
        return

    avg_gap, median_gap, min_gap, max_gap = compute_statistics(all_gaps)

    print("Total proposals (all instances, all validators): {}".format(
        len(all_timestamps)))
    print("Total gaps: {}".format(len(all_gaps)))
    print("Average gap: {:.2f} ms "
          "({:.4f} seconds)".format(avg_gap, avg_gap/1000.0))
    print("Median gap: {:.2f} ms "
          "({:.4f} seconds)".format(median_gap, median_gap/1000.0))
    print("Minimum gap: {} ms "
          "({:.4f} seconds)".format(min_gap, min_gap/1000.0))
    print("Maximum gap: {} ms "
          "({:.4f} seconds)".format(max_gap, max_gap/1000.0))
    print()


if __name__ == "__main__":
    main()
