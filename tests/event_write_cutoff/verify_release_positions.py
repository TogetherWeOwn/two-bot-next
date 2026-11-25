"""Verify a captured admitted-image rehearsal offline.

Before running rehearse.py, save the selected Bot pins from the serving
and rollback records as JSON:

    {
      "current": {"image": "<repo>@sha256:<64 hex>", "source_revision": "<40 hex>"},
      "retained": {"image": "<repo>@sha256:<64 hex>", "source_revision": "<40 hex>"}
    }

Use the admitted-image runner with both per-position announcement controls.
Capture stdout and preserve the exit code (without piping through tee):

    result=0
    PYTHONDONTWRITEBYTECODE=1 python3 tests/event_write_cutoff/rehearse.py \
        --current "$current_image" --retained "$retained_image" > cutoff.log || result=$?
    python3 tests/event_write_cutoff/verify_release_positions.py \
        --pins selected.json --transcript cutoff.log --runner-exit-code "$result"

Use pins independent of the transcript and capture the exit code and stdout
from the same invocation. This checks Bot evidence from the isolated runner.
Worker identity and effective Discord permissions come from separate evidence.
"""

import argparse
import json
import re
import sys
from pathlib import Path


POSITIONS = ("current", "retained")
IMAGE = re.compile(r"[a-z0-9][a-z0-9._:/-]*@sha256:[0-9a-f]{64}")
REVISION = re.compile(r"[0-9a-f]{40}")
PASS = {"verdict": "passed"}


def read_text(path, label, limit):
    try:
        with path.open("rb") as stream:
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


def verify(pins, transcript, runner_exit_code):
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
        pins = json_object(read_text(args.pins, "selected pins", 4096), "Selected pins")
        transcript = read_text(args.transcript, "rehearsal transcript", 1024 * 1024)
        verify(pins, transcript, args.runner_exit_code)
    except ValueError as error:
        print(f"Rehearsal evidence refused: {error}", file=sys.stderr)
        return 1
    print("Rehearsal evidence matches both selected Bot positions")
    return 0


if __name__ == "__main__":
    sys.exit(main())
