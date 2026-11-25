"""Run the protected database inverse and complete a fenced provider restore."""

from dataclasses import asdict
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import subprocess
import sys

from ci.production_migrate_claim import WORKFLOW_PATH, projection_hash, store_projection_hash
from production_migrate_approval import ARTIFACT as PLAN_ARTIFACT, verify_check, verify_environment
from production_ownership_status import read_status
from production_recovery_artifact import FENCE_JOB, FENCE_STEP, authenticated_inverse, current_run, protected_job
from production_recovery_takeover import RUN_ID, UUID, RecoveryPin, complete_legacy_takeover, observed_phase, verify_inverse
from rollback_proof import ProofError
from staging_migrate_approval import digest_json
from staging_recovery_status import DIGEST, authenticated_plan, github, matches
from staging_rollout import GateError, decode, mapping, require
from worker_container_rollback import APPLICATIONS, WORKERS, ProvenanceReceipt, Rollback, RollbackClient
from worker_container_rollback import check_url, control_client, validate_receipt_against_live

CUTOVER, STORE = [475, 479, 480, 481, 482], [478]
FENCE_ACTOR_PREFIX = "production-schema-release:"


def fence_actor(pin):
    return f"{FENCE_ACTOR_PREFIX}{pin.run_id}:{pin.candidate_sha}"


def pair_matches(state, pin, version, sha, digest):
    worker, image = mapping(state.get("worker")), mapping(state.get("container"))
    return (worker.get("version_id") == version and worker.get("sha") == sha
            and worker.get("traffic_percent") == 100 and image.get("image_digest") == digest
            and image.get("application_id") == pin.application_id
            and image.get("namespace_id") == pin.namespace_id and image.get("rollout_status") == "completed")


def stopped_fence(state, pin):
    owner = mapping(state.get("ownership"))
    require(owner.get("phase") == "fenced" and owner.get("running") is False
            and type(owner.get("epoch")) is int and owner["epoch"] == pin.fence_epoch
            and owner.get("owner_actor") == fence_actor(pin), "production_stopped_fence_unproved")


def verify_fence_ack(ack, pin, candidate_version):
    owner = mapping(ack.get("owner"))
    require(ack.get("deploymentId") in (pin.worker_version, candidate_version) and ack.get("running") is False
            and owner.get("phase") == "fenced" and "deploymentId" in owner and owner["deploymentId"] is None
            and owner.get("actor") == fence_actor(pin) and type(owner.get("epoch")) is int
            and owner["epoch"] == pin.fence_epoch and type(owner.get("oldEpoch")) is int
            and owner["oldEpoch"] == pin.fence_epoch - 1, "production_stopped_fence_ack_invalid")


def establish_fence(pin, pair, observe, control):
    state = observe()
    require(any(pair_matches(state, pin, *members) for members in (
        pair, (pin.worker_version, pin.rollback_sha, pin.image_digest),
    )), "production_inverse_serving_pair_changed")
    owner = mapping(state.get("ownership"))
    require(owner.get("phase") in ("active", "fenced") and type(owner.get("epoch")) is int
            and owner["epoch"] == pin.fence_epoch - 1, "production_inverse_fence_epoch_changed")
    version = state["worker"]["version_id"]
    ack = control("fence-exact", pin.fence_epoch - 1, expected_deployment=version)
    verify_fence_ack(ack, pin, pair[0])
    after = observe()
    require(after.get("worker") == state["worker"] and after.get("container") == state["container"],
            "production_inverse_serving_pair_changed")
    stopped_fence(after, pin)
    return {"deploymentId": ack["deploymentId"], "running": False, "owner": {
        key: ack["owner"][key] for key in ("deploymentId", "phase", "epoch", "oldEpoch", "actor")
    }}


def inverse_under_fence(pin, candidate, ack, run, api, inverse):
    # The caller holds the production lane through the whole release. The
    # production-owned job confirms the durable stop twice before handing off
    # its exact epoch. Authenticate that job again at the write boundary.
    verify_fence_ack(ack, pin, candidate[0])
    protected_job(run, api, FENCE_JOB, (FENCE_STEP,))
    return inverse()


