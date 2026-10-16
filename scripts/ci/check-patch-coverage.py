#!/usr/bin/env python3
"""HARD gate: changed-lines coverage must stay at or above the threshold.

Inputs are a ``cargo llvm-cov --lcov`` report and a zero-context
``git diff base head`` over Rust files. A changed line counts when lcov lists
it as executable (a ``DA`` record); it is covered when its execution count is
positive. The gate reports covered/coverable over the intersection and fails
below ``--threshold`` percent (default 80). A patch with no coverable changed
lines passes with a note. A changed line in a measured Rust source file
(``crates/*/src/**`` or ``src/**``) that has no lcov record at all counts as
uncovered when the file defines a function body, so uninstrumented code can
never pass as "nothing to measure" while functionless module/re-export files
remain outside the executable-line denominator.

HARD mode: a shortfall prints ``::error::`` annotations and exits 1. A blind
gate (missing or unusable lcov, unreadable diff) also errors and exits 1; it
must never turn a blind PR green. The check.yml measurement wrapper uses
``--ignore-run-fail`` to collect coverage despite unit-test failures, which are
owned HARD by the rust-tests lane. Build or report-generation failures still
fail this lane. Malformed arguments exit non-zero.
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
    if not text.strip():
        return None, "lcov report is empty"
    hits = {}
    current = None
    data_records = 0
    malformed_records = False
    for raw in text.splitlines():
        if raw.startswith("SF:"):
            if current is not None:
                malformed_records = True
            current = raw[3:].strip() or None
        elif raw.startswith("DA:"):
            parts = raw[3:].split(",")
            try:
                line, count = int(parts[0]), int(parts[1])
            except (ValueError, IndexError):
                malformed_records = True
                continue
            if current is None or line < 1 or count < 0:
                malformed_records = True
                continue
            hits[(current, line)] = count
            data_records += 1
        elif raw.strip() == "end_of_record":
            current = None
    if current is not None:
        malformed_records = True
    if malformed_records:
        return None, "lcov report contains malformed DA records"
    if not data_records:
        return None, "lcov report contains no valid DA records"
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


def is_measurable_source(path):
    """Workspace-relative Rust sources the lib+bins+tests report must instrument.

    Test-only files (``*_test.rs``, ``*_tests.rs``, ``test_*.rs``,
    ``tests.rs``, or anything under a ``tests`` directory) are excluded:
    changing only tests must not fail the gate.
    """
    if not path.endswith(".rs"):
        return False
    parts = path.split("/")
    filename = parts[-1]
    if filename in ("tests.rs",):
        return False
    if (filename.startswith("test_") or filename.endswith("_test.rs")
            or filename.endswith("_tests.rs")):
        return False
    if "tests" in parts:
        return False
    if len(parts) >= 4 and parts[0] == "crates" and parts[2] == "src":
        return True
    if len(parts) >= 2 and parts[0] == "src":
        return True
    return False


def has_instrumentable_function(path):
    """Whether a source file contains a function body the coverage report can map."""
    try:
        source = (ROOT / path).read_text()
    except OSError:
        # An unreadable changed source is not evidence that the gate may skip it.
        return True
    return re.search(
        r'(?m)^\s*(?:(?:pub(?:\([^)]*\))?|async|const|unsafe|default)\s+)*'
        r'(?:extern(?:\s+"[^"]+")?\s+)?fn\s+(?:r#)?\w+'
        r'(?:[^;{}]|\[[^]]*\]|\([^)]*\))*\{',
        source,
    ) is not None


def evaluate(hits, added):
    """(covered, coverable, uncovered-added-lines) over changed executable lines."""
    by_file = {}
    for (path, line), count in hits.items():
        by_file.setdefault(normalize(path), {})[line] = count
    covered, uncovered = 0, []
    for path, line in sorted(added):
        norm = normalize(path)
        file_cov = by_file.get(norm)
        if file_cov is None:
            # No lcov record for this file at all. A changed source file the
            # report must instrument counts as uncovered, so bin code or a new
            # file can never pass as "nothing to measure".
            if is_measurable_source(norm) and has_instrumentable_function(norm):
                uncovered.append((path, line))
            continue
        count = file_cov.get(line)
        if count is None:
            continue
        if count > 0:
            covered += 1
        else:
            uncovered.append((path, line))
    return covered, covered + len(uncovered), uncovered


def error(message):
    print(f"::error::{message}", flush=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lcov", required=True)
    parser.add_argument("--base-ref", default="origin/main")
    parser.add_argument("--head-ref", default="HEAD")
    parser.add_argument("--threshold", type=float, default=80.0)
    args = parser.parse_args(argv)
    if not 0 < args.threshold <= 100:
        parser.error("--threshold must be in (0, 100]")

    hits, read_error = read_lcov(args.lcov)
    if hits is None:
        error(f"patch coverage gate is blind ({read_error}); failing closed")
        return 1

    added, diff_error = added_lines(args.base_ref, args.head_ref)
    if added is None:
        error(f"patch coverage gate could not read the diff ({diff_error}); failing closed")
        return 1

    covered, coverable, uncovered = evaluate(hits, added)
    if not coverable:
        print("patch coverage gate: no coverable changed lines; nothing to measure")
        return 0
    pct = 100.0 * covered / coverable
    print(f"patch coverage gate: {covered}/{coverable} changed lines covered ({pct:.1f}%)")
    if pct >= args.threshold:
        return 0
    for path, line in uncovered[:MAX_ANNOTATIONS]:
        error(f"{path}:{line} changed but not covered by tests")
    if len(uncovered) > MAX_ANNOTATIONS:
        error(f"... and {len(uncovered) - MAX_ANNOTATIONS} more uncovered changed line(s)")
    error(f"patch coverage gate: {pct:.1f}% below the {args.threshold:g}% HARD threshold")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
