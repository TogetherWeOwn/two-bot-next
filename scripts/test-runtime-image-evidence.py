"""Offline contracts for bounded, non-deploying runtime image inspection."""

import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("evidence", ROOT / "scripts/runtime-image-evidence.py")
evidence = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(evidence)
IMAGE_ID = "sha256:" + "a" * 64
SOURCE_SHA = "b" * 40
METADATA = [{"Id": IMAGE_ID, "Architecture": "amd64", "Os": "linux", "Config": {
    "User": "two-bot", "Entrypoint": ["/home/two-bot/two-bot"],
    "Env": ["SECRET=must-not-be-recorded"], "Cmd": None,
}}]


class EvidenceTests(unittest.TestCase):
    def fixture(self, directory):
        (directory / "image-id.txt").write_text(IMAGE_ID + "\n")
        (directory / "source-sha.txt").write_text(SOURCE_SHA + "\n")

    def test_identity_mismatch_stops_before_any_container_runs(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP")) as temporary:
            directory = Path(temporary)
            self.fixture(directory)
            metadata = [{**METADATA[0], "Id": "sha256:" + "c" * 64}]
            with patch.object(evidence, "docker", return_value=subprocess.CompletedProcess([], 0, json.dumps(metadata), "")) as docker:
                with self.assertRaisesRegex(ValueError, "identity"):
                    evidence.collect("two-bot:fixture", directory)
                self.assertEqual(docker.call_count, 1)

    def test_invalid_provenance_stops_before_docker(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP")) as temporary:
            directory = Path(temporary)
            self.fixture(directory)
            (directory / "source-sha.txt").write_text("unknown\n")
            with patch.object(evidence, "docker") as docker:
                with self.assertRaises(ValueError):
                    evidence.collect("two-bot:fixture", directory)
                docker.assert_not_called()

    def test_report_is_bound_to_image_and_does_not_include_environment(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP")) as temporary:
            directory = Path(temporary)
            self.fixture(directory)
            with patch.object(evidence, "docker", return_value=subprocess.CompletedProcess([], 0, json.dumps(METADATA), "")), \
                    patch.object(evidence, "probe", return_value={"returncode": 2, "stdout": "", "stderr": "module missing", "timed_out": False, "cleanup": {"status": "removed"}}):
                report = evidence.collect("two-bot:fixture", directory)
            self.assertEqual(report["image_id"], IMAGE_ID)
            self.assertEqual(report["source_sha"], SOURCE_SHA)
            self.assertEqual(report["architecture"], "amd64")
            self.assertEqual(report["configured_user"], "two-bot")
            self.assertNotIn("SECRET", json.dumps(report))
            self.assertNotIn("accepted", report)
            self.assertEqual(report["probes"]["perl_archive_tar"]["returncode"], 2)
            self.assertEqual(report["probes"]["perl_archive_tar"]["stderr"], "module missing")

    def test_reported_pcre2_package_has_file_and_dependency_probes(self):
        for label in ["affected_package_files", "package_dependencies"]:
            with self.subTest(probe=label):
                self.assertIn("libpcre2-8-0", evidence.PROBES[label].split())

    def test_mount_probe_records_package_source_and_resolved_payload_identity(self):
        command = evidence.PROBES["mount_configuration"]
        for field in ["${binary:Package}", "${Version}", "${Architecture}",
                      "${source:Package}", "${source:Version}"]:
            with self.subTest(field=field):
                self.assertIn(field, command)
        for path in ["/usr/bin/mount", "/usr/bin/umount", "/usr/bin/nsenter",
                     "/usr/lib/x86_64-linux-gnu/libmount.so.1"]:
            with self.subTest(path=path):
                self.assertIn(path, command)
        self.assertIn("readlink -e", command)
        self.assertIn("sha256sum", command)

    def test_payload_identity_shell_preserves_hashes_and_fails_on_missing_evidence(self):
        scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP")
        self.assertTrue(scratch, "Fake tools require run-owned scratch")
        paths = ["/usr/bin/mount", "/usr/bin/umount", "/usr/bin/nsenter",
                 "/usr/lib/x86_64-linux-gnu/libmount.so.1"]
        failures = [(None, None), ("dpkg-query", None), ("sha256sum", None)]
        failures += [("readlink", path) for path in paths]
        for failed_tool, failed_path in failures:
            with self.subTest(tool=failed_tool, path=failed_path), tempfile.TemporaryDirectory(dir=scratch) as temporary:
                directory = Path(temporary)
                payload = directory / "payload with spaces"
                payload.write_bytes(b"offline payload fixture\n")
                calls_path = directory / "calls.jsonl"
                tool = (
                    f"#!{sys.executable}\n"
                    "import hashlib, json, sys\n"
                    "from pathlib import Path\n"
                    "name = Path(sys.argv[0]).name\n"
                    f"with open({str(calls_path)!r}, 'a') as calls:\n"
                    "    calls.write(json.dumps([name, *sys.argv[1:]]) + '\\n')\n"
                    f"if name == {failed_tool!r} and ({failed_path!r} is None or sys.argv[-1] == {failed_path!r}):\n"
                    "    print('fixture evidence unavailable', file=sys.stderr)\n"
                    "    sys.exit(7)\n"
                    "if name == 'dpkg-query':\n"
                    "    print('bsdutils\\t1:2.38.1-5+deb12u3\\tamd64\\tutil-linux\\t2.38.1-5+deb12u3')\n"
                    "elif name == 'readlink':\n"
                    f"    assert sys.argv[1] == '-e' and sys.argv[2] in {paths!r}\n"
                    f"    print({str(payload)!r})\n"
                    "elif name == 'sha256sum':\n"
                    f"    assert sys.argv[1:] == [{str(payload)!r}]\n"
                    "    print(hashlib.sha256(Path(sys.argv[1]).read_bytes()).hexdigest() + '  ' + sys.argv[1])\n"
                )
                for name in ["dpkg-query", "readlink", "sha256sum"]:
                    executable = directory / name
                    executable.write_text(tool)
                    executable.chmod(0o700)
                with patch.dict(os.environ, {"PATH": str(directory)}):
                    result = subprocess.run(["/bin/sh", "-c", evidence.UTIL_LINUX_IDENTITY + "; printf 'later command\\n'"], capture_output=True, text=True)
                calls = [json.loads(line) for line in calls_path.read_text().splitlines()]
                self.assertEqual(calls[0][-8:], ["bsdutils", "libblkid1", "libmount1", "libsmartcols1",
                                               "libuuid1", "mount", "util-linux", "util-linux-extra"])
                if failed_tool:
                    self.assertEqual(result.returncode, 7)
                    self.assertIn("fixture evidence unavailable", result.stderr)
                    self.assertNotIn("later command", result.stdout)
                    self.assertEqual(calls[-1][0], failed_tool)
                else:
                    digest = hashlib.sha256(payload.read_bytes()).hexdigest()
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn("bsdutils\t1:2.38.1-5+deb12u3\tamd64\tutil-linux\t2.38.1-5+deb12u3", result.stdout)
                    for path in paths:
                        self.assertIn(f"resolved\t{path}\t{payload}\n", result.stdout)
                    self.assertEqual(result.stdout.count(digest + "  " + str(payload)), 4)
                    self.assertEqual(len(calls), 9)

    def test_fixed_probe_commands_parse_without_executing(self):
        self.assertEqual(len(evidence.PROBES), 16)
        for label, command in evidence.PROBES.items():
            with self.subTest(probe=label):
                result = subprocess.run(["sh", "-n", "-c", command], capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_probe_is_isolated_and_always_cleans_its_named_container(self):
        calls = []

        def docker(*args, **kwargs):
            calls.append((args, kwargs))
            return subprocess.CompletedProcess(args, 0, "raw observation\n", "")

        with patch.object(evidence, "docker", side_effect=docker):
            result = evidence.probe(IMAGE_ID, "dpkg-query -W")
        run = calls[0][0]
        for flag in ["--read-only", "--no-healthcheck"]:
            self.assertIn(flag, run)
        for flag, value in [("--network", "none"), ("--cap-drop", "ALL"),
                            ("--security-opt", "no-new-privileges"), ("--pids-limit", "32"),
                            ("--memory", "128m"), ("--memory-swap", "128m"), ("--cpus", "0.5"),
                            ("--entrypoint", "/bin/sh")]:
            self.assertEqual(run[run.index(flag) + 1], value)
        self.assertEqual(run[-3:], (IMAGE_ID, "-c", "dpkg-query -W"))
        for forbidden in ["--publish", "--volume", "--mount", "--env", "--privileged"]:
            self.assertNotIn(forbidden, run)
        name = run[run.index("--name") + 1]
        self.assertEqual(calls[-1][0], ("rm", "--force", name))
        self.assertEqual(result["container_name"], name)
        self.assertEqual(calls[0][1]["timeout"], 20)
        self.assertEqual(result["stdout"], "raw observation\n")

    def test_timeout_is_explicit_and_still_cleans_container(self):
        with patch.object(evidence, "docker", side_effect=[subprocess.TimeoutExpired("docker", 20), subprocess.CompletedProcess([], 0, "", "")]) as docker:
            result = evidence.probe(IMAGE_ID, "sleep 1000")
        self.assertTrue(result["timed_out"])
        self.assertIsNone(result["returncode"])
        self.assertEqual(docker.call_args.args[:2], ("rm", "--force"))

    def test_cleanup_failure_is_not_hidden(self):
        with patch.object(evidence, "docker", side_effect=[subprocess.CompletedProcess([], 0, "observation", ""), subprocess.CompletedProcess([], 1, "", "cleanup refused")]):
            result = evidence.probe(IMAGE_ID, "true")
        self.assertEqual(result["stdout"], "observation")
        self.assertEqual(result["cleanup"]["status"], "failed")
        self.assertEqual(result["cleanup"]["stderr"], "cleanup refused")

    def test_precreation_failure_keeps_startup_error_and_confirms_absence(self):
        def docker(*args, **kwargs):
            if args[0] == "run":
                return subprocess.CompletedProcess(args, 125, "", "OCI runtime create failed before container creation")
            return subprocess.CompletedProcess(args, 1, "", f"Error response from daemon: No such container: {args[-1]}\n")

        with patch.object(evidence, "docker", side_effect=docker):
            result = evidence.probe(IMAGE_ID, "true")
        self.assertEqual(result["returncode"], 125)
        self.assertIn("OCI runtime", result["stderr"])
        self.assertEqual(result["cleanup"]["status"], "absent")

    def test_unrelated_missing_container_message_is_not_accepted(self):
        with patch.object(evidence, "docker", side_effect=[
            subprocess.CompletedProcess([], 125, "", "startup failed"),
            subprocess.CompletedProcess([], 1, "", "Error response from daemon: No such container: someone-elses-container"),
        ]):
            result = evidence.probe(IMAGE_ID, "true")
        self.assertEqual(result["cleanup"]["status"], "failed")

    def test_client_and_cleanup_timeouts_preserve_partial_output(self):
        with patch.object(evidence, "docker", side_effect=[
            subprocess.TimeoutExpired("docker", 20, output=b"partial observation", stderr=b"partial error"),
            subprocess.TimeoutExpired("docker rm", 30),
        ]):
            result = evidence.probe(IMAGE_ID, "true")
        self.assertEqual(result["stdout"], "partial observation")
        self.assertIn("partial error", result["stderr"])
        self.assertTrue(result["timed_out"])
        self.assertEqual(result["cleanup"]["status"], "failed")
        self.assertTrue(result["cleanup"]["timed_out"])

    def test_missing_docker_client_is_explicit_and_cleanup_is_attempted(self):
        with patch.object(evidence, "docker", side_effect=FileNotFoundError("docker unavailable")) as docker:
            result = evidence.probe(IMAGE_ID, "true")
        self.assertEqual(docker.call_count, 2)
        self.assertIsNone(result["returncode"])
        self.assertIn("docker unavailable", result["stderr"])
        self.assertEqual(result["cleanup"]["status"], "failed")

    def test_success_then_precreation_failure_retains_all_observations(self):
        calls = []

        def docker(*args, **kwargs):
            if args[:2] == ("image", "inspect"):
                return subprocess.CompletedProcess(args, 0, json.dumps(METADATA), "")
            if args[0] == "run":
                calls.append(args)
                code = 125 if len(calls) == 2 else 0
                return subprocess.CompletedProcess(args, code, "first observation" if len(calls) == 1 else "", "startup failed" if code else "")
            if len(calls) >= 2 and args[-1] == calls[1][calls[1].index("--name") + 1]:
                return subprocess.CompletedProcess(args, 1, "", f"Error response from daemon: No such container: {args[-1]}\n")
            return subprocess.CompletedProcess(args, 0, "", "")

        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP")) as temporary:
            directory = Path(temporary)
            self.fixture(directory)
            with patch.object(evidence, "docker", side_effect=docker), patch.object(sys, "argv", ["evidence", "two-bot:fixture", str(directory)]):
                evidence.main()
            report = json.loads((directory / "runtime-image-evidence.json").read_text())
        self.assertEqual(report["probes"]["installed_packages"]["stdout"], "first observation")
        self.assertEqual(report["probes"]["affected_package_files"]["returncode"], 125)
        self.assertEqual(report["probes"]["affected_package_files"]["cleanup"]["status"], "absent")
        self.assertEqual(len(report["probes"]), 16)
        self.assertTrue(report["complete"])

    def test_cleanup_refusal_saves_partial_report_then_fails_and_stops(self):
        responses = [subprocess.CompletedProcess([], 0, json.dumps(METADATA), ""),
                     subprocess.CompletedProcess([], 0, "first observation", ""),
                     subprocess.CompletedProcess([], 0, "", ""),
                     subprocess.CompletedProcess([], 125, "", "startup failed"),
                     subprocess.CompletedProcess([], 1, "", "cleanup refused")]
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP")) as temporary:
            directory = Path(temporary)
            self.fixture(directory)
            with patch.object(evidence, "docker", side_effect=responses) as docker, patch.object(sys, "argv", ["evidence", "two-bot:fixture", str(directory)]):
                with self.assertRaisesRegex(SystemExit, "cleanup"):
                    evidence.main()
            report = json.loads((directory / "runtime-image-evidence.json").read_text())
        self.assertEqual(docker.call_count, 5)
        self.assertFalse(report["complete"])
        self.assertEqual(len(report["probes"]), 2)
        self.assertEqual(report["probes"]["installed_packages"]["stdout"], "first observation")
        self.assertEqual(report["probes"]["affected_package_files"]["stderr"], "startup failed")
        self.assertEqual(report["probes"]["affected_package_files"]["cleanup"]["stderr"], "cleanup refused")

    def test_cleanup_timeout_and_transport_failures_persist_exact_owned_name(self):
        for failure in [subprocess.TimeoutExpired("docker rm", 30),
                        OSError("daemon transport unavailable"),
                        subprocess.CompletedProcess([], 1, "", "Cannot connect to the Docker daemon")]:
            with self.subTest(failure=failure), tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP")) as temporary:
                directory = Path(temporary)
                self.fixture(directory)
                responses = [subprocess.CompletedProcess([], 0, json.dumps(METADATA), ""),
                             subprocess.CompletedProcess([], 0, "first observation", ""),
                             subprocess.CompletedProcess([], 0, "", ""),
                             subprocess.CompletedProcess([], 0, "second observation", ""),
                             failure]
                with patch.object(evidence, "docker", side_effect=responses) as docker, patch.object(sys, "argv", ["evidence", "two-bot:fixture", str(directory)]):
                    with self.assertRaisesRegex(SystemExit, "cleanup"):
                        evidence.main()
                report = json.loads((directory / "runtime-image-evidence.json").read_text())
                run = docker.call_args_list[3].args
                name = run[run.index("--name") + 1]
                self.assertRegex(name, r"^two-bot-inspect-[0-9a-f]{32}$")
                self.assertEqual(docker.call_args_list[4].args, ("rm", "--force", name))
                result = report["probes"]["affected_package_files"]
                self.assertEqual(result["container_name"], name)
                self.assertEqual(result["stdout"], "second observation")
                self.assertEqual(result["cleanup"]["status"], "failed")
                self.assertEqual(result["cleanup"]["timed_out"], isinstance(failure, subprocess.TimeoutExpired))
                self.assertTrue(result["cleanup"]["stderr"])
                self.assertEqual(report["probes"]["installed_packages"]["stdout"], "first observation")
                self.assertNotEqual(report["probes"]["installed_packages"]["container_name"], name)
                self.assertEqual(report["source_sha"], SOURCE_SHA)
                self.assertEqual(report["image_id"], IMAGE_ID)
                self.assertFalse(report["complete"])
                self.assertEqual(docker.call_count, 5)

    def test_non_utf8_subprocess_output_retains_probe_and_failed_cleanup(self):
        scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP")
        self.assertTrue(scratch, "Fake Docker requires PAPERCLIP_RUN_SCRATCH_DIR or RUNNER_TEMP")
        with tempfile.TemporaryDirectory(dir=scratch) as temporary:
            directory = Path(temporary)
            self.fixture(directory)
            executable = directory / "docker"
            calls_path = directory / "calls.jsonl"
            executable.write_text(
                f"#!{sys.executable}\n"
                "import json, sys\n"
                f"with open({str(calls_path)!r}, 'a') as calls:\n"
                "    calls.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                "if sys.argv[1:3] == ['image', 'inspect']:\n"
                f"    print({json.dumps(METADATA)!r})\n"
                "elif sys.argv[1] == 'run':\n"
                f"    if sys.argv[-1] == {evidence.PROBES['affected_files']!r}:\n"
                "        sys.stdout.buffer.write(b'/tmp/minizip-\\xff\\n')\n"
                "        sys.stderr.buffer.write(b'find warning: \\xfe\\n')\n"
                "        sys.exit(2)\n"
                "    sys.stdout.buffer.write(b'prior observation: \\xef\\xbf\\xbd\\n')\n"
                "elif sys.argv[1] == 'rm':\n"
                f"    calls = [json.loads(line) for line in open({str(calls_path)!r})]\n"
                "    if sum(call[0] == 'run' for call in calls) == 5:\n"
                "        sys.stdout.buffer.write(b'cleanup partial: \\xff\\n')\n"
                "        sys.stderr.buffer.write(b'cleanup refused: \\xfe\\n')\n"
                "        sys.exit(1)\n"
            )
            executable.chmod(0o700)
            # Restrict PATH to the fake executable: never reach a real daemon.
            with patch.dict(os.environ, {"PATH": str(directory)}), patch.object(sys, "argv", ["evidence", "two-bot:fixture", str(directory)]):
                with self.assertRaisesRegex(SystemExit, "cleanup"):
                    evidence.main()
            report = json.loads((directory / "runtime-image-evidence.json").read_text())
            calls = [json.loads(line) for line in calls_path.read_text().splitlines()]
        self.assertEqual(len(calls), 11)
        self.assertEqual(len(report["probes"]), 5)
        self.assertFalse(report["complete"])
        self.assertIn("cleanup", report["collection_error"])
        for key in list(evidence.PROBES)[:4]:
            prior = report["probes"][key]
            self.assertEqual(prior["returncode"], 0)
            self.assertEqual(prior["stdout"], "prior observation: \ufffd\n")
            self.assertEqual(prior["lossy_decoding"], [])
            self.assertFalse(prior["timed_out"])
            self.assertEqual(prior["cleanup"]["status"], "removed")
            self.assertEqual(prior["cleanup"]["lossy_decoding"], [])
        result = report["probes"]["affected_files"]
        run = calls[-2]
        name = run[run.index("--name") + 1]
        self.assertRegex(name, r"^two-bot-inspect-[0-9a-f]{32}$")
        self.assertEqual(calls[-1], ["rm", "--force", name])
        self.assertEqual(result["container_name"], name)
        self.assertNotIn(name, [report["probes"][key]["container_name"] for key in list(evidence.PROBES)[:4]])
        self.assertEqual(result["command"], evidence.PROBES["affected_files"])
        self.assertEqual(result["returncode"], 2)
        self.assertEqual(result["stdout"], "/tmp/minizip-\ufffd\n")
        self.assertEqual(result["stderr"], "find warning: \ufffd\n")
        self.assertEqual(result["lossy_decoding"], ["stdout", "stderr"])
        self.assertFalse(result["timed_out"])
        cleanup = result["cleanup"]
        self.assertEqual(cleanup["status"], "failed")
        self.assertEqual(cleanup["returncode"], 1)
        self.assertEqual(cleanup["stdout"], "cleanup partial: \ufffd\n")
        self.assertEqual(cleanup["stderr"], "cleanup refused: \ufffd\n")
        self.assertEqual(cleanup["lossy_decoding"], ["stdout", "stderr"])
        self.assertFalse(cleanup["timed_out"])
        self.assertEqual(report["source_sha"], SOURCE_SHA)
        self.assertEqual(report["image_id"], IMAGE_ID)

    def test_decoding_loss_marks_only_invalid_utf8_streams(self):
        for stdout, stderr, lossy in [
            (b"valid \xef\xbf\xbd\r\n", b"valid \xef\xbf\xbd\n", []),
            (b"invalid \xff\n", b"valid \xef\xbf\xbd\n", ["stdout"]),
            (b"valid \xef\xbf\xbd\n", b"incomplete \xe2\x82", ["stderr"]),
        ]:
            for timed_out in [False, True]:
                with self.subTest(lossy=lossy, timed_out=timed_out):
                    response = subprocess.TimeoutExpired("docker", 20, output=stdout, stderr=stderr) if timed_out else subprocess.CompletedProcess([], 2, stdout, stderr)
                    with patch.object(evidence, "docker", side_effect=[response]):
                        result = evidence.observation("run", timeout=20)
                    self.assertEqual(result["stdout"], stdout.decode("utf-8", errors="replace"))
                    suffix = "\nDocker command exceeded 20 seconds" if timed_out else ""
                    self.assertEqual(result["stderr"], stderr.decode("utf-8", errors="replace") + suffix)
                    self.assertEqual(result["lossy_decoding"], lossy)
                    self.assertEqual(result["returncode"], None if timed_out else 2)
                    self.assertEqual(result["timed_out"], timed_out)
                    self.assertEqual(json.loads(json.dumps(result)), result)

    def test_lossy_timeout_output_persists_owned_name_and_cleanup_failure(self):
        responses = [subprocess.CompletedProcess([], 0, json.dumps(METADATA), ""),
                     subprocess.CompletedProcess([], 0, b"first observation", b""),
                     subprocess.CompletedProcess([], 0, b"", b""),
                     subprocess.TimeoutExpired("docker", 20, output=b"partial \xe2\x82", stderr=b"valid \xef\xbf\xbd"),
                     subprocess.TimeoutExpired("docker rm", 30, output=b"valid \xef\xbf\xbd", stderr=b"cleanup \xff")]
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP")) as temporary:
            directory = Path(temporary)
            self.fixture(directory)
            with patch.object(evidence, "docker", side_effect=responses) as docker, patch.object(sys, "argv", ["evidence", "two-bot:fixture", str(directory)]):
                with self.assertRaisesRegex(SystemExit, "cleanup"):
                    evidence.main()
            report = json.loads((directory / "runtime-image-evidence.json").read_text())
        result = report["probes"]["affected_package_files"]
        run = docker.call_args_list[3].args
        name = run[run.index("--name") + 1]
        self.assertEqual(result["container_name"], name)
        self.assertEqual(docker.call_args_list[4].args, ("rm", "--force", name))
        self.assertIsNone(result["returncode"])
        self.assertTrue(result["timed_out"])
        self.assertEqual(result["stdout"], "partial �")
        self.assertEqual(result["stderr"], "valid �\nDocker command exceeded 20 seconds")
        self.assertEqual(result["lossy_decoding"], ["stdout"])
        cleanup = result["cleanup"]
        self.assertEqual(cleanup["status"], "failed")
        self.assertIsNone(cleanup["returncode"])
        self.assertTrue(cleanup["timed_out"])
        self.assertEqual(cleanup["stdout"], "valid �")
        self.assertEqual(cleanup["stderr"], "cleanup �\nDocker command exceeded 30 seconds")
        self.assertEqual(cleanup["lossy_decoding"], ["stderr"])
        self.assertEqual(report["probes"]["installed_packages"]["stdout"], "first observation")
        self.assertEqual(len(report["probes"]), 2)
        self.assertFalse(report["complete"])
        self.assertEqual(docker.call_count, 5)

    def test_ci_retains_evidence_after_failed_gates_without_changing_gates(self):
        workflow = (ROOT / ".github/workflows/supply-chain.yml").read_text()
        self.assertIn("python3 scripts/test-runtime-image-evidence.py", workflow)
        diagnostic = workflow.index("name: Collect exact-image applicability evidence")
        self.assertGreater(diagnostic, workflow.index("name: Gate runtime image"))
        self.assertLess(diagnostic, workflow.index("name: Retain SBOMs"))
        step = workflow[diagnostic:workflow.index("name: Retain SBOMs")]
        self.assertIn("!cancelled() && steps.inventory.outcome == 'success'", step)
        self.assertIn('python3 scripts/runtime-image-evidence.py "$IMAGE" sbom', step)
        self.assertIn('python3 scripts/runtime-image-evidence.py "$IMAGE" sbom || status=$?', step)
        self.assertIn("sha256sum runtime-image-evidence.json", step)
        self.assertLess(step.index("sha256sum runtime-image-evidence.json"), step.index('exit "$status"'))
        self.assertEqual(workflow.count("ignore-unfixed: false"), 2)
        self.assertEqual(workflow.count("exit-code: '1'"), 2)


if __name__ == "__main__":
    unittest.main()
