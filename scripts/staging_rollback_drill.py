#!/usr/bin/env python3
"""Staging-only Worker-version rollback drill (docs/runbook.md "Worker-version rollback").

One dispatch runs the whole reviewed drill, so no agent ever holds the staging
ownership token: fence the singleton, move the Worker to a named version that
already served staging traffic, hand ownership to it, time the first `/readyz`
200, then restore the original version the same way.

Fail-closed rules, each pinned by scripts/test_staging_rollback_drill.py:
- The rollback is one Cloudflare deployment POST that never sends `force`, so a
  target whose secrets changed since it was deployed is refused instead of
  being auto-confirmed (Wrangler's non-interactive prompt answers yes).
- `code_update_strategy` is `immediate`. Wrangler 4.147's default is `deferred`
  with a 300 s maximum delay, which would keep the old Durable Object code
  running for up to five minutes and make any time-to-ready figure meaningless.
- A 401/403 from ownership control or Cloudflare stops the run. The restore leg
  is skipped, never retried with another credential.
- Only allowlisted ids, integers, statuses and timestamps reach the log or the
  evidence file. API, ownership and probe bodies are never echoed.
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
from urllib.error import HTTPError, URLError
from urllib.request import Request

sys.path.insert(0, str(Path(__file__).resolve().parent))
import staging_rollout as rollout  # noqa: E402

GateError = rollout.GateError
require = rollout.require
WORKER = rollout.WORKER
VERSION_ID = re.compile(rollout.UUID)
READY_BUDGET_SECONDS = 60
READY_TIMEOUT_SECONDS = 300
POLL_SECONDS = 2
CODE_UPDATE_STRATEGY = {"mode": "immediate"}
ACTOR = re.compile(r"[a-zA-Z0-9_.:@/-]{1,128}")
SAFE_CONTROL_LINE = re.compile(r"(Ownership control failed \(HTTP [0-9]{3}\)|"
                               r"Singleton is intentionally fenced or uninitialized|"
                               r"Ownership transition not confirmed|"
                               r"OWNERSHIP_CONTROL_TOKEN is missing or invalid|"
                               r"Expected the approved two-bot-next-staging workers\.dev origin)")
AUTH_STATUSES = ("401", "403")


class DrillClient(rollout.Client):
    """The gate's read-only client plus the one authenticated deployment POST."""

    def post(self, path, body):
        request = Request(
            self.base + path, data=json.dumps(body).encode(), method="POST",
            headers={"Authorization": f"Bearer {self.token}", "Content-Type": "application/json",
                     "User-Agent": rollout.USER_AGENT})
        try:
            with self.opener.open(request, timeout=20) as response:
                raw = response.read(rollout.MAX_BODY + 1)
                status = response.status
        except HTTPError as error:
            # Cloudflare's error envelope may carry configuration values: keep only
            # the HTTP status and integer error codes.
            codes = []
            try:
                envelope = json.loads(error.read(rollout.MAX_BODY))
                codes = [str(item["code"]) for item in envelope.get("errors", [])
                         if isinstance(item, dict) and type(item.get("code")) is int][:4]
            except (OSError, ValueError, AttributeError, TypeError):
                pass
            raise GateError("api_http_failure", f"status={error.code} codes={','.join(codes) or 'none'}") from None
        except (URLError, TimeoutError, OSError):
            raise GateError("api_transport_failure") from None
        require(status == 200 and len(raw) <= rollout.MAX_BODY, "api_http_failure", f"status={status}")
        data = rollout.mapping(rollout.decode(raw))
        require(data.get("success") is True and "result" in data, "api_failure")
        return data["result"]


