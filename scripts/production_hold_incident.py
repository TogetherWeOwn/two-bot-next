#!/usr/bin/env python3
"""Report overdue production promotion without changing any deployment gate.

Only recorded production deploys backed by a successful production job count;
a green workflow whose production job skipped is never promotion evidence.
--dry-run performs reads and prints the decision, but never writes an issue.
"""

import argparse
from datetime import datetime, timedelta, timezone
import json
import os
import re
import sys
import urllib.parse

import build_identity
import incident_issue

KEY = "production-held"
TITLE = "production held > 6h"
WORKFLOW = ".github/workflows/deploy-production.yml"
PRODUCTION_JOB = "wrangler deploy + readyz gate"
MAX_PAGES = 10
PAGE_SIZE = 100


def moment(raw):
    try:
        value = datetime.fromisoformat(raw.replace("Z", "+00:00"))
        if value.tzinfo is not None:
            return value.astimezone(timezone.utc)
    except (AttributeError, TypeError, ValueError):
        pass
    raise incident_issue.IncidentError("production hold timestamp is unreadable")


def stamp(value):
    return value.strftime("%Y-%m-%dT%H:%M:%SZ")


def sha(value):
    if not isinstance(value, str) or not re.fullmatch(r"[0-9a-f]{40}", value):
        raise incident_issue.IncidentError("production hold revision is unreadable")
    return value


class History:
    def __init__(self, env):
        self.token, self.repo = incident_issue._context(env)
        self.requests = 0

    def get(self, path, **query):
        self.requests += 1
        if self.requests > 100:
            raise incident_issue.IncidentError("production hold history exceeds the request cap")
        suffix = "?" + urllib.parse.urlencode(query) if query else ""
        return incident_issue._request("GET", f"/repos/{self.repo}/{path}{suffix}", self.token)

    def pages(self, path, key=None, **query):
        for page in range(1, MAX_PAGES + 1):
            data = self.get(path, per_page=PAGE_SIZE, page=page, **query)
            rows = data.get(key) if key and isinstance(data, dict) else data
            if not isinstance(rows, list) or len(rows) > PAGE_SIZE or any(
                    not isinstance(row, dict) for row in rows):
                raise incident_issue.IncidentError("production hold history is malformed")
            yield from rows
            if len(rows) < PAGE_SIZE:
                return
        raise incident_issue.IncidentError("production hold history exceeds the page cap")

    def last_promotion(self):
        for row in self.pages("deployments", environment="production"):
            payload = row.get("payload")
            if (row.get("environment") != "production" or not isinstance(payload, dict)
                    or payload.get("rollback") is True
                    or not build_identity.is_deploy_id_shape(payload.get("deploy_id"))):
                continue
            revision = sha(row.get("sha"))
            if payload["deploy_id"].split("+")[-1] != revision[:7]:
                raise incident_issue.IncidentError("production hold deployment identity mismatch")
            deployment_id = row.get("id")
            if type(deployment_id) is not int or deployment_id <= 0:
                raise incident_issue.IncidentError("production hold deployment is malformed")
            # Older successes may now be inactive. Inspect status history, not
            # only the latest status; a later deploy auto-inactivates its predecessor.
            for status in self.pages(f"deployments/{deployment_id}/statuses"):
                if status.get("state") != "success":
                    continue
                match = re.fullmatch(
                    rf"https://github\.com/{re.escape(self.repo)}/actions/runs/([1-9][0-9]*)",
                    status.get("log_url") or "")
                if not match:
                    continue
                run_id = match[1]
                run = self.get(f"actions/runs/{run_id}")
                if (not isinstance(run, dict) or run.get("head_branch") != "main"
                        or run.get("path", "").split("@")[0] != WORKFLOW
                        or (run.get("repository") or {}).get("full_name") != self.repo
                        or (run.get("head_repository") or {}).get("full_name") != self.repo):
                    raise incident_issue.IncidentError("production hold run provenance is malformed")
                attempt = run.get("run_attempt")
                if type(attempt) is not int or attempt <= 0:
                    raise incident_issue.IncidentError("production hold run attempt is malformed")
                jobs = list(self.pages(f"actions/runs/{run_id}/attempts/{attempt}/jobs", key="jobs"))
                production = [job for job in jobs if job.get("name") == PRODUCTION_JOB]
                if (len(production) == 1 and production[0].get("status") == "completed"
                        and production[0].get("conclusion") == "success"):
                    return revision, moment(production[0].get("completed_at"))
                # A record precedes the post-deploy watch: a failed watch or
                # a skipped job on a rerun must not reset the hold clock.
                break
        raise incident_issue.IncidentError("no verified production promotion in bounded history")


