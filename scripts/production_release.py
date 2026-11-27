"""Protected attendance release entry points. All provider reads and writes use exact pins."""

from functools import partial
import json
import os
from pathlib import Path
import re
import subprocess
import sys
from time import sleep

import production_recovery_artifact as artifacts
from production_attendance_plan import validate_attendance_plan
from production_ownership_status import read_status
from production_recovery_takeover import HEX, LEGACY_RELEASE_WORKER_SHA, UUID, RecoveryPin
from production_release_approval import require_unused_attendance_plan
from production_release_ledgers import CUTOVER, STORE, ledger_endpoints
from production_release_preflight import (ReleasePreflight, approved_bound_plan, approved_plan,
                                          apply_under_bound_fence, bound_plan, prepared_epoch, stop_intent,
                                          verify_preflight)
from rollback_proof import ProofError
from rollback_target_preflight import command, image_in_registry
from staging_migrate_approval import digest_json
from staging_recovery_status import github, matches
from staging_rollout import GateError, decode, mapping, require
from worker_container_rollback import APPLICATIONS, WORKERS, ProvenanceReceipt, RollbackClient
from worker_container_rollback import WORKER_RESTORE_POLL_SECONDS, WORKER_RESTORE_READS
from worker_container_rollback import check_url, control_client, validate_receipt_against_live


def release_request(env, api=github):
    run = artifacts.current_run(env, api)
    pin = ReleasePreflight(workflow_sha=run["head_sha"], candidate_sha=env["RELEASE_SHA"],
                           **mapping(decode(env["RELEASE_PINS"].encode())))
    return pin, run, env["RELEASE_PLAN_ID"]


def release_context(env, api=github):
    pin, run, plan_id = release_request(env, api)
    plan, artifact = approved_bound_plan(pin, run, plan_id, env["RELEASE_TARGET_FINGERPRINT"], api)
    return pin, run, plan_id, plan, artifact


def forward_document(env, pin, run, plan_id, plan, artifact, api=github):
    stopped = mapping(decode(env["RELEASE_STOPPED"].encode()))
    epoch = prepared_epoch(pin, run, plan_id, stopped, preflight_epoch=decode(env["RELEASE_PREFLIGHT_EPOCH"].encode()))
    artifacts.protected_job(run, api, artifacts.FORWARD_JOB, artifacts.FORWARD_STEPS)
    document = artifacts.authentic_document(run, artifacts.FORWARD_ARTIFACT, env["FORWARD_ARTIFACT_ID"],
                                           env["FORWARD_ARCHIVE_SHA256"], api)
    _, expanded, restored = ledger_endpoints(plan)
    expected = {"plan_run_id": plan_id, "source_sha": pin.candidate_sha, "run_id": str(run["id"]),
                "run_attempt": 1, "database_fingerprint": digest_json(plan["target"]),
                "expanded_ledgers_sha256": digest_json(expanded), "restored_ledgers_sha256": digest_json(restored),
                "stopped_epoch": epoch, "plan_archive_sha256": artifact["archive_digest"],
                "release_reference": pin.recovery_reference()}
    require(set(document) == set(expected) | {"attendance_fence_sha256"}
            and matches(HEX, document["attendance_fence_sha256"])
            and all(type(document[key]) is type(value) and document[key] == value for key, value in expected.items()),
            "production_forward_artifact_mismatch")
    return RecoveryPin(str(run["id"]), 1, pin.candidate_sha, pin.rollback_sha, pin.rollback_worker,
                       pin.rollback_image, pin.application_id, pin.namespace_id, document["database_fingerprint"],
                       document["expanded_ledgers_sha256"], document["restored_ledgers_sha256"],
                       document["attendance_fence_sha256"], epoch)


def applied_release(env, api=github):
    """Continue a committed attempt using the result that bound its approved plan."""
    pin, run, plan_id = release_request(env, api)
    plan, artifact = bound_plan(pin, run, plan_id, env["RELEASE_TARGET_FINGERPRINT"], api)
    recovery = forward_document(env, pin, run, plan_id, plan, artifact, api)
    return pin, run, plan_id, plan, artifact, recovery


def release_actor(run_id, candidate_sha):
    return f"production-schema-release:{run_id}:{candidate_sha}"


def stopped_ack(ack, epoch, actor, version):
    owner = mapping(ack.get("owner"))
    require(ack.get("deploymentId") == version and ack.get("running") is False
            and owner.get("phase") == "fenced" and "deploymentId" in owner and owner["deploymentId"] is None
            and owner.get("actor") == actor and type(owner.get("epoch")) is int and owner["epoch"] == epoch
            and type(owner.get("oldEpoch")) is int and owner["oldEpoch"] == epoch - 1,
            "production_stopped_fence_ack_invalid")
    return {"deploymentId": version, "running": False, "owner": {
        key: owner[key] for key in ("deploymentId", "phase", "epoch", "oldEpoch", "actor")
    }}


