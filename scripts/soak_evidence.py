#!/usr/bin/env python3
"""Offline soak-evidence parser: gateway event log (JSONL) -> soak summary (JSON).

Offline parser only: it never touches staging or production, only reads a
committed JSONL fixture and prints the summary shape required by
docs/staging-soak.md. An offline PASS here verifies offline reconciliation
only; it makes no claim of live staging coverage or four-hour soak
acceptance:

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

Input records must be in nondecreasing timestamp order; equal timestamps
are accepted. A regressed event becomes UNKNOWN and is not applied to outage,
session, or sequence state. A new session id resets the sequence baseline
(Discord sequences are per-session; see docs/gateway-recovery.md). A repeated
or older seq in the same session is a duplicate delivery, not a missed event.
Any unrecognised or missing ``event`` value or malformed line is preserved as
an UNKNOWN interval without advancing the accepted timestamp window or changing
session, sequence or outage state. Later valid records are still processed.

Connect, resume and dispatch events require a nonempty string ``session``
identity. Any other, missing or empty identity is preserved as a fixed
UNKNOWN interval (``"<event> with invalid session"``) without touching
counters, sequence state or reconnect gaps, and later valid records are
still summarized. Disconnect and explicit unknown events keep their
existing semantics.

Dispatch ``seq`` and any supplied resume ``seq`` must be nonnegative integers;
booleans, nulls, strings, floats, arrays and objects are invalid. Only an omitted
resume ``seq`` is optional and leaves the sequence baseline unchanged. Invalid
sequences become fixed UNKNOWN evidence before counters, sequence baselines,
the accepted timestamp window or pending outages can change. UTC-normalization
overflow in event timestamps or explicit UNKNOWN endpoints likewise becomes
bounded malformed evidence, without echoing the rejected value or aborting the
remaining input. None of these rules changes the live soak acceptance budget.

Intake limits: the log read is bounded before any JSON decode. A path that is
missing, not a regular file, or unreadable, a log over 1048576 bytes (1 MiB),
a line over 65536 bytes (64 KiB, excluding its terminator), or a log over
100000 lines is refused with ``{"verdict": "INVALID", "code": ...,
"next_action": ...}`` on stdout and exit 2, never a traceback. Stored
evidence truncates past 1000 unknown intervals or 10000 redeploy gaps with
``"truncated": true``; verdicts and maxima still see every record.

Usage:

    python3 scripts/soak_evidence.py scripts/fixtures/soak_evidence_sample.jsonl
"""

import argparse
import json
import os
import re
import sys
from datetime import datetime, timezone

SCHEMA_VERSION = 1

# Redeploy RESUME/IDENTIFY budget from docs/staging-soak.md acceptance.
REDEPLOY_GAP_BUDGET_S = 60

KNOWN_EVENTS = ("connect", "disconnect", "resume", "dispatch", "unknown")

# Intake bounds, enforced during the log read before any JSON decode. Each
# refusal carries a fixed code and next_action; nothing echoes the log.
MAX_INPUT_BYTES = 1024 * 1024
MAX_LINE_BYTES = 64 * 1024
MAX_RECORDS = 100_000

# Stored-evidence bounds. The parser keeps at most this many entries; the
# verdict still sees every record (counts, maxima, and NEEDS WORK on any
# unknown), and a truncated summary carries "truncated": true. Valid logs
# never truncate, so their summaries are byte-identical to before.
MAX_UNKNOWN_INTERVALS = 1000
MAX_REDEPLOY_GAPS = 10_000

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
stored evidence truncates with "truncated": true past 1000 unknown intervals
or 10000 redeploy gaps; verdicts and maxima still see every record."""


class IntakeRefused(Exception):
    """The log was refused before parsing; carries a fixed INVALID code."""

    def __init__(self, code):
        super().__init__(code)
        self.code = code


def _read_log_lines(path):
    """Read the log as bounded text lines, refusing over-cap input first.

    The byte, line-length and record budgets are enforced while streaming,
    so a hostile log is refused before any line reaches the JSON decoder.
    A line that is not valid UTF-8 becomes a sentinel that the parser keeps
    as UNKNOWN, never a crash.
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
                lines.append(content.decode("utf-8"))
            except UnicodeDecodeError:
                lines.append("\x00")  # not JSON, so the line is kept as UNKNOWN
    return lines


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

# Strings are skipped whole, so brackets inside them never count. An
# unterminated string runs to the end, which keeps the scan linear.
JSON_TOKEN = re.compile(r'"(?:[^"\\]|\\.)*(?:"|\\?\Z)|[\[\]{}]', re.DOTALL)


def _json_depth_exceeds(text, limit):
    """Lexical nesting pre-scan, run before json.loads.

    The decoder's own RecursionError is no longer a reliable bound: newer
    CPython sizes it from the real C stack and decodes 10,000 levels.
    """
    depth = 0
    for match in JSON_TOKEN.finditer(text):
        token = match.group()
        if token in "[{":
            depth += 1
            if depth > limit:
                return True
        elif token in "]}":
            depth -= 1
    return False


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


def _valid_session(value):
    """True only for a nonempty string session identity."""
    return isinstance(value, str) and value != ""


def _valid_sequence(value):
    """True only for a nonnegative integer, never a boolean."""
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def parse_ts(value):
    """Parse a timestamp representable in UTC; otherwise raise fixed ValueError."""
    if not isinstance(value, str) or not value.strip():
        raise ValueError("invalid timestamp")
    text = value.strip().replace("Z", "+00:00")
    try:
        moment = datetime.fromisoformat(text)
        if moment.tzinfo is None:
            moment = moment.replace(tzinfo=timezone.utc)
        return moment.astimezone(timezone.utc)
    except (ValueError, OverflowError):
        raise ValueError("invalid timestamp") from None


