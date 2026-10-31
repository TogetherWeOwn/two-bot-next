#!/usr/bin/env python3
"""Roll back a Worker version AND its container image, with takeover and a stable gate.

A Worker-version rollback alone leaves the new container image serving: the
10-15 prod incident needed a host POST of a container rollout
(step_percentage 100, previous image) after the Worker rollback. This script
does both atomically from the operator's point of view, then hands ownership
to the restored version and requires /readyz 200 with gateway=ready, stable
for STABLE_PASSES consecutive polls, before reporting success.

Two modes share one code path:
- `snapshot` records the pre-deploy serving state (allowlisted IDs only, so
  the file is safe as a workflow artifact on a public repo). The upcoming
  deploy's auto-rollback reads it back.
- `rollback` restores the previous serving state: fence, Worker deployments
  POST, container rollout POST (image only: the deploy changes nothing else,
  and re-posting a full configuration could revert a rotated secret), variant
  takeover, stable gate. With `--snapshot` the previous state comes from the
  file; with `--target-version`/`--target-image` (manual rollback, no in-run
  snapshot) the operator names both explicitly: rollout images are
  digest-only, so no history scan can link an image back to its Worker
  version. The digest is a content hash, safe to pass as a dispatch input.

Rollback inputs travel as one provenance receipt (Worker version, Git SHA,
container image), never as independently believable values. A production
deploy stamps its guarded SHA as the Worker version's `workers/message`
annotation (`wrangler deploy --message <sha>`), so the version detail binds
version to SHA; the snapshot records that SHA beside the serving version
and image. Before any restore POST, the receipt is validated against live
Cloudflare state: the version's annotated SHA must equal the receipt SHA,
the version must appear in the 100%-serving deployment history, and the
image must appear in a completed rollout for the application, or a
replaced one with convergence evidence (every step completed, every
instance updated: a rollout can be replaced before it finishes). A missing or
mismatched member refuses the
rollback: a merely CI-green SHA paired with some other version is never
admitted.

Fail-closed rules, each pinned by scripts/test_worker_container_rollback.py:
- Only allowlisted IDs, integers, statuses and fixed-vocabulary tokens reach
  the log or the evidence file. API, ownership and probe bodies are never
  echoed; the previous target configuration is held in memory only.
- A 401/403 from ownership control or Cloudflare stops the run. The run is
  never retried with another credential.
- The Worker restore POST never sends `force`, and `code_update_strategy` is
  `immediate`: the restored code must serve now, not after a deferred window.
- The stable gate needs STABLE_PASSES consecutive fully-passing polls; any
  non-passing poll resets the streak, and the deadline fails the run.
- The provenance receipt is immutable once built and is fully validated
  before the fence: no restore POST happens on an incomplete or mismatched
  (version, SHA, image) tuple. Snapshot rollbacks of builds deployed
  before SHA stamping keep the legacy version-header identity and say so
  in the evidence; manual rollbacks have no legacy path.
"""

import argparse
from dataclasses import dataclass
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
from urllib.request import Request

sys.path.insert(0, str(Path(__file__).resolve().parent))
# Same E402-safe convention as the other scripts/ tools (see test header).
import staging_rollout as base

GateError = base.GateError
require = base.require


class DuplicateKeyError(ValueError):
    """A JSON object repeats a key; the message never carries key/value data."""


def _reject_duplicate_keys(pairs):
    """object_pairs_hook refusing repeats instead of last-wins.

    The json decoder calls this for every object at any depth, after
    escape decoding, so {"a": 1, "\\u0061": 2} counts as a repeat. The
    raised message is fixed vocabulary: it names no key, value or path.
    """
    seen = set()
    for key, _value in pairs:
        if key in seen:
            raise DuplicateKeyError("duplicate JSON object key")
        seen.add(key)
    return dict(pairs)


