"""Offline staging rollback drill fixtures; stdlib only, no Cloudflare, ownership or waits."""

import importlib.util
import io
import json
import os
import re
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import MagicMock, patch
from urllib.error import HTTPError, URLError

spec = importlib.util.spec_from_file_location(
    "staging_rollback_drill", Path(__file__).with_name("staging_rollback_drill.py"))
drill = importlib.util.module_from_spec(spec)
spec.loader.exec_module(drill)
GateError = drill.GateError

PRE = "11111111-1111-4111-8111-111111111111"
TARGET = "22222222-2222-4222-8222-222222222222"
OLDER = "33333333-3333-4333-8333-333333333333"
NEVER = "44444444-4444-4444-8444-444444444444"
URL = "https://two-bot-next-staging.offline-fixture.workers.dev"
ACCOUNT = "b" * 32
SENTINEL = "RAW_SECRET_SENTINEL_never_disclose_ownership_token"
DEPLOYMENTS_PATH = "/workers/scripts/two-bot-next-staging/deployments"


class World:
    """One staging singleton: Worker deployments, ownership record, readiness."""

    def __init__(self, history=(PRE, TARGET, OLDER), phase="active", epoch=7, ready_polls=2):
        self.history = list(history)  # newest first
        self.phase, self.epoch, self.running = phase, epoch, phase == "active"
        self.ready_polls = ready_polls
        self.polls_left = 0
        self.t = 1_800_000_000.0
        self.calls = []
        self.posts = []
        self.reject_post = None
        self.fail_control = {}  # (action, nth call) -> error to raise
        self.never_ready = set()  # versions whose Worker never reports ready
        self.events = []  # ordered API and ownership calls for the transport fixtures

    def tick(self, seconds):
        self.t += seconds


class FakeClient:
    def __init__(self, world):
        self.world = world

    def api(self, path):
        if path == DEPLOYMENTS_PATH:
            rows = []
            for version in self.world.history:
                versions = [{"version_id": version, "percentage": 100}] if version else \
                    [{"version_id": PRE, "percentage": 50}, {"version_id": TARGET, "percentage": 50}]
                rows.append({"id": "d", "versions": versions})
            return {"deployments": rows}
        raise GateError("api_http_failure")  # container evidence is best effort

    def post(self, path, body):
        world = self.world
        world.tick(1)
        world.posts.append((path, body))
        if world.reject_post:
            raise world.reject_post
        version = body["versions"][0]["version_id"]
        world.history.insert(0, version)
        return {"id": "new"}

    def request(self, url):
        world = self.world
        version = world.history[0]
        if url.endswith("/health"):
            return (200 if world.phase == "active" else 503), {}, b""
        up = world.phase == "active" and world.running and version not in world.never_ready
        if up and world.polls_left > 0:
            world.polls_left -= 1
            up = False
        body = json.dumps({"components": [["process", "ready"], ["gateway", "ready" if up else "starting"]]})
        return (200 if up else 503), {"x-two-worker-version": version}, body.encode()


class WorldOpener:
    """Exercise the real authenticated client against the offline singleton."""

    def __init__(self, world, refusal, status):
        self.world, self.refusal, self.status = world, refusal, status
        self.counts = {}
        self.refused_at = None

    def open(self, request, timeout):
        path = request.full_url.removeprefix(f"https://api.cloudflare.com/client/v4/accounts/{ACCOUNT}")
        method = request.get_method()
        self.world.events.append((method, path))
        key = (method, path)
        self.counts[key] = self.counts.get(key, 0) + 1
        if (path, self.counts[key]) == self.refusal and method == "GET":
            self.refused_at = len(self.world.events)
            raise HTTPError(request.full_url + SENTINEL, self.status, SENTINEL,
                            {"x-secret": SENTINEL}, io.BytesIO(SENTINEL.encode()))
        fake = FakeClient(self.world)
        if method == "POST":
            result = fake.post(path, json.loads(request.data))
        elif path == "/containers/applications":
            result = [{"id": "app1", "name": drill.rollout.APPLICATION,
                       "durable_objects": {"namespace_id": "n1"}}]
        elif path.startswith("/containers/applications/app1/rollouts"):
            result = [{"created_at": "2026-10-04T22:00:00Z", "status": "completed",
                       "target_configuration": {}, "health": {"instances": {}}}]
        else:
            result = fake.api(path)
        response = MagicMock()
        response.__enter__.return_value = response
        response.status = 200
        response.headers = {}
        response.read.return_value = json.dumps({"success": True, "result": result}).encode()
        return response


