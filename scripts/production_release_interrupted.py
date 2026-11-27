"""Reconcile an interrupted protected attendance apply without a forward receipt."""

from pathlib import Path
import os
import subprocess
import sys

from production_ownership_status import environment_policy, read_status
from production_recovery_artifact import (FORWARD_JOB, FORWARD_STEPS, attempt_job, authentic_document,
                                          protected_job)
from production_recovery_takeover import RecoveryPin, verify_inverse
from production_recovery_workflow import (interrupted_classification, interrupted_pair_phase, pair_matches,
                                          reconcile_interrupted, restore_under_fence, run_inverse,
                                          run_recovery_database)
from production_release import output, release_actor, write_json
from production_release_ledgers import ledger_endpoints
from production_release_preflight import interrupted_release_context, stop_intent
from rollback_proof import ProofError
from staging_migrate_approval import digest_json
from staging_recovery_status import github
from staging_rollout import GateError, decode, mapping, require, sequence
from worker_container_rollback import APPLICATIONS, WORKERS, Rollback, RollbackClient, check_url, control_client

JOB_PREFIX = "Production release / Reconcile interrupted apply / "
ADMISSION_JOB = JOB_PREFIX + "Verify the original stopped writer"
DATABASE_JOB = JOB_PREFIX + "Classify both ledgers and finish the protected inverse"
ADMISSION_STEPS = ("Authenticate the interrupted origin and observe the exact pair",
                   "Retain the authenticated interrupted admission")
CLASSIFY_STEP = "Classify the interrupted database state under lock"
DECISION_STEP = "Retain the locked database classification"
INVERSE_STEPS = ("Run the exact protected inverse for POST", "Retain the verified interrupted inverse")
ADMISSION = "production-release-interrupted-admission"
DECISION = "production-release-interrupted-classification"
INVERSE = "production-release-interrupted-inverse"


def packet(env, run, job, steps, name, prefix, api):
    protected_job(run, api, job, steps)
    return authentic_document(run, name, env[prefix + "_ARTIFACT_ID"],
                              env[prefix + "_ARCHIVE_SHA256"], api)


def verified_admission(document, binding):
    require(set(document) == {"binding", "phase"} and document["binding"] == binding
            and document["phase"] in ("stopped", "resumed"), "production_interrupted_admission_changed")


def verified_decision(document, admission):
    require(set(document) == {"admission", "classification"} and document["admission"] == admission,
            "production_interrupted_decision_changed")
    classification = mapping(document["classification"])
    state = interrupted_classification(classification)
    require(state == "pre" or admission["phase"] == "stopped", "production_interrupted_stop_unproved")
    return classification


def require_forward_attempt(origin, api):
    job = attempt_job(origin, api, FORWARD_JOB)
    steps = [step for step in sequence(job.get("steps")) if mapping(step).get("name") == FORWARD_STEPS[0]]
    require(len(steps) == 1 and steps[0].get("status") == "completed"
            and steps[0].get("conclusion") in ("success", "failure", "cancelled"),
            "production_interrupted_forward_attempt_unproved")


def recovery_pin(pin, origin_id, epoch, plan, stamp):
    _, expanded, restored = ledger_endpoints(plan)
    return RecoveryPin(origin_id, 1, pin.candidate_sha, pin.rollback_sha, pin.rollback_worker,
                       pin.rollback_image, pin.application_id, pin.namespace_id, digest_json(plan["target"]),
                       digest_json(expanded), digest_json(restored), stamp, epoch)


