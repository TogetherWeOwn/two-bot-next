#!/usr/bin/env python3
"""Report overdue production promotion without changing any deployment gate.

Only recorded production deploys backed by a successful production job count;
a green workflow whose production job skipped is never promotion evidence.
--dry-run performs reads and prints the decision, but never writes an issue.
"""

import argparse
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
import json
import os
import re
import sys
import urllib.parse

import build_identity
import incident_issue
from production_hold_pending import pending_since
from production_hold_receipts import (
    AUTO_PROMOTE_DISABLED, GENERATION_FIELDS, KEY, OBSERVATION, RECOVERY,
    body, content_digest, moment, observation, recovery_body, recovery_receipt,
    sha, stale_activity, stale_observation, verified_content,
)


@dataclass(frozen=True)
class Promotion:
    revision: str
    completed_at: datetime
    run_id: str
    attempt: int
    mode: str

TITLE = "production held > 6h"
WORKFLOW = ".github/workflows/deploy-production.yml"
PRODUCTION_JOB = "wrangler deploy + readyz gate"
MAX_PAGES = 10
PAGE_SIZE = 100



def stamp(value):
    return value.strftime("%Y-%m-%dT%H:%M:%SZ")



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
        fallback = None
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
            if fallback is None:
                text = ("promotion gate not green - inspect promotion run"
                        if any(job.get("conclusion") in ("failure", "cancelled") for job in jobs)
                        else "production skipped or pending - inspect promotion run")
                fallback = text, url
        return fallback or ("no successful production promotion", None)

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
                        return Promotion(revision, completed, run_id, historical_attempt,
                                         "rollback" if payload.get("rollback") is True else "deploy")
                break
        raise incident_issue.IncidentError("no verified production promotion in bounded history")


def auto_disabled(raw):
    """Whether the raw disable setting holds, mirroring guard/recheck.

    Only the exact value "true" holds; any other value (including unset,
    empty, "TRUE" or "1") is off. Manual dispatches ignore the disable in
    the guard, and this reporter is automatic-only: a manual deploy-mode
    promotion still recovers the incident through the usual generation
    checks.
    """
    return raw == "true"


def disabled_hold(needs, raw):
    """Whether this report is an auto-promote-disabled hold.

    The incident reporter derives it from the guard/recheck skip outputs:
    only an automatic run whose guard or recheck actually applied the
    disable carries that skip. A manual dispatch never applies it (the
    guard ignores it), so re-reading the variable here would mask a
    manual run's gate failure as an intentional hold. The scheduled
    watch has no needs of its own, so only it re-reads the variable.
    """
    for job in ("guard", "recheck"):
        if needs.get(job, {}).get("outputs", {}).get("skip") == AUTO_PROMOTE_DISABLED:
            return True
    return not needs and auto_disabled(raw)


def reason(needs, disabled=False):
    # The non-expiring disable wins over every timed check, exactly as the
    # guard and pre-deploy recheck skip with it before parsing the hold.
    if disabled:
        return AUTO_PROMOTE_DISABLED
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


def decide(history, needs, now, run_url, disabled=False):
    ref = history.get("git/ref/heads/main")
    obj = ref.get("object") if isinstance(ref, dict) else None
    if not isinstance(obj, dict) or obj.get("type") != "commit":
        raise incident_issue.IncidentError("production hold main ref is unreadable")
    main = sha(obj.get("sha"))
    promotion = history.last_promotion()
    # Local metadata has no patch payload, unlike the first compare API page.
    # Missing objects (e.g. a newer main arriving after checkout) fail the report;
    # a later watcher retries rather than certifying an incomplete comparison.
    pending_at = pending_since(promotion.revision, main)
    report = {"main_sha": main, "last_promoted_sha": promotion.revision,
              "last_promoted_at": stamp(promotion.completed_at), "run_url": run_url,
              "promotion_run_id": promotion.run_id, "promotion_attempt": promotion.attempt,
              "promotion_mode": promotion.mode, "observed_at": stamp(now),
              "reason": reason(needs, disabled=disabled)}
    if pending_at is not None:
        held_since = max(promotion.completed_at, pending_at)
        report["held_since"] = stamp(held_since)
        if now - held_since > timedelta(hours=6):
            return {**report, "action": "alert"}
    # Reconcile from the latest verified serving generation, not ephemeral needs
    # from a reporter that can be lost, delayed, or superseded by a rollback.
    return {**report, "action": "recovery" if promotion.mode == "deploy" else "none"}















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
        disabled = disabled_hold(needs, env.get("AUTO_DISABLED", ""))
        report = decide(history, needs, datetime.now(timezone.utc), run_url, disabled=disabled)
        # The scheduled watch has no needs of its own: without the disable
        # it falls back to the active-run lookup, but the explicit
        # non-expiring class must not be clobbered by that generic reason.
        if not needs and report["action"] == "alert" and not disabled:
            report["reason"], pending_url = history.pending_reason(run_id)
            if pending_url:
                report["run_url"] = pending_url
        if not args.dry_run:
            if report["action"] == "alert":
                incident_issue.open_incident(env, KEY, TITLE, body(report) + "\n" + observation(report))
            elif report["action"] == "recovery":
                incident_issue.close_incident(
                    env, KEY, recovery_body(report, history.repo),
                    # Require lifecycle proof even when updated_at equals the
                    # completion cutoff. The callback retains the usual fence;
                    # other sink callers keep their unchanged default behavior.
                    quiet_since=datetime.min.replace(tzinfo=timezone.utc),
                    activity_is_stale=lambda issue: stale_activity(history, issue, report))
        print(json.dumps(report, sort_keys=True))
        return 0
    except (incident_issue.IncidentError, KeyError, TypeError, ValueError, AttributeError) as error:
        message = str(error) if isinstance(error, incident_issue.IncidentError) else "production hold input is malformed"
        print(f"::error::{message}")
        return 1


if __name__ == "__main__":
    sys.exit(main())
