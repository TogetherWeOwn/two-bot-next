"""Offline signed attendance acceptance with synthetic response envelopes."""

from copy import deepcopy
from datetime import timedelta
from email.message import Message
from io import BytesIO, StringIO
import json
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import Mock, call, patch
from urllib.error import HTTPError
from urllib.response import addinfourl

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import staging_attendance_acceptance as acceptance
from script_intake import decode_json


FIXTURE = decode_json(Path(__file__).with_name("fixtures").joinpath("staging_attendance_acceptance.json").read_bytes())
ROLLOUT, READY, RESULT = (FIXTURE[key] for key in ("rollout", "ready", "result"))
IDENTITY = tuple(ROLLOUT[key] for key in ("worker_version", "revision", "build_id", "deploy_id"))
ORIGIN = acceptance.STAGING_ORIGIN
EVENT = RESULT["event_key"]
START = acceptance.fixture_start("2026-11-24T16:00:00Z")


def observed_at(moment):
    return moment.isoformat(timespec="milliseconds").replace("+00:00", "Z")


def active_observation():
    observed = deepcopy(FIXTURE["observed"])
    lifecycle = observed["lifecycle"]
    lifecycle["observedAt"] = observed_at(START + timedelta(minutes=10))
    lifecycle["use"].update(acquiredAt=int(START.timestamp()) * 1000,
                            expiresAt=int((START.timestamp() + 3600) * 1000))
    return observed


