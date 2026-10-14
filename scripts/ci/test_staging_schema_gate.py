"""Offline gate-contract tests; no database, no Actions dispatch, no secrets."""

import json
import unittest

from staging_schema_gate import (
    Refused,
    connection_warning,
    drift_message,
    pending_versions,
    pins_from_url,
)


class PinsFromUrlTests(unittest.TestCase):
    def test_neon_url_yields_lowercase_host_database_and_empty_branch(self):
        host, database, branch = pins_from_url(
            "postgresql://two_bot_migrator_ro:pw@ep-staging-123.us-east-2.aws.neon.tech/two_bot"
        )
        self.assertEqual(host, "ep-staging-123.us-east-2.aws.neon.tech")
        self.assertEqual(database, "two_bot")
        self.assertEqual(branch, "")

    def test_pooler_port_is_refused_before_any_plan(self):
        with self.assertRaises(Refused):
            pins_from_url(
                "postgresql://two_bot_migrator_ro:pw@ep-staging-123.aws.neon.tech:6432/two_bot"
            )

    def test_pooler_username_is_refused_before_any_plan(self):
        with self.assertRaises(Refused):
            pins_from_url(
                "postgresql://two_bot_migrator_ro|bouncer:pw@ep-staging-123.aws.neon.tech/two_bot"
            )

    def test_garbage_url_is_refused(self):
        for bad in ("", "not-a-url", "postgresql://", "https://example.invalid/db"):
            with self.subTest(bad=bad):
                with self.assertRaises(Refused):
                    pins_from_url(bad)


class PendingVersionsTests(unittest.TestCase):
    def test_empty_pending_means_no_drift(self):
        self.assertEqual(pending_versions({"pending_before": []}), [])

    def test_nonempty_pending_is_returned_in_order(self):
        self.assertEqual(pending_versions({"pending_before": [12, 13]}), [12, 13])

    def test_malformed_manifest_fails_closed(self):
        for bad in ({}, {"pending_before": None}, {"pending_before": ["12"]},
                    {"pending_before": [12, 11]}, {"pending_before": True}):
            with self.subTest(bad=bad):
                with self.assertRaises(Refused):
                    pending_versions(bad)


class DriftMessageTests(unittest.TestCase):
    def test_names_pending_versions_and_manual_apply_dispatch(self):
        message = drift_message([12, 13], "a" * 40)
        self.assertIn("schema drift", message)
        self.assertIn("12", message)
        self.assertIn("13", message)
        self.assertIn("a" * 40, message)
        self.assertIn("staging-migrate", message)
        self.assertIn("apply", message)


class ConnectionWarningTests(unittest.TestCase):
    def test_no_usage_means_no_warning(self):
        self.assertIsNone(connection_warning({}))

    def test_hot_pool_warns_without_failing(self):
        warning = connection_warning({"connection_usage": {"ratio": 0.96}})
        self.assertIsNotNone(warning)
        self.assertIn("connection", warning)


if __name__ == "__main__":
    unittest.main()
