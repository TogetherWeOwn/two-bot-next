#!/usr/bin/env python3
"""Compare operator-captured advisory evidence without contacting GitHub."""

import argparse
from pathlib import Path
import sys

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(SCRIPTS), str(SCRIPTS.parent / ".github/scripts")]
import pr_evidence as evidence
from script_intake import decode_json, read_with_overflow

MAX_CAPTURE = 1024 * 1024


def load(path):
    with path.open("rb") as stream:
        raw = read_with_overflow(stream, MAX_CAPTURE)
    if len(raw) > MAX_CAPTURE:
        raise ValueError("Capture exceeds the size limit.")
    return decode_json(raw)


class CapturedPR:
    def __init__(self, pr):
        self.pr = pr

    def get(self, _path):
        return self.pr


def hosted_result(capture, verdict):
    run, job = capture.get("run"), capture.get("job")
    if run is None and job is None:
        return "unavailable"
    if (run["repository"]["full_name"], run["head_sha"], run["event"], run["path"]) != (
            verdict.repo, verdict.head, "pull_request", ".github/workflows/pr-evidence.yml"):
        raise ValueError("Unexpected advisory run.")
    if not any(type(pr["number"]) is int and pr["number"] == verdict.number
               and pr["head"]["sha"] == verdict.head for pr in run["pull_requests"]):
        raise ValueError("Missing PR association.")
    if (job["name"], job["run_id"], job["run_attempt"], job["head_sha"]) != (
            "pr-evidence", run["id"], run["run_attempt"], verdict.head):
        raise ValueError("Unexpected advisory job.")
    if not all(type(value) is int and value > 0 for value in (
            run["id"], run["run_attempt"], job["run_id"], job["run_attempt"], job["id"])):
        raise ValueError("Invalid advisory identity.")
    if run["status"] != "completed" or job["status"] != "completed":
        return "unavailable"
    verify, = [step for step in job["steps"] if step["name"] == "Verify and publish evidence"]
    result = verify["conclusion"]
    if verify["status"] == "completed" and result in ("success", "failure") and (
            run["conclusion"] == job["conclusion"] == result):
        return result
    return "unavailable"


def compare(before, current):
    pr = current["pr"]
    identity = (pr["number"], pr["base"]["repo"]["full_name"], pr["base"]["sha"], pr["head"]["sha"])
    old, _ = evidence.current_snapshot(CapturedPR(before["pr"]), *identity)
    now, _ = evidence.current_snapshot(CapturedPR(pr), *identity)
    target = pr["base"]["ref"]
    if not isinstance(target, str) or not target or before["pr"]["base"]["ref"] != target:
        raise ValueError("Expected the same target ref.")
    if hosted_result(before, old) != "success":
        raise ValueError("Expected a matching prior green capture.")
    if current.get("run") is not None and current["run"]["id"] == before["run"]["id"]:
        raise ValueError("Expected a distinct advisory run after the edit.")
    return hosted_result(current, now), old != now


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_subparsers(dest="mode", required=True)
    pr = modes.add_parser("pr", help="compare a prior green capture with a current PR capture")
    pr.add_argument("before", type=Path)
    pr.add_argument("current", type=Path)
    group = modes.add_parser("group", help="inspect a merge_group checks_requested payload")
    group.add_argument("event", type=Path)
    args = parser.parse_args()
    try:
        if args.mode == "group":
            event = load(args.event)
            if event["action"] != "checks_requested" or not all(
                    evidence.SHA.fullmatch(event["merge_group"][key]) for key in ("head_sha", "base_sha")):
                raise ValueError("Invalid merge-group event.")
            print("merge_group=unavailable (no authoritative PR membership)")
            return 2
        current, stale = compare(load(args.before), load(args.current))
    except (OSError, ValueError, TypeError, KeyError, AttributeError, RecursionError):
        print("Invalid or unmatched evidence capture.", file=sys.stderr)
        return 2
    print(f"current_advisory={current}")
    print(f"prior_hosted={'stale_green' if stale else 'green_same_description'}")
    return {"success": 0, "failure": 1, "unavailable": 2}[current]


if __name__ == "__main__":
    sys.exit(main())
