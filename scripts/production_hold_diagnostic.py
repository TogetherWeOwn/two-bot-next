"""Inert pending production diagnostics over bounded history."""

import incident_issue
from production_hold_history import History as BoundedHistory

PRODUCTION_JOB = "wrangler deploy + readyz gate"


class History(BoundedHistory):
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
        fallback = None
        for run in runs:
            if str(run["id"]) == current_run_id:
                continue
            jobs = list(self.pages(f"actions/runs/{run['id']}/attempts/{run['run_attempt']}/jobs", key="jobs"))
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
