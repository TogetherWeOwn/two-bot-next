#!/usr/bin/env python3
"""Fail-closed staging gate for Wrangler 4.147.0's rolling Containers API.

Only allowlisted provenance is persisted/printed. API configurations and error
bodies can contain secrets: never print them, including in exception messages.
"""
import argparse
from datetime import datetime
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
WRANGLER = "4.147.0"
VERSION_PROBES = (["--version"], ["-v"])
LIMIT = 100
MAX_BODY = 2 * 1024 * 1024
# Cloudflare rejects the default "Python-urllib/x.y" agent at the edge with 403 (error 1010),
# so an explicit agent is required for the gate to see the Worker at all.
USER_AGENT = "two-bot-next-staging-rollout/1.0"
# Consecutive fully-passing verify passes required before a completed rollout
# whose only deviation is a `LAG_COUNTS` instance-counter shape is accepted. Each pass re-checks every identity and
# runtime probe; any non-passing poll resets the streak.
ACTIVE_LAG_CONFIRMATIONS = 2
# Polls (5s apart) tolerated while a completed rollout's target image is not
# yet reflected by the application listing. The listing is read separately from
# the rollout record and can trail it right after a deploy; a persistent
# mismatch still fails closed as `application_image_drift`.
APPLICATION_IMAGE_STALE_POLLS = 12
TOKEN = re.compile(r"[a-z0-9_]{1,32}")
UUID = r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"
IMAGE = rf"registry\.cloudflare\.com/[^/@\s]+/{APPLICATION}@sha256:[0-9a-f]{{64}}"


class GateError(Exception):
    """Fixed diagnostic class only; never constructed from upstream text.

    `detail` is optional and may only come from `receipt_shape` (allowlisted
    record-type counts and version fields).
    """

    def __init__(self, code, detail=None):
        super().__init__(code)
        self.detail = detail


def require(condition, code, detail=None):
    if not condition:
        raise GateError(code, detail)


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
        # Last allowlisted state seen by verify; printed only on rollout_timeout.
        self.observation = None
        self.opener = build_opener(NoRedirect())

    def request(self, url, authenticated=False):
        timeout = 10 if self.deadline is None else min(10, self.deadline - time.monotonic())
        require(timeout > 0, "rollout_timeout")
        headers = {"Cache-Control": "no-cache", "User-Agent": USER_AGENT}
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
            # Only the bot's own parked-readiness answer (JSON 503, the Worker's
            # probe allowlist) keeps its body: the gate echoes strict tokens from
            # it. Every other error body is discarded (TOG-13044).
            headers = error.headers if error.headers is not None else {}
            media = (headers.get("content-type") or "").split(";")[0].strip().lower()
            if error.code == 503 and media == "application/json":
                try:
                    body = error.read(MAX_BODY + 1)
                except (OSError, ValueError):
                    return error.code, {}, b""
                if len(body) <= MAX_BODY:
                    return error.code, headers, body
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


def command_line(row):
    args = row.get("command_line_args")
    return args if isinstance(args, list) and all(isinstance(arg, str) for arg in args) else None


def wrangler_sessions(records):
    # Wrangler appends one session record per invocation, and wrangler-action
    # probes `wrangler --version` before it deploys. Accept those probes, but
    # only from the pinned version, and demand exactly one deploy invocation.
    sessions = [row for row in records if row.get("type") == "wrangler-session"]
    require(all(row.get("version") == 1 and row.get("wrangler_version") == WRANGLER
                for row in sessions), "wrong_wrangler_receipt")
    deploys = [row for row in sessions if (command_line(row) or [None])[0] == "deploy"]
    require(len(deploys) == 1, "wrong_wrangler_receipt")
    require(all(row is deploys[0] or command_line(row) in VERSION_PROBES for row in sessions),
            "wrong_wrangler_receipt")


SAFE_TYPE = re.compile(r"[a-z][a-z0-9_-]{0,31}")
SAFE_FIELD = re.compile(r"[0-9]{1,4}(?:\.[0-9]{1,4}){0,3}(?:-[A-Za-z0-9.]{1,16})?")


