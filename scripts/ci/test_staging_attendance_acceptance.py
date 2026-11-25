"""Offline signed attendance acceptance with synthetic response envelopes."""

from copy import deepcopy
from email.message import Message
from io import BytesIO, StringIO
import json
from pathlib import Path
import sys
import time
import unittest
from unittest.mock import Mock, patch
from urllib.error import HTTPError

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import staging_attendance_acceptance as acceptance
from script_intake import decode_json


FIXTURE = decode_json(Path(__file__).with_name("fixtures").joinpath("staging_attendance_acceptance.json").read_bytes())
ROLLOUT, READY, RESULT = (FIXTURE[key] for key in ("rollout", "ready", "result"))
IDENTITY = tuple(ROLLOUT[key] for key in ("worker_version", "revision", "build_id", "deploy_id"))
ORIGIN = acceptance.STAGING_ORIGIN
EVENT = RESULT["event_key"]
START = acceptance.fixture_start("2026-11-24T16:00:00Z")


class AcceptanceTests(unittest.TestCase):
    def test_effective_configuration_and_cleanup(self):
        observed = deepcopy(FIXTURE["observed"])
        lifecycle, lease = observed["lifecycle"], observed["lifecycle"]["use"]
        lease["expiresAt"] = int((time.time() + 60) * 1000)
        check = acceptance.validate_configuration
        self.assertEqual(check("finalized", ORIGIN, ROLLOUT, observed), IDENTITY)
        mismatches = {"guild_id": "2", "origin": "https://other.example.test", "build_id": "1234-2",
                      "attendance_read": 1, "attendance_capture": True, "signed_ingress": False, "foreign_fixture": "missing"}
        for field, bad in mismatches.items():
            with self.assertRaises((acceptance.Refused, acceptance.SmokeError)):
                check("finalized", ORIGIN, ROLLOUT, {**observed, field: bad})
        lease["expiresAt"] = 1
        with self.assertRaises(acceptance.Refused):
            check("finalized", ORIGIN, ROLLOUT, observed)
        observed.update(attendance_read=False, fixture_rows=0)
        lifecycle["running"] = False
        for action in ("stop", "release"):
            lifecycle["action"] = action
            with self.assertRaises(acceptance.Refused):
                check("cleanup", ORIGIN, ROLLOUT, observed)
        lease["phase"] = "released"
        self.assertEqual(check("cleanup", ORIGIN, ROLLOUT, observed), IDENTITY)
        with self.assertRaises(acceptance.Refused):
            check("cleanup", ORIGIN, ROLLOUT, {**observed, "fixture_rows": 1})

    def signed_peer(self, phase, *, wrong_result=False, drift=False):
        requests, readies = [], []

        def check(request):
            if request.get_method() == "GET":
                self.assertEqual(request.full_url, ORIGIN + "/readyz")
                self.assertIsNone(request.get_header("X-two-signature"))
                readies.append(request)
                report = {**READY, "build_id": "1234-2"} if drift and requests else READY
                return 200, {"x-two-worker-version": ROLLOUT["worker_version"]}, report
            self.assertEqual(request.full_url, ORIGIN + "/internal/actions")
            key = decode_json(request.data)["event_key"]
            self.assertEqual(decode_json(request.data), {"action": "event.attendance.read", "event_key": key})
            self.assertIsNone(request.get_header("Idempotency-key"))
            self.assertRegex(request.get_header("X-two-signature"), r"^sha256=[a-f0-9]{64}$")
            self.assertRegex(request.get_header("X-two-nonce"), r"^[a-f0-9]{32}$")
            requests.append(key)
            headers = {"cache-control": "no-store"}
            envelope = {"request_id": request.get_header("X-request-id")}
            if phase == "finalized" and key == EVENT:
                result = {**RESULT, "final": 1} if wrong_result else RESULT
                return 200, headers, dict(envelope, ok=True, result=result)
            status, code = (403, "action_not_allowed") if phase == "off" else (404, "not_found")
            envelope.update(ok=False, error={"code": code, "message": "request refused", "retryable": False})
            media = Message()
            media["content-type"], media["cache-control"] = "application/json", "no-store"
            raw = BytesIO(json.dumps(envelope).encode("ascii"))
            with patch.object(acceptance, "no_redirect_opener") as opener:
                opener.return_value.open.side_effect = HTTPError(request.full_url, status, code, media, raw)
                return acceptance.send(request)

        return check, requests, readies

    def test_wire_phases_and_fail_closed_results(self):
        signer = ("synthetic-key", b"x" * 64)
        for phase, keys in [("pending", [EVENT]), ("finalized", [EVENT, EVENT + ":foreign"]), ("off", [EVENT])]:
            check, requests, readies = self.signed_peer(phase)
            acceptance.probe(phase, ORIGIN, IDENTITY, START, signer, check)
            self.assertEqual(requests, keys)
            self.assertEqual(len(readies), len(keys) + 1)
        for check in (self.signed_peer("finalized", wrong_result=True)[0], self.signed_peer("finalized", drift=True)[0]):
            with self.assertRaises(acceptance.Refused):
                acceptance.probe("finalized", ORIGIN, IDENTITY, START, signer, check)
        check, _, _ = self.signed_peer("pending")
        with patch.object(acceptance.time, "monotonic", side_effect=[0, 2.5]), self.assertRaisesRegex(acceptance.Refused, "Web deadline"):
            acceptance.probe("pending", ORIGIN, IDENTITY, START, signer, check)

    def test_capture_off_does_not_extend_the_signed_probe_window(self):
        observed = deepcopy(FIXTURE["observed"])
        boundary = int(START.timestamp()) + 3600
        observed["lifecycle"]["use"]["expiresAt"] = (boundary + acceptance.RUN_BUDGET) * 1000
        env = {"STAGING_ATTENDANCE_KEY_ID": "synthetic-key",
               "STAGING_ATTENDANCE_SIGNING_SECRET": "a" * 64}
        signer = acceptance.credentials(env)
        peer, requests, _ = self.signed_peer("finalized")

        def authenticated_peer(request):
            if request.get_method() == "POST":
                expected = acceptance.signing_headers(
                    *signer, request.get_header("X-two-timestamp"),
                    request.get_header("X-two-nonce"), request.data,
                )
                actual = {name: request.get_header(name.capitalize()) for name in expected}
                self.assertEqual(actual, expected)
            return peer(request)

        check = Mock(side_effect=authenticated_peer)
        args = ["finalized", "--rollout", "rollout", "--observed", "observed",
                "--fixture-start", START.strftime("%Y-%m-%dT%H:%M:%SZ"), "--execute"]

        def run(at):
            with (patch.object(acceptance, "read_object", side_effect=[ROLLOUT, observed]),
                  patch.object(acceptance.time, "time", return_value=at),
                  patch.object(acceptance.time, "monotonic", return_value=0),
                  patch("sys.stdout", new_callable=StringIO),
                  patch("sys.stderr", new_callable=StringIO) as stderr):
                return acceptance.main(args, env=env, check=check), stderr.getvalue()

        observed["attendance_capture"] = True
        self.assertEqual(run(boundary - 1), (1, "FAIL: attendance flags differ from the phase\n"))
        check.assert_not_called()

        observed["attendance_capture"] = False
        self.assertEqual(run(boundary - 1), (0, ""))
        self.assertEqual(requests, [EVENT, EVENT + ":foreign"])

        # Replay the same capture-off readback while the lease is still active.
        # Exactly one probe budget remaining is already too late for admission.
        check.reset_mock()
        self.assertEqual(run(boundary), (1, "FAIL: staging lease did not cover the probe\n"))
        check.assert_not_called()

    def test_signature_matches_shared_wire_vectors(self):
        path = Path(__file__).resolve().parents[2] / "crates/core/tests/fixtures/internal-action-signing.json"
        for vector in decode_json(path.read_bytes())["vectors"]:
            headers = acceptance.signing_headers("fixture", vector["secret"].encode(), vector["timestamp"],
                                                 vector["nonce"], vector["body"].encode())
            self.assertEqual(headers["x-two-signature"], vector["signature"])

    def test_without_execute_sends_nothing(self):
        args = ["pending", "--rollout", "rollout", "--observed", "observed"]
        with patch.object(acceptance, "read_object"), patch.object(acceptance, "validate_configuration"):
            self.assertEqual(acceptance.main(args, env={}, check=lambda _: self.fail("network call")), 2)
            args[0] = "cleanup"
            self.assertEqual(acceptance.main([*args, "--fixture-start", "2026-11-24T16:00:00Z"]), 1)
