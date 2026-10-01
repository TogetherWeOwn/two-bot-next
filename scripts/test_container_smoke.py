"""Offline fixtures for the container gate; no Docker or Cargo needed."""

import contextlib
import importlib.util
import io
import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("container_smoke", Path(__file__).with_name("container-smoke.py"))
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)


class DockerFixture:
    def __init__(self):
        self.calls = []
        self.image_size = 110 * smoke.MIB
        self.binary_size = 7 * smoke.MIB
        self.user = "two-bot"
        self.health_command = ["CMD", smoke.BINARY, "--healthcheck"]
        self.uid = "1000"
        self.live_health_exit = 0
        self.dead_health_exit = 1
        self.health_status = "healthy"
        self.exit_code = "0"
        self.oom = False
        self.port = "127.0.0.1:32768\n"
        self.wait_timeout = False
        self.probe_timeout = False
        self.measure_timeout = False
        self.stopped = False

    def __call__(self, *args, **kwargs):
        self.calls.append((args, kwargs))
        output = ""
        code = 0
        if args[:2] == ("image", "inspect"):
            output = json.dumps([{"Size": self.image_size, "Config": {
                "User": self.user, "Healthcheck": {"Test": self.health_command},
            }}])
        elif args[0] == "history":
            # Two layers summing to image_size (inspect .Size is not used).
            output = f"{self.image_size - 4096}\n4096\n0\n"
        elif args[0] == "run" and "stat" in args:
            if self.measure_timeout:
                raise subprocess.TimeoutExpired(["docker", *args], kwargs.get("timeout"))
            output = str(self.binary_size)
        elif args[0] == "port":
            output = self.port
        elif args[0] == "inspect":
            output = json.dumps([{"State": {
                "Running": not self.stopped, "OOMKilled": self.oom,
                "Health": {"Status": self.health_status},
            }}])
        elif args[0] == "exec" and "cat" in args:
            output = "Name:\ttwo-bot\nUid:\t" + "\t".join([self.uid] * 4) + "\n"
        elif args[0] == "exec":
            code = self.live_health_exit
        elif args[0] == "run" and "--healthcheck" in args:
            if self.probe_timeout:
                raise subprocess.TimeoutExpired(["docker", *args], kwargs.get("timeout"))
            code = self.dead_health_exit
        elif args[0] == "wait":
            if self.wait_timeout:
                raise subprocess.TimeoutExpired("docker wait", kwargs["timeout"])
            self.stopped = True
            output = self.exit_code
        return subprocess.CompletedProcess(args, code, output, "")

    def removals(self):
        return [args for args, _ in self.calls if args[:2] == ("rm", "--force")]


def serve_in_thread(handler_class):
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_class)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server


def server_url(server, path):
    return f"http://127.0.0.1:{server.server_address[1]}{path}"


def json_reply(handler, status, body):
    handler.send_response(status)
    handler.send_header("Content-Type", "application/json")
    handler.send_header("Content-Length", str(len(body)))
    handler.end_headers()
    handler.wfile.write(body)


class FlakyHealthHandler(BaseHTTPRequestHandler):
    """Closes the first /health connection unanswered (RemoteDisconnected)."""

    unanswered = True

    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path == "/health" and type(self).unanswered:
            type(self).unanswered = False
            self.connection.close()
            return
        json_reply(self, 200, b'{"status": "ok"}')


class RedirectReadyzHandler(BaseHTTPRequestHandler):
    """/readyz 302 -> /other 503 with the expected JSON (the review repro)."""

    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path == "/readyz":
            self.send_response(302)
            self.send_header("Location", "/other")
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if self.path == "/other":
            json_reply(self, 503, b'{"components": [["process", "ready"], ["gateway", "down"]]}')
        else:
            json_reply(self, 200, b'{"status": "ok"}')


class HttpHelperTests(unittest.TestCase):
    def tearDown(self):
        FlakyHealthHandler.unanswered = True

    def test_early_close_is_a_retryable_miss_not_a_crash(self):
        server = serve_in_thread(FlakyHealthHandler)
        try:
            url = server_url(server, "/health")
            self.assertEqual(smoke.http_response(url), (None, None))
            self.assertEqual(smoke.http_response(url), (200, {"status": "ok"}))
        finally:
            server.shutdown()
            server.server_close()

    def test_redirect_is_returned_not_followed(self):
        server = serve_in_thread(RedirectReadyzHandler)
        try:
            code, body = smoke.http_response(server_url(server, "/readyz"))
            self.assertEqual(code, 302)
            self.assertIsNone(body)
        finally:
            server.shutdown()
            server.server_close()


PARKED_JOB = {"parked": True, "running": False, "last_start": None,
              "last_success": None, "last_error_class": None,
              "consecutive_failures": 0}


def parked_readyz_body():
    return {"components": [["process", "ready"], ["gateway", "down"]],
            "jobs": {name: dict(PARKED_JOB) for name in (
                "counter", "rank", "scheduled_events", "presence_probe",
                "community_scorecard", "inactivity",
            )}}


