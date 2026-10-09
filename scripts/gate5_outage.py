#!/usr/bin/env python3
"""Offline Gate 5 outage measurement over a readiness tick log (JSONL).

Records must arrive at least every MAX_GAP_S. An outage runs from its first
failure to the third consecutive readyz_ok, the record that verifies recovery,
and its upper-bound start is the last healthy record before it. A failure after
a verified recovery opens a new outage. PASS needs the declared acceptance
interval covered, no gap over MAX_GAP_S, and every upper bound under
OUTAGE_BUDGET_S; anything else is NEEDS WORK. See docs/soak-entry-gates.md, Gate 5.
"""

import argparse
import json
import re
import sys
from datetime import datetime, timezone

SCHEMA_VERSION = 1

# Gate 5 acceptance: recovery strictly under 60 s from outage start.
OUTAGE_BUDGET_S = 60

# Staging /readyz poll cadence (POLL_SECONDS in scripts/staging_container_drill.py).
TICK_CADENCE_S = 5
MAX_GAP_S = 2 * TICK_CADENCE_S

# First ready probe plus CONFIRM_PROBES = 2 (scripts/staging_container_drill.py).
CONFIRM_OK = 3

RECOVERY_EVENT = "readyz_ok"
FAILURE_EVENT = "readyz_fail"
OUTAGE_START_EVENTS = (FAILURE_EVENT, "tick_missed")

TIME_OF_DAY = re.compile(r"[T ]\d{2}:\d{2}")


def parse_ts(value):
    """Parse an ISO-8601 UTC timestamp that includes a time of day."""
    if not isinstance(value, str) or not TIME_OF_DAY.search(value):
        raise ValueError(f"missing ts or time of day: {value!r}")
    text = value.strip().replace("Z", "+00:00")
    moment = datetime.fromisoformat(text)
    if moment.tzinfo is None:
        moment = moment.replace(tzinfo=timezone.utc)
    try:
        return moment.astimezone(timezone.utc)
    except OverflowError as exc:
        raise ValueError(f"timestamp out of range: {value!r}") from exc


def outage_window(outage, end):
    """Window for an outage opened at (first failure, last healthy or None)."""
    first_failure, last_healthy = outage
    window = {
        "start": None,
        "last_healthy": None,
        "end": end.isoformat() if end is not None else None,
        "outage_seconds": None,
        "outage_seconds_max": None,
        "status": "unknown",
    }
    if last_healthy is not None:
        window["start"] = first_failure.isoformat()
        window["last_healthy"] = last_healthy.isoformat()
    if last_healthy is not None and end is not None:
        window["outage_seconds"] = round((end - first_failure).total_seconds(), 1)
        window["outage_seconds_max"] = round((end - last_healthy).total_seconds(), 1)
        window["status"] = "recovered"
    return window


