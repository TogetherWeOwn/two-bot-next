#!/usr/bin/env python3
"""Plan recovery from a missing same-attempt SBOM handoff on a Check PR run."""

import argparse
import json
from pathlib import Path
import re
import sys

from script_intake import decode_json, read_with_overflow


REPO = "TogetherWeOwn/two-bot-next"
CHECK = ".github/workflows/check.yml"
PRODUCER = "PR SBOM dry-run / image scan and SBOM"
VERIFIER = "PR SBOM dry-run / verify SBOM inventories and evidence"
DEPENDENTS = (VERIFIER, "ci-ok", "ci-evidence")
DOWNLOAD_STEP = "Run actions/download-artifact@"
HANDOFF = "sbom-partial-{run}-{attempt}"
MAX_DOCUMENT_BYTES = 4 * 1024 * 1024


class Refusal(ValueError):
    """A fixed diagnostic that is safe to print."""


def require(condition, message):
    if not condition:
        raise Refusal(message)


def is_id(value):
    return type(value) is int and 0 < value < 2**63


def read_snapshot(directory, name):
    try:
        with (directory / f"{name}.json").open("rb") as stream:
            raw = read_with_overflow(stream, MAX_DOCUMENT_BYTES)
        if len(raw) <= MAX_DOCUMENT_BYTES:
            return decode_json(raw, max_depth=32)
    except (OSError, ValueError, RecursionError):
        pass
    raise Refusal(f"Invalid or oversized {name} snapshot")


def rows(document, key):
    entries = document[key]
    total = document["total_count"]
    require(type(total) is int and isinstance(entries, list) and len(entries) == total
            and all(isinstance(entry, dict) for entry in entries), f"Incomplete {key} inventory")
    return entries


def current_run(pr, runs, number, head, run_id, attempt):
    require(is_id(pr["number"]) and pr["number"] == number and pr["state"] == "open" and pr["base"]["ref"] == "main"
            and pr["base"]["repo"]["full_name"] == REPO and pr["head"]["repo"]["full_name"] == REPO,
            "Expected an open same-repository PR targeting main")
    require(pr["head"]["sha"] == head, "PR head changed")
    branch = pr["head"]["ref"]
    require(isinstance(branch, str) and branch, "Missing PR head branch")

    matching = []
    for run in rows(runs, "workflow_runs"):
        require(run["event"] == "pull_request" and run["path"] == CHECK and run["head_sha"] == head
                and run["repository"]["full_name"] == REPO, "Unexpected Check run inventory scope")
        if run["head_branch"] == branch and run["head_repository"]["full_name"] == REPO:
            require(is_id(run["id"]) and is_id(run["run_number"]) and is_id(run["run_attempt"]),
                    "Invalid Check run identity")
            matching.append(run)

    selected = [run for run in matching if run["id"] == run_id]
    require(len(selected) == 1, "Check run does not match the pinned PR head")
    run = selected[0]
    require(isinstance(run["pull_requests"], list)
            and any(is_id(link["number"]) and link["number"] == number and link["head"]["sha"] == head
                    for link in run["pull_requests"]), "Check run is not linked to this PR head")
    require(run["run_attempt"] == attempt, "Check attempt changed")
    require(all(run["status"] == "completed" for run in matching), "An exact-head Check run is active")
    require(all(other is run or other["run_number"] < run["run_number"] for other in matching),
            "A newer exact-head Check run exists")
    require(run["conclusion"] == "failure", "Expected a failed Check run")


def retry_producer(document, head, run_id, attempt):
    inventory = rows(document, "jobs")
    seen_ids = set()
    retry_jobs = {}
    for job in inventory:
        job_id = job["id"]
        name = job["name"]
        job_attempt = job.get("run_attempt", 1)
        require(is_id(job_id) and job_id not in seen_ids and is_id(job["run_id"]) and job["run_id"] == run_id
                and job["head_sha"] == head and is_id(job_attempt) and job_attempt <= attempt,
                "Job identity does not match the Check run")
        require(job["status"] == "completed", "A Check job is active")
        seen_ids.add(job_id)
        if name in (PRODUCER, *DEPENDENTS):
            require(name not in retry_jobs, "Ambiguous SBOM retry job")
            retry_jobs[name] = job
        else:
            require(job["conclusion"] in ("success", "skipped"), "An unrelated Check job did not pass")

    require(len(retry_jobs) == 1 + len(DEPENDENTS), "Missing producer or dependent job")
    producer = retry_jobs[PRODUCER]
    verifier = retry_jobs[VERIFIER]
    require(producer["conclusion"] == "success"
            and all(job.get("run_attempt") == attempt and job["conclusion"] == "failure"
                    for job in (verifier, retry_jobs["ci-ok"])),
            "Expected a successful producer and failed current-attempt verifier and ci-ok")
    require(isinstance(verifier["steps"], list), "Invalid verifier steps")
    download = [step for step in verifier["steps"] if step["name"].startswith(DOWNLOAD_STEP)]
    require(len(download) == 1 and download[0]["status"] == "completed" and download[0]["conclusion"] == "failure",
            "Verifier did not fail at artifact download")
    return producer["id"]


def plan(pr, runs, jobs, artifacts, *, number, head, run_id, attempt):
    require(all(is_id(value) for value in (number, run_id, attempt)) and attempt < 2**63 - 1
            and re.fullmatch(r"[0-9a-f]{40}", head), "Expected a full head SHA and positive PR, run and attempt IDs")
    require(attempt > 1, "Expected a retry attempt")
    current_run(pr, runs, number, head, run_id, attempt)
    producer = retry_producer(jobs, head, run_id, attempt)
    missing = HANDOFF.format(run=run_id, attempt=attempt)
    expected = HANDOFF.format(run=run_id, attempt=attempt + 1)
    for artifact in rows(artifacts, "artifacts"):
        origin = artifact["workflow_run"]
        require(is_id(origin["id"]) and origin["id"] == run_id and origin["head_sha"] == head,
                "Artifact inventory belongs to another run or head")
        require(isinstance(artifact["name"], str) and artifact["name"], "Invalid artifact name")
        require(artifact["name"] != missing, "Current-attempt artifact already exists")
        require(artifact["name"] != expected, "Next-attempt artifact already exists")
    return {
        "pr": number, "head_sha": head, "run_id": run_id, "observed_attempt": attempt,
        "producer_job": PRODUCER, "producer_job_id": producer,
        "producer_job_url": f"https://github.com/{REPO}/actions/runs/{run_id}/job/{producer}",
        "dependent_jobs": DEPENDENTS, "expected_attempt": attempt + 1,
        "expected_artifact": expected,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--pr", type=int, required=True)
    parser.add_argument("--head", required=True)
    parser.add_argument("--run", type=int, required=True)
    parser.add_argument("--attempt", type=int, required=True)
    parser.add_argument("snapshot", type=Path, help="directory holding pr.json, runs.json, jobs.json, artifacts.json")
    args = parser.parse_args()
    try:
        evidence = [read_snapshot(args.snapshot, name) for name in ("pr", "runs", "jobs", "artifacts")]
        verdict = plan(*evidence, number=args.pr, head=args.head, run_id=args.run, attempt=args.attempt)
    except Refusal as exc:
        print(f"SBOM retry plan refused: {exc}", file=sys.stderr)
        return 1
    except (AttributeError, KeyError, TypeError):
        print("SBOM retry plan refused: invalid snapshot shape", file=sys.stderr)
        return 1
    print(json.dumps(verdict, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