def summarize(lines):
    """Fold JSONL log lines into the soak-evidence summary dict."""
    connects = disconnects = resumes = dispatches = 0
    missed = []
    unknowns = []
    first_ts = last_ts = last_event_ts = None
    last_seq = {}  # session -> highest committed dispatch seq
    down_since = None  # ts of latest disconnect awaiting reconnect
    gaps = []  # disconnect->connect/resume durations in seconds
    max_gap_seen = None  # over every gap, including ones dropped below
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

    def note_gap(seconds):
        nonlocal truncated, max_gap_seen
        max_gap_seen = seconds if max_gap_seen is None else max(max_gap_seen, seconds)
        if len(gaps) >= MAX_REDEPLOY_GAPS:
            truncated = True
            return
        gaps.append(seconds)

    for lineno, raw in enumerate(lines, 1):
        line = raw.strip()
        if not line:
            continue
        if _json_depth_exceeds(line, MAX_RECORD_DEPTH):
            note_unknown(None, None,
                         f"malformed line {lineno}: too deeply nested")
            continue
        try:
            record = json.loads(line, object_pairs_hook=_unique_object)
        except RecursionError:
            # Decoder-level backstop behind the lexical pre-scan above, for
            # any decoder whose stack is smaller than MAX_RECORD_DEPTH.
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
        except ValueError:
            note_unknown(None, None, f"malformed line {lineno}: bad ts")
            continue
        if last_event_ts is not None and ts < last_event_ts:
            note_unknown(ts, ts, f"timestamp regression on line {lineno}")
            continue
        # No RecursionError guard here or around parse_ts above: the record
        # already passed the iterative _too_deeply_nested check, so this
        # block performs no recursive traversal and cannot raise it. The
        # only decoder-level backstop is the json.loads handler above.
        event = record.get("event")
        if event not in KNOWN_EVENTS:
            note_unknown(ts, ts, f"unrecognised event {event!r}")
            continue
        session = record.get("session")

        if event in ("connect", "resume", "dispatch"):
            if not _valid_session(session):
                note_unknown(ts, ts, f"{event} with invalid session")
                continue

        seq = record.get("seq")
        if event == "dispatch" and not _valid_sequence(seq):
            note_unknown(ts, ts, "dispatch without integer seq (must be nonnegative)")
            continue
        if event == "resume" and "seq" in record and not _valid_sequence(seq):
            note_unknown(ts, ts, "resume with invalid seq")
            continue

        if event == "unknown":
            start = end = None
            try:
                if record.get("start"):
                    start = parse_ts(record["start"])
                if record.get("end"):
                    end = parse_ts(record["end"])
            except ValueError:
                note_unknown(ts, ts, "unknown interval with bad start/end")
                continue

        last_event_ts = ts
        first_ts = ts if first_ts is None else min(first_ts, ts)
        last_ts = ts if last_ts is None else max(last_ts, ts)

        if event == "connect":
            connects += 1
            last_seq.pop(session, None)
            if down_since is not None:
                note_gap((ts - down_since).total_seconds())
                down_since = None
        elif event == "disconnect":
            disconnects += 1
            if down_since is None:
                down_since = ts
        elif event == "resume":
            resumes += 1
            if down_since is not None:
                note_gap((ts - down_since).total_seconds())
                down_since = None
            if "seq" in record:
                prev = last_seq.get(session)
                if prev is None or seq > prev:
                    last_seq[session] = seq
        elif event == "dispatch":
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
            note_unknown(start or ts, end,
                         str(record.get("reason", "unspecified")))

    if down_since is not None:
        # Log ends while disconnected: the outage length is unbounded, so it
        # stays UNKNOWN instead of being scored as zero.
        note_unknown(down_since, None, "log ends while disconnected")

    missed_events = sum(window["count"] for window in missed)
    max_gap = max_gap_seen
    within_budget = (max_gap is None or max_gap < REDEPLOY_GAP_BUDGET_S)
    # Empty, blank-only, or dispatch-free input leaves no evidence of live
    # traffic. That is missing evidence, not a clean run (per
    # docs/staging-soak.md missing evidence remains UNKNOWN), so it must not
    # PASS. Keep the honest null window/zero counts; fail closed via verdict.
    # missed_events derives only from dispatch seq gaps, so zero dispatches
    # makes "0 missed" vacuous; require at least one dispatch as evidence.
    has_evidence = first_ts is not None and dispatches > 0
    verdict = "PASS" if (has_evidence and missed_events == 0 and not unknowns
                          and within_budget) else "NEEDS WORK"
    summary = {
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
    if truncated:
        summary["truncated"] = True
    return summary


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, epilog=HELP_EPILOG,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("log", help="gateway event log in JSONL format")
    args = parser.parse_args(argv)
    try:
        lines = _read_log_lines(args.log)
    except IntakeRefused as refused:
        json.dump({"verdict": "INVALID", "code": refused.code,
                   "next_action": NEXT_ACTION[refused.code]},
                  sys.stdout, indent=2)
        sys.stdout.write("\n")
        return 2
    summary = summarize(lines)
    json.dump(summary, sys.stdout, indent=2)
    sys.stdout.write("\n")
    if summary["verdict"] != "PASS":
        # Nonzero so CI and the live-run wrapper fail closed; the JSON on
        # stdout still carries the full evidence either way.
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
