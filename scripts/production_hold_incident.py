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

    def pending_reason(self, current_run_id):
        data = self.get("actions/workflows/deploy-production.yml/runs", branch="main", per_page=20)
        runs = data.get("workflow_runs") if isinstance(data, dict) else None
        if not isinstance(runs, list) or len(runs) > 20:
            raise incident_issue.IncidentError("production hold active runs are malformed")
        for run in runs:
            if not isinstance(run, dict) or type(run.get("id")) is not int or run["id"] <= 0:
                raise incident_issue.IncidentError("production hold active run is malformed")
            if str(run["id"]) == current_run_id:
                continue
            attempt = run.get("run_attempt")
            if type(attempt) is not int or attempt <= 0:
                raise incident_issue.IncidentError("production hold active attempt is malformed")
            jobs = list(self.pages(f"actions/runs/{run['id']}/attempts/{attempt}/jobs", key="jobs"))
            url = f"https://github.com/{self.repo}/actions/runs/{run['id']}"
            for job in jobs:
                if job.get("name") in (PRODUCTION_JOB, "wrangler rollback + readyz gate"):
                    state = job.get("status")
                    if state in ("waiting", "queued", "pending", "requested", "in_progress"):
                        return f"production {state}", url
            if any(job.get("conclusion") in ("failure", "cancelled") for job in jobs):
                return "promotion gate not green - inspect promotion run", url
            return "production skipped or pending - inspect promotion run", url
        return "no successful production promotion", None

    def last_promotion(self):
        for row in self.pages("deployments", environment="production"):
            payload = row.get("payload")
            if (row.get("environment") != "production" or not isinstance(payload, dict)
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
                # The deployment status is written inside its producing job,
                # before the post-deploy watch. A later rerun must not erase
                # that historical success or lend a failed attempt its verdict.
                recorded_at = moment(status.get("created_at"))
                expected_job = ("wrangler rollback + readyz gate"
                                if payload.get("rollback") is True else PRODUCTION_JOB)
                if attempt > 50:
                    raise incident_issue.IncidentError("production hold attempt history exceeds the cap")
                for historical_attempt in range(attempt, 0, -1):
                    jobs = list(self.pages(
                        f"actions/runs/{run_id}/attempts/{historical_attempt}/jobs", key="jobs"))
                    production = [job for job in jobs if job.get("name") == expected_job]
                    if (len(production) != 1 or production[0].get("status") != "completed"
                            or production[0].get("conclusion") != "success"):
                        continue
                    started = moment(production[0].get("started_at"))
                    completed = moment(production[0].get("completed_at"))
                    if started <= recorded_at <= completed:
                        # A successful rollback is the restored serving baseline,
                        # not recovery; only a deploy-mode result closes an issue.
                        return revision, completed
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
        if not needs and report["action"] == "alert":
            report["reason"], pending_url = history.pending_reason(run_id)
            if pending_url:
                report["run_url"] = pending_url
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
