#!/usr/bin/env python3

import argparse
import os
import re
import sys
from glob import glob
from typing import Dict, List, Tuple, Set, NamedTuple, Optional


GATLING_LINE_RE = re.compile(
    (
        r"^\[gatling\]\s+Validator\s+(?P<validator>\d+)\s+finalized\s+block\s+"
        r"(?P<block>\d+)\s+from\s+instance\s+(?P<instance>\d+)\s+\(view\s+"
        r"(?P<view>\d+)\).*$"
    ),
    re.IGNORECASE,
)


class GatlingEntry(NamedTuple):
    validator: int
    block: int
    instance: int
    view: int
    is_ancestor: bool
    original_text: str


def parse_gatling_file_all(path: str) -> List[GatlingEntry]:
    entries: List[GatlingEntry] = []

    with open(path, "r", encoding="utf-8") as f:
        for _, raw in enumerate(f, start=1):
            line = raw.rstrip("\n")
            if not line.strip():
                continue
            m = GATLING_LINE_RE.search(line)
            if not m:
                continue
            validator = int(m.group("validator"))
            block = int(m.group("block"))
            instance = int(m.group("instance"))
            view = int(m.group("view"))
            # Detect optional ancestor suffix anywhere in the trailing text
            is_ancestor = "- ancestor" in line.lower()
            entries.append(
                GatlingEntry(
                    validator=validator,
                    block=block,
                    instance=instance,
                    view=view,
                    is_ancestor=is_ancestor,
                    original_text=line.strip(),
                )
            )

    return entries


def write_sorted_gatling_file(
    original_path: str, entries: List[GatlingEntry]
) -> str:
    dir_name = os.path.dirname(original_path)
    base = os.path.basename(original_path)
    name, ext = os.path.splitext(base)
    sorted_name = f"{name}_sorted{ext}"
    sorted_path = os.path.join(dir_name, sorted_name)

    # Sort by view asc, then instance asc; keep stable order for ties
    sorted_entries = sorted(entries, key=lambda e: (e.view, e.instance))

    with open(sorted_path, "w", encoding="utf-8") as out:
        for e in sorted_entries:
            out.write(e.original_text + "\n")

    return sorted_path