def session_kind(row):
    """Classify a `wrangler-session` without echoing its arguments.

    Returns one of "deploy", "probe" or "other"; only the counts are printed.
    """
    args = command_line(row)
    if args is not None and args[:1] == ["deploy"]:
        return "deploy"
    if args in VERSION_PROBES:
        return "probe"
    return "other"


def receipt_shape(records):
    """Allowlisted failure diagnostic: record-type counts and session version fields.

    Never includes `command_line_args`, log paths, timestamps, IDs or any other value.
    """
    def field(value):
        # bool is an int subclass; only plain small integers and version-like strings print.
        if type(value) is int and 0 <= value <= 9999 or isinstance(value, str) and SAFE_FIELD.fullmatch(value):
            return str(value)
        return "invalid"

    counts = {}
    for row in records:
        kind = row.get("type")
        kind = kind if isinstance(kind, str) and SAFE_TYPE.fullmatch(kind) else "other"
        counts[kind] = counts.get(kind, 0) + 1
    sessions = [row for row in records if row.get("type") == "wrangler-session"]
    kinds = [session_kind(row) for row in sessions]
    shape = ("records " + (",".join(f"{kind}={counts[kind]}" for kind in sorted(counts)) or "none")
             + "; sessions " + (",".join(f"{kind}={kinds.count(kind)}"
                                         for kind in ("deploy", "probe", "other")) or "none")
             + "; " + (" ".join(f"v{field(row.get('version'))}/wrangler-{field(row.get('wrangler_version'))}"
                                for row in sessions) or "none"))
    return shape


def deploy_version(records, started):
    # The session rule itself lives in `wrangler_sessions()` (kept verbatim);
    # failures are re-raised with the allowlisted receipt shape attached.
    detail = receipt_shape(records)
    try:
        wrangler_sessions(records)
    except GateError as error:
        raise GateError(str(error), detail) from None
    rows = [row for row in records if row.get("type") == "deploy"]
    require(len(rows) == 1, "deploy_receipt_missing_or_ambiguous", detail)
    row = rows[0]
    checks = {"version": row.get("version") == 1, "worker": row.get("worker_name") == WORKER,
              "environment": row.get("wrangler_environment") == "staging",
              "not_overridden": row.get("worker_name_overridden") is False}
    require(all(checks.values()), "wrong_deploy_receipt",
            detail + "; deploy " + " ".join(f"{name}={'ok' if ok else 'bad'}" for name, ok in checks.items()))
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


def _rollout_shape(row, image, target_version):
    """Shared identity and shape checks for a pinned rollout row.

    Returns None while the rollout has not completed, otherwise the
    (instance counts, progress-converged) pair. Every fail-closed `require`
    matches `converged`, so identity drift, replaced rollouts and malformed
    schemas fail identically on both acceptance paths.
    """
    row = mapping(row)
    require(row.get("strategy") == "rolling" and row.get("kind") == "full_auto",
            "unsupported_rollout_profile")
    require(number(row.get("target_version")) == target_version
            and mapping(row.get("target_configuration")).get("image") == image, "rollout_identity_drift")
    status = row.get("status")
    require(status not in ("replaced", "reverted"), "rollout_replaced_or_reverted")
    require(status in ("pending", "progressing", "completed"), "unknown_rollout_status")
    if status != "completed":
        return None
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
    return counts, total == updated == 1


def converged(row, image, target_version):
    shape = _rollout_shape(row, image, target_version)
    if shape is None:
        return False
    counts, progress_ok = shape
    return (counts == {"active": 1, "healthy": 1, "failed": 0, "starting": 0, "scheduling": 0}
            and progress_ok)


# Control-plane counter shapes that differ from `converged` only in how Cloudflare
# reports one serving instance: `active` still reading 0 right after the rollout,
# or, under durable_object scheduling, the in-use instance counted `active` but
# not `healthy` (observed steady state while the exact build served ready).
LAG_COUNTS = (
    {"active": 0, "healthy": 1, "failed": 0, "starting": 0, "scheduling": 0},
    {"active": 1, "healthy": 0, "failed": 0, "starting": 0, "scheduling": 0},
)


def active_lag(row, image, target_version):
    """A completed rollout identical to `converged` except for one of the
    `LAG_COUNTS` counter shapes. Only `verify` consults this, and only as a
    provisional pass that still needs consecutive fully-passing exact-version
    runtime probes before acceptance.
    """
    shape = _rollout_shape(row, image, target_version)
    if shape is None:
        return False
    counts, progress_ok = shape
    return counts in LAG_COUNTS and progress_ok


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


