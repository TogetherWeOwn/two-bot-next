"""Inactive CLI integration with synthetic HTTP responses; no live metadata."""
import contextlib
import copy
import http.client
import io
import json
import os
import runpy
import subprocess
import sys
import tempfile
import unittest
import yaml
from pathlib import Path
from unittest import mock

from test_attestation_policy import ATTESTATION, BODY, metadata

import attestation_transport as transport
import base_attestation_gate as gate


class SyntheticOpener:
    def __init__(self, data, mutation=None):
        self.pr = copy.deepcopy(data["pull_request"])
        self.calls = []
        self.mutation = mutation

    def open(self, request, timeout):
        self.calls.append((request.full_url, request.get_method(),
                           request.get_header("Authorization"), timeout))
        call = len(self.calls)
        if self.mutation == "transport" and call == 5:
            raise OSError("SENTINEL-TOKEN")
        if "/files?" in request.full_url:
            value = [{"status": "modified", "filename": "sql/change.sql"}]
        elif "/pulls/" in request.full_url:
            value = self.pr
        else:
            value = {"object": {"sha": "a" * 40}}
        if call == self.mutation == 4:
            value = copy.deepcopy(value)
            value["head"]["sha"] = "c" * 40
        if self.mutation in ("base-first", "base-last") and call == (2 if self.mutation == "base-first" else 5):
            value = {"object": {"sha": "c" * 40}}
        raw = json.dumps(value).encode()
        if self.mutation == "malformed" and call == 3:
            raw = b'[{"status":"modified","filename":"sql/change.sql","filename":"sql/change.sql"}]'
        length = len(raw) + (1 if self.mutation == "truncated" and call == 3 else 0)
        sock = mock.Mock()
        sock.makefile.return_value = io.BytesIO(
            b"HTTP/1.1 200 OK\r\nContent-Length: " + str(length).encode() + b"\r\n\r\n" + raw)
        response = http.client.HTTPResponse(sock)
        response.begin()
        return response