def is_stopped(state, epoch, actor):
    owner = mapping(state.get("ownership"))
    return (owner.get("phase") == "fenced" and owner.get("running") is False
            and type(owner.get("epoch")) is int and owner["epoch"] == epoch and owner.get("owner_actor") == actor)


def matches_pair(state, pin, version, sha, digest, *, completed=False):
    worker, image = mapping(state.get("worker")), mapping(state.get("container"))
    return (worker.get("version_id") == version and worker.get("sha") == sha
            and worker.get("traffic_percent") == 100 and image.get("image_digest") == digest
            and image.get("application_id") == pin.application_id and image.get("namespace_id") == pin.namespace_id
            and (not completed or image.get("rollout_status") == "completed"))


def stop_prior_pair(pin, run_id, epoch, observe, control):
    actor = release_actor(run_id, pin.candidate_sha)
    before = observe()
    owner = mapping(before.get("ownership"))
    require(matches_pair(before, pin, pin.rollback_worker, pin.rollback_sha, pin.rollback_image, completed=True)
            and owner.get("phase") == "active" and type(owner.get("epoch")) is int and owner["epoch"] == epoch
            and owner.get("running") is True, "production_release_retained_pair_changed")
    action = "fence-legacy" if pin.rollback_sha == LEGACY_RELEASE_WORKER_SHA else "fence-exact"
    ack = stopped_ack(control(action, epoch, expected_deployment=pin.rollback_worker),
                      epoch + 1, actor, pin.rollback_worker)
    state = observe()
    require(state.get("worker") == before["worker"] and state.get("container") == before["container"]
            and is_stopped(state, epoch + 1, actor), "production_stopped_fence_unproved")
    return ack


def produced_version(output):
    # Wrangler deploy prints the new version ID. A subsequent provider
    # listing alone could be a concurrent secret-only Worker publish.
    versions = re.findall(r"^Current Version ID: ([0-9a-f-]+)\r?$", output, re.MULTILINE)
    require(len(versions) == 1 and UUID.fullmatch(versions[0]), "production_release_worker_version_unproved")
    return versions[0]


def take_over(pin, recovery, version, observe, control, *, pause=sleep):
    require(UUID.fullmatch(version) and version != pin.rollback_worker, "production_release_worker_version_unproved")
    actor = release_actor(recovery.run_id, pin.candidate_sha)
    for attempt in range(WORKER_RESTORE_READS):
        if attempt:
            pause(WORKER_RESTORE_POLL_SECONDS)
        state = observe()
        require(is_stopped(state, recovery.fence_epoch, actor), "production_stopped_fence_unproved")
        if matches_pair(state, pin, version, pin.candidate_sha, pin.candidate_image, completed=True):
            break
        require(any(matches_pair(state, pin, worker, sha, digest)
                    for worker, sha in ((pin.rollback_worker, pin.rollback_sha), (version, pin.candidate_sha))
                    for digest in (pin.rollback_image, pin.candidate_image)),
                "production_release_candidate_pair_unproved")
    else:
        raise GateError("production_release_candidate_pair_unproved")
    control("takeover-fenced", recovery.fence_epoch, expected_deployment=version)
    state = observe()
    owner = mapping(state.get("ownership"))
    require(matches_pair(state, pin, version, pin.candidate_sha, pin.candidate_image, completed=True)
            and owner.get("phase") == "active" and owner.get("epoch") == recovery.fence_epoch + 1
            and owner.get("owner_actor") == actor and owner.get("running") is False,
            "production_release_takeover_unconfirmed")


def database(binary, directory, plan, env, *, plan_id=None):
    target = plan["target"]
    args = [binary, "--target", "production", "--source-sha", plan["source_sha"],
            "--production-host", target["host"], "--production-database", target["database"],
            "--production-branch-id", target["branch_id"], "--recovery-evidence-ref", plan["recovery_evidence_ref"],
            "--acl-plan-ref", plan["acl_plan_ref"], "--expected-pending", ",".join(map(str, CUTOVER))]
    if plan_id is None:
        args.append("--plan")
        binding = "TWO_BOT_PRODUCTION_PLAN_DATABASE_URL"
    else:
        plan_path = directory / "reviewed-plan.json"
        plan_path.write_text(json.dumps(plan), encoding="utf-8")
        args.extend(["--apply", "--expected-store-pending", ",".join(map(str, STORE)),
                     "--plan-manifest-sha256", plan["plan_manifest_sha256"],
                     "--store-plan-manifest-sha256", plan["store_plan_manifest_sha256"],
                     "--plan-run-id", plan_id, "--plan-manifest-path", str(plan_path)])
        binding = "TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL"
    require(bool(env.get(binding)), "production_release_binding_missing")
    outcome = subprocess.run(args, env={binding: env[binding], "TWO_DATABASE_TLS": "required"},
                             capture_output=True, check=False, timeout=120)
    require(outcome.returncode == 0, "production_release_database_refused_or_uncertain")
    return mapping(decode(outcome.stdout))


