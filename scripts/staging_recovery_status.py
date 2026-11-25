"""Bracket no-start staging ownership and database evidence with provider reads."""

from datetime import datetime, timezone
from hashlib import sha256
from io import BytesIO
import json
import os
from pathlib import Path
import re
import sys
from time import monotonic
from urllib.error import HTTPError, URLError
from urllib.request import Request
from zipfile import BadZipFile, ZipFile

from ci.staging_migrate_claim import Refused, projection_hash, store_projection_hash
from rollback_proof import REPO, ProofError, gh, run_identity
from script_intake import no_redirect_opener, read_with_overflow
from staging_rollout import CLASS, USER_AGENT, Client, GateError, decode, mapping, number, require, sequence, timestamp
from worker_container_rollback import APPLICATIONS, SHA, VERSION_ID, WORKERS
from worker_container_rollback import app_rollouts, application_by_name, replaced_converged, rollout_image

WORKFLOW = ".github/workflows/staging-ownership-status.yml"
PLAN_WORKFLOW = ".github/workflows/staging-migrate.yml"
PLAN_ARTIFACT = "staging-migrate-manifest"
REPOSITORY_ID = 1396224242
COMPARE_FILE_LIMIT = 300  # GitHub returns at most 300 changed files, on page one only.
WORKER, APPLICATION = WORKERS["staging"], APPLICATIONS["staging"]
ORIGIN = f"https://{WORKER}.5150.workers.dev"
RUN_ID = re.compile(r"[1-9][0-9]{0,19}")
HEX64 = re.compile(r"[0-9a-f]{64}")
DIGEST = re.compile(r"sha256:[0-9a-f]{64}")
DATABASE = re.compile(r"[A-Za-z0-9_.-]{1,63}")
NAMESPACE = re.compile(r"[0-9a-f]{32}")
PLAN_HOST = re.compile(r"[a-z0-9][a-z0-9-]{0,62}\.pg\.psdb\.cloud")
BRANCH = re.compile(r"[A-Za-z0-9_-]{1,63}")
MAX_OWNER = 4096
MAX_PLAN = 1024 * 1024


def matches(pattern, value):
    return isinstance(value, str) and pattern.fullmatch(value) is not None


def github(path, *, binary=False):
    return gh(f"repos/{REPO}/{path}", binary=binary)


def status_control_path(path):
    # Keep Python import paths in scope, including added modules. The fixture
    # tests can change independently of the runner's inventory tooling.
    return path in (WORKFLOW, PLAN_WORKFLOW, "scripts") or (
        path.startswith("scripts/") and not path.startswith(("scripts/ci/test_", "scripts/test_"))
    )


def verify_dispatch(env, api=github):
    sha = env.get("GITHUB_SHA")
    run_id = env.get("GITHUB_RUN_ID")
    require(env.get("GITHUB_REPOSITORY") == REPO and env.get("GITHUB_REPOSITORY_ID") == str(REPOSITORY_ID)
            and env.get("GITHUB_EVENT_NAME") == "workflow_dispatch" and env.get("GITHUB_RUN_ATTEMPT") == "1"
            and env.get("GITHUB_REF") == "refs/heads/main" and env.get("GITHUB_REF_PROTECTED") == "true"
            and matches(SHA, sha) and env.get("GITHUB_WORKFLOW_SHA") == sha and matches(RUN_ID, run_id)
            and env.get("GITHUB_WORKFLOW_REF") == f"{REPO}/{WORKFLOW}@refs/heads/main",
            "untrusted_status_dispatch")
    run = mapping(api(f"actions/runs/{run_id}"))
    require(number(run.get("id")) == int(run_id) and number(run.get("run_attempt")) == 1
            and run.get("path") == WORKFLOW and run.get("head_sha") == sha
            and all(number(mapping(run.get(key)).get("id")) == REPOSITORY_ID
                    for key in ("repository", "head_repository")), "untrusted_status_dispatch")
    run_identity(run, WORKFLOW, "workflow_dispatch")

    head = mapping(api("branches/main"))
    tip = mapping(head.get("commit")).get("sha")
    require(head.get("protected") is True and matches(SHA, tip), "status_main_unproved")
    if tip == sha:
        return

    # Accept an advanced main only with ancestry and unchanged controls proven
    # for the two immutable commits. Never execute code from the moving tip.
    comparison = mapping(api(f"compare/{sha}...{tip}?per_page=1&page=1"))
    require(comparison.get("status") == "ahead" and number(comparison.get("behind_by")) == 0
            and number(comparison.get("ahead_by")) > 0
            and mapping(comparison.get("base_commit")).get("sha") == sha
            and mapping(comparison.get("merge_base_commit")).get("sha") == sha, "status_main_unproved")
    files = sequence(comparison.get("files"))
    require(len(files) < COMPARE_FILE_LIMIT, "status_main_unproved")
    for entry in map(mapping, files):
        kind, path, previous = (entry.get(key) for key in ("status", "filename", "previous_filename"))
        require(isinstance(path, str) and path and kind in ("added", "removed", "modified", "renamed", "copied", "changed"),
                "status_main_unproved")
        if kind in ("renamed", "copied"):
            require(isinstance(previous, str) and previous, "status_main_unproved")
        else:
            require(previous is None, "status_main_unproved")
        require(not status_control_path(path) and (previous is None or not status_control_path(previous)),
                "status_dispatch_superseded")


