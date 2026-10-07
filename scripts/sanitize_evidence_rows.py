#!/usr/bin/env python3
"""Offline, stdin-only conversion of the evidence-route SQL rows; no DB access.

Input is a JSON array of {idempotency_key, event_type, recorded_at} rows.
Output carries only ordinals, known event kinds, UTC times and truncation.
Raw keys are never printed, logged or persisted. A saturated LIMIT 60 is
conservatively truncated because the original query cannot witness row 61.
"""

import datetime as dt
import json
import sys

MAX_RECEIPTS = 60
MAX_INPUT_BYTES = 1024 * 1024
EVENT_TYPES = frozenset((
    "invite_click", "member_join", "gate_cleared", "onboarding_prompted",
    "game_roles_selected", "channel_routed", "first_message", "second_message",
    "third_message", "first_voice_session", "voice_session_start",
    "voice_session_end", "member_inactive", "member_leave",
))


class Refused(ValueError):
    """Only fixed, privacy-safe messages reach stderr."""


def sanitize(text):
    try:
        raw = json.loads(text)
    except (ValueError, RecursionError):
        raise Refused("malformed row JSON") from None
    if not isinstance(raw, list):
        raise Refused("rows must be a JSON array")
    rows = []
    for ordinal, row in enumerate(raw):
        if not isinstance(row, dict) or set(row) != {
                "idempotency_key", "event_type", "recorded_at"}:
            raise Refused("unexpected row fields")
        if not isinstance(row["idempotency_key"], str) or not row["idempotency_key"]:
            raise Refused("invalid row key")
        kind, timestamp = row["event_type"], row["recorded_at"]
        if not isinstance(kind, str) or kind not in EVENT_TYPES:
            raise Refused("unknown event type")
        if not isinstance(timestamp, str):
            raise Refused("invalid receipt time")
        try:
            at = dt.datetime.fromisoformat(timestamp.replace("Z", "+00:00"))
        except ValueError:
            raise Refused("invalid receipt time") from None
        if at.utcoffset() is None:
            raise Refused("receipt time must carry a timezone")
        try:
            at = at.astimezone(dt.timezone.utc)
        except (ValueError, OverflowError):
            raise Refused("invalid receipt time") from None
        if ordinal < MAX_RECEIPTS:
            rows.append({"ordinal": ordinal, "event_type": kind,
                         "recorded_at": at.isoformat(timespec="milliseconds").replace("+00:00", "Z")})
    return {"rows": rows, "truncated": len(raw) >= MAX_RECEIPTS}


def main():
    try:
        text = sys.stdin.buffer.read(MAX_INPUT_BYTES + 1)
        if len(text) > MAX_INPUT_BYTES:
            raise Refused("input exceeds byte cap")
        packet = sanitize(text)
    except (Refused, OSError):
        # Do not echo JSON decoder, IO, key or timestamp details.
        print("sanitize_evidence_rows: refused input; no artifact written", file=sys.stderr)
        return 2
    print(json.dumps(packet, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
