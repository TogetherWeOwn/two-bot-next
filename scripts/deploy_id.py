#!/usr/bin/env python3
"""Deploy IDs (`YYYY.MM.DD.N+sha7`) for staging/production deploys.

`next` derives N deterministically: one plus the count of that environment's
recorded deploy IDs created on today's UTC date (only Deployment rows whose
description is a deploy ID count; Actions' automatic `environment:` rows
carry none). The deploy lanes serialize
deploys (the deploy-staging workflow concurrency, the deploy-production lane),
so the count cannot race; the listing stops at the first row older than the
date (the API returns newest first), and a same-day flood past the page cap
refuses instead of minting a duplicate N.

`record` stores the deploy as a GitHub Deployment carrying the ID — never a
tag or Release — and posts its success/failure status. Both subcommands speak
only through `gh api` (GH_TOKEN in the workflow); API errors and unexpected
shapes fail closed with fixed vocabulary, never echoed bodies.
"""

import argparse
from datetime import datetime, timezone
import json
import os
import subprocess
from pathlib import Path

from build_identity import (
    count_deployments_for_date,
    deployment_utc_date,
    format_deploy_id,
    is_build_revision_shape,
    is_deploy_id_shape,
    parse_deploy_id,
)

ENVIRONMENTS = ("staging", "production")
DEPLOYMENTS_PER_PAGE = 100
# Six pages (600 rows) bound one day's count: each deploy takes minutes, so
# a fuller day is a miscount, not a deploy rate, and refuses instead of
# minting a duplicate N. Total history is unbounded: the listing stops at
# the first row older than the date (the API returns newest first).
DEPLOYMENTS_MAX_PAGES = 6


class DeployIdError(Exception):
    """Fixed diagnostic class only; never constructed from upstream text."""


def _gh_api(args, body=None):
    """One `gh api` call; the parsed JSON, or a fixed-vocabulary refusal."""
    try:
        result = subprocess.run(
            ["gh", "api", *args],
            input=body,
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError):
        raise DeployIdError("github api unavailable") from None
    if result.returncode != 0:
        raise DeployIdError("github api refused the deployments call")
    try:
        return json.loads(result.stdout)
    except ValueError:
        raise DeployIdError("github api returned invalid JSON") from None


def list_deployments(repo, environment, date_text):
    """Deployment rows for an environment down to a UTC `YYYY.MM.DD` date.

    The API returns newest first, so the listing stops at the first row
    older than the date: total history never matters, only the day. Rows
    newer than the date (clock skew) are kept but never counted. Past the
    page cap without reaching an older row, the day itself is too full to
    count and refuses instead of minting a duplicate N.
    """
    try:
        want = datetime.strptime(date_text, "%Y.%m.%d").date()
    except ValueError:
        raise DeployIdError("date must be a real YYYY.MM.DD calendar date") from None
    rows = []
    for page in range(1, DEPLOYMENTS_MAX_PAGES + 1):
        batch = _gh_api([
            "--method", "GET", f"repos/{repo}/deployments",
            "-f", f"environment={environment}",
            "-f", f"per_page={DEPLOYMENTS_PER_PAGE}",
            "-f", f"page={page}",
        ])
        if not isinstance(batch, list):
            raise DeployIdError("unexpected deployments page")
        for row in batch:
            try:
                day = deployment_utc_date(row)
            except ValueError as error:
                raise DeployIdError(str(error)) from None
            if day < want:
                return rows
            rows.append(row)
        if len(batch) < DEPLOYMENTS_PER_PAGE:
            return rows
    raise DeployIdError("too many deployments to count this date")


def next_deploy_id(deployments, date_text, sha):
    """The next ID for a date and full SHA from already-fetched rows."""
    if not is_build_revision_shape(sha):
        raise DeployIdError("needs a 40-hex SHA")
    try:
        n = count_deployments_for_date(deployments, date_text) + 1
        return format_deploy_id(date_text, n, sha[:7])
    except ValueError as error:
        raise DeployIdError(str(error)) from None