def restore_under_fence(pin, result, observe, control, rollback, image, candidate):
    verify_inverse(pin, result)
    state = observe()
    restored = (pin.worker_version, pin.rollback_sha, pin.image_digest)
    if not pair_matches(state, pin, *restored):
        # Resume a partial restore only when both components individually
        # match a reviewed endpoint, in the order this operation writes them.
        worker = mapping(state.get("worker"))
        prior_version = worker.get("version_id") == pin.worker_version and worker.get("sha") == pin.rollback_sha
        require(pair_matches(state, pin, *candidate)
                or (prior_version and pair_matches(state, pin, *restored[:2], candidate[2])),
                "production_inverse_serving_pair_changed")
        stopped_fence(state, pin)
        if not prior_version:
            try:
                rollback.post_worker_restore(pin.worker_version)
            except GateError:
                state = observe()
                stopped_fence(state, pin)
                if not (pair_matches(state, pin, *restored) or pair_matches(state, pin, *restored[:2], candidate[2])):
                    raise
            rollback.wait_worker_restore(pin.worker_version)
        state = observe()
        stopped_fence(state, pin)
        require(pair_matches(state, pin, *restored) or pair_matches(state, pin, *restored[:2], candidate[2]),
                "production_inverse_serving_pair_changed")
        if not pair_matches(state, pin, *restored):
            try:
                rollback.restore_image(image)
            except GateError:
                state = observe()
                stopped_fence(state, pin)
                if not pair_matches(state, pin, *restored):
                    raise
    def observe_restored():
        observed = observe()
        if mapping(observed.get("ownership")).get("phase") == "fenced":
            stopped_fence(observed, pin)
        return observed

    try:
        complete_legacy_takeover(pin, result, observe_restored, control)
    except GateError:
        # A lost reply may have committed. Reconcile the exact actor and
        # single epoch increment without submitting another POST.
        if observed_phase(pin, observe_restored) != "active":
            raise
    rollback.wait_stable(pin.worker_version, rollback.now())


def release_admission(env, run, api=github):
    admission = mapping(decode(env["RECOVERY_ADMISSION"].encode()))
    required = {"pin", "plan_run_id", "candidate_worker", "candidate_image"}
    require(set(admission) == required, "production_inverse_admission_invalid")
    pin = RecoveryPin(**mapping(admission["pin"]))
    verify_inverse(pin, {"state": "restored", **asdict(pin)})
    candidate_ver, candidate_image = admission["candidate_worker"], admission["candidate_image"]
    plan_id = admission["plan_run_id"]
    require(pin.run_id == str(run["id"]) and matches(RUN_ID, plan_id) and plan_id != pin.run_id
            and matches(UUID, candidate_ver) and candidate_ver != pin.worker_version
            and matches(DIGEST, candidate_image) and candidate_image != pin.image_digest,
            "production_inverse_admission_invalid")
    plan, artifact = authenticated_plan(plan_id, api, workflow=WORKFLOW_PATH, artifact_name=PLAN_ARTIFACT)
    target = {"host": env["PRODUCTION_HOST"], "database": env["PRODUCTION_DATABASE"],
              "branch_id": env["PRODUCTION_BRANCH_ID"]}
    reference = digest_json({"operation": "undo-0480", "candidate_sha": pin.candidate_sha,
                             "candidate_worker": candidate_ver, "candidate_image": candidate_image,
                             "rollback_sha": pin.rollback_sha, "rollback_worker": pin.worker_version,
                             "rollback_image": pin.image_digest, "application_id": pin.application_id,
                             "namespace_id": pin.namespace_id, "workflow_sha": run["head_sha"]})
    require(plan.get("source_sha") == pin.candidate_sha and plan.get("target") == target
            and digest_json(target) == pin.database_fingerprint and plan.get("migration_target") == "production"
            and plan.get("mode") == "plan" and plan.get("role") == "two_bot_migrator_ro"
            and plan.get("recovery_evidence_ref") == f"inverse-{reference}"
            and plan.get("pending_before") == CUTOVER and plan.get("pending_store_before") == STORE
            and plan.get("plan_manifest_sha256") == projection_hash(plan)
            and plan.get("store_plan_manifest_sha256") == store_projection_hash(plan),
            "production_inverse_plan_mismatch")
    expanded = [[dict(row, success=True) for row in plan[key]]
                for key in ("source_migrations", "source_store_migrations")]
    restored = [[row for row in rows if row["version"] not in removed]
                for rows, removed in zip(expanded, (CUTOVER[1:], STORE), strict=True)]
    require(digest_json(expanded) == pin.expanded_ledgers_sha256
            and digest_json(restored) == pin.restored_ledgers_sha256, "production_inverse_ledger_pin_invalid")
    approval = {"plan_run_id": plan_id, "plan_archive_sha256": artifact["archive_digest"],
                "workflow_sha": run["head_sha"], "recovery_ref": plan["recovery_evidence_ref"],
                "candidate_worker": candidate_ver, "candidate_image": candidate_image}
    return pin, (candidate_ver, pin.candidate_sha, candidate_image), plan, approval


