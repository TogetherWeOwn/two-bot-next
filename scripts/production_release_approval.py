"""Validate single-use approval history for a protected attendance release."""

import re
from urllib.parse import urlencode

from ci.production_migrate_claim import WORKFLOW_PATH as PLAN_WORKFLOW
from production_recovery_artifact import REPO_ID, WORKFLOW
from rollback_proof import ProofError, moment, run_identity
from staging_recovery_status import DIGEST, RUN_ID, matches
from staging_rollout import GateError, mapping, number, require, sequence

ATTENDANCE_TITLE = "attendance release plan "
LEGACY_TITLE = "deploy-production"
LEGACY_RELEASE_JOB = "Production release / "
HISTORY_LIMIT = 30
CREATED_AT = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z")
INVALID = "production_attendance_history_invalid"
INCOMPLETE = "production_attendance_history_incomplete"
REUSED = "production_attendance_plan_superseded_by_release_attempt"


def identifier(value):
    return type(value) is int and 0 < value < 10**20


def run_metadata(run, workflow):
    run = mapping(run)
    require(run.get("path") in (workflow, f"{workflow}@main", f"{workflow}@refs/heads/main")
            and isinstance(run.get("head_sha"), str) and identifier(run.get("id"))
            and number(run.get("run_attempt")) > 0 and matches(CREATED_AT, run.get("created_at"))
            and all(number(mapping(run.get(key)).get("id")) == REPO_ID
                    for key in ("repository", "head_repository")), INVALID)
    try:
        run_identity(run, workflow, "workflow_dispatch")
        return moment(run["created_at"])
    except ProofError:
        raise GateError(INVALID) from None


def inventory(api, path, key):
    listing = mapping(api(path))
    rows = sequence(listing.get(key))
    require(number(listing.get("total_count")) == len(rows) <= HISTORY_LIMIT, INCOMPLETE)
    return rows


def require_legacy_non_attempt(api, run):
    # Older runs shared the default title. First-attempt job metadata must show
    # that no attendance job ran. A canceled or unidentifiable job is ambiguous.
    require(run.get("status") == "completed", INCOMPLETE)
    jobs = inventory(api, f"actions/runs/{run['id']}/attempts/1/jobs?per_page={HISTORY_LIMIT}", "jobs")
    require(bool(jobs), INCOMPLETE)
    seen = set()
    for job in map(mapping, jobs):
        job_id, name, result = job.get("id"), job.get("name"), job.get("conclusion")
        require(identifier(job_id) and job_id not in seen and number(job.get("run_id")) == run["id"]
                and number(job.get("run_attempt")) == 1
                and job.get("head_sha") == run["head_sha"] and job.get("status") == "completed"
                and isinstance(name, str) and bool(name) and isinstance(result, str) and bool(result), INVALID)
        seen.add(job_id)
        require(not name.startswith(LEGACY_RELEASE_JOB) or result == "skipped", REUSED)


def require_unused_attendance_plan(api, producer, release_attempt):
    """Raise on reuse or uncertain history. The caller holds write serialization.

    Pass authenticated_plan's artifact metadata and production_recovery_artifact's
    current_run record after verifying the bound approval. The protected workflow
    must stamp ATTENDANCE_TITLE plus its release_plan_run_id at dispatch, before
    any attendance work. A matching dispatch consumes the producer as a whole,
    including when it fails before writing a receipt. Call before stopping the
    writer, using complete retained GitHub history for the approval window.
    """
    producer = mapping(producer)
    producer_id = producer.get("run_id")
    require(identifier(producer_id) and identifier(producer.get("artifact_id"))
            and matches(DIGEST, producer.get("archive_digest")), INVALID)
    producing_run = mapping(api(f"actions/runs/{producer_id}"))
    since = run_metadata(producing_run, PLAN_WORKFLOW)
    require(producing_run["id"] == producer_id and producing_run["run_attempt"] == 1
            and producing_run.get("status") == "completed" and producing_run.get("conclusion") == "success", INVALID)

    created = run_metadata(release_attempt, WORKFLOW)
    title = f"{ATTENDANCE_TITLE}{producer_id}"
    current_id = release_attempt["id"]
    require(current_id != producer_id and release_attempt["run_attempt"] == 1 and created >= since
            and release_attempt.get("status") == "in_progress" and release_attempt.get("display_title") == title, INVALID)

    query = urlencode(dict(event="workflow_dispatch", branch="main", created=f">={producing_run['created_at']}",
                           per_page=HISTORY_LIMIT))
    rows = inventory(api, f"actions/workflows/deploy-production.yml/runs?{query}", "workflow_runs")
    seen = set()
    for run in rows:
        run_created = run_metadata(run, WORKFLOW)
        run_id, run_title = run["id"], run.get("display_title")
        require(run_created >= since and run_id != producer_id and run_id not in seen, INVALID)
        seen.add(run_id)
        if run_id == current_id:
            require(run_created == created and run["head_sha"] == release_attempt["head_sha"]
                    and run["run_attempt"] == 1 and run.get("status") == "in_progress" and run_title == title, INVALID)
        elif run_title == LEGACY_TITLE:
            require_legacy_non_attempt(api, run)
        else:
            require(isinstance(run_title, str) and run_title.startswith(ATTENDANCE_TITLE)
                    and matches(RUN_ID, run_title.removeprefix(ATTENDANCE_TITLE)), INVALID)
            require(run_title != title, REUSED)
    require(current_id in seen, INCOMPLETE)