def execute(argv, env, api):
    pin, origin, plan_id, plan, artifact, epoch = interrupted_release_context(env, api)
    origin_id = str(origin["id"])
    # interrupted_release_context already bound these values to the fresh
    # attempt-one protected run and to the authenticated origin ZIP.
    run = {"id": int(env["GITHUB_RUN_ID"]), "head_sha": env["GITHUB_SHA"]}
    binding = {"run_id": str(run["id"]), "workflow_sha": run["head_sha"],
               "stop_intent": stop_intent(pin, origin, plan_id, plan, artifact, epoch - 1)}
    directory = Path(env["RUNNER_TEMP"]) / "interrupted-production-release"
    directory.mkdir(mode=0o700, exist_ok=True)

    def observe():
        return read_status(env)

    if argv == ["admit"]:
        environment_policy(env.get("AUTO_APPROVE"), api)
        phase = interrupted_pair_phase(pin, origin_id, epoch, observe())
        write_json(directory / f"{ADMISSION}.json", {"binding": binding, "phase": phase})
        return

    admission = packet(env, run, ADMISSION_JOB, ADMISSION_STEPS, ADMISSION, "ADMISSION", api)
    verified_admission(admission, binding)
    decision_path = directory / f"{DECISION}.json"
    if len(argv) == 2 and argv[0] == "classify":
        classification = run_recovery_database(argv[1], directory, plan_id, plan, env,
                                               "--classify-interrupted-release")
        decision = {"admission": admission, "classification": classification}
        verified_decision(decision, admission)
        if classification["state"] == "post":
            require_forward_attempt(origin, api)
        write_json(decision_path, decision)
        output(env, state=classification["state"])
        return
    if len(argv) == 2 and argv[0] == "inverse":
        decision = mapping(decode(decision_path.read_bytes()))
        classification = verified_decision(decision, admission)
        require(classification["state"] == "post", "production_interrupted_inverse_without_post")
        recovery = recovery_pin(pin, origin_id, epoch, plan, classification["attendance_fence_sha256"])
        approval = {"plan_run_id": plan_id, "recovery_ref": plan["recovery_evidence_ref"]}
        result = run_inverse(argv[1], directory, recovery, plan, approval, env)
        write_json(directory / f"{INVERSE}.json", {"decision_sha256": digest_json(decision), "result": result})
        return
    require(argv == ["restart"], "production_interrupted_operation_invalid")
    decision = packet(env, run, DATABASE_JOB, (CLASSIFY_STEP, DECISION_STEP), DECISION, "DECISION", api)
    classification = verified_decision(decision, admission)
    post = classification["state"] == "post"
    if post:
        proof = packet(env, run, DATABASE_JOB, (CLASSIFY_STEP, DECISION_STEP, *INVERSE_STEPS),
                       INVERSE, "INVERSE", api)
        require(set(proof) == {"decision_sha256", "result"} and proof["decision_sha256"] == digest_json(decision),
                "production_interrupted_inverse_decision_changed")
        recovery = recovery_pin(pin, origin_id, epoch, plan, classification["attendance_fence_sha256"])
        verify_inverse(recovery, proof["result"])
    environment_policy(env.get("AUTO_APPROVE"), api)
    url = check_url("production", env["PRODUCTION_WORKER_URL"])
    token = env["OWNERSHIP_CONTROL_TOKEN"]
    actor = release_actor(origin_id, pin.candidate_sha)
    control = control_client("production", token, url, "", actor)
    account = env["CLOUDFLARE_ACCOUNT_ID"]
    client = RollbackClient(account, env["CLOUDFLARE_API_TOKEN"])
    worker, application = WORKERS["production"], APPLICATIONS["production"]
    rollback = Rollback(client, control, url, "production", worker, application,
                        pin.application_id, pin.rollback_sha, origin_id)

    def recover_post(_stamp):
        post_control = control_client("production", token, url, "", recovery.actor)
        # The interrupted-apply route proves the prior pair still serves. An
        # unproved deployment or image must never trigger a provider restore.
        prior_only = ("", pin.candidate_sha, pin.rollback_image)
        image = f"registry.cloudflare.com/{account}/{application}@{pin.rollback_image}"
        restore_under_fence(recovery, proof["result"], observe, post_control, rollback, image, prior_only)
        return {"state": "post", "worker": pin.rollback_worker, "epoch": epoch + 1}

    outcome = reconcile_interrupted(pin, origin_id, epoch, classification, observe, control, recover_post)
    expected_actor = actor
    if post:
        expected_actor = recovery.actor
    else:
        rollback.wait_stable(pin.rollback_worker, rollback.now())
    state = observe()
    require(pair_matches(state, pin, pin.rollback_worker, pin.rollback_sha, pin.rollback_image)
            and state["ownership"] == {"phase": "active", "epoch": epoch + 1,
                                       "owner_actor": expected_actor, "running": True},
            "production_interrupted_restart_unproved")
    write_json(directory / "production-release-interrupted-restored.json",
               {"outcome": outcome, "decision_sha256": digest_json(decision),
                "provider": state, "stable_polls": rollback.evidence["stable_polls"]})


def main(argv, env, api=github):
    try:
        execute(argv, env, api)
    except (GateError, ProofError, KeyError, TypeError, ValueError, OSError, subprocess.SubprocessError):
        print("::error::Interrupted production release recovery refused or uncertain. Preserve the evidence.",
              file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], os.environ))
