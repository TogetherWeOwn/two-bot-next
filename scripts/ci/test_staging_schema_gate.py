"""Offline gate-contract tests; no database, no Actions dispatch, no secrets."""

import json
from pathlib import Path
import tempfile
import types
import unittest
from unittest import mock

import staging_schema_gate
from staging_schema_gate import (
    main,
    Refused,
    branch_id_valid,
    connection_warning,
    drift_message,
    pending_versions,
    pins_from_url,
    run_gate,
    verify_binding,
)


STAGING_HOST = "ep-staging-123.us-east-2.aws.neon.tech"


def plan_report(**fields):
    manifest = {
        "runner_version": 1, "mode": "plan", "migration_target": "staging",
        "source_sha": "a" * 40, "role": "two_bot_migrator_ro",
        "target": {"host": STAGING_HOST, "database": "two_bot", "branch_id": ""},
        "applied_count": 0, "applied_store_count": 0,
        "pending_before": [], "pending_store_before": [],
    }
    manifest.update(fields)
    return manifest


def plan_env():
    return {
        staging_schema_gate.PLAN_URL_ENV: f"postgresql://two_bot_migrator_ro:pw@{STAGING_HOST}/two_bot",
        staging_schema_gate.EXPECTED_HOST_ENV: STAGING_HOST,
        staging_schema_gate.EXPECTED_DATABASE_ENV: "two_bot",
        staging_schema_gate.EXPECTED_BRANCH_ENV: "",
    }


class MainTests(unittest.TestCase):
    def test_store_pending_fails_and_clean_manifest_passes(self):
        source_sha = "a" * 40
        with tempfile.TemporaryDirectory() as directory:
            manifest_out = f"{directory}/manifest.json"

            def run_with_store_pending(*_args):
                return plan_report(pending_store_before=[400])

            result = main(
                ["--binary", "staging-migrate", "--source-sha", source_sha,
                 "--manifest-out", manifest_out],
                env=plan_env(),
                run=run_with_store_pending,
            )
            self.assertEqual(result, 1)

            def run_clean(*_args):
                return plan_report()

            result = main(
                ["--binary", "staging-migrate", "--source-sha", source_sha,
                 "--manifest-out", manifest_out],
                env=plan_env(),
                run=run_clean,
            )
            self.assertEqual(result, 0)

    def test_malformed_port_refuses_before_subprocess_and_writes_no_manifest(self):
        env = {
            staging_schema_gate.PLAN_URL_ENV: (
                "postgresql://two_bot_migrator_ro:pwSENTINEL"
                "@ep-staging-123.aws.neon.tech:notaportSENTINEL/two_bot"
            ),
            staging_schema_gate.EXPECTED_HOST_ENV: "ep-staging-123.aws.neon.tech",
            staging_schema_gate.EXPECTED_DATABASE_ENV: "two_bot",
            staging_schema_gate.EXPECTED_BRANCH_ENV: "",
        }
        with tempfile.TemporaryDirectory() as directory:
            manifest_out = Path(directory) / "manifest.json"
            with mock.patch.object(staging_schema_gate.subprocess, "run") as run_spy:
                with self.assertRaises(Refused) as caught:
                    main(["--binary", "staging-migrate", "--source-sha", "a" * 40,
                          "--manifest-out", str(manifest_out)], env=env)
            run_spy.assert_not_called()
            self.assertFalse(manifest_out.exists())
            self.assertNotIn("SENTINEL", str(caught.exception))


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

    def test_malformed_port_is_a_fixed_refusal_that_never_echoes_input(self):
        for port in ("notaportSENTINEL", "99999", "-1"):
            url = (f"postgresql://two_bot_migrator_ro:pwSENTINEL"
                   f"@ep-staging-123.aws.neon.tech:{port}/two_bot")
            with self.subTest(port=port):
                with self.assertRaises(Refused) as caught:
                    pins_from_url(url)
                message = str(caught.exception)
                self.assertEqual(message, "plan database binding is not a valid database URL")
                self.assertNotIn("SENTINEL", message)
                self.assertNotIn(port, message)
                self.assertNotIn("\n", message)

    def test_absent_and_direct_ports_keep_parsing(self):
        for suffix in ("", ":5432"):
            with self.subTest(suffix=suffix):
                host, database, branch = pins_from_url(
                    f"postgresql://two_bot_migrator_ro:pw@ep-staging-123.aws.neon.tech{suffix}/two_bot"
                )
                self.assertEqual((host, database, branch),
                                 ("ep-staging-123.aws.neon.tech", "two_bot", ""))


