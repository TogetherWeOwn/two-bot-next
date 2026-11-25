"""Authenticate an inverse result emitted by the protected job in this run."""

from hashlib import sha256

from production_recovery_takeover import HEX, RUN_ID, SHA, verify_inverse
from rollback_proof import REPO, run_identity
from staging_migrate_reviewed import APP_BOT_ID
from staging_recovery_status import github, plan_manifest
from staging_rollout import mapping, require, sequence

WORKFLOW = ".github/workflows/deploy-production.yml"
REPO_ID = 1396224242
ARTIFACT = "production-release-inverse"
JOB = "Production release / Recover production schema / Apply or reconcile the protected inverse"
FENCE_JOB = "Production release / Recover production schema / Confirm the protected production writer stopped"
FENCE_STEP = "Fence the exact serving pair at the pinned epoch"
WRITE_STEP = "Verify the stopped fence and run the inverse"
UPLOAD_STEP = "Upload the verified inverse result"
MAX_ARCHIVE = 16 * 1024


def current_run(env, api=github):
    run_id, head = env.get("GITHUB_RUN_ID", ""), env.get("GITHUB_SHA", "")
    actor = env.get("GITHUB_ACTOR_ID", "")
    require(RUN_ID.fullmatch(run_id) and SHA.fullmatch(head)
            and RUN_ID.fullmatch(actor) and actor != str(APP_BOT_ID)
            and env.get("GITHUB_REPOSITORY") == REPO
            and env.get("GITHUB_REPOSITORY_ID") == str(REPO_ID)
            and env.get("GITHUB_REF") == "refs/heads/main"
            and env.get("GITHUB_REF_PROTECTED") == "true"
            and env.get("GITHUB_EVENT_NAME") == "workflow_dispatch"
            and env.get("GITHUB_WORKFLOW_REF") == f"{REPO}/{WORKFLOW}@refs/heads/main"
            and env.get("GITHUB_WORKFLOW_SHA") == head
            and env.get("GITHUB_RUN_ATTEMPT") == "1", "production_recovery_context_invalid")
    run = mapping(api(f"actions/runs/{run_id}"))
    run_identity(run, WORKFLOW, "workflow_dispatch")
    require(run.get("id") == int(run_id) and run.get("head_sha") == head
            and type(run.get("run_attempt")) is int and run["run_attempt"] == 1 and run.get("status") == "in_progress"
            and all(mapping(run.get(key)).get("id") == int(actor) for key in ("actor", "triggering_actor"))
            and all(mapping(run.get(key)).get("id") == REPO_ID
                    for key in ("repository", "head_repository")), "production_recovery_run_invalid")
    return run


def protected_job(run, api, name, expected_steps):
    jobs = mapping(api(f"actions/runs/{run['id']}/attempts/1/jobs?per_page=30"))
    rows = sequence(jobs.get("jobs"))
    require(jobs.get("total_count") == len(rows) <= 30, "production_inverse_job_inventory_invalid")
    protected = [row for row in rows if mapping(row).get("name") == name]
    require(len(protected) == 1, "production_inverse_job_missing_or_ambiguous")
    job = protected[0]
    require(job.get("run_id") == run["id"] and type(job.get("run_attempt")) is int and job["run_attempt"] == 1
            and job.get("head_sha") == run["head_sha"] and job.get("status") == "completed"
            and job.get("conclusion") == "success", "production_inverse_job_invalid")
    steps = [step for step in sequence(job.get("steps")) if mapping(step).get("name") in expected_steps]
    require([(step.get("name"), step.get("status"), step.get("conclusion")) for step in steps]
            == [(step, "completed", "success") for step in expected_steps],
            "production_inverse_step_invalid")


def authenticated_inverse(pin, approval, artifact_id, archive_sha, env, api=github):
    run = current_run(env, api)
    archive_sha = archive_sha.removeprefix("sha256:")
    require(str(run["id"]) == pin.run_id and pin.run_attempt == 1
            and RUN_ID.fullmatch(artifact_id) and HEX.fullmatch(archive_sha),
            "production_inverse_artifact_pin_invalid")
    protected_job(run, api, JOB, (WRITE_STEP, UPLOAD_STEP))
    listing = mapping(api(f"actions/runs/{run['id']}/artifacts?per_page=30"))
    rows = sequence(listing.get("artifacts"))
    require(listing.get("total_count") == len(rows) <= 30, "production_inverse_artifact_inventory_invalid")
    name, digest = f"{ARTIFACT}-{pin.run_id}-1", f"sha256:{archive_sha}"
    candidates = [row for row in rows if mapping(row).get("name") == name or row.get("id") == int(artifact_id)]
    require(len(candidates) == 1, "production_inverse_artifact_missing_or_ambiguous")
    entry = candidates[0]
    meta = mapping(api(f"actions/artifacts/{artifact_id}"))
    for row in (entry, meta):
        require(row.get("name") == name and row.get("id") == int(artifact_id)
                and row.get("expired") is False and row.get("digest") == digest
                and type(row.get("size_in_bytes")) is int and 0 < row["size_in_bytes"] <= MAX_ARCHIVE,
                "production_inverse_artifact_metadata_invalid")
    lineage = mapping(meta.get("workflow_run"))
    require(meta["size_in_bytes"] == entry["size_in_bytes"] and all(lineage.get(key) == value for key, value in {
        "id": run["id"], "repository_id": REPO_ID, "head_repository_id": REPO_ID,
        "head_branch": "main", "head_sha": run["head_sha"],
    }.items()), "production_inverse_artifact_run_invalid")
    raw = api(f"actions/artifacts/{artifact_id}/zip", binary=True)
    require(isinstance(raw, bytes) and len(raw) == meta["size_in_bytes"]
            and sha256(raw).hexdigest() == archive_sha, "production_inverse_archive_invalid")
    document = plan_manifest(raw, digest, ARTIFACT)
    require(set(document) == {"operation", "approval", "result"}
            and document["operation"] == "undo-0480" and document["approval"] == approval,
            "production_inverse_operation_mismatch")
    verify_inverse(pin, document["result"])
    return document["result"]
