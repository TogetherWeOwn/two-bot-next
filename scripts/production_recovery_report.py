#!/usr/bin/env python3
"""Report saved recovery results against one independently observed ownership state."""

import argparse
from datetime import datetime, timedelta
from hashlib import sha256
import json
from pathlib import Path
import sys

from production_recovery_takeover import HEX, UUID, RecoveryPin, observed_phase, verify_inverse
from script_intake import decode_json, read_with_overflow
from staging_rollout import GateError, mapping
from worker_container_rollback import STABLE_PASSES

MAX_BYTES = 64 * 1024
MAX_RECEIPTS = 16


def read_object(path, expected_digest=None):
    with Path(path).open("rb") as stream:
        raw = read_with_overflow(stream, MAX_BYTES)
    if len(raw) > MAX_BYTES or (expected_digest is not None and sha256(raw).hexdigest() != expected_digest):
        raise ValueError
    return mapping(decode_json(raw.decode("utf-8"), max_depth=16))


def observation_time(state):
    at = datetime.fromisoformat(state["ownership_observed_at"])
    if at.utcoffset() != timedelta(0):
        raise ValueError
    return at


def scope(state):
    container = state["container"]
    return container["application_id"], container["namespace_id"]


def deployment(state):
    worker, container = state["worker"], state["container"]
    ids = worker["deployment_id"], container["rollout_id"]
    if not all(isinstance(value, str) and UUID.fullmatch(value) for value in ids):
        raise ValueError
    return ids


def classify(receipt, current, as_of):
    if (receipt.keys() != {"approval", "inverse", "provider", "stable_polls"}
            or not mapping(receipt["approval"])
            or type(receipt["stable_polls"]) is not int or receipt["stable_polls"] < STABLE_PASSES):
        raise ValueError
    inverse = mapping(receipt["inverse"])
    pin = RecoveryPin(**{key: value for key, value in inverse.items() if key != "state"})
    verify_inverse(pin, inverse)
    reported = receipt["provider"]
    if observed_phase(pin, lambda: reported) != "active":
        raise ValueError
    restored_deployment = deployment(reported)
    if scope(current) != scope(reported) or as_of <= observation_time(reported):
        return "unverified"
    epoch = current["ownership"]["epoch"]
    if epoch > pin.fence_epoch + 1:
        return "superseded"
    if (current["ownership"]["running"] and deployment(current) == restored_deployment
            and observed_phase(pin, lambda: current) == "active"):
        return "current_as_of_observation"
    return "unverified"


def report_receipt(digest, path, current, as_of):
    if not HEX.fullmatch(digest):
        return {"status": "unverified"}
    try:
        status = classify(read_object(path, digest), current, as_of)
    except (GateError, KeyError, OSError, RecursionError, TypeError, ValueError):
        status = "unverified"
    return {"sha256": digest, "status": status}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--status", required=True, help="saved independent production ownership status JSON")
    parser.add_argument("--receipt", action="append", nargs=2, metavar=("SHA256", "FILE"), required=True,
                        help="verified restore receipt file and its independently recorded content digest")
    args = parser.parse_args(argv)
    if len(args.receipt) > MAX_RECEIPTS:
        parser.error(f"at most {MAX_RECEIPTS} receipts are supported")
    try:
        current = read_object(args.status)
        as_of = observation_time(current)
        deployment(current)
        owner = mapping(current["ownership"])
        epoch = owner["epoch"]
        if (type(epoch) is not int or not 0 < epoch < 2**53 or type(owner["running"]) is not bool
                or not (owner["phase"] == "active" or (owner["phase"] == "fenced" and not owner["running"]))):
            raise ValueError
    except (GateError, KeyError, OSError, RecursionError, TypeError, ValueError):
        print("recovery report refused: missing or invalid ownership observation", file=sys.stderr)
        return 2
    rows = [report_receipt(digest, path, current, as_of) for digest, path in args.receipt]
    print(json.dumps({"ownership_observed_at": as_of.isoformat(), "receipts": rows}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
