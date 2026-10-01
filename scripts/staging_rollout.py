#!/usr/bin/env python3
"""Fail-closed staging gate for Wrangler 4.143.1's rolling Containers API.

Only allowlisted provenance is persisted/printed. API configurations and error
bodies can contain secrets: never print them, including in exception messages.
"""
import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import subprocess
import time
import tomllib
from urllib.error import HTTPError, URLError
from urllib.request import Request, HTTPRedirectHandler, build_opener

WORKER = "two-bot-next-staging"
APPLICATION = "two-bot-next-twobotcontainer-staging"
CLASS = "TwoBotContainer"
WRANGLER = "4.143.1"
LIMIT = 100
MAX_BODY = 2 * 1024 * 1024
UUID = r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"
IMAGE = rf"registry\.cloudflare\.com/[^/@\s]+/{APPLICATION}@sha256:[0-9a-f]{{64}}"


class GateError(Exception):
    """Fixed diagnostic class only; never constructed from upstream text."""


def require(condition, code):
    if not condition:
        raise GateError(code)


def identifier(value):
    # Application/rollout IDs are opaque strings; DO namespaces are hex IDs,
    # not Worker-version UUIDs. Restrict path syntax, not their undocumented shape.
    require(isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9_-]{1,128}", value), "invalid_identity")
    return value


def timestamp(value):
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
        require(parsed.tzinfo is not None, "invalid_timestamp")
        return parsed.timestamp()
    except (ValueError, TypeError, AttributeError):
        raise GateError("invalid_timestamp") from None


def sequence(value):
    require(isinstance(value, list), "invalid_api_schema")
    return value


def mapping(value):
    require(isinstance(value, dict), "invalid_api_schema")
    return value


def number(value):
    require(type(value) is int and value >= 0, "invalid_api_schema")
    return value


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        raise GateError("unexpected_redirect")


class Client:
    def __init__(self, account, token, deadline=None):
        require(isinstance(account, str) and re.fullmatch(r"[0-9a-f]{32}", account), "invalid_account")
        require(bool(token), "missing_api_token")
        self.base = f"https://api.cloudflare.com/client/v4/accounts/{account}"
        self.token = token
        self.deadline = deadline
        self.opener = build_opener(NoRedirect())

    def request(self, url, authenticated=False):
        timeout = 10 if self.deadline is None else min(10, self.deadline - time.monotonic())
        require(timeout > 0, "rollout_timeout")
        headers = {"Cache-Control": "no-cache"}
        if authenticated:
            headers["Authorization"] = f"Bearer {self.token}"
        try:
            with self.opener.open(Request(url, headers=headers), timeout=timeout) as response:
                body = response.read(MAX_BODY + 1)
                require(len(body) <= MAX_BODY, "response_too_large")
                return response.status, response.headers, body
        except HTTPError as error:
            # Even Cloudflare's error envelope may include configuration values.
            if authenticated:
                raise GateError("api_http_failure") from None
            return error.code, {}, b""
        except (URLError, TimeoutError, OSError):
            if authenticated:
                raise GateError("api_transport_failure") from None
            return 0, {}, b""

    def api(self, path):
        status, _, body = self.request(self.base + path, authenticated=True)
        require(status == 200, "api_http_failure")
        data = decode(body)
        require(mapping(data).get("success") is True and "result" in data, "api_failure")
        return data["result"]


def decode(body):
    try:
        return json.loads(body)
    except (ValueError, UnicodeError):
        raise GateError("invalid_json") from None


def application(client):
    apps = sequence(client.api("/containers/applications"))
    candidates = [mapping(app) for app in apps if mapping(app).get("name") == APPLICATION]
    require(len(candidates) == 1, "application_identity_missing_or_ambiguous")
    app = candidates[0]
    identifier(app.get("id"))
    identifier(mapping(app.get("durable_objects")).get("namespace_id"))
    return app


def rollouts(client, app_id):
    rows = sequence(client.api(f"/containers/applications/{identifier(app_id)}/rollouts?limit={LIMIT}"))
    # The pinned API exposes a `last` input but no verified next-cursor contract.
    # A saturated page is not evidence of a complete before/after snapshot.
    require(len(rows) < LIMIT, "rollout_snapshot_truncated")
    ids = [identifier(mapping(row).get("id")) for row in rows]
    require(len(ids) == len(set(ids)), "duplicate_rollout_identity")
    return rows


