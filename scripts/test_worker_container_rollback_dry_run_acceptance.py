#!/usr/bin/env python3
"""Protected offline acceptance: preview the restore plan with --dry-run.

Contract under test (scripts/worker_container_rollback.py `rollback`):
1. `rollback ... --dry-run` exits 0 on a valid receipt and prints the
   ordered restore plan to stdout: five lines in order, each starting
   with `dry-run plan:` and naming one step in fixed vocabulary:
   `fence`, `worker`, `image`, `takeover`, `gate`. The worker line
   carries the restore version UUID, the image line the restore image
   digest URL, the takeover line the restore version, and the gate line
   the restore version and SHA. When the worker already serves the
   restore version the worker line says `already serves` (or
   `image leg only`) instead of a POST, still in order with the same
   resolved version.
2. Dry-run performs zero POSTs and zero subprocess mutations: no
   `client.post` call, no control `fence`/`takeover`/`deployment-takeover`
   action, and no `subprocess.run` invoking those actions. Control
   `status` reads are allowed but not required; GETs are allowed.
3. Plan/live pre-POST resolution parity: the version, SHA and image the
   plan prints are exactly the provenance receipt members live mode
   validates before any POST for the same inputs (manual or snapshot).
4. Each invalid receipt member refuses identically in both modes: same
   exit 1, same fixed-vocabulary code (and member detail where the live
   path emits one), zero POSTs in both.
5. RED is behavioral: every dry-run assertion below fails on an exit
   code, plan-text, call-count or refusal-code comparison against live
   code paths, never on an import error or missing fixture. Control
   tests at the bottom exercise only live paths and stay GREEN on the
   current head, so a red run with green controls is a missing flag,
   not broken harness. A missing `--dry-run` flag surfaces as argparse
   SystemExit(2); the helpers convert it to an assertion failure naming
   the flag so the suite reports FAIL, not ERROR.
6. Finite-use staging prerequisites (on-demand staging, no always-on
   target): the restore target's immutable Worker-version
   `resources.bindings` must hold exactly one `plain_text` row each for
   `STAGING_RUNTIME_POLICY=bounded`, `TWO_DEPLOY_ENV=staging` and
   `CONTAINER_RELEASE=<receipt SHA>`. Dry-run reads that version (a GET)
   and refuses an absent, duplicate, secret-typed or wrong binding, or a
   pre-stamp target, with the same code, exit and zero POSTs as live mode.
   Neither mode builds the finite-use adapter or runs any
   `staging-use-pipeline.mjs` action on a refusal, and dry-run never does.

Offline only: in-memory FakeClient, temp snapshot files, recording
control stubs and subprocess mocks. No network, no credentials, no
databases. Secret-shaped canaries below are assembled at runtime and
must never appear in plan output.

Protected file: the Builder builds on this commit and must never edit
this file; test corrections go back to its author.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_worker_container_rollback_dry_run_acceptance.py' -v
"""

import io
import json
import re
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
# E402-safe by convention: every scripts/ test adds its own directory first,
# then imports the sibling module under test (no linter runs on scripts/).
import staging_rollout as base
import worker_container_rollback as rollback

ACCOUNT = "a" * 32
WORKER = "two-bot-next-staging"
APPLICATION = "two-bot-next-twobotcontainer-staging"
APP_ID = "app-1"
NAMESPACE = "ns-1"
GOOD_VERSION = "11111111-1111-4111-8111-111111111111"
BAD_VERSION = "22222222-2222-4222-8222-222222222222"
SHA = "0123456789abcdef0123456789abcdef01234567"
OTHER_SHA = "f" * 40
IMAGE_AT = "registry.cloudflare.com/acct/two-bot-next-twobotcontainer-staging@sha256:"
URL = "https://two-bot-next-staging.acct.workers.dev"
PLAN_PREFIX = "dry-run plan:"
PLAN_STEPS = ("fence", "worker", "image", "takeover", "gate")

# Canary credential values: valid shapes (>=32 chars) that must never echo.
OWNERSHIP_CANARY = "dryrun" + "-" + "ownership" + "-" + "canary" + "-" + "1234567890ab"
CF_CANARY = "dryrun" + "-" + "cf" + "-" + "canary" + "-" + "1234567890abcdef"


def good_image():
    return IMAGE_AT + "a" * 64


def bad_image():
    return IMAGE_AT + "b" * 64


