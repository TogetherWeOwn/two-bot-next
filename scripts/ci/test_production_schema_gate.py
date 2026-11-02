"""Offline gate-contract tests; no database, no Actions dispatch, no secrets."""

import json
import os
import tempfile
import types
import unittest
from pathlib import Path
from unittest import mock

import production_schema_gate
from production_schema_gate import (
    Refused,
    connection_warning,
    drift_message,
    is_staging_host,
    main,
    pending_versions,
    pins_from_url,
    run_gate,
    verify_binding,
    waive_reason,
)

PROD_HOST = "two-production-abc123.us-east-1.aws.example.com"
PROD_DB = "two_bot"
PROD_URL = f"postgresql://two_bot_migrator_ro:pw@{PROD_HOST}:5432/{PROD_DB}"
SHA = "a" * 40


def env(url=PROD_URL, host=PROD_HOST, database=PROD_DB, branch=""):
    return {
        "TWO_BOT_PRODUCTION_PLAN_DATABASE_URL": url,
        "TWO_BOT_PRODUCTION_HOST": host,
        "TWO_BOT_PRODUCTION_DATABASE": database,
        "TWO_BOT_PRODUCTION_BRANCH_ID": branch,
    }


def plan_report(**fields):
    manifest = {
        "runner_version": 1, "mode": "plan", "migration_target": "production",
        "source_sha": SHA, "role": "two_bot_migrator_ro",
        "target": {"host": PROD_HOST, "database": PROD_DB, "branch_id": ""},
        "applied_count": 0, "applied_store_count": 0,
        "pending_before": [], "pending_store_before": [],
    }
    manifest.update(fields)
    return manifest


class PinsFromUrlTests(unittest.TestCase):
    def test_direct_url_yields_lowercase_host_and_database(self):
        host, database, branch = pins_from_url(
            "postgresql://two_bot_migrator_ro:pw@Two-Production-1.Example.COM:5432/two_bot"
        )
        self.assertEqual(host, "two-production-1.example.com")
        self.assertEqual(database, "two_bot")
        self.assertEqual(branch, "")

    def test_planetscale_url_yields_branch_from_username_suffix(self):
        host, database, branch = pins_from_url(
            "postgresql://two-bot-migrator.cnlmx96ec5kw:pw@myapp.pg.psdb.cloud:5432/postgres"
        )
        self.assertEqual(host, "myapp.pg.psdb.cloud")
        self.assertEqual(database, "postgres")
        self.assertEqual(branch, "cnlmx96ec5kw")

    def test_pooler_port_is_refused_before_any_plan(self):
        with self.assertRaises(Refused):
            pins_from_url(f"postgresql://two_bot_migrator_ro:pw@{PROD_HOST}:6432/{PROD_DB}")

    def test_pooler_username_is_refused_before_any_plan(self):
        with self.assertRaises(Refused):
            pins_from_url(
                f"postgresql://two_bot_migrator_ro|bouncer:pw@{PROD_HOST}:5432/{PROD_DB}"
            )

    def test_garbage_url_is_refused(self):
        for bad in ("", "not-a-url", "postgresql://", "https://example.invalid/db"):
            with self.subTest(bad=bad):
                with self.assertRaises(Refused):
                    pins_from_url(bad)

    def test_malformed_port_is_a_fixed_refusal_that_never_echoes_input(self):
        for port in ("notaportSENTINEL", "99999", "-1"):
            url = f"postgresql://two_bot_migrator_ro:pwSENTINEL@{PROD_HOST}:{port}/{PROD_DB}"
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
                    f"postgresql://two_bot_migrator_ro:pw@{PROD_HOST}{suffix}/{PROD_DB}"
                )
                self.assertEqual((host, database, branch), (PROD_HOST, PROD_DB, ""))