VERSION_ID = re.compile(base.UUID)
SHA = re.compile(r"[0-9a-f]{40}")
# Consecutive fully-passing gate polls required: ~10 s per poll, so 18 is the
# >=3 min stability the release gate demands after takeover.
STABLE_PASSES = 18
POLL_SECONDS = 10
ROLLOUT_TIMEOUT_SECONDS = 600
GATE_TIMEOUT_SECONDS = 900
CONTROL_TIMEOUT_SECONDS = 420
# HTTP statuses that stop the run without retry (see the module docstring).
AUTH_STATUSES = ("401", "403")
# Above the control client's takeover window plus one attempt of two 15 s
# requests, so a fence/takeover POST is never killed mid-flight.
ACTOR = re.compile(r"[a-zA-Z0-9_.:@/-]{1,128}")
SAFE_CONTROL_LINE = re.compile(
    r"(Ownership control failed \(HTTP [0-9]{3}\)|"
    r"Ownership control request (timeout|failed)|"
    r"Ownership control invalid response|"
    r"Singleton is intentionally fenced or uninitialized|"
    r"Ownership transition not confirmed|"
    r"OWNERSHIP_CONTROL_TOKEN is missing or invalid|"
    r"Expected the approved two-bot-next-staging workers\.dev origin|"
    r"Expected the approved two-bot-next production origin)"
)
VARIANTS = ("staging", "production")
WORKERS = {"staging": "two-bot-next-staging", "production": "two-bot-next-production"}
APPLICATIONS = {
    "staging": "two-bot-next-twobotcontainer-staging",
    "production": "two-bot-next-twobotcontainer-production",
}
CONTROL_SCRIPTS = {
    "staging": "ownership-control.mjs",
    "production": "production-ownership-control.mjs",
}
STAGING_URL = re.compile(r"https://two-bot-next-staging\.[a-z0-9-]+\.workers\.dev")


class RollbackClient(base.Client):
    """The staging gate's read-only client plus authenticated POST."""

    def post(self, path, body):
        data = json.dumps(body).encode()
        request = Request(
            self.base + path,
            data=data,
            method="POST",
            headers={
                "Authorization": f"Bearer {self.token}",
                "Content-Type": "application/json",
                "User-Agent": base.USER_AGENT,
            },
        )
        try:
            with self.opener.open(request, timeout=20) as response:
                raw = response.read(base.MAX_BODY + 1)
                status = response.status
        except base.HTTPError as error:
            codes = []
            try:
                envelope = base._loads_bounded(error.read(base.MAX_BODY))
                codes = [
                    str(item["code"])
                    for item in envelope.get("errors", [])
                    if isinstance(item, dict) and type(item.get("code")) is int
                ][:4]
            except (OSError, ValueError, AttributeError, TypeError):
                pass
            raise GateError(
                "api_http_failure", f"status={error.code} codes={','.join(codes) or 'none'}"
            ) from None
        except (base.URLError, TimeoutError, OSError):
            raise GateError("api_transport_failure") from None
        # Cloudflare answers a created container rollout with 201, a Worker deployment with 200.
        require(status in (200, 201) and len(raw) <= base.MAX_BODY, "api_http_failure", f"status={status}")
        data = base.mapping(base.decode(raw))
        require(data.get("success") is True and "result" in data, "api_failure")
        return data["result"]


def application_by_name(client, name):
    apps = base.sequence(client.api("/containers/applications"))
    candidates = [base.mapping(app) for app in apps if base.mapping(app).get("name") == name]
    require(len(candidates) == 1, "application_identity_missing_or_ambiguous")
    app = candidates[0]
    base.identifier(app.get("id"))
    base.identifier(base.mapping(app.get("durable_objects")).get("namespace_id"))
    return app


def app_rollouts(client, app_id):
    rows = base.sequence(
        client.api(f"/containers/applications/{base.identifier(app_id)}/rollouts?limit={base.LIMIT}")
    )
    require(len(rows) < base.LIMIT, "rollout_snapshot_truncated")
    ids = [base.identifier(base.mapping(row).get("id")) for row in rows]
    require(len(ids) == len(set(ids)), "duplicate_rollout_identity")
    return rows


def deployments(client, worker):
    rows = base.sequence(
        base.mapping(client.api(f"/workers/scripts/{base.identifier(worker)}/deployments")).get(
            "deployments"
        )
    )
    require(bool(rows), "worker_deployment_missing")
    result = []
    for row in rows:
        row = base.mapping(row)
        versions = base.sequence(row.get("versions"))
        if len(versions) == 1 and base.mapping(versions[0]).get("percentage") == 100:
            version = versions[0].get("version_id")
            require(isinstance(version, str) and VERSION_ID.fullmatch(version), "invalid_version_id")
            result.append(version)
        else:
            result.append(None)  # split traffic is never a rollback source or target
    return result


def serving_version(client, worker):
    current = deployments(client, worker)[0]
    require(current is not None, "split_traffic_deployment")
    return current


