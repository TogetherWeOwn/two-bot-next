#!/usr/bin/env python3
"""HARD gate: Semgrep Rust rules fail on new findings.

Runs the vendored rule set ``.semgrep/rust-warn.yml`` (local file only, so
the gate is deterministic and needs no registry access) and intersects the
findings with the lines the PR adds (``git diff -U0 base head``). New
findings print ``::error::`` annotations and the script exits 1;
pre-existing ones are grandfathered in ``docs/quality-gates.md`` and stay
quiet.

HARD mode: a blind gate (missing binary, unreadable config or diff, or a scan
that reports a parse, timeout or config error) fails closed instead of
passing. Only malformed arguments exit non-zero for another reason.
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
EMPTY_EXPECT_RULES = frozenset({"rust-empty-expect", "rust-empty-expect-in-macros"})
# Only these scan diagnostics leave coverage intact, so they annotate the PR
# instead of failing it. Every other `errors` entry (timeouts, partial or
# failed parses, config errors, unknown types or levels) means a file was not
# fully scanned and blinds the gate.
NONFATAL_LEVELS = {"warn", "info"}
NONFATAL_TYPES = {"dependencyresolutionerror"}


def error_type_name(raw):
    """Semgrep emits a bare string for most error types and a ``[name,
    payload]`` list for those that carry data (e.g. ``PartialParsing``)."""
    if isinstance(raw, list) and raw and isinstance(raw[0], str):
        return raw[0]
    return raw if isinstance(raw, str) else None


def run_semgrep(semgrep):
    """All findings as (rule_id, path, line) triples."""
    try:
        proc = subprocess.run(
            [semgrep, "scan", "--config", str(CONFIG), "--json", "--quiet",
             "--timeout", "0"],
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
    scan_warnings = []
    errors = report.get("errors", [])
    if not isinstance(errors, list):
        return None, "semgrep report has an invalid errors field"
    for entry in errors:
        if not isinstance(entry, dict):
            return None, "semgrep report contains an invalid error entry"
        level = entry.get("level")
        error_type = error_type_name(entry.get("type"))
        path = entry.get("path")
        message = entry.get("message")
        if (not isinstance(level, str) or not isinstance(error_type, str)
                or (path is not None and not isinstance(path, str))
                or (message is not None and not isinstance(message, str))):
            return None, "semgrep report contains an invalid error entry"
        level = level.replace("\r", " ").replace("\n", " ").strip()
        error_type = error_type.replace("\r", " ").replace("\n", " ").strip()
        path = (path or "<unknown>").replace("\r", " ").replace("\n", " ").strip()
        message = (message or "<no message>").replace("\r", " ").replace("\n", " ").strip()
        detail = (f"level={level}, type={error_type}, path={path}: "
                  f"{message[:300]}")
        if level.lower() in NONFATAL_LEVELS and error_type.lower() in NONFATAL_TYPES:
            scan_warnings.append(detail)
            continue
        return None, f"semgrep reported a scan error ({detail})"
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
        if rule in EMPTY_EXPECT_RULES:
            end = result.get("end")
            if not isinstance(end, dict):
                return None, "semgrep report contains an invalid result location"
            line = end.get("line")
            if type(line) is not int or line < start.get("line"):
                return None, "semgrep report contains an invalid result location"
        findings.add((rule, path, line))
    base_lines = {(path, line) for (rule, path, line) in findings
                  if rule == "rust-empty-expect"}
    findings = {finding for finding in findings
                if finding[0] != "rust-empty-expect-in-macros"
                or (finding[1], finding[2]) not in base_lines}
    return findings, "\n".join(scan_warnings)


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


def error(message):
    print(f"::error::{message}", flush=True)


def warn(message):
    print(f"::warning::{message}", flush=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-ref", default="origin/main")
    parser.add_argument("--head-ref", default="HEAD")
    parser.add_argument("--semgrep", default="semgrep")
    args = parser.parse_args(argv)

    if not CONFIG.is_file():
        error(f"semgrep HARD gate is blind (missing {CONFIG.relative_to(ROOT)}); failing closed")
        return 1

    findings, scan_warnings = run_semgrep(args.semgrep)
    if findings is None:
        error(f"semgrep HARD gate is blind ({scan_warnings}); failing closed")
        return 1
    for warning in scan_warnings.splitlines():
        warn(f"semgrep scan warning ({warning})")
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
