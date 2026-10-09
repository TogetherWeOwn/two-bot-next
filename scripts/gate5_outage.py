#!/usr/bin/env python3
"""Offline Gate 5 outage-start-to-recovery measurement: tick log (JSONL) -> summary (JSON).

Tooling for the B2 soak's Gate 5 rule
(docs/soak-entry-gates.md "Gate 5 — qualifying-clock rules"): every actual
outage must recover in **under 60 seconds**, measured from the actual outage
start to verified recovery. This script never touches staging or production;
it only reads a committed or piped JSONL fixture and prints the summary.

Log format (one JSON object per line, in time order):

    {"ts": "2026-10-01T00:00:01Z", "event": "readyz_ok"}
    {"ts": "2026-10-01T00:00:02Z", "event": "readyz_fail", "status": 503}
    {"ts": "2026-10-01T00:00:03Z", "event": "tick_missed"}
    {"ts": "2026-10-01T00:01:00Z", "event": "readyz_ok"}

Outage start is the first ``readyz_fail`` or ``tick_missed`` while healthy;
recovery is the first ``readyz_ok`` after that. A repeated failure inside an
open window does not move the start: the window always spans first failure
to first recovery. Each recovered window carries ``outage_seconds`` rounded
to 0.1 s, reusing the ``outage_seconds`` convention of
``scripts/staging_container_drill.py`` (fence-to-first-ready). Any
unrecognised ``event`` value or malformed line is preserved as an UNKNOWN
interval, never silently dropped.

A window still open when the log ends stays explicitly UNKNOWN
(``status: "unknown"``, ``end: null``, ``outage_seconds: null``): like
``scripts/soak_evidence.py``, the outage length is unbounded there, so it is
never scored as zero.

Verdict: PASS needs complete, ordered evidence and every recovered outage under
budget. A measured outage at or over budget is NEEDS WORK. Anything missing is
NOT VERIFIED, never PASS: an empty log, a silent gap of the budget or more
between records, an out-of-order record, an unknown or malformed record, a log
that began mid-outage, an outage still open at the end, and a recovery a budget
or more after the last healthy sample (the outage could have started anywhere
after that sample).

Usage:

    python3 scripts/gate5_outage.py path/to/tick-log.jsonl
"""

import argparse
import json
import sys
from datetime import datetime, timezone

SCHEMA_VERSION = 1

# Gate 5 acceptance: recovery strictly under 60 s from outage start.
OUTAGE_BUDGET_S = 60
# A silence this long could hide an over-budget outage.
SILENT_GAP_LIMIT_S = OUTAGE_BUDGET_S

# Recovery signal; everything else that opens a window is a start signal.
RECOVERY_EVENT = "readyz_ok"
OUTAGE_START_EVENTS = ("readyz_fail", "tick_missed")


def parse_ts(value):
    """Parse an ISO-8601 UTC timestamp; raise ValueError when absent/garbled."""
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"missing ts: {value!r}")
    text = value.strip().replace("Z", "+00:00")
    moment = datetime.fromisoformat(text)
    if moment.tzinfo is None:
        moment = moment.replace(tzinfo=timezone.utc)
    return moment.astimezone(timezone.utc)


def summarize(lines):
    """Fold JSONL tick-log lines into the Gate 5 outage summary dict."""
    readyz_ok = readyz_fail = tick_missed = 0
    outage_windows = []
    unknowns = []
    first_ts = last_ts = None
    last_healthy_ts = None  # the outage cannot have started before this sample
    outage_start = None  # ts of the first failure of the currently open window

    def note_unknown(start, end, reason):
        unknowns.append({
            "start": start.isoformat() if start else None,
            "end": end.isoformat() if end else None,
            "reason": reason,
        })

    for lineno, raw in enumerate(lines, 1):
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
        else:
            gap = (ts - last_ts).total_seconds()
            if gap < 0:
                note_unknown(ts, ts, f"out-of-order line {lineno}: before previous record")
                continue
            if gap >= SILENT_GAP_LIMIT_S:
                note_unknown(last_ts, ts, f"silent gap of {gap:.1f} s before line {lineno}")
        last_ts = ts
        event = record.get("event")

        if event == RECOVERY_EVENT:
            readyz_ok += 1
            if outage_start is not None:
                measured = (ts - outage_start).total_seconds()
                if last_healthy_ts is None:
                    note_unknown(outage_start, ts,
                                 "log began mid-outage: actual start not observed")
                elif measured < OUTAGE_BUDGET_S <= (ts - last_healthy_ts).total_seconds():
                    note_unknown(last_healthy_ts, ts,
                                 "outage start not pinned: last healthy sample too far back")
                else:
                    outage_windows.append({
                        "start": outage_start.isoformat(),
                        "end": ts.isoformat(),
                        "outage_seconds": round(measured, 1),
                        "status": "recovered",
                    })
                outage_start = None
            last_healthy_ts = ts
        elif event in OUTAGE_START_EVENTS:
            if event == "readyz_fail":
                readyz_fail += 1
            else:
                tick_missed += 1
            if outage_start is None:
                outage_start = ts
        else:
            note_unknown(ts, ts, f"unrecognised event {event!r}")

    if outage_start is not None:
        # Log ends while still down: the outage length is unbounded, so the
        # window stays explicitly UNKNOWN instead of being scored as zero.
        outage_windows.append({
            "start": outage_start.isoformat(),
            "end": None,
            "outage_seconds": None,
            "status": "unknown",
        })
    if not (readyz_ok or readyz_fail or tick_missed):
        note_unknown(None, None, "no readiness events in log")

    recovered = [window["outage_seconds"] for window in outage_windows
                 if window["status"] == "recovered"]
    max_outage = max(recovered) if recovered else None
    unknown_open = any(window["status"] == "unknown" for window in outage_windows)
    over_budget = any(seconds >= OUTAGE_BUDGET_S for seconds in recovered)
    if over_budget:
        verdict = "NEEDS WORK"
    elif unknowns or unknown_open:
        verdict = "NOT VERIFIED"
    else:
        verdict = "PASS"
    return {
        "schema_version": SCHEMA_VERSION,
        "window": {
            "start": first_ts.isoformat() if first_ts else None,
            "end": last_ts.isoformat() if last_ts else None,
        },
        "readyz_ok": readyz_ok,
        "readyz_fail": readyz_fail,
        "tick_missed": tick_missed,
        "outage_windows": outage_windows,
        "unknown_intervals": unknowns,
        "max_outage_seconds": max_outage,
        "outage_budget_s": OUTAGE_BUDGET_S,
        "verdict": verdict,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", help="readiness tick log in JSONL format")
    args = parser.parse_args(argv)
    with open(args.log, encoding="utf-8") as handle:
        summary = summarize(handle)
    json.dump(summary, sys.stdout, indent=2)
    sys.stdout.write("\n")
    if summary["verdict"] != "PASS":
        # Nonzero so CI and the live-run wrapper fail closed; the JSON on
        # stdout still carries the full evidence either way.
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
