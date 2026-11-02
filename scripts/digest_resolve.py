#!/usr/bin/env python3
"""Resolve a digest's production SHA using passive /readyz or recorded Deployments."""

import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request

from build_identity import is_deploy_id_shape

SHA = re.compile(r"[0-9a-f]{40}")


def live_revision(worker_url):
    """The live /readyz build revision, or an empty string when unreadable."""
    worker_url = worker_url.strip().rstrip("/")
    try:
        parsed = urllib.parse.urlsplit(worker_url)
        if (parsed.scheme != "https" or not parsed.netloc or parsed.username
                or parsed.password or parsed.query or parsed.fragment):
            return ""
        request = urllib.request.Request(
            worker_url + "/readyz", headers={"User-Agent": "two-bot-next-daily-digest/1.0"},
        )
        with urllib.request.urlopen(request, timeout=10) as response:
            status, body = response.status, response.read()
    except urllib.error.HTTPError as error:
        if error.code != 503:
            return ""
        status, body = error.code, error.read()
    except (ValueError, OSError, urllib.error.URLError):
        return ""
    if status not in (200, 503):
        return ""
    try:
        report = json.loads(body)
    except ValueError:
        return ""
    revision = report.get("build_revision") if isinstance(report, dict) else None
    return revision if isinstance(revision, str) and SHA.fullmatch(revision) else ""


def api_json(endpoint, *fields):
    args = ["gh", "api", "--method", "GET", endpoint]
    for field in fields:
        args.extend(["-f", field])
    return json.loads(subprocess.check_output(args, text=True, timeout=30))


def newest_success_deployment(repo):
    """Ignore Actions environment rows: their SHA is not the deployed commit."""
    try:
        batch = api_json(f"repos/{repo}/deployments", "environment=production", "per_page=10")
        if not isinstance(batch, list):
            return ""
        for row in batch:
            if not isinstance(row, dict):
                return ""
            dep_id, dep_sha = row.get("id"), row.get("sha")
            if type(dep_id) is not int or not isinstance(dep_sha, str) or not SHA.fullmatch(dep_sha):
                continue
            if not is_deploy_id_shape(row.get("description")):
                continue
            statuses = api_json(f"repos/{repo}/deployments/{dep_id}/statuses", "per_page=1")
            if (isinstance(statuses, list) and statuses and isinstance(statuses[0], dict)
                    and statuses[0].get("state") == "success"):
                return dep_sha
    except (subprocess.SubprocessError, ValueError, OSError):
        return ""
    return ""


def main():
    sha = live_revision(os.environ.get("PRODUCTION_WORKER_URL", ""))
    source = "readyz"
    if not sha:
        sha = newest_success_deployment(os.environ["GITHUB_REPOSITORY"])
        source = "deployment"
    if not sha:
        print("::error::could not read the live production revision or a successful production Deployment; failing closed")
        return 1
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as out:
        out.write(f"live_sha={sha}\n")
    print(f"production SHA from {source}: {sha}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