def newest_completed_rollout(rows):
    completed = [
        base.mapping(row) for row in rows if base.mapping(row).get("status") == "completed"
    ]
    require(bool(completed), "no_completed_rollout")
    return max(completed, key=lambda row: base.timestamp(row.get("created_at")))


def image_pattern(application):
    # Digest-only: a mutable `:tag` prefix cannot pin the build.
    return rf"registry\.cloudflare\.com/[^/:@\s]+/{re.escape(application)}@sha256:[0-9a-f]{{64}}"


def rollout_image(row, application):
    image = base.mapping(row.get("target_configuration")).get("image")
    require(isinstance(image, str) and re.fullmatch(image_pattern(application), image),
            "invalid_rollout_image")
    return image


def build_snapshot(client, worker, application):
    """Snapshot that never blocks a first deploy: an environment with no
    serving deployment or no completed rollout records `has_previous: False`,
    so there is simply nothing to restore. Genuine API failures still refuse."""
    try:
        return snapshot_state(client, worker, application)
    except GateError as error:
        if str(error) in ("worker_deployment_missing", "no_completed_rollout"):
            return {"has_previous": False, "worker": worker, "application": application}
        raise


def snapshot_state(client, worker, application):
    """Allowlisted pre-deploy serving state; safe as a public workflow artifact."""
    app = application_by_name(client, application)
    namespace = base.mapping(app.get("durable_objects")).get("namespace_id")
    base.identifier(namespace)
    history = deployments(client, worker)
    if history[0] is None:
        return {"has_previous": False, "worker": worker, "application": application}
    rows = app_rollouts(client, app["id"])
    prev = newest_completed_rollout(rows)
    # The provenance receipt's SHA member: the serving version's stamped
    # deploy message. An unstamped version (pre-stamp, or a secret change
    # without --message) records None (the receipt is incomplete) instead
    # of blocking the deploy this snapshot precedes.
    serving_sha = version_sha(client, worker, history[0])
    return {
        "has_previous": True,
        "worker": worker,
        "application": application,
        "application_id": app["id"],
        "namespace_id": namespace,
        "serving_version": history[0],
        "serving_sha": serving_sha,
        "provenance_complete": serving_sha is not None,
        "serving_rollout_id": prev["id"],
        "serving_image": rollout_image(prev, application),
        "recorded_at": time.time(),
    }


def restore_image_shape(image, application):
    """A manual rollback names its container image explicitly; only a
    digest-pinned registry URL for this application is accepted."""
    require(isinstance(image, str) and re.fullmatch(image_pattern(application), image),
            "invalid_target_image")
    return image


@dataclass(frozen=True)
class ProvenanceReceipt:
    """The known-good (Worker version, Git SHA, container image) tuple a
    rollback restores. Built once, immutable, and shape-validated here;
    `validate_receipt_against_live` binds it to Cloudflare state before
    any restore POST. All three members are content identifiers (a UUID,
    a commit SHA, an image digest), safe in logs and workflow artifacts."""

    worker: str
    application: str
    version: str
    sha: str
    image: str

    def __post_init__(self):
        require(isinstance(self.worker, str) and bool(self.worker),
                "provenance_worker_invalid")
        require(isinstance(self.application, str) and bool(self.application),
                "provenance_application_invalid")
        require(isinstance(self.version, str) and VERSION_ID.fullmatch(self.version),
                "provenance_version_invalid")
        require(isinstance(self.sha, str) and SHA.fullmatch(self.sha),
                "provenance_sha_invalid")
        require(isinstance(self.image, str)
                and re.fullmatch(image_pattern(self.application), self.image),
                "provenance_image_invalid")

    def validate(self, *, version, sha, image):
        """Refuse unless the named inputs are exactly this receipt's tuple.
        The detail names the mismatched member in fixed vocabulary only."""
        for member, expected, actual in (
                ("version", self.version, version),
                ("sha", self.sha, sha),
                ("image", self.image, image)):
            require(actual == expected, "provenance_mismatch", f"member={member}")


