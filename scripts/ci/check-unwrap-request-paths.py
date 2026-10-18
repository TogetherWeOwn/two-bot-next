#!/usr/bin/env python3
"""HARD gate: no new `.unwrap()` in request-path source files.

The request-path rules are defined by
``ast-grep-rules/no-unwrap-request-paths.yml`` (direct ``$X.unwrap()`` calls)
and ``ast-grep-rules/no-unwrap-request-paths-in-macros.yml`` (``unwrap``
inside macro token trees, which the base pattern cannot see because ast-grep
parses macro arguments as opaque ``token_tree``). Both share the request-path
file globs with test files/dirs ignored; inline ``#[cfg(test)]`` modules are
included. Six additional Rust advisories and their fixtures come from the
shared meta-only assets. The script runs ``ast-grep scan --json --warning``
and intersects all reported hits with
the lines the PR adds (``git diff -U0 base head``), so only newly added
findings fail. Existing unwrap hits are listed in ``docs/unwrap-backlog.md``.

HARD mode: a newly added `.unwrap()` in scope prints ``::error::``
annotations and the script exits 1. Findings from the six shared advisory
rules stay WARN-only (``::warning::``, exit still 0 when they are the only
fresh hits). A blind gate (missing binary, failed/empty scan, malformed
scanner output, or unreadable diff) fails closed instead of passing; only
malformed arguments exit non-zero for another reason.
"""

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RULE_ID = "no-unwrap-request-paths"
MACRO_RULE_ID = "no-unwrap-request-paths-in-macros"
ENFORCED_RULE_IDS = frozenset({RULE_ID, MACRO_RULE_ID})
BACKLOG_NAME = "docs/unwrap-backlog.md"
MAX_ANNOTATIONS = 10


