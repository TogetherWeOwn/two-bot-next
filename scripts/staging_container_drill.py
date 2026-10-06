#!/usr/bin/env python3
"""Staging-only reviewed-image container backout/restore drill.

One dispatch backouts staging to a previously reviewed Rust source/image pair
and restores the baseline through the FULL container rollout path, so a bad
Rust image can be recovered without rebuilding old source. It never performs
a Worker-versions-only rollback: every leg fences the singleton, deploys the
pinned pre-built image with the reviewed `wrangler deploy` full-container
path, hands ownership to it with an epoch-checked takeover, and verifies the
exact rollout convergence plus the exact Worker/image identity before timing
the first ready probe.

Fail-closed rules, each pinned by scripts/test_staging_container_drill.py:
- Orchestration always runs from `main` (`github.ref == refs/heads/main` in
  the workflow); the backout target arrives only as immutable pins
  (source SHA, build id, digest-pinned registry image, Worker version UUID,
  prior completed rollout id). Mutable tags, `latest` and non-matching
  values are refused before any change.
- The backout image must differ from the serving image, must equal the image
  of a named prior rollout that completed on staging, and the named Worker
  version must have served staging traffic with the same durable-object
  namespace as the baseline. Same-image, never-served and
  binding-incompatible pairs are refused before any change. The named-rollout
  proof is three separate refusals: `backout_rollout_absent` (pin missing
  from the live rollouts page, including pins aged out of it),
  `backout_rollout_not_completed` (`status=<allowlisted>`; `replaced` and
  `reverted` stay unproven even when the image matches, so pins expire on
  any newer rollout and the target stays the newest completed rollout with
  a differing image), and `backout_rollout_image_mismatch`.
- The restore leg returns to the live-captured healthy baseline (serving
  Worker version, serving image, `/readyz` revision/build); the final
  running image must equal the baseline image or the drill fails.
- A 401/403 from ownership control or Cloudflare stops the run. The restore
  leg is skipped, never retried with another credential.
- Readiness requires every `/readyz` component `ready` (process AND gateway
  plus database) on the exact Worker version, revision and build id, with a
  converged full_auto rollout and the application listing pointing at the
  intended image. A converged rollout alone is not readiness.
- Exactly one gateway session per leg is witnessed, never assumed: the
  drill records a temporally complete probe timeline (baseline ready,
  teardown gap, first ready on the target, confirmation probes) and folds in
  an optional operator log-count attestation. Without a covering
  distinct-sessions==1 attestation the witness is NOT_PROVEN and the drill
  fails acceptance; the restore still runs. One healthy instance or a bare
  `/readyz` 200 is never that witness.
- Only allowlisted ids, digests, integers, statuses and timestamps reach the
  log or the evidence file. Wrangler output, API/ownership/probe bodies,
  session identifiers, tokens and configs are never echoed.
"""

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import tomllib
from urllib.error import HTTPError, URLError
from urllib.request import Request

sys.path.insert(0, str(Path(__file__).resolve().parent))
import staging_rollout as rollout  # noqa: E402
from staging_rollback_drill import ownership_control  # noqa: E402

GateError = rollout.GateError
require = rollout.require
WORKER = rollout.WORKER
APPLICATION = rollout.APPLICATION
CLASS = rollout.CLASS
IMAGE = re.compile(rollout.IMAGE)
VERSION_ID = re.compile(rollout.UUID)
SHA = re.compile(r"[0-9a-f]{40}")
BUILD_ID = re.compile(r"[0-9]{1,20}-[0-9]{1,20}")
RUN_ID = re.compile(r"[0-9]{1,20}")
# Dispatcher-attested provenance references: strict tokens only, so a review
# citation or run id can never smuggle a body, identifier or secret.
ATTESTED_REF = re.compile(r"[A-Za-z0-9_.,:+\-/]{1,120}")
NOTE = re.compile(r"[A-Za-z0-9_.,:+\-]{1,200}")
ACTOR = re.compile(r"[a-zA-Z0-9_.:@/-]{1,128}")
AUTH_STATUSES = ("401", "403")
READY_BUDGET_SECONDS = 60
READY_TIMEOUT_SECONDS = 300
POLL_SECONDS = 5
CONFIRM_PROBES = 2
STALE_IMAGE_POLLS = 3
WRANGLER_TIMEOUT_SECONDS = 600


