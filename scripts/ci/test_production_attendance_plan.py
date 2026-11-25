"""Synthetic tests for the dormant production attendance preflight validator."""

from copy import deepcopy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from production_attendance_plan import validate_attendance_plan
from production_migrate_approval import stable_plan
from staging_rollout import GateError


class ProductionAttendancePlanTests(unittest.TestCase):
    def setUp(self):
        self.plan = {
            "runner_version": 1,
            "mode": "plan",
            "migration_target": "production",
            "role": "two_bot_migrator_ro",
            "role_verified_connections": 1,
            "plan_run_id": "",
            "plan_provenance_verified": False,
            "applied_count": 0,
            "applied_store_count": 0,
            "ledger_before": [{"version": 474, "success": True}],
            "ledger_after": [{"version": 474, "success": True}],
            "ledger_store_before": [{"version": 477, "success": True}],
            "ledger_store_after": [{"version": 477, "success": True}],
            "pending_before": [475, 479, 480, 481, 482],
            "pending_store_before": [478],
            "audit": {
                "connection_usage": {"used": 10},
                "verify_findings": [
                    "missing object: public.event_attendance_windows",
                    "missing object: public.event_voice_sessions",
                    "missing object: public.event_voice_sessions_id_seq",
                    "missing object: public.live_sequence_watermarks",
                    "missing object: public.voice_heat_hours",
                    "missing object: web_v1.voice_heat",
                ],
            },
        }

    def test_admits_pre_plans_with_pending_migrations_and_advisory_connection_changes(self):
        fresh = deepcopy(self.plan)
        fresh["role_verified_connections"] = 2
        fresh["audit"]["connection_usage"] = {"used": 11}
        self.assertIsNone(validate_attendance_plan(self.plan))
        self.assertIsNone(validate_attendance_plan(fresh))
        self.assertEqual(stable_plan(self.plan), stable_plan(fresh))

    def test_refuses_non_read_only_manifest_shape(self):
        changes = {
            "mode": "apply", "migration_target": "staging", "runner_version": True,
            "role": "two_bot_migrator", "role_verified_connections": True,
            "plan_run_id": "123", "plan_provenance_verified": 0,
            "applied_count": False, "applied_store_count": 1,
            "ledger_before": {}, "ledger_after": [],
            "ledger_store_before": {}, "ledger_store_after": [],
        }
        for key, value in changes.items():
            with self.subTest(key=key), self.assertRaisesRegex(GateError, "production_attendance_plan_invalid"):
                validate_attendance_plan({**self.plan, key: value})
        for key in ("role_verified_connections", "ledger_after", "ledger_store_after"):
            plan = {name: value for name, value in self.plan.items() if name != key}
            with self.subTest(missing=key), self.assertRaisesRegex(GateError, "production_attendance_plan_invalid"):
                validate_attendance_plan(plan)

    def test_refuses_missing_failed_or_malformed_audit_evidence(self):
        findings = self.plan["audit"]["verify_findings"]
        cases = (None, [], {}, {"error": "synthetic audit failure"},
                 {"error": None, "verify_findings": findings}, {"verify_findings": None},
                 {"verify_findings": findings[0]}, {"verify_findings": [*findings[:-1], {"finding": findings[-1]}]})
        for audit in cases:
            with self.subTest(audit=audit), self.assertRaisesRegex(GateError, "production_attendance_audit_invalid"):
                validate_attendance_plan({**self.plan, "audit": audit})
        del self.plan["audit"]
        with self.assertRaisesRegex(GateError, "production_attendance_audit_invalid"):
            validate_attendance_plan(self.plan)

    def test_refuses_incomplete_duplicate_or_unexpected_findings_even_when_both_plans_agree(self):
        findings = self.plan["audit"]["verify_findings"]
        cases = ([], findings[:-1], [*findings[:-1], findings[0]], [*findings, findings[0]],
                 [*findings, "missing object: public.unrelated"],
                 [*findings[:-1], "missing schema: web_v1"],
                 [*findings[:-1], "object kind/owner differs: web_v1.voice_heat"],
                 [*findings[:-1], findings[-1] + "_other"])
        for observed in cases:
            plan = {**self.plan, "audit": {"verify_findings": observed}}
            self.assertEqual(stable_plan(plan), stable_plan(deepcopy(plan)))
            with self.subTest(findings=observed), self.assertRaisesRegex(
                    GateError, "production_attendance_audit_findings_disallowed"):
                validate_attendance_plan(plan)
