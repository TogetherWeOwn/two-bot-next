"""Offline role-gate contract tests; no database, no Actions dispatch, no secrets."""

import json
import os
import tempfile
import unittest

from roles_gate import (
    Refused,
    drift_message,
    findings_from_output,
    main,
    run_gate,
    validated_findings,
    verify_binding,
    waive_reason,
)


STAGING_HOST = "ep-staging-123.us-east-2.aws.neon.tech"
STAGING_DB = "two_bot"
STAGING_URL = f"postgresql://two_bot_migrator_ro:pw@{STAGING_HOST}/{STAGING_DB}"
PROD_HOST = "prod-db-1.example.com"
PROD_DB = "two_bot"
PROD_URL = f"postgresql://two_bot_migrator_ro:pw@{PROD_HOST}/{PROD_DB}"


def gate_args(target="staging", **overrides):
    args = {
        "--binary": "./target/debug/two-bot",
        "--target": target,
        "--expected-host": STAGING_HOST if target == "staging" else PROD_HOST,
        "--expected-database": STAGING_DB if target == "staging" else PROD_DB,
        "--expected-branch-id": "",
    }
    args.update({f"--{key}": value for key, value in overrides.items()})
    return [item for pair in args.items() for item in pair]


class FindingsTests(unittest.TestCase):
    def test_empty_findings_list_passes(self):
        self.assertEqual(validated_findings([]), [])

    def test_nonempty_findings_list_is_returned(self):
        findings = ["missing grant SELECT on public.member_events to two_bot_runtime"]
        self.assertEqual(validated_findings(findings), findings)

    def test_malformed_findings_fail_closed(self):
        for bad in (None, {}, "drift", [12], [""], [None], [{"finding": 1}], True):
            with self.subTest(bad=bad):
                with self.assertRaises(Refused):
                    validated_findings(bad)

    def test_drift_lines_are_collected_without_contents(self):
        stdout = "DATABASE ROLES VERIFIED\n"
        stderr = (
            "drift: missing grant SELECT on public.member_events to two_bot_runtime\n"
            "drift: unexpected grant DELETE on public.guild_settings to two_bot_runtime\n"
            "DATABASE ROLE DRIFT: 2 finding(s)\n"
        )
        self.assertEqual(findings_from_output(stdout, stderr), [
            "missing grant SELECT on public.member_events to two_bot_runtime",
            "unexpected grant DELETE on public.guild_settings to two_bot_runtime",
        ])

    def test_clean_output_yields_no_findings(self):
        self.assertEqual(
            findings_from_output(
                "DATABASE ROLES VERIFIED (groups only; login bindings require operator audit)\n", "",
            ),
            [],
        )

    def test_empty_drift_lines_are_dropped(self):
        self.assertEqual(findings_from_output("", "drift: \ndrift: x\n"), ["x"])


class DriftMessageTests(unittest.TestCase):
    def test_names_count_target_and_operator_fix(self):
        message = drift_message(["a", "b"], "staging")
        self.assertIn("role drift", message)
        self.assertIn("staging", message)
        self.assertIn("2 finding(s)", message)
        self.assertIn("two-bot db roles plan", message)
        self.assertIn("two-bot db roles verify", message)
        self.assertIn("reads 0", message)
        self.assertIn("deploy-staging", message)

    def test_production_names_redispatch(self):
        message = drift_message(["a"], "production")
        self.assertIn("production", message)
        self.assertIn("1 finding(s)", message)
        self.assertIn("deploy-production", message)

    def test_empty_findings_have_no_drift_message(self):
        with self.assertRaises(Refused):
            drift_message([], "staging")

    def test_unknown_target_is_refused(self):
        with self.assertRaises(Refused):
            drift_message(["a"], "canary")


class VerifyBindingTests(unittest.TestCase):
    """The binding is compared with independently pinned vars, never itself."""

    def test_matching_staging_binding_passes(self):
        self.assertEqual(
            verify_binding("staging", STAGING_URL, STAGING_HOST, STAGING_DB, ""),
            (STAGING_HOST, STAGING_DB, ""),
        )

    def test_matching_production_binding_passes(self):
        self.assertEqual(
            verify_binding("production", PROD_URL, PROD_HOST, PROD_DB, ""),
            (PROD_HOST, PROD_DB, ""),
        )

    def test_cross_target_binding_is_refused(self):
        with self.assertRaises(Refused):
            verify_binding("staging", PROD_URL, STAGING_HOST, STAGING_DB, "")
        with self.assertRaises(Refused):
            verify_binding("production", STAGING_URL, PROD_HOST, PROD_DB, "")

    def test_unknown_target_is_refused(self):
        with self.assertRaises(Refused):
            verify_binding("canary", STAGING_URL, STAGING_HOST, STAGING_DB, "")


