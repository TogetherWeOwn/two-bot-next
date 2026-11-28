"""Run the protected database inverse and complete a fenced provider restore."""

from dataclasses import asdict, replace
import json
import os
from pathlib import Path
import subprocess
import sys

from production_ownership_status import read_status
from production_recovery_artifact import FENCE_JOB, FENCE_STEP, authenticated_inverse, protected_job
from production_recovery_takeover import (HEX, RUN_ID, UUID, complete_legacy_takeover, observed_phase,
                                          require_same_provider_pair, retained_takeover_action, verify_inverse)
from production_release import applied_release, is_stopped, matches_pair, release_actor, stopped_ack, write_json
from rollback_proof import ProofError
from staging_migrate_approval import digest_json
from staging_recovery_status import github, matches
from staging_rollout import GateError, decode, mapping, require
from worker_container_rollback import APPLICATIONS, WORKERS, ProvenanceReceipt, Rollback, RollbackClient
from worker_container_rollback import WORKER_RESTORE_POLL_SECONDS, WORKER_RESTORE_READS
from worker_container_rollback import check_url, control_client, validate_receipt_against_live


def fence_actor(pin):
    return release_actor(pin.run_id, pin.candidate_sha)


def pair_matches(state, pin, version, sha, digest):
    return matches_pair(state, pin, version, sha, digest, completed=True)


def stopped_fence(state, pin):
    require(is_stopped(state, pin.fence_epoch, fence_actor(pin)), "production_stopped_fence_unproved")


def verify_fence_ack(ack, pin, candidate_version):
    require(ack.get("deploymentId") in (pin.worker_version, candidate_version),
            "production_stopped_fence_ack_invalid")
    return stopped_ack(ack, pin.fence_epoch, fence_actor(pin), ack["deploymentId"])


def recoverable_pair(state, pin, candidate):
    # A provider failure can leave either component at its reviewed endpoint.
    # An ambiguous Worker publish has no candidate ID and therefore only
    # permits recovery if the exact prior Worker still serves.
    return any(matches_pair(state, pin, version, sha, image)
               for version, sha in ((pin.worker_version, pin.rollback_sha), candidate[:2]) if version
               for image in (pin.image_digest, candidate[2]))


def establish_fence(pin, pair, observe, control):
    state = observe()
    require(recoverable_pair(state, pin, pair), "production_inverse_serving_pair_changed")
    actor = fence_actor(pin)
    version = state["worker"]["version_id"]
    if is_stopped(state, pin.fence_epoch, actor):
        # The authenticated forward stop is still in force, even when the
        # exact candidate version now serves. Preserve its original ack.
        return pin, None
    owner = mapping(state.get("ownership"))
    require(version == pair[0] and owner.get("phase") == "active" and owner.get("owner_actor") == actor
            and type(owner.get("epoch")) is int and owner["epoch"] == pin.fence_epoch + 1,
            "production_inverse_fence_epoch_changed")
    epoch = owner["epoch"]
    recovered = replace(pin, fence_epoch=epoch + 1)
    ack = stopped_ack(control("fence-exact", epoch, expected_deployment=version), epoch + 1, actor, version)
    after = observe()
    require(recoverable_pair(after, pin, pair) and after["worker"] == state["worker"],
            "production_inverse_serving_pair_changed")
    stopped_fence(after, recovered)
    return recovered, ack


def inverse_under_fence(pin, candidate, ack, run, api, inverse):
    # The caller holds the production lane through the whole release. The
    # production-owned job confirms the durable stop twice before handing off
    # its exact epoch. Authenticate that job again at the write boundary.
    verify_fence_ack(ack, pin, candidate[0])
    protected_job(run, api, FENCE_JOB, (FENCE_STEP,))
    return inverse()


def interrupted_classification(classification):
    classification = mapping(classification)
    state = classification.get("state")
    stamp = classification.get("attendance_fence_sha256")
    require((state == "pre" and set(classification) == {"state"})
            or (state == "post" and set(classification) == {"state", "attendance_fence_sha256"}
                and matches(HEX, stamp)), "production_interrupted_database_unproved")
    return state


def interrupted_pair_phase(pin, origin_run_id, epoch, observed, candidate_worker=None):
    require(matches(RUN_ID, origin_run_id) and type(epoch) is int and 1 < epoch < 2**53 - 1,
            "production_interrupted_origin_invalid")
    actor = release_actor(origin_run_id, pin.candidate_sha)
    if candidate_worker and pair_matches(observed, pin, candidate_worker, pin.candidate_sha, pin.candidate_image):
        require(is_stopped(observed, epoch, actor), "production_interrupted_stop_unproved")
        return "candidate-stopped"
    require(pair_matches(observed, pin, pin.rollback_worker, pin.rollback_sha, pin.rollback_image),
            "production_interrupted_prior_pair_changed")
    if is_stopped(observed, epoch, actor):
        return "stopped"
    owner = mapping(observed.get("ownership"))
    require(owner.get("phase") == "active" and type(owner.get("epoch")) is int
            and owner["epoch"] == epoch + 1 and owner.get("owner_actor") == actor,
            "production_interrupted_stop_unproved")
    return "resumed"


