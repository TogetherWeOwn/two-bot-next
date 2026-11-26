"""Dormant receipt contract for a protected PR evidence producer."""

from dataclasses import asdict, dataclass

import pr_evidence as evidence


WORKFLOW_PATH = ".github/workflows/pr-evidence.yml"
VERIFY_STEP = "Verify and publish evidence"


@dataclass(frozen=True)
class ReceiptAttempt:
    """Identity established independently of the receipt by a trusted caller."""

    repo_id: int
    workflow_id: int
    workflow_sha: str
    run_id: int
    attempt: int
    job_id: int

    def __post_init__(self):
        if (not all(type(value) is int and value > 0 for value in (
                self.repo_id, self.workflow_id, self.run_id, self.attempt, self.job_id))
                or not isinstance(self.workflow_sha, str) or not evidence.SHA.fullmatch(self.workflow_sha)):
            raise evidence.EvidenceError("Invalid protected verification attempt.")


def _receipt(attempt, verdict):
    return {"version": 1, "workflow_path": WORKFLOW_PATH, **asdict(attempt), **asdict(verdict)}


def issue_receipt(policy, reader, number, repo, base, head, *, attempt):
    """Issue from the digest of the description validated and rechecked in this attempt.

    The caller must be the protected verification job and obtain attempt identity
    from trusted metadata. The advisory entry point cannot establish that binding.
    """
    if not isinstance(attempt, ReceiptAttempt):
        raise evidence.EvidenceError("Missing protected verification attempt.")
    return _receipt(attempt, evidence.evaluate_current(policy, reader, number, repo, base, head))


def _fields_match(record, expected):
    return all(type(record[key]) is type(value) and record[key] == value for key, value in expected.items())


def matches_receipt(receipt, verdict, run, listing, *, attempt, receipt_job_id):
    """Match a decoded receipt to the intended snapshot and GitHub attempt records.

    The caller authenticates the protected workflow revision in `attempt` and the
    output origin in `receipt_job_id` independently of the receipt. `verdict` is
    the expected snapshot from a fresh PR read. `run` and `listing` must come from
    GitHub's endpoints for that exact attempt. Files alone establish no provenance.
    """
    try:
        if (not isinstance(attempt, ReceiptAttempt) or not isinstance(verdict, evidence.EvidenceVerdict)
                or type(receipt_job_id) is not int
                or receipt_job_id != attempt.job_id or type(receipt) is not dict):
            return False
        expected = _receipt(attempt, verdict)
        if receipt.keys() != expected.keys() or not _fields_match(receipt, expected):
            return False
        (association,) = run["pull_requests"]
        (job,) = listing["jobs"]
        (step,) = [step for step in job["steps"] if step["name"] == VERIFY_STEP]
        success = {"status": "completed", "conclusion": "success"}
        return (
            _fields_match(run, dict(success, id=attempt.run_id, run_attempt=attempt.attempt,
                                    workflow_id=attempt.workflow_id, path=WORKFLOW_PATH,
                                    event="pull_request", head_sha=verdict.head))
            and _fields_match(run["repository"], {"id": attempt.repo_id, "full_name": verdict.repo})
            and _fields_match(association, {"number": verdict.number})
            and _fields_match(association["base"], {"sha": verdict.base})
            and _fields_match(association["base"]["repo"], {"id": attempt.repo_id})
            and _fields_match(association["head"], {"sha": verdict.head})
            and _fields_match(listing, {"total_count": 1})
            and _fields_match(job, dict(success, id=attempt.job_id, name="pr-evidence",
                                        run_id=attempt.run_id, head_sha=verdict.head))
            and ((attempt.attempt == 1 and "run_attempt" not in job)
                 or _fields_match(job, {"run_attempt": attempt.attempt}))
            and _fields_match(step, success)
        )
    except (AttributeError, KeyError, TypeError, ValueError):
        return False