def make_control(world):
    def control(action, epoch=None, release_fence=False):
        world.tick(1)
        count = sum(1 for call in world.calls if call[0] == action) + 1
        world.calls.append((action, epoch, release_fence))
        world.events.append(("control", action))
        failure = world.fail_control.get((action, count))
        if failure:
            raise failure
        if action == "status":
            return {"owner": {"phase": world.phase, "epoch": world.epoch}, "running": world.running,
                    "deploymentId": "x"}
        if action == "fence":
            if world.phase != "active" or epoch != world.epoch:
                raise GateError("ownership_control_failed")
            world.phase, world.epoch, world.running = "fenced", world.epoch + 1, False
            return {"owner": {"phase": "fenced", "epoch": world.epoch}, "running": False}
        if action == "deployment-takeover":
            if world.phase != "active" and not release_fence:
                raise GateError("ownership_control_failed")
            world.phase, world.epoch, world.running = "active", world.epoch + 1, True
            world.polls_left = world.ready_polls
            return {"owner": {"phase": "active", "epoch": world.epoch}, "running": True}
        raise AssertionError(action)

    return control


def run_drill(world, target=TARGET, logs=None):
    logs = [] if logs is None else logs
    instance = drill.Drill(FakeClient(world), make_control(world), URL, "123456", now=lambda: world.t,
                           sleep=world.tick, log=logs.append)
    return instance, instance.run(target), logs


class ReadyTests(unittest.TestCase):
    BODY = json.dumps({"components": [["process", "ready"], ["gateway", "ready"]]}).encode()

    def test_requires_200_version_header_and_every_component_ready(self):
        header = {"x-two-worker-version": PRE}
        self.assertTrue(drill.ready(200, header, self.BODY, PRE))
        self.assertFalse(drill.ready(503, header, self.BODY, PRE))
        self.assertFalse(drill.ready(200, {"x-two-worker-version": TARGET}, self.BODY, PRE))
        self.assertFalse(drill.ready(200, {}, self.BODY, PRE))
        gateway_down = json.dumps({"components": [["process", "ready"], ["gateway", "starting"]]}).encode()
        self.assertFalse(drill.ready(200, header, gateway_down, PRE))
        self.assertFalse(drill.ready(200, header, b'{"components": []}', PRE))
        self.assertFalse(drill.ready(200, header, b"not json", PRE))

    def test_split_traffic_deployment_is_never_a_source_or_target(self):
        world = World(history=(None, PRE))
        with self.assertRaises(GateError) as caught:
            drill.serving_version(FakeClient(world))
        self.assertEqual(str(caught.exception), "split_traffic_deployment")
        self.assertEqual(drill.deployments(FakeClient(world)), [None, PRE])


class HappyPathTests(unittest.TestCase):
    def test_fence_rollback_takeover_then_restore_in_order(self):
        world = World()
        instance, failures, logs = run_drill(world)
        self.assertEqual(failures, [])
        self.assertEqual([call[0] for call in world.calls],
                         ["status", "status", "fence", "deployment-takeover",   # preflight, drill leg
                          "status", "fence", "deployment-takeover"])            # restore leg
        # Fence takes the epoch just read; every takeover releases the fence explicitly.
        self.assertEqual(world.calls[2][1], 7)
        self.assertEqual(world.calls[5][1], 9)
        self.assertTrue(all(call[2] for call in world.calls if call[0] == "deployment-takeover"))
        self.assertEqual([body["versions"][0]["version_id"] for _, body in world.posts], [TARGET, PRE])
        self.assertEqual(world.history[0], PRE)
        self.assertEqual((world.phase, world.running), ("active", True))

    def test_rollback_is_never_forced_and_code_update_is_immediate(self):
        world = World()
        run_drill(world)
        for path, body in world.posts:
            self.assertEqual(path, DEPLOYMENTS_PATH)  # no ?force=true query: changed secrets must refuse
            self.assertEqual(body["strategy"], "percentage")
            self.assertEqual(body["versions"][0]["percentage"], 100)
            self.assertEqual(len(body["versions"]), 1)
            self.assertEqual(body["code_update_strategy"], {"mode": "immediate"})
            self.assertLessEqual(len(body["annotations"]["workers/message"]), 120)

    def test_times_ready_from_rollback_and_reports_outage_from_fence(self):
        world = World(ready_polls=2)
        instance, failures, _ = run_drill(world)
        leg = instance.evidence["drill"]
        # takeover 1 s after rollback, then two 2 s not-ready polls before the first ready one.
        self.assertEqual(leg["time_to_ready_seconds"], 5.0)
        self.assertEqual(leg["outage_seconds"], 6.0)  # stamped once the fence call returned: POST 1 s + 5 s
        self.assertEqual(leg["probes"], 3)
        self.assertEqual(leg["non_200_readyz"], 2)
        self.assertTrue(leg["budget_met"])
        self.assertEqual(instance.evidence["pre_version"], PRE)
        self.assertEqual(instance.evidence["target_version"], TARGET)
        self.assertEqual(instance.evidence["epoch_before"], 7)
        self.assertEqual(instance.evidence["restore"]["version"], PRE)

    def test_budget_miss_is_reported_not_failed(self):
        world = World(ready_polls=40)
        instance, failures, _ = run_drill(world)
        self.assertEqual(failures, [])
        self.assertFalse(instance.evidence["drill"]["budget_met"])
        self.assertGreater(instance.evidence["drill"]["time_to_ready_seconds"], drill.READY_BUDGET_SECONDS)


