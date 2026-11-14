"""Inert pending production diagnostics over bounded history."""

import incident_issue
from production_hold_evidence import PRODUCTION_JOB, active_runs, classify_jobs
from production_hold_history import History as BoundedHistory


class History(BoundedHistory):
    def pending_reason(self, current_run_id):
        data = self.get("actions/workflows/deploy-production.yml/runs", branch="main", per_page=20)
        fallback = None
        for run in active_runs(data, current_run_id):
            jobs = list(self.pages(f"actions/runs/{run['id']}/attempts/{run['run_attempt']}/jobs", key="jobs"))
            active, reason = classify_jobs(jobs)
            url = f"https://github.com/{self.repo}/actions/runs/{run['id']}"
            if active:
                return reason, url
            if fallback is None:
                fallback = reason, url
        return fallback or ("no successful production promotion", None)
