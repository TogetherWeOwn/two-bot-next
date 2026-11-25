"""Admission and bracketed GET-only evidence for production-ownership-status.yml."""

import hashlib
import json
import os
import re
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from time import monotonic

import staging_rollout as provider
from staging_rollout import GateError, decode, mapping, number, require, sequence
from worker_container_rollback import APPLICATIONS, SHA, WORKERS, app_rollouts, application_by_name, rollout_image
from worker_container_rollback import VERSION_ID as UUID

REPO = "TogetherWeOwn/two-bot-next"
REF = "refs/heads/main"
WORKER, APPLICATION = WORKERS["production"], APPLICATIONS["production"]
PRODUCTION_URL = f"https://{WORKER}.5150.workers.dev"
NAMESPACE = re.compile(r"[0-9a-f]{32}")
CI_ACTOR = re.compile(r"github-actions:[1-9][0-9]{0,19}:[0-9a-f]{40}")
RELEASE_FENCE_ACTOR = re.compile(r"production-schema-release:[1-9][0-9]{0,19}:[0-9a-f]{40}")


def matches(pattern, value):
    return isinstance(value, str) and pattern.fullmatch(value) is not None


def read_json(command, timeout, env=None):
    try:
        result = subprocess.run(command, env=env, capture_output=True, check=True, timeout=timeout)
    except (OSError, subprocess.SubprocessError):
        raise GateError("read_client_failed") from None
    return mapping(decode(result.stdout))


def github_get(path):
    return read_json(["gh", "api", "--hostname", "github.com", "--method", "GET", f"repos/{REPO}/{path}"], 10)


def verify_dispatch(env, event, api=github_get):
    sha = env.get("GITHUB_SHA")
    require(env.get("GITHUB_REPOSITORY") == REPO and env.get("GITHUB_REF") == REF
            and matches(SHA, sha) and env.get("GITHUB_WORKFLOW_SHA") == sha,
            "invalid_dispatch_context")
    event = mapping(event)
    require(event.get("ref") in ("main", REF) and event.get("inputs") in (None, {}), "invalid_dispatch_payload")
    commit = mapping(api("git/ref/heads/main").get("object"))
    require(commit.get("type") == "commit" and commit.get("sha") == sha, "dispatch_is_not_current_main")


def environment_policy(auto_approve, api=github_get):
    settings = api("environments/production")
    policy = mapping(settings.get("deployment_branch_policy"))
    branches = api("environments/production/deployment-branch-policies?per_page=2")
    require(settings.get("name") == "production" and policy.get("custom_branch_policies") is True
            and policy.get("protected_branches") is False and number(branches.get("total_count")) == 1
            and [(mapping(row).get("type", "branch"), row.get("name")) for row in sequence(branches.get("branch_policies"))]
            == [("branch", "main")], "production_environment_not_main_only")
    rules = [mapping(rule) for rule in sequence(settings.get("protection_rules"))]
    require(sum(rule.get("type") == "branch_policy" for rule in rules) == 1, "branch_rule_missing")
    reviewers = [sequence(rule.get("reviewers")) for rule in rules if rule.get("type") == "required_reviewers"]
    require(all(reviewers) if reviewers else auto_approve == "true", "production_environment_approval_missing")
    return hashlib.sha256(json.dumps([rules, branches, auto_approve], sort_keys=True).encode()).hexdigest()


def provider_sample(client, account, *, allow_rollout=False):
    route = f"/workers/scripts/{WORKER}"
    deployments = sequence(mapping(client.api(f"{route}/deployments")).get("deployments"))
    require(bool(deployments), "worker_deployment_missing")
    deployment = mapping(deployments[0])
    require(matches(UUID, deployment.get("id")), "invalid_deployment_id")
    versions = sequence(deployment.get("versions"))
    require(len(versions) == 1 and mapping(versions[0]).get("percentage") == 100,
            "split_traffic_deployment")
    version = versions[0].get("version_id")
    require(matches(UUID, version), "invalid_version_id")
    detail = mapping(client.api(f"{route}/versions/{version}"))
    sha = mapping(detail.get("annotations")).get("workers/message")
    require(detail.get("id") == version and matches(SHA, sha), "worker_version_unbound")
    bindings = sequence(mapping(detail.get("resources")).get("bindings"))
    bot = [binding for binding in bindings if mapping(binding).get("name") == "TWO_BOT"]
    require(len(bot) == 1 and bot[0].get("type") == "durable_object_namespace"
            and bot[0].get("class_name") == provider.CLASS, "worker_namespace_unbound")
    namespace = bot[0].get("namespace_id")
    require(matches(NAMESPACE, namespace), "invalid_namespace_id")
    app = application_by_name(client, APPLICATION)
    app_id = app["id"]
    require(matches(UUID, app_id) and app["durable_objects"]["namespace_id"] == namespace,
            "application_namespace_mismatch")

    ranked = sorted(((provider.timestamp(row.get("created_at")), row) for row in app_rollouts(client, app_id)),
                    key=lambda item: item[0], reverse=True)
    require(bool(ranked) and (len(ranked) == 1 or ranked[0][0] > ranked[1][0]), "latest_rollout_unbound")
    rollout = ranked[0][1]
    statuses = ("pending", "progressing", "completed") if allow_rollout else ("completed",)
    require(matches(UUID, rollout["id"]) and rollout.get("status") in statuses, "latest_rollout_unbound")
    image = rollout_image(rollout, APPLICATION)
    prefix = f"registry.cloudflare.com/{account}/{APPLICATION}@"
    require(image.startswith(prefix) and mapping(app.get("configuration")).get("image") == image,
            "application_image_mismatch")
    return {
        "worker": {"deployment_id": deployment["id"], "version_id": version, "sha": sha, "traffic_percent": 100},
        "container": {"application_id": app_id, "namespace_id": namespace, "rollout_id": rollout["id"],
                      "rollout_status": "completed", "image_digest": image.removeprefix(prefix)},
    }


