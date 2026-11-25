"""Check the approved attendance release and its live recovery pair before fencing."""

from dataclasses import asdict, dataclass
from datetime import datetime, timezone
import re

from ci.production_migrate_claim import WORKFLOW_PATH
from production_migrate_approval import ARTIFACT, stable_plan, verify_check, verify_environment
from rollback_proof import moment
from production_recovery_artifact import (STOP_INTENT_ARTIFACT, artifact_entries, attempt_job, authentic_document,
                                          current_run, interrupted_origin, protected_job)
from production_recovery_takeover import HEX, RUN_ID, SHA, UUID
from production_release_forward import forward_receipt, ledger_endpoints
from staging_migrate_approval import digest_json
from staging_recovery_status import DIGEST, authenticated_plan, github, matches
from staging_rollout import mapping, require, sequence

NAMESPACE = re.compile(r"[0-9a-f]{32}")
PREPARE_JOB = "Production release / Prepare and stop the prior pair"
PREPARE_STEPS = ("Verify forward release preflight", "Confirm the exact stopped writer")
STOP_INTENT_STEP = "Retain the exact recovery stop intent"
STOP_INTENT_STEPS = (PREPARE_STEPS[0], STOP_INTENT_STEP, PREPARE_STEPS[1])


@dataclass(frozen=True)
class ReleasePreflight:
    workflow_sha: str
    candidate_sha: str
    candidate_archive: str
    candidate_image: str
    rollback_sha: str
    rollback_worker: str
    rollback_image: str
    application_id: str
    namespace_id: str
    restore_proof_sha256: str

    def recovery_reference(self):
        fields = (
            (SHA, self.workflow_sha), (SHA, self.candidate_sha), (SHA, self.rollback_sha),
            (UUID, self.rollback_worker), (UUID, self.application_id), (NAMESPACE, self.namespace_id),
            (DIGEST, self.candidate_archive), (DIGEST, self.candidate_image), (DIGEST, self.rollback_image),
            (HEX, self.restore_proof_sha256),
        )
        require(all(matches(pattern, value) for pattern, value in fields)
                and self.candidate_sha != self.rollback_sha and self.candidate_image != self.rollback_image,
                "production_release_preflight_pins_invalid")
        # The provider assigns a new Worker version when it uploads the
        # approved modules. Bind the immutable package before that upload.
        return "inverse-" + digest_json({"operation": "attendance-0480", **asdict(self)})


def verify_preflight(pin, target, plan, fresh, observe, control):
    """Verify a fresh PRE plan and the exact live pair's read-only takeover capability."""
    reference = pin.recovery_reference()
    require(plan.get("mode") == "plan" and plan.get("role") == "two_bot_migrator_ro"
            and plan.get("migration_target") == "production" and plan.get("target") == target
            and plan.get("source_sha") == pin.candidate_sha and plan.get("recovery_evidence_ref") == reference
            and stable_plan(plan) == stable_plan(fresh), "production_release_preflight_plan_changed")
    ledger_endpoints(plan)
    state = observe()
    worker, image, owner = (mapping(state.get(key)) for key in ("worker", "container", "ownership"))
    require(worker.get("version_id") == pin.rollback_worker and worker.get("sha") == pin.rollback_sha
            and worker.get("traffic_percent") == 100 and image.get("image_digest") == pin.rollback_image
            and image.get("application_id") == pin.application_id and image.get("namespace_id") == pin.namespace_id
            and image.get("rollout_status") == "completed" and owner.get("phase") == "active"
            and owner.get("running") is True and type(owner.get("epoch")) is int and 0 < owner["epoch"] < 2**53 - 4,
            "production_release_retained_pair_changed")
    capability = control("takeover-capability", expected_deployment=pin.rollback_worker)
    require(capability == {"deploymentId": pin.rollback_worker, "exactTakeover": True}
            and capability["exactTakeover"] is True, "production_release_retained_takeover_unproved")
    after = observe()
    require(all(after.get(key) == state[key] for key in ("worker", "container", "ownership")),
            "production_release_retained_pair_changed")
    return owner["epoch"]


def prepared_epoch(pin, run, plan_id, stopped):
    run_id = run.get("id")
    owner = mapping(stopped.get("owner"))
    epoch = owner.get("epoch")
    actor = f"production-schema-release:{run_id}:{pin.candidate_sha}"
    require(type(run_id) is int and matches(RUN_ID, str(run_id)) and run.get("head_sha") == pin.workflow_sha
            and matches(RUN_ID, plan_id) and plan_id != str(run_id) and run.get("run_attempt") == 1
            and stopped.get("deploymentId") == pin.rollback_worker and stopped.get("running") is False
            and owner.get("phase") == "fenced" and "deploymentId" in owner and owner["deploymentId"] is None
            and owner.get("actor") == actor and type(epoch) is int and 1 < epoch < 2**53 - 1
            and type(owner.get("oldEpoch")) is int and owner["oldEpoch"] == epoch - 1,
            "production_forward_stopped_fence_unproved")
    return epoch


