#!/usr/bin/env python3
"""WARN gate: changed-lines coverage must stay at or above the threshold.

Inputs are a ``cargo llvm-cov --lcov`` report and a zero-context
``git diff base head`` over Rust files. A changed line counts when lcov lists
it as executable (a ``DA`` record); it is covered when its execution count is
positive. The gate reports covered/coverable over the intersection and warns
below ``--threshold`` percent (default 80). A patch with no coverable changed
lines passes with a note.

WARN mode: a shortfall prints ``::warning::`` annotations and the script
exits 0. A blind gate (missing lcov, unreadable diff) also warns instead of
failing; it must never turn a PR red. Only malformed arguments exit non-zero.
"""

import argparse
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MAX_ANNOTATIONS = 10


def read_lcov(path):
    """Map of (file, line) -> hit count for executable lines."""
    try:
        text = Path(path).read_text()
    except OSError as exc:
        return None, f"lcov report unreadable: {exc}"
    hits = {}
    current = None
    for raw in text.splitlines():
        if raw.startswith("SF:"):
            current = raw[3:].strip()
        elif raw.startswith("DA:") and current is not None:
            parts = raw[3:].split(",")
            try:
                hits[(current, int(parts[0]))] = int(parts[1])
            except (ValueError, IndexError):
                continue
        elif raw.strip() == "end_of_record":
            current = None
    return hits, ""


def added_lines(base_ref, head_ref):
    """Added (file, new-line) pairs from a zero-context diff of Rust files."""
    try:
        proc = subprocess.run(
            ["git", "-C", str(ROOT), "diff", "-U0", "--no-color", "--no-renames",
             f"{base_ref}", f"{head_ref}", "--", "*.rs"],
            capture_output=True,
            text=True,
            timeout=120,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return None, f"git diff did not run: {exc}"
    if proc.returncode != 0:
        return None, f"git diff exited {proc.returncode}: {proc.stderr.strip()[:200]}"
    added = set()
    path, new_line = None, 0
    for raw in proc.stdout.splitlines():
        if raw.startswith("+++ b/"):
            path = raw[len("+++ b/"):]
            continue
        match = re.match(r"@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@", raw)
        if match:
            new_line = int(match.group(1))
            continue
        if path is None:
            continue
        if raw.startswith("+") and not raw.startswith("+++"):
            added.add((path, new_line))
            new_line += 1
        elif raw.startswith("-") and not raw.startswith("---"):
            continue
        else:
            new_line += 1
    return added, ""


def normalize(path):
    """Match lcov source paths (absolute or workspace-relative) to diff paths."""
    text = str(path)
    try:
        return str(Path(text).relative_to(ROOT))
    except ValueError:
        return text.lstrip("./")


def evaluate(hits, added):
    """(covered, coverable, uncovered-added-lines) over changed executable lines."""
    by_file = {}
    for (path, line), count in hits.items():
        by_file.setdefault(normalize(path), {})[line] = count
    covered, uncovered = 0, []
    for path, line in sorted(added):
        count = by_file.get(path, {}).get(line)
        if count is None:
            continue
        if count > 0:
            covered += 1
        else:
            uncovered.append((path, line))
    return covered, covered + len(uncovered), uncovered


def warn(message):
    print(f"::warning::{message}", flush=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lcov", required=True)
    parser.add_argument("--base-ref", default="origin/main")
    parser.add_argument("--head-ref", default="HEAD")
    parser.add_argument("--threshold", type=float, default=80.0)
    args = parser.parse_args(argv)
    if not 0 < args.threshold <= 100:
        parser.error("--threshold must be in (0, 100]")

    hits, error = read_lcov(args.lcov)
    if hits is None:
        warn(f"patch coverage gate is blind ({error}); treating as pass")
        return 0

    added, error = added_lines(args.base_ref, args.head_ref)
    if added is None:
        warn(f"patch coverage gate could not read the diff ({error}); treating as pass")
        return 0

    covered, coverable, uncovered = evaluate(hits, added)
    if not coverable:
        print("patch coverage gate: no coverable changed lines; nothing to measure")
        return 0
    pct = 100.0 * covered / coverable
    print(f"patch coverage gate: {covered}/{coverable} changed lines covered ({pct:.1f}%)")
    if pct >= args.threshold:
        return 0
    for path, line in uncovered[:MAX_ANNOTATIONS]:
        warn(f"{path}:{line} changed but not covered by tests")
    if len(uncovered) > MAX_ANNOTATIONS:
        warn(f"... and {len(uncovered) - MAX_ANNOTATIONS} more uncovered changed line(s)")
    warn(f"patch coverage gate: {pct:.1f}% below the {args.threshold:g}% WARN threshold")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
