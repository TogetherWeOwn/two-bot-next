#!/usr/bin/env python3
"""Offline soak-evidence parser: gateway event log (JSONL) -> soak summary (JSON).

Tooling for TOG-10955's live 7-day collection. This script never touches
staging or production; it only reads a committed or piped JSONL fixture and
prints the summary shape required by docs/staging-soak.md:

- connects / disconnects / resumes counts,
- missed-sequence windows (dispatch seq gaps within one session),
- UNKNOWN intervals, preserved verbatim, never silently dropped.

Log format (one JSON object per line):

    {"ts": "2026-09-20T00:00:01Z", "event": "connect", "session": "a1"}
    {"ts": "...", "event": "dispatch", "session": "a1", "seq": 42}
    {"ts": "...", "event": "disconnect", "session": "a1", "code": 4007}
    {"ts": "...", "event": "resume", "session": "a1", "seq": 42}
    {"ts": "...", "event": "unknown", "start": "...", "end": "...",
     "reason": "log gap: supervisor restart"}

A new session id resets the sequence baseline (Discord sequences are
per-session; see docs/gateway-recovery.md). A repeated or older seq in the
same session is a duplicate delivery, not a missed event. Any unrecognised
``event`` value or malformed line is preserved as an UNKNOWN interval so the
live run in TOG-10955 must explain it instead of inheriting a silent skip.

Usage:

    python3 scripts/soak_evidence.py scripts/fixtures/soak_evidence_sample.jsonl
"""

import argparse
import json
import sys
from datetime import datetime, timezone

SCHEMA_VERSION = 1

# Redeploy RESUME/IDENTIFY budget from docs/staging-soak.md acceptance.
REDEPLOY_GAP_BUDGET_S = 60

KNOWN_EVENTS = ("connect", "disconnect", "resume", "dispatch", "unknown")


def _unique_object(pairs):
    """object_pairs_hook refusing duplicate keys instead of last-wins."""
    seen = set()
    for key, _value in pairs:
        if key in seen:
            raise ValueError("duplicate field")
        seen.add(key)
    return dict(pairs)


# Well below CPython's default recursion limit (1000): any record deeper
# than this would raise RecursionError under recursive traversal/repr, so
# it is refused up front with an explicit stack instead of recursing.
MAX_RECORD_DEPTH = 100


def _too_deeply_nested(record, limit=MAX_RECORD_DEPTH):
    """Iterative depth check; True when nesting exceeds limit."""
    stack = [(record, 1)]
    while stack:
        obj, depth = stack.pop()
        if depth > limit:
            return True
        if isinstance(obj, dict):
            for value in obj.values():
                if isinstance(value, (dict, list)):
                    stack.append((value, depth + 1))
        elif isinstance(obj, list):
            for value in obj:
                if isinstance(value, (dict, list)):
                    stack.append((value, depth + 1))
    return False


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
    """Fold JSONL log lines into the soak-evidence summary dict."""
    connects = disconnects = resumes = dispatches = 0
    missed = []
    unknowns = []
    first_ts = last_ts = None
    last_seq = {}  # session -> highest committed dispatch seq
    down_since = None  # ts of latest disconnect awaiting reconnect
    gaps = []  # disconnect->connect/resume durations in seconds

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
            record = json.loads(line, object_pairs_hook=_unique_object)
        except RecursionError:
            note_unknown(None, None,
                         f"malformed line {lineno}: too deeply nested")
            continue
        except json.JSONDecodeError:
            note_unknown(None, None, f"malformed line {lineno}: not JSON")
            continue
        except ValueError as exc:
            if "duplicate field" in str(exc):
                note_unknown(None, None,
                             f"malformed line {lineno}: duplicate field")
                continue
            note_unknown(None, None, f"malformed line {lineno}: not JSON")
            continue
        if _too_deeply_nested(record):
            note_unknown(None, None,
                         f"malformed line {lineno}: too deeply nested")
            continue
        if not isinstance(record, dict):
            note_unknown(None, None, f"malformed line {lineno}: not an object")
            continue
        try:
            ts = parse_ts(record.get("ts"))
        except RecursionError:
            note_unknown(None, None,
                         f"malformed line {lineno}: too deeply nested")
            continue
        except ValueError:
            note_unknown(None, None, f"malformed line {lineno}: bad ts")
            continue
        first_ts = ts if first_ts is None else min(first_ts, ts)
        last_ts = ts if last_ts is None else max(last_ts, ts)
        try:
            event = record.get("event")
            session = record.get("session")

            if event == "connect":
                connects += 1
                last_seq.pop(session, None)
                if down_since is not None:
                    gaps.append((ts - down_since).total_seconds())
                    down_since = None
            elif event == "disconnect":
                disconnects += 1
                if down_since is None:
                    down_since = ts
            elif event == "resume":
                resumes += 1
                if down_since is not None:
                    gaps.append((ts - down_since).total_seconds())
                    down_since = None
                if isinstance(record.get("seq"), int):
                    prev = last_seq.get(session)
                    if prev is None or record["seq"] > prev:
                        last_seq[session] = record["seq"]
            elif event == "dispatch":
                seq = record.get("seq")
                if not isinstance(seq, int):
                    note_unknown(ts, ts, "dispatch without integer seq")
                    continue
                dispatches += 1
                prev = last_seq.get(session)
                if prev is None:
                    last_seq[session] = seq
                elif seq <= prev:
                    pass  # duplicate/redelivered dispatch, not a miss
                else:
                    if seq > prev + 1:
                        missed.append({
                            "session": session,
                            "expected_after": prev,
                            "received": seq,
                            "count": seq - prev - 1,
                            "at": ts.isoformat(),
                        })
                    last_seq[session] = seq
            elif event == "unknown":
                start = end = None
                try:
                    if record.get("start"):
                        start = parse_ts(record["start"])
                    if record.get("end"):
                        end = parse_ts(record["end"])
                except ValueError:
                    note_unknown(ts, ts, "unknown interval with bad start/end")
                    continue
                note_unknown(start or ts, end,
                             str(record.get("reason", "unspecified")))
            else:
                note_unknown(ts, ts, f"unrecognised event {event!r}")
        except RecursionError:
            note_unknown(ts, ts,
                         f"malformed line {lineno}: too deeply nested")
            continue

    if down_since is not None:
        # Log ends while disconnected: the outage length is unbounded, so it
        # stays UNKNOWN instead of being scored as zero.
        note_unknown(down_since, None, "log ends while disconnected")

    missed_events = sum(window["count"] for window in missed)
    max_gap = max(gaps) if gaps else None
    within_budget = (max_gap is None or max_gap < REDEPLOY_GAP_BUDGET_S)
    verdict = "PASS" if (missed_events == 0 and not unknowns
                          and within_budget) else "NEEDS WORK"
    return {
        "schema_version": SCHEMA_VERSION,
        "window": {
            "start": first_ts.isoformat() if first_ts else None,
            "end": last_ts.isoformat() if last_ts else None,
        },
        "connects": connects,
        "disconnects": disconnects,
        "resumes": resumes,
        "dispatches": dispatches,
        "missed_sequence_windows": missed,
        "missed_events": missed_events,
        "unknown_intervals": unknowns,
        "redeploy_gaps_s": gaps,
        "max_redeploy_gap_s": max_gap,
        "redeploy_gap_budget_s": REDEPLOY_GAP_BUDGET_S,
        "verdict": verdict,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", help="gateway event log in JSONL format")
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
