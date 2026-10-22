#!/usr/bin/env python3
"""Compare same-workload pipeline measurements, independently of required PR CI.

Input limits: each report is read bounded before any JSON decode. A report
that is missing, not a regular file, or unreadable, over 1048576 bytes
(1 MiB), with a line over 65536 bytes (64 KiB, excluding its terminator), or
with more than 100000 lines is refused, as is a report that is not valid JSON
or repeats a JSON object key. Every refusal exits 2 with {"verdict":
"INVALID", "code": ..., "next_action": ...} on stdout plus an "INVALID
benchmark:" line on stderr; nothing echoes report content.

Codes: unreadable_input, input_too_large, line_too_long, too_many_records,
invalid_json, duplicate_keys, invalid_benchmark. A record is one line; JSON
reports are small single documents, so pretty-print a minified blob whose one
line would exceed the line cap.
"""

import argparse
import json
import math
import re
import sys
from pathlib import Path


METRICS = (
    ("peak_rss_mib",),
    ("handler_latency_us", "p50"),
    ("handler_latency_us", "p99"),
    ("db_round_trips_per_event",),
)

# Intake bounds, enforced on each report before any JSON decode. Each refusal
# carries a fixed code and next_action; nothing echoes report content.
MAX_INPUT_BYTES = 1024 * 1024
MAX_LINE_BYTES = 64 * 1024
MAX_RECORDS = 100_000

NEXT_ACTION = {
    "unreadable_input": "point the CLI at readable report files and retry",
    "input_too_large": "shrink the report below 1 MiB and retry",
    "line_too_long": "pretty-print lines longer than 64 KiB and retry",
    "too_many_records": "shrink the report below 100000 lines and retry",
    "invalid_json": "fix the JSON syntax and retry",
    "duplicate_keys": "remove the repeated JSON object key and retry",
    "invalid_benchmark": "fix the benchmark report fields and retry",
}

INTAKE_DETAIL = {
    "unreadable_input": "benchmark report is unreadable",
    "input_too_large": "benchmark report exceeds the 1048576-byte cap",
    "line_too_long": "benchmark report has a line over the 65536-byte cap",
    "too_many_records": "benchmark report has more than 100000 lines",
    "invalid_json": "benchmark report is not valid JSON",
    "duplicate_keys": "benchmark report repeats a JSON object key",
}


class IntakeRefused(Exception):
    """A report was refused before comparison; carries a fixed INVALID code."""

    def __init__(self, code):
        super().__init__(code)
        self.code = code


class DuplicateKeyError(ValueError):
    """A JSON object repeats a key; the message never carries key/value data."""


def _reject_duplicate_keys(pairs):
    """object_pairs_hook refusing repeats instead of last-wins.

    The json decoder calls this for every object at any depth, after
    escape decoding, so {"a": 1, "\\u0061": 2} counts as a repeat. The
    raised message is fixed vocabulary: it names no key, value or path.
    """
    seen = set()
    for key, _value in pairs:
        if key in seen:
            raise DuplicateKeyError("duplicate JSON object key")
        seen.add(key)
    return dict(pairs)


def _read_report(path):
    """Read one report bounded; refuse over-cap or unreadable input first.

    One bounded binary read witnesses EOF and enforces the byte, line and
    record budgets before any decode. Parse only that checked snapshot, even
    if a producer replaces or grows the report after the read.
    """
    if not path.is_file():
        raise IntakeRefused("unreadable_input")
    try:
        with path.open("rb") as handle:
            data = handle.read(MAX_INPUT_BYTES + 1)
    except OSError:
        raise IntakeRefused("unreadable_input") from None
    if len(data) > MAX_INPUT_BYTES:
        raise IntakeRefused("input_too_large")
    raw_lines = data.splitlines()
    if len(raw_lines) > MAX_RECORDS:
        raise IntakeRefused("too_many_records")
    if any(len(line) > MAX_LINE_BYTES for line in raw_lines):
        raise IntakeRefused("line_too_long")
    try:
        return data.decode("utf-8")
    except UnicodeDecodeError:
        raise IntakeRefused("invalid_json") from None


def _invalid(code, detail=None):
    """Exit-2 refusal: JSON verdict on stdout, fixed line on stderr."""
    print(f"INVALID benchmark: {detail or INTAKE_DETAIL[code]}", file=sys.stderr)
    json.dump({"verdict": "INVALID", "code": code,
               "next_action": NEXT_ACTION[code]}, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 2


def metric(report, path):
    value = report["metrics"]
    for key in path:
        value = value[key]
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"{'.'.join(path)} must be numeric")
    if not math.isfinite(value) or value < 0:
        raise ValueError(f"{'.'.join(path)} must be finite and nonnegative")
    return value


def validate(report):
    if report["schema_version"] != 1:
        raise ValueError("unsupported schema_version")
    if not isinstance(report["workload"], dict) or not report["workload"]:
        raise ValueError("missing workload")
    values = {path: metric(report, path) for path in METRICS}
    if values[("peak_rss_mib",)] <= 0:
        raise ValueError("RSS measurement unavailable")
    if values[("handler_latency_us", "p50")] <= 0:
        raise ValueError("latency measurement unavailable")
    if values[("handler_latency_us", "p99")] < values[("handler_latency_us", "p50")]:
        raise ValueError("p99 must be at least p50")
    return values


