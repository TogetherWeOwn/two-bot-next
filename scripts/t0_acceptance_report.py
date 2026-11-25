#!/usr/bin/env python3
"""Compare saved smoke observations with the serving window in an operator handoff."""

import argparse
from datetime import datetime
import hashlib
import json
from pathlib import Path
import re

from build_identity import is_build_id_shape, is_build_revision_shape, is_deploy_id_shape
from container_stdout_read import MAX_LIMIT, UUID_RE
from script_intake import decode_json, read_with_overflow
from staging_help_embed_smoke import EMBED_KEYS, SCHEMA as HELP_SCHEMA

MAX_BYTES = 64 * 1024
HEX_DIGEST = re.compile(r"[0-9a-f]{64}")
UTC_INSTANT = re.compile(r"([0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2})"
                         r"(?:\.([0-9]+))?(?:Z|[+-]00:?00)")
WINDOW_FIELDS = ("environment", "build_revision", "build_id", "deploy_id", "image_digest",
                 "worker_version", "application_id", "start", "end")
SIGNALS = ("ready", "gateway_resumed", "shard_closed", "gateway_reconnect_failed",
           "identify_planned", "resume_planned")
GATEWAY_MISMATCHES = ("service_mismatch", "stale_evidence", "mixed_build", "build_mismatch")
TRANSITIONS = ("empty-room-rename", "occupied-room-return")
PRODUCTION_WATCH_SCHEMA = "production_watch.v1"
WATCH_OBSERVATIONS = {"help": ("branding-readbacks-and-deletions",), "naming": TRANSITIONS}
NEEDED = {
    "help": "Record Help branding, readbacks and deletions during the intended serving window.",
    "naming": "Record occupied, empty before cleanup and occupied-again names on the same room during the window.",
    "gateway": "Retain the passive summary from the unfiltered capture and its verified single-image application window.",
}


def read_object(path):
    with path.open("rb") as stream:
        raw = read_with_overflow(stream, MAX_BYTES)
    if len(raw) > MAX_BYTES:
        raise ValueError
    document = decode_json(raw.decode("utf-8"), max_depth=16)
    if not isinstance(document, dict):
        raise ValueError
    return document, hashlib.sha256(raw).hexdigest()


def token(pattern, value):
    return isinstance(value, str) and pattern.fullmatch(value) is not None


def utc_instant(value):
    match = UTC_INSTANT.fullmatch(value) if isinstance(value, str) else None
    if match is None:
        raise ValueError
    second = datetime.fromisoformat(match[1])
    # Keep the fraction as digits so parsing cannot truncate sub-microsecond bounds.
    return second, (match[2] or "").rstrip("0")


def format_utc(instant):
    second, fraction = instant
    return second.isoformat() + ("." + fraction if fraction else "") + "Z"


def serving_window(value):
    if not isinstance(value, dict):
        raise ValueError
    env, revision, build, deploy, digest, worker, app, start, end = map(value.get, WINDOW_FIELDS)
    start, end = utc_instant(start), utc_instant(end)
    if (not is_build_revision_shape(revision) or not is_build_id_shape(build)
            or not is_deploy_id_shape(deploy) or not deploy.endswith("+" + revision[:7])
            or not isinstance(digest, str) or not digest.startswith("sha256:")
            or not token(HEX_DIGEST, digest[7:]) or not token(UUID_RE, worker) or not token(UUID_RE, app)
            or env not in ("staging", "production") or start >= end):
        raise ValueError
    return {**{key: value[key] for key in WINDOW_FIELDS}, "start": format_utc(start), "end": format_utc(end)}


def help_observation(report):
    rows, verdict = report.get("rows"), report.get("verdict")
    if report.get("schema") != HELP_SCHEMA or not isinstance(rows, list):
        raise ValueError
    if verdict == "REFUSED" and rows == [] and isinstance(report.get("refusal"), str) and report["refusal"]:
        return {"result": "probe_failed"}
    if ("refusal" in report or not rows or not all(
            isinstance(row, dict) and row.get("result") in ("pass", "fail")
            and isinstance(row.get("actual"), str) and type(row.get("duration_ms")) is int
            and row["duration_ms"] >= 0 for row in rows)):
        raise ValueError
    names = tuple(row.get("name") for row in rows)
    if verdict == "FAIL" and names in (("preflight",), EMBED_KEYS[:len(rows)]) and any(
            row["result"] == "fail" for row in rows):
        return {"result": "probe_failed"}
    if (verdict != "PASS" or names != EMBED_KEYS or not all(
            row["result"] == "pass" and row["actual"] == "read-back branding, text and fields match; message deleted"
            and "cleanup_failed" not in row for row in rows)):
        raise ValueError
    return {"result": "readbacks_and_deletions_recorded"}


def naming_observation(report):
    if report.get("transport") != "offline-naming-observations":
        raise ValueError
    outcome = report.get("result")
    if outcome == "fail":
        return {"result": "probe_failed"}
    checks = report.get("checks")
    if (not isinstance(checks, list) or len(checks) != len(TRANSITIONS) or not all(
            isinstance(row, dict) and row.get("check") == name and row.get("verdict") in ("pass", "skip")
            for name, row in zip(TRANSITIONS, checks))):
        raise ValueError
    missing = [row["check"] for row in checks if row["verdict"] == "skip"]
    if outcome != ("incomplete" if missing else "pass"):
        raise ValueError
    return {"result": "not_observed" if missing else "transitions_recorded", "missing_transitions": missing}