def iso(epoch):
    return datetime.fromtimestamp(epoch, timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def deployments(client):
    """Newest-first [(deployment id, version id)] for deployments serving ONE version at 100%."""
    rows = rollout.sequence(rollout.mapping(client.api(f"/workers/scripts/{WORKER}/deployments")).get("deployments"))
    require(bool(rows), "worker_deployment_missing")
    result = []
    for row in rows:
        row = rollout.mapping(row)
        versions = rollout.sequence(row.get("versions"))
        if len(versions) == 1 and rollout.mapping(versions[0]).get("percentage") == 100:
            version = versions[0].get("version_id")
            require(isinstance(version, str) and VERSION_ID.fullmatch(version), "invalid_version_id")
            result.append(version)
        else:
            result.append(None)  # a split-traffic deployment is never a drill source or target
    return result


def serving_version(client):
    current = deployments(client)[0]
    require(current is not None, "split_traffic_deployment")
    return current


def container_snapshot(client):
    """Best-effort image digest and instance counts of the newest container rollout.

    A Worker rollback does not change the container image, so this records what
    kept serving. Never fails the drill: evidence only.
    """
    try:
        app = rollout.application(client)
        rows = rollout.rollouts(client, app["id"])
        latest = max((rollout.mapping(row) for row in rows), key=lambda row: rollout.timestamp(row.get("created_at")))
        image = rollout.mapping(latest.get("target_configuration")).get("image")
        digest = image.rsplit("@", 1)[1] if isinstance(image, str) and re.fullmatch(rollout.IMAGE, image) else None
        return {"image_digest": digest, "rollout": rollout.rollout_observation(latest)}
    except (GateError, ValueError):
        return {"unavailable": True}


def ready(status, headers, body, version):
    """200, the expected Worker version header, and every readiness component `ready`."""
    if status != 200 or headers.get("x-two-worker-version") != version:
        return False
    try:
        components = rollout.sequence(rollout.mapping(rollout.decode(body)).get("components"))
    except GateError:
        return False
    return bool(components) and all(isinstance(part, list) and len(part) == 2 and part[1] == "ready"
                                    for part in components)


class Drill:
    def __init__(self, client, control, url, run_id, *, now=time.time, sleep=time.sleep, log=print):
        self.client, self.control, self.url = client, control, url
        self.run_id, self.now, self.sleep, self.log = run_id, now, sleep, log
        self.mutated = False
        self.auth_failed = False
        self.evidence = {"worker": WORKER, "code_update_strategy": CODE_UPDATE_STRATEGY["mode"],
                         "budget_seconds": READY_BUDGET_SECONDS}

    # -- primitives ---------------------------------------------------------
    def owner(self, state):
        owner = state.get("owner")
        return owner if isinstance(owner, dict) else {}

    def stamp(self, name):
        moment = self.now()
        self.log(f"{name} {iso(moment)}")
        return moment

    def probe(self, version):
        status, headers, body = self.client.request(self.url + "/readyz")
        health, _, _ = self.client.request(self.url + "/health")
        return status, health, ready(status, headers, body, version)

    def rollback(self, version, label):
        """Deploy `version` to 100% without `force`; read the deployment back."""
        self.client.post(f"/workers/scripts/{WORKER}/deployments", {
            "strategy": "percentage", "versions": [{"version_id": version, "percentage": 100}],
            "annotations": {"workers/message": f"staging rollback drill {label} run {self.run_id}"[:120]},
            "code_update_strategy": CODE_UPDATE_STRATEGY})
        moment = self.stamp(f"{label}: rolled back")
        require(serving_version(self.client) == version, "rollback_not_active")
        return moment

    def wait_ready(self, version, started):
        """Poll /readyz + /health until ready; returns (first ready time, probe counts)."""
        deadline = started + READY_TIMEOUT_SECONDS
        probes = non_200 = 0
        while True:
            status, health, passed = self.probe(version)
            probes += 1
            if status != 200:
                non_200 += 1
            if passed and health == 200:
                return self.stamp("ready"), {"probes": probes, "non_200_readyz": non_200}
            require(self.now() < deadline, "ready_timeout", f"probes={probes} last_readyz={status}")
            self.sleep(POLL_SECONDS)

    # -- phases -------------------------------------------------------------
    def preflight(self, target):
        require(isinstance(target, str) and VERSION_ID.fullmatch(target), "invalid_target_version")
        history = deployments(self.client)
        pre = serving_version(self.client)
        require(target != pre, "target_is_serving_version")
        require(target in history, "target_never_served_traffic")
        state = self.control("status")
        require(self.owner(state).get("phase") == "active" and state.get("running") is True,
                "singleton_not_active_and_running")
        status, health, passed = self.probe(pre)
        require(passed and health == 200, "baseline_not_ready", f"readyz={status} health={health}")
        epoch = self.owner(state).get("epoch")
        self.evidence.update({"pre_version": pre, "target_version": target})
        if type(epoch) is int:
            self.evidence["epoch_before"] = epoch
        return pre

    def leg(self, name, version):
        """Fence, move the Worker to `version`, hand ownership to it, time first ready."""
        state = self.control("status")
        phase = self.owner(state).get("phase")
        require(phase in ("active", "fenced"), "owner_state_unexpected",
                "phase=" + (phase if isinstance(phase, str) and re.fullmatch(r"[a-z_]{1,20}", phase) else "unrecognized"))
        # Recorded as it happens, so a failed leg still shows how far it got.
        leg = self.evidence[name] = {"version": version}
        self.mutated = True
        started = self.stamp(f"{name}: start")
        leg["start"] = iso(started)
        if phase == "active":
            self.control("fence", self.owner(state).get("epoch"))
        fenced = self.stamp(f"{name}: fenced")
        leg["fenced"] = iso(fenced)
        if serving_version(self.client) != version:
            rolled = self.rollback(version, name)
        else:
            rolled = fenced  # restore after a refused rollback: the original Worker still serves
        leg["rolled_back"] = iso(rolled)
        self.control("deployment-takeover", release_fence=True)
        taken = self.stamp(f"{name}: ownership taken")
        leg["takeover"] = iso(taken)
        first_ready, counts = self.wait_ready(version, rolled)
        leg.update({"first_ready": iso(first_ready),
                    "time_to_ready_seconds": round(first_ready - rolled, 1),
                    "outage_seconds": round(first_ready - fenced, 1), **counts,
                    "container": container_snapshot(self.client)})
        leg["budget_met"] = leg["time_to_ready_seconds"] <= READY_BUDGET_SECONDS
        return leg

    def run(self, target):
        """Returns the list of failure codes; empty means the drill and its restore both passed."""
        failures = []
        pre = None
        try:
            pre = self.preflight(target)
            self.leg("drill", target)
        except GateError as error:
            failures.append(self.failed("drill", error))
        except Exception:  # noqa: BLE001 - no traceback: it may carry request data
            failures.append(self.failed("drill", GateError("unexpected_error")))
        if self.mutated and pre and not self.auth_failed:
            try:
                self.leg("restore", pre)
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
                                               any(f"status={s}" in (error.detail or "") for s in AUTH_STATUSES)):
            self.auth_failed = True
        detail = f" ({error.detail})" if error.detail else ""
        self.log(f"{phase} failed: {code}{detail}")
        return f"{phase}:{code}"