class ContainerDrillClient(rollout.Client):
    """The gate's read-only client, preserving only the numeric HTTP status.

    `staging_rollout.Client` strips authenticated error bodies down to a bare
    code so a 401/403 cannot leak configuration values. The drill additionally
    needs the numeric status to tell an authentication failure (restore must
    be skipped, never retried with another credential) from any other API
    failure, so this override keeps `status=` plus integer error codes only,
    mirroring `staging_rollback_drill.DrillClient.post`.
    """

    def api(self, path):
        request = Request(
            self.base + path,
            headers={"Authorization": f"Bearer {self.token}",
                     "Cache-Control": "no-cache",
                     "User-Agent": rollout.USER_AGENT})
        try:
            with self.opener.open(request, timeout=20) as response:
                raw = response.read(rollout.MAX_BODY + 1)
                status = response.status
        except HTTPError as error:
            codes = []
            try:
                envelope = json.loads(error.read(rollout.MAX_BODY))
                codes = [str(item["code"]) for item in envelope.get("errors", [])
                         if isinstance(item, dict) and type(item.get("code")) is int][:4]
            except (OSError, ValueError, AttributeError, TypeError):
                pass
            raise GateError("api_http_failure",
                            f"status={error.code} codes={','.join(codes) or 'none'}") from None
        except (URLError, TimeoutError, OSError):
            raise GateError("api_transport_failure") from None
        require(status == 200 and len(raw) <= rollout.MAX_BODY, "api_http_failure",
                f"status={status}")
        data = rollout.mapping(rollout.decode(raw))
        require(data.get("success") is True and "result" in data, "api_failure")
        return data["result"]