def production_watch_observation(kind, report, window):
    """Read the fixed fields projected from authorized execution-card rows."""
    rows = report.get("rows")
    checks = WATCH_OBSERVATIONS[kind]
    if (report.keys() != {"schema", "signal", "rows"} or report["signal"] != kind
            or not isinstance(rows, list) or not 1 <= len(rows) <= len(checks)):
        raise ValueError

    outcomes = {}
    times = {}
    for row in rows:
        if not isinstance(row, dict) or row.keys() != {"utc", "observation", "disposition"}:
            raise ValueError
        check, disposition = row["observation"], row["disposition"]
        if (check not in checks or check in outcomes
                or disposition not in ("pass", "fail", "not_observed")):
            raise ValueError
        times[check] = utc_instant(row["utc"])
        outcomes[check] = disposition

    start, end = utc_instant(window["start"]), utc_instant(window["end"])
    if any(not start <= at <= end for at in times.values()):
        return None
    missing = [check for check in checks if outcomes.get(check, "not_observed") == "not_observed"]
    if "fail" in outcomes.values():
        return {"result": "probe_failed"}
    if kind == "help":
        return {"result": "not_observed" if missing else "readbacks_and_deletions_recorded"}
    if not missing and times[TRANSITIONS[0]] >= times[TRANSITIONS[1]]:
        raise ValueError
    return {"result": "not_observed" if missing else "transitions_recorded", "missing_transitions": missing}


def gateway_observation(report):
    counts = report.get("signals")
    if (not is_build_revision_shape(report.get("build_sha")) or not isinstance(counts, dict)
            or counts.keys() != set(SIGNALS) or not all(type(n) is int and 0 <= n <= MAX_LIMIT
                                                       for n in counts.values())
            or sum(counts.values()) > MAX_LIMIT):
        raise ValueError
    verdict = "observed" if any(counts.values()) else "not_observed"
    if report.get("verdict") != verdict:
        raise ValueError
    readiness = "observed" if counts["ready"] or counts["gateway_resumed"] else "not_observed"
    return {"result": verdict, "readiness": readiness, "signals": {key: counts[key] for key in SIGNALS}}


def observation(kind, parse, path, binding, expected):
    def gap(status, reason, **details):
        return {"status": status, "reason": reason, "needed": NEEDED[kind], **details}

    if path is None:
        return gap("missing", "no saved observation supplied")
    try:
        report, digest = read_object(path)
    except FileNotFoundError:
        return gap("missing", "saved observation file absent")
    except (OSError, RecursionError, ValueError):
        return gap("unverified", "saved observation unreadable or invalid")

    if (kind == "gateway" and is_build_revision_shape(report.get("build_sha"))
            and report["build_sha"] != expected["build_revision"]):
        return gap("mismatched", "gateway summary names a different build revision")
    if kind == "gateway" and report.get("verdict") in GATEWAY_MISMATCHES:
        return gap("mismatched", "passive validator reported a mismatch", source_verdict=report["verdict"])
    if not isinstance(binding, dict) or not token(HEX_DIGEST, binding.get("sha256")):
        return gap("unverified", "no existing attribution for these observation bytes")
    try:
        recorded = serving_window(binding.get("window"))
    except ValueError:
        return gap("unverified", "no complete serving identity for this observation")
    if binding["sha256"] != digest:
        return gap("mismatched", "saved observation differs from the attributed bytes")
    differing = [key for key in expected if expected[key] != recorded[key]]
    if differing:
        return gap("mismatched", "observation belongs to a different serving window",
                   fields=differing, window=recorded)
    try:
        if expected["environment"] == "production" and kind in WATCH_OBSERVATIONS:
            if report.get("schema") != PRODUCTION_WATCH_SCHEMA:
                return gap("unverified", "this probe format lacks production observations")
            outcome = production_watch_observation(kind, report, recorded)
            if outcome is None:
                return gap("mismatched", "production watch row falls outside the serving window")
        else:
            outcome = parse(report)
    except ValueError:
        return gap("unverified", "saved probe output is invalid or refused")
    row = {"status": "attributed", "window": recorded, **outcome}
    if outcome["result"] in ("not_observed", "probe_failed"):
        row["needed"] = NEEDED[kind]
    if outcome.get("readiness") == "not_observed":
        row["readiness_needed"] = "A committed READY or RESUMED in the serving window."
    return row


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--handoff", required=True, type=Path, help="local serving window and existing attributions")
    parser.add_argument("--help-report", type=Path, help="saved Help smoke or production watch JSON")
    parser.add_argument("--naming-report", type=Path, help="saved room naming or production watch JSON")
    parser.add_argument("--gateway-report", type=Path, help="saved passive gateway summary JSON")
    args = parser.parse_args(argv)
    try:
        handoff, _ = read_object(args.handoff)
        expected = serving_window(handoff.get("expected"))
        bindings = handoff.get("observations", {})
        if not isinstance(bindings, dict):
            raise ValueError
    except (OSError, RecursionError, ValueError):
        print(json.dumps({"error": "handoff unreadable or missing a complete expected serving window"}))
        return 2

    rows = {kind: observation(kind, parse, path, bindings.get(kind), expected) for kind, parse, path in (
        ("help", help_observation, args.help_report), ("naming", naming_observation, args.naming_report),
        ("gateway", gateway_observation, args.gateway_report),
    )}
    rows["gateway"]["endpoint_denial"] = "unverified"
    rows["gateway"]["endpoint_denial_needed"] = "Explicit passive evidence of gateway endpoint rejection."
    print(json.dumps({"expected": expected, "observations": rows}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
