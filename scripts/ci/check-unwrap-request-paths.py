#!/usr/bin/env python3
"""WARN gate: no new `.unwrap()` in non-test request paths.

Enforcement scope is the ast-grep rule
``ast-grep-rules/no-unwrap-request-paths.yml`` (request-path file globs with
test files/dirs ignored). The rule file is the single source of truth: this
script runs ``ast-grep scan --json`` and intersects the reported hits with
the lines the PR adds (``git diff -U0 base head``), so only newly added
unwraps warn. Existing hits are listed in ``docs/unwrap-backlog.md``.

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


def run_scan(ast_grep):
    """Enforced-scope hits as a list of (path, line) pairs.

    Duplicates are kept: several findings can share one line (chained
    calls), and the backlog counts findings, so the total must match it.
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
        if finding.get("ruleId", RULE_ID) != RULE_ID:
            continue
        line = (finding.get("range") or {}).get("start", {}).get("line")
        path = finding.get("file")
        if isinstance(line, int) and isinstance(path, str):
            hits.append((path, line))
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

    hits, error = run_scan(args.ast_grep)
    if hits is None:
        warn(f"unwrap request-path gate is blind ({error}); treating as pass")
        return 0
    print(f"unwrap request-path gate: {len(hits)} enforced-scope hit(s)")

    added, error = added_lines(args.base_ref, args.head_ref)
    if added is None:
        warn(f"unwrap request-path gate could not read the diff ({error}); totals only")
        return 0

    fresh = sorted(set(h for h in hits if h in added))
    if not fresh:
        print("unwrap request-path gate: no added .unwrap() in scope")
        return 0
    for path, line in fresh[:MAX_ANNOTATIONS]:
        warn(f"{path}:{line} adds .unwrap() in a non-test request path")
    if len(fresh) > MAX_ANNOTATIONS:
        warn(f"... and {len(fresh) - MAX_ANNOTATIONS} more added .unwrap() call(s)")
    warn(f"unwrap request-path gate: {len(fresh)} added .unwrap() call(s) in scope "
         f"(WARN only; existing backlog in {BACKLOG_NAME})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
