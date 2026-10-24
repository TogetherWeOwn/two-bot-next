"""Offline fixtures for the container gate; no Docker or Cargo needed."""

import contextlib
import importlib.util
import io
import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("container_smoke", Path(__file__).with_name("container-smoke.py"))
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)


def tar_of(path, data, mode=0o644, extra=()):
    """The archive `docker cp CONTAINER:PATH -` writes for one file."""
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w") as tar:
        for name, content, member_mode in ((Path(path).name, data, mode), *extra):
            info = tarfile.TarInfo(name)
            info.size, info.mode = len(content), member_mode
            tar.addfile(info, io.BytesIO(content))
    return buffer.getvalue()


class DockerFixture:
    def __init__(self):
        self.calls = []
        self.image_size = 110 * smoke.MIB
        self.daemon_size = 140 * smoke.MIB
        self.image_id = "sha256:" + "a" * 64
        self.history_output = None
        self.history_error = None
        self.binary_size = 7 * smoke.MIB
        self.user = "65532:65532"
        self.health_command = ["CMD", smoke.BINARY, "--healthcheck"]
        self.ca_bundle = "-----BEGIN CERTIFICATE-----\nfixture\n-----END CERTIFICATE-----\n"
        self.bundle_mode = 0o644
        self.archives = {}
        self.pid = 4242
        self.uids = ["65532"] * 4
        self.top_header = "PID   RUID   EUID   SUID   FSUID"
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
            output = json.dumps([{"Id": self.image_id, "Size": self.daemon_size, "Config": {
                "User": self.user, "Healthcheck": {"Test": self.health_command},
            }}])
        elif args[0] == "history":
            if self.history_error is not None:
                raise self.history_error
            # Two layers summing to image_size (inspect .Size is not used).
            output = self.history_output if self.history_output is not None else f"{self.image_size - 4096}\n4096\n0\n"
        elif args[0] == "create":
            output = "f" * 64 + "\n"
        elif args[0] == "cp":
            if self.measure_timeout:
                raise subprocess.TimeoutExpired(["docker", *args], kwargs.get("timeout"))
            path = args[2].partition(":")[2]
            if path in self.archives:
                output = self.archives[path]
            elif path == smoke.BINARY:
                output = tar_of(path, bytes(self.binary_size), 0o755)
            else:
                output = tar_of(path, self.ca_bundle.encode(), self.bundle_mode)
        elif args[0] == "port":
            output = self.port
        elif args[0] == "inspect":
            output = json.dumps([{"State": {
                "Running": not self.stopped, "OOMKilled": self.oom, "Pid": self.pid,
                "Health": {"Status": self.health_status},
            }}])
        elif args[0] == "top":
            # A sibling process first: PID 1 is selected by host PID, not order.
            output = f"{self.top_header}\n4300   0   0   0   0\n4242   " + "   ".join(self.uids) + "\n"
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


class TruncatedBodyHandler(BaseHTTPRequestHandler):
    """Promises 16 body bytes, sends 8, then closes (dropped mid-body)."""

    truncate = True

    def log_message(self, *args):
        pass

    def do_GET(self):
        if type(self).truncate:
            type(self).truncate = False
            self.send_response(200)
            self.send_header("Content-Length", "16")
            self.end_headers()
            self.wfile.write(b'{"statu')
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
            json_reply(self, 503, b'{"components": [["process", "ready"], ["gateway", "down"], ["database", "down"]]}')
        else:
            json_reply(self, 200, b'{"status": "ok"}')


def padded_json(size):
    """A valid JSON object of exactly `size` bytes (trailing whitespace)."""
    head = b'{"status": "ok"}'
    return head + b" " * (size - len(head))


class SizedBodyHandler(BaseHTTPRequestHandler):
    """Answers every GET with a valid JSON body of `size` bytes."""

    size = 0
    user_agents = []

    def log_message(self, *args):
        pass

    def do_GET(self):
        type(self).user_agents.append(self.headers.get("User-Agent"))
        try:
            json_reply(self, 200, padded_json(type(self).size))
        except OSError:
            pass  # the client refused the body and hung up mid-write


class EndlessBodyHandler(BaseHTTPRequestHandler):
    """Streams a close-delimited body with no Content-Length, until hung up on."""

    written = 0
    ended = threading.Event()

    def log_message(self, *args):
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Connection", "close")
        self.end_headers()
        chunk = b" " * 4096
        try:
            # Far more than any socket buffer: an unbounded client would take
            # all of it, a bounded one leaves this blocked then broken-piped.
            while type(self).written < 256 * smoke.MIB:
                self.wfile.write(chunk)
                self.wfile.flush()
                type(self).written += len(chunk)
        except OSError:
            pass
        finally:
            type(self).ended.set()


