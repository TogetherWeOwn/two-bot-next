#!/usr/bin/env python3
"""Offline Gate 5 outage-start-to-recovery measurement: tick log (JSONL) -> summary (JSON).

Tooling for the B2 soak's Gate 5 rule
(docs/soak-entry-gates.md "Gate 5 — qualifying-clock rules"): every actual
outage must recover in **under 60 seconds**, measured from the actual outage
start to verified recovery. This script never touches staging or production;
it only reads a committed or piped JSONL fixture and prints the summary.

Log format (one JSON object per line, in time order, one record per readiness
tick, no more than MAX_GAP_S apart):

    {"ts": "2026-10-01T00:00:00Z", "event": "readyz_ok", "status": 200}
    {"ts": "2026-10-01T00:00:05Z", "event": "readyz_fail", "status": 503}
    {"ts": "2026-10-01T00:00:10Z", "event": "tick_missed"}
    {"ts": "2026-10-01T00:00:15Z", "event": "readyz_ok", "status": 200}
    {"ts": "2026-10-01T00:00:20Z", "event": "readyz_ok", "status": 200}
    {"ts": "2026-10-01T00:00:25Z", "event": "readyz_ok", "status": 200}

The actual outage starts after the last healthy record before the first
``readyz_fail`` or ``tick_missed``, and at or before that failure. Recovery is
the first of CONFIRM_OK consecutive ``readyz_ok`` records, matching the drill's
first ready probe plus its confirmation probes; a failure restarts the run. A
recovered window carries ``outage_seconds`` (first failure to recovery, a lower
bound) and ``outage_seconds_max`` (last healthy record to recovery, an upper
bound). A window with an unbounded start, or still open at the end of the log,
is UNKNOWN with null seconds, never scored as zero.

PASS needs the declared acceptance interval (--expect-start, --expect-end)
covered by records within MAX_GAP_S at each end, no UNKNOWN intervals or
windows, and every ``outage_seconds_max`` under budget. Anything else is
NEEDS WORK. Unrecognised events, malformed or non-UTF-8 lines, timestamps
without a time of day, and records that contradict their own status are kept
as UNKNOWN intervals, never silently dropped or trusted.

Usage:

    python3 scripts/gate5_outage.py --expect-start 2026-10-01T00:00:00Z \\
        --expect-end 2026-10-01T04:00:00Z path/to/tick-log.jsonl
"""

import argparse
import json
import re
import sys
from datetime import datetime, timezone

SCHEMA_VERSION = 1

# Gate 5 acceptance: recovery strictly under 60 s from outage start.
OUTAGE_BUDGET_S = 60

# No tick producer exists yet; this cadence matches the staging /readyz poll
# (POLL_SECONDS in scripts/staging_container_drill.py). Silence longer than two
# cadences is unobserved time, never healthy time.
TICK_CADENCE_S = 5
MAX_GAP_S = 2 * TICK_CADENCE_S

# The drill's first ready probe plus CONFIRM_PROBES = 2 agreeing probes.
CONFIRM_OK = 3

# Recovery signal; everything else that opens a window is a start signal.
RECOVERY_EVENT = "readyz_ok"
FAILURE_EVENT = "readyz_fail"
OUTAGE_START_EVENTS = (FAILURE_EVENT, "tick_missed")

TIME_OF_DAY = re.compile(r"[T ]\d{2}:\d{2}")


def parse_ts(value):
    """Parse an ISO-8601 UTC timestamp with a time of day; raise ValueError otherwise."""
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
    """Window for an outage opened at (first failure, last healthy record or None)."""
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
    last_healthy = None  # latest readyz_ok seen outside an outage
    outage = None  # (first failure, last healthy before it) while an outage is open
    run_start = None  # first readyz_ok of the confirmation run in progress
    run_len = 0

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
        except json.JSONDecodeError:
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
        elif ts < last_ts:
            note_unknown(last_ts, ts, f"out-of-order ts at line {lineno}")
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
            if outage is None:
                last_healthy = ts
            else:
                if run_len == 0:
                    run_start = ts
                run_len += 1
                if run_len == CONFIRM_OK:
                    outage_windows.append(outage_window(outage, run_start))
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
        # Log ends while still down: the outage length is unbounded, so the
        # window stays explicitly UNKNOWN instead of being scored as zero.
        outage_windows.append(outage_window(outage, None))
    if first_ts is not None:
        if (first_ts - expect_start).total_seconds() > MAX_GAP_S:
            note_unknown(expect_start, first_ts, "no records from the declared start")
        if (expect_end - last_ts).total_seconds() > MAX_GAP_S:
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