class RefusalTests(unittest.TestCase):
    def assert_refused_without_change(self, world, target, code):
        instance, failures, _ = run_drill(world, target)
        self.assertEqual(failures, [f"drill:{code}"])
        self.assertEqual(world.posts, [])
        self.assertNotIn("fence", [call[0] for call in world.calls])
        self.assertFalse(instance.mutated)

    def test_target_must_be_a_uuid(self):
        self.assert_refused_without_change(World(), "latest", "invalid_target_version")
        self.assert_refused_without_change(World(), TARGET + " --force", "invalid_target_version")

    def test_target_is_never_the_serving_version(self):
        self.assert_refused_without_change(World(), PRE, "target_is_serving_version")

    def test_target_must_have_served_traffic(self):
        self.assert_refused_without_change(World(), NEVER, "target_never_served_traffic")

    def test_singleton_must_be_active_and_running(self):
        self.assert_refused_without_change(World(phase="fenced"), TARGET, "singleton_not_active_and_running")

    def test_baseline_must_be_ready_on_the_serving_version(self):
        world = World(ready_polls=0)
        world.never_ready.add(PRE)
        self.assert_refused_without_change(world, TARGET, "baseline_not_ready")


class FailureRecoveryTests(unittest.TestCase):
    def test_refused_rollback_still_releases_the_fence_on_the_original_version(self):
        world = World()
        world.reject_post = GateError("api_http_failure", "status=409 codes=10220")
        instance, failures, _ = run_drill(world)
        self.assertEqual(failures, ["drill:api_http_failure"])
        # Restore: owner is fenced, original Worker still serves, so only the takeover runs.
        self.assertEqual([call[0] for call in world.calls][-2:], ["status", "deployment-takeover"])
        self.assertEqual(world.posts, [(DEPLOYMENTS_PATH, world.posts[0][1])])  # no second POST
        self.assertEqual((world.phase, world.running, world.history[0]), ("active", True, PRE))
        self.assertEqual(instance.evidence["restore"]["version"], PRE)

    def test_target_that_never_becomes_ready_is_rolled_forward(self):
        world = World()
        world.never_ready.add(TARGET)
        instance, failures, logs = run_drill(world)
        self.assertEqual(len(failures), 1)
        self.assertTrue(failures[0].startswith("drill:ready_timeout"))
        self.assertEqual([body["versions"][0]["version_id"] for _, body in world.posts], [TARGET, PRE])
        self.assertEqual((world.phase, world.running, world.history[0]), ("active", True, PRE))

    def test_authentication_failure_stops_without_a_restore_attempt(self):
        world = World()
        world.fail_control[("deployment-takeover", 1)] = GateError("ownership_auth_failed")
        instance, failures, logs = run_drill(world)
        self.assertEqual(failures, ["drill:ownership_auth_failed", "restore_skipped_after_auth_failure"])
        self.assertEqual([call[0] for call in world.calls].count("deployment-takeover"), 1)
        self.assertEqual(len(world.posts), 1)  # no rollback of the rollback with a rejected credential
        self.assertTrue(any("never substitute credentials" in line for line in logs))

    def test_cloudflare_authentication_failure_also_skips_the_restore(self):
        world = World()
        world.reject_post = GateError("api_http_failure", "status=403 codes=10000")
        instance, failures, _ = run_drill(world)
        self.assertEqual(failures, ["drill:api_http_failure", "restore_skipped_after_auth_failure"])
        self.assertEqual(len(world.posts), 1)

    def test_authenticated_get_refusal_stops_every_later_control_and_deployment(self):
        # Deployment reads before/after each POST, plus both best-effort snapshot reads.
        cases = [
            ("drill", DEPLOYMENTS_PATH, 3, 0, "fenced"),
            ("drill", DEPLOYMENTS_PATH, 4, 1, "fenced"),
            ("restore", DEPLOYMENTS_PATH, 5, 1, "fenced"),
            ("restore", DEPLOYMENTS_PATH, 6, 2, "fenced"),
            ("drill", "/containers/applications", 1, 1, "active"),
            ("restore", "/containers/applications", 2, 2, "active"),
            ("drill", "/containers/applications/app1/rollouts?limit=100", 1, 1, "active"),
            ("restore", "/containers/applications/app1/rollouts?limit=100", 2, 2, "active"),
        ]
        for status in (401, 403):
            for phase, path, nth, posts, owner_phase in cases:
                with self.subTest(status=status, phase=phase, path=path, nth=nth):
                    world, logs = World(), []
                    client = drill.DrillClient(ACCOUNT, SENTINEL)
                    opener = client.opener = WorldOpener(world, (path, nth), status)
                    # Keep real Cloudflare GET/POST handling, but no live readiness probes.
                    authenticated_request = client.request

                    def request(url, authenticated=False):
                        if authenticated:
                            return authenticated_request(url, authenticated=True)
                        return FakeClient(world).request(url)

                    with patch.object(client, "request", side_effect=request):
                        instance = drill.Drill(client, make_control(world), URL, "123456",
                                               now=lambda: world.t, sleep=world.tick, log=logs.append)
                        failures = instance.run(TARGET)
                    expected = [f"{phase}:api_http_failure"]
                    if phase == "drill":
                        expected.append("restore_skipped_after_auth_failure")
                    self.assertEqual(failures, expected)
                    self.assertTrue(instance.auth_failed)
                    self.assertIsNotNone(opener.refused_at)
                    self.assertEqual(world.events[opener.refused_at:], [])
                    self.assertEqual(len(world.posts), posts)
                    self.assertEqual(world.phase, owner_phase)
                    self.assertIn(f"status={status}", "\n".join(logs))
                    for text in ("\n".join(logs), json.dumps(instance.evidence),
                                 drill.summary(instance.evidence, failures)):
                        self.assertNotIn(SENTINEL, text)

    def test_unexpected_exception_still_attempts_the_restore_without_a_traceback(self):
        world = World()
        world.fail_control[("deployment-takeover", 1)] = KeyError(SENTINEL)
        instance, failures, logs = run_drill(world)
        self.assertEqual(failures, ["drill:unexpected_error"])
        self.assertNotIn(SENTINEL, "\n".join(logs))
        self.assertEqual((world.phase, world.running, world.history[0]), ("active", True, PRE))


