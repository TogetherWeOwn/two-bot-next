"""Protected, failing-first acceptance for the production readiness body cap.

Run: python3 -m unittest discover -s scripts/ci \
    -p 'test_production_readyz_body_cap_acceptance.py' -v

The cap and refusal vocabulary mirror staging_probe's 64 KiB contract. These
expectations are independent of production constants. No network or credentials:
CLI fixtures are local files; the actual workflow gate runs with a fake curl.
The Builder must not edit this file to make the implementation pass.
"""

import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/production_deploy.py"
CAP = 65_536
SHA = "0123456789abcdef0123456789abcdef01234567"
BUILD_ID = "700-1"
OVER_CAP = "/readyz answered over the body cap\n"
PASS = f"gateway ready, revision matches the SHA, build {BUILD_ID}\n"


def report(revision=SHA, build_id=BUILD_ID, **extra):
    return json.dumps({
        "components": [["process", "ready"], ["gateway", "ready"],
                       ["database", "ready"], ["token_invalid", "ready"]],
        "jobs": {}, "build_revision": revision, "build_id": build_id, **extra,
    }, ensure_ascii=False).encode("utf-8")


def padded(body, size):
    if len(body) > size:
        raise ValueError("test fixture does not fit")
    return body + b" " * (size - len(body))


