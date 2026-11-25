"""Print the reviewed current image/revision followed by the retained image/revision."""

import argparse
import sys
from pathlib import Path

from verify_release_positions import POSITIONS, read_selected_pins, validate_selected_pins


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pins", type=Path, required=True, help="sanitized reviewed release pair JSON")
    args = parser.parse_args()

    try:
        pins = read_selected_pins(args.pins)
        validate_selected_pins(pins)
        for position in POSITIONS:
            worker = pins[position].get("worker_version")
            if not isinstance(worker, str) or not worker.strip():
                raise ValueError(f"{position} requires a reviewed worker_version")
        current, retained = (pins[position] for position in POSITIONS)
        current_digest = current["image"].split("@", 1)[1]
        retained_digest = retained["image"].split("@", 1)[1]
        if current_digest == retained_digest and current["source_revision"] != retained["source_revision"]:
            raise ValueError("shared image has conflicting source revisions")
    except ValueError as error:
        print(f"Rehearsal preparation refused: {error}", file=sys.stderr)
        return 1

    print(current["image"], current["source_revision"], retained["image"], retained["source_revision"])
    return 0


if __name__ == "__main__":
    sys.exit(main())
