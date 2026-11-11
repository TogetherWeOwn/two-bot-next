#!/usr/bin/env python3
"""Diff the database-exchange mechanism between two qualified pipeline reports.

Usage: pipeline_bench_profile.py BEFORE AFTER

Both reports must be schema_version 1, share one workload and carry the
independent durability and counter-integrity evidence the pipeline_bench
example emits. A historical report without that evidence is refused as
unqualified. This tool scores nothing and never changes a baseline: it only
explains, per server statement fingerprint and per event kind, where the
db_round_trips_per_event delta came from.

Intake reuses compare_pipeline_bench: bounded reads (1 MiB, 64 KiB lines,
100000 lines), duplicate JSON key refusal and metric validation. Every
refusal exits 2 with the same fixed INVALID JSON on stdout and an "INVALID
benchmark:" line on stderr. Nothing echoes report content, and the output
holds statement fingerprints only, never SQL text or parameters.
"""

import argparse
import json
import math
import re
import sys
from pathlib import Path

import compare_pipeline_bench as bench


METRIC = ("db_round_trips_per_event",)
OBSERVER_TRANSACTIONS = 2
INT64_MIN = -(2**63)
INT64_MAX = 2**63 - 1
FINGERPRINT = re.compile(r"-?[0-9]{1,19}")
EVENT_NAME = re.compile(r"[A-Za-z0-9_]{1,64}")
# Tight: the metric is one float division of integers.
REL_TOL = 1e-12
ABS_TOL = 1e-12


REQUIRED = (
    "measured_events", "sqlx_statements", "begin_exchanges", "commits",
    "server_xact_commit", "server_xact_rollback", "server_observer_transactions",
    "server_statement_calls", "server_statement_profile", "event_statement_profile",
    "expected_durable_events", "durable_events", "journal_rows", "journaled_events",
    "every_event_durable_after_drain", "disposable_database_dropped",
)


def _count(evidence, name, minimum):
    value = evidence.get(name)
    if isinstance(value, bool) or not isinstance(value, int) or value < minimum:
        raise ValueError(f"evidence.{name} must be an integer of at least {minimum}")
    return value


def _equal(evidence, name, expected):
    if _count(evidence, name, 0) != expected:
        raise ValueError(f"evidence.{name} does not match its counter identity")


def _profile_object(evidence, name):
    profile = evidence.get(name)
    if not isinstance(profile, dict) or not profile:
        raise ValueError(f"evidence.{name} must be a non-empty object")
    return profile


def _server_profile(evidence, calls):
    profile = {}
    for key, count in _profile_object(evidence, "server_statement_profile").items():
        if not FINGERPRINT.fullmatch(key) or not INT64_MIN <= int(key) <= INT64_MAX:
            raise ValueError("server_statement_profile fingerprint must be a signed 64-bit integer string")
        if isinstance(count, bool) or not isinstance(count, int) or count <= 0:
            raise ValueError("server_statement_profile calls must be positive integers")
        profile[key] = count
    if sum(profile.values()) != calls:
        raise ValueError("server_statement_profile does not sum to server_statement_calls")
    return profile


def _event_profile(evidence, events, calls):
    profile = {}
    for name, pair in _profile_object(evidence, "event_statement_profile").items():
        if not EVENT_NAME.fullmatch(name):
            raise ValueError("event_statement_profile event name is malformed")
        if not isinstance(pair, list) or len(pair) != 2 or any(
            isinstance(n, bool) or not isinstance(n, int) or n <= 0 for n in pair
        ):
            raise ValueError("event_statement_profile entries must be two positive integers")
        profile[name] = (pair[0], pair[1])
    if sum(count for count, _ in profile.values()) != events:
        raise ValueError("event_statement_profile event counts do not sum to measured_events")
    if sum(exchanges for _, exchanges in profile.values()) != calls:
        raise ValueError("event_statement_profile exchanges do not sum to server_statement_calls")
    return profile


def _workload_event_counts(workload):
    for name, minimum, maximum in (("duration_secs", 1, 120),
                                   ("messages_per_sec", 0, 1000),
                                   ("voice_per_sec", 0, 1000)):
        value = workload.get(name)
        if isinstance(value, bool) or not isinstance(value, int) or not minimum <= value <= maximum:
            raise ValueError(f"workload.{name} must be an integer in {minimum}..{maximum}")
    return {family: workload["duration_secs"] * workload[rate]
            for family, rate in (("MESSAGE_CREATE", "messages_per_sec"),
                                 ("VOICE_STATE_UPDATE", "voice_per_sec"))
            if workload[rate] > 0}


