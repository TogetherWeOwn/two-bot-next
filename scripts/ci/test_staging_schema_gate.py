"""Offline fixtures for the staging schema gate (TOG-24362).

The gate evaluates the read-only staging-migrate --plan manifest the
deploy-staging schema-gate job prints, and fails the deploy before any
rollout when staging schema or role grants drifted behind the deploy SHA.
No database, no network, no secrets: pure manifest evaluation.
"""

import unittest

from staging_schema_gate import evaluate

SHA = "a" * 40
OTHER_SHA = "b" * 40


def manifest(**overrides):
    base = {
        "mode": "plan",
        "migration_target": "staging",
        "source_sha": SHA,
        "pending_before": [],
        "audit": {
            "ledger_owner": "two_bot_migrator",
            "verify_findings": [],
        },
    }
    base.update(overrides)
    return base


class EvaluateTests(unittest.TestCase):
    def test_clean_plan_passes(self):
        self.assertEqual(evaluate(manifest(), SHA), [])

    def test_pending_migrations_fail_with_versions(self):
        problems = evaluate(manifest(pending_before=[433, 434]), SHA)
        self.assertEqual(len(problems), 1)
        self.assertIn("433", problems[0])
        self.assertIn("434", problems[0])
        self.assertIn("staging-migrate", problems[0])

    def test_verify_findings_fail_with_remediation(self):
        audit = {"ledger_owner": "two_bot_migrator",
                 "verify_findings": ["missing SELECT on privacy_opt_outs"]}
        problems = evaluate(manifest(audit=audit), SHA)
        self.assertEqual(len(problems), 1)
        self.assertIn("database_roles", problems[0])

    def test_both_drift_classes_reported_together(self):
        audit = {"verify_findings": ["missing SELECT on privacy_opt_outs"]}
        problems = evaluate(manifest(pending_before=[433], audit=audit), SHA)
        self.assertEqual(len(problems), 2)

    def test_wrong_sha_fails_closed(self):
        problems = evaluate(manifest(), OTHER_SHA)
        self.assertTrue(problems)
        self.assertIn(SHA, problems[0])

    def test_apply_manifest_refused(self):
        problems = evaluate(manifest(mode="apply"), SHA)
        self.assertTrue(problems)

    def test_production_manifest_refused(self):
        problems = evaluate(manifest(migration_target="production"), SHA)
        self.assertTrue(problems)

    def test_audit_error_fails_closed(self):
        problems = evaluate(manifest(audit={"error": "audit verify read failed"}), SHA)
        self.assertTrue(problems)

    def test_missing_audit_fails_closed(self):
        doc = manifest()
        del doc["audit"]
        self.assertTrue(evaluate(doc, SHA))

    def test_malformed_pending_fails_closed(self):
        self.assertTrue(evaluate(manifest(pending_before=["433"]), SHA))
        self.assertTrue(evaluate(manifest(pending_before=[434, 433]), SHA))

    def test_malformed_manifest_fails_closed(self):
        for bad in (None, [], "plan", {"mode": "plan"}):
            with self.subTest(bad=bad):
                self.assertTrue(evaluate(bad, SHA))

    def test_finding_count_is_bounded_in_output(self):
        audit = {"verify_findings": [f"finding {i}" for i in range(50)]}
        problems = evaluate(manifest(audit=audit), SHA)
        self.assertEqual(len(problems), 1)
        self.assertIn("50", problems[0])
        self.assertLess(len(problems[0]), 2000)


if __name__ == "__main__":
    unittest.main()