def rollout_row(rollout_id, image, created="2026-10-15T00:00:00Z", status="completed",
                target_version=7):
    return {"id": rollout_id, "created_at": created, "status": status,
            "strategy": "rolling", "kind": "full_auto", "target_version": target_version,
            "current_version": target_version - 1,
            "target_configuration": {"image": image, "instance_type": "lite"},
            "current_configuration": {"image": image},
            "health": {"instances": {"active": 1, "healthy": 1, "failed": 0,
                                     "starting": 0, "scheduling": 0}},
            "progress": {"total_steps": 1, "current_step": 1, "total_instances": 1,
                         "updated_instances": 1},
            "steps": [{"status": "completed"}]}


def policy_bindings(policy="bounded", env="staging", release=SHA):
    """The finite-use binding set; None drops that row (an absent binding)."""
    rows = []
    for name, value in (("STAGING_RUNTIME_POLICY", policy), ("TWO_DEPLOY_ENV", env),
                        ("CONTAINER_RELEASE", release)):
        if value is not None:
            rows.append({"name": name, "type": "plain_text", "text": value})
    return rows


NO_RESOURCES = object()


def version_route(version=GOOD_VERSION, message=SHA, bindings=None):
    result = {"id": version}
    if bindings is not NO_RESOURCES:
        result["resources"] = {
            "bindings": policy_bindings() if bindings is None else bindings}
    if message is not None:
        result["annotations"] = {"workers/message": message}
    return lambda: {"success": True, "result": result}


class FakeClient(rollback.RollbackClient):
    """In-memory Cloudflare: routes are (method, path) -> handler."""

    def __init__(self, routes):
        super().__init__(ACCOUNT, CF_CANARY)
        self.routes = routes
        self.calls = []

    def api(self, path):
        self.calls.append(("GET", path))
        data = self.routes[("GET", path)]()
        base.require(base.mapping(data).get("success") is True and "result" in data,
                     "api_failure")
        return data["result"]

    def post(self, path, body):
        self.calls.append(("POST", path, body))
        return self.routes[("POST", path)](body)

    def request(self, url, authenticated=False):
        self.calls.append(("PROBE", url))
        return self.routes[("PROBE", url.rsplit("/", 1)[-1])]()


def control_stub(epoch=9):
    calls = []
    phase = {"current": "active"}

    def control(action, epoch_arg=None, expected_deployment=None):
        calls.append((action, epoch_arg, expected_deployment))
        if action == "status":
            return {"owner": {"phase": phase["current"], "epoch": epoch}, "running": True}
        if action == "fence":
            phase["current"] = "fenced"
        else:
            phase["current"] = "active"
        return {"owner": {"phase": phase["current"], "epoch": epoch + 1,
                          "deploymentId": expected_deployment},
                "deploymentId": expected_deployment}
    return control, calls


def base_env(**extra):
    # A complete finite-use window, so a live refusal below is never a
    # missing-window artifact.
    env = {"CLOUDFLARE_ACCOUNT_ID": ACCOUNT, "CLOUDFLARE_API_TOKEN": CF_CANARY,
           "OWNERSHIP_CONTROL_TOKEN": OWNERSHIP_CANARY, "GITHUB_RUN_ID": "700",
           "GITHUB_RUN_ATTEMPT": "1", "STAGING_USE_EXPIRES_AT": "1700001000000"}
    env.update(extra)
    return env


VERSION_PATH = f"/workers/scripts/{WORKER}/versions/{GOOD_VERSION}"


def live_routes(message=SHA, history=(BAD_VERSION, GOOD_VERSION), served_image=None,
                bindings=None):
    served_image = good_image() if served_image is None else served_image
    deployments = [{"versions": [{"version_id": v, "percentage": 100}]}
                   for v in history]
    return {
        ("GET", VERSION_PATH): version_route(message=message, bindings=bindings),
        ("GET", f"/workers/scripts/{WORKER}/deployments"): lambda: {
            "success": True, "result": {"deployments": deployments}},
        ("GET", "/containers/applications"): lambda: {
            "success": True, "result": [{"id": APP_ID, "name": APPLICATION,
                                         "durable_objects": {"namespace_id": NAMESPACE}}]},
        ("GET", f"/containers/applications/{APP_ID}/rollouts?limit=100"): lambda: {
            "success": True, "result": [rollout_row("r-prev", served_image)]},
    }


