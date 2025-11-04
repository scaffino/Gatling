#!/usr/bin/env python3

import argparse
import os
import re
import sys
from glob import glob
from typing import Dict, List, Tuple, Set


GATLING_LINE_RE = re.compile(
    r"^\[gatling\]\s+Validator\s+\d+\s+finalized\s+block\s+(?P<block>\d+)\s+from\s+instance\s+(?P<instance>\d+)\s+\(view\s+(?P<view>\d+)\)",
    re.IGNORECASE,
)


def parse_gatling_file(path: str) -> Tuple[List[Tuple[int, int, int]], Dict[int, Set[int]]]:
    sequence: List[Tuple[int, int, int]] = []  # (instance, block, view)
    blocks_by_instance: Dict[int, Set[int]] = {}

    with open(path, "r", encoding="utf-8") as f:
        for line_num, raw in enumerate(f, start=1):
            line = raw.strip()
            if not line:
                continue
            m = GATLING_LINE_RE.search(line)
            if not m:
                # Ignore lines that don't match the expected gatling format
                continue
            block = int(m.group("block"))
            instance = int(m.group("instance"))
            view = int(m.group("view"))
            sequence.append((instance, block, view))
            blocks_by_instance.setdefault(instance, set()).add(block)

    return sequence, blocks_by_instance


def find_first_diff(a: List[Tuple[int, int, int]], b: List[Tuple[int, int, int]]):
    n = min(len(a), len(b))
    for i in range(n):
        if a[i] != b[i]:
            return i, a[i], b[i]
    if len(a) != len(b):
        return n, None, None
    return None


def check_no_holes(blocks: Set[int]) -> Tuple[bool, List[int]]:
    if not blocks:
        return True, []
    min_b = min(blocks)
    max_b = max(blocks)
    missing = [b for b in range(min_b, max_b + 1) if b not in blocks]
    return len(missing) == 0, missing


def main() -> int:
    parser = argparse.ArgumentParser(description="Verify gatling logs for consistent sequences and completeness.")
    parser.add_argument(
        "--dir",
        default=None,
        help="Directory containing gatling_X.log files (defaults to <repo>/logs/gatling).",
    )
    args = parser.parse_args()

    script_dir = os.path.dirname(os.path.abspath(__file__))
    default_dir = os.path.join(script_dir, "logs", "gatling")
    target_dir = args.dir or default_dir

    if not os.path.isdir(target_dir):
        print(f"ERROR: Directory not found: {target_dir}", file=sys.stderr)
        return 2

    paths = sorted(glob(os.path.join(target_dir, "gatling_*.log")))
    if not paths:
        print(f"ERROR: No gatling_*.log files found in {target_dir}", file=sys.stderr)
        return 2

    # Parse all files
    sequences: Dict[str, List[Tuple[int, int, int]]] = {}
    per_file_blocks: Dict[str, Dict[int, Set[int]]] = {}

    total_entries = 0
    for p in paths:
        seq, blocks_map = parse_gatling_file(p)
        sequences[p] = seq
        per_file_blocks[p] = blocks_map
        total_entries += len(seq)

    print(f"Check: parsed files - OK (files={len(paths)}, total_entries={total_entries})")

    # Ensure all sequences are identical (same order and content of (instance, block, view))
    ref_file = paths[0]
    ref_seq = sequences[ref_file]
    ok = True

    mismatch_count = 0
    for p in paths[1:]:
        seq = sequences[p]
        diff = find_first_diff(ref_seq, seq)
        if diff is not None:
            idx, a, b = diff
            if a is None and b is None:
                # Length mismatch after a matching common prefix: ignore per user request
                continue
            else:
                print(
                    f"ERROR: Sequence mismatch at index {idx} between files:\n  {ref_file}: {ref_seq[idx]}\n  {p}: {seq[idx]}",
                    file=sys.stderr,
                )
                ok = False
                mismatch_count += 1

    if mismatch_count == 0:
        print("Check: common-prefix sequence consistency - OK")
    else:
        print(f"Check: common-prefix sequence consistency - FAIL (mismatches={mismatch_count})")

    # Check for holes per instance in each file
    holes_count = 0
    for p, blocks_map in per_file_blocks.items():
        for instance, blocks in sorted(blocks_map.items()):
            complete, missing = check_no_holes(blocks)
            if not complete:
                print(
                    f"ERROR: Missing blocks for instance {instance} in {p}: {missing}",
                    file=sys.stderr,
                )
                ok = False
                holes_count += 1

    if holes_count == 0:
        print("Check: per-instance block completeness - OK")
    else:
        print(f"Check: per-instance block completeness - FAIL (instances_with_holes={holes_count})")

    if ok:
        print("All gatling logs are consistent and complete.")
        return 0
    else:
        return 1


if __name__ == "__main__":
    sys.exit(main())