class HttpHelperTests(unittest.TestCase):
    def tearDown(self):
        FlakyHealthHandler.unanswered = True
        TruncatedBodyHandler.truncate = True
        SizedBodyHandler.user_agents = []
        EndlessBodyHandler.written = 0
        EndlessBodyHandler.ended.clear()

    def sized_get(self, size):
        SizedBodyHandler.size = size
        server = serve_in_thread(SizedBodyHandler)
        try:
            return smoke.http_response(server_url(server, "/readyz"))
        finally:
            server.shutdown()
            server.server_close()

    def test_cap_matches_sibling_probes(self):
        self.assertEqual(smoke.MAX_BODY_BYTES, 64 * 1024)

    def test_exact_cap_body_still_parses(self):
        self.assertEqual(self.sized_get(smoke.MAX_BODY_BYTES), (200, {"status": "ok"}))

    def test_body_one_byte_over_cap_is_refused_before_parsing(self):
        # Valid JSON the whole way, so only the cap can be what rejects it.
        with self.assertRaises(smoke.BodyTooLarge) as caught:
            self.sized_get(smoke.MAX_BODY_BYTES + 1)
        self.assertEqual(str(caught.exception), "loopback response body exceeds the 64 KiB cap")
        self.assertIsInstance(caught.exception, RuntimeError)
        self.assertNotIsInstance(caught.exception, ValueError)

    def test_over_cap_refusal_is_not_a_retryable_transport_miss(self):
        # The caller loops on (None, None) until its deadline; an oversized
        # body must surface immediately instead of being retried.
        with self.assertRaises(smoke.BodyTooLarge):
            self.sized_get(10 * smoke.MIB)

    def test_endless_body_is_cut_off_near_the_cap(self):
        server = serve_in_thread(EndlessBodyHandler)
        try:
            with self.assertRaises(smoke.BodyTooLarge):
                smoke.http_response(server_url(server, "/health"))
            self.assertTrue(EndlessBodyHandler.ended.wait(10), "client never hung up")
            # Loopback socket buffers hold a few MiB at most; reading the
            # whole 256 MiB stream would have written all of it.
            self.assertLess(EndlessBodyHandler.written, 64 * smoke.MIB)
        finally:
            server.shutdown()
            server.server_close()

    def test_every_read_is_bounded(self):
        reads = []

        class Response:
            status = 200
            length = 0

            def read(self, amt=None):
                reads.append(amt)
                return b'{"status": "ok"}'

        class Connection:
            def __init__(self, *args, **kwargs):
                pass

            def request(self, *args, **kwargs):
                pass

            def getresponse(self):
                return Response()

            def close(self):
                pass

        with patch.object(smoke.http.client, "HTTPConnection", Connection):
            self.assertEqual(smoke.http_response("http://127.0.0.1:1/health"), (200, {"status": "ok"}))
        self.assertEqual(reads, [smoke.MAX_BODY_BYTES + 1])

    def test_user_agent_is_pinned(self):
        self.sized_get(len(b'{"status": "ok"}'))
        self.assertEqual(SizedBodyHandler.user_agents, [smoke.USER_AGENT])
        self.assertTrue(smoke.USER_AGENT.startswith("two-bot-next-container-smoke/"))

    def test_early_close_is_a_retryable_miss_not_a_crash(self):
        server = serve_in_thread(FlakyHealthHandler)
        try:
            url = server_url(server, "/health")
            self.assertEqual(smoke.http_response(url), (None, None))
            self.assertEqual(smoke.http_response(url), (200, {"status": "ok"}))
        finally:
            server.shutdown()
            server.server_close()

    def test_body_cut_short_mid_stream_is_a_retryable_miss(self):
        server = serve_in_thread(TruncatedBodyHandler)
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
    return {"components": [["process", "ready"], ["gateway", "down"], ["database", "down"], ["token_invalid", "ready"]],
            "jobs": {name: dict(PARKED_JOB) for name in (
                "counter", "rank", "scheduled_events", "settings", "presence_probe",
                "community_scorecard", "inactivity", "audit_retry",
                "self_role_recovery", "scheduled_messages", "feeds",
                "member_unban_sweep", "retention", "lfg_sweep", "giveaway_countdown",
            )}}


