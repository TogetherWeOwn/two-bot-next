#!/usr/bin/env python3
"""Check saved, deploy-called Help evidence against the intended rollout.

Use evidence.json from the trusted intended rollout as the pin. Supply the
original Help report and start JSONL from authenticated artifacts of that same
protected deploy attempt. This checks their consistency offline. The report
has no embedded run ID, so its original filename is part of the binding, and
local files alone cannot authenticate their origin. A pass covers the staging
test-bot readbacks and message deletion acknowledgements.
"""

import argparse
from pathlib import Path
import re
import sys

from build_identity import (
    is_build_id_shape,
    is_build_revision_shape,
    is_deploy_id_shape,
)
from script_intake import decode_json, read_with_overflow
from staging_automation_read_smoke import is_snowflake
from staging_help_embed_smoke import EMBED_KEYS, SCHEMA
from staging_rollout import UUID


CAP = 65536
WINDOW_SCHEMA = "two-bot-staging-use.v1"
READBACK_AND_DELETE = "read-back branding, text and fields match; message deleted"


class Refused(ValueError):
    """A fixed diagnostic for a refused evidence bundle."""


def require(condition, reason):
    if not condition:
        raise Refused(reason)


def read(path):
    with path.open("rb") as stream:
        raw = read_with_overflow(stream, CAP)
    require(len(raw) <= CAP, "evidence exceeds the size cap")
    return raw


def object_from_json(raw):
    row = decode_json(raw.decode("utf-8"), max_depth=16)
    require(isinstance(row, dict), "evidence must contain JSON objects")
    return row


def positive_safe_integer(value):
    return type(value) is int and 0 < value < 2**53


def rollout_identity(rollout):
    sha, build, deploy, worker = (rollout.get(key) for key in
                                  ("revision", "build_id", "deploy_id", "worker_version"))
    require(is_build_revision_shape(sha) and is_build_id_shape(build)
            and is_deploy_id_shape(deploy) and deploy.endswith("+" + sha[:7])
            and isinstance(worker, str) and re.fullmatch(UUID, worker)
            and all(type(rollout.get(key)) is int and rollout[key] == 200
                    for key in ("health", "readyz")), "intended rollout is incomplete")
    run, attempt = build.split("-")
    # The reusable Help workflow refuses a deploy-called rerun. The final
    # digit in the report filename counts Discord retries, not run attempts.
    require(run[0] != "0" and attempt == "1", "intended rollout is not a first attempt")
    return sha, worker, run


def check_window(raw, sha, worker, run):
    lines = raw.splitlines()
    require(len(lines) == 2, "Help start receipt is incomplete")
    attempt, ready = map(object_from_json, lines)
    expiry = ready.get("expiresAt")
    require(positive_safe_integer(expiry) and all(
        row.get("schema") == WINDOW_SCHEMA and row.get("environment") == "staging"
        and row.get("runId") == run and type(row.get("runAttempt")) is int
        and row["runAttempt"] == 1 and type(row.get("expiresAt")) is int
        and row["expiresAt"] == expiry for row in (attempt, ready)
    ), "Help start receipt names a different run or window")
    epoch = ready.get("epoch")
    require(attempt.get("action") == "attempt" and ready.get("action") == "ready"
            and ready.get("running") is True and positive_safe_integer(epoch),
            "Help start receipt did not confirm ready")
    binding = {"sha": sha, "deploymentId": worker, "epoch": epoch}
    use = ready.get("use")
    require(all(ready.get(key) == value for key, value in binding.items())
            and isinstance(use, dict)
            and all(use.get(key) == value and type(use[key]) is type(value)
                    for key, value in {**binding, "environment": "staging", "runId": run,
                                       "runAttempt": 1, "expiresAt": expiry, "phase": "active"}.items()),
            "Help start receipt names a different serving deployment")


def check_report(report):
    require(report.get("schema") == SCHEMA and report.get("verdict") == "PASS"
            and "refusal" not in report, "Help report did not pass with the current schema")
    rows = report.get("rows")
    require(isinstance(rows, list) and len(rows) == len(EMBED_KEYS),
            "Help report has an incomplete readback inventory")
    message_ids = set()
    for name, row in zip(EMBED_KEYS, rows):
        require(isinstance(row, dict) and row.get("name") == name
                and row.get("result") == "pass" and row.get("actual") == READBACK_AND_DELETE
                and "cleanup_failed" not in row and is_snowflake(row.get("message_id")),
                "Help report is missing a readback or deletion acknowledgement")
        message_ids.add(row["message_id"])
    require(len(message_ids) == len(rows), "Help report repeats a message ID")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rollout", required=True, type=Path, help="intended rollout evidence.json")
    parser.add_argument("--start", required=True, type=Path, help="deploy-called Help start JSONL")
    parser.add_argument("--report", required=True, type=Path, help="Help JSON with its original filename")
    args = parser.parse_args(argv)
    try:
        sha, worker, run = rollout_identity(object_from_json(read(args.rollout)))
        require(re.fullmatch(rf"help-embed-synthetic-{run}-[1-3]\.json", args.report.name),
                "Help report filename names a different run")
        check_window(read(args.start), sha, worker, run)
        check_report(object_from_json(read(args.report)))
    except Refused as error:
        print(f"REFUSED {error}", file=sys.stderr)
        return 1
    except (OSError, ValueError, RecursionError):
        print("REFUSED evidence is unreadable or invalid bounded JSON", file=sys.stderr)
        return 1
    print("PASS staging Help readbacks and message deletions match the intended rollout")
    return 0


if __name__ == "__main__":
    sys.exit(main())