def deploy_version(records, started):
    sessions = [row for row in records if row.get("type") == "wrangler-session"]
    require(len(sessions) == 1 and sessions[0].get("version") == 1
            and sessions[0].get("wrangler_version") == WRANGLER, "wrong_wrangler_receipt")
    rows = [row for row in records if row.get("type") == "deploy"]
    require(len(rows) == 1, "deploy_receipt_missing_or_ambiguous")
    row = rows[0]
    require(row.get("version") == 1 and row.get("worker_name") == WORKER
            and row.get("wrangler_environment") == "staging"
            and row.get("worker_name_overridden") is False, "wrong_deploy_receipt")
    require(timestamp(row.get("timestamp")) >= started, "stale_deploy_receipt")
    version = identifier(row.get("version_id"))
    require(re.fullmatch(UUID, version), "invalid_worker_version")
    return version


def image_receipt(tags, details, version, revision, build_id):
    prefix = version.split("-")[0]
    pattern = rf"registry\.cloudflare\.com/[^/@\s]+/{APPLICATION}:{prefix}"
    selected = [tag for tag in tags if re.fullmatch(pattern, tag)]
    require(len(selected) == 1, "image_tag_missing_or_ambiguous")
    require(details.get("revision") == revision and details.get("build_id") == build_id,
            "image_build_mismatch")
    digests = [digest for digest in sequence(details.get("digests"))
               if isinstance(digest, str) and re.fullmatch(IMAGE, digest)]
    require(len(digests) == 1 and digests[0].split("@")[0] == selected[0].rsplit(":", 1)[0],
            "image_digest_missing_or_ambiguous")
    return digests[0]


def docker_image(version, revision, build_id):
    def docker(*args):
        try:
            result = subprocess.run(["docker", *args], capture_output=True, timeout=20, check=True)
            require(len(result.stdout) <= MAX_BODY, "response_too_large")
            return result.stdout
        except (OSError, subprocess.SubprocessError):
            raise GateError("image_inspection_failed") from None

    tags = docker("image", "ls", "--format", "{{.Repository}}:{{.Tag}}").decode().splitlines()
    pattern = rf"registry\.cloudflare\.com/[^/@\s]+/{APPLICATION}:{version.split('-')[0]}"
    matches = [tag for tag in tags if re.fullmatch(pattern, tag)]
    require(len(matches) == 1, "image_tag_missing_or_ambiguous")
    # Inspect only the two non-secret labels and RepoDigests, never Config.Env.
    format_string = ('{"revision":{{json (index .Config.Labels "org.opencontainers.image.revision")}},'
                     '"build_id":{{json (index .Config.Labels "com.togetherweown.build-id")}},'
                     '"digests":{{json .RepoDigests}}}')
    details = mapping(decode(docker("image", "inspect", matches[0], "--format", format_string)))
    return image_receipt(tags, details, version, revision, build_id)


def select_rollout(rows, baseline, image):
    candidates = []
    for row in rows:
        row = mapping(row)
        if identifier(row.get("id")) in baseline["rollout_ids"]:
            continue
        require(timestamp(row.get("created_at")) >= baseline["started"], "stale_new_rollout")
        require(mapping(row.get("target_configuration")).get("image") == image,
                "competing_rollout")
        candidates.append(row)
    require(len(candidates) <= 1, "ambiguous_new_rollout")
    return candidates[0] if candidates else None