def find_first_diff(
    a: List[Tuple[int, int, int]], b: List[Tuple[int, int, int]]
):
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
    parser = argparse.ArgumentParser(
        description=(
            "Verify gatling logs: create *_sorted logs (by view, instance) "
            "and check cross-validator ordering."
        )
    )
    parser.add_argument(
        "--dir",
        default=None,
        help=(
            "Directory containing gatling_X.log files "
            "(defaults to <repo>/logs/gatling)."
        ),
    )
    args = parser.parse_args()

    script_dir = os.path.dirname(os.path.abspath(__file__))
    default_dir = os.path.join(script_dir, "logs", "gatling")
    target_dir = args.dir or default_dir

    if not os.path.isdir(target_dir):
        print(f"ERROR: Directory not found: {target_dir}", file=sys.stderr)
        return 2

    paths = sorted(glob(os.path.join(target_dir, "gatling_*.log")))
    # Ignore already-sorted files to avoid creating *_sorted_sorted.log
    paths = [
        p for p in paths if not os.path.basename(p).endswith("_sorted.log")
    ]
    if not paths:
        print(
            f"ERROR: No gatling_*.log files found in {target_dir}",
            file=sys.stderr,
        )
        return 2
    # 1) Read gatling log files and parse relevant lines
    parsed: Dict[str, List[GatlingEntry]] = {}
    total_matched = 0
    for p in paths:
        entries = parse_gatling_file_all(p)
        parsed[p] = entries
        total_matched += len(entries)

    parsed_msg = (
        f"Parsed gatling logs - OK (files={len(paths)}, "
        f"matched_lines={total_matched})"
    )
    print(parsed_msg)

    # 2) Create *_sorted files sorted by (view asc, instance asc)
    sorted_paths: Dict[str, str] = {}
    for p in paths:
        sorted_path = write_sorted_gatling_file(p, parsed[p])
        sorted_paths[p] = sorted_path

    print(f"Created sorted logs - OK (files={len(sorted_paths)})")

    # Build sequences from sorted files for cross-validator comparison
    sequences: Dict[str, List[Tuple[int, int, int]]] = {}
    per_file_blocks: Dict[str, Dict[int, Set[int]]] = {}

    for orig_path, sorted_path in sorted_paths.items():
        seq: List[Tuple[int, int, int]] = []  # (block, view, instance)
        blocks_by_instance: Dict[int, Set[int]] = {}

        # Re-parse from the sorted file to reflect exactly what was written
        entries = parse_gatling_file_all(sorted_path)
        for e in entries:
            seq.append((e.block, e.view, e.instance))
            blocks_by_instance.setdefault(e.instance, set()).add(e.block)

        sequences[sorted_path] = seq
        per_file_blocks[sorted_path] = blocks_by_instance

    # 3) Verify all sorted files show same ordering of (block, view, instance)
    ok = True
    sorted_list = sorted(sequences.keys())
    ref_file = sorted_list[0]
    ref_seq = sequences[ref_file]
    mismatch_count = 0

    for p in sorted_list[1:]:
        seq = sequences[p]
        diff = find_first_diff(ref_seq, seq)
        if diff is not None:
            idx, a, b = diff
            if a is None and b is None:
                # Length mismatch is OK - validators can be behind
                # Sequences matched up to the common prefix length
                continue
            else:
                ref_prev = ref_seq[idx - 1] if idx - 1 >= 0 else None
                ref_next = ref_seq[idx + 1] if idx + 1 < len(ref_seq) else None
                seq_prev = seq[idx - 1] if idx - 1 >= 0 else None
                seq_next = seq[idx + 1] if idx + 1 < len(seq) else None

                def fmt(t: Optional[Tuple[int, int, int]]) -> str:
                    if t is None:
                        return "<none>"
                    bnum, v, inst = t
                    return f"(block={bnum}, view={v}, instance={inst})"

                ref_base = os.path.basename(ref_file)
                p_base = os.path.basename(p)
                print(
                    (
                        "ERROR: Sorted order mismatch detected\n"
                        f"  at index {idx}:\n"
                        f"    {ref_base} -> {fmt(ref_seq[idx])}\n"
                        f"    {p_base} -> {fmt(seq[idx])}\n"
                        "  context (previous):\n"
                        f"    {ref_base} -> {fmt(ref_prev)}\n"
                        f"    {p_base} -> {fmt(seq_prev)}\n"
                        "  context (next):\n"
                        f"    {ref_base} -> {fmt(ref_next)}\n"
                        f"    {p_base} -> {fmt(seq_next)}"
                    ),
                    file=sys.stderr,
                )
                ok = False
                mismatch_count += 1

    if mismatch_count == 0 and ok:
        print("Check: sorted sequence consistency across validators - OK")
    else:
        print(
            (
                "Check: sorted sequence consistency across validators - FAIL "
                f"(mismatches={mismatch_count})"
            )
        )

    # Optional: per-instance block continuity in the sorted outputs
    holes_count = 0
    for p, blocks_map in per_file_blocks.items():
        for instance, blocks in sorted(blocks_map.items()):
            complete, missing = check_no_holes(blocks)
            if not complete:
                print(
                    (
                        "ERROR: Missing blocks within instance in "
                        f"{p}: instance {instance} misses block(s) {missing}"
                    ),
                    file=sys.stderr,
                )
                ok = False
                holes_count += 1

    if holes_count == 0:
        print("Check: per-instance block completeness (sorted) - OK")
    else:
        print(
            (
                "Check: per-instance block completeness (sorted) - FAIL "
                f"(instances_with_holes={holes_count})"
            )
        )

    if ok:
        print("All sorted gatling logs are consistent.")
        return 0
    else:
        return 1


if __name__ == "__main__":
    sys.exit(main())
