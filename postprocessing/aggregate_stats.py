#!/usr/bin/env python3
"""
Aggregate stats.csv across repeated runs.

For each (instances, crashed_validators) combination, combines:
  avg_ms     — weighted mean (weighted by unique_txs)
  stddev_ms  — combined standard deviation via law of total variance
  min_ms     — min of mins
  max_ms     — max of maxes
  unique_txs — sum of counts

Median cannot be correctly combined from summary statistics alone and is omitted.

Input files may use section headers from a merged pipeline:
  === N VALIDATOR(S) CRASHED ===
or per-run files from generate_csv.py (no crash section); pass --default-crashed N
for those rows. Use --only-crashed N to restrict the output CSV to one fault regime.
With --only-crashed 1 and no -o/--output, writes aggregated_1val_crashed/aggregated_stats.csv.
"""

import argparse
import csv
import math
import os
import re
import sys
from collections import defaultdict

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_INPUT = os.path.join(REPO_ROOT, "stats.csv")
DEFAULT_OUTPUT = os.path.join(REPO_ROOT, "aggregated_stats.csv")
DEFAULT_OUTPUT_1VAL_CRASHED = os.path.join(
    REPO_ROOT, "aggregated_1val_crashed", "aggregated_stats.csv"
)

SECTION_RE = re.compile(r"===\s*(\d+)\s+VALIDATOR[S]?\s+CRASHED\s*===", re.IGNORECASE)


def parse_stats_csv(path, default_crashed=None):
    rows = []
    current_crashed = None
    skipped_no_fault = 0

    with open(path) as f:
        for line in f:
            line = line.rstrip("\n")

            m = SECTION_RE.search(line)
            if m:
                current_crashed = int(m.group(1))
                continue

            if not line.strip() or line.startswith("instances"):
                continue

            parts = line.split(",")
            if len(parts) < 7:
                continue

            try:
                crashed = (
                    current_crashed
                    if current_crashed is not None
                    else default_crashed
                )
                if crashed is None:
                    skipped_no_fault += 1
                    continue
                rows.append({
                    "instances": int(parts[0]),
                    "crashed_validators": crashed,
                    "avg_ms": float(parts[1]),
                    # parts[2] is median — omitted
                    "stddev_ms": float(parts[3]),
                    "min_ms": float(parts[4]),
                    "max_ms": float(parts[5]),
                    "unique_txs": int(parts[6]),
                })
            except ValueError:
                continue

    if skipped_no_fault:
        print(
            "Warning: {0} data rows in {1} had no crash-count section; "
            "use --default-crashed or merge stats with section headers.".format(
                skipped_no_fault, path
            ),
            file=sys.stderr,
        )

    return rows


def combine(group):
    N = sum(r["unique_txs"] for r in group)
    if N == 0:
        return None

    mu = sum(r["unique_txs"] * r["avg_ms"] for r in group) / N

    combined_var = sum(
        r["unique_txs"] * (r["stddev_ms"] ** 2 + (r["avg_ms"] - mu) ** 2)
        for r in group
    ) / N

    return {
        "instances": group[0]["instances"],
        "crashed_validators": group[0]["crashed_validators"],
        "num_runs": len(group),
        "avg_ms": mu,
        "stddev_ms": math.sqrt(combined_var),
        "min_ms": min(r["min_ms"] for r in group),
        "max_ms": max(r["max_ms"] for r in group),
        "unique_txs": N,
    }


def aggregate(rows, only_crashed=None):
    groups = defaultdict(list)
    for r in rows:
        if only_crashed is not None and r["crashed_validators"] != only_crashed:
            continue
        groups[(r["crashed_validators"], r["instances"])].append(r)

    aggregated = []
    for (crashed, instances), group in sorted(groups.items()):
        combined = combine(group)
        if combined:
            aggregated.append(combined)
    return aggregated


def write_aggregated(output_path, aggregated):
    os.makedirs(os.path.dirname(output_path) or ".", exist_ok=True)
    with open(output_path, "w", newline="") as f:
        writer = csv.writer(f)
        writer.writerow(["instances", "crashed_validators", "num_runs",
                         "avg_ms", "stddev_ms", "min_ms", "max_ms", "unique_txs"])
        for row in aggregated:
            writer.writerow([
                row["instances"],
                row["crashed_validators"],
                row["num_runs"],
                "{0:.3f}".format(row["avg_ms"]),
                "{0:.3f}".format(row["stddev_ms"]),
                "{0:.3f}".format(row["min_ms"]),
                "{0:.3f}".format(row["max_ms"]),
                row["unique_txs"],
            ])


def legacy_argv_passthrough(argv):
    """Support: script.py [input_csv [output_csv]] with no flags."""
    if any(a.startswith("-") for a in argv):
        return None
    if len(argv) == 0:
        return DEFAULT_INPUT, DEFAULT_OUTPUT, None, None, []
    if len(argv) == 1:
        return argv[0], DEFAULT_OUTPUT, None, None, []
    if len(argv) == 2:
        return argv[0], argv[1], None, None, []
    return None


def main():
    argv = sys.argv[1:]
    legacy = legacy_argv_passthrough(argv)

    if legacy is not None:
        input_path, output_path, _, _, _ = legacy
        input_paths = [input_path]
        default_crashed = None
        only_crashed = None
    else:
        p = argparse.ArgumentParser(description=__doc__)
        p.add_argument(
            "inputs",
            nargs="*",
            default=[DEFAULT_INPUT],
            help="stats.csv file(s) (default: repo root stats.csv)",
        )
        p.add_argument(
            "-o", "--output",
            default=None,
            help="output CSV path (default: repo aggregated_stats.csv, or "
                 "aggregated_1val_crashed/aggregated_stats.csv if --only-crashed 1)",
        )
        p.add_argument(
            "--default-crashed",
            type=int,
            metavar="N",
            help="treat rows without a '=== N VALIDATORS CRASHED ===' section as N",
        )
        p.add_argument(
            "--only-crashed",
            type=int,
            metavar="N",
            help="only write rows for N crashed validators (e.g. 1)",
        )
        args = p.parse_args(argv)
        input_paths = args.inputs
        if args.output is not None:
            output_path = args.output
        elif args.only_crashed == 1:
            output_path = DEFAULT_OUTPUT_1VAL_CRASHED
        else:
            output_path = DEFAULT_OUTPUT
        default_crashed = args.default_crashed
        only_crashed = args.only_crashed

    all_rows = []
    for path in input_paths:
        if not os.path.isfile(path):
            print("Error: not a file: {0}".format(path), file=sys.stderr)
            sys.exit(1)
        all_rows.extend(parse_stats_csv(path, default_crashed=default_crashed))

    if not all_rows:
        print("No data found in {0}".format(", ".join(input_paths)))
        sys.exit(1)

    aggregated = aggregate(all_rows, only_crashed=only_crashed)
    if not aggregated:
        print("No groups left after filtering.", file=sys.stderr)
        sys.exit(1)

    write_aggregated(output_path, aggregated)

    print("Written {0} rows to {1}".format(len(aggregated), output_path))
    print(
        "Parsed {0} input rows from {1} → {2} aggregated rows.".format(
            len(all_rows),
            ", ".join(input_paths),
            len(aggregated),
        )
    )


if __name__ == "__main__":
    main()
