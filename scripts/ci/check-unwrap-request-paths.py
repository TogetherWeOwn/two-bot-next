#!/usr/bin/env python3
"""WARN gate: no new `.unwrap()` in non-test request paths.

The request-path rule is defined by
``ast-grep-rules/no-unwrap-request-paths.yml`` (request-path file globs with
test files/dirs ignored). Six additional Rust advisories and their fixtures
come from the shared meta-only assets. The script runs ``ast-grep scan --json``
and intersects all reported hits with
the lines the PR adds (``git diff -U0 base head``), so only newly added
findings warn. Existing unwrap hits are listed in ``docs/unwrap-backlog.md``.

WARN mode: findings print ``::warning::`` annotations and the script exits 0.
A blind gate (missing binary, unreadable diff) also warns instead of failing;
it must never turn a PR red. Only malformed arguments exit non-zero.
"""

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RULE_ID = "no-unwrap-request-paths"
BACKLOG_NAME = "docs/unwrap-backlog.md"
MAX_ANNOTATIONS = 10


def run_scan(ast_grep, all_rules=False):
    """Return enforced hits as (path, line) or (rule_id, path, line) tuples.

    Duplicates are kept: several findings can share one line (chained calls).
    Callers build a set for diff intersection.
    """
    try:
        proc = subprocess.run(
            [ast_grep, "scan", "--json"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=300,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return None, f"ast-grep scan did not run: {exc}"
    try:
        findings = json.loads(proc.stdout or "[]")
    except json.JSONDecodeError as exc:
        return None, f"ast-grep scan output is not JSON: {exc}"
    hits = []
    for finding in findings:
        if not all_rules and finding.get("ruleId", RULE_ID) != RULE_ID:
            continue
        line = (finding.get("range") or {}).get("start", {}).get("line")
        path = finding.get("file")
        if isinstance(line, int) and isinstance(path, str):
            rule_id = finding.get("ruleId", RULE_ID)
            hits.append((rule_id, path, line) if all_rules else (path, line))
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


def warn(message):
    print(f"::warning::{message}", flush=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-ref", default="origin/main")
    parser.add_argument("--head-ref", default="HEAD")
    parser.add_argument("--ast-grep", default="ast-grep")
    args = parser.parse_args(argv)

    hits, error = run_scan(args.ast_grep, all_rules=True)
    if hits is None:
        warn(f"ast-grep WARN gate is blind ({error}); treating as pass")
        return 0
    unwrap_hits = [hit for hit in hits if hit[0] == RULE_ID]
    print(f"unwrap request-path gate: {len(unwrap_hits)} enforced-scope hit(s)")
    print(f"ast-grep advisory gate: {len(hits) - len(unwrap_hits)} other rule hit(s)")

    added, error = added_lines(args.base_ref, args.head_ref)
    if added is None:
        warn(f"ast-grep WARN gate could not read the diff ({error}); totals only")
        return 0

    fresh = sorted(set(hit for hit in hits if (hit[1], hit[2]) in added))
    if not fresh:
        print("ast-grep WARN gate: no added findings in scope")
        return 0
    for rule_id, path, line in fresh[:MAX_ANNOTATIONS]:
        if rule_id == RULE_ID:
            warn(f"{path}:{line} adds .unwrap() in a non-test request path")
        else:
            warn(f"{path}:{line} matches ast-grep rule {rule_id}")
    if len(fresh) > MAX_ANNOTATIONS:
        warn(f"... and {len(fresh) - MAX_ANNOTATIONS} more new finding(s)")
    unwrap_fresh = [hit for hit in fresh if hit[0] == RULE_ID]
    if unwrap_fresh:
        warn(f"unwrap request-path gate: {len(unwrap_fresh)} added .unwrap() call(s) in scope "
             f"(WARN only; existing backlog in {BACKLOG_NAME})")
    warn(f"ast-grep WARN gate: {len(fresh)} new finding(s) (WARN only)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