def write_json(path, value):
    path.write_text(json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n", encoding="utf-8")


def output(env, **fields):
    with Path(env["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as handle:
        handle.writelines(f"{key}={value}\n" for key, value in fields.items())


def main(argv, env):
    try:
        if argv == ["version"]:
            output(env, worker=produced_version(env["WRANGLER_DEPLOY_OUTPUT"]))
            return 0
        directory = Path(env["RUNNER_TEMP"]) / "production-release"
        directory.mkdir(mode=0o700, exist_ok=True)
        if argv == ["plan"]:
            pin, run, plan_id = release_request(env)
            target = {key: env["PRODUCTION_" + key.upper()] for key in ("host", "database", "branch_id")}
            require(all(target.values()), "production_release_target_unconfigured")
            plan, _ = approved_plan(pin, run, plan_id, target)
            fresh = database(env["RELEASE_RUNNER"], directory, plan, env)
            require(fresh.get("target") == target, "production_release_fresh_target_changed")
            write_json(directory / "fresh-plan.json", fresh)
            output(env, **{"target-fingerprint": digest_json(target)})
            return 0
        if argv == ["apply"]:
            pin, run, plan_id = release_request(env)
            stopped = mapping(decode(env["RELEASE_STOPPED"].encode()))
            receipt = apply_under_bound_fence(
                pin, run, plan_id, env["RELEASE_TARGET_FINGERPRINT"], stopped,
                lambda approved_id, approved: database(env["RELEASE_RUNNER"], directory, approved, env,
                                                        plan_id=approved_id),
                preflight_epoch=decode(env["RELEASE_PREFLIGHT_EPOCH"].encode()),
            )
            write_json(directory / "production-release-forward.json", receipt)
            return 0

        if argv in (["authorize-deploy"], ["takeover"]):
            pin, run, _, _, _, recovery = applied_release(env)
        else:
            pin, run, plan_id, plan, artifact = release_context(env)

        actor = release_actor(str(run["id"]), pin.candidate_sha)
        url = check_url("production", env["PRODUCTION_WORKER_URL"])
        make_control = partial(control_client, "production", env["OWNERSHIP_CONTROL_TOKEN"], url, "", actor)
        if argv == ["prepare"]:
            fresh = mapping(decode((directory / "fresh-plan.json").read_bytes()))
            validate_attendance_plan(plan)
            validate_attendance_plan(fresh)
            require_unused_attendance_plan(github, artifact, run)
            epoch = verify_preflight(pin, plan["target"], plan, fresh, lambda: read_status(env), make_control())
            account = env["CLOUDFLARE_ACCOUNT_ID"]
            client = RollbackClient(account, env["CLOUDFLARE_API_TOKEN"])
            app = APPLICATIONS["production"]
            image = f"registry.cloudflare.com/{account}/{app}@{pin.rollback_image}"
            validate_receipt_against_live(client, ProvenanceReceipt(
                WORKERS["production"], app, pin.rollback_worker, pin.rollback_sha, image))
            image_in_registry(image, account, partial(command, cwd=Path(__file__).resolve().parents[1] / "wrangler"))
            write_json(directory / f"{artifacts.STOP_INTENT_ARTIFACT}.json",
                       stop_intent(pin, run, plan_id, plan, artifact, epoch))
            output(env, epoch=epoch, archive=pin.candidate_archive, image=pin.candidate_image)
        elif argv == ["stop"]:
            proof = mapping(decode((directory / f"{artifacts.STOP_INTENT_ARTIFACT}.json").read_bytes()))
            require(type(proof.get("epoch")) is int and proof == stop_intent(pin, run, plan_id, plan, artifact, proof["epoch"]),
                    "production_release_preflight_plan_changed")
            require_unused_attendance_plan(github, artifact, run)
            ack = stop_prior_pair(pin, str(run["id"]), proof["epoch"], lambda: read_status(env), make_control())
            write_json(directory / "stopped.json", ack)
            output(env, ack=json.dumps(ack, separators=(",", ":")))
        elif argv in (["authorize-deploy"], ["takeover"]):
            if argv == ["authorize-deploy"]:
                state = read_status(env)
                require(matches_pair(state, pin, pin.rollback_worker, pin.rollback_sha, pin.rollback_image)
                        and is_stopped(state, recovery.fence_epoch, actor), "production_stopped_fence_unproved")
                output(env, archive=pin.candidate_archive, image=pin.candidate_image)
            else:
                take_over(pin, recovery, env["NEW_VERSION"],
                          lambda: read_status(env, allow_rollout=True), make_control())
        else:
            raise GateError("production_release_operation_invalid")
    except (GateError, ProofError, KeyError, TypeError, ValueError, OSError, subprocess.SubprocessError):
        print("::error::Protected production release refused or interrupted. Preserve and reconcile the evidence.",
              file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:], os.environ))