# CTO decision (TOG-11786): p99 swings 6-232 ms on the shared pool, so it is reported but never gates.
ADVISORY = {("handler_latency_us", "p99")}


def compare(baseline, actual, tolerance=0.25, max_rss_mib=200, advisories=None):
    if not math.isfinite(tolerance) or tolerance < 0:
        raise ValueError("tolerance must be finite and nonnegative")
    if not math.isfinite(max_rss_mib) or not 0 < max_rss_mib <= 256:
        raise ValueError("RSS limit must be positive and at most the 256 MiB lite ceiling")
    before, after = validate(baseline), validate(actual)
    if baseline["workload"] != actual["workload"]:
        raise ValueError("workload differs from baseline; do not compare unlike profiles")
    failures = []
    for path in METRICS:
        limit = before[path] * (1 + tolerance)
        if after[path] > limit:
            line = f"{'.'.join(path)}: {after[path]:.3f} > {limit:.3f} ({tolerance:.0%} tolerance)"
            if path in ADVISORY and advisories is not None:
                advisories.append(line)
            else:
                failures.append(line)
    if after[("peak_rss_mib",)] >= max_rss_mib:
        failures.append(f"peak_rss_mib: {after[('peak_rss_mib',)]:.3f} >= lite budget {max_rss_mib:.3f}")
    return failures


# Argparse-time bound checks, mirroring the timing_seconds precedent: a bad
# flag refuses before any file read. Messages reuse the exact compare()
# vocabulary so the CLI and library refusals stay one fixed text.
TOLERANCE_REFUSAL = "tolerance must be finite and nonnegative"
RSS_REFUSAL = "RSS limit must be positive and at most the 256 MiB lite ceiling"


def valid_tolerance(raw):
    """One finite, nonnegative argparse type for --tolerance."""
    try:
        value = float(raw)
    except (TypeError, ValueError):
        raise argparse.ArgumentTypeError(TOLERANCE_REFUSAL) from None
    if not math.isfinite(value) or value < 0:
        raise argparse.ArgumentTypeError(TOLERANCE_REFUSAL)
    return value


def valid_rss_limit(raw):
    """One finite, positive, lite-capped argparse type for --max-rss-mib."""
    try:
        value = float(raw)
    except (TypeError, ValueError):
        raise argparse.ArgumentTypeError(RSS_REFUSAL) from None
    if not math.isfinite(value) or not 0 < value <= 256:
        raise argparse.ArgumentTypeError(RSS_REFUSAL)
    return value


class BenchParser(argparse.ArgumentParser):
    """Feed -inf/-nan/scientific values to their type check.

    argparse reads a value starting with "-" as another flag unless it
    looks like a negative number, and the stdlib match covers only -1/-0.5
    spellings. Without this, `--tolerance -inf` dies as "expected one
    argument" instead of reaching the bound check and its fixed refusal.
    """

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        # Digit runs allow single underscores between digits, matching the
        # float() literal rules, so `-1_0` reaches its type check instead of
        # dying as "expected one argument".
        self._negative_number_matcher = re.compile(
            r"^-\d(?:_?\d)*$|^-(?:\d(?:_?\d)*)?\.\d(?:_?\d)*$|^-\d(?:_?\d)*\.$"
            r"|^-\d(?:_?\d)*(\.(?:\d(?:_?\d)*)?)?[eE][+-]?\d(?:_?\d)*$|^-(\.\d(?:_?\d)*)[eE][+-]?\d(?:_?\d)*$"
            r"|^-inf(inity)?$|^-nan$",
            re.IGNORECASE,
        )


def main():
    parser = BenchParser(description=__doc__)
    parser.add_argument("actual", type=Path)
    parser.add_argument("--baseline", type=Path, default=Path("docs/pipeline-benchmark-baseline.json"))
    parser.add_argument("--tolerance", type=valid_tolerance, default=0.25)
    parser.add_argument("--max-rss-mib", type=valid_rss_limit, default=200)
    args = parser.parse_args()
    try:
        baseline_text = _read_report(args.baseline)
        actual_text = _read_report(args.actual)
    except IntakeRefused as refused:
        return _invalid(refused.code)
    try:
        baseline = json.loads(baseline_text, object_pairs_hook=_reject_duplicate_keys)
        actual = json.loads(actual_text, object_pairs_hook=_reject_duplicate_keys)
    except DuplicateKeyError:
        return _invalid("duplicate_keys")
    except (ValueError, RecursionError):
        return _invalid("invalid_json")
    try:
        advisories = []
        failures = compare(baseline, actual, args.tolerance, args.max_rss_mib, advisories)
    except OverflowError:
        return _invalid("invalid_benchmark", "benchmark metric exceeds numeric range")
    except (ValueError, KeyError, TypeError) as error:
        return _invalid("invalid_benchmark", str(error))
    for line in advisories:
        print(f"ADVISORY (does not fail): {line}")
    if failures:
        print("NEEDS WORK\n" + "\n".join(failures))
        return 1
    print("PASS: same workload, metrics within tolerance and lite RSS budget")
    return 0


if __name__ == "__main__":
    sys.exit(main())