class EvidenceTests(unittest.TestCase):
    def test_failed_leg_records_how_far_it_got(self):
        world = World()
        world.reject_post = GateError("api_http_failure", "status=409 codes=10220")
        instance, failures, _ = run_drill(world)
        leg = instance.evidence["drill"]
        self.assertEqual(set(leg), {"version", "start", "fenced"})  # fenced, then the rollback was refused
        text = drill.summary(instance.evidence, failures)
        self.assertIn("incomplete; reached: start, fenced", text)
        self.assertIn("FAIL", text)

    def test_unrecognised_owner_phase_is_never_echoed(self):
        world = World()
        control = make_control(world)

        def hostile(action, epoch=None, release_fence=False):
            state = control(action, epoch, release_fence)
            if action == "status" and world.calls.count(("status", None, False)) >= 2:
                state["owner"]["phase"] = f"weird {SENTINEL}"
            return state

        instance = drill.Drill(FakeClient(world), hostile, URL, "1", now=lambda: world.t, sleep=world.tick,
                               log=lambda line: logs.append(line))
        logs = []
        failures = instance.run(TARGET)
        self.assertEqual(failures, ["drill:owner_state_unexpected"])
        self.assertNotIn(SENTINEL, "\n".join(logs))
        self.assertIn("phase=unrecognized", "\n".join(logs))

    def test_container_snapshot_is_best_effort_and_allowlisted(self):
        image = f"registry.cloudflare.com/{ACCOUNT}/two-bot-next-twobotcontainer-staging@sha256:{'d' * 64}"
        row = {"id": "r1", "created_at": "2026-10-04T22:00:00Z", "target_configuration": {"image": image},
               "status": "completed",
               "health": {"instances": {"active": 1, "healthy": 1, "failed": 0, "starting": 0, "scheduling": 0}}}

        class Client:
            def api(self, path):
                if path == "/containers/applications":
                    return [{"id": "app1", "name": "two-bot-next-twobotcontainer-staging",
                             "durable_objects": {"namespace_id": "n1"}}]
                return [row, {**row, "id": "r0", "created_at": "2026-10-04T21:00:00Z",
                              "target_configuration": {"image": SENTINEL}}]

        snapshot = drill.container_snapshot(Client())
        self.assertEqual(snapshot["image_digest"], "sha256:" + "d" * 64)
        self.assertEqual(snapshot["rollout"], "rollout=completed instances=active:1,healthy:1,failed:0,starting:0,scheduling:0")
        self.assertEqual(drill.container_snapshot(FakeClient(World())), {"unavailable": True})