class VerifyBindingTests(unittest.TestCase):
    """The binding is compared with independently pinned vars, never itself."""

    def test_matching_binding_passes_and_returns_pins(self):
        self.assertEqual(verify_binding(PROD_URL, PROD_HOST, PROD_DB, ""), (PROD_HOST, PROD_DB, ""))

    def test_wrong_target_binding_is_refused(self):
        other = f"postgresql://two_bot_migrator_ro:pw@{PROD_HOST}:5432/other_db"
        with self.assertRaises(Refused):
            verify_binding(other, PROD_HOST, PROD_DB, "")
        wrong_host = f"postgresql://two_bot_migrator_ro:pw@other-host.example.com:5432/{PROD_DB}"
        with self.assertRaises(Refused):
            verify_binding(wrong_host, PROD_HOST, PROD_DB, "")

    def test_prod_like_pins_stay_allowed(self):
        # The production host and database are production-like by
        # construction; only the staging fence applies.
        self.assertEqual(
            verify_binding(
                "postgresql://two_bot_migrator_ro:pw@ep-prod-1.example.com:5432/two_bot_prod",
                "ep-prod-1.example.com",
                "two_bot_prod",
                "",
            ),
            ("ep-prod-1.example.com", "two_bot_prod", ""),
        )

    def test_staging_pins_and_bindings_are_refused(self):
        staging_url = "postgresql://two_bot_migrator_ro:pw@ep-staging-1.aws.neon.tech/two_bot"
        with self.assertRaises(Refused):
            verify_binding(staging_url, "ep-staging-1.aws.neon.tech", "two_bot", "")
        with self.assertRaises(Refused):
            verify_binding(PROD_URL, "ep-staging-1.aws.neon.tech", PROD_DB, "")
        with self.assertRaises(Refused):
            verify_binding(staging_url, PROD_HOST, PROD_DB, "")

    def test_planetscale_binding_refuses_without_a_branch_pin(self):
        psdb = "postgresql://two-bot-migrator.cnlmx96ec5kw:pw@myapp.pg.psdb.cloud:5432/postgres"
        with self.assertRaises(Refused):
            verify_binding(psdb, "myapp.pg.psdb.cloud", "postgres", "")

    def test_planetscale_binding_passes_with_matching_branch_pin(self):
        psdb = "postgresql://two-bot-migrator.cnlmx96ec5kw:pw@myapp.pg.psdb.cloud:5432/postgres"
        self.assertEqual(
            verify_binding(psdb, "myapp.pg.psdb.cloud", "postgres", "cnlmx96ec5kw"),
            ("myapp.pg.psdb.cloud", "postgres", "cnlmx96ec5kw"),
        )

    def test_planetscale_binding_refuses_mismatched_or_malformed_branch_pin(self):
        psdb = "postgresql://two-bot-migrator.cnlmx96ec5kw:pw@myapp.pg.psdb.cloud:5432/postgres"
        for branch in ("otherbranch1", "bad branch", "bad/branch"):
            with self.subTest(branch=branch):
                with self.assertRaises(Refused):
                    verify_binding(psdb, "myapp.pg.psdb.cloud", "postgres", branch)

    def test_empty_or_pooler_expected_pins_are_refused(self):
        for host, database in (
            ("", PROD_DB),
            (PROD_HOST, ""),
            ("direct-pooler.example.com", PROD_DB),
        ):
            with self.subTest(host=host, database=database):
                with self.assertRaises(Refused):
                    verify_binding(PROD_URL, host, database, "")

    def test_staging_host_fence(self):
        for host in ("agent-testdb", "127.0.0.1", "localhost",
                     "ep-staging-1.aws.neon.tech", "ep-x.us-east-2.aws.neon.tech"):
            self.assertTrue(is_staging_host(host), host)
        for host in ("two-production-1.example.com", "ep-prod-1.example.com"):
            self.assertFalse(is_staging_host(host), host)


class RunGateArgvTests(unittest.TestCase):
    """run_gate must forward the identity pins to the binary, not just to main."""

    def capture_argv(self, *args):
        captured = []

        def fake_run(argv, **kwargs):
            captured.append(list(argv))
            return types.SimpleNamespace(
                returncode=0,
                stdout=json.dumps({"pending_before": [], "pending_store_before": []}) + "\n",
                stderr="",
            )

        with mock.patch.object(production_schema_gate.subprocess, "run", fake_run):
            manifest = run_gate("bin", SHA, *args)
        self.assertEqual(manifest["pending_before"], [])
        self.assertEqual(len(captured), 1)
        return captured[0]

    def flag_value(self, argv, flag):
        self.assertIn(flag, argv)
        return argv[argv.index(flag) + 1]

    def test_forwards_branch_pin_for_planetscale_host(self):
        argv = self.capture_argv(
            "postgresql://two-bot-migrator.cnlmx96ec5kw:pw@myapp.pg.psdb.cloud:5432/postgres",
            "myapp.pg.psdb.cloud",
            "postgres",
            "cnlmx96ec5kw",
        )
        self.assertEqual(self.flag_value(argv, "--production-branch-id"), "cnlmx96ec5kw")
        self.assertEqual(self.flag_value(argv, "--production-host"), "myapp.pg.psdb.cloud")
        self.assertEqual(self.flag_value(argv, "--production-database"), "postgres")

    def test_forwards_empty_branch_for_non_planetscale_host(self):
        # The binary treats an empty --production-branch-id as no-pin.
        argv = self.capture_argv(PROD_URL, PROD_HOST, PROD_DB, "")
        self.assertEqual(self.flag_value(argv, "--production-branch-id"), "")

    def test_malformed_port_refuses_before_the_plan_subprocess(self):
        bad = f"postgresql://two_bot_migrator_ro:pwSENTINEL@{PROD_HOST}:notaportSENTINEL/{PROD_DB}"
        with mock.patch.object(production_schema_gate.subprocess, "run") as run_spy:
            with self.assertRaises(Refused) as caught:
                run_gate("bin", SHA, bad, PROD_HOST, PROD_DB, "")
        run_spy.assert_not_called()
        self.assertNotIn("SENTINEL", str(caught.exception))