class WaiveTests(unittest.TestCase):
    def test_deploy_never_waives(self):
        self.assertIsNone(waive_reason("deploy", "false"))

    def test_rollback_waives_without_new_table_grants(self):
        self.assertIsNotNone(waive_reason("rollback", "false"))

    def test_live_sha_redeploy_waives(self):
        self.assertIsNotNone(waive_reason("deploy", "true"))


class RunGateTests(unittest.TestCase):
    def invoke(self, returncode, stdout="", stderr=""):
        from unittest import mock
        import types

        proc = types.SimpleNamespace(returncode=returncode, stdout=stdout, stderr=stderr)
        with mock.patch("roles_gate.subprocess.run", return_value=proc):
            return run_gate("two-bot", "staging", STAGING_URL, STAGING_HOST, STAGING_DB, "")

    def test_clean_exit_requires_verified_success_marker(self):
        self.assertEqual(self.invoke(0, "DATABASE ROLES VERIFIED\n"), [])
        for output in (
            "",
            "NOT DATABASE ROLES VERIFIED\n",
            "drift: missing grant X\n",
            "DATABASE ROLES VERIFIED\ndrift: missing grant X\n",
        ):
            with self.subTest(output=output), self.assertRaises(Refused):
                self.invoke(0, output)

    def test_exit_one_returns_reported_drift_findings(self):
        self.assertEqual(
            self.invoke(1, "", "drift: missing grant SELECT on public.t\n"),
            ["missing grant SELECT on public.t"],
        )

    def test_exit_one_without_drift_refuses(self):
        with self.assertRaises(Refused):
            self.invoke(1, "", "could not connect")

    def test_other_exit_refuses_without_child_stderr(self):
        with self.assertRaises(Refused) as raised:
            self.invoke(2, "", "postgresql://user:S3cr3tPW@host/db")
        self.assertEqual(str(raised.exception), "roles verify refused the gate (exit 2)")

    def test_timeout_refuses(self):
        import subprocess
        from unittest import mock

        with mock.patch("roles_gate.subprocess.run", side_effect=subprocess.TimeoutExpired("two-bot", 600)):
            with self.assertRaises(Refused):
                run_gate("two-bot", "staging", STAGING_URL, STAGING_HOST, STAGING_DB, "")


class MainTests(unittest.TestCase):
    def run_main(self, findings, target="staging", **overrides):
        with tempfile.TemporaryDirectory() as tmp:
            manifest = os.path.join(tmp, "manifest.json")
            code = main(
                gate_args(target, **overrides) + ["--manifest-out", manifest],
                env={},
                run=lambda *args: findings,
            )
            with open(manifest, encoding="utf-8") as handle:
                return code, json.load(handle)

    def test_empty_findings_pass_with_clean_manifest(self):
        code, manifest = self.run_main([])
        self.assertEqual(code, 0)
        self.assertEqual(manifest["findings"], [])
        self.assertEqual(manifest["target"], "staging")

    def test_nonempty_findings_fail_with_drift_manifest(self):
        code, manifest = self.run_main(["missing grant SELECT on public.t to two_bot_runtime"])
        self.assertEqual(code, 1)
        self.assertEqual(len(manifest["findings"]), 1)

    def test_malformed_findings_refuse(self):
        with self.assertRaises(Refused):
            self.run_main([12])

    def test_waived_run_records_the_reason(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest = os.path.join(tmp, "manifest.json")
            code = main(
                gate_args("production", mode="rollback") + ["--manifest-out", manifest],
                env={},
                run=lambda *args: self.fail("waived runs never verify"),
            )
            self.assertEqual(code, 0)
            with open(manifest, encoding="utf-8") as handle:
                body = json.load(handle)
            self.assertIn("waived", body)
            self.assertEqual(body["mode"], "rollback")


if __name__ == "__main__":
    unittest.main()