def converged(row, image, target_version):
    row = mapping(row)
    require(row.get("strategy") == "rolling" and row.get("kind") == "full_auto",
            "unsupported_rollout_profile")
    require(number(row.get("target_version")) == target_version
            and mapping(row.get("target_configuration")).get("image") == image, "rollout_identity_drift")
    status = row.get("status")
    require(status not in ("replaced", "reverted"), "rollout_replaced_or_reverted")
    require(status in ("pending", "progressing", "completed"), "unknown_rollout_status")
    if status != "completed":
        return False
    # `current_*` describes the BEFORE version, even after completion; it is
    # not an acknowledgement of the target. Verify the application separately.
    number(row.get("current_version"))
    mapping(row.get("current_configuration"))
    health = mapping(mapping(row.get("health")).get("instances"))
    counts = {key: number(health.get(key)) for key in ("active", "healthy", "failed", "starting", "scheduling")}
    progress = mapping(row.get("progress"))
    steps = sequence(row.get("steps"))
    require(all(mapping(step).get("status") == "completed" for step in steps), "rollout_steps_incomplete")
    total_steps = number(progress.get("total_steps"))
    require(total_steps > 0 and len(steps) == total_steps
            and number(progress.get("current_step")) <= total_steps, "invalid_rollout_progress")
    total = number(progress.get("total_instances"))
    updated = number(progress.get("updated_instances"))
    return (counts == {"active": 1, "healthy": 1, "failed": 0, "starting": 0, "scheduling": 0}
            and total == updated == 1)


def worker_namespace(client, version):
    data = mapping(client.api(f"/workers/scripts/{WORKER}/versions/{identifier(version)}"))
    bindings = sequence(mapping(data.get("resources")).get("bindings"))
    matches = [mapping(binding) for binding in bindings
               if mapping(binding).get("type") == "durable_object_namespace"
               and binding.get("class_name") == CLASS and binding.get("name") == "TWO_BOT"]
    require(len(matches) == 1, "worker_namespace_missing_or_ambiguous")
    return identifier(matches[0].get("namespace_id"))


def active_worker(client, version):
    data = mapping(client.api(f"/workers/scripts/{WORKER}/deployments"))
    deployments = sequence(data.get("deployments"))
    require(bool(deployments), "worker_deployment_missing")
    versions = sequence(mapping(deployments[0]).get("versions"))
    require(len(versions) == 1 and mapping(versions[0]).get("version_id") == version
            and versions[0].get("percentage") == 100, "worker_version_not_active")


def runtime_ready(status, headers, body, version, revision, build_id):
    if status != 200:
        return False
    # Unknown/mismatched identities never pass, even for a healthy old process.
    require(headers.get("x-two-worker-version") == version, "serving_worker_mismatch")
    report = mapping(decode(body))
    require(report.get("build_revision") == revision and report.get("build_id") == build_id,
            "serving_image_mismatch")
    components = sequence(report.get("components"))
    return bool(components) and all(isinstance(part, list) and len(part) == 2 and part[1] == "ready"
                                   for part in components)


def staging_url():
    url = os.environ.get("STAGING_URL", "").rstrip("/")
    # Staging deploy job only; no production fallback or credentialed URLs.
    require(re.fullmatch(r"https://two-bot-next-staging\.[a-z0-9-]+\.workers\.dev", url),
            "invalid_staging_url")
    return url


def save(path, value):
    Path(path).write_text(json.dumps(value, indent=2) + "\n")


def prepare(args, client):
    staging_url()
    revision = os.environ.get("GITHUB_SHA", "")
    build_id = f"{os.environ.get('GITHUB_RUN_ID', '')}-{os.environ.get('GITHUB_RUN_ATTEMPT', '')}"
    require(re.fullmatch(r"[0-9a-f]{40}", revision) and re.fullmatch(r"[0-9]+-[0-9]+", build_id),
            "invalid_build_identity")
    started = time.time()
    app = application(client)
    rows = rollouts(client, app["id"])
    require(all(mapping(row).get("status") in ("completed", "replaced", "reverted") for row in rows),
            "prior_rollout_in_flight")
    source = Path(args.config).resolve()
    config = tomllib.loads(source.read_text())
    require(config.get("name") == "two-bot-next", "wrong_staging_config")
    staging = mapping(mapping(config.get("env")).get("staging"))
    containers = sequence(staging.get("containers"))
    require(len(containers) == 1 and containers[0].get("class_name") == CLASS
            and containers[0].get("max_instances") == 1, "wrong_staging_config")
    require(mapping(staging.get("version_metadata")).get("binding") == "CF_VERSION_METADATA",
            "missing_worker_version_binding")
    config["main"] = str((source.parent / config["main"]).resolve())
    for container in [*config.get("containers", []), *[item for env in config["env"].values()
                                                     for item in env.get("containers", [])]]:
        container["image"] = str((source.parent / container["image"]).resolve())
        container["image_build_context"] = str(source.parent.parent)
    containers[0]["image_vars"] = {"BOT_BUILD_REVISION": revision, "BOT_BUILD_ID": build_id}
    containers[0]["rollout_kind"] = "full_auto"
    save(args.deploy_config, config)
    save(args.receipt, {"started": started, "application_id": app["id"],
                        "namespace_id": app["durable_objects"]["namespace_id"],
                        "rollout_ids": [row["id"] for row in rows],
                        "revision": revision, "build_id": build_id})
    # This path must be fresh: Wrangler appends NDJSON across sessions.
    require(not Path(args.output).exists(), "deploy_output_not_fresh")
    print("staging rollout baseline recorded")


