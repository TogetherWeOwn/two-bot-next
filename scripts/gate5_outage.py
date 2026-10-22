#!/usr/bin/env python3
"""Offline Gate 5 outage-start-to-recovery measurement: tick log (JSONL) -> summary (JSON).

Tooling for the B2 soak's Gate 5 rule
(docs/soak-entry-gates.md "Gate 5 — qualifying-clock rules"): every actual
outage must recover in **under 60 seconds**, measured from the actual outage
start to verified recovery. This script never touches staging or production;
it only reads a committed JSONL fixture and prints the summary.

Log format (one JSON object per line, in time order, UTC timestamps with an offset):

    {"ts": "2026-10-01T00:00:01Z", "event": "readyz_ok"}
    {"ts": "2026-10-01T00:00:02Z", "event": "readyz_fail", "status": 503}
    {"ts": "2026-10-01T00:00:03Z", "event": "tick_missed"}
    {"ts": "2026-10-01T00:01:00Z", "event": "readyz_ok"}

Outage start is the first ``readyz_fail`` or ``tick_missed`` while healthy;
recovery is the first ``readyz_ok`` after that. A repeated failure inside an
open window does not move the start: the window always spans first failure
to first recovery. Each recovered window carries ``outage_seconds`` floored to
0.1 s for display, so a displayed value reaches the budget only when the outage
does; verdicts use the unfloored duration, reusing the ``outage_seconds``
convention of ``scripts/staging_container_drill.py`` (fence-to-first-ready).
A line that is not valid UTF-8, not JSON, or lacks a UTC offset is preserved as
an UNKNOWN interval, never silently dropped.

A window still open when the log ends stays explicitly UNKNOWN
(``status: "unknown"``, ``end: null``, ``outage_seconds: null``): like
``scripts/soak_evidence.py``, the outage length is unbounded there, so it is
never scored as zero. The window still carries ``outage_seconds_min``, the
floored time from the first observed failure to the last record: a lower
bound, since recovery only counts at a ``readyz_ok``. Once that lower bound
reaches the budget, the Gate 5 rule has definitely failed.

Verdict: NEEDS WORK when a recovered outage is at or over the budget, or when
an outage still open at the end already reaches the budget. PASS needs
the acceptance interval and a log that covers it: the first record within a
budget of the interval start, the last record at or after the interval end, and
no silent gap longer than a budget. Missing evidence is NOT VERIFIED, never PASS:
no interval, an empty log, a silent gap, an out-of-order or unknown record, a
log that began mid-outage (NEEDS WORK instead when that outage already reaches
the budget), an outage still open at the end (NEEDS WORK instead when it
already reaches the budget), a start that cannot be pinned
within the budget (the last healthy sample is more than a budget before
recovery), a readyz_ok with a non-200 status or a readyz_fail with status 200,
and an outage that recurs within a budget of its recovery.

Intake limits: the log read is bounded before any JSON decode. A path that is
missing, not a regular file, or unreadable, a log over 1048576 bytes (1 MiB),
a line over 65536 bytes (64 KiB, excluding its terminator), or a log over
100000 lines is refused with ``{"verdict": "INVALID", "code": ...,
"next_action": ...}`` on stdout and exit 2, never a traceback. Stored
unknown intervals truncate past 1000 entries with ``"truncated": true``; the
verdict still sees every record.

Usage:

    python3 scripts/gate5_outage.py tick-log.jsonl --interval-start START --interval-end END

START and END are the UTC acceptance-interval bounds from the approved soak record.
"""

import argparse
import json
import math
import os
import sys
from datetime import datetime, timezone

SCHEMA_VERSION = 1

# Gate 5 acceptance: recovery strictly under 60 s from outage start.
OUTAGE_BUDGET_S = 60
# Silences up to the budget cannot hide an over-budget outage.
SILENT_GAP_LIMIT_S = OUTAGE_BUDGET_S

# Recovery signal; everything else that opens a window is a start signal.
RECOVERY_EVENT = "readyz_ok"
OUTAGE_START_EVENTS = ("readyz_fail", "tick_missed")

# Intake bounds, enforced during the log read before any JSON decode. Each
# refusal carries a fixed code and next_action; nothing echoes the log.
MAX_INPUT_BYTES = 1024 * 1024
MAX_LINE_BYTES = 64 * 1024
MAX_RECORDS = 100_000

# Stored-evidence bound. The parser keeps at most this many unknown
# intervals; the verdict still sees every record (any unknown keeps the log
# from passing), and a truncated summary carries "truncated": true. Valid
# logs never truncate, so their summaries are byte-identical to before.
MAX_UNKNOWN_INTERVALS = 1000