def write_snapshot(tmp, name="snapshot.json", **overrides):
    state = {"has_previous": True, "worker": WORKER, "application": APPLICATION,
             "application_id": APP_ID, "namespace_id": NAMESPACE,
             "serving_version": GOOD_VERSION, "serving_sha": SHA,
             "provenance_complete": True, "serving_rollout_id": "r-prev",
             "serving_image": good_image(), "recorded_at": 1.0}
    state.update(overrides)
    path = Path(tmp) / name
    path.write_text(json.dumps(state))
    return path


def snapshot_reread_routes(image=None):
    image = good_image() if image is None else image
    return {("GET", f"/containers/applications/{APP_ID}/rollouts/r-prev"):
            lambda: {"success": True, "result": rollout_row("r-prev", image)}}


def manual_argv(evidence, *extra):
    return ["rollback", "--variant", "staging", "--worker-url", URL,
            "--target-version", GOOD_VERSION, "--target-image", good_image(),
            "--expected-sha", SHA, "--actor", "a:b",
            "--evidence", str(evidence), *extra]


def snapshot_argv(path, evidence, *extra):
    return ["rollback", "--variant", "staging", "--worker-url", URL,
            "--snapshot", str(path), "--actor", "a:b",
            "--evidence", str(evidence), *extra]


def posts_of(client):
    return [call for call in client.calls if call[0] == "POST"]


def finite_use_recorder(record):
    """Acceptance-only stand-in for main's finite-use adapter factory: it
    records construction and every action and never runs node. The
    production adapter is not edited; only this test replaces it."""
    def factory(env, url, token, version, sha, receipt, root=None):
        record.append(("adapter", version, sha))
        return lambda action: record.append(("use", action))
    return mock.patch.object(rollback, "staging_restore_client", factory)


class DryRunCase(unittest.TestCase):
    """Helpers that turn a missing --dry-run flag into FAIL, not ERROR."""

    def run_live(self, argv, client, env=None):
        stdout = io.StringIO()
        self.live_finite_use = []
        with finite_use_recorder(self.live_finite_use), redirect_stdout(stdout):
            code = rollback.main(argv, environ=env or base_env(),
                                 client_factory=lambda _a, _t: client)
        return code, stdout.getvalue()

    def run_dry(self, argv, client, env=None):
        """Run argv with --dry-run; fail behaviorally if the flag is missing."""
        stdout, stderr = io.StringIO(), io.StringIO()
        self.dry_finite_use = []
        try:
            with (finite_use_recorder(self.dry_finite_use),
                  redirect_stdout(stdout), redirect_stderr(stderr)):
                code = rollback.main([*argv, "--dry-run"],
                                     environ=env or base_env(),
                                     client_factory=lambda _a, _t: client)
        except SystemExit as exited:
            self.fail("behavioral RED: --dry-run not recognized "
                      f"(SystemExit {exited.code}): {stderr.getvalue()[:200]}")
        return code, stdout.getvalue()

    def plan_lines(self, out):
        lines = [line for line in out.splitlines() if line.startswith(PLAN_PREFIX)]
        self.assertEqual(len(lines), 5,
                         f"expected 5 ordered plan lines, got {len(lines)}: {out[:800]}")
        for step, line in zip(PLAN_STEPS, lines):
            self.assertIn(step, line, f"plan step out of order: {lines}")
        return lines

    def assert_no_posts(self, client, out):
        self.assertEqual(posts_of(client), [],
                         f"dry-run must not POST: {posts_of(client)}; out={out[:400]}")

    def assert_no_mutations(self, control_calls, out):
        mutations = [call for call in control_calls
                     if call[0] in ("fence", "takeover", "deployment-takeover")]
        self.assertEqual(mutations, [],
                         f"dry-run must not mutate ownership: {mutations}; out={out[:400]}")
        # A preview never asserts, consumes or rebinds the finite-use window.
        self.assertEqual(getattr(self, "dry_finite_use", []), [],
                         f"dry-run must not touch the finite-use window; out={out[:400]}")

    def assert_policy_read(self, client):
        self.assertIn(("GET", VERSION_PATH), client.calls,
                      "staging restore must read the target version's bindings")

    def assert_same_refusal(self, live_out, dry_out, code_text, member=None):
        for label, out in (("live", live_out), ("dry-run", dry_out)):
            self.assertIn(code_text, out,
                          f"{label} must refuse with {code_text}: {out[:400]}")
            if member is not None:
                self.assertIn(member, out,
                              f"{label} must name {member}: {out[:400]}")

    def run_both(self, argv, routes):
        live_client, dry_client = FakeClient(routes()), FakeClient(routes())
        live_control, live_calls = control_stub()
        with mock.patch.object(rollback, "control_client",
                               lambda *a, **k: live_control):
            live_code, live_out = self.run_live(list(argv), live_client)
        # Every receipt/policy refusal precedes ownership control and the
        # finite-use adapter in live mode too: no fence, no window use.
        self.assertEqual(live_calls, [],
                         f"live refusal reached ownership control: {live_out[:400]}")
        self.assertEqual(self.live_finite_use, [],
                         f"live refusal reached the finite-use window: {live_out[:400]}")
        control, calls = control_stub()
        with mock.patch.object(rollback, "control_client",
                               lambda *a, **k: control):
            dry_code, dry_out = self.run_dry(list(argv), dry_client)
        return (live_code, live_out, live_client,
                dry_code, dry_out, dry_client, calls)