class WaiveReasonTests(unittest.TestCase):
    def test_rollback_waives(self):
        self.assertIsNotNone(waive_reason("rollback", "false"))

    def test_live_sha_takeover_redeploy_waives(self):
        self.assertIsNotNone(waive_reason("deploy", "true"))

    def test_new_build_must_run_the_gate(self):
        self.assertIsNone(waive_reason("deploy", "false"))


class MainTests(unittest.TestCase):
    def run_main(self, argv, run, manifest_out):
        return main(argv + ["--manifest-out", str(manifest_out)], env=env(), run=run)

    def test_rollback_waives_without_reading_the_database(self):
        def refuse_run(*args):
            raise AssertionError("waived runs must not reach the database")

        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "manifest.json"
            self.assertEqual(
                self.run_main(["--binary", "bin", "--source-sha", SHA,
                               "--mode", "rollback"], refuse_run, out), 0)
            manifest = json.loads(out.read_text())
            self.assertIn("waived", manifest)
            self.assertEqual(manifest["mode"], "rollback")

    def test_live_sha_redeploy_waives_without_reading_the_database(self):
        def refuse_run(*args):
            raise AssertionError("waived runs must not reach the database")

        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "manifest.json"
            self.assertEqual(
                self.run_main(["--binary", "bin", "--source-sha", SHA,
                               "--mode", "deploy", "--allow-unstaged", "true"],
                              refuse_run, out), 0)
            self.assertIn("waived", json.loads(out.read_text()))

    def test_main_forwards_branch_pin_from_env(self):
        seen = []
        host, database, branch = "myapp.pg.psdb.cloud", "postgres", "cnlmx96ec5kw"

        def capture(*args):
            seen.append(args)
            return plan_report(target={"host": host, "database": database, "branch_id": branch})

        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "manifest.json"
            self.assertEqual(
                main(["--binary", "bin", "--source-sha", SHA, "--mode", "deploy",
                      "--manifest-out", str(out)],
                     env=env(url=f"postgresql://two-bot-migrator-ro.{branch}:pw@{host}/{database}",
                             host=host, database=database, branch=branch), run=capture), 0)
            self.assertEqual(seen[0][-1], "cnlmx96ec5kw")

    def test_clean_plan_passes(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "manifest.json"
            self.assertEqual(
                self.run_main(["--binary", "bin", "--source-sha", SHA,
                               "--mode", "deploy"],
                              lambda *args: plan_report(), out), 0)
            manifest = json.loads(out.read_text())
            self.assertEqual(manifest["pending_before"], [])
            self.assertEqual(manifest["pending_store_before"], [])

    def test_store_chain_drift_fails_even_when_cutover_is_clean(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "manifest.json"
            result = self.run_main(
                ["--binary", "bin", "--source-sha", SHA, "--mode", "deploy"],
                lambda *args: plan_report(pending_store_before=[400, 401]), out)
            self.assertEqual(result, 1)
            self.assertEqual(
                json.loads(out.read_text())["pending_store_before"], [400, 401])

    def test_malformed_store_pending_refuses_the_gate(self):
        for store_pending in (None, ["400"], [401, 400], True):
            with self.subTest(store_pending=store_pending), tempfile.TemporaryDirectory() as tmp:
                out = Path(tmp) / "manifest.json"
                with self.assertRaises(Refused):
                    self.run_main(
                        ["--binary", "bin", "--source-sha", SHA, "--mode", "deploy"],
                        lambda *args: plan_report(pending_store_before=store_pending), out)

    def test_drift_fails_with_the_pending_list(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "manifest.json"
            self.assertEqual(
                self.run_main(["--binary", "bin", "--source-sha", SHA,
                               "--mode", "deploy"],
                              lambda *args: plan_report(pending_before=[424, 425]), out), 1)

    def test_refused_binding_exits_unusable(self):
        def refuse(*args):
            raise Refused("plan database binding is not provided")

        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "manifest.json"
            with self.assertRaises(Refused):
                self.run_main(["--binary", "bin", "--source-sha", SHA,
                               "--mode", "deploy"], refuse, out)

    def test_malformed_port_writes_no_manifest(self):
        bad = f"postgresql://two_bot_migrator_ro:pwSENTINEL@{PROD_HOST}:99999/{PROD_DB}"
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "manifest.json"
            with mock.patch.object(production_schema_gate.subprocess, "run") as run_spy:
                with self.assertRaises(Refused) as caught:
                    main(["--binary", "bin", "--source-sha", SHA,
                          "--mode", "deploy", "--manifest-out", str(out)],
                         env=env(url=bad))
            run_spy.assert_not_called()
            self.assertFalse(out.exists())
            self.assertNotIn("SENTINEL", str(caught.exception))
            self.assertNotIn("99999", str(caught.exception))

    def test_short_sha_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "manifest.json"
            with self.assertRaises(Refused):
                self.run_main(["--binary", "bin", "--source-sha", "abc",
                               "--mode", "deploy"],
                              lambda *args: plan_report(), out)


class PendingVersionsTests(unittest.TestCase):
    def test_empty_pending_means_no_drift(self):
        self.assertEqual(pending_versions({"pending_before": [], "pending_store_before": []}), [])

    def test_nonempty_pending_is_returned_in_order(self):
        self.assertEqual(pending_versions({"pending_before": [424, 425]}), [424, 425])

    def test_empty_store_pending_means_no_drift(self):
        self.assertEqual(pending_versions({"pending_store_before": []}, "pending_store_before"), [])

    def test_malformed_manifest_fails_closed(self):
        for bad in ({}, {"pending_before": None}, {"pending_before": ["424"]},
                    {"pending_before": [425, 424]}, {"pending_before": True}):
            with self.subTest(bad=bad):
                with self.assertRaises(Refused):
                    pending_versions(bad)
        for bad in ({}, {"pending_store_before": None},
                    {"pending_store_before": ["424"]},
                    {"pending_store_before": [425, 424]},
                    {"pending_store_before": True}):
            with self.subTest(bad=bad):
                with self.assertRaises(Refused):
                    pending_versions(bad, "pending_store_before")


class DriftMessageTests(unittest.TestCase):
    def test_names_pending_versions_and_manual_apply_dispatch(self):
        message = drift_message([424, 425], SHA)
        self.assertIn("schema drift", message)
        self.assertIn("424", message)
        self.assertIn("425", message)
        self.assertIn(SHA, message)
        self.assertIn("production-migrate", message)
        self.assertIn("apply", message)
        self.assertIn("deploy-production", message)
        self.assertIn("cutover chain", message)
        self.assertIn("expected_pending", message)

    def test_names_only_store_chain_and_its_apply_pin(self):
        message = drift_message([], SHA, [400, 401])
        self.assertIn("store chain", message)
        self.assertIn("400,401", message)
        self.assertIn("expected_store_pending", message)
        self.assertNotIn("expected_pending", message)
        self.assertIn("production-migrate", message)
        self.assertIn("deploy-production", message)

    def test_names_both_lagging_chains(self):
        message = drift_message([424], SHA, [400])
        self.assertIn("cutover chain pending versions [424]", message)
        self.assertIn("store chain pending versions [400]", message)
        self.assertIn("expected_pending", message)
        self.assertIn("expected_store_pending", message)


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

    def run_with_stdout(self, stdout):
        def fake_run(argv, **kwargs):
            return types.SimpleNamespace(returncode=0, stdout=stdout, stderr="")

        with mock.patch.object(production_schema_gate.subprocess, "run", fake_run):
            return run_gate("bin", SHA, PROD_URL, PROD_HOST, PROD_DB, "")

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
