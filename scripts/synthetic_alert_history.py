#!/usr/bin/env python3
"""Decide synthetic alert transitions from this workflow's recent probe runs."""

import json
import os
import urllib.error
import urllib.parse
import urllib.request

API = "https://api.github.com"
WORKFLOW = "synthetic-user-flow.yml"
PROBE_STEPS = {
    "staging": "Run the staging user-flow probe",
    "prod": "Run the passive production probe",
}
ALERT_STEPS = {
    "staging": "Alert the ops channel on incident transition",
    "prod": "Alert the ops channel on incident transition",
}
# Decide step (writes event to GITHUB_OUTPUT, never posts). The post step
# above is the only delivery signal: success means delivered, failure means
# undelivered, skipped/None means no-op and must not move incident state.
DECIDE_STEPS = {
    "staging": "Decide the synthetic alert event",
    "prod": "Decide the synthetic alert event",
}
JOB_NAMES = {
    "staging": "staging user-flow synthetic",
    "prod": "prod passive user-flow synthetic",
}


class HistoryError(Exception):
    """A safe-to-report failure reading probe history."""


def incident_state(previous):
    """Rebuild whether the recent incident was alerted and then recovered.

    Only a post-step conclusion of "success" counts as delivered; "failure"
    means undelivered. Any other conclusion ("skipped", None, cancelled)
    is a no-op and leaves incident state unchanged, so a history-read
    no-op can never close an open incident or fake a delivery.
    """
    failures = 0
    incident = False
    alert_delivered = False
    for outcome, delivery in reversed(previous):
        if outcome == "failure":
            failures += 1
            if failures >= 2:
                incident = True
                if delivery == "success":
                    alert_delivered = True
                elif delivery == "failure":
                    alert_delivered = False
        else:
            failures = 0
            if incident:
                if alert_delivered and delivery == "success":
                    incident = False
                    alert_delivered = False
                elif not alert_delivered and delivery == "success":
                    alert_delivered = True
    return failures, incident, alert_delivered


def transition(current, previous, force_fail=False):
    """Return incident transitions, retrying alerts that did not deliver."""
    if current not in ("success", "failure"):
        return None
    failures, incident, alert_delivered = incident_state(previous)
    if current == "failure":
        if force_fail or (not incident and failures >= 1) or (incident and not alert_delivered):
            return "alert"
        return None
    if incident:
        return "recovery" if alert_delivered else "alert"
    return None


def _get(path, token):
    request = urllib.request.Request(
        API + path,
        headers={"Accept": "application/vnd.github+json",
                 "Authorization": "Bearer " + token,
                 "X-GitHub-Api-Version": "2022-11-28",
                 "User-Agent": "two-bot-next-synthetic-alert/1"},
    )
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            return json.loads(response.read(1 << 20))
    except (OSError, ValueError, urllib.error.HTTPError) as error:
        raise HistoryError("could not read recent synthetic workflow history ("
                           + error.__class__.__name__ + ")") from None


def previous_probe_outcomes(env, environment):
    token = env.get("GITHUB_TOKEN")
    repository = env.get("GITHUB_REPOSITORY")
    run_id = env.get("GITHUB_RUN_ID") or env.get("RUN_ID")
    if not token or not repository or not run_id:
        raise HistoryError("workflow history context is missing")
    repo = urllib.parse.quote(repository, safe="/")
    query = urllib.parse.urlencode({"branch": "main", "event": "schedule",
                                    "status": "completed", "per_page": 10})
    runs = _get(f"/repos/{repo}/actions/workflows/{WORKFLOW}/runs?{query}", token)
    prior = sorted(
        (run for run in runs.get("workflow_runs", [])
         if str(run.get("id")) != str(run_id) and run.get("event") == "schedule"),
        key=lambda run: run.get("run_number", 0), reverse=True,
    )
    outcomes = []
    for run in prior:
        jobs = _get(f"/repos/{repo}/actions/runs/{run['id']}/jobs?per_page=100", token)
        job = next((item for item in jobs.get("jobs", [])
                    if item.get("name") == JOB_NAMES[environment]), None)
        if not job:
            continue
        steps = job.get("steps", [])
        probe = next((step for step in steps
                      if step.get("name") == PROBE_STEPS[environment]), None)
        alert = next((step for step in steps
                      if step.get("name") == ALERT_STEPS[environment]), None)
        outcome = probe.get("conclusion") if probe else None
        if outcome in ("success", "failure"):
            outcomes.append((outcome, alert.get("conclusion") if alert else None))
    return outcomes