def _plan_for_target(pin, run, plan_id, matches_target, api):
    require(matches(RUN_ID, plan_id) and plan_id != str(run["id"]), "production_forward_plan_changed")
    plan, artifact = authenticated_plan(plan_id, api, workflow=WORKFLOW_PATH, artifact_name=ARTIFACT)
    require(plan.get("mode") == "plan" and plan.get("migration_target") == "production"
            and plan.get("role") == "two_bot_migrator_ro"
            and matches_target(plan.get("target"))
            and plan.get("source_sha") == pin.candidate_sha
            and plan.get("recovery_evidence_ref") == pin.recovery_reference(),
            "production_forward_plan_changed")
    ledger_endpoints(plan)
    verify_environment(api)
    return plan, artifact


def bound_plan(pin, run, plan_id, target_fingerprint, api=github):
    """Authenticate the plan identity used by the current protected attempt."""
    return _plan_for_target(pin, run, plan_id, lambda target: matches(HEX, target_fingerprint)
                            and digest_json(mapping(target)) == target_fingerprint, api)


def approved_plan(pin, run, plan_id, target, api=github):
    plan, artifact = _plan_for_target(pin, run, plan_id, lambda planned: planned == target, api)
    _, approved_at = verify_check(
        api, run, f"production-apply:{plan_id}:{artifact['archive_digest'].removeprefix('sha256:')}",
        datetime.now(timezone.utc),
    )
    require(moment(api(f"actions/runs/{plan_id}").get("created_at")) <= approved_at,
            "independent_production_approval_stale")
    return plan, artifact


def stop_intent(pin, run, plan_id, plan, artifact, epoch):
    return {"operation": STOP_INTENT_ARTIFACT, "pin": asdict(pin), "epoch": epoch,
            "run_id": str(run["id"]), "run_attempt": 1, "plan_run_id": plan_id,
            "plan_archive_sha256": artifact["archive_digest"],
            "target_fingerprint": digest_json(mapping(plan["target"]))}


def interrupted_release_context(env, api=github):
    """Authenticate the original, durable stop intent without a forward result."""
    current = current_run(env, api)
    origin = interrupted_origin(env.get("RELEASE_RECOVERY_RUN_ID"), str(current["id"]), api)
    if origin["head_sha"] != current["head_sha"]:
        compare = mapping(api(f"compare/{origin['head_sha']}...{current['head_sha']}"))
        require(compare.get("status") == "ahead"
                and mapping(compare.get("merge_base_commit")).get("sha") == origin["head_sha"],
                "production_interrupted_origin_not_ancestor")
    job = attempt_job(origin, api, PREPARE_JOB)
    require(job.get("conclusion") in ("success", "failure", "cancelled"),
            "production_interrupted_stop_producer_invalid")
    steps = [step for step in sequence(job.get("steps")) if mapping(step).get("name") in STOP_INTENT_STEPS]
    require(tuple(step["name"] for step in steps) == STOP_INTENT_STEPS
            and all(step.get("status") == "completed" for step in steps)
            and all(step.get("conclusion") == "success" for step in steps[:2])
            and steps[-1].get("conclusion") in ("success", "failure", "cancelled"),
            "production_interrupted_stop_steps_invalid")

    name = f"{STOP_INTENT_ARTIFACT}-{origin['id']}-1"
    candidates = [entry for entry in artifact_entries(origin, api) if entry.get("name") == name]
    require(len(candidates) == 1, "production_interrupted_stop_intent_missing_or_ambiguous")
    entry = candidates[0]
    require(type(entry.get("id")) is int and matches(DIGEST, entry.get("digest")),
            "production_interrupted_stop_intent_invalid")
    intent = authentic_document(origin, STOP_INTENT_ARTIFACT, str(entry["id"]),
                                entry["digest"].removeprefix("sha256:"), api)
    pin = ReleasePreflight(**mapping(intent.get("pin")))
    plan_id, fingerprint, epoch = intent.get("plan_run_id"), intent.get("target_fingerprint"), intent.get("epoch")
    require(pin.workflow_sha == origin["head_sha"] and type(epoch) is int and 0 < epoch < 2**53 - 4,
            "production_interrupted_stop_intent_invalid")
    plan, artifact = bound_plan(pin, origin, plan_id, fingerprint, api)
    require(type(intent.get("run_attempt")) is int and intent == stop_intent(pin, origin, plan_id, plan, artifact, epoch),
            "production_interrupted_stop_intent_invalid")
    return pin, origin, plan_id, plan, artifact, epoch + 1


def apply_under_prepared_fence(pin, run, plan_id, target, stopped, apply_database, api=github):
    """Recheck the current attempt's stopped writer and approval before applying."""
    epoch = prepared_epoch(pin, run, plan_id, stopped)
    run_id = run["id"]
    protected_job(run, api, PREPARE_JOB, PREPARE_STEPS)
    plan, artifact = approved_plan(pin, run, plan_id, target, api)
    receipt = forward_receipt(plan_id, plan, apply_database(plan_id, plan))
    return {**receipt, "run_id": str(run_id), "run_attempt": 1, "stopped_epoch": epoch,
            "plan_archive_sha256": artifact["archive_digest"], "release_reference": plan["recovery_evidence_ref"]}