class ContainerSmokeTests(unittest.TestCase):
    def setUp(self):
        self.fixture = DockerFixture()
        self.clock = 0
        self.http = lambda url: (503, parked_readyz_body()) if url.endswith("/readyz") else (200, {"status": "ok"})

    def tick(self):
        self.clock += 1
        return self.clock

    def run_smoke(self, live_http=False, image="two-bot:fixture", **kwargs):
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
            smoke.smoke(image, **kwargs)
        return output.getvalue()

    def assert_rejected(self, message, **kwargs):
        with self.assertRaisesRegex(RuntimeError, message):
            self.run_smoke(**kwargs)

    def test_valid_image_contract_and_cleanup(self):
        output = self.run_smoke()
        self.assertIn("image (summed uncompressed Docker history layer bytes): 115343360 bytes", output)
        self.assertIn("Docker storage-driver Size (diagnostic only): 146800640 bytes", output)
        self.assertIn("7340032 bytes", output)
        self.assertIn("SIGTERM exits 0", output)
        args, _ = self.fixture.calls[-1]
        self.assertEqual(args[:2], ("rm", "--force"))
        run = next(args for args, _ in self.fixture.calls if "--detach" in args)
        self.assertIn("256m", run)
        self.assertIn(self.fixture.image_id, run)
        self.assertIn("127.0.0.1::8080", run)
        self.assertNotIn("-e", run)
        self.assertNotIn("--env-file", run)
        wait = next(kwargs for args, kwargs in self.fixture.calls if args[0] == "wait")
        self.assertLessEqual(wait["timeout"], 10)

    def test_immutable_input_is_used_for_measure_runtime_and_dead_health_probe(self):
        image_id = "sha256:" + "a" * 64
        self.run_smoke(image=image_id)
        self.assertEqual(self.fixture.calls[0][0], ("image", "inspect", image_id))
        runs = [args for args, _ in self.fixture.calls if args[0] in ("create", "run")]
        self.assertEqual([args[0] for args in runs], ["create", "run", "run"])
        for args in runs:
            self.assertIn(image_id, args)
            self.assertNotIn("two-bot:fixture", args)

    def test_ci_smoke_is_bound_to_build_output_not_shared_tag(self):
        workflow = (Path(__file__).resolve().parent.parent / ".github/workflows/check.yml").read_text()
        job = workflow[workflow.index("\n  container:\n"):workflow.index("\n  community-db:\n")]
        self.assertNotIn("two-bot:ci", job)
        self.assertIn("IMAGE: two-bot-next:smoke-${{ github.run_id }}-${{ github.run_attempt }}", job)
        self.assertIn("id: build", job)
        self.assertIn("tags: ${{ env.IMAGE }}", job)
        # container-smoke, its deliberate-breakage proof, and the JSON-log smoke.
        self.assertEqual(job.count("IMAGE_ID: ${{ steps.build.outputs.imageid }}"), 3)
        self.assertIn('python3 scripts/container-smoke.py "$IMAGE_ID"', job)
        self.assertIn('python3 scripts/test-logging-container.py "$IMAGE_ID"', job)
        self.assertIn('python3 scripts/container-smoke.py "$IMAGE_ID" "--$budget-max-bytes" 1', job)
        self.assertIn('docker image rm "$IMAGE"', job)
        self.assertNotIn("docker image prune", job)

    def test_image_files_are_read_from_a_never_started_container_without_exec_helpers(self):
        self.run_smoke()
        calls = [args for args, _ in self.fixture.calls]
        create = next(args for args in calls if args[0] == "create")
        measure = create[create.index("--name") + 1]
        self.assertIn("none", create)
        copies = [args for args in calls if args[0] == "cp"]
        self.assertEqual(copies, [("cp", "--follow-link", f"{measure}:{path}", "-")
                                  for path in (smoke.BINARY, smoke.CA_BUNDLE)])
        self.assertTrue(all(kwargs["text"] is False for args, kwargs in self.fixture.calls if args[0] == "cp"))
        self.assertLess(calls.index(("rm", "--force", measure)), next(
            index for index, args in enumerate(calls) if "--detach" in args))
        # The distroless runtime has no shell, cat or grep: the only exec is
        # the image's own binary.
        execs = [args for args in calls if args[0] == "exec"]
        self.assertEqual([args[2:] for args in execs], [(smoke.BINARY, "--healthcheck")])

    def test_missing_or_non_pem_trust_bundle_fails_and_cleans_up(self):
        for bundle, mode in (("-----BEGIN CERTIFICATE-----x\n-----END CERTIFICATE-----\n", 0o644),
                             (DockerFixture().ca_bundle, 0o640), (DockerFixture().ca_bundle, 0o600)):
            with self.subTest(bundle=bundle, mode=oct(mode)):
                self.fixture.ca_bundle, self.fixture.bundle_mode = bundle, mode
                self.assert_rejected("trust bundle must contain PEM certificates readable by the runtime user")
                self.assertEqual(self.fixture.calls[-1][0][:2], ("rm", "--force"))
                self.assertFalse(any("--detach" in args for args, _ in self.fixture.calls))

    def test_image_file_must_be_one_regular_file(self):
        directory = io.BytesIO()
        with tarfile.open(fileobj=directory, mode="w") as tar:
            info = tarfile.TarInfo("certs")
            info.type = tarfile.DIRTYPE
            tar.addfile(info)
        empty = b"\0" * 1024
        for archive in (directory.getvalue(), empty,
                        tar_of(smoke.CA_BUNDLE, b"pem", extra=[("extra", b"x", 0o644)])):
            with self.subTest(size=len(archive)):
                self.fixture.calls.clear()
                self.fixture.archives = {smoke.CA_BUNDLE: archive}
                self.assert_rejected("must be one regular file")
                self.assertEqual(self.fixture.calls[-1][0][:2], ("rm", "--force"))

    def assert_history_rejected(self, history, message):
        self.fixture = DockerFixture()
        self.fixture.history_output = history
        self.fixture.daemon_size = 1  # A valid, tiny Size cannot rescue history.
        self.assert_rejected(message)
        self.assertEqual([args[0] for args, _ in self.fixture.calls], ["image", "history"])

    def test_measurement_failure_never_falls_back_to_daemon_size(self):
        self.assert_history_rejected("not a layer size\n", "nonempty nonnegative integers")

    def test_missing_history_is_rejected_before_creating_containers(self):
        self.assert_history_rejected("", "no layer sizes")

    def test_malformed_history_records_are_rejected_before_creating_containers(self):
        for history in (
            "\n", " \t\n", "4096\n\n0\n", "\n4096\n", "4096\n \t\n",
            "4096 8192\n", "4096B\n", "4.1kB\n", "1.5\n", "NaN\n",
            "+4096\n", "4_096\n", "1e3\n", "٤٠٩٦\n",
        ):
            with self.subTest(history=history):
                self.assert_history_rejected(history, "nonempty nonnegative integers")

    def test_negative_history_records_are_rejected_before_creating_containers(self):
        for history in ("-1\n", "4096\n-1\n", "-0\n"):
            with self.subTest(history=history):
                self.assert_history_rejected(history, "nonempty nonnegative integers")

    def test_all_zero_history_is_rejected_before_creating_containers(self):
        for history in ("0\n", "0\n0\n", "000\n"):
            with self.subTest(history=history):
                self.assert_history_rejected(history, "only zero-size layers")

    def test_history_sums_integer_records_with_whitespace_and_zero_layers(self):
        self.fixture.history_output = " 4096 \n\t0\t\n8192\n"
        output = self.run_smoke()
        self.assertIn("image (summed uncompressed Docker history layer bytes): 12288 bytes", output)

    def test_history_budget_does_not_fall_back_to_smaller_daemon_size(self):
        self.fixture.daemon_size = 1
        self.fixture.image_size = smoke.IMAGE_MAX_BYTES + 1
        self.assert_rejected("image exceeds size budget")
        self.assertFalse(any("--detach" in args for args, _ in self.fixture.calls))

    def test_history_and_all_image_probes_use_the_once_inspected_immutable_id(self):
        self.run_smoke()
        inspections = [args for args, _ in self.fixture.calls if args[:2] == ("image", "inspect")]
        self.assertEqual(inspections, [("image", "inspect", "two-bot:fixture")])
        histories = [args for args, _ in self.fixture.calls if args[0] == "history"]
        self.assertEqual(histories, [(
            "history", "--no-trunc", "--human=false", "--format", "{{.Size}}", self.fixture.image_id,
        )])
        # Measurement is a never-started `create`; the runtime and the
        # no-server probe are the two `run` calls. All three use the ID.
        probes = [args for args, _ in self.fixture.calls if args[0] in ("create", "run")]
        self.assertEqual([args[0] for args in probes], ["create", "run", "run"])
        for args in probes:
            self.assertIn(self.fixture.image_id, args)
        for args, _ in self.fixture.calls[1:]:
            self.assertNotIn("two-bot:fixture", args)

    def assert_history_subprocess_failure(self, error):
        self.fixture.history_error = error
        self.fixture.daemon_size = 1
        # Exercise the real docker wrapper, not just the smoke's Docker mock.
        with patch.object(smoke.subprocess, "run", side_effect=lambda command, **kwargs: self.fixture(*command[1:], **kwargs)), \
                patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": ""}), \
                contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(type(error)) as raised:
                smoke.smoke("two-bot:fixture")
        self.assertIs(raised.exception, error)
        self.assertEqual([args[0] for args, _ in self.fixture.calls], ["image", "history"])
        args, kwargs = self.fixture.calls[-1]
        self.assertEqual(args[-1], self.fixture.image_id)
        self.assertEqual(kwargs, {"capture_output": True, "text": True, "timeout": 30, "check": True})

    def test_history_timeout_aborts_without_size_fallback_or_containers(self):
        self.assert_history_subprocess_failure(subprocess.TimeoutExpired(["docker", "history"], 30))

    def test_history_nonzero_exit_aborts_without_size_fallback_or_containers(self):
        self.assert_history_subprocess_failure(subprocess.CalledProcessError(
            1, ["docker", "history"], stderr="history unavailable",
        ))

    def test_image_budget_is_enforced_before_runtime_start(self):
        self.assert_rejected("image exceeds size budget", image_max_bytes=1)
        self.assertFalse(any("--detach" in args for args, _ in self.fixture.calls))

    def test_ci_image_size_failure_keeps_default_budgets(self):
        self.fixture.image_size = 142437143
        self.fixture.binary_size = 10287160
        self.assertEqual(smoke.IMAGE_MAX_BYTES, 112 * smoke.MIB)
        self.assertEqual(smoke.BINARY_MAX_BYTES, 18 * smoke.MIB)
        self.assert_rejected("image exceeds size budget")
        self.assertFalse(any("--detach" in args for args, _ in self.fixture.calls))

    def test_runtime_ca_bundle_is_required(self):
        for bundle in ("", "not PEM", "-----BEGIN CERTIFICATE-----\n"):
            with self.subTest(bundle=bundle):
                self.fixture.ca_bundle = bundle
                self.assert_rejected("runtime CA bundle must contain PEM certificates")
                self.assertTrue(self.fixture.removals())

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
            if args[0] == "cp" and args[2].endswith(":" + smoke.BINARY):
                raise subprocess.CalledProcessError(1, args, stderr="binary missing")
            return original(*args, **kwargs)
        self.fixture = missing
        with self.assertRaises(subprocess.CalledProcessError):
            self.run_smoke()

    def test_root_image_fails(self):
        self.fixture.user = "root"
        self.assert_rejected("non-root user")

    def test_root_pid_fails_even_with_non_root_image_metadata(self):
        for index in range(4):
            with self.subTest(uid_field=index):
                self.fixture = DockerFixture()
                self.fixture.uids[index] = "0"
                self.assert_rejected("PID 1 is root")
                self.assertEqual(self.fixture.calls[-1][0][:2], ("rm", "--force"))
        top = next(args for args, _ in self.fixture.calls if args[0] == "top")
        self.assertEqual(top[2:], ("-o", "pid,ruid,euid,suid,fsuid"))

    def test_unprovable_pid1_identity_fails(self):
        for field, value, message in (("pid", 0, "no host process"), ("pid", None, "no host process"),
                                      ("pid", 9999, "did not report PID 1"),
                                      ("top_header", "PID USER", "unexpected docker top header"),
                                      ("uids", ["65532"] * 3, "did not report PID 1")):
            with self.subTest(field=field, value=value):
                self.fixture = DockerFixture()
                setattr(self.fixture, field, value)
                self.assert_rejected(message)
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
        body = parked_readyz_body()
        del body["jobs"]
        self.http = lambda url: (503, body) if url.endswith("/readyz") else (200, {"status": "ok"})
        self.assert_rejected("all fifteen jobs parked")

    def test_readyz_without_jobs_map_fails(self):
        body = parked_readyz_body()
        body["jobs"] = {}
        self.http = lambda url: (503, body) if url.endswith("/readyz") else (200, {"status": "ok"})
        self.assert_rejected("all fifteen jobs parked")

    def test_readyz_with_missing_job_fails(self):
        for name in parked_readyz_body()["jobs"]:
            with self.subTest(job=name):
                body = parked_readyz_body()
                del body["jobs"][name]
                self.http = lambda url: (503, body) if url.endswith("/readyz") else (200, {"status": "ok"})
                self.assert_rejected("all fifteen jobs parked")

    def test_readyz_with_unexpected_job_fails(self):
        body = parked_readyz_body()
        body["jobs"]["unexpected"] = dict(PARKED_JOB)
        self.http = lambda url: (503, body) if url.endswith("/readyz") else (200, {"status": "ok"})
        self.assert_rejected("all fifteen jobs parked")

    def test_readyz_with_running_job_fails(self):
        for name in parked_readyz_body()["jobs"]:
            with self.subTest(job=name):
                body = parked_readyz_body()
                body["jobs"][name] = dict(PARKED_JOB, running=True)
                self.http = lambda url: (503, body) if url.endswith("/readyz") else (200, {"status": "ok"})
                self.assert_rejected("all fifteen jobs parked")

    def test_readyz_with_started_job_fails(self):
        for name in parked_readyz_body()["jobs"]:
            with self.subTest(job=name):
                body = parked_readyz_body()
                body["jobs"][name] = dict(PARKED_JOB, parked=False, last_start=100)
                self.http = lambda url: (503, body) if url.endswith("/readyz") else (200, {"status": "ok"})
                self.assert_rejected("all fifteen jobs parked")

    def test_readyz_with_wrong_components_fails(self):
        body = parked_readyz_body()
        body["components"] = [["process", "ready"], ["gateway", "ready"]]
        self.http = lambda url: (503, body) if url.endswith("/readyz") else (200, {"status": "ok"})
        self.assert_rejected("ready process, parked gateway")

    def test_readyz_missing_or_down_token_state_fails(self):
        for token in (None, ["token_invalid", "down"]):
            with self.subTest(token=token):
                body = parked_readyz_body()
                body["components"].pop()
                if token is not None:
                    body["components"].append(token)
                self.http = lambda url: (503, body) if url.endswith("/readyz") else (200, {"status": "ok"})
                self.assert_rejected("valid token state")

    def test_readyz_components_mismatch_reports_actual_body(self):
        body = parked_readyz_body()
        body["components"] = [["process", "ready"]]
        self.http = lambda url: (503, body) if url.endswith("/readyz") else (200, {"status": "ok"})
        with self.assertRaisesRegex(RuntimeError, r"got status=503 body="):
            self.run_smoke()

    def test_readyz_database_contract_is_enforced(self):
        for components in (
            [["process", "ready"], ["gateway", "down"]],
            [["process", "ready"], ["gateway", "down"], ["database", "ready"]],
            [["process", "ready"], ["gateway", "down"], ["database", "down"], ["extra", "down"]],
        ):
            with self.subTest(components=components):
                body = parked_readyz_body()
                body["components"] = components
                self.http = lambda url: (503, body) if url.endswith("/readyz") else (200, {"status": "ok"})
                self.assert_rejected("database down")

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
        runs = [args for args, _ in self.fixture.calls if args[0] in ("create", "run")]
        self.assertEqual(len(runs), 3)  # created measure, detached main, probe
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

    def test_oversized_health_body_fails_the_smoke_live(self):
        SizedBodyHandler.size = smoke.MAX_BODY_BYTES + 1
        server = serve_in_thread(SizedBodyHandler)
        try:
            self.fixture.port = f"127.0.0.1:{server.server_address[1]}\n"
            with self.assertRaises(smoke.BodyTooLarge):
                self.run_smoke(live_http=True)
            self.assertEqual(self.fixture.calls[-1][0][:2], ("rm", "--force"))
        finally:
            server.shutdown()
            server.server_close()
            SizedBodyHandler.user_agents = []

    def test_main_reports_the_body_cap_refusal_without_a_traceback(self):
        with patch.object(smoke, "smoke", side_effect=smoke.BodyTooLarge("loopback response body exceeds the 64 KiB cap")), \
                patch.object(sys, "argv", ["container-smoke.py", "two-bot:fixture"]):
            with self.assertRaises(SystemExit) as caught:
                smoke.main()
        self.assertEqual(
            caught.exception.code,
            "FAIL container smoke: loopback response body exceeds the 64 KiB cap")

    def test_oom_not_accepted(self):
        self.fixture.oom = True
        self.assert_rejected("shut down cleanly")

    def test_cleanup_guard_failure_matrix_preserves_primary_and_bounds_output(self):
        name = "two-bot-smoke-" + "a" * 32
        sentinel = "uncontrolled Docker detail"
        failures = (
            subprocess.CompletedProcess([], 1, sentinel, sentinel),
            subprocess.TimeoutExpired([sentinel], 30, output=sentinel, stderr=sentinel),
            OSError(sentinel),
            subprocess.CalledProcessError(1, [sentinel], output=sentinel, stderr=sentinel),
        )
        for stage in ("measurement", "health-probe", "main"):
            for failure in failures:
                for primary in (None, RuntimeError("primary smoke failure")):
                    with self.subTest(stage=stage, failure=type(failure), primary=primary):
                        def remove(*args, **kwargs):
                            if isinstance(failure, BaseException):
                                raise failure
                            return failure
                        with patch.object(smoke, "docker", side_effect=remove) as docker, \
                                patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": ""}), \
                                contextlib.redirect_stdout(io.StringIO()) as output:
                            with self.assertRaises(RuntimeError) as caught:
                                with smoke.owned_container(name, stage):
                                    if primary is not None:
                                        raise primary
                        docker.assert_called_once_with("rm", "--force", name, timeout=30, check=False)
                        if primary is not None:
                            self.assertIs(caught.exception, primary)
                        else:
                            self.assertIsInstance(caught.exception, smoke.CleanupFailure)
                            self.assertIsNone(caught.exception.__cause__)
                            self.assertTrue(caught.exception.__suppress_context__)
                        self.assertIn(f"FAIL cleanup {stage}:", output.getvalue())
                        self.assertNotIn(sentinel, output.getvalue() + str(caught.exception))
                        self.assertLess(len(output.getvalue()), 300)

    def test_failed_log_collection_does_not_mask_primary_or_skip_cleanup(self):
        fixture = self.fixture
        self.http = lambda url: (500, {})
        for error in (OSError("log process unavailable"), subprocess.TimeoutExpired("docker logs", 30)):
            with self.subTest(error=type(error)):
                fixture.calls.clear()
                def docker(*args, **kwargs):
                    if args[0] == "logs":
                        raise error
                    return fixture(*args, **kwargs)
                self.fixture = docker
                self.assert_rejected("/health did not return 200")
                self.assertEqual(len(fixture.removals()), 2)

    def test_cleanup_only_failure_does_not_collect_runtime_logs(self):
        fixture = self.fixture
        def docker(*args, **kwargs):
            if args[:2] == ("rm", "--force") and args[2].endswith("-probe"):
                fixture.calls.append((args, kwargs))
                return subprocess.CompletedProcess(args, 1, "", "")
            return fixture(*args, **kwargs)
        self.fixture = docker
        self.assert_rejected("cleanup health-probe")
        self.assertEqual(len(fixture.removals()), 3)
        self.assertFalse(any(args[0] == "logs" for args, _ in fixture.calls))

    def test_cleanup_report_io_failure_preserves_arbitrated_exception(self):
        name = "two-bot-smoke-" + "a" * 32
        for stage in ("measurement", "health-probe", "main"):
            for error in (OSError(28, "summary disk full"), BrokenPipeError("stdout closed")):
                for primary in (None, RuntimeError("primary smoke failure")):
                    with self.subTest(stage=stage, error=type(error), primary=primary):
                        with patch.object(smoke, "docker", return_value=subprocess.CompletedProcess([], 1)), \
                                patch.object(smoke, "report", side_effect=error) as report:
                            with self.assertRaises(RuntimeError) as caught:
                                with smoke.owned_container(name, stage):
                                    if primary is not None:
                                        raise primary
                        report.assert_called_once_with(f"FAIL cleanup {stage}: nonzero removal exit")
                        if primary is not None:
                            self.assertIs(caught.exception, primary)
                        else:
                            self.assertIsInstance(caught.exception, smoke.CleanupFailure)
                            self.assertEqual(str(caught.exception), f"FAIL cleanup {stage}: nonzero removal exit")

    def test_cleanup_summary_write_failure_does_not_collect_runtime_logs(self):
        real_report = smoke.report
        for stage in ("measurement", "health-probe", "main"):
            with self.subTest(stage=stage):
                fixture = DockerFixture()
                def docker(*args, **kwargs):
                    result = fixture(*args, **kwargs)
                    if args[:2] == ("rm", "--force"):
                        name = args[2]
                        actual = "measurement" if name.startswith("two-bot-measure-") else (
                            "health-probe" if name.endswith("-probe") else "main")
                        if actual == stage:
                            result.returncode = 1
                    return result
                self.fixture = docker
                def report(message):
                    if message.startswith("FAIL cleanup"):
                        with patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": "fixture-summary"}), \
                                patch.object(smoke.Path, "open", side_effect=OSError(28, "summary disk full")):
                            real_report(message)
                    else:
                        real_report(message)
                with patch.object(smoke, "report", side_effect=report):
                    with self.assertRaises(smoke.CleanupFailure) as caught:
                        self.run_smoke()
                self.assertEqual(str(caught.exception), f"FAIL cleanup {stage}: nonzero removal exit")
                self.assertEqual(len(fixture.removals()), 1 if stage == "measurement" else 3)
                self.assertFalse(any(args[0] == "logs" for args, _ in fixture.calls))
                with patch.object(smoke, "smoke", side_effect=caught.exception), \
                        patch.object(sys, "argv", ["container-smoke.py", "two-bot:fixture"]):
                    with self.assertRaises(SystemExit) as exit_result:
                        smoke.main()
                self.assertEqual(exit_result.exception.code,
                                 f"FAIL container smoke: FAIL cleanup {stage}: nonzero removal exit")

    def test_unavailable_log_report_io_failure_preserves_primary(self):
        real_report = smoke.report
        for error in (OSError(28, "summary disk full"), BrokenPipeError("stdout closed")):
            with self.subTest(error=type(error)):
                fixture = DockerFixture()
                primary = RuntimeError("primary smoke failure")
                def docker(*args, **kwargs):
                    if args[0] == "logs":
                        raise OSError("log process unavailable")
                    return fixture(*args, **kwargs)
                def http(url):
                    raise primary
                def report(message):
                    if message == "FAIL container smoke: diagnostic logs unavailable":
                        raise error
                    real_report(message)
                self.fixture, self.http = docker, http
                with patch.object(smoke, "report", side_effect=report):
                    with self.assertRaises(RuntimeError) as caught:
                        self.run_smoke()
                self.assertIs(caught.exception, primary)
                self.assertEqual(len(fixture.removals()), 2)

    def test_summary_records_measurements(self):
        scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP")
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            path = Path(directory) / "summary.md"
            with patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": str(path)}), \
                    contextlib.redirect_stdout(io.StringIO()):
                smoke.report("image: 115343360 bytes")
            self.assertEqual(path.read_text(), "image: 115343360 bytes\n\n")


