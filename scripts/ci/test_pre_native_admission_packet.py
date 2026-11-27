"""Synthetic evidence for the offline native PRE release gate."""

from dataclasses import asdict, replace
from hashlib import sha256
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch
from uuid import UUID

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_native_admission_packet import MAX_PACKET_BYTES, evaluate_packet
    from pre_target import ADMISSION_SECONDS, Target


PHASES = ("first_create", "rehearsal", "takeover", "readiness", "cleanup", "egress")
TARGET = Target("pre-fixture-admission", 123, 1_800_000_000, "0473/0465")
NOW = TARGET.expires_at + 10
DIGEST = "sha256:" + "a" * 64
COMMIT = "b" * 40
START_ID = UUID(int=123)
PINS = dict(target=TARGET, predecessor_digest=DIGEST, predecessor_commit=COMMIT,
            start_id=START_ID, now=NOW)


def fixture():
    receipts = {
        phase: dict(target=asdict(TARGET), predecessor_digest=DIGEST, predecessor_commit=COMMIT,
                    verdict="pass", observed_at=NOW - 20)
        for phase in PHASES
    }
    receipts["cleanup"]["terminal_observation"] = dict(
        start_id=str(START_ID), source="runtime", terminal=True, observed_at=NOW - 21,
    )
    return dict(target=asdict(TARGET), receipts=receipts, production_auto_promote_disabled=dict(
        name="PRODUCTION_AUTO_PROMOTE_DISABLED", value="true", observed_at=NOW,
    ))


def evaluate(packet, **overrides):
    return evaluate_packet(json.dumps(packet).encode(), **(PINS | overrides))