def summarize(lines, expect_start, expect_end):
    """Fold JSONL tick-log lines into the Gate 5 outage summary dict."""
    readyz_ok = readyz_fail = tick_missed = 0
    outage_windows = []
    unknowns = []
    first_ts = last_ts = None
    last_healthy = None
    outage = None  # (first failure, last healthy before it) while open
    run_len = 0  # readyz_ok records since the open outage's last failure

    def note_unknown(start, end, reason):
        unknowns.append({
            "start": start.isoformat() if start else None,
            "end": end.isoformat() if end else None,
            "reason": reason,
        })

    for lineno, raw in enumerate(lines, 1):
        if isinstance(raw, bytes):
            try:
                raw = raw.decode("utf-8")
            except UnicodeDecodeError:
                note_unknown(None, None, f"malformed line {lineno}: not UTF-8")
                continue
        line = raw.strip()
        if not line:
            continue
        try:
            record = json.loads(line)
        except (ValueError, RecursionError):
            note_unknown(None, None, f"malformed line {lineno}: not JSON")
            continue
        if not isinstance(record, dict):
            note_unknown(None, None, f"malformed line {lineno}: not an object")
            continue
        try:
            ts = parse_ts(record.get("ts"))
        except ValueError:
            note_unknown(None, None, f"malformed line {lineno}: bad ts")
            continue
        if last_ts is None:
            first_ts = ts
        elif ts <= last_ts:
            note_unknown(last_ts, ts, f"out-of-order or duplicate ts at line {lineno}")
            continue
        else:
            gap = (ts - last_ts).total_seconds()
            if gap > MAX_GAP_S:
                note_unknown(last_ts, ts,
                             f"{gap:.1f} s without records exceeds {MAX_GAP_S} s")
        last_ts = ts
        event = record.get("event")
        status = record.get("status")
        if ((event == RECOVERY_EVENT and status not in (None, 200))
                or (event == FAILURE_EVENT and status == 200)):
            note_unknown(ts, ts, f"{event} contradicts status {status!r}")
            continue

        if event == RECOVERY_EVENT:
            readyz_ok += 1
            last_healthy = ts
            if outage is not None:
                run_len += 1
                if run_len == CONFIRM_OK:
                    outage_windows.append(outage_window(outage, ts))
                    outage = None
                    run_len = 0
        elif event in OUTAGE_START_EVENTS:
            if event == FAILURE_EVENT:
                readyz_fail += 1
            else:
                tick_missed += 1
            if outage is None:
                outage = (ts, last_healthy)
            run_len = 0
        else:
            note_unknown(ts, ts, f"unrecognised event {event!r}")

    if outage is not None:
        # Still down at log end: UNKNOWN, never scored as zero.
        outage_windows.append(outage_window(outage, None))
    if first_ts is not None:
        # Leading silence within MAX_GAP_S hides no budget-length outage; trailing silence can.
        if (first_ts - expect_start).total_seconds() > MAX_GAP_S:
            note_unknown(expect_start, first_ts, "no records from the declared start")
        if last_ts < expect_end:
            note_unknown(last_ts, expect_end, "no records to the declared end")

    recovered = [window for window in outage_windows if window["status"] == "recovered"]
    max_outage = max((window["outage_seconds_max"] for window in recovered), default=None)
    unknown_windows = any(window["status"] == "unknown" for window in outage_windows)
    possibly_over = any(window["outage_seconds_max"] >= OUTAGE_BUDGET_S
                        for window in recovered)
    passed = (first_ts is not None and not unknowns and not unknown_windows
              and not possibly_over)
    return {
        "schema_version": SCHEMA_VERSION,
        "expected": {"start": expect_start.isoformat(), "end": expect_end.isoformat()},
        "window": {
            "start": first_ts.isoformat() if first_ts is not None else None,
            "end": last_ts.isoformat() if last_ts is not None else None,
        },
        "readyz_ok": readyz_ok,
        "readyz_fail": readyz_fail,
        "tick_missed": tick_missed,
        "outage_windows": outage_windows,
        "unknown_intervals": unknowns,
        "max_outage_seconds": max_outage,
        "outage_budget_s": OUTAGE_BUDGET_S,
        "verdict": "PASS" if passed else "NEEDS WORK",
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", help="readiness tick log in JSONL format")
    parser.add_argument("--expect-start", required=True, type=parse_ts,
                        help="UTC start of the declared acceptance interval")
    parser.add_argument("--expect-end", required=True, type=parse_ts,
                        help="UTC end of the declared acceptance interval")
    args = parser.parse_args(argv)
    if args.expect_end <= args.expect_start:
        parser.error("--expect-end must be after --expect-start")
    with open(args.log, "rb") as handle:
        summary = summarize(handle, args.expect_start, args.expect_end)
    json.dump(summary, sys.stdout, indent=2)
    sys.stdout.write("\n")
    if summary["verdict"] != "PASS":
        # Nonzero so CI and the live-run wrapper fail closed; the JSON on
        # stdout still carries the full evidence either way.
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