NEXT_ACTION = {
    "unreadable_input": "point the CLI at a readable log file and retry",
    "input_too_large": "shrink the log below 1 MiB and retry",
    "line_too_long": "split lines longer than 64 KiB and retry",
    "too_many_records": "split the log into fewer than 100000 records and retry",
}

HELP_EPILOG = """intake limits (refused with {"verdict": "INVALID", ...} on stdout, exit 2):
  unreadable_input  path is missing, not a regular file, or unreadable
  input_too_large   log over 1048576 bytes (1 MiB)
  line_too_long     any line over 65536 bytes (64 KiB, excluding its terminator)
  too_many_records  log over 100000 lines
stored unknown intervals truncate with "truncated": true past 1000 entries;
the verdict still sees every record."""


class IntakeRefused(Exception):
    """The log was refused before parsing; carries a fixed INVALID code."""

    def __init__(self, code):
        super().__init__(code)
        self.code = code


def _read_log_lines(path):
    """Read the log as bounded text lines, refusing over-cap input first.

    The byte, line-length and record budgets are enforced while streaming,
    so a hostile log is refused before any line reaches the JSON decoder.
    Decoding matches the previous byte-iteration behavior exactly (BOM
    tolerated, undecodable lines kept as UNKNOWN), never a crash.
    """
    if not os.path.isfile(path):
        raise IntakeRefused("unreadable_input")
    try:
        handle = open(path, "rb")
    except OSError:
        raise IntakeRefused("unreadable_input") from None
    lines = []
    total = 0
    with handle:
        # Two terminator bytes allow an exact-cap CRLF line. Never let a
        # missing newline turn the line cap into an unbounded allocation.
        while raw := handle.readline(MAX_LINE_BYTES + 2):
            total += len(raw)
            if total > MAX_INPUT_BYTES:
                raise IntakeRefused("input_too_large")
            content = raw[:-1] if raw.endswith(b"\n") else raw
            if content.endswith(b"\r"):
                content = content[:-1]
            if len(content) > MAX_LINE_BYTES:
                raise IntakeRefused("line_too_long")
            if len(lines) + 1 > MAX_RECORDS:
                raise IntakeRefused("too_many_records")
            try:
                lines.append(content.decode("utf-8-sig"))
            except UnicodeDecodeError:
                lines.append("\x00")  # not JSON, so the line is kept as UNKNOWN
    return lines


def parse_ts(value):
    """Parse an ISO-8601 timestamp with a UTC offset; raise ValueError otherwise."""
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"missing ts: {value!r}")
    text = value.strip().replace("Z", "+00:00")
    moment = datetime.fromisoformat(text)
    if moment.tzinfo is None:
        raise ValueError(f"ts lacks a UTC offset: {value!r}")
    return moment.astimezone(timezone.utc)


def _reject_duplicate_keys(pairs):
    keys = [key for key, _ in pairs]
    if len(keys) != len(set(keys)):
        raise ValueError("duplicate key in record")
    return dict(pairs)


def _display_seconds(seconds):
    return math.floor(round(seconds, 6) * 10) / 10


