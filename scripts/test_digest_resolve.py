#!/usr/bin/env python3
"""Offline resolver fixtures; never access a live endpoint or real gh."""

import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import MagicMock, patch
import urllib.error

import digest_resolve as resolve

LIVE = "a" * 40
DEPLOYED = "b" * 40
AUTOMATIC = "c" * 40


class DigestResolveTest(unittest.TestCase):
    def ready(self, body, status=200):
        response = MagicMock()
        response.__enter__.return_value.status = status
        response.__enter__.return_value.read.return_value = body
        return response

    def test_readyz_accepts_200_and_503_full_revisions(self):
        for status in (200, 503):
            with self.subTest(status=status), patch.object(resolve.urllib.request, "urlopen") as opened:
                body = json.dumps({"build_revision": LIVE}).encode()
                if status == 503:
                    opened.side_effect = urllib.error.HTTPError("https://example.invalid/readyz", 503, "unready", {}, io.BytesIO(body))
                else:
                    opened.return_value = self.ready(body)
                self.assertEqual(resolve.live_revision("https://example.invalid/"), LIVE)
                request = opened.call_args.args[0]
                self.assertEqual(request.full_url, "https://example.invalid/readyz")
                self.assertEqual(request.get_method(), "GET")
                self.assertEqual(opened.call_args.kwargs["timeout"], 10)

    def test_readyz_requires_https_origin_without_credentials(self):
        for url in ("http://example.invalid", "", "https:///empty", "https://u:p@example.invalid", "https://example.invalid?q=x", "https://example.invalid#fragment"):
            with self.subTest(url=url), patch.object(resolve.urllib.request, "urlopen") as opened:
                opened.return_value = self.ready(json.dumps({"build_revision": LIVE}).encode())
                self.assertEqual(resolve.live_revision(url), "")
                opened.assert_not_called()

    def test_readyz_rejects_malformed_body_revision_and_status(self):
        for body in (b"not json", b"[]", b"null", b"{}", b'{"build_revision":1}', b'{"build_revision":"abc"}', json.dumps({"build_revision": "A" * 40}).encode()):
            with self.subTest(body=body), patch.object(resolve.urllib.request, "urlopen", return_value=self.ready(body)):
                self.assertEqual(resolve.live_revision("https://example.invalid"), "")
        with patch.object(resolve.urllib.request, "urlopen", return_value=self.ready(json.dumps({"build_revision": LIVE}).encode(), 201)):
            self.assertEqual(resolve.live_revision("https://example.invalid"), "")
        for error in (urllib.error.URLError("offline"), urllib.error.HTTPError("https://example.invalid", 403, "forbidden", {}, None)):
            with patch.object(resolve.urllib.request, "urlopen", side_effect=error):
                self.assertEqual(resolve.live_revision("https://example.invalid"), "")

    def recorded(self, ident, sha=DEPLOYED):
        return {"id": ident, "sha": sha, "description": "2026.10.21.1+" + sha[:7]}

    def test_fallback_ignores_automatic_and_unsuccessful_recorded_rows(self):
        batch = [{"id": 1, "sha": AUTOMATIC, "description": ""}, self.recorded(2), self.recorded(3)]
        for state in ("failure", "pending", "inactive"):
            with self.subTest(state=state), patch.object(resolve.subprocess, "check_output") as gh:
                gh.side_effect = [json.dumps(batch), json.dumps([{"state": state}]), json.dumps([{"state": "success"}])]
                self.assertEqual(resolve.newest_success_deployment("o/r"), DEPLOYED)
                calls = [call.args[0] for call in gh.call_args_list]
                self.assertIn("environment=production", calls[0])
                self.assertIn("repos/o/r/deployments/2/statuses", calls[1])
                self.assertIn("repos/o/r/deployments/3/statuses", calls[2])
                self.assertTrue(all(call[1:4] == ["api", "--method", "GET"] for call in calls))

    def test_fallback_refuses_only_automatic_or_malformed_deployments(self):
        cases = ({}, [None], [{"id": True, "sha": DEPLOYED}],
                 [{"id": 1, "sha": AUTOMATIC, "description": ""}],
                 [self.recorded(1, "short")])
        for batch in cases:
            with self.subTest(batch=batch), patch.object(resolve.subprocess, "check_output", return_value=json.dumps(batch)) as gh:
                self.assertEqual(resolve.newest_success_deployment("o/r"), "")
                self.assertEqual(gh.call_count, 1)

    def test_fallback_fails_closed_on_api_and_status_errors(self):
        for error in (subprocess.CalledProcessError(1, "gh"), subprocess.TimeoutExpired("gh", 30), OSError("offline")):
            with patch.object(resolve.subprocess, "check_output", side_effect=error):
                self.assertEqual(resolve.newest_success_deployment("o/r"), "")
        for status in ([], {}, [None], [{"state": "failure"}]):
            with patch.object(resolve.subprocess, "check_output", side_effect=[json.dumps([self.recorded(1)]), json.dumps(status)]):
                self.assertEqual(resolve.newest_success_deployment("o/r"), "")
        with patch.object(resolve.subprocess, "check_output", return_value="not json"):
            self.assertEqual(resolve.newest_success_deployment("o/r"), "")

    def test_main_prefers_live_then_falls_back_and_fails_closed(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as temp:
            output = Path(temp) / "outputs"
            for live, fallback, expected in ((LIVE, DEPLOYED, LIVE), ("", DEPLOYED, DEPLOYED), ("", "", "")):
                with self.subTest(live=live, fallback=fallback), patch.dict(os.environ, {"GITHUB_REPOSITORY": "o/r", "GITHUB_OUTPUT": str(output)}), patch.object(resolve, "live_revision", return_value=live), patch.object(resolve, "newest_success_deployment", return_value=fallback) as deployment, contextlib.redirect_stdout(io.StringIO()):
                    output.write_text("")
                    self.assertEqual(resolve.main(), 0 if expected else 1)
                    self.assertEqual(output.read_text(), f"live_sha={expected}\n" if expected else "")
                    self.assertEqual(deployment.call_count, 0 if live else 1)


if __name__ == "__main__":
    unittest.main()