class AcceptanceTests(unittest.TestCase):
    def test_effective_configuration_and_cleanup(self):
        observed = active_observation()
        lifecycle, lease = observed["lifecycle"], observed["lifecycle"]["use"]

        def check(*args):
            with patch.object(acceptance.time, "time", return_value=START.timestamp() + 2700):
                return acceptance.validate_configuration(*args)

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
        observed = active_observation()
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

    def test_finalized_probe_stops_on_signed_read_refusal_after_preflight(self):
        observed = active_observation()
        now = int(START.timestamp()) + 3600
        observed["lifecycle"]["use"]["expiresAt"] = (now + 60) * 1000
        env = {"STAGING_ATTENDANCE_KEY_ID": "synthetic-key",
               "STAGING_ATTENDANCE_SIGNING_SECRET": "a" * 64}
        signer = acceptance.credentials(env)
        ready_peer, _, _ = self.signed_peer("finalized")
        calls = []

        def check(request):
            calls.append((request.get_method(), request.full_url))
            if request.get_method() == "GET" and len(calls) == 1:
                return ready_peer(request)
            self.assertEqual(request.get_method(), "POST")
            self.assertEqual(decode_json(request.data), {"action": acceptance.ACTION, "event_key": EVENT})
            expected = acceptance.signing_headers(
                *signer, str(now), request.get_header("X-two-nonce"), request.data,
            )
            self.assertEqual({name: request.get_header(name.capitalize()) for name in expected}, expected)
            # Stand in for a receiver that refuses a read after healthy preflight.
            # Even a retryable refusal must stop the finalized probe.
            return 502, {"cache-control": "no-store"}, {
                "ok": False, "request_id": request.get_header("X-request-id"),
                "error": {"code": "discord_unavailable", "message": "synthetic diagnostic", "retryable": True},
            }

        args = ["finalized", "--rollout", "rollout", "--observed", "observed",
                "--fixture-start", START.strftime("%Y-%m-%dT%H:%M:%SZ"), "--execute"]
        with (patch.object(acceptance, "read_object", side_effect=[ROLLOUT, observed]),
              patch.object(acceptance.time, "time", return_value=now),
              patch.object(acceptance.time, "monotonic", return_value=0),
              patch("sys.stdout", new_callable=StringIO) as stdout,
              patch("sys.stderr", new_callable=StringIO) as stderr):
            status = acceptance.main(args, env=env, check=check)
        self.assertEqual(status, 1)
        self.assertEqual(stdout.getvalue(), "")
        self.assertEqual(stderr.getvalue(), "FAIL: signed attendance response did not match the expected envelope\n")
        self.assertEqual(calls, [("GET", ORIGIN + "/readyz"), ("POST", ORIGIN + "/internal/actions")])

    def test_finalized_receipt_refuses_nested_duplicate_keys(self):
        observed = active_observation()
        now = int(START.timestamp()) + 3600
        observed["lifecycle"]["use"]["expiresAt"] = (now + 60) * 1000
        env = {"STAGING_ATTENDANCE_KEY_ID": "synthetic-key",
               "STAGING_ATTENDANCE_SIGNING_SECRET": "a" * 64}
        args = ["finalized", "--rollout", "rollout", "--observed", "observed",
                "--fixture-start", START.strftime("%Y-%m-%dT%H:%M:%SZ"), "--execute"]

        def run(duplicate):
            calls = []

            def open_response(request, *, timeout):
                headers = Message()
                headers["content-type"] = "application/json"
                if request.get_method() == "GET":
                    self.assertEqual(request.full_url, ORIGIN + "/readyz")
                    calls.append("readyz")
                    headers["x-two-worker-version"] = ROLLOUT["worker_version"]
                    return addinfourl(BytesIO(json.dumps(READY).encode()), headers, request.full_url, 200)
                self.assertEqual(request.get_method(), "POST")
                self.assertEqual(request.full_url, ORIGIN + "/internal/actions")
                event_key = decode_json(request.data)["event_key"]
                calls.append(event_key)
                headers["cache-control"] = "no-store"
                envelope = {"request_id": request.get_header("X-request-id")}
                if event_key == EVENT:
                    envelope.update(ok=True, result=RESULT)
                    body = json.dumps(envelope).encode()
                    if duplicate:
                        # A last-wins decoder would still see the valid result.
                        body = body.replace(b'"starts_at":',
                                            b'"starts_at":"synthetic-private-response-text","starts_at":', 1)
                    return addinfourl(BytesIO(body), headers, request.full_url, 200)
                self.assertEqual(event_key, EVENT + ":foreign")
                envelope.update(ok=False, error={"code": "not_found", "message": "request refused", "retryable": False})
                raise HTTPError(request.full_url, 404, "not_found", headers, BytesIO(json.dumps(envelope).encode()))

            with (patch.object(acceptance, "no_redirect_opener") as opener,
                  patch.object(acceptance, "read_object", side_effect=[ROLLOUT, observed]),
                  patch.object(acceptance.time, "time", return_value=now),
                  patch.object(acceptance.time, "monotonic", return_value=0),
                  patch("sys.stdout", new_callable=StringIO) as stdout,
                  patch("sys.stderr", new_callable=StringIO) as stderr):
                opener.return_value.open.side_effect = open_response
                status = acceptance.main(args, env=env)
            return status, stdout.getvalue(), stderr.getvalue(), calls

        self.assertEqual(run(duplicate=False),
                         (0, "PASS: attendance finalized\n", "", ["readyz", EVENT, "readyz", EVENT + ":foreign", "readyz"]))
        self.assertEqual(run(duplicate=True),
                         (1, "", "FAIL: attendance acceptance input or transport refused\n", ["readyz", EVENT]))

    def test_execute_requires_a_coherent_lease_that_covers_the_armed_probe(self):
        expires = START.timestamp() + 3600
        latest_start = expires - acceptance.RUN_BUDGET
        admitted_at = latest_start - 1
        env = {"STAGING_ATTENDANCE_KEY_ID": "synthetic-key",
               "STAGING_ATTENDANCE_SIGNING_SECRET": "a" * 64}
        signer = acceptance.credentials(env)
        args = ["finalized", "--rollout", "rollout", "--observed", "observed",
                "--fixture-start", START.strftime("%Y-%m-%dT%H:%M:%SZ"), "--execute"]
        refused = (1, "", "FAIL: staging lease did not cover the probe\n")
        future_readback = START + timedelta(seconds=admitted_at - START.timestamp() + 0.25)

        cases = [
            ("coherent", latest_start - 0.75, 0.25, {}, {}, True),
            ("deadline equals expiry", latest_start, 0, {}, {}, False),
            ("active but too short", latest_start + 0.5, 0, {}, {}, False),
            ("expired during setup", expires + 1, 0, {}, {}, False),
            ("cutoff crossed while arming", latest_start - 0.5, 0.5, {}, {}, False),
            ("observation after local time", latest_start - 0.5, 0,
             {"observedAt": observed_at(future_readback)}, {}, False),
            ("acquisition after observation", admitted_at, 0, {},
             {"acquiredAt": int((START.timestamp() + 601) * 1000)}, False),
        ]
        for name, signer_ready_at, arm_delay, receipt_fields, lease_fields, passes in cases:
            with self.subTest(name=name):
                observed = active_observation()
                observed["lifecycle"].update(receipt_fields)
                observed["lifecycle"]["use"].update(lease_fields)
                now = admitted_at
                peer, reads, readies = self.signed_peer("finalized")

                def delayed_credentials(_env):
                    nonlocal now
                    now = signer_ready_at
                    return signer

                def delayed_alarm(seconds):
                    nonlocal now
                    if seconds:
                        now += arm_delay

                def authenticated_peer(request):
                    if request.get_method() == "POST":
                        timestamp = request.get_header("X-two-timestamp")
                        self.assertEqual(timestamp, str(int(signer_ready_at + arm_delay)))
                        headers = acceptance.signing_headers(
                            *signer, timestamp, request.get_header("X-two-nonce"), request.data,
                        )
                        self.assertEqual({name: request.get_header(name.capitalize()) for name in headers}, headers)
                    return peer(request)

                check = Mock(side_effect=authenticated_peer)
                with (patch.object(acceptance, "read_object", side_effect=[ROLLOUT, observed]),
                      patch.object(acceptance.time, "time", side_effect=lambda: now),
                      patch.object(acceptance.time, "monotonic", side_effect=lambda: now - admitted_at),
                      patch.object(acceptance, "credentials", side_effect=delayed_credentials),
                      patch.object(acceptance.signal, "alarm", side_effect=delayed_alarm) as alarm,
                      patch("sys.stdout", new_callable=StringIO) as stdout,
                      patch("sys.stderr", new_callable=StringIO) as stderr):
                    status = acceptance.main(args, env=env, check=check)

                verdict = status, stdout.getvalue(), stderr.getvalue()
                if arm_delay:
                    self.assertEqual(alarm.call_args_list, [call(acceptance.RUN_BUDGET), call(0)])
                if passes:
                    self.assertEqual(verdict, (0, "PASS: attendance finalized\n", ""))
                    self.assertEqual(reads, [EVENT, EVENT + ":foreign"])
                    self.assertEqual(len(readies), 3)
                else:
                    self.assertEqual((verdict, check.call_count), (refused, 0))

    def test_acceptance_sequence_uses_one_pinned_rollout(self):
        env = {"STAGING_ATTENDANCE_KEY_ID": "synthetic-key",
               "STAGING_ATTENDANCE_SIGNING_SECRET": "a" * 64}
        other_rollout = {**ROLLOUT, "worker_version": "bbbbbbbb-cccc-dddd-eeee-ffffffffffff",
                         "revision": "b" * 40, "build_id": "1234-2", "deploy_id": "2026.11.24.2+bbbbbbb"}
        no_traffic = Mock(side_effect=AssertionError("unexpected staging request"))

        with (TemporaryDirectory() as directory,
              patch.object(acceptance.time, "time") as clock,
              patch.object(acceptance.time, "monotonic", return_value=0)):
            rollout_path = Path(directory) / "rollout.json"
            observed_path = Path(directory) / "observed.json"
            rollout_path.write_text(json.dumps(ROLLOUT), encoding="utf-8")

            def run(phase, observed, check):
                observed_path.write_text(json.dumps(observed), encoding="utf-8")
                args = [phase, "--rollout", str(rollout_path), "--observed", str(observed_path)]
                if phase != "cleanup":
                    args.extend(["--execute", "--fixture-start", START.strftime("%Y-%m-%dT%H:%M:%SZ")])
                with (patch("sys.stdout", new_callable=StringIO) as stdout,
                      patch("sys.stderr", new_callable=StringIO) as stderr):
                    status = acceptance.main(args, env=env if phase != "cleanup" else {}, check=check)
                return status, stdout.getvalue(), stderr.getvalue()

            phases = [("pending", 10, [EVENT]), ("finalized", 46, [EVENT, EVENT + ":foreign"]),
                      ("off", 47, [EVENT]), ("cleanup", 48, [])]
            for phase, minutes_after_start, expected_reads in phases:
                with self.subTest(phase=phase):
                    clock.return_value = START.timestamp() + 60 * minutes_after_start
                    observed = active_observation()
                    observed.update(attendance_read=phase in ("pending", "finalized"),
                                    primary_fixture="pending" if phase == "pending" else "finalized")
                    lifecycle = observed["lifecycle"]
                    lifecycle["use"]["expiresAt"] = int((START.timestamp() + 7200) * 1000)
                    if phase == "cleanup":
                        lifecycle.update(action="release", running=False)
                        lifecycle["use"]["phase"] = "released"
                        del observed["primary_fixture"], observed["foreign_fixture"]
                        observed["fixture_rows"] = 0

                    if phase in ("finalized", "cleanup"):
                        other_observed = deepcopy(observed)
                        other_observed.update(worker_version=other_rollout["worker_version"],
                                              build_revision=other_rollout["revision"], build_id=other_rollout["build_id"],
                                              deploy_id=other_rollout["deploy_id"])
                        other_lifecycle = other_observed["lifecycle"]
                        for record in (other_lifecycle, other_lifecycle["use"]):
                            record.update(deploymentId=other_rollout["worker_version"], sha=other_rollout["revision"])
                        # The substitute is internally valid under its own receipt.
                        acceptance.validate_configuration(phase, ORIGIN, other_rollout, other_observed)
                        self.assertEqual(run(phase, other_observed, no_traffic),
                                         (1, "", "FAIL: configuration is for a different staging build\n"))

                    check, reads, readies = self.signed_peer(phase) if expected_reads else (no_traffic, [], [])
                    self.assertEqual(run(phase, observed, check), (0, f"PASS: attendance {phase}\n", ""))
                    self.assertEqual(reads, expected_reads)
                    self.assertEqual(len(readies), len(expected_reads) + 1 if expected_reads else 0)

        no_traffic.assert_not_called()

    def test_rehearsal_evidence_requires_the_originating_lease(self):
        def observations(acquired, attempt):
            rows = {}
            for phase, minutes in zip(acceptance.PHASES, (10, 46, 47, 48)):
                row = deepcopy(FIXTURE["observed"])
                row.update(attendance_read=phase in ("pending", "finalized"),
                           primary_fixture="pending" if phase == "pending" else "finalized")
                lifecycle = row["lifecycle"]
                lifecycle["observedAt"] = (acquired + timedelta(minutes=minutes)).isoformat(
                    timespec="milliseconds").replace("+00:00", "Z")
                lifecycle["use"].update(environment="staging", runId="1234", runAttempt=attempt,
                                        acquiredAt=int(acquired.timestamp()) * 1000,
                                        expiresAt=int((acquired + timedelta(hours=1)).timestamp()) * 1000)
                if phase == "cleanup":
                    lifecycle.update(action="release", running=False)
                    lifecycle["use"]["phase"] = "released"
                    del row["primary_fixture"], row["foreign_fixture"]
                    row["fixture_rows"] = 0
                rows[phase] = row
            return rows

        first = observations(START, 1)
        second = observations(START + timedelta(hours=1), 2)
        no_traffic = Mock(side_effect=AssertionError("unexpected staging request"))
        with (TemporaryDirectory() as directory,
              patch.object(acceptance, "credentials", side_effect=AssertionError("unexpected credentials")),
              patch.object(acceptance.time, "time", return_value=START.timestamp() + 86400)):
            rollout_path = Path(directory) / "rollout.json"
            observed_path = Path(directory) / "observed.json"
            rollout_path.write_text(json.dumps(ROLLOUT), encoding="utf-8")

            def run(rows, phase="rehearsal", extra=()):
                observed_path.write_text(json.dumps(rows), encoding="utf-8")
                args = [phase, "--rollout", str(rollout_path), "--observed", str(observed_path), *extra]
                with (patch("sys.stdout", new_callable=StringIO) as stdout,
                      patch("sys.stderr", new_callable=StringIO) as stderr):
                    status = acceptance.main(args, env={}, check=no_traffic)
                return status, stdout.getvalue(), stderr.getvalue()

            passed = (0, "PASS: attendance rehearsal\n", "")
            mismatch = (1, "", "FAIL: staging lease identity did not match\n")
            self.assertEqual(run(first), passed)
            self.assertEqual(run(second), passed)
            self.assertEqual(run(second["cleanup"], "cleanup"), (0, "PASS: attendance cleanup\n", ""))
            for phase in acceptance.PHASES:
                with self.subTest(phase=phase):
                    self.assertEqual(run({**first, phase: second[phase]}), mismatch)

            rebound = deepcopy(first["cleanup"])
            rebound["lifecycle"]["epoch"] += 1
            rebound["lifecycle"]["use"]["epoch"] += 1
            self.assertEqual(run({**first, "cleanup": rebound}), mismatch)

            late = deepcopy(first["off"])
            last_start = START + timedelta(hours=1, seconds=-acceptance.RUN_BUDGET)
            late["lifecycle"]["observedAt"] = last_start.strftime("%Y-%m-%dT%H:%M:%S.000Z")
            self.assertEqual(run({**first, "off": late}), (1, "", "FAIL: staging lease did not cover the probe\n"))
            self.assertEqual(run({phase: row for phase, row in first.items() if phase != "cleanup"}),
                             (1, "", "FAIL: rehearsal requires all four observations\n"))
            self.assertEqual(run(first, extra=("--execute",)),
                             (1, "", "FAIL: rehearsal only accepts offline readback\n"))
        no_traffic.assert_not_called()

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