def summarize(lines, interval_start=None, interval_end=None):
    """Fold JSONL tick-log lines into the Gate 5 outage summary dict."""
    readyz_ok = readyz_fail = tick_missed = 0
    outage_windows = []
    unknowns = []
    first_ts = last_ts = None
    last_healthy_ts = None  # the outage cannot have started before this sample
    last_recovery_ts = None  # an outage recurring within a budget of this recovery
    outage_start = None  # ts of the first failure of the currently open window
    breach = False
    interval_ok = (interval_start is not None and interval_end is not None
                   and interval_end > interval_start)
    truncated = False

    def note_unknown(start, end, reason):
        nonlocal truncated
        if len(unknowns) >= MAX_UNKNOWN_INTERVALS:
            truncated = True
            return
        unknowns.append({
            "start": start.isoformat() if start else None,
            "end": end.isoformat() if end else None,
            "reason": reason,
        })

    if not interval_ok:
        note_unknown(None, None, "acceptance interval missing or not after its start")

    for lineno, raw in enumerate(lines, 1):
        line = raw.strip()
        if not line:
            continue
        try:
            record = json.loads(line, object_pairs_hook=_reject_duplicate_keys)
        except (ValueError, RecursionError):
            note_unknown(None, None, f"malformed line {lineno}: not JSON or repeats a key")
            continue
        if not isinstance(record, dict):
            note_unknown(None, None, f"malformed line {lineno}: not an object")
            continue
        try:
            ts = parse_ts(record.get("ts"))
        except (ValueError, OverflowError) as exc:
            note_unknown(None, None, f"malformed line {lineno}: bad ts ({exc})")
            continue
        if last_ts is None:
            first_ts = ts
        else:
            gap = (ts - last_ts).total_seconds()
            if gap < 0:
                note_unknown(ts, ts, f"out-of-order line {lineno}: before previous record")
                continue
            if gap > SILENT_GAP_LIMIT_S:
                note_unknown(last_ts, ts, f"silent gap of {gap:.1f} s before line {lineno}")
        last_ts = ts
        event = record.get("event")

        if event == RECOVERY_EVENT and record.get("status", 200) != 200:
            note_unknown(ts, ts, f"readyz_ok with status {record.get('status')!r}")
            continue
        if event == "readyz_fail" and record.get("status") == 200:
            note_unknown(ts, ts, "readyz_fail with status 200")
            continue
        if event == RECOVERY_EVENT:
            readyz_ok += 1
            if outage_start is not None:
                measured = (ts - outage_start).total_seconds()
                pinned = (last_healthy_ts is not None
                          and (ts - last_healthy_ts).total_seconds() <= OUTAGE_BUDGET_S)
                if measured >= OUTAGE_BUDGET_S or pinned:
                    breach = breach or measured >= OUTAGE_BUDGET_S
                    outage_windows.append({
                        "start": outage_start.isoformat(),
                        "end": ts.isoformat(),
                        "outage_seconds": _display_seconds(measured),
                        "status": "recovered",
                    })
                elif last_healthy_ts is None:
                    note_unknown(outage_start, ts,
                                 "no healthy sample before the first failure: start not observed")
                else:
                    note_unknown(last_healthy_ts, ts,
                                 "outage start not pinned: last healthy sample too far back")
                outage_start = None
                last_recovery_ts = ts
            last_healthy_ts = ts
        elif event in OUTAGE_START_EVENTS:
            if event == "readyz_fail":
                readyz_fail += 1
            else:
                tick_missed += 1
            if outage_start is None:
                if (last_recovery_ts is not None
                        and (ts - last_recovery_ts).total_seconds() < OUTAGE_BUDGET_S):
                    note_unknown(last_recovery_ts, ts,
                                 "outage recurred within a budget of its recovery")
                outage_start = ts
        else:
            note_unknown(ts, ts, f"unrecognised event {event!r}")

    if outage_start is not None:
        # Log ends while still down: the outage length is unbounded above, so
        # the window stays explicitly UNKNOWN instead of being scored as zero.
        # The time from the first observed failure to the last record is still
        # a lower bound (recovery only counts at a readyz_ok): once it already
        # reaches the budget, the Gate 5 rule has definitely failed.
        lower = (last_ts - outage_start).total_seconds()
        if lower >= OUTAGE_BUDGET_S:
            breach = True
        outage_windows.append({
            "start": outage_start.isoformat(),
            "end": None,
            "outage_seconds": None,
            "outage_seconds_min": _display_seconds(lower),
            "status": "unknown",
        })
    if not (readyz_ok or readyz_fail or tick_missed):
        note_unknown(None, None, "no readiness events in log")
    if interval_ok and first_ts is not None:
        if (first_ts - interval_start).total_seconds() > SILENT_GAP_LIMIT_S:
            note_unknown(interval_start, first_ts,
                         "log starts more than a budget after the interval start")
        if last_ts < interval_end:
            note_unknown(last_ts, interval_end, "log ends before the interval end")

    recovered = [window["outage_seconds"] for window in outage_windows
                 if window["status"] == "recovered"]
    max_outage = max(recovered) if recovered else None
    unknown_open = any(window["status"] == "unknown" for window in outage_windows)
    if breach:
        verdict = "NEEDS WORK"
    elif unknowns or unknown_open:
        verdict = "NOT VERIFIED"
    else:
        verdict = "PASS"
    summary = {
        "schema_version": SCHEMA_VERSION,
        "acceptance_interval": {
            "start": interval_start.isoformat() if interval_start else None,
            "end": interval_end.isoformat() if interval_end else None,
        },
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
    if truncated:
        summary["truncated"] = True
    return summary


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, epilog=HELP_EPILOG,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("log", help="readiness tick log in JSONL format")
    parser.add_argument("--interval-start", type=parse_ts,
                        help="acceptance interval start, UTC ISO-8601")
    parser.add_argument("--interval-end", type=parse_ts,
                        help="acceptance interval end, UTC ISO-8601")
    args = parser.parse_args(argv)
    try:
        lines = _read_log_lines(args.log)
    except IntakeRefused as refused:
        json.dump({"verdict": "INVALID", "code": refused.code,
                   "next_action": NEXT_ACTION[refused.code]},
                  sys.stdout, indent=2)
        sys.stdout.write("\n")
        return 2
    summary = summarize(lines, args.interval_start, args.interval_end)
    json.dump(summary, sys.stdout, indent=2)
    sys.stdout.write("\n")
    if summary["verdict"] != "PASS":
        # Nonzero so CI and the live-run wrapper fail closed; the JSON on
        # stdout still carries the full evidence either way.
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
