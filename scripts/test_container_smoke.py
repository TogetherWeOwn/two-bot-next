"""Offline fixtures for the container gate; no Docker or Cargo needed."""

import contextlib
import gzip
import hashlib
import importlib.util
import io
import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import subprocess
import tarfile
import tempfile
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("container_smoke", Path(__file__).with_name("container-smoke.py"))
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)

from docker_image_size import archive_image_bytes, image_bytes


class DockerFixture:
    def __init__(self):
        self.calls = []
        self.image_size = 110 * smoke.MIB
        self.daemon_size = 140 * smoke.MIB
        self.image_id = "sha256:" + "a" * 64
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
            output = json.dumps([{"Id": self.image_id, "Size": self.daemon_size,
                                  "FixtureLayerBytes": self.image_size, "Config": {
                "User": self.user, "Healthcheck": {"Test": self.health_command},
            }}])
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


def tar_bytes(objects):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w") as archive:
        for path, body in objects:
            member = tarfile.TarInfo(path)
            member.size = len(body)
            archive.addfile(member, io.BytesIO(body))
    return output.getvalue()


def image_archive_fixture(compressed=False, shared=False, oci=False):
    lower = tar_bytes([("removed.txt", b"lower-layer content" * 1000)])
    upper = tar_bytes([(".wh.removed.txt", b""), ("binary", b"runtime")])
    layers = [lower, upper, lower] if shared else [lower, upper]
    diff_ids = ["sha256:" + hashlib.sha256(layer).hexdigest() for layer in layers]
    config = json.dumps({"os": "linux", "architecture": "amd64",
                         "rootfs": {"type": "layers", "diff_ids": diff_ids}}).encode()
    config_digest = hashlib.sha256(config).hexdigest()
    config_path = "blobs/sha256/" + config_digest if oci else config_digest + ".json"
    objects = {}
    paths = []
    for i, layer in enumerate(layers):
        encoded = gzip.compress(layer, mtime=0) if compressed else layer
        path = "blobs/sha256/" + hashlib.sha256(encoded).hexdigest() if oci else f"layer{i}/layer.tar"
        objects[path] = encoded
        paths.append(path)
    objects[config_path] = config
    objects["manifest.json"] = json.dumps([{"Config": config_path, "Layers": paths}]).encode()
    metadata = {"Id": "sha256:" + config_digest, "Os": "linux", "Architecture": "amd64",
                "RootFS": {"Type": "layers", "Layers": diff_ids}}
    return objects, metadata, len(lower) + len(upper)


