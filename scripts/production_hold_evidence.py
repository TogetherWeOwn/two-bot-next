"""Inert validation and classification of supplied production hold evidence."""

import incident_issue

PRODUCTION_JOB = "wrangler deploy + readyz gate"
ROLLBACK_JOB = "wrangler rollback + readyz gate"
ACTIVE_STATES = ("waiting", "queued", "pending", "requested", "in_progress")
CONCLUSIONS = (None, "success", "neutral", "skipped", "failure", "cancelled",
               "timed_out", "action_required", "startup_failure", "stale")
UNSUCCESSFUL = ("failure", "cancelled", "timed_out", "action_required",
                "startup_failure", "stale")


def active_runs(data, current_run_id):
    """Validate the whole batch before returning non-current runs in order."""
    runs = data.get("workflow_runs") if isinstance(data, dict) else None
    if not isinstance(runs, list) or len(runs) > 20:
        raise incident_issue.IncidentError("production hold active runs are malformed")
    result = []
    for run in runs:
        if not isinstance(run, dict) or type(run.get("id")) is not int or run["id"] <= 0:
            raise incident_issue.IncidentError("production hold active run is malformed")
        if str(run["id"]) == current_run_id:
            continue
        attempt = run.get("run_attempt")
        if type(attempt) is not int or attempt <= 0:
            raise incident_issue.IncidentError("production hold active attempt is malformed")
        result.append(run)
    return result


def classify_jobs(jobs):
    """Return (is_active, reason) only after validating the complete job list."""
    if not isinstance(jobs, list):
        raise incident_issue.IncidentError("production hold active jobs are malformed")
    for job in jobs:
        if (not isinstance(job, dict) or not isinstance(job.get("name"), str)
                or not job["name"] or job.get("status") not in (*ACTIVE_STATES, "completed")
                or "conclusion" not in job or job["conclusion"] not in CONCLUSIONS
                or (job["status"] == "completed" and job["conclusion"] is None)):
            raise incident_issue.IncidentError("production hold active job is malformed")
    for job in jobs:
        if job["name"] in (PRODUCTION_JOB, ROLLBACK_JOB) and job["status"] in ACTIVE_STATES:
            return True, f"production {job['status']}"
    if any(job["conclusion"] in UNSUCCESSFUL for job in jobs):
        return False, "promotion gate not green - inspect promotion run"
    return False, "production skipped or pending - inspect promotion run"
