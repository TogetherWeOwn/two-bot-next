"""Run the protected database inverse and complete a fenced provider restore."""

from dataclasses import asdict, replace
import json
import os
from pathlib import Path
import subprocess
import sys

from production_ownership_status import read_status
from production_recovery_artifact import FENCE_JOB, FENCE_STEP, authenticated_inverse, protected_job
from production_recovery_takeover import UUID, complete_legacy_takeover, observed_phase, verify_inverse
from production_release import forward_document, is_stopped, matches_pair, release_actor, release_context, stopped_ack, write_json
from production_release_ledgers import CUTOVER, STORE
from rollback_proof import ProofError
from staging_migrate_approval import digest_json
from staging_recovery_status import github
from staging_rollout import GateError, decode, mapping, require
from worker_container_rollback import APPLICATIONS, WORKERS, ProvenanceReceipt, Rollback, RollbackClient
from worker_container_rollback import check_url, control_client, validate_receipt_against_live


def fence_actor(pin):
    return release_actor(pin.run_id, pin.candidate_sha)


def pair_matches(state, pin, version, sha, digest):
    return matches_pair(state, pin, version, sha, digest) and state["container"].get("rollout_status") == "completed"


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
        state = observe()
        stopped_fence(state, pin)
        require(state["worker"]["version_id"] == pin.worker_version and recoverable_pair(state, pin, candidate),
                "production_inverse_serving_pair_changed")
        if not pair_matches(state, pin, *restored):
            # A prior-image rollout may already be in flight. Leave the
            # fence in place and avoid submitting a duplicate image write.
            require(state["container"]["image_digest"] != pin.image_digest,
                    "production_recovery_prior_rollout_pending")
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


def release_admission(env):
    release, run, plan_id, plan, artifact = release_context(env)
    pin = forward_document(env, release, run, plan_id, plan, artifact)
    version = env["CANDIDATE_WORKER"]
    require(version == "" or UUID.fullmatch(version) and version != pin.worker_version,
            "production_inverse_admission_invalid")
    approval = {"plan_run_id": plan_id, "plan_archive_sha256": artifact["archive_digest"],
                "workflow_sha": run["head_sha"], "recovery_ref": plan["recovery_evidence_ref"],
                "candidate_worker": version, "candidate_image": release.candidate_image,
                "forward_artifact_id": env["FORWARD_ARTIFACT_ID"], "forward_archive_sha256": env["FORWARD_ARCHIVE_SHA256"]}
    return pin, (version, pin.candidate_sha, release.candidate_image), plan, approval, run


def run_inverse(binary, directory, pin, plan, approval, env):
    plan_path = directory / "reviewed-plan.json"
    plan_path.write_text(json.dumps(plan), encoding="utf-8")
    target = plan["target"]
    args = [binary, "--target", "production", "--apply", "--source-sha", pin.candidate_sha,
            "--production-host", target["host"], "--production-database", target["database"],
            "--production-branch-id", target["branch_id"], "--recovery-evidence-ref", approval["recovery_ref"],
            "--acl-plan-ref", plan["acl_plan_ref"], "--expected-pending", ",".join(map(str, CUTOVER)),
            "--expected-store-pending", ",".join(map(str, STORE)), "--plan-manifest-sha256", plan["plan_manifest_sha256"],
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
