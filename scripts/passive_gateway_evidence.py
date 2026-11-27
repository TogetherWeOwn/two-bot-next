#!/usr/bin/env python3
"""Summarize a saved, sanitized container stdout packet without network access."""

import argparse
from datetime import datetime
import json
import re
import sys

from container_stdout_read import (
    BUILD_SHA_RE, MAX_LIMIT, MAX_MINUTES, SCHEMA_VERSION, UUID_RE, Refused, parse_instant,
)
from script_intake import decode_json, read_with_overflow

MAX_PACKET_BYTES = 4 * 1024 * 1024
SUMMARY_SCHEMA = "passive_gateway_summary.v1"
DENIAL_SUMMARY_SCHEMA = "passive_gateway_summary.v2"
POLICY_REFUSAL = "gateway_resume_endpoint_policy_refused"
PACKET_TIMESTAMP = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3}Z")
SIGNALS = {
    "gateway_connecting": "two_bot",
    "ready": "two_bot::gateway",
    "gateway_resumed": "two_bot::gateway",
    "shard_closed": "two_bot::logging",
    "gateway_reconnect_failed": "two_bot::gateway",
    POLICY_REFUSAL: "two_bot::gateway",
}


def packet_time(value):
    if not isinstance(value, str) or not PACKET_TIMESTAMP.fullmatch(value):
        raise ValueError
    return datetime.fromisoformat(value[:-1] + "+00:00")


def is_sha(value):
    return isinstance(value, str) and BUILD_SHA_RE.fullmatch(value) is not None


def summarize(packet, *, service, sha, start, end):
    """Count observed signals only within the independently verified serving window."""
    if (not isinstance(packet, dict) or type(packet.get("schema_version")) is not int
            or packet["schema_version"] != SCHEMA_VERSION):
        return {"verdict": "invalid_packet"}
    if packet.get("service") != service:
        return {"verdict": "service_mismatch"}
    if packet.get("contains") is True:
        return {"verdict": "filtered_window"}
    if packet.get("contains") is not False:
        return {"verdict": "invalid_packet"}
    window, events, limit = (packet.get(key) for key in ("window", "events", "limit"))
    count, truncated = packet.get("event_count"), packet.get("possibly_truncated")
    if (not isinstance(window, dict) or not isinstance(events, list)
            or type(limit) is not int or not 1 <= limit <= MAX_LIMIT
            or type(count) is not int or count != len(events) or count > limit
            or truncated is not (count == limit)):
        return {"verdict": "invalid_packet"}
    try:
        first, last = packet_time(window.get("start")), packet_time(window.get("end"))
        times = [packet_time(event.get("ts")) for event in events]
    except (AttributeError, ValueError):
        return {"verdict": "invalid_packet"}
    if (first, last) != (start, end) or any(not start <= ts <= end for ts in times):
        return {"verdict": "stale_evidence"}

    builds = {event["build_sha"] for event in events if is_sha(event.get("build_sha"))}
    if builds - {sha}:
        return {"verdict": "mixed_build" if len(builds) > 1 else "build_mismatch"}
    if truncated:
        return {"verdict": "incomplete_window"}

    signals = dict.fromkeys(("ready", "gateway_resumed", "shard_closed", "gateway_reconnect_failed",
                             "identify_planned", "resume_planned"), 0)
    refused_at = None
    for event in events:
        name = event.get("msg")
        if not isinstance(name, str) or name not in SIGNALS or event.get("target") != SIGNALS[name]:
            continue
        if event.get("build_sha") != sha:
            return {"verdict": "unbound_build"}
        if name == POLICY_REFUSAL:
            stamp = event["ts"]  # All timestamps have been validated in the same UTC format.
            refused_at = stamp if refused_at is None else min(refused_at, stamp)
            continue
        if name == "gateway_connecting":
            resume = event.get("resume")
            if type(resume) is not bool:
                return {"verdict": "invalid_packet"}
            name = "resume_planned" if resume else "identify_planned"
        signals[name] += 1
    if not builds:
        return {"verdict": "no_build_events"}
    summary = {"schema": SUMMARY_SCHEMA, "verdict": "observed" if any(signals.values()) else "not_observed",
               "build_sha": sha, "window": {"start": window["start"], "end": window["end"]}, "signals": signals}
    if refused_at is not None:
        summary.update(schema=DENIAL_SUMMARY_SCHEMA, verdict="observed", endpoint_policy_refused_at=refused_at)
    return summary


def main(argv=None, *, stdin=None, stdout=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--service", required=True, help="verified Container application id")
    parser.add_argument("--sha", required=True, help="verified serving 40-hex source revision")
    parser.add_argument("--start", required=True, help="verified observation start (UTC, ending in Z)")
    parser.add_argument("--end", required=True, help="verified observation end (UTC, ending in Z)")
    args = parser.parse_args(argv)
    stdin = sys.stdin.buffer if stdin is None else stdin
    stdout = sys.stdout if stdout is None else stdout
    result = {"verdict": "invalid_packet"}
    try:
        start, end = parse_instant(args.start), parse_instant(args.end)
        if not (UUID_RE.fullmatch(args.service) and is_sha(args.sha)
                and 0 < (end - start).total_seconds() <= MAX_MINUTES * 60):
            raise Refused
        raw = read_with_overflow(stdin, MAX_PACKET_BYTES)
        if len(raw) <= MAX_PACKET_BYTES:
            packet = decode_json(raw.decode("utf-8"))
            result = summarize(packet, service=args.service, sha=args.sha, start=start, end=end)
    except (OSError, RecursionError, Refused, ValueError):
        pass  # fixed verdict only, including on malformed input
    print(json.dumps(result), file=stdout)
    return 0 if result["verdict"] in ("observed", "not_observed") else 1


if __name__ == "__main__":
    raise SystemExit(main())