def version_sha(client, worker, version):
    """The Git SHA a Worker version was deployed from: its immutable
    `workers/message` annotation (`wrangler deploy --message <sha>`, or
    `wrangler versions secret put --message <sha>` for a secret change).
    Returns None for an unstamped version — deployed before SHA stamping,
    or a secret change without --message — whose annotation is absent or
    not a commit SHA."""
    data = base.mapping(client.api(
        f"/workers/scripts/{base.identifier(worker)}/versions/{base.identifier(version)}"))
    require(data.get("id") == version, "version_identity_drift")
    annotations = data.get("annotations")
    message = annotations.get("workers/message") if isinstance(annotations, dict) else None
    return message if isinstance(message, str) and SHA.fullmatch(message) else None


def replaced_converged(row):
    """Convergence evidence for a `replaced` provenance row.

    A rollout can be `replaced` before it finishes (superseded in flight),
    so status alone never proves the image rolled out. Accept only rows
    whose record shows every step completed and every instance updated.
    Any malformed shape reads as unconverged: fail closed. Mirrors the
    staging drill's replaced_converged (scripts/staging_container_drill.py).
    """
    try:
        record = base.mapping(row)
        steps = base.sequence(record.get("steps"))
        progress = base.mapping(record.get("progress"))
        total_steps = base.number(progress.get("total_steps"))
        total = base.number(progress.get("total_instances"))
        updated = base.number(progress.get("updated_instances"))
        steps_done = all(base.mapping(step).get("status") == "completed"
                         for step in steps)
    except GateError:
        return False
    if not steps or total_steps <= 0 or len(steps) != total_steps:
        return False
    if not steps_done:
        return False
    return total > 0 and total == updated


def validate_receipt_against_live(client, receipt):
    """Bind a receipt to live Cloudflare state with reads only, before any
    restore POST: the version must have been deployed from the receipt
    SHA and must have served 100% before, and the image must come from a
    rollout of this application that completed (status completed, or
    replaced with convergence evidence: Cloudflare flips every superseded
    rollout to replaced, so after any deploy the previous image lives only
    there, but a rollout can also be replaced before it finishes). Rollout
    images carry no version link, so served-before membership is the
    strongest image binding the API offers; version and SHA bind exactly."""
    require(version_sha(client, receipt.worker, receipt.version) == receipt.sha,
            "provenance_mismatch", "member=sha")
    require(receipt.version in deployments(client, receipt.worker),
            "provenance_version_never_served")
    app = application_by_name(client, receipt.application)
    served = set()
    for row in app_rollouts(client, app["id"]):
        row = base.mapping(row)
        image = base.mapping(row.get("target_configuration")).get("image")
        status = row.get("status")
        proven = (status == "completed"
                  or (status == "replaced" and replaced_converged(row)))
        if (proven and isinstance(image, str)
                and re.fullmatch(image_pattern(receipt.application), image)):
            served.add(image)
    require(receipt.image in served, "provenance_image_never_served")


def gateway_state(report):
    components = report.get("components") if isinstance(report, dict) else None
    if not isinstance(components, list):
        return "absent"
    for row in components:
        if isinstance(row, list) and len(row) == 2 and row[0] == "gateway":
            return row[1] if row[1] in ("ready", "starting", "down") else "unready"
    return "absent"


def control_client(variant, token, worker_url, staging_url, actor, root=None):
    """Run the reviewed ownership-control client for the variant.

    The token only ever travels in the child's environment; failures carry
    fixed vocabulary, never request or response text. Takeover calls pin
    `expected_deployment` to the restored Worker version, so a retry answered
    by a stale version refuses instead of re-posting to it.
    """
    root = Path(root or Path(__file__).resolve().parents[1])

    def control(action, epoch=None, expected_deployment=None):
        command = ["node", str(root / "wrangler/scripts" / CONTROL_SCRIPTS[variant]), action]
        if epoch is not None:
            command.append(str(epoch))
        env = {
            "PATH": os.environ.get("PATH", os.defpath),
            "OWNERSHIP_CONTROL_TOKEN": token,
            "OWNERSHIP_ACTOR": actor,
            "OWNERSHIP_RELEASE_FENCE": "true" if action in (
                "takeover", "deployment-takeover") else "false",
        }
        if variant == "production":
            env["PRODUCTION_WORKER_URL"] = worker_url
            env["STAGING_WORKER_URL"] = staging_url
        else:
            env["STAGING_WORKER_URL"] = worker_url
        if expected_deployment is not None:
            env["OWNERSHIP_EXPECTED_DEPLOYMENT"] = expected_deployment
        try:
            result = subprocess.run(
                command, env=env, capture_output=True, timeout=CONTROL_TIMEOUT_SECONDS, text=True
            )
        except (OSError, subprocess.SubprocessError):
            raise GateError("ownership_control_unavailable") from None
        if result.returncode != 0:
            line = (result.stderr.strip().splitlines() or [""])[0]
            match = SAFE_CONTROL_LINE.match(line)
            if match and any(f"HTTP {s}" in match.group(0) for s in AUTH_STATUSES):
                raise GateError("ownership_auth_failed")
            detail = match.group(0) if match else None
            # No operator release hint here: this client's takeover calls run
            # with OWNERSHIP_RELEASE_FENCE=true, so the staging client never
            # answers them with the fenced refusal (ownership-control.mjs
            # throws it only when !releaseFence), and the production client
            # throws the same prefix with a different release route. The
            # scoped fence guidance lives in the deploy-staging takeover
            # step, which runs with release_fence=false by default.
            raise GateError("ownership_control_failed", detail)
        try:
            return base.mapping(base._loads_bounded(result.stdout, object_pairs_hook=_reject_duplicate_keys))
        except (ValueError, RecursionError):
            raise GateError("ownership_control_invalid_response") from None

    return control