def rollout_observation(row):
    # Fixed vocabulary and integers only; configurations never reach the log.
    status = row.get("status")
    parts = [f"rollout={status if status in ('pending', 'progressing', 'completed') else 'unknown'}"]
    health = row.get("health")
    instances = health.get("instances") if isinstance(health, dict) else None
    if isinstance(instances, dict):
        counts = [f"{key}:{instances[key]}" for key in ("active", "healthy", "failed", "starting", "scheduling")
                  if type(instances.get(key)) is int]
        parts.append("instances=" + ",".join(counts))
    return " ".join(parts)


def gateway_failure(report):
    # The bot's fixed phase/class for a failed gateway task, as `phase:class`;
    # anything that is not two strict tokens is dropped, never echoed.
    failure = report.get("gateway_failure")
    if not isinstance(failure, dict):
        return None
    parts = [failure.get("phase"), failure.get("class")]
    if all(isinstance(part, str) and TOKEN.fullmatch(part) for part in parts):
        return ":".join(parts)
    return None


def runtime_observation(status, headers, body, version, revision, build_id):
    # Only component names/states that fit a strict token are echoed; the probe
    # body and headers are service output and are otherwise discarded.
    parts = [f"readyz={status}"]
    try:
        report = mapping(decode(body))
        components = [f"{part[0]}:{part[1]}" for part in sequence(report.get("components"))
                      if isinstance(part, list) and len(part) == 2
                      and all(isinstance(item, str) and TOKEN.fullmatch(item) for item in part)]
        parts.append("components=" + ",".join(components))
        parts.append("identity=" + ("match" if headers.get("x-two-worker-version") == version
                                    and report.get("build_revision") == revision
                                    and report.get("build_id") == build_id else "mismatch"))
        failure = gateway_failure(report)
        if failure:
            parts.append("gateway_failure=" + failure)
    except GateError:
        parts.append("body=unreadable")
    return " ".join(parts)


def unconverged_failure(client, url, version, revision, build_id):
    # Best effort while the rollout has not converged, so the timeout log can
    # name why the gateway never became ready: the failure of THIS build only.
    # An older Worker or image (identity mismatch) never contributes.
    _, headers, body = client.request(url + "/readyz")
    try:
        report = mapping(decode(body))
    except GateError:
        return None
    if (headers.get("x-two-worker-version") != version or report.get("build_revision") != revision
            or report.get("build_id") != build_id):
        return None
    return gateway_failure(report)


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


def read_records(path):
    return [mapping(decode(line)) for line in Path(path).read_bytes().splitlines() if line.strip()]


def receipt(args, client=None):
    """Network-free: accept the Wrangler deploy receipt before ownership moves."""
    baseline = mapping(decode(Path(args.receipt).read_bytes()))
    version = deploy_version(read_records(args.output), baseline["started"])
    save(args.evidence, {"worker_version": version})
    print("wrangler deploy receipt accepted")


