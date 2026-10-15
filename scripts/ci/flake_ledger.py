#!/usr/bin/env python3
"""Deterministic JUnit-to-flake-ledger parser (SDLC 2.3 CI speed pilot).

Reads the JUnit XML produced by `cargo nextest run --profile ci`
(retries=1, see .config/nextest.toml) and appends one JSON object per
distinct test to a JSONL flake ledger. Pure stdlib, no network, no clock:
identical input bytes always produce identical output bytes, so the check
lane can backtest it against fixtures instead of trusting a CI run.

Flake rule (tool-agnostic, document-order based):
- Group <testcase> elements by (classname, name) in document order.
- attempts = number of elements in the group.
- The LAST element decides the outcome: a trailing failure/error means
  status fail/error; otherwise pass (a skips-only group stays skipped).
- flaky = attempts > 1 and the group is not uniformly one outcome, i.e. a
  retry changed the result (fail-then-pass) or results disagree.

Ledger row keys (stable order): suite, classname, name, status, flaky,
attempts, time_ms (summed across attempts, truncated to int milliseconds).

Exit codes: 0 on success even when tests failed (the ledger is evidence,
not a gate; the nextest step itself fails the advisory job). 2 when the
JUnit file is missing or not well-formed XML.
"""

import argparse
import json
import sys
import xml.etree.ElementTree as ET
from collections import OrderedDict
from pathlib import Path


def parse_time_ms(raw):
    """JUnit `time` is seconds as a float; truncate to whole milliseconds."""
    try:
        return int(float(raw or 0) * 1000)
    except (TypeError, ValueError):
        return 0


def case_outcome(case):
    """Map one <testcase> element to pass/fail/error/skipped."""
    for child in case:
        if child.tag == "failure":
            return "fail"
        if child.tag == "error":
            return "error"
    for child in case:
        if child.tag == "skipped":
            return "skipped"
    return "pass"


def parse_junit(path):
    """Return ledger rows for a JUnit file. Raises on missing/invalid input."""
    try:
        tree = ET.parse(str(path))
    except FileNotFoundError:
        raise SystemExit(f"flake ledger: junit file not found: {path}")
    except ET.ParseError as exc:
        raise SystemExit(f"flake ledger: invalid JUnit XML in {path}: {exc}")
    root = tree.getroot()
    if root.tag == "testsuites":
        suites = root.findall("testsuite")
    elif root.tag == "testsuite":
        suites = [root]
    else:
        raise SystemExit(f"flake ledger: unexpected root element <{root.tag}> in {path}")

    groups = OrderedDict()
    order = []
    for suite in suites:
        suite_name = suite.get("name", "")
        for case in suite.findall("testcase"):
            key = (suite_name, case.get("classname", ""), case.get("name", ""))
            if key not in groups:
                groups[key] = []
                order.append(key)
            groups[key].append(case)

    rows = []
    for suite_name, classname, name in sorted(order):
        cases = groups[(suite_name, classname, name)]
        outcomes = [case_outcome(case) for case in cases]
        attempts = len(cases)
        last = outcomes[-1]
        if last in ("fail", "error"):
            status = last
        elif last == "skipped" and all(o == "skipped" for o in outcomes):
            status = "skipped"
        else:
            status = "pass"
        flaky = attempts > 1 and (len(set(outcomes)) > 1 or last in ("fail", "error"))
        time_ms = sum(parse_time_ms(case.get("time")) for case in cases)
        rows.append(
            {
                "suite": suite_name,
                "classname": classname,
                "name": name,
                "status": status,
                "flaky": flaky,
                "attempts": attempts,
                "time_ms": time_ms,
            }
        )
    return rows


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--junit", type=Path, required=True, help="nextest JUnit XML input")
    parser.add_argument("--output", type=Path, required=True, help="flake ledger JSONL output")
    args = parser.parse_args(argv)
    rows = parse_junit(args.junit)
    with args.output.open("w", encoding="utf-8") as handle:
        for row in rows:
            handle.write(json.dumps(row, sort_keys=True) + "\n")
    flaky = sum(1 for row in rows if row["flaky"])
    failed = sum(1 for row in rows if row["status"] in ("fail", "error"))
    print(f"flake ledger: {len(rows)} tests, {failed} failed, {flaky} flaky -> {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