class ClientAndControlTests(unittest.TestCase):
    def client(self):
        return drill.DrillClient(ACCOUNT, SENTINEL)

    def test_get_auth_error_keeps_status_but_never_reads_or_echoes_upstream_data(self):
        for status in (401, 403):
            with self.subTest(status=status):
                client = self.client()
                body = MagicMock()
                error = HTTPError("https://api.cloudflare.com/" + SENTINEL, status, SENTINEL,
                                  {"x-secret": SENTINEL}, body)
                client.opener = MagicMock()
                client.opener.open.side_effect = error
                with self.assertRaises(GateError) as caught:
                    client.api(DEPLOYMENTS_PATH)
                self.assertEqual(str(caught.exception), "api_http_failure")
                self.assertEqual(caught.exception.detail, f"status={status}")
                body.read.assert_not_called()
                self.assertNotIn(SENTINEL, repr(caught.exception.detail))

    def test_get_diagnostics_do_not_change_other_rollout_clients_or_probes(self):
        for status in (401, 403):
            for client_type in (drill.DrillClient, drill.rollout.Client):
                with self.subTest(status=status, client=client_type.__name__):
                    client = client_type(ACCOUNT, SENTINEL)
                    client.opener = MagicMock()
                    client.opener.open.side_effect = HTTPError(URL, status, SENTINEL, {}, io.BytesIO(b""))
                    self.assertEqual(client.request(URL + "/readyz"), (status, {}, b""))
                    if client_type is drill.rollout.Client:
                        with self.assertRaises(GateError) as caught:
                            client.api(DEPLOYMENTS_PATH)
                        self.assertIsNone(caught.exception.detail)

    def test_authenticated_get_preserves_transport_and_redirect_refusals(self):
        for error, code in ((URLError(SENTINEL), "api_transport_failure"),
                            (TimeoutError(SENTINEL), "api_transport_failure"),
                            (GateError("unexpected_redirect"), "unexpected_redirect")):
            with self.subTest(code=code):
                client = self.client()
                client.opener = MagicMock()
                client.opener.open.side_effect = error
                with self.assertRaises(GateError) as caught:
                    client.api(DEPLOYMENTS_PATH)
                self.assertEqual(str(caught.exception), code)
                self.assertIsNone(caught.exception.detail)

    def test_authenticated_get_keeps_deadline_headers_and_body_limit(self):
        client = self.client()
        client.deadline = 12
        response = MagicMock()
        response.__enter__.return_value = response
        response.status = 200
        response.headers = {}
        response.read.return_value = b'{"success": true, "result": {}}'
        client.opener = MagicMock()
        client.opener.open.return_value = response
        with patch.object(drill.time, "monotonic", return_value=10):
            self.assertEqual(client.api(DEPLOYMENTS_PATH), {})
        request = client.opener.open.call_args.args[0]
        self.assertEqual(request.get_header("Authorization"), f"Bearer {SENTINEL}")
        self.assertEqual(request.get_header("User-agent"), drill.rollout.USER_AGENT)
        self.assertEqual(request.get_header("Cache-control"), "no-cache")
        self.assertEqual(client.opener.open.call_args.kwargs["timeout"], 2)
        response.read.assert_called_once_with(drill.rollout.MAX_BODY + 1)
        response.read.return_value = b"x" * (drill.rollout.MAX_BODY + 1)
        with patch.object(drill.time, "monotonic", return_value=10), self.assertRaises(GateError) as caught:
            client.api(DEPLOYMENTS_PATH)
        self.assertEqual(str(caught.exception), "response_too_large")
        client.opener.reset_mock()
        with patch.object(drill.time, "monotonic", return_value=12), self.assertRaises(GateError) as caught:
            client.api(DEPLOYMENTS_PATH)
        self.assertEqual(str(caught.exception), "rollout_timeout")
        client.opener.open.assert_not_called()

    def test_post_error_keeps_only_status_and_integer_codes(self):
        client = self.client()
        envelope = json.dumps({"errors": [{"code": 10220, "message": SENTINEL},
                                          {"code": "text", "message": SENTINEL}]}).encode()
        error = HTTPError("https://api.cloudflare.com", 409, "Conflict", {}, io.BytesIO(envelope))
        client.opener = MagicMock()
        client.opener.open.side_effect = error
        with self.assertRaises(GateError) as caught:
            client.post("/workers/scripts/two-bot-next-staging/deployments", {})
        self.assertEqual(str(caught.exception), "api_http_failure")
        self.assertEqual(caught.exception.detail, "status=409 codes=10220")
        self.assertNotIn(SENTINEL, repr(caught.exception.detail))

    def test_post_sends_bearer_json_to_the_account_path_only(self):
        client = self.client()
        response = MagicMock()
        response.__enter__.return_value = response
        response.status = 200
        response.read.return_value = b'{"success": true, "result": {"id": "x"}}'
        client.opener = MagicMock()
        client.opener.open.return_value = response
        self.assertEqual(client.post("/workers/scripts/two-bot-next-staging/deployments", {"a": 1}), {"id": "x"})
        request = client.opener.open.call_args.args[0]
        self.assertEqual(request.full_url, f"https://api.cloudflare.com/client/v4/accounts/{ACCOUNT}"
                                           "/workers/scripts/two-bot-next-staging/deployments")
        self.assertEqual(request.get_method(), "POST")
        self.assertEqual(request.get_header("Authorization"), f"Bearer {SENTINEL}")

    def control(self, completed):
        control = drill.ownership_control(SENTINEL, URL, "github-actions:1:rollback-drill", root="/repo")
        with patch.object(drill.subprocess, "run", return_value=completed) as run:
            return control, run

    def test_token_travels_only_in_the_child_environment(self):
        done = subprocess.CompletedProcess([], 0, stdout=json.dumps({"owner": {"phase": "active", "epoch": 1}}),
                                           stderr="")
        control = drill.ownership_control(SENTINEL, URL, "github-actions:1:rollback-drill", root="/repo")
        with patch.object(drill.subprocess, "run", return_value=done) as run:
            state = control("fence", 7)
        self.assertEqual(state["owner"]["epoch"], 1)
        command = run.call_args.args[0]
        self.assertEqual(command[-2:], ["fence", "7"])
        self.assertNotIn(SENTINEL, " ".join(command))
        env = run.call_args.kwargs["env"]
        self.assertEqual(env["OWNERSHIP_CONTROL_TOKEN"], SENTINEL)
        self.assertEqual(env["OWNERSHIP_RELEASE_FENCE"], "false")
        with patch.object(drill.subprocess, "run", return_value=done) as run:
            control("deployment-takeover", release_fence=True)
        self.assertEqual(run.call_args.kwargs["env"]["OWNERSHIP_RELEASE_FENCE"], "true")

    def test_child_environment_keeps_only_path_and_explicit_ownership_inputs(self):
        inherited = {"PATH": "/runtime/bin:/usr/bin", "CLOUDFLARE_API_TOKEN": SENTINEL,
                     "GH_TOKEN": SENTINEL, "DATABASE_URL": SENTINEL, "UNRELATED_SECRET": SENTINEL,
                     "NODE_OPTIONS": SENTINEL, "NODE_PATH": SENTINEL, "LD_PRELOAD": SENTINEL,
                     "HTTPS_PROXY": SENTINEL, "HOME": SENTINEL, "STAGING_WORKER_URL": SENTINEL,
                     "OWNERSHIP_CONTROL_TOKEN": "wrong", "OWNERSHIP_ACTOR": "wrong",
                     "OWNERSHIP_RELEASE_FENCE": "true"}
        done = subprocess.CompletedProcess([], 0, stdout='{"configured": true}', stderr="")
        actor = "github-actions:1:rollback-drill"
        control = drill.ownership_control(SENTINEL, URL, actor, root="/repo")
        for runtime in ({"PATH": inherited["PATH"]}, {}):
            with self.subTest(path_present=bool(runtime)):
                parent = {key: value for key, value in inherited.items() if key != "PATH"}
                with patch.dict(os.environ, {**parent, **runtime}, clear=True), \
                        patch.object(drill.subprocess, "run", return_value=done) as run:
                    self.assertEqual(control("preflight"), {"configured": True})
                self.assertEqual(run.call_args.kwargs["env"], {
                    "PATH": runtime.get("PATH", os.defpath), "STAGING_WORKER_URL": URL,
                    "OWNERSHIP_CONTROL_TOKEN": SENTINEL, "OWNERSHIP_ACTOR": actor,
                    "OWNERSHIP_RELEASE_FENCE": "false"})
                self.assertNotIn(SENTINEL, " ".join(run.call_args.args[0]))

    @unittest.skipUnless(shutil.which("node"), "Node runtime not installed")
    def test_minimal_child_environment_runs_node_preflight_without_network(self):
        with patch.dict(os.environ, {"NODE_OPTIONS": "--invalid-synthetic-node-option",
                                     "CLOUDFLARE_API_TOKEN": SENTINEL, "UNRELATED_SECRET": SENTINEL}):
            control = drill.ownership_control(SENTINEL, URL, "github-actions:1:rollback-drill")
            self.assertEqual(control("preflight"), {"configured": True})

    def test_control_failures_echo_only_known_fixed_lines(self):
        control = drill.ownership_control(SENTINEL, URL, "github-actions:1:rollback-drill", root="/repo")
        cases = [
            ("Ownership control failed (HTTP 401); stop, do not change credentials", "ownership_auth_failed", None),
            ("Ownership control failed (HTTP 403); stop, do not change credentials", "ownership_auth_failed", None),
            ("Ownership control failed (HTTP 503); stop, do not change credentials", "ownership_control_failed",
             "Ownership control failed (HTTP 503)"),
            ("Ownership control failed (HTTP 503) reason=deployment_mismatch attempts=12 elapsed=121s; "
             "stop, do not change credentials", "ownership_control_failed", "Ownership control failed (HTTP 503)"),
            ("Singleton is intentionally fenced or uninitialized; explicit staging release required; "
             "earlier takeover refusal HTTP 503 reason=shutdown_unconfirmed attempts=2 elapsed=0s",
             "ownership_control_failed", "Singleton is intentionally fenced or uninitialized"),
            ("Ownership control failed; earlier takeover refusal HTTP 503 reason=deployment_mismatch "
             "attempts=2 elapsed=0s; stop, do not change credentials", "ownership_control_failed", None),
            (f"boom {SENTINEL}", "ownership_control_failed", None),
        ]
        for line, code, detail in cases:
            with self.subTest(line=line[:30]):
                failed = subprocess.CompletedProcess([], 1, stdout="", stderr=line + "\n")
                with patch.object(drill.subprocess, "run", return_value=failed):
                    with self.assertRaises(GateError) as caught:
                        control("status")
                self.assertEqual(str(caught.exception), code)
                self.assertEqual(caught.exception.detail, detail)
                self.assertNotIn(SENTINEL, repr(caught.exception.detail))