def owner_of(state):
    owner = state.get("owner")
    return owner if isinstance(owner, dict) else {}


class Rollback:
    def __init__(self, client, control, url, variant, worker, application, app_id,
                 expected_sha, run_id, *, now=time.time, sleep=time.sleep, log=print):
        self.client, self.control, self.url = client, control, url
        self.variant, self.worker, self.application, self.app_id = (
            variant, worker, application, app_id)
        self.expected_sha, self.run_id = expected_sha, run_id
        self.now, self.sleep, self.log = now, sleep, log
        self.evidence = {"worker": worker, "application": application, "variant": variant}

    def stamp(self, name):
        moment = self.now()
        self.log(f"{name} {moment:.1f}")
        return moment

    def restore_worker(self, version):
        """Move 100% of traffic to `version` without `force`; read it back."""
        self.client.post(f"/workers/scripts/{base.identifier(self.worker)}/deployments", {
            "strategy": "percentage", "versions": [{"version_id": version, "percentage": 100}],
            "annotations": {"workers/message": f"rollback to {version} run {self.run_id}"[:120]},
            "code_update_strategy": {"mode": "immediate"}})
        self.stamp("worker restored")
        require(serving_version(self.client, self.worker) == version, "rollback_not_active")

    def restore_image(self, image):
        """Roll the container image back with a single 100% rolling step."""
        started = self.now()
        before = {base.identifier(base.mapping(row).get("id")) for row in
                  app_rollouts(self.client, self.app_id)}
        result = self.client.post(
            f"/containers/applications/{base.identifier(self.app_id)}/rollouts", {
                "description": f"rollback to previous image run {self.run_id}"[:120],
                "strategy": "rolling", "kind": "full_auto", "step_percentage": 100,
                "target_configuration": {"image": image}})
        rollout_id = base.identifier(base.mapping(result).get("id", ""))
        require(rollout_id not in before, "rollout_not_created")
        deadline = started + ROLLOUT_TIMEOUT_SECONDS
        target_version = base.number(base.mapping(result).get("target_version", 0)) or None
        while True:
            row = base.mapping(self.client.api(
                f"/containers/applications/{base.identifier(self.app_id)}/rollouts/{rollout_id}"))
            require(row.get("id") == rollout_id, "rollout_identity_drift")
            status = row.get("status")
            require(status not in ("replaced", "reverted"), "rollout_replaced_or_reverted")
            require(status in ("pending", "progressing", "completed"), "unknown_rollout_status")
            require(base.mapping(row.get("target_configuration")).get("image") == image,
                    "rollout_identity_drift")
            if target_version is None:
                try:
                    target_version = base.number(row.get("target_version"))
                except GateError:
                    target_version = None
            # The pinned rollout proves the image leg, including the same
            # completed counter-lag shapes accepted by staging verify. Staging's
            # application listing can lag that record indefinitely; takeover
            # and the unchanged stable runtime gate still have to pass next.
            # Production retains its separate application-listing requirement.
            ready = (target_version is not None
                     and (base.converged(row, image, target_version)
                          or base.active_lag(row, image, target_version)))
            if target_version is None:
                waiting_for = "target_version"
            elif status != "completed":
                waiting_for = "rollout_completion"
            else:
                waiting_for = "counters_or_progress"
            if ready and self.variant != "staging":
                ready = base.mapping(application_by_name(
                    self.client, self.application).get("configuration")).get("image") == image
                waiting_for = "application_image"
            if ready:
                self.log(f"image rollout {status}")
                self.evidence.pop("image_restore_wait", None)
                self.evidence["restored_rollout_id"] = rollout_id
                self.stamp("image restored")
                return
            self.evidence["image_restore_wait"] = waiting_for
            self.log(f"image rollout {status} waiting_for={waiting_for}")
            require(self.now() < deadline, "image_rollout_timeout",
                    f"waiting_for={waiting_for} rollout={status}")
            self.sleep(POLL_SECONDS)

    def take_over(self, version):
        state = self.control("status")
        phase = owner_of(state).get("phase")
        require(phase in ("active", "fenced"), "owner_state_unexpected")
        epoch = owner_of(state).get("epoch")
        if phase == "active":
            require(type(epoch) is int, "owner_epoch_missing")
            self.control("fence", epoch)
            self.stamp("fenced")
            state = self.control("status")
            epoch = owner_of(state).get("epoch")
        if self.variant == "staging":
            self.control("deployment-takeover", expected_deployment=version)
        else:
            require(type(epoch) is int, "owner_epoch_missing")
            self.control("takeover", epoch, expected_deployment=version)
        self.stamp("ownership taken")
        self.evidence["takeover"] = True

    def gate_pass(self, version):
        """One fully-passing stable-gate poll: health 200, readyz 200 naming
        the restored version, the expected revision, and gateway ready."""
        health, _, _ = self.client.request(self.url + "/health")
        if health != 200:
            return False, f"health={health}"
        status, headers, body = self.client.request(self.url + "/readyz")
        if status != 200 or headers.get("x-two-worker-version") != version:
            return False, f"readyz={status} identity=mismatch"
        try:
            report = base.mapping(base.decode(body))
        except GateError:
            return False, f"readyz={status} body=unreadable"
        revision = report.get("build_revision")
        if revision == "unknown" and report.get("build_id") == "unknown":
            # Pre-stamp version: the restored build predates build identity.
            # Recorded as a pass like the release gate, never a failure.
            self.evidence["pre_stamp_recorded"] = True
        elif self.expected_sha is not None and revision != self.expected_sha:
            # The expected SHA is the receipt's: the dispatch's --expected-sha
            # for a manual rollback, or the snapshot's recorded serving SHA
            # for an auto-rollback. Only a pre-stamp snapshot carries none;
            # identity then rests on the x-two-worker-version header above.
            return False, f"readyz={status} identity=mismatch"
        if gateway_state(report) != "ready":
            return False, f"readyz={status} gateway=not-ready"
        return True, f"readyz={status} gateway=ready"

    def wait_stable(self, version, started):
        """STABLE_PASSES consecutive passing polls; any miss resets the streak."""
        deadline = started + GATE_TIMEOUT_SECONDS
        streak = 0
        polls = 0
        while True:
            passed, observation = self.gate_pass(version)
            polls += 1
            streak = streak + 1 if passed else 0
            self.log(f"gate poll {polls}: {observation} streak={streak}/{STABLE_PASSES}")
            if streak >= STABLE_PASSES:
                self.stamp("gate stable")
                self.evidence.update({"stable_polls": polls, "gate": "stable-200-gateway-ready"})
                return
            require(self.now() < deadline, "gate_timeout", f"polls={polls} last={observation}")
            self.sleep(POLL_SECONDS)

    def run(self, restore_version, restore_image):
        started = self.stamp("rollback start")
        current = serving_version(self.client, self.worker)
        if current != restore_version:
            self.take_over_fence_first()
            self.restore_worker(restore_version)
        else:
            self.log("worker already serves the restore version; image leg only")
        self.restore_image(restore_image)
        self.take_over(restore_version)
        self.wait_stable(restore_version, self.now())
        self.evidence.update({
            "restored_version": restore_version,
            "restored_image_digest": restore_image.rsplit("@", 1)[-1],
            "from_version": current,
        })
        return self.evidence

    def take_over_fence_first(self):
        """Fence the singleton before replacing anything, so a half-applied
        restore can never serve traffic without ownership."""
        state = self.control("status")
        if owner_of(state).get("phase") == "active":
            epoch = owner_of(state).get("epoch")
            require(type(epoch) is int, "owner_epoch_missing")
            self.control("fence", epoch)
            self.stamp("fenced")


