#!/usr/bin/env python3

import argparse
import os
import re
import sys
from glob import glob
from collections import namedtuple


GATLING_LINE_RE = re.compile(
    (
        r"^(?:[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\."
        r"[0-9]+Z\s+)?"
        r"\[gatling\]\s+Validator\s+(?P<validator>\d+)\s+finalized\s+block\s+"
        r"(?P<block>\d+)\s+from\s+instance\s+(?P<instance>\d+)\s+\(view\s+"
        r"(?P<view>\d+)\).*$"
    ),
    re.IGNORECASE,
)

# Matches: gatling_i{N}_{id}.log  (id = hex string or alphabetic location name)
FILENAME_RE = re.compile(r"^gatling_i(\d+)_([0-9a-z]+)\.log$", re.IGNORECASE)

GatlingEntry = namedtuple('GatlingEntry', [
    'validator',
    'block',
    'instance',
    'view',
    'original_text',
])


def get_repo_root():
    script_dir = os.path.dirname(os.path.abspath(__file__))
    return os.path.dirname(script_dir)


def parse_gatling_file_all(path):
    entries = []
    with open(path, "r", encoding="utf-8") as f:
        for raw in f:
            line = raw.rstrip("\n")
            if not line.strip():
                continue
            m = GATLING_LINE_RE.search(line)
            if not m:
                continue
            entries.append(GatlingEntry(
                validator=int(m.group("validator")),
                block=int(m.group("block")),
                instance=int(m.group("instance")),
                view=int(m.group("view")),
                original_text=line.strip(),
            ))
    return entries


def extract_instance_count_from_filename(path):
    """Return the instance count embedded in the filename, or None."""
    base = os.path.basename(path)
    m = FILENAME_RE.match(base)
    if m:
        return int(m.group(1))
    return None


def find_first_diff(a, b):
    n = min(len(a), len(b))
    for i in range(n):
        if a[i] != b[i]:
            return i, a[i], b[i]
    if len(a) != len(b):
        return n, None, None
    return None


def check_no_holes(blocks):
    if not blocks:
        return True, []
    min_b = min(blocks)
    max_b = max(blocks)
    missing = [b for b in range(min_b, max_b + 1) if b not in blocks]
    return len(missing) == 0, missing


def verify_group(paths, parsed, instance_count):
    """Verify a group of gatling log files with the same instance count."""
    ok = True
    sequences = {}
    per_file_blocks = {}

    for p in paths:
        seq = []
        blocks_by_instance = {}
        for e in parsed[p]:
            seq.append((e.block, e.view, e.instance))
            blocks_by_instance.setdefault(e.instance, set()).add(e.block)
        sequences[p] = seq
        per_file_blocks[p] = blocks_by_instance

    instance_msg = (
        "instance_count={}".format(instance_count)
        if instance_count is not None else ""
    )
    if instance_msg:
        print("Verifying group ({}, files={})...".format(instance_msg, len(paths)))

    sorted_list = sorted(sequences.keys())
    if not sorted_list:
        return True

    ref_file = sorted_list[0]
    ref_seq = sequences[ref_file]
    mismatch_count = 0

    for p in sorted_list[1:]:
        seq = sequences[p]
        diff = find_first_diff(ref_seq, seq)
        if diff is not None:
            idx, a, b = diff
            if a is None and b is None:
                continue
            ref_prev = ref_seq[idx - 1] if idx - 1 >= 0 else None
            ref_next = ref_seq[idx + 1] if idx + 1 < len(ref_seq) else None
            seq_prev = seq[idx - 1] if idx - 1 >= 0 else None
            seq_next = seq[idx + 1] if idx + 1 < len(seq) else None

            def fmt(t):
                if t is None:
                    return "<none>"
                bnum, v, inst = t
                return "(block={}, view={}, instance={})".format(bnum, v, inst)

            group_info = "  group: {}\n".format(instance_msg) if instance_msg else ""
            print((
                "ERROR: Order mismatch detected\n"
                "{group_info}"
                "  at index {idx}:\n"
                "    {ref_base} -> {ref_val}\n"
                "    {p_base} -> {p_val}\n"
                "  context (previous):\n"
                "    {ref_base} -> {ref_prev}\n"
                "    {p_base} -> {p_prev}\n"
                "  context (next):\n"
                "    {ref_base} -> {ref_next}\n"
                "    {p_base} -> {p_next}"
            ).format(
                group_info=group_info,
                idx=idx,
                ref_base=os.path.basename(ref_file),
                ref_val=fmt(ref_seq[idx]),
                p_base=os.path.basename(p),
                p_val=fmt(seq[idx]),
                ref_prev=fmt(ref_prev),
                p_prev=fmt(seq_prev),
                ref_next=fmt(ref_next),
                p_next=fmt(seq_next),
            ), file=sys.stderr)
            ok = False
            mismatch_count += 1

    msg_suffix = " ({})".format(instance_msg) if instance_msg else ""
    if mismatch_count == 0:
        print("  Check: sequence consistency{} - OK".format(msg_suffix))
    else:
        print("  Check: sequence consistency{} - FAIL (mismatches={})".format(
            msg_suffix, mismatch_count))

    holes_count = 0
    for p, blocks_map in per_file_blocks.items():
        for instance, blocks in sorted(blocks_map.items()):
            complete, missing = check_no_holes(blocks)
            if not complete:
                print(
                    "ERROR: Missing blocks within instance in "
                    "{}: instance {} misses block(s) {}".format(p, instance, missing),
                    file=sys.stderr,
                )
                ok = False
                holes_count += 1

    if holes_count == 0:
        print("  Check: per-instance block completeness{} - OK".format(msg_suffix))
    else:
        print("  Check: per-instance block completeness{} - FAIL (instances_with_holes={})".format(
            msg_suffix, holes_count))

    return ok


def main():
    parser = argparse.ArgumentParser(
        description=(
            "Verify gatling logs: check cross-validator ordering and "
            "per-instance completeness."
        )
    )
    parser.add_argument(
        "--dir",
        default=None,
        help=(
            "Directory containing gatling_i*_*.log files "
            "(defaults to <repo>/logs/gatling)."
        ),
    )
    args = parser.parse_args()

    default_dir = os.path.join(get_repo_root(), "logs", "gatling")
    target_dir = args.dir or default_dir

    if not os.path.isdir(target_dir):
        print("ERROR: Directory not found: {}".format(target_dir), file=sys.stderr)
        return 2

    paths = sorted(glob(os.path.join(target_dir, "gatling_i*_*.log")))
    if not paths:
        print(
            "ERROR: No gatling_i*_*.log files found in {}".format(target_dir),
            file=sys.stderr,
        )
        return 2

    parsed = {}
    for p in paths:
        parsed[p] = parse_gatling_file_all(p)

    # Group files by instance count
    groups = {}
    for p in paths:
        instance_count = extract_instance_count_from_filename(p)
        groups.setdefault(instance_count, []).append(p)

    all_ok = True
    for instance_count, group_paths in sorted(groups.items()):
        if not verify_group(group_paths, parsed, instance_count):
            all_ok = False

    if all_ok:
        print("All gatling logs are consistent.")
        print()
        return 0
    else:
        return 1


if __name__ == "__main__":
    sys.exit(main())