def qualify(report):
    """Validate one report; return its comparable mechanism facts."""
    if not isinstance(report, dict):
        raise ValueError("report must be an object")
    metrics = bench.validate(report)
    evidence = report.get("evidence")
    if not isinstance(evidence, dict):
        raise ValueError("unqualified report: evidence missing")
    if any(name not in evidence for name in REQUIRED):
        raise ValueError("unqualified report: required evidence fields missing")
    events = _count(evidence, "measured_events", 1)
    statements = _count(evidence, "sqlx_statements", 0)
    _equal(evidence, "begin_exchanges", events)
    _equal(evidence, "commits", events)
    _equal(evidence, "server_xact_commit", events)
    _equal(evidence, "server_xact_rollback", 0)
    _equal(evidence, "server_observer_transactions", OBSERVER_TRANSACTIONS)
    calls = statements + events
    _equal(evidence, "server_statement_calls", calls)
    durable = _count(evidence, "expected_durable_events", 1)
    for name in ("durable_events", "journal_rows", "journaled_events"):
        _equal(evidence, name, durable)
    for name in ("every_event_durable_after_drain", "disposable_database_dropped"):
        if evidence.get(name) is not True:
            raise ValueError(f"evidence.{name} must be true")
    server = _server_profile(evidence, calls)
    per_event = _event_profile(evidence, events, calls)
    if {name: count for name, (count, _) in per_event.items()} != _workload_event_counts(report["workload"]):
        raise ValueError("event_statement_profile event counts do not match declared workload")
    if not math.isclose(metrics[METRIC], calls / events, rel_tol=REL_TOL, abs_tol=ABS_TOL):
        raise ValueError("db_round_trips_per_event does not match statement counters")
    return {"events": events, "calls": calls, "metric": metrics[METRIC],
            "server": server, "per_event": per_event}


def diff(before_report, after_report):
    """Return the mechanism diff of two qualified same-workload reports."""
    before, after = qualify(before_report), qualify(after_report)
    if before_report["workload"] != after_report["workload"]:
        raise ValueError("workload differs; do not compare unlike profiles")
    if before["events"] != after["events"]:
        raise ValueError("measured_events differs; do not compare unlike profiles")
    events = before["events"]
    exchange_delta = after["calls"] - before["calls"]
    metric_delta = after["metric"] - before["metric"]
    if not math.isclose(metric_delta * events, exchange_delta, rel_tol=REL_TOL, abs_tol=1e-9):
        raise ValueError("metric delta is not explained by the mechanism profile sum")
    fingerprints = []
    for key in sorted(before["server"].keys() | after["server"].keys(), key=int):
        was, now = before["server"].get(key, 0), after["server"].get(key, 0)
        if was != now:
            fingerprints.append({"fingerprint": key, "before": was, "after": now, "delta": now - was})
    event_kinds = []
    for name in sorted(before["per_event"].keys() | after["per_event"].keys()):
        was = before["per_event"].get(name, (0, 0))
        now = after["per_event"].get(name, (0, 0))
        event_kinds.append({
            "event": name,
            "before": {"events": was[0], "exchanges": was[1]},
            "after": {"events": now[0], "exchanges": now[1]},
            "delta": {"events": now[0] - was[0], "exchanges": now[1] - was[1]},
        })
    return {
        "schema_version": 1,
        "measured_events": events,
        "total_exchange_delta": exchange_delta,
        "db_round_trips_per_event": {
            "before": before["metric"], "after": after["metric"], "delta": metric_delta,
        },
        "changed_server_fingerprints": fingerprints,
        "event_statement_profile": event_kinds,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    args = parser.parse_args(argv)
    try:
        texts = [bench._read_report(path) for path in (args.before, args.after)]
    except bench.IntakeRefused as refused:
        return bench._invalid(refused.code)
    try:
        reports = [json.loads(text, object_pairs_hook=bench._reject_duplicate_keys) for text in texts]
    except bench.DuplicateKeyError:
        return bench._invalid("duplicate_keys")
    except (ValueError, RecursionError):
        return bench._invalid("invalid_json")
    try:
        result = diff(*reports)
    except OverflowError:
        return bench._invalid("invalid_benchmark", "benchmark metric exceeds numeric range")
    except (ValueError, KeyError, TypeError) as error:
        return bench._invalid("invalid_benchmark", str(error))
    json.dump(result, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