def check_url(variant, url):
    url = url.rstrip("/")
    if variant == "staging":
        require(STAGING_URL.fullmatch(url), "invalid_staging_url")
    else:
        require(re.fullmatch(r"https://two-bot-next-production\.[a-z0-9-]+\.workers\.dev", url)
                or re.fullmatch(r"https://[a-z0-9-]+\.[a-z0-9-.]+\.[a-z]+", url),
                "invalid_production_url")
    return url


def print_dry_run_plan(*, client, worker, restore_version, restore_image,
                     receipt_sha, log=print):
    """Print the ordered restore plan; reads only, never a POST or mutation.

    The five lines name the fence, worker, image, takeover and gate steps,
    carrying the validated receipt members (version UUID, image digest URL,
    SHA). Only allowlisted IDs reach the log. The fence line mirrors the live
    path: a pre-restore fence when the worker leg runs, otherwise deferred to
    takeover, since the live path only fences inside takeover after the image
    rollout when the worker already serves the restore version.
    """
    current = serving_version(client, worker)
    if current != restore_version:
        log(f"dry-run plan: fence the singleton before restoring version {restore_version}")
        log(f"dry-run plan: worker POST deployments restoring version {restore_version}")
    else:
        log("dry-run plan: fence deferred to takeover; no pre-restore fence "
            "(worker leg skipped)")
        log(f"dry-run plan: worker already serves {restore_version}; image leg only")
    log(f"dry-run plan: image POST container rollout restoring image {restore_image}")
    log(f"dry-run plan: takeover ownership to version {restore_version}")
    if receipt_sha is not None:
        log(f"dry-run plan: gate stable readyz for version {restore_version} sha {receipt_sha}")
    else:
        log(f"dry-run plan: gate stable readyz for version {restore_version} pre-stamp no-sha")