def run_inverse(binary, directory, pin, plan, approval, env):
    plan_path = directory / "reviewed-plan.json"
    plan_path.write_text(json.dumps(plan), encoding="utf-8")
    target = plan["target"]
    args = [binary, "--target", "production", "--apply", "--source-sha", pin.candidate_sha,
            "--production-host", target["host"], "--production-database", target["database"],
            "--production-branch-id", target["branch_id"], "--recovery-evidence-ref", approval["recovery_ref"],
            "--acl-plan-ref", plan["acl_plan_ref"], "--expected-pending", "475,479,480,481,482",
            "--expected-store-pending", "478", "--plan-manifest-sha256", plan["plan_manifest_sha256"],
            "--store-plan-manifest-sha256", plan["store_plan_manifest_sha256"], "--plan-run-id", approval["plan_run_id"],
            "--plan-manifest-path", str(plan_path), "--inverse-attendance-fence-sha256", pin.attendance_fence_sha256]
    binding = "TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL"
    require(bool(env.get(binding)), "production_inverse_binding_missing")
    result = subprocess.run(args, env={binding: env[binding], "TWO_DATABASE_TLS": "required"},
                            capture_output=True, timeout=120, check=False)
    require(result.returncode == 0, "production_inverse_failed_reconcile_under_fence")
    outcome = mapping(decode(result.stdout))
    require(outcome.get("state") == "restored" and outcome.get("attendance_fence_sha256") == pin.attendance_fence_sha256
            and digest_json(outcome.get("expanded")) == pin.expanded_ledgers_sha256
            and digest_json(outcome.get("restored")) == pin.restored_ledgers_sha256,
            "production_inverse_database_readback_invalid")
    return {"state": "restored", **asdict(pin)}


def main(argv, env):
    try:
        run = current_run(env)
        pin, candidate, plan, approval = release_admission(env, run)
        verify_environment(github)
        verify_check(github, run, f"production-apply:{approval['plan_run_id']}:"
                     f"{approval['plan_archive_sha256'].removeprefix('sha256:')}", datetime.now(timezone.utc))

        def observe():
            return read_status(env)

        def retained_pair():
            account = env["CLOUDFLARE_ACCOUNT_ID"]
            client = RollbackClient(account, env["CLOUDFLARE_API_TOKEN"])
            worker, application = WORKERS["production"], APPLICATIONS["production"]
            image = f"registry.cloudflare.com/{account}/{application}@{pin.image_digest}"
            receipt = ProvenanceReceipt(worker, application, pin.worker_version, pin.rollback_sha, image)
            validate_receipt_against_live(client, receipt)
            return client, receipt

        if argv == ["fence"]:
            url = check_url("production", env["PRODUCTION_WORKER_URL"])
            retained_pair()
            control = control_client("production", env["OWNERSHIP_CONTROL_TOKEN"], url, "", fence_actor(pin))
            ack = establish_fence(pin, candidate, observe, control)
            with Path(env["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as output:
                print(f"ack={json.dumps(ack, separators=(',', ':'))}", file=output)
            return 0

        ack = mapping(decode(env["STOPPED_FENCE_ACK"].encode()))
        if len(argv) == 3 and argv[0] == "inverse":
            directory = Path(argv[2])
            directory.mkdir(mode=0o700)
            result = inverse_under_fence(pin, candidate, ack, current_run(env), github,
                                         lambda: run_inverse(argv[1], directory, pin, plan, approval, env))
            (directory / "production-release-inverse.json").write_text(json.dumps({
                "operation": "undo-0480", "approval": approval, "result": result,
            }) + "\n", encoding="utf-8")
        elif argv == ["restore"]:
            verify_fence_ack(ack, pin, candidate[0])
            result = authenticated_inverse(pin, approval, env["ARTIFACT_ID"], env["ARCHIVE_SHA256"], env)
            url = check_url("production", env["PRODUCTION_WORKER_URL"])
            client, receipt = retained_pair()
            control = control_client("production", env["OWNERSHIP_CONTROL_TOKEN"], url, "", pin.actor)
            rollback = Rollback(client, control, url, "production", receipt.worker, receipt.application, pin.application_id,
                                pin.rollback_sha, pin.run_id)
            restore_under_fence(pin, result, observe, control, rollback, receipt.image, candidate)
        else:
            raise GateError("production_inverse_operation_invalid")
    except (GateError, ProofError, KeyError, TypeError, ValueError, OSError, subprocess.SubprocessError):
        print("::error::Protected production recovery refused or interrupted. Reconcile the fence and both ledgers.",
              file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:], os.environ))