def ownership_control(token, url, actor, root=None):
    """Run the reviewed control client; the token only ever travels in the child's environment."""
    root = Path(root or Path(__file__).resolve().parents[1])

    def control(action, epoch=None, release_fence=False):
        command = ["node", str(root / "wrangler/scripts/ownership-control.mjs"), action]
        if epoch is not None:
            command.append(str(epoch))
        env = {**os.environ, "STAGING_WORKER_URL": url, "OWNERSHIP_CONTROL_TOKEN": token,
               "OWNERSHIP_ACTOR": actor, "OWNERSHIP_RELEASE_FENCE": "true" if release_fence else "false"}
        try:
            result = subprocess.run(command, env=env, capture_output=True, timeout=90, text=True)
        except (OSError, subprocess.SubprocessError):
            raise GateError("ownership_control_unavailable") from None
        if result.returncode != 0:
            line = (result.stderr.strip().splitlines() or [""])[0]
            match = SAFE_CONTROL_LINE.match(line)
            if match and any(f"HTTP {s}" in match.group(0) for s in AUTH_STATUSES):
                raise GateError("ownership_auth_failed")
            raise GateError("ownership_control_failed", match.group(0) if match else None)
        try:
            state = json.loads(result.stdout)
        except ValueError:
            raise GateError("ownership_control_invalid_response") from None
        return rollout.mapping(state)

    return control