def reason(needs):
    for job in ("guard", "staging-soak-evidence", "soak", "recheck"):
        skip = needs.get(job, {}).get("outputs", {}).get("skip")
        if skip:
            # Guard strings include a hold expiry; never publish arbitrary logs.
            if re.fullmatch(r"[A-Za-z0-9 :._+\-]{1,160}", skip):
                return skip
            return job + " skipped"
    failed = [job for job in ("guard", "schema-gate", "roles-gate", "soak",
                              "staging-soak-evidence", "recheck", "production")
              if needs.get(job, {}).get("result") in ("failure", "cancelled")]
    return ", ".join(failed) + " not green" if failed else "no successful production promotion"


def decide(history, needs, now, run_url):
    main = sha(history.get("commits/main").get("sha"))
    promoted, promoted_at = history.last_promotion()
    comparison = history.get(f"compare/{promoted}...{main}", per_page=1)
    state = comparison.get("status")
    if state not in ("ahead", "identical"):
        raise incident_issue.IncidentError("production hold main ancestry is unreadable")
    report = {"main_sha": main, "last_promoted_sha": promoted,
              "last_promoted_at": stamp(promoted_at), "run_url": run_url,
              "reason": reason(needs)}
    if (needs.get("production", {}).get("result") == "success"
            and needs.get("guard", {}).get("outputs", {}).get("mode") == "deploy"):
        return {**report, "action": "recovery"}
    if state == "identical":
        return {**report, "action": "none"}
    commits = comparison.get("commits")
    if not isinstance(commits, list) or not commits:
        raise incident_issue.IncidentError("production hold pending commit is unreadable")
    # Main uses squash merges. The first unpromoted commit's committer time
    # is its main integration time, not the author date of work on a branch.
    pending_at = moment(commits[0].get("commit", {}).get("committer", {}).get("date"))
    held_since = max(promoted_at, pending_at)
    return {**report, "held_since": stamp(held_since),
            "action": "alert" if now - held_since > timedelta(hours=6) else "none"}


def body(report):
    return (f"- Held since: {report.get('held_since', 'recovered')}\n"
            f"- Last promoted SHA: `{report['last_promoted_sha']}`\n"
            f"- Last promotion completed: {report['last_promoted_at']}\n"
            f"- Current main SHA: `{report['main_sha']}`\n"
            f"- Skip/failure reason: {report['reason']}\n"
            f"- Promotion run: {report['run_url']}\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    env = dict(os.environ)
    try:
        needs = json.loads(env["NEEDS_JSON"], object_pairs_hook=incident_issue._reject_duplicate_keys)
        if not isinstance(needs, dict):
            raise ValueError()
        run_id = env["GITHUB_RUN_ID"]
        if not re.fullmatch(r"[1-9][0-9]*", run_id):
            raise ValueError()
        history = History(env)
        run_url = f"https://github.com/{history.repo}/actions/runs/{run_id}"
        report = decide(history, needs, datetime.now(timezone.utc), run_url)
        if not args.dry_run:
            if report["action"] == "alert":
                incident_issue.open_incident(env, KEY, TITLE, body(report))
            elif report["action"] == "recovery":
                incident_issue.close_incident(env, KEY, "Production promotion recovered.\n\n" + body(report),
                                              quiet_since=moment(report["last_promoted_at"]))
        print(json.dumps(report, sort_keys=True))
        return 0
    except (incident_issue.IncidentError, KeyError, TypeError, ValueError, AttributeError) as error:
        message = str(error) if isinstance(error, incident_issue.IncidentError) else "production hold input is malformed"
        print(f"::error::{message}")
        return 1


if __name__ == "__main__":
    sys.exit(main())
