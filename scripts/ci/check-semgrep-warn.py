#!/usr/bin/env python3
"""WARN gate: Semgrep Rust rules report, never fail.

Runs the vendored rule set ``.semgrep/rust-warn.yml`` (local file only, so
the gate is deterministic and needs no registry access) and intersects the
findings with the lines the PR adds (``git diff -U0 base head``). New
findings print ``::warning::`` annotations; pre-existing ones are listed in
``docs/quality-gates.md`` and stay quiet.

WARN mode: findings print warnings and the script exits 0. A blind gate
(missing binary, unreadable config or diff) also warns instead of failing;
it must never turn a PR red. Only malformed arguments exit non-zero.
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
    try:
        report = json.loads(proc.stdout or "{}")
    except json.JSONDecodeError as exc:
        return None, f"semgrep output is not JSON: {exc}"
    findings = set()
    for result in report.get("results", []):
        rule = result.get("check_id", "").split(".")[-1]
        path = result.get("path")
        line = (result.get("start") or {}).get("line")
        if rule and isinstance(path, str) and isinstance(line, int):
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


def warn(message):
    print(f"::warning::{message}", flush=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-ref", default="origin/main")
    parser.add_argument("--head-ref", default="HEAD")
    parser.add_argument("--semgrep", default="semgrep")
    args = parser.parse_args(argv)

    if not CONFIG.is_file():
        warn(f"semgrep WARN gate is blind (missing {CONFIG.relative_to(ROOT)}); treating as pass")
        return 0

    findings, error = run_semgrep(args.semgrep)
    if findings is None:
        warn(f"semgrep WARN gate is blind ({error}); treating as pass")
        return 0
    print(f"semgrep WARN gate: {len(findings)} finding(s)")

    added, error = added_lines(args.base_ref, args.head_ref)
    if added is None:
        warn(f"semgrep WARN gate could not read the diff ({error}); totals only")
        return 0

    fresh = sorted(f for f in findings if (f[1], f[2]) in added)
    if not fresh:
        print("semgrep WARN gate: no added lines match a rule")
        return 0
    for rule, path, line in fresh[:MAX_ANNOTATIONS]:
        warn(f"{path}:{line} matches semgrep rule {rule}")
    if len(fresh) > MAX_ANNOTATIONS:
        warn(f"... and {len(fresh) - MAX_ANNOTATIONS} more new finding(s)")
    warn(f"semgrep WARN gate: {len(fresh)} new finding(s) (WARN only)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
