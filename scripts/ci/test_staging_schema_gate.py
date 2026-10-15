"""Offline gate-contract tests; no database, no Actions dispatch, no secrets."""

import json
import unittest

from staging_schema_gate import (
    Refused,
    branch_id_valid,
    connection_warning,
    drift_message,
    pending_versions,
    pins_from_url,
    verify_binding,
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


class VerifyBindingTests(unittest.TestCase):
    """The binding is compared with independently pinned vars, never itself."""

    STAGING_HOST = "ep-staging-123.us-east-2.aws.neon.tech"
    STAGING_DB = "two_bot"
    STAGING_URL = f"postgresql://two_bot_migrator_ro:pw@{STAGING_HOST}/{STAGING_DB}"
    PSDB_HOST = "myapp.pg.psdb.cloud"
    PSDB_BRANCH = "cnlmx96ec5kw"
    PSDB_URL = f"postgresql://two-bot-migrator.{PSDB_BRANCH}:pw@{PSDB_HOST}:5432/postgres"

    def test_matching_neon_binding_passes_and_returns_pins(self):
        pins = verify_binding(self.STAGING_URL, self.STAGING_HOST, self.STAGING_DB, "")
        self.assertEqual(pins, (self.STAGING_HOST, self.STAGING_DB, ""))

    def test_wrong_target_binding_is_refused(self):
        # A production-class binding aimed at the gate refuses even though its
        # own shape parses: the pins come from vars, not from the URL.
        prod_url = "postgresql://two_bot_migrator_ro:pw@ep-prod-999.us-east-2.aws.neon.tech/two_bot"
        with self.assertRaises(Refused):
            verify_binding(prod_url, self.STAGING_HOST, self.STAGING_DB, "")
        wrong_db = f"postgresql://two_bot_migrator_ro:pw@{self.STAGING_HOST}/other_db"
        with self.assertRaises(Refused):
            verify_binding(wrong_db, self.STAGING_HOST, self.STAGING_DB, "")

    def test_matching_planetscale_branch_passes(self):
        pins = verify_binding(self.PSDB_URL, self.PSDB_HOST, "postgres", self.PSDB_BRANCH)
        self.assertEqual(pins, (self.PSDB_HOST, "postgres", self.PSDB_BRANCH))

    def test_planetscale_branch_mismatch_is_refused(self):
        with self.assertRaises(Refused):
            verify_binding(self.PSDB_URL, self.PSDB_HOST, "postgres", "otherbranch1")
        # Unpinned dispatch against a PlanetScale binding refuses.
        with self.assertRaises(Refused):
            verify_binding(self.PSDB_URL, self.PSDB_HOST, "postgres", "")

    def test_missing_expected_branch_pin_is_refused(self):
        with self.assertRaises(Refused):
            verify_binding(self.STAGING_URL, self.PSDB_HOST, "postgres", "")

    def test_production_like_or_pooler_expected_pins_are_refused(self):
        for host, database in (
            ("ep-prod-1.aws.neon.tech", self.STAGING_DB),
            (self.STAGING_HOST, "two_bot_prod"),
            ("ep-pooler-staging.aws.neon.tech", self.STAGING_DB),
            ("", self.STAGING_DB),
            (self.STAGING_HOST, ""),
        ):
            with self.subTest(host=host, database=database):
                with self.assertRaises(Refused):
                    verify_binding(self.STAGING_URL, host, database, "")

    def test_malformed_expected_branch_is_refused(self):
        for bad in ("not a branch!", "x" * 64, ""):
            if not bad:
                continue
            with self.subTest(bad=bad):
                with self.assertRaises(Refused):
                    verify_binding(self.PSDB_URL, self.PSDB_HOST, "postgres", bad)

    def test_branch_id_shape(self):
        self.assertTrue(branch_id_valid("cnlmx96ec5kw"))
        self.assertFalse(branch_id_valid(""))
        self.assertFalse(branch_id_valid("not a branch!"))


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

    def test_effective_ceiling_from_reserved_warns(self):
        # 18/25 is 72% of raw max but 82% of the 22-login ceiling.
        manifest = {"audit": {"connection_usage": {
            "used": 18, "max_connections": 25, "reserved_connections": 3}}}
        warning = connection_warning(manifest)
        self.assertIsNotNone(warning)
        self.assertIn("18/22", warning)
        self.assertIn("non-superuser", warning)

    def test_effective_max_field_warns_without_reserved(self):
        manifest = {"audit": {"connection_usage": {
            "used": 18, "max_connections": 25, "effective_max_connections": 22}}}
        warning = connection_warning(manifest)
        self.assertIsNotNone(warning)
        self.assertIn("18/22", warning)
        self.assertIn("non-superuser", warning)

    def test_older_manifest_without_ceiling_uses_raw_max(self):
        def manifest(used):
            return {"audit": {"connection_usage": {"used": used, "max_connections": 25}}}

        self.assertIsNone(connection_warning(manifest(20)))
        warning = connection_warning(manifest(21))
        self.assertIsNotNone(warning)
        self.assertIn("21/25", warning)
        self.assertIn("of max_connections", warning)

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