def run_scan(ast_grep, all_rules=False):
    """Return enforced hits as (path, line) or (rule_id, path, line) tuples.

    Duplicates are kept: several findings can share one line (chained calls).
    Callers build a set for diff intersection.
    """
    try:
        proc = subprocess.run(
            [ast_grep, "scan", "--json", "--warning"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=300,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return None, f"ast-grep scan did not run: {exc}"
    if proc.returncode != 0:
        return None, f"ast-grep scan exited {proc.returncode}: {proc.stderr.strip()[:200]}"
    if not proc.stdout.strip():
        return None, "ast-grep scan output is empty"
    try:
        findings = json.loads(proc.stdout)
    except json.JSONDecodeError as exc:
        return None, f"ast-grep scan output is not JSON: {exc}"
    if not isinstance(findings, list):
        return None, "ast-grep scan output has an invalid shape"
    hits = []
    for finding in findings:
        if not isinstance(finding, dict):
            return None, "ast-grep scan contains an invalid finding"
        rule_id = finding.get("ruleId", RULE_ID)
        if not isinstance(rule_id, str) or not rule_id:
            return None, "ast-grep scan contains an invalid finding"
        location = finding.get("range")
        if not isinstance(location, dict):
            return None, "ast-grep scan contains an invalid finding location"
        start = location.get("start")
        if not isinstance(start, dict):
            return None, "ast-grep scan contains an invalid finding location"
        line = start.get("line")
        path = finding.get("file")
        if (not isinstance(path, str) or not path
                or type(line) is not int or line < 0):
            return None, "ast-grep scan contains an invalid finding"
        if not all_rules and rule_id not in ENFORCED_RULE_IDS:
            continue
        if rule_id in ENFORCED_RULE_IDS:
            end = location.get("end")
            if not isinstance(end, dict):
                return None, "ast-grep scan contains an invalid finding location"
            line = end.get("line")
            if type(line) is not int or line < start.get("line"):
                return None, "ast-grep scan contains an invalid finding location"
        line += 1
        hits.append((rule_id, path, line) if all_rules else (path, line))
    return hits, ""


def added_lines(base_ref, head_ref):
    """Added (file, new-line) pairs from a zero-context diff of Rust files."""
    try:
        proc = subprocess.run(
            ["git", "-C", str(ROOT), "-c", "core.quotepath=false", "diff",
             "-U0", "--no-color", "-M", f"{base_ref}", f"{head_ref}", "--", "*.rs"],
            capture_output=True,
            text=True,
            timeout=120,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return None, f"git diff did not run: {exc}"
    if proc.returncode != 0:
        return None, f"git diff exited {proc.returncode}: {proc.stderr.strip()[:200]}"
    added = set()
    path, new_line, in_hunk = None, 0, False
    for raw in proc.stdout.splitlines():
        if raw.startswith("diff --git "):
            path, in_hunk = None, False
            continue
        if not in_hunk and raw.startswith("+++ "):
            # Git terminates the +++ header with a single TAB delimiter when
            # the path contains a space (see check-patch-coverage.py). Strip
            # exactly one trailing TAB so spaced names match the scanner's
            # exact repository-relative filename. A literal tab would arrive
            # C-escaped, and quoted/malformed headers still fail closed below.
            header = raw[:-1] if raw.endswith("\t") else raw
            if header == "+++ /dev/null":
                path = None
            elif header.startswith("+++ b/") and header[len("+++ b/"):]:
                path = header[len("+++ b/"):]
            else:
                return None, "git diff contains an unrecognized file path header"
            continue
        match = re.match(r"@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@", raw)
        if match:
            new_line = int(match.group(1))
            in_hunk = True
            continue
        if path is None or not in_hunk:
            continue
        if raw.startswith("+"):
            added.add((path, new_line))
            new_line += 1
        elif raw.startswith("-"):
            continue
        elif raw == r"\ No newline at end of file":
            continue
        elif raw.startswith(" "):
            new_line += 1
        else:
            return None, "git diff contains an unrecognized hunk line"
    return added, ""


def warn(message):
    print(f"::warning::{message}", flush=True)


def error(message):
    print(f"::error::{message}", flush=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-ref", default="origin/main")
    parser.add_argument("--head-ref", default="HEAD")
    parser.add_argument("--ast-grep", default="ast-grep")
    args = parser.parse_args(argv)

    hits, scan_error = run_scan(args.ast_grep, all_rules=True)
    if hits is None:
        error(f"ast-grep HARD gate is blind ({scan_error}); failing closed")
        return 1
    unwrap_hits = [hit for hit in hits if hit[0] in ENFORCED_RULE_IDS]
    print(f"unwrap request-path gate: {len(unwrap_hits)} enforced-scope hit(s)")
    print(f"ast-grep advisory gate: {len(hits) - len(unwrap_hits)} other rule hit(s)")

    added, diff_error = added_lines(args.base_ref, args.head_ref)
    if added is None:
        error(f"ast-grep HARD gate could not read the diff ({diff_error}); failing closed")
        return 1

    fresh = sorted(set(hit for hit in hits if (hit[1], hit[2]) in added))
    if not fresh:
        print("ast-grep HARD gate: no added findings in scope")
        return 0
    unwrap_fresh = [hit for hit in fresh if hit[0] in ENFORCED_RULE_IDS]
    for rule_id, path, line in fresh[:MAX_ANNOTATIONS]:
        if rule_id in ENFORCED_RULE_IDS:
            error(f"{path}:{line} adds .unwrap() in a request-path source file "
                  "(inline test modules are included)")
        else:
            warn(f"{path}:{line} matches ast-grep rule {rule_id}")
    if len(fresh) > MAX_ANNOTATIONS:
        (error if unwrap_fresh else warn)(
            f"... and {len(fresh) - MAX_ANNOTATIONS} more new finding(s)")
    if unwrap_fresh:
        error(f"unwrap request-path gate: {len(unwrap_fresh)} added .unwrap() call(s) in scope "
              f"(existing backlog in {BACKLOG_NAME})")
        return 1
    warn(f"ast-grep advisory gate: {len(fresh)} new finding(s) (WARN only)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