class AttestationIntegration(unittest.TestCase):
    def test_sparse_fixture_loads_intake_from_the_verified_checkout(self):
        root = Path(__file__).resolve().parents[2]
        fixture = yaml.safe_load((root / "scripts/ci/fixtures/base-attestation.yml").read_text())
        checkout_step = fixture["jobs"]["metadata"]["steps"][0]["with"]
        source = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root,
                                         text=True, timeout=10).strip()
        with tempfile.TemporaryDirectory() as directory:
            checkout = Path(directory) / "checkout"
            subprocess.run(["git", "clone", "--quiet", "--shared", "--no-checkout", str(root), str(checkout)],
                           check=True, timeout=20)
            subprocess.run(["git", "sparse-checkout", "set", "--no-cone", "--stdin"], cwd=checkout,
                           input=checkout_step["sparse-checkout"], text=True, check=True, timeout=10)
            subprocess.run(["git", "checkout", "--quiet", "--detach", source], cwd=checkout,
                           check=True, timeout=20)
            self.assertFalse((checkout / "scripts" / "bot_promotion.py").exists())
            program = """
import sys
from pathlib import Path

sys.path.insert(0, str(Path.cwd() / ".github/scripts"))
from base_attestation_gate import decode_legacy_json

print(Path(sys.modules[decode_legacy_json.__module__].__file__).resolve())
"""
            actual = subprocess.check_output([sys.executable, "-I", "-B", "-c", program],
                                             cwd=checkout, text=True, timeout=10).strip()
            self.assertEqual(Path(actual), (checkout / "scripts/script_intake.py").resolve())

    def run_gate(self, body, mutation=None):
        data = metadata(body)
        opener = SyntheticOpener(data, mutation)
        output, errors = io.StringIO(), io.StringIO()
        env = {"GITHUB_EVENT_NAME": "pull_request_target", "GITHUB_REPOSITORY": "org/repo",
               "GITHUB_SHA": "a" * 40, "GH_TOKEN": "SENTINEL-TOKEN"}
        with tempfile.TemporaryDirectory() as directory:
            event = Path(directory) / "event.json"
            event.write_text(json.dumps(data), encoding="utf-8")
            with mock.patch.dict(os.environ, env, clear=True), \
                    mock.patch.object(transport, "no_redirect_opener", return_value=opener), \
                    contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                result = gate.main(["--event", str(event)])
        self.assertNotIn("SENTINEL-TOKEN", output.getvalue() + errors.getvalue())
        return result, output.getvalue(), errors.getvalue(), opener.calls

    def test_rendered_bootstrap_synthetic_identity_and_freshness(self):
        root = Path(__file__).resolve().parents[2]
        fixture = yaml.safe_load((root / "scripts/ci/fixtures/base-attestation.yml").read_text())
        script = compile(fixture["jobs"]["metadata"]["steps"][1]["run"], "bootstrap", "exec")
        cases = (("matching", "a" * 40, "a" * 40, "a" * 40, None, 0, 5, True),
                 ("checkout mismatch", "a" * 40, "b" * 40, "a" * 40, None, None, 0, False),
                 ("malformed run", "invalid", "a" * 40, "a" * 40, None, None, 0, False),
                 ("event divergence", "a" * 40, "a" * 40, "c" * 40, None, 1, 0, True),
                 ("malformed event", "a" * 40, "a" * 40, "invalid", None, 1, 0, True),
                 ("first base advance", "a" * 40, "a" * 40, "a" * 40, "base-first", 1, 2, True),
                 ("last base advance", "a" * 40, "a" * 40, "a" * 40, "base-last", 1, 5, True),
                 ("stale snapshot", "a" * 40, "a" * 40, "a" * 40, 4, 1, 4, True))
        for name, run_sha, checkout_sha, event_sha, mutation, result, count, invoked in cases:
            with self.subTest(case=name), tempfile.TemporaryDirectory() as directory:
                data = metadata(BODY + ATTESTATION)
                data["pull_request"]["base"]["sha"] = event_sha
                event = Path(directory) / "event.json"
                event.write_text(json.dumps(data), encoding="utf-8")
                opener = SyntheticOpener(data, mutation)
                env = {"GITHUB_EVENT_NAME": "pull_request_target", "GITHUB_REPOSITORY": "org/repo",
                       "GITHUB_SHA": run_sha, "GITHUB_WORKSPACE": str(root),
                       "GITHUB_EVENT_PATH": str(event), "GH_TOKEN": "SENTINEL-TOKEN"}
                output, errors = io.StringIO(), io.StringIO()
                with mock.patch.dict(os.environ, env, clear=True), \
                        mock.patch("subprocess.check_output", return_value=checkout_sha + "\n"), \
                        mock.patch.object(transport, "no_redirect_opener", return_value=opener), \
                        mock.patch.object(runpy, "run_path", wraps=runpy.run_path) as run, \
                        mock.patch.object(sys, "path", list(sys.path)), \
                        mock.patch.object(sys, "argv", list(sys.argv)), \
                        contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                    with self.assertRaises(SystemExit) as exit_status:
                        exec(script, {})
                self.assertEqual(run.call_count, int(invoked))
                if invoked:
                    self.assertEqual(exit_status.exception.code, result)
                    self.assertEqual(errors.getvalue(), "" if result == 0 else gate.REFUSAL + "\n")
                else:
                    self.assertIsInstance(exit_status.exception.code, str)
                self.assertEqual(len(opener.calls), count)
                self.assertNotIn("SENTINEL-TOKEN", output.getvalue() + errors.getvalue())

    def test_attested_change_passes_through_all_components(self):
        result, output, errors, calls = self.run_gate(BODY + ATTESTATION)
        self.assertEqual(result, 0)
        self.assertEqual(errors, "")
        self.assertEqual(json.loads(output), {"head_sha": "b" * 40, "base_sha": "a" * 40,
                                             "file_count": 1, "path_count": 1})
        prefix = "https://api.github.com/repos/org/repo/"
        self.assertEqual([item[0] for item in calls], [prefix + route for route in (
            "pulls/1", "git/ref/heads/main", "pulls/1/files?per_page=100&page=1",
            "pulls/1", "git/ref/heads/main")])
        for _, method, authorization, timeout in calls:
            self.assertEqual((method, authorization, timeout),
                             ("GET", "Bearer SENTINEL-TOKEN", transport.TIMEOUT_SECONDS))

    def test_denied_stale_malformed_truncated_and_transport_fail_closed(self):
        for body, mutation, count in ((BODY, None, 3), (BODY + ATTESTATION, 4, 4),
                                      (BODY + ATTESTATION, "malformed", 3),
                                      (BODY + ATTESTATION, "truncated", 3),
                                      (BODY + ATTESTATION, "transport", 5)):
            with self.subTest(mutation=mutation, body=body == BODY):
                result, output, errors, calls = self.run_gate(body, mutation)
                self.assertEqual((result, output, errors), (1, "", gate.REFUSAL + "\n"))
                self.assertEqual(len(calls), count)


if __name__ == "__main__":
    unittest.main()