class ContainerSmokeTests(unittest.TestCase):
    def setUp(self):
        self.fixture = DockerFixture()
        self.clock = 0
        self.http = lambda url: (503, parked_readyz_body()) if url.endswith("/readyz") else (200, {"status": "ok"})

    def tick(self):
        self.clock += 1
        return self.clock

    def run_smoke(self, live_http=False, **kwargs):
        if live_http:
            http_patcher = contextlib.nullcontext()
        else:
            http_patcher = patch.object(
                smoke, "http_response", side_effect=lambda url: self.http(url))
        with patch.object(smoke, "docker", self.fixture), \
                http_patcher, \
                patch.object(smoke.time, "monotonic", side_effect=self.tick), \
                patch.object(smoke.time, "sleep"), \
                patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": ""}), \
                contextlib.redirect_stdout(io.StringIO()) as output:
            smoke.smoke("two-bot:fixture", **kwargs)
        return output.getvalue()

    def assert_rejected(self, message, **kwargs):
        with self.assertRaisesRegex(RuntimeError, message):
            self.run_smoke(**kwargs)

    def test_valid_image_contract_and_cleanup(self):
        output = self.run_smoke()
        self.assertIn("115343360 bytes", output)
        self.assertIn("7340032 bytes", output)
        self.assertIn("SIGTERM exits 0", output)
        args, _ = self.fixture.calls[-1]
        self.assertEqual(args[:2], ("rm", "--force"))
        run = next(args for args, _ in self.fixture.calls if "--detach" in args)
        self.assertIn("256m", run)
        self.assertIn("127.0.0.1::8080", run)
        self.assertNotIn("-e", run)
        self.assertNotIn("--env-file", run)
        wait = next(kwargs for args, kwargs in self.fixture.calls if args[0] == "wait")
        self.assertLessEqual(wait["timeout"], 10)

    def test_image_budget_is_enforced_before_runtime_start(self):
        self.assert_rejected("image exceeds size budget", image_max_bytes=1)
        self.assertFalse(any("--detach" in args for args, _ in self.fixture.calls))

    def test_binary_budget_is_enforced(self):
        self.assert_rejected("release binary exceeds size budget", binary_max_bytes=1)

    def test_default_image_budget_is_enforced(self):
        self.fixture.image_size = smoke.IMAGE_MAX_BYTES + 1
        self.assert_rejected("image exceeds size budget")

    def test_default_binary_budget_is_enforced(self):
        self.fixture.binary_size = smoke.BINARY_MAX_BYTES + 1
        self.assert_rejected("release binary exceeds size budget")

    def test_exact_budget_is_allowed(self):
        self.run_smoke(image_max_bytes=self.fixture.image_size, binary_max_bytes=self.fixture.binary_size)

    def test_missing_binary_fails(self):
        original = self.fixture
        def missing(*args, **kwargs):
            if "stat" in args:
                raise subprocess.CalledProcessError(1, args, stderr="binary missing")
            return original(*args, **kwargs)
        self.fixture = missing
        with self.assertRaises(subprocess.CalledProcessError):
            self.run_smoke()

    def test_root_image_fails(self):
        self.fixture.user = "root"
        self.assert_rejected("non-root user")

    def test_root_pid_fails_even_with_non_root_image_metadata(self):
        self.fixture.uid = "0"
        self.assert_rejected("PID 1 is root")
        self.assertEqual(self.fixture.calls[-1][0][:2], ("rm", "--force"))

    def test_broken_docker_healthcheck_fails(self):
        self.fixture.health_command = ["CMD", "/bin/true"]
        self.assert_rejected("HEALTHCHECK must invoke")

    def test_health_never_ready_fails_and_cleans_up(self):
        self.http = lambda url: (500, {})
        self.assert_rejected("/health did not return 200")
        self.assertEqual(self.fixture.calls[-1][0][:2], ("rm", "--force"))

    def test_falsely_ready_gateway_fails(self):
        self.http = lambda url: (200, {"status": "ok"})
        self.assert_rejected("/readyz must be 503")

    def test_untruthful_health_body_fails(self):
        self.http = lambda url: (200, {"status": "down"})
        self.assert_rejected("/health body must report status ok")

    def test_untruthful_readyz_body_fails(self):
        self.http = lambda url: (503, {}) if url.endswith("/readyz") else (200, {"status": "ok"})
        self.assert_rejected("/readyz body must report")

    def test_legacy_components_only_readyz_body_fails(self):
        # The pre-jobs contract is deliberately superseded: an informational
        # jobs map is now always serialized, so a bare components body no
        # longer satisfies the smoke gate.
        self.http = lambda url: (503, {"components": [["process", "ready"], ["gateway", "down"]]}) if url.endswith("/readyz") else (200, {"status": "ok"})
        self.assert_rejected("all six jobs parked")

    def test_readyz_without_jobs_map_fails(self):
        self.http = lambda url: (503, {"components": [["process", "ready"], ["gateway", "down"]], "jobs": {}}) if url.endswith("/readyz") else (200, {"status": "ok"})
        self.assert_rejected("all six jobs parked")

    def test_readyz_with_missing_job_fails(self):
        for name in parked_readyz_body()["jobs"]:
            with self.subTest(job=name):
                body = parked_readyz_body()
                del body["jobs"][name]
                self.http = lambda url: (503, body) if url.endswith("/readyz") else (200, {"status": "ok"})
                self.assert_rejected("all six jobs parked")

    def test_readyz_with_unexpected_job_fails(self):
        body = parked_readyz_body()
        body["jobs"]["unexpected"] = dict(PARKED_JOB)
        self.http = lambda url: (503, body) if url.endswith("/readyz") else (200, {"status": "ok"})
        self.assert_rejected("all six jobs parked")

    def test_readyz_with_running_job_fails(self):
        for name in parked_readyz_body()["jobs"]:
            with self.subTest(job=name):
                body = parked_readyz_body()
                body["jobs"][name] = dict(PARKED_JOB, running=True)
                self.http = lambda url: (503, body) if url.endswith("/readyz") else (200, {"status": "ok"})
                self.assert_rejected("all six jobs parked")

    def test_readyz_with_started_job_fails(self):
        for name in parked_readyz_body()["jobs"]:
            with self.subTest(job=name):
                body = parked_readyz_body()
                body["jobs"][name] = dict(PARKED_JOB, parked=False, last_start=100)
                self.http = lambda url: (503, body) if url.endswith("/readyz") else (200, {"status": "ok"})
                self.assert_rejected("all six jobs parked")

    def test_readyz_with_wrong_components_fails(self):
        body = parked_readyz_body()
        body["components"] = [["process", "ready"], ["gateway", "ready"]]
        self.http = lambda url: (503, body) if url.endswith("/readyz") else (200, {"status": "ok"})
        self.assert_rejected("ready process and parked gateway")

    def test_readyz_non_object_body_fails(self):
        self.http = lambda url: (503, []) if url.endswith("/readyz") else (200, {"status": "ok"})
        self.assert_rejected("must be a JSON object")

    def test_live_healthcheck_failure_fails(self):
        self.fixture.live_health_exit = 1
        self.assert_rejected("exit 0 against the live process")

    def test_unconditional_healthcheck_success_fails(self):
        self.fixture.dead_health_exit = 0
        self.assert_rejected("without a server must exit 1")

    def test_docker_healthcheck_never_healthy_fails(self):
        self.fixture.health_status = "unhealthy"
        self.assert_rejected("HEALTHCHECK did not become healthy")

    def test_sigkill_exit_not_accepted(self):
        self.fixture.exit_code = "137"
        self.assert_rejected("SIGTERM exit code was 137")

    def test_sigterm_timeout_not_accepted(self):
        self.fixture.wait_timeout = True
        with self.assertRaises(subprocess.TimeoutExpired):
            self.run_smoke()
        self.assertEqual(self.fixture.calls[-1][0][:2], ("rm", "--force"))

    def test_auxiliary_containers_are_named_capped_and_removed(self):
        self.run_smoke()
        runs = [args for args, _ in self.fixture.calls if args[0] == "run"]
        self.assertEqual(len(runs), 3)  # measure, detached main, probe
        names = set()
        for args in runs:
            self.assertNotIn("--rm", args)
            self.assertIn("--name", args)
            self.assertIn("256m", args)
            names.add(args[args.index("--name") + 1])
        self.assertEqual(len(names), 3)
        removed = {args[2] for args in self.fixture.removals()}
        self.assertEqual(removed, names)

    def test_probe_timeout_still_removes_probe_and_main(self):
        self.fixture.probe_timeout = True
        with self.assertRaises(subprocess.TimeoutExpired):
            self.run_smoke()
        # Measure was already removed before the probe ran; the probe and
        # the main container must both still be cleaned up on timeout.
        removed = {args[2] for args in self.fixture.removals()}
        self.assertEqual(len(removed), 3)

    def test_measure_timeout_still_removes_measure_container(self):
        self.fixture.measure_timeout = True
        with self.assertRaises(subprocess.TimeoutExpired):
            self.run_smoke()
        self.assertEqual(len(self.fixture.removals()), 1)

    def test_redirecting_readyz_is_rejected_live(self):
        server = serve_in_thread(RedirectReadyzHandler)
        try:
            self.fixture.port = f"127.0.0.1:{server.server_address[1]}\n"
            with self.assertRaisesRegex(RuntimeError, "/readyz must be 503"):
                self.run_smoke(live_http=True)
            self.assertEqual(self.fixture.calls[-1][0][:2], ("rm", "--force"))
        finally:
            server.shutdown()
            server.server_close()

    def test_oom_not_accepted(self):
        self.fixture.oom = True
        self.assert_rejected("shut down cleanly")

    def test_summary_records_measurements(self):
        scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP")
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            path = Path(directory) / "summary.md"
            with patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(path)}), \
                    contextlib.redirect_stdout(io.StringIO()):
                smoke.report("image: 115343360 bytes")
            self.assertEqual(path.read_text(), "image: 115343360 bytes\n\n")


if __name__ == "__main__":
    unittest.main()