class ImageArchiveTests(unittest.TestCase):
    def measure(self, objects, metadata):
        return archive_image_bytes(io.BytesIO(tar_bytes(list(objects.items()))), metadata)

    def test_plain_and_gzip_layouts_count_unique_uncompressed_layers(self):
        for compressed in (False, True):
            for oci in (False, True):
                for shared in (False, True):
                    with self.subTest(compressed=compressed, oci=oci, shared=shared):
                        objects, metadata, expected = image_archive_fixture(compressed, shared, oci)
                        self.assertEqual(self.measure(objects, metadata), expected)
                        self.assertGreater(expected, len(objects["manifest.json"]))

    def test_whiteout_does_not_remove_lower_layer_bytes(self):
        objects, metadata, expected = image_archive_fixture()
        upper = objects["layer1/layer.tar"]
        self.assertGreater(expected, len(upper))
        self.assertEqual(self.measure(objects, metadata), expected)

    def test_missing_layer_is_refused(self):
        objects, metadata, _ = image_archive_fixture()
        del objects["layer0/layer.tar"]
        with self.assertRaisesRegex(RuntimeError, "missing image layer"):
            self.measure(objects, metadata)

    def test_changed_layer_bytes_are_refused(self):
        objects, metadata, _ = image_archive_fixture()
        objects["layer0/layer.tar"] += b"changed"
        with self.assertRaisesRegex(RuntimeError, "layer digest mismatch"):
            self.measure(objects, metadata)

    def test_descriptor_digest_mismatch_is_refused(self):
        objects, metadata, _ = image_archive_fixture(True, oci=True)
        path = next(iter(objects))
        objects["blobs/sha256/" + "0" * 64] = objects.pop(path)
        with self.assertRaisesRegex(RuntimeError, "descriptor digest mismatch"):
            self.measure(objects, metadata)

    def test_wrong_config_image_platform_and_rootfs_are_refused(self):
        for key, value, error in (
            ("Id", "sha256:" + "0" * 64, "config digest mismatch"),
            ("Architecture", "arm64", "platform mismatch"),
            ("RootFS", {"Type": "layers", "Layers": []}, "inspected image layer mismatch"),
        ):
            with self.subTest(key=key):
                objects, metadata, _ = image_archive_fixture()
                metadata[key] = value
                with self.assertRaisesRegex(RuntimeError, error):
                    self.measure(objects, metadata)

    def test_missing_and_ambiguous_manifests_are_refused(self):
        for manifest in (None, [], [{}, {}], [None]):
            with self.subTest(manifest=manifest):
                objects, metadata, _ = image_archive_fixture()
                if manifest is None:
                    del objects["manifest.json"]
                else:
                    objects["manifest.json"] = json.dumps(manifest).encode()
                with self.assertRaisesRegex(RuntimeError, "manifest"):
                    self.measure(objects, metadata)

    def test_missing_config_is_refused(self):
        objects, metadata, _ = image_archive_fixture()
        path = json.loads(objects["manifest.json"])[0]["Config"]
        del objects[path]
        with self.assertRaisesRegex(RuntimeError, "missing image config"):
            self.measure(objects, metadata)

    def test_invalid_metadata_is_refused(self):
        objects, metadata, _ = image_archive_fixture()
        objects["manifest.json"] = b"invalid JSON"
        with self.assertRaises(ValueError):
            self.measure(objects, metadata)

    def test_complete_members_without_tar_terminator_are_refused(self):
        objects, metadata, _ = image_archive_fixture()
        # Also exercise a final payload whose own tar terminator is all zero:
        # those bytes cannot substitute for the outer archive's terminator.
        entries = list(objects.items())
        entries.append(("extra/layer.tar", objects["layer0/layer.tar"]))
        for members in (list(objects.items()), entries):
            with self.subTest(extra_layer=len(members) > len(objects)):
                data = tar_bytes(members)
                content_end = sum(512 + ((len(body) + 511) // 512) * 512 for _, body in members)
                with self.assertRaisesRegex(RuntimeError, "truncated image archive"):
                    archive_image_bytes(io.BytesIO(data[:content_end]), metadata)

    def test_failed_decode_kills_and_reaps_export(self):
        objects, metadata, _ = image_archive_fixture()
        process = unittest.mock.Mock()
        process.stdout = io.BytesIO(b"not an archive")
        process.poll.return_value = None
        with patch("docker_image_size.subprocess.Popen", return_value=process), \
                patch("docker_image_size.threading.Timer") as timer:
            with self.assertRaisesRegex(RuntimeError, "invalid Docker image archive"):
                image_bytes(metadata)
        process.kill.assert_called_once()
        process.wait.assert_called_once_with(timeout=5)
        timer.return_value.cancel.assert_called_once()
        self.assertTrue(process.stdout.closed)

    def test_duplicate_objects_are_refused(self):
        objects, metadata, _ = image_archive_fixture()
        entries = list(objects.items()) + [("manifest.json", objects["manifest.json"])]
        with self.assertRaisesRegex(RuntimeError, "duplicate image archive object"):
            archive_image_bytes(io.BytesIO(tar_bytes(entries)), metadata)

    def test_truncated_archive_and_gzip_are_refused(self):
        objects, metadata, _ = image_archive_fixture(True)
        archive = tar_bytes(list(objects.items()))
        with self.assertRaises((RuntimeError, tarfile.TarError)):
            archive_image_bytes(io.BytesIO(archive[:1000]), metadata)
        objects["layer0/layer.tar"] = objects["layer0/layer.tar"][:-4]
        with self.assertRaises((EOFError, OSError)):
            self.measure(objects, metadata)

    def test_unsafe_paths_and_symlinks_are_refused(self):
        objects, metadata, _ = image_archive_fixture()
        objects["../escape"] = b"no extraction"
        with self.assertRaisesRegex(RuntimeError, "unsafe image archive path"):
            self.measure(objects, metadata)
        output = io.BytesIO()
        with tarfile.open(fileobj=output, mode="w") as archive:
            link = tarfile.TarInfo("manifest.json")
            link.type = tarfile.SYMTYPE
            link.linkname = "/other"
            archive.addfile(link)
        with self.assertRaisesRegex(RuntimeError, "unsupported image archive object"):
            archive_image_bytes(io.BytesIO(output.getvalue()), metadata)

    def test_metadata_and_decompression_are_bounded(self):
        objects, metadata, _ = image_archive_fixture(True)
        with patch("docker_image_size.ARCHIVE_MAX_BYTES", 1024):
            with self.assertRaisesRegex(RuntimeError, "measurement bound"):
                self.measure(objects, metadata)
        archive = tar_bytes(list(objects.items()))
        with patch("docker_image_size.ARCHIVE_MAX_BYTES", len(archive)):
            with self.assertRaisesRegex(RuntimeError, "uncompressed image exceeds measurement bound"):
                archive_image_bytes(io.BytesIO(archive), metadata)
        with patch("docker_image_size.METADATA_MAX_BYTES", 2):
            with self.assertRaisesRegex(RuntimeError, "oversized image metadata"):
                self.measure(objects, metadata)

    def test_export_is_pinned_and_cleaned_up_even_on_failure(self):
        objects, metadata, expected = image_archive_fixture(True, oci=True)
        for code in (0, 1):
            with self.subTest(code=code):
                process = unittest.mock.Mock()
                process.stdout = io.BytesIO(tar_bytes(list(objects.items())))
                process.wait.return_value = code
                process.poll.return_value = code
                with patch("docker_image_size.subprocess.Popen", return_value=process) as launch, \
                        patch("docker_image_size.threading.Timer") as timer:
                    if code == 0:
                        self.assertEqual(image_bytes(metadata), expected)
                    else:
                        with self.assertRaisesRegex(RuntimeError, "export failed"):
                            image_bytes(metadata)
                self.assertEqual(launch.call_args.args[0], ["docker", "image", "save", metadata["Id"]])
                self.assertEqual(timer.call_args.args[0], 120)
                timer.return_value.cancel.assert_called_once()
                self.assertTrue(process.stdout.closed)


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
                patch.object(smoke, "image_bytes", side_effect=lambda metadata: metadata["FixtureLayerBytes"]), \
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
        self.assertIn(self.fixture.image_id, run)
        self.assertIn("127.0.0.1::8080", run)
        self.assertNotIn("-e", run)
        self.assertNotIn("--env-file", run)
        wait = next(kwargs for args, kwargs in self.fixture.calls if args[0] == "wait")
        self.assertLessEqual(wait["timeout"], 10)

    def test_measurement_failure_never_falls_back_to_daemon_size(self):
        self.fixture.daemon_size = 1
        with patch.object(smoke, "docker", self.fixture), \
                patch.object(smoke, "image_bytes", side_effect=RuntimeError("invalid archive")):
            with self.assertRaisesRegex(RuntimeError, "invalid archive"):
                smoke.smoke("two-bot:fixture")
        self.assertFalse(any(args[0] == "run" for args, _ in self.fixture.calls))

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