class NativeAdmissionPacketTests(unittest.TestCase):
    def assert_denied(self, result, reason):
        self.assertFalse(result["admitted"])
        self.assertIn(reason, result["reasons"])

    def test_pass_canonical_digest_and_complete_evidence(self):
        packet = fixture()
        canonical = json.dumps(packet, sort_keys=True, separators=(",", ":")).encode()
        expected = dict(admitted=True, reasons=[], packet_digest="sha256:" + sha256(canonical).hexdigest())
        self.assertEqual(evaluate(packet), expected)
        self.assertEqual(evaluate_packet(b"\n" + canonical + b"\n", **PINS), expected)
        packet["receipts"]["cleanup"]["terminal_observation"]["source"] = "provider"
        alternate = evaluate(packet)
        self.assertTrue(alternate["admitted"])
        self.assertNotEqual(alternate["packet_digest"], expected["packet_digest"])

    def test_every_phase_required_and_passing(self):
        for phase in PHASES:
            with self.subTest(phase=phase):
                packet = fixture()
                packet["receipts"][phase]["verdict"] = "fail"
                self.assert_denied(evaluate(packet), f"{phase}_not_passed")
                del packet["receipts"][phase]
                self.assert_denied(evaluate(packet), "invalid_phases")

    def test_every_phase_bound_to_all_pins(self):
        neighbors = (replace(TARGET, scope="pre-fixture-other"), replace(TARGET, run_id=124),
                     replace(TARGET, started_at=TARGET.started_at - 1), replace(TARGET, shape="0475/0465"))
        for phase in PHASES:
            for neighbor in neighbors:
                with self.subTest(phase=phase, neighbor=neighbor):
                    packet = fixture()
                    packet["receipts"][phase]["target"] = asdict(neighbor)
                    self.assert_denied(evaluate(packet), f"{phase}_target_mismatch")
            for field, alternate in (("predecessor_digest", "sha256:" + "c" * 64),
                                     ("predecessor_commit", "d" * 40)):
                with self.subTest(phase=phase, field=field):
                    packet = fixture()
                    packet["receipts"][phase][field] = alternate
                    self.assert_denied(evaluate(packet), f"{phase}_predecessor_mismatch")
        packet = fixture()
        packet["target"] = asdict(neighbors[0])
        self.assert_denied(evaluate(packet), "target_mismatch")
        self.assert_denied(evaluate(fixture(), start_id=UUID(int=0)), "invalid_admission_pins")

    def test_freshness_boundaries_and_reservation_start(self):
        earliest = NOW - ADMISSION_SECONDS
        for phase in PHASES:
            for time in (earliest - 1, NOW + 1):
                with self.subTest(phase=phase, time=time):
                    packet = fixture()
                    packet["receipts"][phase]["observed_at"] = time
                    self.assert_denied(evaluate(packet), f"{phase}_outside_freshness_window")
        packet = fixture()
        packet["receipts"]["first_create"]["observed_at"] = earliest
        self.assertTrue(evaluate(packet)["admitted"])
        packet["receipts"]["first_create"]["observed_at"] = TARGET.started_at - 1
        self.assert_denied(evaluate(packet, now=NOW - 40), "first_create_outside_freshness_window")
        self.assert_denied(evaluate(fixture(), now=True), "invalid_admission_clock")

    def test_reasons_are_ordered_and_digest_is_independent_of_verdict(self):
        packet = fixture()
        admitted = evaluate(packet)
        denied = evaluate(packet, now=NOW + ADMISSION_SECONDS)
        self.assertEqual(denied["packet_digest"], admitted["packet_digest"])
        self.assertFalse(denied["admitted"])
        self.assertEqual(denied["reasons"], [f"{phase}_outside_freshness_window" for phase in PHASES]
                         + ["cleanup_observation_outside_freshness_window"])

    def test_cleanup_requires_direct_terminal_observation_of_original_actor(self):
        changes = (
            ({"start_id": str(UUID(int=124))}, "cleanup_actor_not_confirmed_terminal"),
            ({"terminal": False}, "cleanup_actor_not_confirmed_terminal"),
            ({"source": "inventory"}, "invalid_cleanup_observation"),
            ({"observed_at": NOW - 19}, "cleanup_observation_outside_freshness_window"),
            ({"observed_at": NOW - ADMISSION_SECONDS - 1}, "cleanup_observation_outside_freshness_window"),
        )
        for change, reason in changes:
            with self.subTest(change=change):
                packet = fixture()
                packet["receipts"]["cleanup"]["terminal_observation"].update(change)
                self.assert_denied(evaluate(packet), reason)
        packet = fixture()
        packet["receipts"]["cleanup"]["terminal_observation"] = {"inventory": [], "absent": True}
        self.assert_denied(evaluate(packet), "invalid_cleanup_observation")

    def test_promotion_readback_exact_and_fresh(self):
        changes = (
            ({"value": "false"}, "production_auto_promotion_not_disabled"),
            ({"value": True}, "invalid_promotion_readback"),
            ({"name": "STAGING_AUTO_PROMOTE_DISABLED"}, "invalid_promotion_readback"),
            ({"observed_at": NOW + 1}, "production_readback_outside_freshness_window"),
            ({"observed_at": NOW - ADMISSION_SECONDS - 1}, "production_readback_outside_freshness_window"),
        )
        for change, reason in changes:
            with self.subTest(change=change):
                packet = fixture()
                packet["production_auto_promote_disabled"].update(change)
                self.assert_denied(evaluate(packet), reason)

    def test_strict_json_and_closed_schema(self):
        raw = json.dumps(fixture()).encode()
        invalid_json = (raw.replace(b'"verdict": "pass"', b'"verdict": "fail", "verdict": "pass"', 1),
                        b'\xff', b'[' * 10 + b']' * 10, b'{"number": 1e999}')
        for malformed in invalid_json:
            result = evaluate_packet(malformed, **PINS)
            self.assert_denied(result, "invalid_packet_json")
            self.assertIsNone(result["packet_digest"])
        self.assert_denied(evaluate_packet(b" " * (MAX_PACKET_BYTES + 1), **PINS), "invalid_packet_size_or_type")
        for path in ((), ("target",), ("receipts",), ("receipts", "readiness"),
                     ("receipts", "readiness", "target"), ("receipts", "cleanup", "terminal_observation"),
                     ("production_auto_promote_disabled",)):
            with self.subTest(path=path):
                packet = fixture()
                nested = packet
                for key in path:
                    nested = nested[key]
                nested["unexpected"] = "untrusted_value"
                result = evaluate(packet)
                self.assertFalse(result["admitted"])
                self.assertIsNone(result["packet_digest"])
                self.assertNotIn("untrusted_value", repr(result))
        packet = fixture()
        packet["target"]["run_id"] = True
        self.assert_denied(evaluate(packet), "invalid_pre_run")


if __name__ == "__main__":
    unittest.main()
