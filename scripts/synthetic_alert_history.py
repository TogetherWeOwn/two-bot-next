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
# Bounded reconstruction: page the workflow-runs list (GitHub caps per_page
# at 100) and read jobs newest-first until the collected suffix determines
# the incident state on its own (a checkpoint: replaying it from any prior
# state converges), the history is exhausted, or the scan bound below is
# hit. Hitting the bound means the incident state is unknown, which is a
# HistoryError so the caller keeps the documented fail-open policy instead
# of acting on a partial guess. 200 prior runs at the five-minute schedule
# is roughly sixteen hours of history.
RUNS_PAGE_SIZE = 100
MAX_HISTORY_RUNS = 200


class HistoryError(Exception):
    """A safe-to-report failure reading probe history."""


def _advance(state, outcome, delivery):
    """One chronological sample applied to (failures, incident, delivered)."""
    failures, incident, alert_delivered = state
    if outcome == "failure":
        failures += 1
        # Delivery evidence counts even below the two-failure threshold, so
        # a delivered opening alert that is the oldest retained sample (the
        # failures before it rolled out of the window) is not discarded; it
        # takes effect once the retained streak reaches incident size.
        if delivery == "success":
            alert_delivered = True
        elif delivery == "failure":
            alert_delivered = False
        if failures >= 2:
            incident = True
    else:
        failures = 0
        if incident:
            if alert_delivered and delivery == "success":
                incident = False
                alert_delivered = False
            elif not alert_delivered and delivery == "success":
                alert_delivered = True
        else:
            # A healthy sample ends any sub-threshold streak, so delivery
            # evidence carried from it must not leak into the next incident.
            alert_delivered = False
    return failures, incident, alert_delivered


def _replay(chronological, initial=(0, False, False)):
    state = initial
    for outcome, delivery in chronological:
        state = _advance(state, outcome, delivery)
    return state


def _checkpoint(outcomes):
    """Whether newest-first `outcomes` fix the incident state by themselves.

    True once replaying the suffix from every prior state (failure count
    capped at the decision-relevant 0/1/2+, incident and delivered either
    way) converges to one state: older history can then no longer change
    the decision, so reconstruction may stop before it.
    """
    chronological = list(reversed(outcomes))
    finals = {
        (min(failures, 2), incident, delivered)
        for failures, incident, delivered in (
            _replay(chronological, (count, incident, delivered))
            for count in (0, 1, 2)
            for incident in (False, True)
            for delivered in (False, True)
        )
    }
    return len(finals) == 1


def incident_state(previous):
    """Rebuild whether the recent incident was alerted and then recovered.

    Only a post-step conclusion of "success" counts as delivered; "failure"
    means undelivered. Any other conclusion ("skipped", None, cancelled)
    is a no-op and leaves incident state unchanged, so a history-read
    no-op can never close an open incident or fake a delivery.
    """
    return _replay(reversed(previous))


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


def _run_outcome(repo, run, environment, token):
    jobs = _get(f"/repos/{repo}/actions/runs/{run['id']}/jobs?per_page=100", token)
    job = next((item for item in jobs.get("jobs", [])
                if item.get("name") == JOB_NAMES[environment]), None)
    if not job:
        return None
    steps = job.get("steps", [])
    probe = next((step for step in steps
                  if step.get("name") == PROBE_STEPS[environment]), None)
    alert = next((step for step in steps
                  if step.get("name") == ALERT_STEPS[environment]), None)
    outcome = probe.get("conclusion") if probe else None
    if outcome in ("success", "failure"):
        return outcome, alert.get("conclusion") if alert else None
    return None


def previous_probe_outcomes(env, environment):
    """Newest-first prior scheduled probe samples, rebuilt page by page.

    Pages are read lazily and jobs newest-first; the scan stops early once
    the collected suffix is a confirmed checkpoint (its incident state no
    longer depends on older history, confirmed by one further sample). If
    MAX_HISTORY_RUNS prior runs are scanned with no checkpoint and more
    history may exist, the state is unknown: raise HistoryError rather than
    return a partial suffix that would silently invent one.
    """
    token = env.get("GITHUB_TOKEN")
    repository = env.get("GITHUB_REPOSITORY")
    run_id = env.get("GITHUB_RUN_ID") or env.get("RUN_ID")
    if not token or not repository or not run_id:
        raise HistoryError("workflow history context is missing")
    repo = urllib.parse.quote(repository, safe="/")
    outcomes = []
    seen = set()
    scanned = 0
    confirmed = False
    page = 1
    while scanned < MAX_HISTORY_RUNS:
        query = urllib.parse.urlencode(
            {"branch": "main", "event": "schedule", "status": "completed",
             "per_page": RUNS_PAGE_SIZE, "page": page})
        runs = _get(f"/repos/{repo}/actions/workflows/{WORKFLOW}/runs?{query}", token)
        page_runs = runs.get("workflow_runs", [])
        prior = sorted(
            (run for run in page_runs
             if str(run.get("id")) != str(run_id) and run.get("event") == "schedule"),
            key=lambda run: run.get("run_number", 0), reverse=True,
        )
        bound_hit = False
        for run in prior:
            if run.get("id") in seen:
                continue
            seen.add(run.get("id"))
            if scanned >= MAX_HISTORY_RUNS:
                bound_hit = True
                break
            scanned += 1
            sample = _run_outcome(repo, run, environment, token)
            if sample is None:
                continue
            outcomes.append(sample)
            if _checkpoint(outcomes):
                if confirmed:
                    return outcomes
                confirmed = True
            else:
                confirmed = False
        if bound_hit:
            break
        if len(page_runs) < RUNS_PAGE_SIZE:
            return outcomes
        total = runs.get("total_count")
        if isinstance(total, int) and page * RUNS_PAGE_SIZE >= total:
            return outcomes
        page += 1
    raise HistoryError(
        "synthetic workflow history is incomplete beyond the reconstruction "
        f"bound of {MAX_HISTORY_RUNS} prior runs; incident state is unknown")
