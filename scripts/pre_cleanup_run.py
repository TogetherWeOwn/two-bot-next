"""Classify the protected PRE reservation's current GitHub Actions run."""

from enum import StrEnum

from pre_target import SHA, Target
from rollback_proof import ProofError, run_identity
from staging_recovery_status import REPOSITORY_ID, matches
from staging_rollout import GateError, mapping, number


WORKFLOW = ".github/workflows/staging-pre-qualification.yml"
_MAIN_PATHS = (WORKFLOW, f"{WORKFLOW}@main", f"{WORKFLOW}@refs/heads/main")
_ACTIVE = ("queued", "in_progress", "requested", "waiting", "pending")
_UNSUCCESSFUL = ("action_required", "cancelled", "failure", "neutral", "skipped",
                 "stale", "startup_failure", "timed_out")


class CleanupRunStatus(StrEnum):
    UNRESOLVED = "unresolved"
    ACTIVE = "active"
    TERMINAL_UNSUCCESSFUL = "terminal_unsuccessful"
    TERMINAL_SUCCESS = "terminal_success"

    @property
    def sweep_status(self) -> str | None:
        """Translate to the sweep planner's status without attesting cleanup."""
        if self is CleanupRunStatus.ACTIVE:
            return "active"
        if self in (CleanupRunStatus.TERMINAL_UNSUCCESSFUL, CleanupRunStatus.TERMINAL_SUCCESS):
            return "terminal"
        return None


def classify_cleanup_run(target: Target, run: object, *, expected_workflow_id: int,
                         expected_head_sha: str) -> CleanupRunStatus:
    """Attribute a run status to one authenticated first-attempt reservation.

    Obtain both pins from trusted reservation context. Supply the freshly and
    strictly decoded response to the authenticated repository-scoped
    ``GET /actions/runs/{target.run_id}``, or None on an unconfirmed read.
    The current run detail exposes retries. An older attempt's detail can
    still report success after a retry supersedes it.

    Terminal success is the reserved workflow's reported outcome. Returning
    cleared still requires independently settled startup and fresh provider
    proof of absence, zero counts and revoked write access.
    """
    if (type(target) is not Target or type(expected_workflow_id) is not int
            or not 0 < expected_workflow_id < 10**20 or not matches(SHA, expected_head_sha)):
        return CleanupRunStatus.UNRESOLVED

    try:
        run = mapping(run)
        if (number(run.get("id")) != target.run_id or number(run.get("run_attempt")) != 1
                or number(run.get("workflow_id")) != expected_workflow_id
                or run.get("path") not in _MAIN_PATHS or run.get("head_sha") != expected_head_sha
                or any(number(mapping(run.get(key)).get("id")) != REPOSITORY_ID
                       for key in ("repository", "head_repository"))):
            return CleanupRunStatus.UNRESOLVED
        run_identity(run, WORKFLOW, "workflow_dispatch")
    except (GateError, ProofError):
        return CleanupRunStatus.UNRESOLVED

    status, conclusion = run.get("status"), run.get("conclusion")
    if status in _ACTIVE and "conclusion" in run and conclusion is None:
        return CleanupRunStatus.ACTIVE
    if status == "completed":
        if conclusion == "success":
            return CleanupRunStatus.TERMINAL_SUCCESS
        if conclusion in _UNSUCCESSFUL:
            return CleanupRunStatus.TERMINAL_UNSUCCESSFUL
    return CleanupRunStatus.UNRESOLVED