class DryRunPlanTests(DryRunCase):
    def test_manual_dry_run_prints_ordered_plan_with_resolved_ids(self):
        with tempfile.TemporaryDirectory() as tmp:
            evidence = Path(tmp) / "evidence.json"
            client = FakeClient(live_routes())
            control, calls = control_stub()
            with mock.patch.object(rollback, "control_client",
                                   lambda *a, **k: control):
                code, out = self.run_dry(manual_argv(evidence), client)
        self.assertEqual(code, 0, out)
        plan = self.plan_lines(out)
        self.assertIn(GOOD_VERSION, plan[1], "worker step must name the restore version")
        self.assertIn(good_image(), plan[2], "image step must name the restore image")
        self.assertIn(GOOD_VERSION, plan[3], "takeover step must name the restore version")
        self.assertIn(GOOD_VERSION, plan[4], "gate step must name the restore version")
        self.assertIn(SHA, plan[4], "gate step must name the receipt SHA")
        self.assertNotIn("Traceback", out)
        self.assert_no_posts(client, out)
        self.assert_no_mutations(calls, out)
        self.assert_policy_read(client)

    def test_snapshot_dry_run_prints_ordered_plan_with_resolved_ids(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write_snapshot(tmp)
            evidence = Path(tmp) / "evidence.json"
            routes = live_routes()
            routes.update(snapshot_reread_routes())
            client = FakeClient(routes)
            control, calls = control_stub()
            with mock.patch.object(rollback, "control_client",
                                   lambda *a, **k: control):
                code, out = self.run_dry(snapshot_argv(path, evidence), client)
        self.assertEqual(code, 0, out)
        plan = self.plan_lines(out)
        self.assertIn(GOOD_VERSION, plan[1])
        self.assertIn(good_image(), plan[2])
        self.assertIn(GOOD_VERSION, plan[3])
        self.assertIn(GOOD_VERSION, plan[4])
        self.assertIn(SHA, plan[4])
        self.assertNotIn("Traceback", out)
        self.assert_no_posts(client, out)
        self.assert_no_mutations(calls, out)
        self.assert_policy_read(client)

    def test_dry_run_skips_worker_step_when_already_serving(self):
        # Live run logs "worker already serves the restore version; image leg
        # only" and skips the deployments POST; the plan must say the same in
        # order instead of naming a worker POST.
        with tempfile.TemporaryDirectory() as tmp:
            evidence = Path(tmp) / "evidence.json"
            client = FakeClient(live_routes(history=(GOOD_VERSION, BAD_VERSION)))
            control, calls = control_stub()
            with mock.patch.object(rollback, "control_client",
                                   lambda *a, **k: control):
                code, out = self.run_dry(manual_argv(evidence), client)
        self.assertEqual(code, 0, out)
        plan = self.plan_lines(out)
        lowered = " ".join(plan).lower()
        self.assertTrue("already serves" in lowered or "image leg only" in lowered,
                        f"skip case must say so: {plan}")
        self.assertIn(GOOD_VERSION, plan[1])
        self.assertIn(good_image(), plan[2])
        self.assertIn(GOOD_VERSION, plan[3])
        self.assertIn(SHA, plan[4])
        self.assert_no_posts(client, out)
        self.assert_no_mutations(calls, out)

    def test_dry_run_performs_zero_subprocess_mutations(self):
        # No control_client stub here: the real client runs against a
        # recording subprocess, so a fence/takeover subprocess invocation
        # would be observed. Status reads answer success. Any finite-use
        # pipeline invocation (whatever the action) is recorded separately.
        def fake_run(command, **kwargs):
            action = command[2] if len(command) > 2 else ""
            fake_run.calls.append(action)
            if len(command) > 1 and Path(str(command[1])).name == "staging-use-pipeline.mjs":
                fake_run.finite_use.append(action)
            return rollback.subprocess.CompletedProcess(
                args=command, returncode=0,
                stdout=json.dumps({"owner": {"phase": "active", "epoch": 9}}),
                stderr="")
        fake_run.calls = []
        fake_run.finite_use = []
        with tempfile.TemporaryDirectory() as tmp:
            evidence = Path(tmp) / "evidence.json"
            client = FakeClient(live_routes())
            with mock.patch.object(rollback.subprocess, "run", side_effect=fake_run):
                code, out = self.run_dry(manual_argv(evidence), client)
            written = list(Path(tmp).iterdir())
        self.assertEqual(code, 0, out)
        self.plan_lines(out)
        self.assert_no_posts(client, out)
        mutations = [a for a in fake_run.calls
                     if a in ("fence", "takeover", "deployment-takeover",
                              "begin", "consume", "restore-check", "rebind", "finish")]
        self.assertEqual(mutations, [],
                         f"dry-run must not invoke mutating subprocess: {fake_run.calls}")
        self.assertEqual(fake_run.finite_use, [],
                         f"dry-run must not run the finite-use pipeline: {fake_run.calls}")
        self.assertEqual(self.dry_finite_use, [])
        self.assertEqual(written, [], "dry-run must write no evidence or use receipt")

    def test_dry_run_never_echoes_credentials(self):
        with tempfile.TemporaryDirectory() as tmp:
            evidence = Path(tmp) / "evidence.json"
            client = FakeClient(live_routes())
            control, _ = control_stub()
            with mock.patch.object(rollback, "control_client",
                                   lambda *a, **k: control):
                code, out = self.run_dry(manual_argv(evidence), client)
        self.assertEqual(code, 0, out)
        self.assertNotIn(OWNERSHIP_CANARY, out, "plan echoed the ownership token")
        self.assertNotIn(CF_CANARY, out, "plan echoed the Cloudflare token")
        self.assertNotIn("bearer", out.lower())


class PlanLiveParityTests(DryRunCase):
    def test_manual_plan_ids_match_live_receipt_members(self):
        # The plan's printed members are exactly the receipt live mode
        # validates: rebuilding the receipt from the plan and binding it to
        # the same live state must pass.
        with tempfile.TemporaryDirectory() as tmp:
            evidence = Path(tmp) / "evidence.json"
            routes = live_routes()
            client = FakeClient(routes)
            control, _ = control_stub()
            with mock.patch.object(rollback, "control_client",
                                   lambda *a, **k: control):
                code, out = self.run_dry(manual_argv(evidence), client)
        self.assertEqual(code, 0, out)
        self.assertIn(GOOD_VERSION, out)
        self.assertIn(SHA, out)
        self.assertIn(good_image(), out)
        receipt = rollback.ProvenanceReceipt(WORKER, APPLICATION, GOOD_VERSION,
                                             SHA, good_image())
        rollback.validate_receipt_against_live(FakeClient(live_routes()), receipt)

    def test_snapshot_plan_ids_match_recorded_receipt_members(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write_snapshot(tmp)
            evidence = Path(tmp) / "evidence.json"
            routes = live_routes()
            routes.update(snapshot_reread_routes())
            client = FakeClient(routes)
            control, _ = control_stub()
            with mock.patch.object(rollback, "control_client",
                                   lambda *a, **k: control):
                code, out = self.run_dry(snapshot_argv(path, evidence), client)
            recorded = json.loads(Path(path).read_text())
        self.assertEqual(code, 0, out)
        for member in (recorded["serving_version"], recorded["serving_sha"],
                       recorded["serving_image"]):
            self.assertIn(member, out, f"plan must carry recorded member {member[:24]}")
        receipt = rollback.ProvenanceReceipt(
            WORKER, APPLICATION, recorded["serving_version"],
            recorded["serving_sha"], recorded["serving_image"])
        check_routes = live_routes()
        check_routes.update(snapshot_reread_routes())
        rollback.validate_receipt_against_live(FakeClient(check_routes), receipt)


class InvalidParityTests(DryRunCase):
    """Each invalid receipt member refuses identically in both modes."""

    def test_manual_invalid_members_refuse_identically_in_both_modes(self):
        with tempfile.TemporaryDirectory() as tmp:
            evidence = str(Path(tmp) / "evidence.json")
            base = ["rollback", "--variant", "staging", "--worker-url", URL,
                    "--actor", "a:b", "--evidence", evidence]
            cases = [
                ("missing-sha",
                 [*base, "--target-version", GOOD_VERSION,
                  "--target-image", good_image()],
                 live_routes, "manual_rollback_needs_expected_sha", None),
                ("sha-mismatch",
                 [*base, "--target-version", GOOD_VERSION,
                  "--target-image", good_image(), "--expected-sha", OTHER_SHA],
                 live_routes, "provenance_mismatch", "member=sha"),
                ("pre-stamp",
                 [*base, "--target-version", GOOD_VERSION,
                  "--target-image", good_image(), "--expected-sha", SHA],
                 lambda: live_routes(message=None),
                 "provenance_mismatch", "member=sha"),
                ("version-never-served",
                 [*base, "--target-version", GOOD_VERSION,
                  "--target-image", good_image(), "--expected-sha", SHA],
                 lambda: live_routes(history=(BAD_VERSION,)),
                 "provenance_version_never_served", None),
                ("image-never-served",
                 [*base, "--target-version", GOOD_VERSION,
                  "--target-image", good_image(), "--expected-sha", SHA],
                 lambda: live_routes(served_image=bad_image()),
                 "provenance_image_never_served", None),
                ("bad-version-shape",
                 [*base, "--target-version", "not-a-uuid",
                  "--target-image", good_image(), "--expected-sha", SHA],
                 live_routes, "invalid_target_version", None),
                ("bad-image-shape",
                 [*base, "--target-version", GOOD_VERSION, "--target-image",
                  IMAGE_AT.replace("@sha256:", ":latest@sha256:") + "a" * 64,
                  "--expected-sha", SHA],
                 live_routes, "invalid_target_image", None),
                ("bad-expected-sha-shape",
                 [*base, "--target-version", GOOD_VERSION,
                  "--target-image", good_image(), "--expected-sha", "zzz"],
                 live_routes, "invalid_expected_sha", None),
            ]
            for name, argv, routes_fn, code_text, member in cases:
                with self.subTest(case=name):
                    (live_code, live_out, live_client,
                     dry_code, dry_out, dry_client, calls) = self.run_both(argv, routes_fn)
                    self.assertEqual(live_code, 1, live_out)
                    self.assertEqual(dry_code, 1, dry_out)
                    self.assert_same_refusal(live_out, dry_out, code_text, member)
                    self.assertEqual(posts_of(live_client), [])
                    self.assertEqual(posts_of(dry_client), [])
                    self.assert_no_mutations(calls, dry_out)

    def test_manual_non_completed_image_refuses_identically_in_both_modes(self):
        for status in ("failed", "reverted"):
            with self.subTest(status=status), tempfile.TemporaryDirectory() as tmp:
                evidence = str(Path(tmp) / "evidence.json")
                argv = ["rollback", "--variant", "staging", "--worker-url", URL,
                        "--target-version", GOOD_VERSION,
                        "--target-image", good_image(),
                        "--expected-sha", SHA, "--actor", "a:b",
                        "--evidence", evidence]

                def routes():
                    served = rollout_row("r-prev", good_image(), status=status)
                    table = live_routes()
                    table[("GET", f"/containers/applications/{APP_ID}/rollouts?limit=100")] = \
                        lambda: {"success": True, "result": [served]}
                    return table

                (live_code, live_out, live_client,
                 dry_code, dry_out, dry_client, _) = self.run_both(argv, routes)
                self.assertEqual(live_code, 1, live_out)
                self.assertEqual(dry_code, 1, dry_out)
                self.assert_same_refusal(live_out, dry_out,
                                         "provenance_image_never_served")
                self.assertEqual(posts_of(live_client), [])
                self.assertEqual(posts_of(dry_client), [])

    def test_snapshot_invalid_members_refuse_identically_in_both_modes(self):
        with tempfile.TemporaryDirectory() as tmp:
            evidence = str(Path(tmp) / "evidence.json")

            def argv_for(snapshot_path, *extra):
                return ["rollback", "--variant", "staging", "--worker-url", URL,
                        "--snapshot", str(snapshot_path), "--actor", "a:b",
                        "--evidence", evidence, *extra]

            snap_drift = write_snapshot(tmp, "drift.json")
            snap_ok = write_snapshot(tmp, "ok.json")
            snap_bad_sha = write_snapshot(tmp, "bad-sha.json", serving_sha="zzz")
            snap_no_prev = write_snapshot(tmp, "no-prev.json", has_previous=False)
            snap_pre_stamp = write_snapshot(tmp, "pre-stamp.json", serving_sha=None,
                                            provenance_complete=False)

            def conflicting_routes():
                table = live_routes()
                table.update(snapshot_reread_routes())
                return table

            def stale_routes():
                table = snapshot_reread_routes()
                table[("GET", f"/workers/scripts/{WORKER}/versions/{GOOD_VERSION}")] = \
                    version_route(message=OTHER_SHA)
                return table

            cases = [
                ("image-drift", argv_for(snap_drift),
                 lambda: snapshot_reread_routes(bad_image()),
                 "provenance_mismatch", "member=image"),
                ("conflicting-expected-sha",
                 argv_for(snap_ok, "--expected-sha", OTHER_SHA),
                 conflicting_routes,
                 "provenance_mismatch", "member=sha"),
                ("malformed-recorded-sha", argv_for(snap_bad_sha),
                 snapshot_reread_routes,
                 "invalid_snapshot_sha", None),
                ("stale-revalidation", argv_for(snap_ok),
                 stale_routes,
                 "provenance_mismatch", "member=sha"),
                ("has-no-previous", argv_for(snap_no_prev),
                 snapshot_reread_routes,
                 "snapshot_has_no_previous", None),
                # Staging has no pre-stamp/legacy restore: an unstamped
                # target cannot prove its finite-use release binding.
                ("pre-stamp-staging", argv_for(snap_pre_stamp),
                 conflicting_routes,
                 "staging_restore_identity_invalid", None),
            ]

            for name, argv, routes_fn, code_text, member in cases:
                with self.subTest(case=name):
                    (live_code, live_out, live_client,
                     dry_code, dry_out, dry_client, calls) = self.run_both(argv, routes_fn)
                    self.assertEqual(live_code, 1, live_out)
                    self.assertEqual(dry_code, 1, dry_out)
                    self.assert_same_refusal(live_out, dry_out, code_text, member)
                    self.assertEqual(posts_of(live_client), [])
                    self.assertEqual(posts_of(dry_client), [])
                    self.assert_no_mutations(calls, dry_out)


def without(name):
    return [row for row in policy_bindings() if row["name"] != name]


def policy_row(name, value, kind="plain_text"):
    return {"name": name, "type": kind, "text": value}


# Each world is an always-on, foreign or unprovable target; none may restore.
POLICY_REFUSALS = [
    ("no-resources", NO_RESOURCES),
    ("empty-bindings", []),
    ("policy-absent-always-on", without("STAGING_RUNTIME_POLICY")),
    ("policy-wrong-value", without("STAGING_RUNTIME_POLICY")
     + [policy_row("STAGING_RUNTIME_POLICY", "always-on")]),
    ("policy-duplicate-identical", policy_bindings()
     + [policy_row("STAGING_RUNTIME_POLICY", "bounded")]),
    ("policy-duplicate-conflicting", policy_bindings()
     + [policy_row("STAGING_RUNTIME_POLICY", "always-on")]),
    ("policy-secret-typed", without("STAGING_RUNTIME_POLICY")
     + [policy_row("STAGING_RUNTIME_POLICY", "bounded", "secret_text")]),
    ("env-absent", without("TWO_DEPLOY_ENV")),
    ("env-production", without("TWO_DEPLOY_ENV")
     + [policy_row("TWO_DEPLOY_ENV", "production")]),
    ("release-absent", without("CONTAINER_RELEASE")),
    ("release-other-sha", without("CONTAINER_RELEASE")
     + [policy_row("CONTAINER_RELEASE", OTHER_SHA)]),
    ("release-duplicate", policy_bindings() + [policy_row("CONTAINER_RELEASE", SHA)]),
]


class StagingPolicyParityTests(DryRunCase):
    """Finite-use binding refusals: identical in both modes, before any mutation."""

    def assert_policy_refusal(self, argv, routes):
        (live_code, live_out, live_client,
         dry_code, dry_out, dry_client, calls) = self.run_both(argv, routes)
        self.assertEqual(live_code, 1, live_out)
        self.assertEqual(dry_code, 1, dry_out)
        self.assert_same_refusal(live_out, dry_out, "staging_restore_policy_unproved")
        for out in (live_out, dry_out):
            self.assertNotIn(PLAN_PREFIX, out, f"refused target printed a plan: {out[:400]}")
            self.assertNotIn("diagnostic", out)
            self.assertNotIn("always-on", out, "binding text must not echo")
            self.assertNotIn(OWNERSHIP_CANARY, out)
            self.assertNotIn(CF_CANARY, out)
        self.assertEqual(posts_of(live_client), [])
        self.assertEqual(posts_of(dry_client), [])
        self.assert_policy_read(live_client)
        self.assert_policy_read(dry_client)
        self.assert_no_mutations(calls, dry_out)

    def test_manual_policy_refusals_match_in_both_modes(self):
        with tempfile.TemporaryDirectory() as tmp:
            evidence = Path(tmp) / "evidence.json"
            for name, bindings in POLICY_REFUSALS:
                with self.subTest(case=name):
                    self.assert_policy_refusal(
                        manual_argv(evidence), lambda b=bindings: live_routes(bindings=b))
            self.assertEqual(list(Path(tmp).iterdir()), [])

    def test_snapshot_policy_refusals_match_in_both_modes(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write_snapshot(tmp)
            evidence = Path(tmp) / "evidence.json"

            def routes(bindings):
                table = live_routes(bindings=bindings)
                table.update(snapshot_reread_routes())
                return table

            for name, bindings in POLICY_REFUSALS:
                with self.subTest(case=name):
                    self.assert_policy_refusal(snapshot_argv(path, evidence),
                                               lambda b=bindings: routes(b))
            self.assertEqual(list(Path(tmp).iterdir()), [path])


class ControlTests(unittest.TestCase):
    """Live-only controls: GREEN on the current head proves the harness."""

    def test_control_live_receipt_binds_to_live_state(self):
        receipt = rollback.ProvenanceReceipt(WORKER, APPLICATION, GOOD_VERSION,
                                             SHA, good_image())
        rollback.validate_receipt_against_live(FakeClient(live_routes()), receipt)

    def test_control_live_manual_invalid_sha_refuses_before_any_post(self):
        with tempfile.TemporaryDirectory() as tmp:
            evidence = str(Path(tmp) / "evidence.json")
            argv = ["rollback", "--variant", "staging", "--worker-url", URL,
                    "--target-version", GOOD_VERSION,
                    "--target-image", good_image(),
                    "--expected-sha", OTHER_SHA, "--actor", "a:b",
                    "--evidence", evidence]
            client = FakeClient(live_routes())
            stdout = io.StringIO()
            with redirect_stdout(stdout):
                code = rollback.main(argv, environ=base_env(),
                                     client_factory=lambda _a, _t: client)
        self.assertEqual(code, 1)
        self.assertIn("provenance_mismatch", stdout.getvalue())
        self.assertEqual(posts_of(client), [])

    def test_control_live_policy_bindings_hold(self):
        # GREEN control: the fixture's binding set is exactly what the live
        # validator accepts, and dropping the policy row is what it refuses.
        client = FakeClient(live_routes())
        rollback.validate_staging_restore_policy(client, WORKER, GOOD_VERSION, SHA)
        self.assertEqual(client.calls, [("GET", VERSION_PATH)])
        with self.assertRaises(base.GateError) as raised:
            rollback.validate_staging_restore_policy(
                FakeClient(live_routes(bindings=without("STAGING_RUNTIME_POLICY"))),
                WORKER, GOOD_VERSION, SHA)
        self.assertEqual(str(raised.exception), "staging_restore_policy_unproved")

    def test_control_receipt_shape_pins_hold(self):
        receipt = rollback.ProvenanceReceipt(WORKER, APPLICATION, GOOD_VERSION,
                                             SHA, good_image())
        receipt.validate(version=GOOD_VERSION, sha=SHA, image=good_image())
        with self.assertRaises(base.GateError) as raised:
            rollback.ProvenanceReceipt(WORKER, APPLICATION, "not-a-uuid",
                                       SHA, good_image())
        self.assertEqual(str(raised.exception), "provenance_version_invalid")


if __name__ == "__main__":
    unittest.main()