def iso(epoch):
    return datetime.fromtimestamp(epoch, timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def deployments(client):
    """Newest-first Worker version ids serving 100% (None for split traffic)."""
    rows = rollout.sequence(
        rollout.mapping(client.api(f"/workers/scripts/{WORKER}/deployments")).get("deployments"))
    require(bool(rows), "worker_deployment_missing")
    result = []
    for row in rows:
        versions = rollout.sequence(rollout.mapping(row).get("versions"))
        if len(versions) == 1 and rollout.mapping(versions[0]).get("percentage") == 100:
            version = versions[0].get("version_id")
            require(isinstance(version, str) and VERSION_ID.fullmatch(version),
                    "invalid_version_id")
            result.append(version)
        else:
            result.append(None)
    return result


def serving_version(client):
    current = deployments(client)[0]
    require(current is not None, "split_traffic_deployment")
    return current


def completed_rollouts(client, app_id):
    rows = rollout.rollouts(client, app_id)
    return [rollout.mapping(row) for row in rows
            if mapping_status(rollout.mapping(row)) == "completed"]


def mapping_status(row):
    status = row.get("status")
    return status if isinstance(status, str) else ""


def rollout_image(row):
    image = rollout.mapping(row.get("target_configuration")).get("image")
    return image if isinstance(image, str) and IMAGE.fullmatch(image) else None


def serving_image(client, app):
    """The image staging actually serves: listing plus newest completed rollout."""
    listing = rollout.mapping(app.get("configuration")).get("image")
    require(isinstance(listing, str) and IMAGE.fullmatch(listing),
            "serving_image_unproven")
    rows = completed_rollouts(client, app["id"])
    require(bool(rows), "serving_image_unproven")
    newest = max(rows, key=lambda row: rollout.timestamp(row.get("created_at")))
    image = rollout_image(newest)
    require(image is not None, "serving_image_unproven")
    require(listing == image, "application_image_drift")
    return image


def probe_report(client, url):
    """ readiness probe returning (readyz status, health status, report or None)."""
    status, headers, body = client.request(url + "/readyz")
    health, _, _ = client.request(url + "/health")
    try:
        report = rollout.mapping(rollout.decode(body))
    except GateError:
        return status, health, headers, None
    return status, health, headers, report


def report_identity(report, headers):
    if report is None:
        return None, None, None
    return (headers.get("x-two-worker-version"), report.get("build_revision"),
            report.get("build_id"))


def validate_pin(raw):
    """Immutable backout pins; every mutable/latest/malformed value refused."""
    require(isinstance(raw.get("source_sha"), str) and SHA.fullmatch(raw["source_sha"]),
            "invalid_source_sha")
    require(isinstance(raw.get("build_id"), str) and BUILD_ID.fullmatch(raw["build_id"]),
            "invalid_build_id")
    require(isinstance(raw.get("image"), str) and IMAGE.fullmatch(raw["image"]),
            "invalid_image_digest")
    require(isinstance(raw.get("worker_version"), str)
            and VERSION_ID.fullmatch(raw["worker_version"]), "invalid_worker_version")
    try:
        rollout.identifier(raw.get("rollout_id"))
    except GateError:
        raise GateError("invalid_rollout_id") from None
    require(isinstance(raw.get("review_ref"), str)
            and ATTESTED_REF.fullmatch(raw["review_ref"]), "invalid_review_ref")
    require(isinstance(raw.get("staging_run_id"), str)
            and RUN_ID.fullmatch(raw["staging_run_id"]), "invalid_run_id")
    require(isinstance(raw.get("compatibility_note"), str)
            and NOTE.fullmatch(raw["compatibility_note"]), "invalid_compatibility_note")
    return {"source_sha": raw["source_sha"], "build_id": raw["build_id"],
            "image": raw["image"], "worker_version": raw["worker_version"],
            "rollout_id": raw["rollout_id"], "review_ref": raw["review_ref"],
            "staging_run_id": raw["staging_run_id"],
            "compatibility_note": raw["compatibility_note"]}


def validate_attestation(text):
    """Optional operator log-count attestation: counts and windows only.

    Shape: {"drill": {"distinct_sessions": 1, "window_start": iso,
    "window_end": iso}, "restore": {...}}. No identifiers, bodies or configs
    can survive validation: only the two leg names, small integers and ISO
    timestamps are accepted.
    """
    if text is None or text == "":
        return None
    try:
        raw = json.loads(text)
    except ValueError:
        raise GateError("invalid_session_attestation") from None
    require(isinstance(raw, dict), "invalid_session_attestation")
    attestation = {}
    for leg in ("drill", "restore"):
        entry = raw.get(leg)
        if entry is None:
            continue
        require(isinstance(entry, dict), "invalid_session_attestation")
        require(set(entry) == {"distinct_sessions", "window_start", "window_end"},
                "invalid_session_attestation")
        count = entry.get("distinct_sessions")
        require(type(count) is int and 0 <= count <= 9, "invalid_session_attestation")
        try:
            start = rollout.timestamp(entry.get("window_start"))
            end = rollout.timestamp(entry.get("window_end"))
        except GateError:
            raise GateError("invalid_session_attestation") from None
        require(start <= end, "invalid_session_attestation")
        attestation[leg] = {"distinct_sessions": count, "window_start": start,
                            "window_end": end}
    require(set(raw) <= {"drill", "restore"}, "invalid_session_attestation")
    return attestation


class Drill:
    def __init__(self, client, control, url, run_id, wrangler, *, now=time.time,
                 sleep=time.sleep, log=print):
        self.client, self.control, self.url = client, control, url
        self.run_id, self.now, self.sleep, self.log = run_id, now, sleep, log
        # (deploy config path, deploy closure, wrangler directory holding the
        # reviewed wrangler.toml) for the full-container deploy.
        self.deploy_path, self.deploy, self.wrangler_dir = wrangler
        self.mutated = False
        self.auth_failed = False
        self.evidence = {"worker": WORKER, "application": APPLICATION,
                         "budget_seconds": READY_BUDGET_SECONDS}

    # -- primitives ---------------------------------------------------------
    def owner(self, state):
        owner = state.get("owner")
        return owner if isinstance(owner, dict) else {}

    def stamp(self, name):
        moment = self.now()
        self.log(f"{name} {iso(moment)}")
        return moment

    def deploy_image(self, image, label):
        """Full-container deploy of the pinned pre-built image, no rebuild.

        The deploy config is generated from the reviewed `wrangler.toml` on
        main with only the container image overridden to the digest-pinned
        registry reference (Cloudflare docs: for a registry image
        `wrangler deploy` uses the configured reference without building).
        Old source is never checked out and no Dockerfile is rebuilt.
        Wrangler's own output may carry paths or gleamed config, so it is
        discarded: only the exit code and duration are recorded.
        """
        generate_deploy_config(self.deploy_path, image, self.wrangler_dir)
        started = self.now()
        returncode = self.deploy(self.deploy_path)
        elapsed = round(self.now() - started, 1)
        require(returncode == 0, "deploy_failed", f"elapsed={elapsed}")
        return self.stamp(f"{label}: deployed"), elapsed

    def wait_ready(self, worker, revision, build_id, image, deployed,
                   expect_rollout=True):
        """Poll rollout convergence plus exact runtime identity until ready.

        Returns (first ready time, probe counts, timeline). The timeline is
        the temporally complete in-band half of the session witness: every
        poll records its UTC time, `/readyz` status, gateway state, version
        agreement and rollout convergence with no bodies or identifiers.
        """
        deadline = deployed + READY_TIMEOUT_SECONDS
        pinned, stale = None, 0
        probes = non_200 = 0
        timeline = []
        while True:
            app = rollout.application(self.client)
            if pinned is None and expect_rollout:
                rows = rollout.rollouts(self.client, app["id"])
                baseline_ids = self.evidence.get("rollout_ids_before", [])
                candidates = [rollout.mapping(row) for row in rows
                              if row.get("id") not in baseline_ids
                              and rollout_image(rollout.mapping(row)) == image]
                require(len(candidates) <= 1, "ambiguous_new_rollout")
                pinned = candidates[0] if candidates else None
            if pinned is None and not expect_rollout:
                # No deploy happened on this leg (restore after a refused
                # backout deploy): verify the still-serving singleton
                # directly instead of waiting for a rollout that never comes.
                status, health, headers, report = probe_report(self.client, self.url)
                probes += 1
                if status != 200:
                    non_200 += 1
                version_ok = (headers.get("x-two-worker-version") == worker
                              and report is not None
                              and report.get("build_revision") == revision
                              and report.get("build_id") == build_id)
                components = report.get("components") if report is not None else None
                gateway_ready = (status == 200 and isinstance(components, list)
                                 and bool(components) and all(
                                     isinstance(part, list) and len(part) == 2
                                     and part[1] == "ready" for part in components))
                timeline.append((self.now(), status, gateway_ready, version_ok, False))
                if gateway_ready and version_ok and health == 200:
                    moment = self.stamp("ready")
                    return (moment, {"probes": probes, "non_200_readyz": non_200},
                            timeline)
                require(self.now() < deadline, "ready_timeout",
                        f"probes={probes} last_readyz={status}")
                self.sleep(POLL_SECONDS)
                continue
            status = health = 0
            gateway_ready = version_ok = converged_now = False
            if pinned is not None:
                row = rollout.mapping(self.client.api(
                    f"/containers/applications/{app['id']}/rollouts/"
                    f"{rollout.identifier(pinned['id'])}"))
                require(row.get("id") == pinned["id"], "rollout_identity_drift")
                target = rollout.number(row.get("target_version"))
                converged_now = rollout.converged(row, image, target)
                listing = rollout.mapping(app.get("configuration")).get("image")
                if converged_now and listing != image:
                    stale += 1
                    require(stale <= STALE_IMAGE_POLLS, "application_image_drift")
                elif converged_now:
                    stale = 0
                    rollout.active_worker(self.client, worker)
                    status, health, headers, report = probe_report(self.client, self.url)
                    probes += 1
                    if status != 200:
                        non_200 += 1
                    version_ok = (headers.get("x-two-worker-version") == worker
                                  and report is not None
                                  and report.get("build_revision") == revision
                                  and report.get("build_id") == build_id)
                    components = report.get("components") if report is not None else None
                    gateway_ready = (status == 200 and isinstance(components, list)
                                     and all(isinstance(part, list) and len(part) == 2
                                             and part[1] == "ready" for part in components)
                                     and bool(components))
                    if (gateway_ready and version_ok and health == 200):
                        moment = self.stamp("ready")
                        timeline.append((moment, status, True, True, True))
                        if self.confirm(moment, worker, revision, build_id):
                            timeline.extend(self.confirm_timeline)
                            return (moment,
                                    {"probes": probes + CONFIRM_PROBES,
                                     "non_200_readyz": non_200},
                                    timeline)
                        version_ok = False  # confirmation disagreed: keep polling
            timeline.append((self.now(), status, gateway_ready, version_ok, converged_now))
            require(self.now() < deadline, "ready_timeout",
                    f"probes={probes} last_readyz={status}")
            self.sleep(POLL_SECONDS)

    confirm_timeline = ()

    def confirm(self, first_ready, worker, revision, build_id):
        """Post-ready confirmation probes: every pass must agree on the target.

        A later pass on another Worker version means two instances served
        across the handoff, which makes the single-session witness ambiguous.
        """
        self.confirm_timeline = []
        for _ in range(CONFIRM_PROBES):
            self.sleep(POLL_SECONDS)
            status, health, headers, report = probe_report(self.client, self.url)
            _, revision_seen, build_seen = report_identity(report, headers)
            agreed = (status == 200 and health == 200
                      and headers.get("x-two-worker-version") == worker
                      and revision_seen == revision and build_seen == build_id)
            self.confirm_timeline.append((self.now(), status, agreed, agreed, True))
            if not agreed:
                return False
        self.stamp("ready confirmed")
        return True

    # -- phases -------------------------------------------------------------
    def preflight(self, pin):
        app = rollout.application(self.client)
        history = deployments(self.client)
        pre_worker = serving_version(self.client)
        pre_image = serving_image(self.client, app)
        require(pin["image"] != pre_image, "same_image_as_serving")
        require(pin["worker_version"] in history, "backout_worker_never_served")
        rows = {rollout.identifier(rollout.mapping(row).get("id")): rollout.mapping(row)
                for row in rollout.rollouts(self.client, app["id"])}
        # The live rollouts page holds at most ~50 rows despite limit=100
        # (retention ~31h seen 10-04 to 10-06), so a pin older than that reads
        # as absent here. Pins therefore expire on ANY newer rollout and are
        # always re-derived from a live read of this endpoint (distance-1:
        # newest completed rollout with a differing image).
        require(pin["rollout_id"] in rows, "backout_rollout_absent")
        proven = rows[pin["rollout_id"]]
        # Strict: `replaced` with a matching image stays unproven. A replaced
        # rollout was superseded (run 5: pin present, image matched, status
        # `replaced` after 5 newer rollouts), so its convergence proof no
        # longer belongs to it. The detail names the allowlisted status.
        proven_status = mapping_status(proven)
        require(proven_status == "completed", "backout_rollout_not_completed",
                "status=" + (proven_status if proven_status in
                              ("pending", "progressing", "completed",
                               "replaced", "reverted") else "unknown"))
        require(rollout_image(proven) == pin["image"], "backout_rollout_image_mismatch")
        namespace = rollout.identifier(
            rollout.mapping(app.get("durable_objects")).get("namespace_id"))
        require(rollout.worker_namespace(self.client, pin["worker_version"]) == namespace,
                "incompatible_worker_binding")
        state = self.control("status")
        require(self.owner(state).get("phase") == "active" and state.get("running") is True,
                "singleton_not_active_and_running")
        status, health, headers, report = probe_report(self.client, self.url)
        require(status == 200 and health == 200, "baseline_not_ready",
                f"readyz={status} health={health}")
        require(headers.get("x-two-worker-version") == pre_worker,
                "baseline_not_ready")
        revision, build_id = report.get("build_revision"), report.get("build_id")
        require(isinstance(revision, str) and SHA.fullmatch(revision)
                and isinstance(build_id, str) and BUILD_ID.fullmatch(build_id),
                "baseline_revision_unproven")
        epoch = self.owner(state).get("epoch")
        baseline = {"worker_version": pre_worker, "image": pre_image,
                    "source_sha": revision, "build_id": build_id}
        self.evidence.update({"pre_worker_version": pre_worker,
                              "pre_image": pre_image,
                              "pre_source_sha": revision,
                              "pre_build_id": build_id,
                              "backout": {key: pin[key] for key in
                                          ("source_sha", "build_id", "image",
                                           "worker_version", "rollout_id",
                                           "review_ref", "staging_run_id",
                                           "compatibility_note")}})
        if type(epoch) is int:
            self.evidence["epoch_before"] = epoch
        self.evidence["rollout_ids_before"] = [rollout.identifier(
            rollout.mapping(row).get("id")) for row in rows.values()]
        return baseline

    def leg(self, name, pin, attestation):
        """Fence, full-container deploy of `pin`, takeover, verify, witness."""
        state = self.control("status")
        phase = self.owner(state).get("phase")
        require(phase in ("active", "fenced"), "owner_state_unexpected",
                "phase=" + (phase if isinstance(phase, str)
                             and re.fullmatch(r"[a-z_]{1,20}", phase) else "unrecognized"))
        leg = self.evidence[name] = {"worker_version": pin["worker_version"],
                                     "source_sha": pin["source_sha"],
                                     "build_id": pin["build_id"],
                                     "intended_image": pin["image"]}
        self.mutated = True
        started = self.stamp(f"{name}: start")
        leg["start"] = iso(started)
        if phase == "active":
            self.control("fence", self.owner(state).get("epoch"))
        fenced = self.stamp(f"{name}: fenced")
        leg["fenced"] = iso(fenced)
        if serving_image(self.client, rollout.application(self.client)) != pin["image"]:
            deployed, deploy_elapsed = self.deploy_image(pin["image"], name)
            expect_rollout = True
        else:
            # Restore after a refused backout deploy: the baseline still serves.
            deployed, deploy_elapsed, expect_rollout = fenced, 0.0, False
        leg["deployed"] = iso(deployed)
        leg["deploy_elapsed_seconds"] = deploy_elapsed
        self.control("deployment-takeover", release_fence=True)
        taken = self.stamp(f"{name}: ownership taken")
        leg["takeover"] = iso(taken)
        target_worker = serving_version(self.client)
        first_ready, counts, timeline = self.wait_ready(
            target_worker, pin["source_sha"], pin["build_id"], pin["image"], deployed,
            expect_rollout)
        leg.update({"serving_worker_version": target_worker,
                    "first_ready": iso(first_ready),
                    "time_to_ready_seconds": round(first_ready - deployed, 1),
                    "outage_seconds": round(first_ready - fenced, 1), **counts})
        running = serving_image(self.client, rollout.application(self.client))
        leg["running_image"] = running
        require(running == pin["image"], "running_image_mismatch")
        leg["budget_met"] = leg["time_to_ready_seconds"] <= READY_BUDGET_SECONDS
        leg["session_witness"] = evaluate_session(
            timeline, (attestation or {}).get(name), fenced, first_ready)
        if leg["session_witness"]["status"] != "SINGLE_WITNESSED":
            raise GateError("session_not_proven",
                            leg["session_witness"]["reason"])
        return leg

    def run(self, pin, attestation):
        """Returns the failure codes; empty means backout, restore and both
        session witnesses passed."""
        failures = []
        baseline = None
        try:
            baseline = self.preflight(pin)
            self.leg("drill", pin, attestation)
        except GateError as error:
            failures.append(self.failed("drill", error))
        except Exception:  # noqa: BLE001 - no traceback: it may carry request data
            failures.append(self.failed("drill", GateError("unexpected_error")))
        if self.mutated and baseline and not self.auth_failed:
            restore_pin = {"source_sha": baseline["source_sha"],
                           "build_id": baseline["build_id"],
                           "image": baseline["image"],
                           "worker_version": baseline["worker_version"],
                           "rollout_id": "live-baseline",
                           "review_ref": "live-baseline",
                           "staging_run_id": self.run_id,
                           "compatibility_note": "live-baseline"}
            try:
                self.leg("restore", restore_pin, attestation)
            except GateError as error:
                failures.append(self.failed("restore", error))
            except Exception:  # noqa: BLE001
                failures.append(self.failed("restore", GateError("unexpected_error")))
        elif self.mutated and self.auth_failed:
            failures.append("restore_skipped_after_auth_failure")
            self.log("restore skipped: authentication failed; never substitute credentials. "
                     "Recover with deploy-staging workflow_dispatch (release_fence=true) once the "
                     "staging ownership binding is fixed.")
        self.evidence["failures"] = failures
        return failures

    def failed(self, phase, error):
        code = str(error)
        if code == "ownership_auth_failed" or (code == "api_http_failure" and
                                               any(f"status={s}" in (error.detail or "")
                                                   for s in AUTH_STATUSES)):
            self.auth_failed = True
        detail = f" ({error.detail})" if error.detail else ""
        self.log(f"{phase} failed: {code}{detail}")
        return f"{phase}:{code}"


def evaluate_session(timeline, attestation, fenced, first_ready):
    """Single-gateway-session witness verdict from the probe timeline plus the
    operator log-count attestation. Secret-safe by construction: the timeline
    holds only UTC times, statuses and booleans, and the attestation holds
    only a count plus its window. Anything absent or ambiguous is NOT_PROVEN,
    never PASS."""
    if not timeline:
        return {"status": "NOT_PROVEN", "reason": "timeline_empty"}
    if not any(entry[2] for entry in timeline):
        return {"status": "NOT_PROVEN", "reason": "no_ready_probe"}
    if attestation is None:
        return {"status": "NOT_PROVEN", "reason": "session_log_count_absent"}
    if attestation.get("distinct_sessions") != 1:
        return {"status": "NOT_PROVEN", "reason": "session_count_ambiguous"}
    if not (attestation["window_start"] <= fenced
            and attestation["window_end"] >= first_ready):
        return {"status": "NOT_PROVEN", "reason": "attestation_window_uncovered"}
    return {"status": "SINGLE_WITNESSED", "method": "probe_timeline_plus_log_count",
            "reason": "one_ready_version_across_handoff_with_covering_log_count"}


def generate_deploy_config(path, image, wrangler_dir):
    """Write a staging deploy config identical to `staging_rollout.prepare`
    except the container image is the pinned pre-built digest reference.

    Read from the reviewed `wrangler.toml` on main; the only mutation is the
    image override, so no old Dockerfile is rebuilt and no old source is
    checked out. `main` is absolutized because the generated config lives
    outside the wrangler directory.
    """
    source = Path(wrangler_dir) / "wrangler.toml"
    config = tomllib.loads(source.read_text())
    require(config.get("name") == "two-bot-next", "wrong_staging_config")
    staging = rollout.mapping(rollout.mapping(config.get("env")).get("staging"))
    containers = rollout.sequence(staging.get("containers"))
    require(len(containers) == 1 and containers[0].get("class_name") == CLASS
            and containers[0].get("max_instances") == 1, "wrong_staging_config")
    require(rollout.mapping(staging.get("version_metadata")).get("binding")
            == "CF_VERSION_METADATA", "missing_worker_version_binding")
    config["main"] = str((source.parent / config["main"]).resolve())
    containers[0]["image"] = image
    containers[0]["rollout_kind"] = "full_auto"
    containers[0].pop("image_vars", None)
    Path(path).write_text(json.dumps(config, indent=2) + "\n")


def wrangler_deploy(wrangler_bin, wrangler_dir):
    """Deploy closure: pinned wrangler binary, discarded output, exit code only."""

    def deploy(config_path):
        try:
            result = subprocess.run(
                [wrangler_bin, "deploy", "--config", config_path, "--env", "staging"],
                cwd=wrangler_dir, capture_output=True,
                timeout=WRANGLER_TIMEOUT_SECONDS)
        except (OSError, subprocess.SubprocessError):
            raise GateError("deploy_unavailable") from None
        return result.returncode

    return deploy


def summary(evidence, failures):
    lines = ["## Staging container backout/restore drill", "",
             f"Result: **{'PASS' if not failures else 'FAIL'}**", ""]
    for key in ("pre_worker_version", "pre_image", "pre_source_sha", "epoch_before"):
        if key in evidence:
            lines.append(f"- {key}: `{evidence[key]}`")
    backout = evidence.get("backout")
    if backout:
        lines.append(f"- backout: `{backout['source_sha']}` `{backout['image']}` "
                     f"(review `{backout['review_ref']}`, staging run `{backout['staging_run_id']}`)")
    for name in ("drill", "restore"):
        leg = evidence.get(name)
        if not leg:
            continue
        if "first_ready" not in leg:
            lines += ["", f"### {name} leg → `{leg.get('intended_image', 'unknown')}`",
                      "- incomplete; reached: " + ", ".join(key for key in
                                                            ("start", "fenced", "deployed",
                                                             "takeover") if key in leg)]
        else:
            lines += ["", f"### {name} leg → `{leg['intended_image']}`",
                      f"- fenced {leg['fenced']}, deployed {leg['deployed']}, ownership "
                      f"{leg['takeover']}, first ready {leg['first_ready']}",
                      f"- time-to-ready {leg['time_to_ready_seconds']} s (budget "
                      f"{READY_BUDGET_SECONDS} s: "
                      f"{'met' if leg['budget_met'] else 'MISSED'}); outage from fence "
                      f"{leg['outage_seconds']} s; {leg['probes']} probes, "
                      f"{leg['non_200_readyz']} non-200 readyz",
                      f"- worker `{leg['serving_worker_version']}`, intended "
                      f"`{leg['intended_image']}`, running `{leg['running_image']}`",
                      f"- session witness: "
                      f"{leg['session_witness']['status']} "
                      f"({leg['session_witness']['reason']})"]
    if failures:
        lines += ["", "Failures: " + ", ".join(f"`{item}`" for item in failures)]
    return "\n".join(lines) + "\n"


def main(argv=None, *, environ=None, make_control=ownership_control,
         client_factory=ContainerDrillClient, deploy_factory=wrangler_deploy):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--backout-source-sha", required=True)
    parser.add_argument("--backout-build-id", required=True)
    parser.add_argument("--backout-image", required=True)
    parser.add_argument("--backout-worker-version", required=True)
    parser.add_argument("--backout-rollout-id", required=True)
    parser.add_argument("--backout-review-ref", required=True)
    parser.add_argument("--backout-staging-run-id", required=True)
    parser.add_argument("--compatibility-note", required=True)
    parser.add_argument("--session-attestation", default="")
    parser.add_argument("--evidence", default="staging-container-drill-evidence.json")
    parser.add_argument("--deploy-config", default="staging-container-deploy.json")
    parser.add_argument("--wrangler-bin", default="node_modules/.bin/wrangler")
    parser.add_argument("--wrangler-dir", default=None)
    args = parser.parse_args(argv)
    env = os.environ if environ is None else environ
    try:
        url = env.get("STAGING_WORKER_URL", "").rstrip("/")
        require(re.fullmatch(r"https://two-bot-next-staging\.[a-z0-9-]+\.workers\.dev", url),
                "invalid_staging_url")
        token = env.get("OWNERSHIP_CONTROL_TOKEN", "")
        require(len(token) >= 32, "missing_ownership_token")
        run_id = env.get("GITHUB_RUN_ID", "")
        require(RUN_ID.fullmatch(run_id), "invalid_run_id")
        actor = f"github-actions:{run_id}:container-drill"
        require(ACTOR.fullmatch(actor), "invalid_actor")
        pin = validate_pin({"source_sha": args.backout_source_sha,
                            "build_id": args.backout_build_id,
                            "image": args.backout_image,
                            "worker_version": args.backout_worker_version,
                            "rollout_id": args.backout_rollout_id,
                            "review_ref": args.backout_review_ref,
                            "staging_run_id": args.backout_staging_run_id,
                            "compatibility_note": args.compatibility_note})
        attestation = validate_attestation(args.session_attestation)
        wrangler_dir = args.wrangler_dir or str(
            Path(__file__).resolve().parents[1] / "wrangler")
        client = client_factory(env.get("CLOUDFLARE_ACCOUNT_ID"),
                                env.get("CLOUDFLARE_API_TOKEN"))
        drill = Drill(client, make_control(token, url, actor), url, run_id,
                      (args.deploy_config,
                       deploy_factory(args.wrangler_bin, wrangler_dir),
                       wrangler_dir),
                      log=print)
        failures = drill.run(pin, attestation)
    except GateError as error:
        print(f"staging container drill refused before any change: {error}")
        return 1
    Path(args.evidence).write_text(json.dumps(drill.evidence, indent=2, sort_keys=True) + "\n")
    print("staging container drill evidence: " + json.dumps(drill.evidence, sort_keys=True))
    if env.get("GITHUB_STEP_SUMMARY"):
        with open(env["GITHUB_STEP_SUMMARY"], "a") as handle:
            handle.write(summary(drill.evidence, failures))
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