def verify(args, client):
    baseline = mapping(decode(Path(args.receipt).read_bytes()))
    version = deploy_version(read_records(args.output), baseline["started"])
    image = docker_image(version, baseline["revision"], baseline["build_id"])
    require(worker_namespace(client, version) == baseline["namespace_id"], "worker_namespace_changed")
    url = staging_url()
    pinned = None
    lag_streak = 0
    image_stale = 0
    while time.monotonic() < client.deadline:
        probed = False
        passed = False
        app = application(client)
        require(app["id"] == baseline["application_id"]
                and app["durable_objects"]["namespace_id"] == baseline["namespace_id"], "application_identity_drift")
        if pinned is None:
            pinned = select_rollout(rollouts(client, app["id"]), baseline, image)
            client.observation = "no_new_rollout"
        if pinned is not None:
            row = mapping(client.api(f"/containers/applications/{app['id']}/rollouts/{identifier(pinned['id'])}"))
            require(row.get("id") == pinned["id"], "rollout_identity_drift")
            complete = converged(row, image, number(pinned.get("target_version")))
            lag = False if complete else active_lag(row, image, number(pinned.get("target_version")))
            client.observation = rollout_observation(row)
            stale = (complete or lag) and mapping(app.get("configuration")).get("image") != image
            if stale:
                image_stale += 1
                require(image_stale <= APPLICATION_IMAGE_STALE_POLLS, "application_image_drift")
                client.observation += " application_image=stale"
            else:
                image_stale = 0
            if (complete or lag) and not stale:
                active_worker(client, version)
                status, headers, body = client.request(url + "/readyz")
                probed = True
                observed = "rollout=active_lag " if lag else "rollout=converged "
                client.observation = observed + runtime_observation(
                    status, headers, body, version, baseline["revision"], baseline["build_id"])
                if runtime_ready(status, headers, body, version, baseline["revision"], baseline["build_id"]):
                    health, health_headers, _ = client.request(url + "/health")
                    client.observation = f"{observed}readyz=200 health={health}"
                    if health == 200 and health_headers.get("x-two-worker-version") == version:
                        # Re-read control plane after the runtime probes; neither
                        # Worker activation nor container rollout is transactional.
                        active_worker(client, version)
                        final = client.api(f"/containers/applications/{app['id']}/rollouts/{pinned['id']}")
                        require(mapping(final).get("id") == pinned["id"], "rollout_identity_drift")
                        # The instance counters can wobble between the `converged`
                        # and lag shapes while the exact build keeps serving, so
                        # either completed shape passes the re-read; identity,
                        # steps and failed/starting/scheduling stay exact.
                        require(converged(final, image, pinned["target_version"])
                                or active_lag(final, image, pinned["target_version"]),
                                "rollout_not_converged")
                        final_app = application(client)
                        require(final_app["id"] == app["id"]
                                and final_app["durable_objects"]["namespace_id"] == baseline["namespace_id"]
                                and mapping(final_app.get("configuration")).get("image") == image,
                                "application_image_drift")
                        if complete or lag_streak + 1 >= ACTIVE_LAG_CONFIRMATIONS:
                            evidence = {"worker_version": version, "application_id": app["id"],
                                        "rollout_id": pinned["id"],
                                        "target_version": pinned["target_version"],
                                        "image": image, "revision": baseline["revision"],
                                        "build_id": baseline["build_id"], "readyz": 200, "health": 200}
                            if lag:
                                evidence["active_lag"] = True
                            save(args.evidence, evidence)
                            print("intended staging rollout completed; exact Worker and image ready")
                            return
                        lag_streak += 1
                        passed = True
        # Warming is allowed, but never acceptance evidence; discard the body.
        client.request(url + "/health")
        # Only once this build's rollout exists: before that nothing of ours runs.
        if pinned is not None and not probed:
            failure = unconverged_failure(client, url, version, baseline["revision"], baseline["build_id"])
            if failure:
                client.observation += " gateway_failure=" + failure
        if not passed:
            lag_streak = 0
        time.sleep(max(0, min(5, client.deadline - time.monotonic())))
    raise GateError("rollout_timeout")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["prepare", "receipt", "verify"])
    parser.add_argument("--receipt", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--config", default="wrangler.toml")
    parser.add_argument("--deploy-config", default="staging-deploy.json")
    parser.add_argument("--evidence", default="staging-rollout-evidence.json")
    args = parser.parse_args()
    client = None
    try:
        deadline = time.monotonic() + 300 if args.mode == "verify" else None
        # `receipt` only reads the local Wrangler NDJSON; it needs no Cloudflare credentials.
        client = None if args.mode == "receipt" else Client(
            os.environ.get("CLOUDFLARE_ACCOUNT_ID"), os.environ.get("CLOUDFLARE_API_TOKEN"), deadline)
        {"prepare": prepare, "receipt": receipt, "verify": verify}[args.mode](args, client)
    except GateError as error:
        print(f"staging rollout gate failed: {error}")
        if error.detail:
            print(f"staging rollout diagnostic: {error.detail}")
        if str(error) == "rollout_timeout" and client is not None and client.observation:
            print(f"last observation before timeout: {client.observation}")
        return 1
    except Exception:
        # No traceback: filesystem, SDK receipt and JSON errors may carry data.
        print("staging rollout gate failed: invalid_local_receipt")
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