def plan_manifest(raw, digest, artifact_name=PLAN_ARTIFACT):
    require(len(raw) <= MAX_PLAN and matches(DIGEST, digest),
            "invalid_plan_archive")
    require(f"sha256:{sha256(raw).hexdigest()}" == digest, "plan_archive_digest_mismatch")
    try:
        with ZipFile(BytesIO(raw)) as archive:
            members = archive.infolist()
            require(len(members) == 1 and members[0].filename == f"{artifact_name}.json"
                    and 0 < members[0].file_size <= MAX_PLAN, "invalid_plan_archive")
            return mapping(decode(archive.read(members[0])))
    except (BadZipFile, RuntimeError, OSError, ValueError, NotImplementedError):
        raise GateError("invalid_plan_archive") from None


def plan_projection(plan):
    source = plan.get("source_sha")
    require(matches(SHA, source) and type(plan.get("runner_version")) is int and plan["runner_version"] == 1
            and plan.get("migration_target") == "staging"
            and plan.get("mode") == "plan" and plan.get("role") == "two_bot_migrator_ro"
            and number(plan.get("role_verified_connections")) > 0
            and number(plan.get("applied_count")) == number(plan.get("applied_store_count")) == 0
            and plan.get("plan_provenance_verified") is False and plan.get("plan_run_id") == ""
            and mapping(plan.get("audit")).get("verify_findings") == [], "invalid_read_only_plan")
    cutover_digest, store_digest = projection_hash(plan), store_projection_hash(plan)
    require(plan.get("plan_manifest_sha256") == cutover_digest
            and plan.get("store_plan_manifest_sha256") == store_digest, "plan_projection_mismatch")

    def ledger_max(ledger, source_key, pending_key, required_version):
        before = sequence(plan.get(f"{ledger}_before"))
        require(before == plan.get(f"{ledger}_after") and plan.get(pending_key) == []
                and all(mapping(row).get("success") is True for row in before), "plan_ledger_unqualified")
        keys = ("version", "description", "sha384")
        rows = [tuple(row.get(key) for key in keys) for row in before]
        require(rows == [tuple(row.get(key) for key in keys) for row in plan[source_key]]
                and any(row[0] == required_version for row in rows), "plan_ledger_unqualified")
        return rows[-1][0]

    cutover = ledger_max("ledger", "source_migrations", "pending_before", 481)
    store = ledger_max("ledger_store", "source_store_migrations", "pending_store_before", 478)
    target = mapping(plan.get("target"))
    host, database = target.get("host"), target.get("database")
    require(set(target) == {"host", "database", "branch_id"} and matches(BRANCH, target["branch_id"])
            and matches(PLAN_HOST, host) and "-pooler" not in host and "prod" not in host
            and matches(DATABASE, database)
            and "prod" not in database.lower(), "plan_target_unqualified")
    fingerprint = sha256(json.dumps(target, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    return {"source_sha": source, "target_fingerprint": fingerprint,
            "cutover_version": cutover, "store_version": store,
            "cutover_projection_sha256": cutover_digest, "store_projection_sha256": store_digest}


def authenticated_plan(run_id, api=github, *, workflow=PLAN_WORKFLOW, artifact_name=PLAN_ARTIFACT):
    require(matches(RUN_ID, run_id), "invalid_plan_run_id")
    route = f"actions/runs/{run_id}"
    run = mapping(api(route))
    run_identity(run, workflow, "workflow_dispatch")
    require(run.get("id") == int(run_id) and run.get("run_attempt") == 1
            and run.get("status") == "completed" and run.get("conclusion") == "success", "invalid_plan_run")
    jobs = mapping(api(f"{route}/attempts/1/jobs?per_page=4"))
    name = Path(workflow).stem
    expected = {f"{name} (plan)": "success", f"{name} (claim)": "skipped", f"{name} (apply)": "skipped"}
    rows = sequence(jobs.get("jobs"))
    require(jobs.get("total_count") == len(rows) == len(expected)
            and {mapping(row).get("name"): row.get("conclusion") for row in rows} == expected, "invalid_plan_jobs")
    listing = mapping(api(f"{route}/artifacts?per_page=2"))
    artifacts = sequence(listing.get("artifacts"))
    require(listing.get("total_count") == len(artifacts) == 1, "invalid_plan_artifacts")
    artifact = mapping(artifacts[0])
    artifact_id = number(artifact.get("id"))
    require(artifact_id > 0 and artifact.get("name") == artifact_name and artifact.get("expired") is False
            and 0 < number(artifact.get("size_in_bytes")) <= MAX_PLAN, "invalid_plan_artifact")
    digest = artifact.get("digest")
    plan = plan_manifest(api(f"actions/artifacts/{artifact_id}/zip", binary=True), digest, artifact_name)
    return plan, {"run_id": int(run_id), "artifact_id": artifact_id, "archive_digest": digest}


def verified_plan(run_id, api=github):
    plan, artifact = authenticated_plan(run_id, api)
    return {**artifact, **plan_projection(plan)}


def checked_plan_summary(value):
    if value is None:
        return None
    plan = mapping(value)
    digests = {"source_sha": SHA, "archive_digest": DIGEST, "target_fingerprint": HEX64,
               "cutover_projection_sha256": HEX64, "store_projection_sha256": HEX64}
    numbers = ("run_id", "artifact_id", "cutover_version", "store_version")
    require(set(plan) == set(digests) | set(numbers)
            and all(matches(pattern, plan[key]) for key, pattern in digests.items())
            and all(0 < number(plan[key]) < 2**63 for key in numbers), "invalid_plan_summary")
    return plan


def provider_sample(client, account):
    path = f"/workers/scripts/{WORKER}"
    deployments = sequence(mapping(client.api(f"{path}/deployments")).get("deployments"))
    require(bool(deployments), "worker_deployment_missing")
    deployment = mapping(deployments[0])
    deployment_id = deployment.get("id")
    require(matches(VERSION_ID, deployment_id), "invalid_deployment_id")
    versions = sequence(deployment.get("versions"))
    require(len(versions) == 1 and mapping(versions[0]).get("percentage") == 100, "split_traffic_deployment")
    version = versions[0].get("version_id")
    require(matches(VERSION_ID, version), "invalid_version_id")
    detail = mapping(client.api(f"{path}/versions/{version}"))
    source = mapping(detail.get("annotations")).get("workers/message")
    require(detail.get("id") == version and matches(SHA, source), "worker_version_unbound")
    bindings = sequence(mapping(detail.get("resources")).get("bindings"))

    def binding(name, kind):
        rows = [row for row in bindings if mapping(row).get("name") == name]
        require(len(rows) == 1 and rows[0].get("type") == kind, "worker_binding_unproved")
        return rows[0]

    for name, value in (("STAGING_RUNTIME_POLICY", "bounded"), ("TWO_DEPLOY_ENV", "staging"),
                        ("CONTAINER_RELEASE", source)):
        require(binding(name, "plain_text").get("text") == value, "staging_policy_unproved")
    bot = binding("TWO_BOT", "durable_object_namespace")
    namespace = bot.get("namespace_id")
    require(bot.get("class_name") == CLASS and matches(NAMESPACE, namespace)
            and bot.get("script_name") in (None, WORKER) and bot.get("environment") in (None, "staging"),
            "worker_namespace_unproved")

    app = application_by_name(client, APPLICATION)
    app_id = app["id"]
    require(matches(VERSION_ID, app_id) and app["durable_objects"]["namespace_id"] == namespace,
            "application_namespace_mismatch")
    ranked = sorted(((timestamp(row.get("created_at")), row) for row in app_rollouts(client, app_id)),
                    key=lambda item: item[0], reverse=True)
    require(bool(ranked) and (len(ranked) == 1 or ranked[0][0] > ranked[1][0]), "newest_rollout_unproved")
    rollout = ranked[0][1]
    require(rollout.get("status") == "completed", "newest_rollout_not_completed")
    rollout_id = rollout["id"]
    require(matches(VERSION_ID, rollout_id), "invalid_rollout_id")
    record = mapping(client.api(f"/containers/applications/{app_id}/rollouts/{rollout_id}"))
    require(record.get("id") == rollout_id and record.get("status") == "completed"
            and timestamp(record.get("created_at")) == ranked[0][0]
            and replaced_converged(record), "newest_rollout_not_converged")
    image = rollout_image(record, APPLICATION)
    prefix = f"registry.cloudflare.com/{account}/{APPLICATION}@"
    require(image.startswith(prefix) and rollout_image(rollout, APPLICATION) == image
            and mapping(app.get("configuration")).get("image") == image, "application_image_mismatch")
    return {"worker": {"deployment_id": deployment_id, "version_id": version, "source_sha": source,
                       "traffic_percent": 100, "runtime_policy": "bounded"},
            "container": {"application_id": app_id, "namespace_id": namespace,
                          "rollout_id": rollout_id, "rollout_status": "completed",
                          "image_digest": image.removeprefix(prefix)}}


def read_ownership(token, expected_fingerprint, *, opener=None, deferred=False):
    endpoint = ORIGIN + "/internal/ownership"
    if expected_fingerprint is not None:
        require(matches(HEX64, expected_fingerprint), "invalid_plan_fingerprint")
        endpoint += "?runtime_db_fingerprint=" + expected_fingerprint
    headers = {"Authorization": f"Bearer {token}", "Cache-Control": "no-cache", "User-Agent": USER_AGENT}
    request = Request(endpoint, headers=headers)
    opener = opener or no_redirect_opener(use_proxy=False)
    try:
        with opener.open(request, timeout=15) as response:
            require(response.status == 200, "ownership_get_failed")
            body = read_with_overflow(response, MAX_OWNER)
    except HTTPError as error:
        with error:
            if deferred and error.code == 503:
                body = read_with_overflow(error, MAX_OWNER)
                if len(body) <= MAX_OWNER and mapping(decode(body)).get("reason") == "deployment_mismatch":
                    raise GateError("ownership_deployment_mismatch") from None
        raise GateError("ownership_get_failed") from None
    except (URLError, TimeoutError, OSError):
        raise GateError("ownership_get_failed") from None
    require(len(body) <= MAX_OWNER, "ownership_response_too_large")
    return mapping(decode(body))


def read_status(env, plan, *, client_factory=Client, read=read_ownership):
    require(env.get("STAGING_WORKER_URL") in (ORIGIN, ORIGIN + "/"), "worker_origin_mismatch")
    token = env.get("OWNERSHIP_CONTROL_TOKEN", "")
    require(len(token) >= 32, "ownership_token_missing")
    account = env.get("CLOUDFLARE_ACCOUNT_ID")
    client = client_factory(account, env.get("CLOUDFLARE_API_TOKEN"), deadline=monotonic() + 95)
    before = provider_sample(client, account)
    expected = plan["target_fingerprint"] if plan is not None else None
    state = mapping(read(token, expected))
    observed = datetime.now(timezone.utc).isoformat()
    after = provider_sample(client, account)
    require(after == before, "provider_state_drift")
    version = before["worker"]["version_id"]
    owner = mapping(state.get("owner"))
    require(state.get("deploymentId") == version and owner.get("phase") == "active"
            and owner.get("deploymentId") == version and type(state.get("running")) is bool,
            "ownership_target_mismatch")
    epoch = number(owner.get("epoch"))
    require(0 < epoch < 2**53, "invalid_ownership_epoch")
    db_status = "plan_not_supplied" if plan is None else "worker_unsupported"
    if plan is not None and "runtimeDatabaseFingerprint" in state:
        fingerprint = state["runtimeDatabaseFingerprint"]
        require(matches(HEX64, fingerprint) and fingerprint == expected,
                "runtime_database_target_unproved")
        db_status = "verified"
    return {"schema_version": 1, "workflow_sha": env["GITHUB_SHA"], "workflow_run_id": int(env["GITHUB_RUN_ID"]),
            "observed_at": observed, "verification": "full_tuple" if db_status == "verified" else "provider_only",
            **after, "ownership": {"epoch": epoch, "phase": "active", "running": state["running"]},
            "runtime_database": db_status, "plan": plan}


def main(argv, env):
    try:
        if argv == ["plan"]:
            verify_dispatch(env)
            run_id = env.get("PLAN_RUN_ID")
            print(json.dumps(verified_plan(run_id) if run_id else None, sort_keys=True))
        elif argv == ["inventory"]:
            with (Path(env["RUNNER_TEMP"]) / "staging-status-plan.json").open("rb") as reader:
                body = read_with_overflow(reader, MAX_OWNER)
            require(len(body) <= MAX_OWNER, "plan_projection_too_large")
            print(json.dumps(read_status(env, checked_plan_summary(decode(body))), sort_keys=True))
        else:
            require(argv == ["verify-dispatch"], "invalid_operation")
            verify_dispatch(env)
        return 0
    except (GateError, ProofError, Refused, KeyError, OSError, TypeError, ValueError, OverflowError) as error:
        reason = str(error) if isinstance(error, (GateError, ProofError)) else "invalid_evidence"
        print(f"::error::Staging recovery status refused ({reason})", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], os.environ))