def summary(evidence, failures):
    lines = ["## Staging Worker rollback drill", "", f"Result: **{'PASS' if not failures else 'FAIL'}**", ""]
    for key in ("pre_version", "target_version", "epoch_before"):
        if key in evidence:
            lines.append(f"- {key}: `{evidence[key]}`")
    for name in ("drill", "restore"):
        leg = evidence.get(name)
        if leg and "first_ready" not in leg:
            lines += ["", f"### {name} leg → `{leg['version']}`",
                      "- incomplete; reached: " + ", ".join(key for key in ("start", "fenced", "rolled_back", "takeover")
                                                            if key in leg)]
        elif leg:
            lines += ["", f"### {name} leg → `{leg['version']}`",
                      f"- fenced {leg['fenced']}, rolled back {leg['rolled_back']}, ownership {leg['takeover']}, "
                      f"first ready {leg['first_ready']}",
                      f"- time-to-ready {leg['time_to_ready_seconds']} s (budget {READY_BUDGET_SECONDS} s: "
                      f"{'met' if leg['budget_met'] else 'MISSED'}); outage from fence {leg['outage_seconds']} s; "
                      f"{leg['probes']} probes, {leg['non_200_readyz']} non-200 readyz"]
            container = leg.get("container") or {}
            if container.get("image_digest"):
                lines.append(f"- container image `{container['image_digest']}`, {container.get('rollout', 'rollout unknown')}")
    if failures:
        lines += ["", "Failures: " + ", ".join(f"`{item}`" for item in failures)]
    return "\n".join(lines) + "\n"


def main(argv=None, *, environ=None, make_control=ownership_control, client_factory=DrillClient):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target-version", required=True)
    parser.add_argument("--evidence", default="staging-rollback-drill-evidence.json")
    args = parser.parse_args(argv)
    env = os.environ if environ is None else environ
    try:
        url = env.get("STAGING_WORKER_URL", "").rstrip("/")
        require(re.fullmatch(r"https://two-bot-next-staging\.[a-z0-9-]+\.workers\.dev", url), "invalid_staging_url")
        token = env.get("OWNERSHIP_CONTROL_TOKEN", "")
        require(len(token) >= 32, "missing_ownership_token")
        run_id = env.get("GITHUB_RUN_ID", "")
        require(re.fullmatch(r"[0-9]{1,20}", run_id), "invalid_run_id")
        actor = f"github-actions:{run_id}:rollback-drill"
        require(ACTOR.fullmatch(actor), "invalid_actor")
        client = client_factory(env.get("CLOUDFLARE_ACCOUNT_ID"), env.get("CLOUDFLARE_API_TOKEN"))
        drill = Drill(client, make_control(token, url, actor), url, run_id, log=print)
        failures = drill.run(args.target_version)
    except GateError as error:
        print(f"staging rollback drill refused before any change: {error}")
        return 1
    Path(args.evidence).write_text(json.dumps(drill.evidence, indent=2, sort_keys=True) + "\n")
    print("staging rollback drill evidence: " + json.dumps(drill.evidence, sort_keys=True))
    if env.get("GITHUB_STEP_SUMMARY"):
        with open(env["GITHUB_STEP_SUMMARY"], "a") as handle:
            handle.write(summary(drill.evidence, failures))
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
