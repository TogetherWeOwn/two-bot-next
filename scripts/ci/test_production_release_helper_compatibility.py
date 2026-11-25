"""Exercise the two protected plan bindings with synthetic evidence."""

from pathlib import Path
import sys
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import production_release_preflight as preflight
from staging_migrate_approval import digest_json
from staging_rollout import GateError


class ReleaseHelperCompatibilityTests(unittest.TestCase):
    def test_recovery_fingerprint_does_not_replace_forward_target_or_approval(self):
        target = {"host": "example.invalid", "database": "fixture", "branch_id": "test"}
        pin = SimpleNamespace(candidate_sha="a" * 40, recovery_reference=lambda: "fixture-proof")
        run = {"id": 123}
        plan = {"mode": "plan", "migration_target": "production", "role": "two_bot_migrator_ro",
                "target": target, "source_sha": pin.candidate_sha, "recovery_evidence_ref": pin.recovery_reference()}
        archive = {"archive_digest": "sha256:" + "b" * 64}
        api = Mock(return_value={"created_at": "2026-01-01T00:00:00Z"})
        with (patch.object(preflight, "authenticated_plan", return_value=(plan, archive)),
              patch.object(preflight, "ledger_endpoints"), patch.object(preflight, "verify_environment"),
              patch.object(preflight, "verify_check", return_value=(None, preflight.moment("2026-01-02T00:00:00Z")))
              as review):
            expected = (plan, archive)
            self.assertEqual(preflight.bound_plan(pin, run, "124", digest_json(target), api), expected)
            review.assert_not_called()
            self.assertEqual(preflight.approved_plan(pin, run, "124", target=target, api=api), expected)
            review.assert_called_once()
            with self.assertRaisesRegex(GateError, "production_forward_plan_changed"):
                preflight.bound_plan(pin, run, "124", "c" * 64, api)