class VerifyBindingTests(unittest.TestCase):
    """The binding is compared with independently pinned vars, never itself."""

    STAGING_HOST = "ep-staging-123.us-east-2.aws.neon.tech"
    STAGING_DB = "two_bot"
    STAGING_URL = f"postgresql://two_bot_migrator_ro:pw@{STAGING_HOST}/{STAGING_DB}"
    PSDB_HOST = "myapp.pg.psdb.cloud"
    PSDB_BRANCH = "cnlmx96ec5kw"
    PSDB_URL = f"postgresql://two-bot-migrator.{PSDB_BRANCH}:pw@{PSDB_HOST}:5432/postgres"
    SHA = "a" * 40

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

    def test_store_pending_reads_its_own_field(self):
        manifest = {"pending_before": [], "pending_store_before": [400, 401]}
        self.assertEqual(pending_versions(manifest, "pending_store_before"), [400, 401])
        self.assertEqual(pending_versions(manifest), [])

    def test_malformed_manifest_fails_closed(self):
        for bad in ({}, {"pending_before": None}, {"pending_before": ["12"]},
                    {"pending_before": [12, 11]}, {"pending_before": True}):
            with self.subTest(bad=bad):
                with self.assertRaises(Refused):
                    pending_versions(bad)
        # A manifest without the store field fails closed too: the gate must
        # see both chains before calling a deploy clean.
        for bad in ({}, {"pending_store_before": None}, {"pending_store_before": ["400"]},
                    {"pending_store_before": [401, 400]}):
            with self.subTest(bad=bad):
                with self.assertRaises(Refused):
                    pending_versions(bad, "pending_store_before")


class DriftMessageTests(unittest.TestCase):
    def test_names_pending_versions_and_manual_apply_dispatch(self):
        message = drift_message([12, 13], "a" * 40)
        self.assertIn("schema drift", message)
        self.assertIn("12", message)
        self.assertIn("13", message)
        self.assertIn("a" * 40, message)
        self.assertIn("staging-migrate", message)
        self.assertIn("apply", message)
        self.assertNotIn("Store chain", message)

    def test_store_drift_names_the_store_fix_and_role_restore_sequence(self):
        message = drift_message([], "a" * 40, [400, 401])
        self.assertIn("schema drift", message)
        self.assertIn("400", message)
        self.assertIn("expected_store_pending", message)
        self.assertIn("reapply the full role-plan", message)
        self.assertIn("two-bot db roles plan", message)
        self.assertIn("verify", message)
        self.assertIn("redeploy", message)


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


class DuplicateManifestRefusalTests(unittest.TestCase):
    """Duplicate manifest fields refuse with fixed vocabulary, never last-wins.

    A duplicated ``pending_before`` must not read last-wins (the gate
    could pass on the second copy); every object at any depth is
    strict, and an excessively nested manifest maps to the existing
    invalid-JSON refusal instead of escaping as a traceback.
    """

    STAGING_HOST = "ep-staging-123.us-east-2.aws.neon.tech"
    STAGING_DB = "two_bot"
    STAGING_URL = f"postgresql://two_bot_migrator_ro:pw@{STAGING_HOST}/{STAGING_DB}"
    SHA = "a" * 40

    def run_with_stdout(self, stdout):
        def fake_run(argv, **kwargs):
            return types.SimpleNamespace(returncode=0, stdout=stdout, stderr="")

        with mock.patch.object(staging_schema_gate.subprocess, "run", fake_run):
            return run_gate("bin", self.SHA, self.STAGING_URL,
                            self.STAGING_HOST, self.STAGING_DB, "")

    def test_top_level_duplicate_pending_refused_both_orders(self):
        for stdout in (
            '{"pending_before": [], "pending_before": [424], "pending_store_before": []}\n',
            '{"pending_before": [424], "pending_before": [], "pending_store_before": []}\n',
        ):
            with self.subTest(stdout=stdout[:44]):
                with self.assertRaises(Refused) as caught:
                    self.run_with_stdout(stdout)
                self.assertEqual(str(caught.exception), "plan manifest has duplicate fields")
                self.assertNotIn("424", str(caught.exception))

    def test_nested_duplicate_refused_without_echo(self):
        stdout = (
            '{"pending_before": [], "pending_store_before": [], '
            '"audit": {"connection_usage": {"used": 25, "used": 1, '
            '"max_connections": 25}}}\n'
        )
        with self.assertRaises(Refused) as caught:
            self.run_with_stdout(stdout)
        self.assertEqual(str(caught.exception), "plan manifest has duplicate fields")

    def test_escaped_key_spelling_counts_as_duplicate(self):
        stdout = (
            '{"pending_before": [], "pending_store_before": [], '
            '"audit": {"used": 25, "\\u0075sed": 1}}\n'
        )
        with self.assertRaises(Refused) as caught:
            self.run_with_stdout(stdout)
        self.assertEqual(str(caught.exception), "plan manifest has duplicate fields")

    def test_deeply_nested_manifest_maps_recursion_to_invalid_json(self):
        stdout = '{"a":' * 10000 + "0" + "}" * 10000 + "\n"
        with self.assertRaises(Refused) as caught:
            self.run_with_stdout(stdout)
        self.assertEqual(str(caught.exception), "plan manifest is not valid JSON")

    def test_overlong_integer_maps_digit_cap_to_invalid_json(self):
        stdout = '{"pending_before": [%s], "pending_store_before": []}\n' % ("9" * 5000)
        with self.assertRaises(Refused) as caught:
            self.run_with_stdout(stdout)
        self.assertEqual(str(caught.exception), "plan manifest is not valid JSON")

    def test_unique_nested_manifest_still_parses(self):
        stdout = (
            '{"pending_before": [], "pending_store_before": [], '
            '"audit": {"connection_usage": {"used": 25, "max_connections": 25}}}\n'
        )
        manifest = self.run_with_stdout(stdout)
        self.assertEqual(manifest["pending_before"], [])
        self.assertEqual(manifest["pending_store_before"], [])
        self.assertIsNotNone(connection_warning(manifest))


if __name__ == "__main__":
    unittest.main()
