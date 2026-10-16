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


class HistoryError(Exception):
    """A safe-to-report failure reading probe history."""


def consecutive_failures(outcomes):
    count = 0
    for outcome in outcomes:
        if outcome != "failure":
            break
        count += 1
    return count


def transition(current, previous, force_fail=False):
    """Return alert/recovery only at incident transitions; two failures open."""
    if current not in ("success", "failure"):
        return None
    failures = consecutive_failures(previous)
    if current == "failure":
        if force_fail or failures == 1:
            return "alert"
        return None
    return "recovery" if failures >= 2 else None


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
    target_job = "staging user-flow synthetic" if environment == "staging" else "prod passive user-flow synthetic"
    for run in prior:
        jobs = _get(f"/repos/{repo}/actions/runs/{run['id']}/jobs?per_page=100", token)
        job = next((item for item in jobs.get("jobs", []) if item.get("name") == target_job), None)
        if not job:
            continue
        probe = next((step for step in job.get("steps", [])
                      if step.get("name") == PROBE_STEPS[environment]), None)
        outcome = probe.get("conclusion") if probe else None
        if outcome in ("success", "failure"):
            outcomes.append(outcome)
            if len(outcomes) == 2:
                break
    return outcomes