def main(argv=None, *, environ=None, client_factory=RollbackClient):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    snap = commands.add_parser("snapshot", help="record the pre-deploy serving state")
    snap.add_argument("--variant", choices=VARIANTS, required=True)
    snap.add_argument("--worker", default=None)
    snap.add_argument("--application", default=None)
    snap.add_argument("--out", required=True)
    roll = commands.add_parser("rollback", help="restore the Worker version and container image")
    roll.add_argument("--variant", choices=VARIANTS, required=True)
    roll.add_argument("--worker", default=None)
    roll.add_argument("--application", default=None)
    roll.add_argument("--worker-url", required=True)
    roll.add_argument("--staging-url", default="")
    roll.add_argument("--expected-sha", default=None)
    roll.add_argument("--snapshot", default=None)
    roll.add_argument("--target-version", default=None)
    roll.add_argument("--target-image", default=None)
    roll.add_argument("--actor", required=True)
    roll.add_argument("--evidence", required=True)
    roll.add_argument("--dry-run", action="store_true",
                      help="validate the receipt and print the ordered restore plan "
                           "without any POST or ownership mutation")
    args = parser.parse_args(argv)
    env = os.environ if environ is None else environ
    rollback = None
    try:
        worker = args.worker or WORKERS[args.variant]
        application = args.application or APPLICATIONS[args.variant]
        client = client_factory(env.get("CLOUDFLARE_ACCOUNT_ID"), env.get("CLOUDFLARE_API_TOKEN"))
        if args.command == "snapshot":
            state = build_snapshot(client, worker, application)
            Path(args.out).write_text(json.dumps(state, indent=2, sort_keys=True) + "\n")
            print("rollback snapshot: " + json.dumps(state, sort_keys=True))
            return 0
        url = check_url(args.variant, args.worker_url)
        manual = bool(args.target_version) or bool(args.target_image)
        require(bool(args.snapshot) != manual,
                "either --snapshot or --target-version with --target-image is required")
        if manual:
            require(bool(args.target_version) and bool(args.target_image),
                    "--target-version needs --target-image")
        expected_sha = args.expected_sha
        if expected_sha is not None:
            require(SHA.fullmatch(expected_sha), "invalid_expected_sha")
        receipt = None
        if args.snapshot:
            try:
                snap_state = base.mapping(base.decode(Path(args.snapshot).read_bytes()))
            except OSError:
                raise GateError("invalid_local_snapshot") from None
            require(snap_state.get("has_previous") is True, "snapshot_has_no_previous")
            require(snap_state.get("worker") == worker
                    and snap_state.get("application") == application, "snapshot_identity_mismatch")
            restore_version = snap_state.get("serving_version")
            require(isinstance(restore_version, str) and VERSION_ID.fullmatch(restore_version),
                    "invalid_snapshot_version")
            prev_rollout = base.identifier(snap_state.get("serving_rollout_id"))
            row = base.mapping(client.api(
                f"/containers/applications/{base.identifier(snap_state.get('application_id'))}"
                f"/rollouts/{prev_rollout}"))
            require(row.get("id") == prev_rollout, "rollout_identity_drift")
            restore_image = rollout_image(row, application)
            # The snapshot recorded the image it snapshotted; the live
            # re-read of that same rollout must still name it.
            recorded_image = snap_state.get("serving_image")
            if recorded_image is not None:
                require(recorded_image == restore_image,
                        "provenance_mismatch", "member=image")
            # A complete snapshot carries the serving build's SHA: it
            # becomes the stable gate's expected revision, and an explicit
            # --expected-sha must agree with it. The receipt is then bound
            # to live Cloudflare state exactly like a manual rollback's, so
            # a stale or hand-edited snapshot refuses before any POST
            # instead of at the stable gate after traffic moved. A
            # pre-stamp snapshot has no SHA member and keeps the legacy
            # header-only identity.
            recorded_sha = snap_state.get("serving_sha")
            if recorded_sha is not None:
                require(isinstance(recorded_sha, str) and SHA.fullmatch(recorded_sha),
                        "invalid_snapshot_sha")
                receipt = ProvenanceReceipt(worker, application, restore_version,
                                            recorded_sha, restore_image)
                if expected_sha is not None:
                    receipt.validate(version=restore_version, sha=expected_sha,
                                     image=restore_image)
                validate_receipt_against_live(client, receipt)
                expected_sha = receipt.sha
        else:
            # A manual rollback is admitted only as a complete receipt:
            # version, image and the SHA that version was deployed from,
            # validated against live Cloudflare state below. No member is
            # optional, so no arbitrary CI-green SHA can ride along.
            require(expected_sha is not None, "manual_rollback_needs_expected_sha")
            restore_version = args.target_version
            require(isinstance(restore_version, str) and VERSION_ID.fullmatch(restore_version),
                    "invalid_target_version")
            restore_image = restore_image_shape(args.target_image, application)
            receipt = ProvenanceReceipt(worker, application, restore_version,
                                        expected_sha, restore_image)
            validate_receipt_against_live(client, receipt)
        if args.dry_run:
            # The receipt is validated identically to the live path above; the
            # plan below reads the serving version only and never POSTs, never
            # touches ownership control, and writes no evidence file.
            print_dry_run_plan(client=client, worker=worker,
                               restore_version=restore_version,
                               restore_image=restore_image,
                               receipt_sha=receipt.sha if receipt is not None else expected_sha)
            return 0
        token = env.get("OWNERSHIP_CONTROL_TOKEN", "")
        require(len(token) >= 32, "missing_ownership_token")
        require(ACTOR.fullmatch(args.actor), "invalid_actor")
        control = control_client(args.variant, token, url, args.staging_url, args.actor)
        rollback = Rollback(client, control, url, args.variant, worker, application,
                            application_by_name(client, application)["id"],
                            expected_sha, env.get("GITHUB_RUN_ID", ""),
                            log=print)
        evidence = rollback.run(restore_version, restore_image)
        if receipt is not None:
            evidence.update({"provenance": "validated", "provenance_sha": receipt.sha})
        else:
            evidence["provenance"] = "legacy-no-sha"
    except GateError as error:
        print(f"worker-container rollback failed: {error}")
        if error.detail:
            print(f"worker-container rollback diagnostic: {error.detail}")
        if rollback is not None and str(error) == "image_rollout_timeout":
            evidence = dict(rollback.evidence, failure="image_rollout_timeout")
            Path(args.evidence).write_text(json.dumps(evidence, indent=2, sort_keys=True) + "\n")
        return 1
    Path(args.evidence).write_text(json.dumps(evidence, indent=2, sort_keys=True) + "\n")
    print("worker-container rollback evidence: " + json.dumps(evidence, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
