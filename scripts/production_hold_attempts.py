"""Inert verifier for a historical workflow-job receipt."""

import incident_issue
from production_hold_receipts import moment

WORKFLOW = ".github/workflows/deploy-production.yml"


def verified_attempt(history, run_id, raw_recorded_at, expected_job):
    run = history.get(f"actions/runs/{run_id}")
    if (not isinstance(run, dict) or run.get("head_branch") != "main"
            or run.get("path", "").split("@")[0] != WORKFLOW
            or (run.get("repository") or {}).get("full_name") != history.repo
            or (run.get("head_repository") or {}).get("full_name") != history.repo):
        raise incident_issue.IncidentError("production hold run provenance is malformed")
    attempt = run.get("run_attempt")
    if type(attempt) is not int or attempt <= 0:
        raise incident_issue.IncidentError("production hold run attempt is malformed")
    recorded_at = moment(raw_recorded_at)
    if attempt > 50:
        raise incident_issue.IncidentError("production hold attempt history exceeds the cap")
    for historical_attempt in range(attempt, 0, -1):
        jobs = list(history.pages(
            f"actions/runs/{run_id}/attempts/{historical_attempt}/jobs", key="jobs"))
        production = [job for job in jobs if job.get("name") == expected_job]
        if (len(production) != 1 or production[0].get("status") != "completed"
                or production[0].get("conclusion") != "success"):
            continue
        started = moment(production[0].get("started_at"))
        completed = moment(production[0].get("completed_at"))
        if started <= recorded_at <= completed:
            return historical_attempt, completed
    return None
