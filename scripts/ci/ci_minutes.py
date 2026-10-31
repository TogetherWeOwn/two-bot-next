#!/usr/bin/env python3
"""CI minutes per merged PR: baseline comparison and ledger recorder.

SDLC 2.3 needs a before/after number, not a claim. This script is the
deterministic half: pure file I/O over a checked-in baseline
(docs/ci-speed-baseline.json) and a JSONL ledger of per-PR measurements.
No network, no clock reads, no `gh` calls, so the check lane can pin it.

- `report` (default): read the baseline, attach this run's identity
  (run id, sha, event) from flags, and write a comparison skeleton whose
  `delta` stays "pending" until post-merge `record` rows exist on both
  sides of the change.
- `record`: append one measured row (pr number, wall-clock minutes for the
  PR's CI, head sha) to a ledger file. Maintainers run this after a merge
  with the minutes read from the run page; the pilot workflow uploads the
  skeleton as an artifact instead of guessing.
"""

import argparse
import json
import math
import re
import sys
from pathlib import Path

PR_NUMBER_RE = re.compile(r"[1-9][0-9]*")


def load_json(path):
    try:
        return json.loads(Path(path).read_text(encoding="utf-8"))
    except FileNotFoundError:
        raise SystemExit(f"ci minutes: file not found: {path}")
    except json.JSONDecodeError as exc:
        raise SystemExit(f"ci minutes: invalid JSON in {path}: {exc}")


def report(baseline_path, run_id, sha, event, output_path):
    baseline = load_json(baseline_path)
    if "minutes_per_merged_pr" not in baseline:
        raise SystemExit(f"ci minutes: {baseline_path} has no minutes_per_merged_pr key")
    record = {
        "baseline_minutes_per_merged_pr": baseline["minutes_per_merged_pr"],
        "baseline_source": baseline.get("source", ""),
        "this_run": {"run_id": run_id, "sha": sha, "event": event},
        "delta_minutes_per_merged_pr": "pending",
        "note": "record post-merge rows on both sides, then compare ledger means",
    }
    Path(output_path).write_text(json.dumps(record, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"ci minutes: baseline {baseline['minutes_per_merged_pr']} min/merged PR; delta pending -> {output_path}")
    return 0


def record(ledger_path, pr, minutes, sha):
    try:
        value = float(minutes)
    except (TypeError, ValueError):
        raise SystemExit(f"ci minutes: minutes must be numeric, got {minutes!r}")
    if not math.isfinite(value):
        raise SystemExit(f"ci minutes: minutes must be finite, got {minutes!r}")
    if value < 0:
        raise SystemExit("ci minutes: minutes must not be negative")
    pr_text = str(pr)
    if PR_NUMBER_RE.fullmatch(pr_text) is None:
        raise SystemExit(f"ci minutes: pr must be a positive integer, got {pr!r}")
    row = {"pr": int(pr_text), "minutes": value, "sha": sha}
    with Path(ledger_path).open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(row, sort_keys=True) + "\n")
    print(f"ci minutes: recorded PR #{pr} {value} min -> {ledger_path}")
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command")
    rep = sub.add_parser("report", help="write a baseline comparison skeleton")
    rep.add_argument("--baseline", required=True)
    rep.add_argument("--run-id", required=True)
    rep.add_argument("--sha", required=True)
    rep.add_argument("--event", required=True)
    rep.add_argument("--output", required=True)
    rec = sub.add_parser("record", help="append one measured row to a ledger")
    rec.add_argument("--ledger", required=True)
    rec.add_argument("--pr", required=True)
    rec.add_argument("--minutes", required=True)
    rec.add_argument("--sha", required=True)
    args = parser.parse_args(argv)
    if args.command == "record":
        return record(args.ledger, args.pr, args.minutes, args.sha)
    if args.command is None:
        parser.print_usage(sys.stderr)
        return 2
    return report(args.baseline, args.run_id, args.sha, args.event, args.output)


if __name__ == "__main__":
    raise SystemExit(main())
