"""Verify a captured admitted-image rehearsal offline.

The Bot pins used by this verifier come from the serving and rollback records:

    {
      "current": {"image": "<repo>@sha256:<64 hex>", "source_revision": "<40 hex>"},
      "retained": {"image": "<repo>@sha256:<64 hex>", "source_revision": "<40 hex>"}
    }

Prepare and run the admitted-image rehearsal as documented in
docs/internal-event-executors.md. With its stdout and exit code:

    python3 tests/event_write_cutoff/verify_release_positions.py \
        --pins selected.json --transcript cutoff.log --runner-exit-code "$result"

Use pins independent of the transcript, run the reviewed fixture, and capture
its exit code and stdout together. The verifier checks consistency across those
inputs. Worker identity and effective Discord permissions come from separate evidence.
"""

import argparse
import json
import os
import re
import stat
import sys
from pathlib import Path


POSITIONS = ("current", "retained")
IMAGE = re.compile(r"[a-z0-9][a-z0-9._:/-]*@sha256:[0-9a-f]{64}")
REVISION = re.compile(r"[0-9a-f]{40}")
PASS = {"verdict": "passed"}


def read_text(path, label, limit):
    try:
        with open(path, "rb", opener=lambda path, flags: os.open(path, flags | os.O_NONBLOCK)) as stream:
            if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
                raise ValueError(f"{label} must be a regular file")
            data = stream.read(limit + 1)
        if len(data) > limit:
            raise ValueError(f"{label} exceeds {limit} bytes")
        return data.decode("utf-8")
    except (OSError, UnicodeError) as error:
        raise ValueError(f"Cannot read {label} as UTF-8") from error


def unique_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate object key")
        result[key] = value
    return result


def json_object(line, label):
    try:
        result = json.loads(line, object_pairs_hook=unique_keys)
    except (ValueError, RecursionError) as error:
        raise ValueError(f"{label} contains invalid JSON or duplicate keys") from error
    if not isinstance(result, dict):
        raise ValueError(f"{label} must be a JSON object")
    return result


def read_selected_pins(path):
    return json_object(read_text(path, "selected pins", 4096), "Selected pins")


def validate_selected_pins(pins):
    if set(pins) != set(POSITIONS):
        raise ValueError("Selected pins must contain current and retained positions")
    for position in POSITIONS:
        pin = pins[position]
        if not isinstance(pin, dict):
            raise ValueError(f"{position} requires an image and source revision")
        for field, pattern in (("image", IMAGE), ("source_revision", REVISION)):
            value = pin.get(field)
            if not isinstance(value, str) or not pattern.fullmatch(value):
                raise ValueError(f"{position} requires an immutable {field}")


def verify(pins, transcript, runner_exit_code):
    validate_selected_pins(pins)
    if runner_exit_code != 0:
        raise ValueError(f"Rehearsal exited {runner_exit_code}")
    lines = [line.strip() for line in transcript.splitlines() if line.strip()]
    if not lines:
        raise ValueError("Missing rehearsal transcript")
    header = json_object(lines[0], "Rehearsal identity record")
    for field, recorded_field in (("image", "images"), ("source_revision", "source_revisions")):
        recorded = header.get(recorded_field)
        if not isinstance(recorded, dict) or set(recorded) != set(POSITIONS):
            raise ValueError(f"Rehearsal must report {recorded_field} for both positions")
        for position in POSITIONS:
            if recorded[position] != pins[position][field]:
                raise ValueError(f"Rehearsal {position} {field} differs from selected pin")

    if not lines[-1].startswith("{") or json_object(lines[-1], "Final record") != PASS:
        raise ValueError("Missing final rehearsal pass")

    event_and_role_passed = False
    announcements = set()
    for line in lines[1:-1]:
        if not line.startswith("{"):
            continue
        record = json_object(line, "Rehearsal stage record")
        if "images" in record or "source_revisions" in record or record == PASS:
            raise ValueError("Unexpected additional rehearsal identity or final record")
        if "event_and_role_verdict" in record:
            if event_and_role_passed or record["event_and_role_verdict"] != "passed":
                raise ValueError("Expected one passing event-and-role stage")
            event_and_role_passed = True
        if record.get("case") == "positive":
            position = record.get("runtime")
            writes = record.get("writes")
            if not event_and_role_passed:
                raise ValueError("Announcement control precedes a passing event-and-role stage")
            if (position not in POSITIONS or position in announcements
                    or type(writes) is not int or writes != 1 or record.get("verdict") != "passed"):
                raise ValueError("Invalid per-position announcement positive control")
            announcements.add(position)

    if not event_and_role_passed:
        raise ValueError("Missing passing event-and-role stage")
    for position in POSITIONS:
        if position not in announcements:
            raise ValueError(f"Missing {position} announcement positive control")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--pins", type=Path, required=True, help="JSON pins selected before the run")
    parser.add_argument("--transcript", type=Path, required=True, help="captured rehearsal stdout")
    parser.add_argument("--runner-exit-code", type=int, required=True, help="exit status of that run")
    args = parser.parse_args()
    try:
        pins = read_selected_pins(args.pins)
        transcript = read_text(args.transcript, "rehearsal transcript", 1024 * 1024)
        verify(pins, transcript, args.runner_exit_code)
    except ValueError as error:
        print(f"Rehearsal evidence refused: {error}", file=sys.stderr)
        return 1
    print("Rehearsal evidence matches both selected Bot positions")
    return 0


if __name__ == "__main__":
    sys.exit(main())