def deployment_request(environment, sha, deploy_id, rollback=False):
    """The create-deployment body: ref + environment, carrying the deploy ID.

    The description carries the ID for humans, the payload for machines; a
    rollback record additionally names itself so the event ID (fresh) is never
    mistaken for the restored build's stamped ID. `production_environment`
    marks production serving state on GitHub's dashboard only.
    """
    if environment not in ENVIRONMENTS:
        raise DeployIdError("environment must be staging or production")
    if not is_build_revision_shape(sha):
        raise DeployIdError("needs a 40-hex SHA")
    if not is_deploy_id_shape(deploy_id):
        raise DeployIdError("needs a canonical deploy ID")
    try:
        _date_text, _n, sha7 = parse_deploy_id(deploy_id)
    except ValueError:
        raise DeployIdError("needs a canonical deploy ID") from None
    if sha7 != sha[:7]:
        raise DeployIdError("deploy ID names a different commit")
    payload = {"deploy_id": deploy_id}
    if rollback:
        payload["rollback"] = True
    return {
        "ref": sha,
        "task": "deploy",
        "environment": environment,
        "production_environment": environment == "production",
        "auto_merge": False,
        "required_contexts": [],
        "description": deploy_id,
        "payload": payload,
    }


def deployment_status_request(state, deploy_id, log_url=None, environment_url=None):
    """The deployment-status body for a recorded deploy ID."""
    if state not in ("success", "failure"):
        raise DeployIdError("status must be success or failure")
    if not is_deploy_id_shape(deploy_id):
        raise DeployIdError("needs a canonical deploy ID")
    body = {"state": state, "description": deploy_id}
    if log_url is not None:
        body["log_url"] = log_url
    if environment_url is not None:
        body["environment_url"] = environment_url
    return body


def record_deployment(repo, environment, sha, deploy_id, state,
                      rollback=False, log_url=None, environment_url=None):
    """Create the Deployment and post its status; returns the deployment id."""
    created = _gh_api(
        ["--method", "POST", f"repos/{repo}/deployments", "--input", "-"],
        json.dumps(deployment_request(environment, sha, deploy_id, rollback)),
    )
    if not isinstance(created, dict) or type(created.get("id")) is not int:
        raise DeployIdError("deployment record unreadable")
    _gh_api(
        ["--method", "POST",
         f"repos/{repo}/deployments/{created['id']}/statuses", "--input", "-"],
        json.dumps(deployment_status_request(state, deploy_id, log_url, environment_url)),
    )
    return created["id"]


def _repo():
    # The runner exports GITHUB_REPOSITORY; GH_REPO is the `gh` CLI spelling.
    repo = os.environ.get("GITHUB_REPOSITORY", "") or os.environ.get("GH_REPO", "")
    if not repo or "/" not in repo:
        raise DeployIdError("needs GITHUB_REPOSITORY owner/name")
    return repo


def cmd_next(args):
    if args.environment not in ENVIRONMENTS:
        raise DeployIdError("environment must be staging or production")
    date_text = args.date or datetime.now(timezone.utc).strftime("%Y.%m.%d")
    if args.deployments_json is not None:
        try:
            deployments = json.loads(Path(args.deployments_json).read_text())
        except (OSError, ValueError):
            raise DeployIdError("deployments fixture unreadable") from None
        if not isinstance(deployments, list):
            raise DeployIdError("deployments fixture is not a list")
    else:
        deployments = list_deployments(_repo(), args.environment, date_text)
    deploy_id = next_deploy_id(deployments, date_text, args.sha)
    # The single stdout line is a GITHUB_ENV assignment; the workflow appends
    # it, humans read it directly.
    print(f"DEPLOY_ID={deploy_id}")
    return 0


def cmd_record(args):
    deployment_id = record_deployment(
        _repo(), args.environment, args.sha, args.deploy_id, args.state,
        rollback=args.rollback, log_url=args.run_url,
        environment_url=args.environment_url,
    )
    print(f"deployment_id={deployment_id} state={args.state}")
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    commands = parser.add_subparsers(dest="command", required=True)

    next_cmd = commands.add_parser("next", help="compute the next deploy ID for an environment")
    next_cmd.add_argument("--environment", required=True)
    next_cmd.add_argument("--sha", required=True)
    next_cmd.add_argument("--date", default=None,
                          help="UTC YYYY.MM.DD override (default today; tests only)")
    next_cmd.add_argument("--deployments-json", default=None,
                          help="offline deployments list (default: read gh api)")

    record_cmd = commands.add_parser("record", help="record the deploy as a GitHub Deployment")
    record_cmd.add_argument("--environment", required=True)
    record_cmd.add_argument("--sha", required=True)
    record_cmd.add_argument("--deploy-id", required=True)
    record_cmd.add_argument("--state", choices=("success", "failure"), required=True)
    record_cmd.add_argument("--rollback", action="store_true",
                            help="the record restores an older build, not a fresh stamp")
    record_cmd.add_argument("--run-url", default=None)
    record_cmd.add_argument("--environment-url", default=None)

    args = parser.parse_args(argv)
    try:
        if args.command == "next":
            return cmd_next(args)
        return cmd_record(args)
    except DeployIdError as error:
        print(f"deploy_id failed: {error}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
