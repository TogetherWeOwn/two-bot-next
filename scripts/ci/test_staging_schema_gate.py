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
        manifest = {
            "pending_before": [],
            "audit": {"connection_usage": {"used": 25, "max_connections": 25}},
        }
        warning = connection_warning(manifest)
        self.assertIsNotNone(warning)
        self.assertIn("25/25", warning)

    def test_warning_boundary_is_strictly_above_eighty_percent(self):
        def manifest(used, maximum):
            return {"audit": {"connection_usage": {"used": used, "max_connections": maximum}}}

        self.assertIsNone(connection_warning(manifest(80, 100)))
        self.assertIsNotNone(connection_warning(manifest(81, 100)))

    def test_degenerate_or_misplaced_usage_never_warns(self):
        for bad in (
            {"audit": None},
            {"audit": {}},
            {"audit": {"connection_usage": None}},
            {"audit": {"connection_usage": {"used": 25}}},
            {"audit": {"connection_usage": {"max_connections": 25}}},
            {"audit": {"connection_usage": {"used": -1, "max_connections": 25}}},
            {"audit": {"connection_usage": {"used": 25, "max_connections": 0}}},
            {"audit": {"connection_usage": {"used": True, "max_connections": 25}}},
            # Top-level usage is not the emitted shape; it must not warn.
            {"connection_usage": {"used": 25, "max_connections": 25}},
        ):
            with self.subTest(bad=bad):
                self.assertIsNone(connection_warning(bad))


if __name__ == "__main__":
    unittest.main()
