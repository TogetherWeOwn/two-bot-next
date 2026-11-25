"""Authenticate the inventory artifact from an approved staging ownership run."""

import json
import sys

from rollback_proof import ProofError, run_identity
from staging_recovery_status import (
    DIGEST, MAX_PLAN, REPOSITORY_ID, RUN_ID, SHA, WORKFLOW, checked_plan_summary, github, matches, plan_manifest,
)
from staging_rollout import GateError, mapping, number, require, sequence

ARTIFACT_STEM = "staging-recovery-status"
MAIN_PATHS = (WORKFLOW, f"{WORKFLOW}@main", f"{WORKFLOW}@refs/heads/main")
PROJECTION = ("schema_version", "workflow_sha", "workflow_run_id", "observed_at", "worker", "container", "ownership")


def verify_protected_main(sha, api):
    branch = mapping(api("branches/main"))
    tip = mapping(branch.get("commit")).get("sha")
    require(branch.get("protected") is True and matches(SHA, tip), "inventory_main_unproved")
    if sha != tip:
        comparison = mapping(api(f"compare/{sha}...{tip}?per_page=1&page=1"))
        require(comparison.get("status") == "ahead" and number(comparison.get("behind_by")) == 0
                and number(comparison.get("ahead_by")) > 0
                and mapping(comparison.get("base_commit")).get("sha") == sha
                and mapping(comparison.get("merge_base_commit")).get("sha") == sha,
                "inventory_main_unproved")


def read_inventory(workflow_sha, run_id, api=github):
    """Use the protected publisher and the authenticated run-scoped artifact list."""
    require(matches(SHA, workflow_sha) and matches(RUN_ID, run_id) and int(run_id) < 2**53,
            "invalid_inventory_pin")
    route = f"actions/runs/{run_id}"
    run = mapping(api(route))
    require(number(run.get("id")) == int(run_id) and number(run.get("run_attempt")) == 1
            and run.get("path") in MAIN_PATHS and run.get("head_sha") == workflow_sha
            and run.get("status") == "completed" and run.get("conclusion") == "success"
            and all(number(mapping(run.get(key)).get("id")) == REPOSITORY_ID
                    for key in ("repository", "head_repository")), "invalid_inventory_run")
    run_identity(run, WORKFLOW, "workflow_dispatch")
    # The pinned producer also checks the job's protected runner ref and
    # verifies dispatch again before it publishes this uniquely named artifact.
    verify_protected_main(workflow_sha, api)

    listing = mapping(api(f"{route}/artifacts?per_page=2"))
    artifacts = sequence(listing.get("artifacts"))
    require(number(listing.get("total_count")) == len(artifacts) == 1, "invalid_inventory_artifacts")
    artifact = mapping(artifacts[0])
    artifact_id, size = number(artifact.get("id")), number(artifact.get("size_in_bytes"))
    digest = artifact.get("digest")
    require(artifact_id > 0 and 0 < size <= MAX_PLAN and artifact.get("expired") is False
            and matches(DIGEST, digest) and artifact.get("name") == f"{ARTIFACT_STEM}-{run_id}-1",
            "invalid_inventory_artifact")
    raw = api(f"actions/artifacts/{artifact_id}/zip", binary=True)
    require(isinstance(raw, bytes) and len(raw) == size, "invalid_inventory_archive")
    inventory = plan_manifest(raw, digest, ARTIFACT_STEM)
    require(inventory.keys() == {*PROJECTION, "verification", "runtime_database", "plan"}
            and number(inventory.get("schema_version")) == 1 and inventory.get("workflow_sha") == workflow_sha
            and number(inventory.get("workflow_run_id")) == int(run_id), "invalid_inventory_provenance")
    plan = checked_plan_summary(inventory["plan"])
    # The AutoMod admission and action approvals own the database gate.
    allowed = ((("provider_only", "plan_not_supplied"),) if plan is None
               else (("full_tuple", "verified"), ("provider_only", "worker_unsupported")))
    require((inventory["verification"], inventory["runtime_database"]) in allowed, "invalid_inventory_plan")
    owner = mapping(inventory["ownership"])
    require(owner.keys() == {"phase", "epoch", "running"} and type(owner["running"]) is bool,
            "invalid_inventory_owner")
    return {**{key: inventory[key] for key in PROJECTION}, "ownership": {"phase": owner["phase"], "epoch": owner["epoch"]}}


def main(argv, api=github):
    try:
        require(len(argv) == 2, "invalid_inventory_pin")
        print(json.dumps(read_inventory(argv[0], argv[1], api), allow_nan=False, separators=(",", ":")))
    except (GateError, ProofError, OSError, ValueError):
        print("Staging inventory evidence unconfirmed", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
