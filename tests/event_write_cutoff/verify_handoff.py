"""Compare already-admitted cutoff evidence offline, by release position."""

import argparse
import re
import sys
from pathlib import Path

from verify_release_positions import POSITIONS, json_object, read_text, verify


NONCE = re.compile(r"[0-9a-fA-F]{32}")
MATCH = "matches release position"


def compare_attribution(position, pin, receipt, requests):
    """Compare projections of already checked evidence."""
    nonce = receipt["scope_nonce"]
    matches = [request for request in requests if request["nonce"].lower() == nonce.lower()]
    if len(matches) != 1 or matches[0]["nonce"] != nonce:
        return "attribution gap: signed request"

    expected = {
        "position": position,
        "image_digest": pin["image"].split("@", 1)[1],
        "source_revision": pin["source_revision"],
        "worker_version": pin["worker_version"],
        "guild_id": pin["guild_id"],
        "bot_id": pin["bot_id"],
    }
    for field, value in expected.items():
        actual = matches[0].get(field)
        if not actual:
            return f"attribution gap: {field}"
        if actual != value:
            return f"attribution mismatch: {field}"
    return MATCH


def valid_nonce(value):
    return isinstance(value, str) and NONCE.fullmatch(value) is not None


def verify_handoff(pins, transcript, runner_exit_code, attribution):
    verify(pins, transcript, runner_exit_code)

    receipts = attribution.get("receipts")
    requests = attribution.get("requests")
    if not isinstance(receipts, dict) or set(receipts).difference(POSITIONS):
        raise ValueError("attribution gap: expected permission receipts by release position")
    if not isinstance(requests, list) or len(requests) > len(POSITIONS) or any(
        not isinstance(request, dict) or not valid_nonce(request.get("nonce"))
        for request in requests
    ):
        raise ValueError("attribution gap: expected signed request records with valid nonces")

    for position in POSITIONS:
        pin = pins[position]
        for field in ("worker_version", "guild_id", "bot_id"):
            value = pin.get(field)
            if not isinstance(value, str) or not value.strip():
                raise ValueError(f"{position} attribution gap: missing reviewed {field}")
        receipt = receipts.get(position)
        if (not isinstance(receipt, dict) or type(receipt.get("v")) is not int
                or receipt["v"] != 1 or not valid_nonce(receipt.get("scope_nonce"))):
            raise ValueError(f"{position} attribution gap: expected a v1 receipt with a valid nonce")
        result = compare_attribution(position, pin, receipt, requests)
        if result != MATCH:
            raise ValueError(f"{position} {result}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pins", type=Path, required=True, help="reviewed release pair JSON")
    parser.add_argument("--transcript", type=Path, required=True, help="captured rehearsal stdout")
    parser.add_argument("--runner-exit-code", type=int, required=True, help="exit status of that run")
    parser.add_argument("--attribution", type=Path, required=True, help="sanitized permission JSON")
    args = parser.parse_args()
    try:
        pins = json_object(read_text(args.pins, "selected pins", 4096), "Selected pins")
        transcript = read_text(args.transcript, "rehearsal transcript", 1024 * 1024)
        attribution = json_object(
            read_text(args.attribution, "permission attribution", 8192), "Permission attribution",
        )
        verify_handoff(pins, transcript, args.runner_exit_code, attribution)
    except ValueError as error:
        print(f"Cutoff handoff unresolved: {error}", file=sys.stderr)
        return 1
    print("Cutoff handoff evidence matches both release positions")
    return 0


if __name__ == "__main__":
    sys.exit(main())