class MainTests(unittest.TestCase):
    def environ(self, **changes):
        env = {"STAGING_WORKER_URL": URL, "OWNERSHIP_CONTROL_TOKEN": SENTINEL, "GITHUB_RUN_ID": "123456",
               "CLOUDFLARE_ACCOUNT_ID": ACCOUNT, "CLOUDFLARE_API_TOKEN": "t" * 40}
        env.update(changes)
        return env

    def invoke(self, env, world=None, args=None):
        world = world or World()
        with tempfile.TemporaryDirectory() as directory:
            evidence = Path(directory) / "evidence.json"
            summary = Path(directory) / "summary.md"
            env = {**env, "GITHUB_STEP_SUMMARY": str(summary)}
            output = io.StringIO()
            with patch("builtins.print", lambda *a, **k: output.write(" ".join(map(str, a)) + "\n")), \
                    patch.object(drill, "POLL_SECONDS", 0):
                code = drill.main(args or ["--target-version", TARGET, "--evidence", str(evidence)], environ=env,
                                  make_control=lambda token, url, actor: make_control(world),
                                  client_factory=lambda account, token: FakeClient(world))
            return (code, output.getvalue(), evidence.read_text() if evidence.exists() else None,
                    summary.read_text() if summary.exists() else None)

    def test_success_writes_evidence_and_summary_without_secrets(self):
        code, output, evidence, summary = self.invoke(self.environ())
        self.assertEqual(code, 0)
        data = json.loads(evidence)
        self.assertEqual(data["failures"], [])
        self.assertIn("PASS", summary)
        for text in (output, evidence, summary):
            self.assertNotIn(SENTINEL, text)

    def test_failure_exits_nonzero_but_still_writes_evidence(self):
        world = World()
        world.reject_post = GateError("api_http_failure", "status=409 codes=10220")
        code, output, evidence, summary = self.invoke(self.environ(), world)
        self.assertEqual(code, 1)
        self.assertIn("drill:api_http_failure", json.loads(evidence)["failures"])
        self.assertIn("FAIL", summary)

    def test_bad_environment_is_refused_before_any_change(self):
        cases = [
            self.environ(STAGING_WORKER_URL="https://two-bot-next.5150.workers.dev"),  # production-like name
            self.environ(STAGING_WORKER_URL="http://two-bot-next-staging.x.workers.dev"),
            self.environ(OWNERSHIP_CONTROL_TOKEN="short"),
            self.environ(GITHUB_RUN_ID="12 ; rm -rf"),
            self.environ(CLOUDFLARE_ACCOUNT_ID=None),
        ]
        for env in cases:
            with self.subTest(env={k: v for k, v in env.items() if k != "OWNERSHIP_CONTROL_TOKEN"}):
                env = {key: value for key, value in env.items() if value is not None}
                world = World()
                code, output, evidence, _ = self.invoke(
                    env, world) if env.get("CLOUDFLARE_ACCOUNT_ID") else self.invoke_real_client(env)
                self.assertEqual(code, 1)
                self.assertEqual(world.calls, [])
                self.assertIsNone(evidence)
                self.assertNotIn(SENTINEL, output)
                self.assertIn("refused before any change", output)

    def invoke_real_client(self, env):
        output = io.StringIO()
        with tempfile.TemporaryDirectory() as directory, \
                patch("builtins.print", lambda *a, **k: output.write(" ".join(map(str, a)) + "\n")):
            code = drill.main(["--target-version", TARGET, "--evidence", str(Path(directory) / "e.json")],
                              environ=env, make_control=lambda *a: (_ for _ in ()).throw(AssertionError))
        return code, output.getvalue(), None, None


