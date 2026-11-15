"""Inactive CLI integration with synthetic HTTP responses; no live metadata."""
import contextlib
import copy
import http.client
import io
import json
import os
import tempfile
import unittest
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
                    mock.patch.object(transport, "build_opener", return_value=opener), \
                    contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                result = gate.main(["--event", str(event)])
        self.assertNotIn("SENTINEL-TOKEN", output.getvalue() + errors.getvalue())
        return result, output.getvalue(), errors.getvalue(), opener.calls

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