def reconcile_interrupted(pin, origin_run_id, epoch, classification, observe, control, recover_post,
                          *, candidate_worker=None):
    """Resume only from authenticated attempt, stop and database evidence."""
    state = interrupted_classification(classification)

    before = observe()
    phase = interrupted_pair_phase(pin, origin_run_id, epoch, before, candidate_worker)
    if state == "post":
        require(phase in ("stopped", "candidate-stopped"), "production_interrupted_stop_unproved")
        return recover_post(classification["attendance_fence_sha256"])

    require(phase != "candidate-stopped", "production_interrupted_pre_candidate_unproved")
    if phase == "stopped":
        def resumed():
            after = observe()
            require_same_provider_pair(before, after)
            return interrupted_pair_phase(pin, origin_run_id, epoch, after) == "resumed"

        try:
            control(retained_takeover_action(pin.rollback_sha), epoch, expected_deployment=pin.rollback_worker)
        except GateError:
            # The POST might have committed. An exact active readback can
            # confirm it, but a pending fence cannot authorize another POST.
            if not resumed():
                raise
        else:
            require(resumed(), "production_interrupted_takeover_unconfirmed")
    return {"state": "pre", "worker": pin.rollback_worker, "epoch": epoch + 1}


def wait_for_restore_rollout(pin, candidate, observe, pause):
    state = observe()
    worker = state["worker"]
    initial = state["container"]
    for attempt in range(WORKER_RESTORE_READS):
        if attempt:
            pause(WORKER_RESTORE_POLL_SECONDS)
            state = observe()
        stopped_fence(state, pin)
        image = state["container"]
        require(worker["version_id"] == pin.worker_version and state["worker"] == worker
                and recoverable_pair(state, pin, candidate) and image["rollout_id"] == initial["rollout_id"]
                and image["image_digest"] == initial["image_digest"], "production_inverse_serving_pair_changed")
        if image["rollout_status"] == "completed":
            return state
    raise GateError("production_recovery_rollout_pending")


def restore_under_fence(pin, result, observe, control, rollback, image, candidate):
    verify_inverse(pin, result)
    state = observe()
    restored = (pin.worker_version, pin.rollback_sha, pin.image_digest)
    if not pair_matches(state, pin, *restored):
        # Resume a partial restore only when both components individually
        # match a reviewed endpoint, in the order this operation writes them.
        worker = mapping(state.get("worker"))
        prior_version = worker.get("version_id") == pin.worker_version and worker.get("sha") == pin.rollback_sha
        require(recoverable_pair(state, pin, candidate), "production_inverse_serving_pair_changed")
        stopped_fence(state, pin)
        if not prior_version:
            try:
                rollback.post_worker_restore(pin.worker_version)
            except GateError:
                state = observe()
                stopped_fence(state, pin)
                if not (state["worker"]["version_id"] == pin.worker_version and recoverable_pair(state, pin, candidate)):
                    raise
            rollback.wait_worker_restore(pin.worker_version)
        state = wait_for_restore_rollout(pin, candidate, observe, rollback.sleep)
        if not pair_matches(state, pin, *restored):
            try:
                rollback.restore_image(image, writer_stopped=True)
            except GateError:
                state = wait_for_restore_rollout(pin, candidate, observe, rollback.sleep)
                if not pair_matches(state, pin, *restored):
                    raise

    def observe_restored():
        observed = observe()
        if mapping(observed.get("ownership")).get("phase") == "fenced":
            stopped_fence(observed, pin)
        return observed

    complete_legacy_takeover(pin, result, observe_restored, control)
    rollback.wait_stable(pin.worker_version, rollback.now())


def release_admission(env):
    release, run, plan_id, plan, artifact, pin = applied_release(env)
    version = env["CANDIDATE_WORKER"]
    require(version == "" or UUID.fullmatch(version) and version != pin.worker_version,
            "production_inverse_admission_invalid")
    approval = {"plan_run_id": plan_id, "plan_archive_sha256": artifact["archive_digest"],
                "workflow_sha": run["head_sha"], "recovery_ref": plan["recovery_evidence_ref"],
                "candidate_worker": version, "candidate_image": release.candidate_image,
                "forward_artifact_id": env["FORWARD_ARTIFACT_ID"], "forward_archive_sha256": env["FORWARD_ARCHIVE_SHA256"]}
    return pin, (version, pin.candidate_sha, release.candidate_image), plan, approval, run