class MalformedInspectRegressionTests(unittest.TestCase):
    """Review regression: malformed `image inspect` FAILs clean, never tracebacks."""

    def run_cli(self, stdout):
        calls = []

        def docker(*args, **kwargs):
            calls.append(args)
            if args[:2] == ("image", "inspect"):
                return subprocess.CompletedProcess(args, 0, stdout, "")
            raise AssertionError(f"unexpected docker call: {args!r}")

        with patch.object(smoke, "docker", docker), \
                patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": ""}), \
                patch.object(sys, "argv", ["container-smoke.py", "two-bot:fixture"]), \
                contextlib.redirect_stdout(io.StringIO()) as out, \
                contextlib.redirect_stderr(io.StringIO()) as err, \
                self.assertRaises(SystemExit) as caught:
            smoke.main()
        return caught.exception, out.getvalue(), err.getvalue(), calls

    def test_malformed_inspect_fails_clean_without_containers(self):
        for stdout, label in (
            ("[]", "empty-array"),
            ("[null]", "null-element"),
            ("123", "non-list"),
            ("[[]]", "nested-array"),
            ('["hi"]', "string-element"),
            ("{}", "non-list-object"),
            ("not json", "non-json"),
            (json.dumps([{"Size": 1}]), "missing-id"),
            (json.dumps([{"Id": "sha256:" + "a" * 64}]), "missing-size"),
            # A wrong-typed Id must not reach subprocess argv (TypeError
            # traceback): it fails at validation before any history call.
            # Each fixture carries a valid Config so the Id require is the
            # only line stopping the history call (individually pinned).
            (json.dumps([{"Id": 123, "Size": 1, "Config": {}}]), "numeric-id"),
            (json.dumps([{"Id": None, "Size": 1, "Config": {}}]), "null-id"),
            (json.dumps([{"Id": ["sha256:" + "b" * 64], "Size": 1,
                          "Config": {}}]), "list-id"),
            (json.dumps([{"Id": "", "Size": 1, "Config": {}}]), "empty-id"),
            # A missing or wrong-typed Config (or Healthcheck) must fail at
            # validation too, not as AttributeError at config.get below.
            (json.dumps([{"Id": "sha256:" + "a" * 64, "Size": 1}]),
             "missing-config"),
            (json.dumps([{"Id": "sha256:" + "a" * 64, "Size": 1,
                          "Config": None}]), "null-config"),
            (json.dumps([{"Id": "sha256:" + "a" * 64, "Size": 1,
                          "Config": "x"}]), "string-config"),
            (json.dumps([{"Id": "sha256:" + "a" * 64, "Size": 1,
                          "Config": {"User": "65532:65532",
                                     "Healthcheck": "x"}}]),
             "string-healthcheck"),
        ):
            with self.subTest(label=label):
                error, out, err, calls = self.run_cli(stdout)
                self.assertIsInstance(error.code, str, label)
                self.assertTrue(error.code.startswith("FAIL container smoke:"), label)
                self.assertNotIn("Traceback", out + err, label)
                self.assertEqual(calls, [("image", "inspect", "two-bot:fixture")], label)


if __name__ == "__main__":
    unittest.main()