def verify(args, client):
    baseline = mapping(decode(Path(args.receipt).read_bytes()))
    records = [mapping(decode(line)) for line in Path(args.output).read_bytes().splitlines() if line.strip()]
    version = deploy_version(records, baseline["started"])
    image = docker_image(version, baseline["revision"], baseline["build_id"])
    require(worker_namespace(client, version) == baseline["namespace_id"], "worker_namespace_changed")
    url = staging_url()
    pinned = None
    while time.monotonic() < client.deadline:
        app = application(client)
        require(app["id"] == baseline["application_id"]
                and app["durable_objects"]["namespace_id"] == baseline["namespace_id"], "application_identity_drift")
        if pinned is None:
            pinned = select_rollout(rollouts(client, app["id"]), baseline, image)
        if pinned is not None:
            row = mapping(client.api(f"/containers/applications/{app['id']}/rollouts/{identifier(pinned['id'])}"))
            require(row.get("id") == pinned["id"], "rollout_identity_drift")
            complete = converged(row, image, number(pinned.get("target_version")))
            if complete:
                require(mapping(app.get("configuration")).get("image") == image, "application_image_drift")
                active_worker(client, version)
                status, headers, body = client.request(url + "/readyz")
                if runtime_ready(status, headers, body, version, baseline["revision"], baseline["build_id"]):
                    health, health_headers, _ = client.request(url + "/health")
                    if health == 200 and health_headers.get("x-two-worker-version") == version:
                        # Re-read control plane after the runtime probes; neither
                        # Worker activation nor container rollout is transactional.
                        active_worker(client, version)
                        final = client.api(f"/containers/applications/{app['id']}/rollouts/{pinned['id']}")
                        require(mapping(final).get("id") == pinned["id"], "rollout_identity_drift")
                        require(converged(final, image, pinned["target_version"]), "rollout_not_converged")
                        final_app = application(client)
                        require(final_app["id"] == app["id"]
                                and final_app["durable_objects"]["namespace_id"] == baseline["namespace_id"]
                                and mapping(final_app.get("configuration")).get("image") == image,
                                "application_image_drift")
                        save(args.evidence, {"worker_version": version, "application_id": app["id"],
                                             "rollout_id": pinned["id"], "target_version": pinned["target_version"],
                                             "image": image, "revision": baseline["revision"],
                                             "build_id": baseline["build_id"], "readyz": 200, "health": 200})
                        print("intended staging rollout completed; exact Worker and image ready")
                        return
        # Warming is allowed, but never acceptance evidence; discard the body.
        client.request(url + "/health")
        time.sleep(max(0, min(5, client.deadline - time.monotonic())))
    raise GateError("rollout_timeout")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["prepare", "verify"])
    parser.add_argument("--receipt", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--config", default="wrangler.toml")
    parser.add_argument("--deploy-config", default="staging-deploy.json")
    parser.add_argument("--evidence", default="staging-rollout-evidence.json")
    args = parser.parse_args()
    try:
        deadline = time.monotonic() + 300 if args.mode == "verify" else None
        client = Client(os.environ.get("CLOUDFLARE_ACCOUNT_ID"), os.environ.get("CLOUDFLARE_API_TOKEN"), deadline)
        (prepare if args.mode == "prepare" else verify)(args, client)
    except GateError as error:
        print(f"staging rollout gate failed: {error}")
        return 1
    except Exception:
        # No traceback: filesystem, SDK receipt and JSON errors may carry data.
        print("staging rollout gate failed: invalid_local_receipt")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