class SourcePinTests(unittest.TestCase):
    SOURCE = Path(drill.__file__).read_text()

    def test_no_force_and_no_wrangler_cli_in_the_rollback_path(self):
        self.assertNotIn("force=true", self.SOURCE)
        self.assertNotIn("wrangler rollback", self.SOURCE.replace('"Worker-version rollback"', ""))
        self.assertNotIn("--yes", self.SOURCE)

    def test_worker_name_is_the_staging_worker_only(self):
        self.assertEqual(drill.WORKER, "two-bot-next-staging")
        self.assertNotIn("production", self.SOURCE.lower().replace("never production", ""))


class OwnershipControlTimeoutTests(unittest.TestCase):
    def test_timeout_outlasts_the_client_takeover_window(self):
        client = (Path(__file__).resolve().parents[1] / "wrangler/scripts/ownership-control.mjs").read_text()
        window_ms = int(re.search(r"takeoverWindowMs = (\d+)", client).group(1))
        request_ms = int(re.search(r"AbortSignal\.timeout\((\d+)\)", client).group(1))
        self.assertGreater(drill.OWNERSHIP_CONTROL_TIMEOUT_SECONDS * 1000, window_ms + 2 * request_ms)

    def test_control_subprocess_is_called_with_the_timeout(self):
        done = subprocess.CompletedProcess([], 0, stdout=json.dumps({"owner": {"phase": "active", "epoch": 1}}),
                                           stderr="")
        control = drill.ownership_control(SENTINEL, URL, "github-actions:1:rollback-drill", root="/repo")
        with patch.object(drill.subprocess, "run", return_value=done) as run:
            control("status")
        self.assertEqual(run.call_args.kwargs["timeout"], drill.OWNERSHIP_CONTROL_TIMEOUT_SECONDS)


if __name__ == "__main__":
    unittest.main()