def _run_recovery_database(binary, directory, plan_id, plan, env, *operation, source_sha, recovery_ref,
                           errors=("production_inverse_binding_missing", "production_inverse_failed_reconcile_under_fence")):
    plan_path = directory / "reviewed-plan.json"
    plan_path.write_text(json.dumps(plan), encoding="utf-8")
    target = plan["target"]
    args = [binary, "--target", "production", "--apply", "--source-sha", source_sha,
            "--production-host", target["host"], "--production-database", target["database"],
            "--production-branch-id", target["branch_id"], "--recovery-evidence-ref", recovery_ref,
            "--acl-plan-ref", plan["acl_plan_ref"], "--expected-pending", ",".join(map(str, plan["pending_before"])),
            "--expected-store-pending", ",".join(map(str, plan["pending_store_before"])),
            "--plan-manifest-sha256", plan["plan_manifest_sha256"],
            "--store-plan-manifest-sha256", plan["store_plan_manifest_sha256"], "--plan-run-id", plan_id,
            "--plan-manifest-path", str(plan_path), *operation]
    binding = "TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL"
    require(bool(env.get(binding)), errors[0])
    result = subprocess.run(args, env={binding: env[binding], "TWO_DATABASE_TLS": "required"},
                            capture_output=True, timeout=120, check=False)
    require(result.returncode == 0, errors[1])
    return mapping(decode(result.stdout))


def run_recovery_database(binary, directory, plan_id, plan, env, *operation):
    return _run_recovery_database(binary, directory, plan_id, plan, env, *operation,
                                  source_sha=plan["source_sha"], recovery_ref=plan["recovery_evidence_ref"],
                                  errors=("production_recovery_binding_missing",
                                          "production_recovery_database_refused_or_uncertain"))


def run_inverse(binary, directory, pin, plan, approval, env):
    outcome = _run_recovery_database(binary, directory, approval["plan_run_id"], plan, env,
                                     "--inverse-attendance-fence-sha256", pin.attendance_fence_sha256,
                                     source_sha=pin.candidate_sha, recovery_ref=approval["recovery_ref"])
    require(outcome.get("state") == "restored" and outcome.get("attendance_fence_sha256") == pin.attendance_fence_sha256
            and digest_json(outcome.get("expanded")) == pin.expanded_ledgers_sha256
            and digest_json(outcome.get("restored")) == pin.restored_ledgers_sha256,
            "production_inverse_database_readback_invalid")
    return {"state": "restored", **asdict(pin)}


def main(argv, env):
    try:
        pin, candidate, plan, approval, run = release_admission(env)

        def observe():
            return read_status(env, allow_rollout=True)

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
            pin, ack = establish_fence(pin, candidate, observe, control)
            if ack is None:
                ack = mapping(decode(env["RELEASE_STOPPED"].encode()))
            verify_fence_ack(ack, pin, candidate[0])
            write_json(Path(env["RUNNER_TEMP"]) / "production-release-recovery-stop.json", ack)
            with Path(env["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as output:
                print(f"ack={json.dumps(ack, separators=(',', ':'))}", file=output)
            return 0

        ack = mapping(decode(env["STOPPED_FENCE_ACK"].encode()))
        epoch = mapping(ack.get("owner")).get("epoch")
        require(type(epoch) is int and epoch in (pin.fence_epoch, pin.fence_epoch + 2),
                "production_inverse_fence_epoch_changed")
        pin = replace(pin, fence_epoch=epoch)
        if len(argv) == 3 and argv[0] == "inverse":
            directory = Path(argv[2])
            directory.mkdir(mode=0o700)
            result = inverse_under_fence(pin, candidate, ack, run, github,
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
            state = observe()
            actor = mapping(state.get("ownership")).get("owner_actor")
            require(actor == pin.actor and observed_phase(pin, lambda: state) == "active",
                    "production_recovery_takeover_unconfirmed")
            write_json(Path(env["RUNNER_TEMP"]) / "production-release-restored.json",
                       {"approval": approval, "inverse": result, "provider": state,
                        "stable_polls": rollback.evidence["stable_polls"]})
        else:
            raise GateError("production_inverse_operation_invalid")
    except (GateError, ProofError, KeyError, TypeError, ValueError, OSError, subprocess.SubprocessError):
        print("::error::Protected production recovery refused or interrupted. Reconcile the fence and both ledgers.",
              file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:], os.environ))
