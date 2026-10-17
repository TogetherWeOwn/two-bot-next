#!/usr/bin/env python3
"""Deterministic JUnit-to-flake-ledger parser (SDLC 2.3 CI speed pilot).

Reads the JUnit XML produced by `cargo nextest run --profile ci`
(retries=1, see .config/nextest.toml) and appends one JSON object per
distinct test to a JSONL flake ledger. Pure stdlib, no network, no clock:
identical input bytes always produce identical output bytes, so the check
lane can backtest it against fixtures instead of trusting a CI run.

Flake rule (nextest-first, document-order based):
- Group <testcase> elements by (suite, classname, name) in document order.
- cargo-nextest writes ONE <testcase> per test and records failed attempts
  as child elements: a fail-then-pass test carries flakyFailure/flakyError
  children, and a test that fails every retry carries its terminal
  failure/error plus rerunFailure/rerunError children per exhausted retry.
- attempts = number of elements in the group PLUS the number of retry
  children across those elements (1 testcase + 1 flakyFailure = 2 attempts;
  2 repeated testcases from merged JUnit = 2 attempts).
- The LAST element decides the outcome: a trailing failure/error means
  status fail/error; otherwise pass (a skips-only group stays skipped).
  Retry children never decide the outcome: flakyFailure means the final
  attempt passed.
- flaky = any flakyFailure/flakyError child (fail-then-pass), or, for
  merged non-nextest JUnit only, repeated elements whose outcomes
  disagree. Exhausted retries on a final failure stay flaky=false: the
  test failed on every attempt, and attempts already records the retries.

Ledger row keys (stable order): suite, classname, name, status, flaky,
attempts, time_ms (summed across attempts, truncated to int milliseconds).

Exit codes: 0 on success even when tests failed (the ledger is evidence,
not a gate; the nextest step itself fails the advisory job). 2 when the
JUnit file is missing or not well-formed XML.
"""

import argparse
import json
import math
import sys
import xml.etree.ElementTree as ET
from collections import OrderedDict
from pathlib import Path


def parse_time_ms(raw):
    """JUnit `time` is seconds as a float; truncate to whole milliseconds.

    Unparsable, non-finite, or overflowing values contribute 0 ms instead
    of aborting the ledger: float() accepts inf/nan spellings, and finite
    seconds can still overflow to inf when scaled to milliseconds, both
    of which int() rejects (OverflowError/ValueError).
    """
    try:
        milliseconds = float(raw or 0) * 1000
    except (TypeError, ValueError, OverflowError):
        return 0
    if not math.isfinite(milliseconds):
        return 0
    return int(milliseconds)


# Child tags cargo-nextest 0.9.99 uses for failed attempts. flaky* means a
# failed attempt on a test that eventually passed; rerun* means an
# exhausted retry on a test whose terminal failure/error is also present.
RETRY_TAGS = frozenset({"flakyFailure", "flakyError", "rerunFailure", "rerunError"})
FLAKY_TAGS = frozenset({"flakyFailure", "flakyError"})


def case_outcome(case):
    """Map one <testcase> element to pass/fail/error/skipped.

    Retry children (flaky*/rerun*) never decide the outcome: a testcase
    carrying only flakyFailure children passed its final attempt.
    """
    for child in case:
        if child.tag == "failure":
            return "fail"
        if child.tag == "error":
            return "error"
    for child in case:
        if child.tag == "skipped":
            return "skipped"
    return "pass"


def retry_children(case):
    """Failed-attempt children: one per exhausted retry (nextest shape)."""
    return [child for child in case if child.tag in RETRY_TAGS]


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
        retries = [child for case in cases for child in retry_children(case)]
        attempts = len(cases) + len(retries)
        last = outcomes[-1]
        if last in ("fail", "error"):
            status = last
        elif last == "skipped" and all(o == "skipped" for o in outcomes):
            status = "skipped"
        else:
            status = "pass"
        flaky = (
            any(child.tag in FLAKY_TAGS for child in retries)
            # Merged non-nextest JUnit only: repeated elements whose outcomes
            # disagree mean a retry changed the result. Repeated identical
            # failures are deterministic, not flakes.
            or (len(cases) > 1 and len(set(outcomes)) > 1)
        )
        time_ms = sum(
            parse_time_ms(case.get("time"))
            + sum(parse_time_ms(child.get("time")) for child in retry_children(case))
            for case in cases
        )
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
