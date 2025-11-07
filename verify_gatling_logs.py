#!/usr/bin/env python3

import argparse
import os
import re
import sys
from glob import glob
from typing import Dict, List, Tuple, Set, NamedTuple, Optional


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


class GatlingEntry(NamedTuple):
    validator: int
    block: int
    instance: int
    view: int
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
            entries.append(
                GatlingEntry(
                    validator=validator,
                    block=block,
                    instance=instance,
                    view=view,
                    original_text=line.strip(),
                )
            )

    return entries


# No sorted-file generation.
# We operate directly on original gatling_*.log files.


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


def extract_instance_count_from_filename(path: str) -> Optional[int]:
    """Extract instance count from filename.
    Preferred format: gatling_v{validator}_i{instances}_r{run}.log
    Back-compat: gatling_{a}_{b}.log (instances=b) and gatling_{a}.log.
    Returns None if format doesn't match or instances unspecified.
    """
    base = os.path.basename(path)
    # New format: gatling_vX_iY_rZ.log -> return Y
    m = re.match(r"^gatling_v(\d+)_i(\d+)_r(\d+)\.log$", base, re.IGNORECASE)
    if m:
        return int(m.group(2))
    # Intermediate format: gatling_A_B.log -> return B
    m = re.match(r"^gatling_(\d+)_(\d+)\.log$", base, re.IGNORECASE)
    if m:
        return int(m.group(2))
    # Old format: gatling_A.log -> instances unknown
    m = re.match(r"^gatling_(\d+)\.log$", base, re.IGNORECASE)
    if m:
        return None
    return None


def verify_group(
    paths: List[str],
    parsed: Dict[str, List[GatlingEntry]],
    instance_count: Optional[int],
) -> bool:
    """Verify a group of gatling log files with the same instance count.
    Returns True if all checks pass, False otherwise."""
    ok = True

    # Build sequences from original files for cross-validator comparison
    sequences: Dict[str, List[Tuple[int, int, int]]] = {}
    per_file_blocks: Dict[str, Dict[int, Set[int]]] = {}

    for p in paths:
        seq: List[Tuple[int, int, int]] = []  # (block, view, instance)
        blocks_by_instance: Dict[int, Set[int]] = {}

        entries = parsed[p]
        for e in entries:
            seq.append((e.block, e.view, e.instance))
            blocks_by_instance.setdefault(e.instance, set()).add(e.block)

        sequences[p] = seq
        per_file_blocks[p] = blocks_by_instance

    # Create instance message only if instance_count is available
    instance_msg = (
        f"instance_count={instance_count}"
        if instance_count is not None
        else ""
    )
    if instance_msg:
        print(f"Verifying group ({instance_msg}, files={len(paths)})...")

    # Verify all files show same ordering of (block, view, instance)
    sorted_list = sorted(sequences.keys())
    if len(sorted_list) == 0:
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
                group_info = (
                    f"  group: {instance_msg}\n" if instance_msg else ""
                )
                print(
                    (
                        "ERROR: Order mismatch detected\n"
                        f"{group_info}"
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

    if mismatch_count == 0:
        msg_suffix = f" ({instance_msg})" if instance_msg else ""
        print(f"  Check: sequence consistency{msg_suffix} - OK")
    else:
        msg_suffix = f" ({instance_msg})" if instance_msg else ""
        print(
            (
                f"  Check: sequence consistency{msg_suffix} - FAIL "
                f"(mismatches={mismatch_count})"
            )
        )

    # Per-instance block continuity in the original outputs
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
        msg_suffix = f" ({instance_msg})" if instance_msg else ""
        print(
            f"  Check: per-instance block completeness{msg_suffix} - OK"
        )
    else:
        msg_suffix = f" ({instance_msg})" if instance_msg else ""
        print(
            (
                f"  Check: per-instance block completeness{msg_suffix} "
                f"- FAIL (instances_with_holes={holes_count})"
            )
        )

    return ok


def main() -> int:
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
            "Directory containing gatling_v*_i*_r*.log (preferred), "
            "or legacy gatling_*.log files (defaults to <repo>/logs/gatling). "
            "Files are grouped by "
            "instance count and verified separately."
        ),
    )
    args = parser.parse_args()

    script_dir = os.path.dirname(os.path.abspath(__file__))
    default_dir = os.path.join(script_dir, "logs", "gatling")
    target_dir = args.dir or default_dir

    if not os.path.isdir(target_dir):
        print(f"ERROR: Directory not found: {target_dir}", file=sys.stderr)
        return 2

    # Prefer new-format files first; fall back to legacy if none found
    preferred = sorted(glob(os.path.join(target_dir, "gatling_v*_i*_r*.log")))
    if preferred:
        paths = preferred
    else:
        paths = sorted(glob(os.path.join(target_dir, "gatling_*.log")))
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

    #parsed_msg = (
    #    f"Parsed gatling logs - OK (files={len(paths)}, "
    #    f"matched_lines={total_matched})"
    #)
    #print(parsed_msg)

    # 2) Group files by instance count
    groups: Dict[Optional[int], List[str]] = {}
    for p in paths:
        instance_count = extract_instance_count_from_filename(p)
        groups.setdefault(instance_count, []).append(p)

    #print(f"Grouped files by instance count - OK (groups={len(groups)})")

    # 3) Verify each group separately
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
