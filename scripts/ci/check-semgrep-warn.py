#!/usr/bin/env python3
"""HARD gate: Semgrep Rust rules fail on new findings.

Runs the vendored rule set ``.semgrep/rust-warn.yml`` (local file only, so
the gate is deterministic and needs no registry access) and intersects the
findings with the lines the PR adds (``git diff -U0 base head``). New
findings print ``::error::`` annotations and the script exits 1;
pre-existing ones are grandfathered in ``docs/quality-gates.md`` and stay
quiet.

HARD mode: a blind gate (missing binary, unreadable config or diff) fails
closed instead of passing. Only malformed arguments exit non-zero for
another reason.
"""

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CONFIG = ROOT / ".semgrep/rust-warn.yml"
MAX_ANNOTATIONS = 10


def run_semgrep(semgrep):
    """All findings as (rule_id, path, line) triples."""
    try:
        proc = subprocess.run(
            [semgrep, "scan", "--config", str(CONFIG), "--json", "--quiet"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=600,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return None, f"semgrep did not run: {exc}"
    if proc.returncode != 0:
        return None, f"semgrep exited {proc.returncode}: {proc.stderr.strip()[:200]}"
    if not proc.stdout.strip():
        return None, "semgrep output is empty"
    try:
        report = json.loads(proc.stdout)
    except json.JSONDecodeError as exc:
        return None, f"semgrep output is not JSON: {exc}"
    if not isinstance(report, dict) or not isinstance(report.get("results"), list):
        return None, "semgrep report has an invalid shape"
    if report.get("errors"):
        return None, "semgrep reported errors"
    findings = set()
    for result in report["results"]:
        if not isinstance(result, dict):
            return None, "semgrep report contains an invalid result"
        start = result.get("start")
        if not isinstance(start, dict):
            return None, "semgrep report contains an invalid result location"
        check_id = result.get("check_id")
        path = result.get("path")
        line = start.get("line")
        if (not isinstance(check_id, str) or not check_id
                or not isinstance(path, str) or not path
                or type(line) is not int or line < 1):
            return None, "semgrep report contains an invalid result"
        rule = check_id.split(".")[-1]
        findings.add((rule, path, line))
    return findings, ""


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


def error(message):
    print(f"::error::{message}", flush=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-ref", default="origin/main")
    parser.add_argument("--head-ref", default="HEAD")
    parser.add_argument("--semgrep", default="semgrep")
    args = parser.parse_args(argv)

    if not CONFIG.is_file():
        error(f"semgrep HARD gate is blind (missing {CONFIG.relative_to(ROOT)}); failing closed")
        return 1

    findings, scan_error = run_semgrep(args.semgrep)
    if findings is None:
        error(f"semgrep HARD gate is blind ({scan_error}); failing closed")
        return 1
    print(f"semgrep HARD gate: {len(findings)} finding(s)")

    added, diff_error = added_lines(args.base_ref, args.head_ref)
    if added is None:
        error(f"semgrep HARD gate could not read the diff ({diff_error}); failing closed")
        return 1

    fresh = sorted(f for f in findings if (f[1], f[2]) in added)
    if not fresh:
        print("semgrep HARD gate: no added lines match a rule")
        return 0
    for rule, path, line in fresh[:MAX_ANNOTATIONS]:
        error(f"{path}:{line} matches semgrep rule {rule}")
    if len(fresh) > MAX_ANNOTATIONS:
        error(f"... and {len(fresh) - MAX_ANNOTATIONS} more new finding(s)")
    error(f"semgrep HARD gate: {len(fresh)} new finding(s)")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
