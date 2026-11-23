"""Prepare and attest protected, no-start staging recovery deployments."""

import json
import os
from pathlib import Path
import sys
from time import monotonic

from ci.staging_rollback_compatibility import Refused as IncompatibleSource, verify as verify_migration_sources
from script_intake import read_with_overflow
from staging_recovery_status import (
    APPLICATION, HEX64, MAX_OWNER, MAX_PLAN, ORIGIN, WORKER,
    matches, plan_projection, provider_sample, read_ownership,
)
from staging_rollout import Client, GateError, decode, mapping, require, save
from worker_container_rollback import SHA, VERSION_ID, deployments, version_sha

PIN = "STAGING_RECOVERY_DB_FINGERPRINT"


def read_json(path, limit=MAX_OWNER):
    with path.open("rb") as reader:
        raw = read_with_overflow(reader, limit)
    require(len(raw) <= limit, "recovery_file_too_large")
    return mapping(decode(raw))


def verified_plan(plan, source):
    proof = plan_projection(plan)
    require(proof["source_sha"] == source, "recovery_plan_source_mismatch")
    require(any(row["version"] == 475 for row in plan["ledger_before"]), "recovery_plan_missing_0475")
    return {**proof, "cutover_0475_applied": True}


def pin_config(config, fingerprint):
    tables = [mapping(config.get("vars", {}))]
    environments = mapping(config.get("env"))
    tables.extend(mapping(mapping(env).get("vars", {})) for env in environments.values())
    require(all(PIN not in variables for variables in tables), "recovery_pin_already_configured")
    staging = mapping(mapping(environments.get("staging")).get("vars"))
    require(staging.get("TWO_DEPLOY_ENV") == "staging" and staging.get("STAGING_RUNTIME_POLICY") == "bounded",
            "recovery_requires_bounded_staging")
    staging[PIN] = fingerprint


def attest(client, read, version, source, fingerprint=None, *, active_owner=False):
    require(matches(VERSION_ID, version) and matches(SHA, source), "recovery_version_invalid")
    require(deployments(client, WORKER)[0] == version and version_sha(client, WORKER, version) == source,
            "recovery_serving_version_mismatch")
    state = mapping(read(fingerprint))
    require(state.get("deploymentId") == version and state.get("running") is False,
            "recovery_worker_not_stopped_or_mismatched")
    if active_owner:
        owner = mapping(state.get("owner"))
        require(owner.get("phase") == "active" and owner.get("deploymentId") == version,
                "recovery_predecessor_not_owner")
    if fingerprint is not None:
        require(state.get("runtimeDatabaseFingerprint") == fingerprint
                and state.get("runtimeDatabaseStartupGuard") is True, "recovery_database_unverified")
    require(deployments(client, WORKER)[0] == version, "recovery_serving_version_mismatch")


def prepare(mode, root, repo, client, account, read, source, fingerprint, expected_pair=""):
    snapshot = read_json(root / "staging-rollback-snapshot.json")
    require(snapshot.get("has_previous") is True, "recovery_predecessor_unavailable")
    prior, prior_source = snapshot.get("serving_version"), snapshot.get("serving_sha")
    if mode == "verified":
        verify_migration_sources(repo, source, source, prior_source)
        pair = provider_sample(client, account)
        image = pair["container"]["image_digest"]
        require(expected_pair == f"{prior}@{image}", "recovery_pair_not_pinned")
        require(pair["worker"]["version_id"] == prior and pair["worker"]["source_sha"] == prior_source
                and pair["container"]["application_id"] == snapshot.get("application_id")
                and pair["container"]["namespace_id"] == snapshot.get("namespace_id")
                and pair["container"]["rollout_id"] == snapshot.get("serving_rollout_id")
                and snapshot.get("serving_image") == f"registry.cloudflare.com/{account}/{APPLICATION}@{image}",
                "recovery_pair_mismatch")
        attest(client, read, prior, prior_source, fingerprint, active_owner=True)
        require(provider_sample(client, account) == pair, "recovery_pair_drift")
    else:
        attest(client, read, prior, prior_source)
    path = root / "staging-deploy.json"
    config = read_json(path, MAX_PLAN)
    pin_config(config, fingerprint)
    save(path, config)


def main(argv, env):
    try:
        source = env.get("DEPLOY_SHA")
        require(matches(SHA, source) and source == env.get("GITHUB_SHA"), "recovery_requires_current_main")
        if argv == ["plan"]:
            proof = verified_plan(read_json(Path("staging-schema-gate-manifest.json"), MAX_PLAN), source)
            with Path(env["GITHUB_OUTPUT"]).open("a") as out:
                out.write(f"fingerprint={proof['target_fingerprint']}\n")
            print(json.dumps(proof, sort_keys=True))
            return 0
        mode, fingerprint = env.get("RECOVERY_MODE"), env.get("RECOVERY_FINGERPRINT")
        require(mode in ("bootstrap", "verified") and matches(HEX64, fingerprint), "recovery_plan_unavailable")
        token, account = env.get("OWNERSHIP_CONTROL_TOKEN", ""), env.get("CLOUDFLARE_ACCOUNT_ID")
        require(env.get("STAGING_WORKER_URL") in (ORIGIN, ORIGIN + "/") and 32 <= len(token) <= 4096,
                "recovery_control_unavailable")
        client = Client(account, env.get("CLOUDFLARE_API_TOKEN"), deadline=monotonic() + 95)
        root = Path(env["ROLLOUT_DIR"])
        if argv == ["prepare"]:
            prepare(mode, root, ".", client, account, lambda expected: read_ownership(token, expected),
                    source, fingerprint, env.get("RECOVERY_PAIR", ""))
        else:
            require(argv == ["verify"], "recovery_operation_invalid")
            version = read_json(root / "receipt.json").get("worker_version")
            attest(client, lambda expected: read_ownership(token, expected), version, source, fingerprint)
            proof = {"worker_version": version, "source_sha": source, "target_fingerprint": fingerprint,
                     "running": False, "startup_guard": True}
            save(root / "staging-runtime-binding.json", proof)
        return 0
    except (GateError, IncompatibleSource) as error:
        print(f"staging recovery refused: {error}", file=sys.stderr)
    except Exception:
        print("staging recovery refused: invalid_or_unavailable_evidence", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], os.environ))
