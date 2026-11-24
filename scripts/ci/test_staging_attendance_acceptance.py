"""Offline signed attendance acceptance with synthetic response envelopes."""

from copy import deepcopy
from pathlib import Path
import sys
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import staging_attendance_acceptance as acceptance
from script_intake import decode_json
from staging_automation_read_smoke import STAGING_GUILD_ID


FIXTURE = decode_json(Path(__file__).with_name("fixtures").joinpath("staging_attendance_acceptance.json").read_bytes())
ROLLOUT, READY, RESULT = (FIXTURE[key] for key in ("rollout", "ready", "result"))
IDENTITY = tuple(ROLLOUT[key] for key in ("worker_version", "revision", "build_id", "deploy_id"))
VERSION, SHA, _, _ = IDENTITY
ORIGIN = acceptance.STAGING_ORIGIN
EVENT = RESULT["event_key"]
START = acceptance.fixture_start("2026-11-24T16:00:00Z")


class AcceptanceTests(unittest.TestCase):
    def test_effective_configuration_and_cleanup(self):
        lease = {"phase": "active", "deploymentId": VERSION, "sha": SHA, "epoch": 8,
                 "expiresAt": int((time.time() + 60) * 1000)}
        lifecycle = {"schema": "two-bot-staging-use.v1", "environment": "staging", "deploymentId": VERSION,
                     "sha": SHA, "epoch": 8, "action": "ready", "running": True, "use": lease}
        observed = {**ROLLOUT, "build_revision": SHA, "environment": "staging", "origin": ORIGIN,
                    "guild_id": STAGING_GUILD_ID, "attendance_read": True, "attendance_capture": False,
                    "signed_ingress": True, "primary_fixture": "finalized", "foreign_fixture": "foreign-only",
                    "lifecycle": lifecycle}
        check = acceptance.validate_configuration
        self.assertEqual(check("finalized", ORIGIN, ROLLOUT, observed), IDENTITY)
        for field, bad in {"guild_id": "2", "build_id": "1234-2", "attendance_read": 1, "attendance_capture": True,
                           "signed_ingress": False, "foreign_fixture": "missing"}.items():
            with self.assertRaises((acceptance.Refused, acceptance.SmokeError)):
                check("finalized", ORIGIN, ROLLOUT, {**observed, field: bad})
        lease["expiresAt"] = 1
        with self.assertRaises(acceptance.Refused):
            check("finalized", ORIGIN, ROLLOUT, observed)
        lifecycle.update(action="release", running=False, use=None)
        observed.update(attendance_read=False, fixture_rows=0)
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
                return 200, {"x-two-worker-version": VERSION}, report
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
                result = deepcopy(RESULT)
                if wrong_result:
                    result["final"] = 1
                return 200, headers, dict(envelope, ok=True, result=result)
            status, code = (403, "action_not_allowed") if phase == "off" else (404, "not_found")
            return status, headers, dict(envelope, ok=False, error={
                "code": code, "message": "request refused", "retryable": False})

        return check, requests, readies

    def test_wire_phases_and_fail_closed_results(self):
        signer = ("synthetic-key", b"x" * 64)
        for phase, keys in [("pending", [EVENT]), ("finalized", [EVENT, EVENT + ":foreign"]), ("off", [EVENT])]:
            check, requests, readies = self.signed_peer(phase)
            acceptance.probe(phase, ORIGIN, IDENTITY, START, signer, check)
            self.assertEqual(requests, keys)
            self.assertEqual(len(readies), len(keys) + 1)
        for mismatch in ({"wrong_result": True}, {"drift": True}):
            check, _, _ = self.signed_peer("finalized", **mismatch)
            with self.assertRaises(acceptance.Refused):
                acceptance.probe("finalized", ORIGIN, IDENTITY, START, signer, check)

    def test_signature_matches_shared_wire_vectors(self):
        path = Path(__file__).resolve().parents[2] / "crates/core/tests/fixtures/internal-action-signing.json"
        for vector in decode_json(path.read_bytes())["vectors"]:
            headers = acceptance.signing_headers("fixture", vector["secret"].encode(), vector["timestamp"],
                                                 vector["nonce"], vector["body"].encode())
            self.assertEqual(headers["x-two-signature"], vector["signature"])

    def test_without_execute_sends_nothing(self):
        args = ["pending", "--staging-url", ORIGIN, "--rollout", "rollout", "--observed", "observed"]
        with patch.object(acceptance, "read_object"), patch.object(acceptance, "validate_configuration"):
            self.assertEqual(acceptance.main(args, env={}, check=lambda _: self.fail("network call")), 2)
