#!/usr/bin/env python3
"""Assemble one production watch JSON report from approved local TSV rows."""

import argparse
import json
from pathlib import Path
import sys

from script_intake import read_with_overflow
from t0_acceptance_report import (
    MAX_BYTES,
    PRODUCTION_WATCH_SCHEMA,
    WATCH_OBSERVATIONS,
    production_watch_observation,
    production_watch_window,
    read_object,
)

HEADER = "utc\tsignal\tobservation\tdisposition"


class WatchParser(argparse.ArgumentParser):
    def error(self, _message):
        self.exit(2, "production watch refused: invalid command arguments (see --help)\n")


def assemble(signal, rows_path, window_path):
    if signal not in WATCH_OBSERVATIONS:
        raise ValueError
    window, _ = read_object(window_path)
    window = production_watch_window(window)

    with rows_path.open("rb") as stream:
        raw = read_with_overflow(stream, MAX_BYTES)
    if len(raw) > MAX_BYTES:
        raise ValueError
    header, *lines = raw.decode("utf-8").removesuffix("\n").split("\n")
    # A new projection requires an explicit card row for each check, including unavailable checks.
    if header != HEADER or len(lines) != len(WATCH_OBSERVATIONS[signal]):
        raise ValueError

    rows = []
    for line in lines:
        utc, row_signal, observation, disposition = line.split("\t")
        if row_signal != signal:
            raise ValueError
        rows.append({"utc": utc, "observation": observation, "disposition": disposition})
    report = {"schema": PRODUCTION_WATCH_SCHEMA, "signal": signal, "window": window, "rows": rows}
    if production_watch_observation(signal, report, window) is None:
        raise ValueError
    output = json.dumps(report, separators=(",", ":")) + "\n"
    if len(output) > MAX_BYTES:
        raise ValueError
    return output


def main(argv=None):
    parser = WatchParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--signal", required=True, help="help or naming")
    parser.add_argument("--rows", required=True, type=Path, help="approved TSV rows for the selected signal")
    parser.add_argument("--window", required=True, type=Path, help="independently verified production serving window JSON")
    args = parser.parse_args(argv)
    try:
        output = assemble(args.signal, args.rows, args.window)
    except (OSError, RecursionError, ValueError):
        print("production watch refused: missing or invalid rows or serving window", file=sys.stderr)
        return 2
    sys.stdout.write(output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
