"""Bracket no-start staging ownership with provider reads."""

from datetime import datetime, timezone
import json
import os
import re
import sys
from time import monotonic
from urllib.error import URLError
from urllib.request import Request

from rollback_proof import REPO, ProofError, gh
from script_intake import no_redirect_opener, read_with_overflow
from staging_rollout import CLASS, Client, GateError, decode, mapping, number, require, sequence, timestamp
from worker_container_rollback import APPLICATIONS, SHA, VERSION_ID, WORKERS
from worker_container_rollback import app_rollouts, application_by_name, replaced_converged, rollout_image

WORKFLOW = ".github/workflows/staging-ownership-status.yml"
WORKER, APPLICATION = WORKERS["staging"], APPLICATIONS["staging"]
ORIGIN = f"https://{WORKER}.5150.workers.dev"
NAMESPACE = re.compile(r"[0-9a-f]{32}")
MAX_OWNER = 4096


def matches(pattern, value):
    return isinstance(value, str) and pattern.fullmatch(value) is not None


def github(path, *, binary=False):
    return gh(f"repos/{REPO}/{path}", binary=binary)


def verify_main(env, api=github):
    sha = env.get("GITHUB_SHA")
    require(env.get("GITHUB_REPOSITORY") == REPO and env.get("GITHUB_REPOSITORY_ID") == "1396224242"
            and env.get("GITHUB_EVENT_NAME") == "workflow_dispatch" and env.get("GITHUB_RUN_ATTEMPT") == "1"
            and env.get("GITHUB_REF") == "refs/heads/main" and env.get("GITHUB_REF_PROTECTED") == "true"
            and matches(SHA, sha) and env.get("GITHUB_WORKFLOW_SHA") == sha
            and env.get("GITHUB_WORKFLOW_REF") == f"{REPO}/{WORKFLOW}@refs/heads/main",
            "untrusted_status_dispatch")
    head = mapping(api("branches/main"))
    require(head.get("protected") is True and mapping(head.get("commit")).get("sha") == sha,
            "dispatch_is_not_current_main")


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


def read_ownership(token, *, opener=None):
    endpoint = ORIGIN + "/internal/ownership"
    request = Request(endpoint, headers={"Authorization": f"Bearer {token}", "Cache-Control": "no-cache"})
    opener = opener or no_redirect_opener(use_proxy=False)
    try:
        with opener.open(request, timeout=15) as response:
            require(response.status == 200, "ownership_get_failed")
            body = read_with_overflow(response, MAX_OWNER)
    except (URLError, TimeoutError, OSError):
        raise GateError("ownership_get_failed") from None
    require(len(body) <= MAX_OWNER, "ownership_response_too_large")
    return mapping(decode(body))


def read_status(env, *, client_factory=Client, read=read_ownership):
    require(env.get("STAGING_WORKER_URL") in (ORIGIN, ORIGIN + "/"), "worker_origin_mismatch")
    token = env.get("OWNERSHIP_CONTROL_TOKEN", "")
    require(len(token) >= 32, "ownership_token_missing")
    account = env.get("CLOUDFLARE_ACCOUNT_ID")
    client = client_factory(account, env.get("CLOUDFLARE_API_TOKEN"), deadline=monotonic() + 95)
    before = provider_sample(client, account)
    state = mapping(read(token))
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
    return {"schema_version": 1, "workflow_sha": env["GITHUB_SHA"], "workflow_run_id": int(env["GITHUB_RUN_ID"]),
            "observed_at": observed, "verification": "provider_only", **after,
            "ownership": {"epoch": epoch, "phase": "active", "running": state["running"]}}


def main(argv, env):
    try:
        if argv == ["inventory"]:
            print(json.dumps(read_status(env), sort_keys=True))
        else:
            require(argv == ["verify-main"], "invalid_operation")
            verify_main(env)
        return 0
    except (GateError, ProofError, KeyError, OSError, TypeError, ValueError, OverflowError) as error:
        reason = str(error) if isinstance(error, (GateError, ProofError)) else "invalid_evidence"
        print(f"::error::Staging recovery status refused ({reason})", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], os.environ))