def ownership_get(token):
    return read_json(["node", "wrangler/scripts/production-ownership-control.mjs", "observe"], 20,
                     {"PATH": os.environ.get("PATH", os.defpath), "PRODUCTION_WORKER_URL": PRODUCTION_URL,
                      "OWNERSHIP_CONTROL_TOKEN": token})


def ownership_projection(state, version):
    running = state.get("running")
    require(state.get("deploymentId") == version and type(running) is bool, "ownership_deployment_mismatch")
    owner = state["owner"]
    if owner is None:
        require(not running, "ownership_running_without_owner")
        return {"phase": "uninitialized", "epoch": None, "owner_actor": None, "running": running}
    owner = mapping(owner)
    phase = owner.get("phase")
    epoch = number(owner.get("epoch"))
    actor = owner.get("actor")
    require(0 < epoch < 2**53 and isinstance(actor, str), "invalid_ownership_record")
    require((phase == "active" and owner.get("deploymentId") == version)
            or (phase == "fenced" and not running
                and (owner.get("deploymentId") is None or owner.get("deploymentId") == version)),
            "ownership_target_mismatch")
    release_fence = phase == "fenced" and matches(RELEASE_FENCE_ACTOR, actor)
    require(not release_fence or ("deploymentId" in owner and owner["deploymentId"] is None),
            "production_release_fence_target_invalid")
    return {"phase": phase, "epoch": epoch,
            "owner_actor": actor if matches(RELEASE_FENCE_ACTOR, actor) or matches(CI_ACTOR, actor) else "redacted",
            "running": running}


def read_status(env, *, client_factory=provider.Client, read=ownership_get, allow_rollout=False):
    require(env.get("PRODUCTION_WORKER_URL") in (PRODUCTION_URL, PRODUCTION_URL + "/"), "worker_origin_mismatch")
    token = env.get("OWNERSHIP_CONTROL_TOKEN", "")
    require(len(token) >= 32, "ownership_token_missing")
    account = env.get("CLOUDFLARE_ACCOUNT_ID")
    client = client_factory(account, env.get("CLOUDFLARE_API_TOKEN"), deadline=monotonic() + 95)
    before = provider_sample(client, account, allow_rollout=allow_rollout)
    state = ownership_projection(read(token), before["worker"]["version_id"])
    observed = datetime.now(timezone.utc).isoformat()
    after = provider_sample(client, account, allow_rollout=allow_rollout)
    require(after == before, "provider_state_drift")
    return {"ownership_observed_at": observed, **after, "ownership": state}


def main(argv, env):
    try:
        if argv == ["status"]:
            print(json.dumps(read_status(env), sort_keys=True))
            return 0
        require(argv in (["guard"], ["recheck"]), "invalid_operation")
        event = decode(Path(env["GITHUB_EVENT_PATH"]).read_bytes())
        verify_dispatch(env, event)
        policy = environment_policy(env.get("AUTO_APPROVE"))
        if argv == ["guard"]:
            with Path(env["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as output:
                output.write(f"policy={policy}\n")
        else:
            require(env.get("EXPECTED_POLICY") == policy, "production_environment_drift")
        return 0
    except (GateError, KeyError, OSError, TypeError, ValueError, OverflowError) as error:
        reason = str(error) if isinstance(error, GateError) else "invalid_read"
        print(f"::error::Production status refused ({reason})", file=sys.stderr)
    return 1

if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], os.environ))