class ReadyzBodyCapAcceptance(unittest.TestCase):
    def judge(self, body, status="200", mode="deploy"):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            path = Path(tmp) / "readyz.json"
            path.write_bytes(body)
            return subprocess.run(
                [sys.executable, str(SCRIPT), "readyz", "--body", str(path),
                 "--status", status, "--mode", mode, "--sha", SHA,
                 "--build-id", BUILD_ID], capture_output=True, timeout=10,
            )

    def assert_verdict(self, body, code, message, **kwargs):
        result = self.judge(body, **kwargs)
        self.assertEqual(result.returncode, code, result.stderr.decode())
        self.assertEqual(result.stderr, b"")
        self.assertEqual(result.stdout, message.encode())

    def test_exact_cap_valid_json_passes_in_deploy_and_rollback(self):
        for mode in ("deploy", "rollback"):
            with self.subTest(mode=mode):
                self.assert_verdict(padded(report(), CAP), 0, PASS, mode=mode)

    def test_cap_plus_one_valid_json_refuses_without_echo(self):
        # A valid document plus whitespace catches silent truncation at CAP.
        for status in ("200", "503"):
            for mode in ("deploy", "rollback"):
                with self.subTest(status=status, mode=mode):
                    self.assert_verdict(padded(report(), CAP + 1), 1, OVER_CAP,
                                        status=status, mode=mode)

    def test_cap_is_bytes_not_unicode_characters(self):
        body = report(note="é" * (CAP // 2))
        self.assertGreater(len(body), CAP)
        self.assertLess(len(body.decode("utf-8")), CAP)
        self.assert_verdict(body, 1, OVER_CAP)

    def test_over_cap_hostile_and_undecodable_bodies_have_only_fixed_refusal(self):
        marker = b"REMOTE-MARKER-NEVER-ECHO\n::error::forged\r\x1b"
        cases = (
            padded(marker, CAP + 1),
            padded(b"\xff" + marker, CAP + 1),
            b"[" * (CAP + 1),
            padded(b'{"repeated":1,"repeated":2}', CAP + 1),
        )
        for index, body in enumerate(cases):
            with self.subTest(case=index):
                self.assert_verdict(body, 1, OVER_CAP)

    def test_under_cap_duplicate_and_depth_verdicts_are_unchanged(self):
        cases = (
            (b'{"a":1,"\\u0061":2}', "/readyz carries duplicate JSON object keys\n"),
            (b'{"payload":' + b"[" * 101 + b"]" * 101 + b"}",
             "/readyz JSON is too deeply nested\n"),
            (b"[" * 10_000, "/readyz JSON is too deeply nested\n"),
            (b"not json", "/readyz did not return JSON\n"),
            (b"[]", "/readyz JSON is not an object\n"),
        )
        for body, message in cases:
            for mode in ("deploy", "rollback"):
                with self.subTest(message=message, mode=mode):
                    self.assertLess(len(body), CAP)
                    self.assert_verdict(body, 1, message, mode=mode)

    def test_under_cap_identity_and_gateway_verdicts_are_unchanged(self):
        cases = (
            (report(revision="f" * 40), "deploy", "200", 1,
             "build_revision does not match the SHA\n"),
            (report(build_id="699-1"), "deploy", "200", 1,
             "build_id 699-1 is not this run's build (700-1): "
             "a previous container still serves this SHA\n"),
            (report(build_id="699-1"), "rollback", "200", 0,
             "gateway ready, revision matches the SHA, build 699-1\n"),
            (report(revision="unknown", build_id="unknown"), "rollback", "200", 0,
             "pre-stamp version: build identity unknown, revision not verifiable "
             "(recorded; rollback not failed)\n"),
            (report(revision="unknown", build_id="unknown"), "deploy", "200", 1,
             "build_revision is unknown: this build was not stamped\n"),
            (report(revision="REMOTE\n::error::forged"), "deploy", "200", 1,
             "/readyz build_revision has an unexpected shape\n"),
            (report(build_id="REMOTE\n::error::forged"), "deploy", "200", 1,
             "/readyz build_id has an unexpected shape\n"),
            (report(), "deploy", "503", 1,
             "gateway not ready: /readyz is not 200 with gateway ready; roll back\n"),
            (report(components=[["process", "ready"], ["gateway", "down"]]),
             "deploy", "200", 1,
             "gateway not ready: /readyz is not 200 with gateway ready; roll back\n"),
        )
        for body, mode, status, code, message in cases:
            with self.subTest(mode=mode, status=status, message=message):
                self.assertLess(len(body), CAP)
                self.assert_verdict(body, code, message, mode=mode, status=status)

    def observe_read(self, body):
        # Spy on real file I/O, not a capped fake: an unbounded implementation
        # receives the whole file and fails the budget assertion behaviorally.
        sys.path.insert(0, str(ROOT / "scripts"))
        try:
            spec = importlib.util.spec_from_file_location("production_cap_subject", SCRIPT)
            subject = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(subject)
        finally:
            sys.path.pop(0)
        reads = []
        real_open = io.open

        class RecordingFile:
            def __init__(self, handle):
                self.handle = handle

            def __enter__(self):
                self.handle.__enter__()
                return self

            def __exit__(self, *args):
                return self.handle.__exit__(*args)

            def read(self, size=-1):
                data = self.handle.read(size)
                reads.append((size, len(data.encode("utf-8") if isinstance(data, str) else data)))
                return data

            def __getattr__(self, name):
                return getattr(self.handle, name)

        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            path = Path(tmp) / "readyz.json"
            path.write_bytes(body)

            def recording_open(file, *args, **kwargs):
                handle = real_open(file, *args, **kwargs)
                return RecordingFile(handle) if Path(file) == path else handle

            args = SimpleNamespace(body=str(path), status="200", mode="deploy",
                                   sha=SHA, build_id=BUILD_ID)
            out, err = io.StringIO(), io.StringIO()
            with patch("io.open", side_effect=recording_open), \
                    patch("builtins.open", side_effect=recording_open), \
                    patch.object(subject.json, "loads", wraps=json.loads) as parse, \
                    contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                code = subject.readyz(args)
                parses = parse.call_count
        return code, out.getvalue(), err.getvalue(), reads, parses

    def test_reader_consumes_at_most_cap_plus_one_bytes(self):
        code, out, err, reads, _ = self.observe_read(padded(report(), CAP * 8))
        self.assertTrue(reads, "the file read must be observable")
        self.assertLessEqual(sum(count for _, count in reads), CAP + 1,
                             "readyz consumed bytes beyond the sentinel")
        self.assertTrue(all(0 < size <= CAP + 1 for size, _ in reads),
                        f"every read needs a bounded byte budget: {reads}")
        self.assertEqual((code, out, err), (1, OVER_CAP, ""))

    def test_over_cap_is_refused_before_json_parsing(self):
        code, out, err, _, parses = self.observe_read(padded(report(), CAP + 1))
        self.assertEqual(parses, 0, "over-cap bytes reached the JSON decoder")
        self.assertEqual((code, out, err), (1, OVER_CAP, ""))


def gate_script():
    lines = (ROOT / ".github/workflows/deploy-production.yml").read_text().splitlines()
    start = lines.index("      - name: Gate on stable /health + /readyz gateway-ready")
    start = next(i for i in range(start, len(lines)) if lines[i] == "        run: |") + 1
    end = start
    while end < len(lines) and (not lines[end].strip() or lines[end].startswith("          ")):
        end += 1
    return textwrap.dedent("\n".join(lines[start:end]))


class GateCurlCapAcceptance(unittest.TestCase):
    def gate_calls(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            root = Path(tmp)
            work = root / "work"
            work.mkdir()
            (root / "tooling").symlink_to(ROOT, target_is_directory=True)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            curl = bin_dir / "curl"
            curl.write_text(f"#!{sys.executable}\n" + textwrap.dedent('''\
                import json, os, pathlib, sys
                args = sys.argv[1:]
                with open(os.environ["CURL_CALLS"], "a") as log:
                    log.write(json.dumps(args) + "\\n")
                output = args[args.index("-o") + 1]
                pathlib.Path(output).write_bytes(pathlib.Path(os.environ["BODY_FIXTURE"]).read_bytes())
                print("200", end="")
                '''))
            curl.chmod(0o755)
            sleep = bin_dir / "sleep"
            sleep.write_text("#!/bin/sh\nexit 0\n")
            sleep.chmod(0o755)
            fixture = root / "body.json"
            fixture.write_bytes(padded(report(), CAP))
            calls = root / "calls.jsonl"
            env = {
                "PATH": str(bin_dir) + os.pathsep + os.defpath,
                "RUNNER_TEMP": str(root), "GITHUB_STEP_SUMMARY": str(root / "summary"),
                "PRODUCTION_URL": "https://fixture.invalid", "SHA": SHA, "MODE": "deploy",
                "GITHUB_RUN_ID": "700", "GITHUB_RUN_ATTEMPT": "1",
                "CURL_CALLS": str(calls), "BODY_FIXTURE": str(fixture),
                "PYTHONDONTWRITEBYTECODE": "1",
            }
            result = subprocess.run(["bash", "-c", gate_script()], cwd=work, env=env,
                                    capture_output=True, text=True, timeout=20)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            return [json.loads(line) for line in calls.read_text().splitlines()]

    def assert_capped_calls(self, endpoint):
        calls = [args for args in self.gate_calls() if args[-1] == "https://fixture.invalid" + endpoint]
        self.assertEqual(len(calls), 19, "first pass plus 18 stable polls must exercise the curl")
        for args in calls:
            # Inspect the executed argv, so a commented-out flag cannot pass.
            flags = [arg for arg in args if arg == "--max-filesize" or arg.startswith("--max-filesize=")]
            self.assertEqual(len(flags), 1, f"{endpoint} curl must enforce exactly one body cap")
            flag = flags[0]
            size = flag.split("=", 1)[1] if "=" in flag else args[args.index(flag) + 1]
            self.assertEqual(size, str(CAP), "curl and readyz need the same byte cap")

    def test_health_curl_carries_the_body_cap_on_every_poll(self):
        self.assert_capped_calls("/health")

    def test_readyz_curl_carries_the_body_cap_on_every_poll(self):
        self.assert_capped_calls("/readyz")


if __name__ == "__main__":
    unittest.main()
