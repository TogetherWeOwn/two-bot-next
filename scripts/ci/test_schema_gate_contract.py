"""Additional boundary and publication regressions for both read-only gates."""

import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

import schema_gate_common as common
import test_schema_gate_plan_binding_acceptance as fixtures


class SchemaGateContractTests(unittest.TestCase):
    def terminal(self, outcome, code):
        self.assertIsNone(outcome.escaped)
        expected_verdict, expected_action, expected_exit = common.RESULTS[code]
        self.assertEqual(outcome.exit_code, expected_exit)
        self.assertEqual(json.loads(outcome.stdout.strip().splitlines()[-1]), {
            "code": code, "verdict": expected_verdict, "next_action": expected_action,
        })
        self.assertNotIn(fixtures.CANARY, outcome.stdout + outcome.stderr + (outcome.saved or ""))
        self.assertNotIn("Traceback", outcome.stdout + outcome.stderr)

    def test_argument_errors_are_terminal_and_do_not_echo_values(self):
        for target in ("staging", "production"):
            extra = ("--unknown", fixtures.CANARY)
            with self.subTest(target=target):
                outcome = fixtures.invoke(target, fixtures.report(target), extra=extra)
                self.terminal(outcome, "arguments_invalid")
                self.assertEqual(outcome.calls, [])
                self.assertIsNone(outcome.saved)
        outcome = fixtures.invoke("production", extra=("--mode", fixtures.CANARY))
        self.terminal(outcome, "arguments_invalid")
        self.assertEqual(outcome.calls, [])

    def test_timeout_and_decoding_errors_never_disclose_child_diagnostics(self):
        faults = (
            (subprocess.TimeoutExpired(fixtures.CANARY, 600, output=fixtures.CANARY,
                                       stderr=fixtures.CANARY), "plan_timeout"),
            (UnicodeDecodeError("utf-8", b"\xff", 0, 1, fixtures.CANARY), "manifest_invalid"),
        )
        for target in ("staging", "production"):
            for fault, code in faults:
                with self.subTest(target=target, code=code):
                    outcome = fixtures.invoke(target, fault=fault)
                    self.terminal(outcome, code)
                    self.assertIsNone(outcome.saved)

    def test_nonfinite_json_is_refused_even_in_optional_fields(self):
        for target in ("staging", "production"):
            for value in (float("nan"), float("inf"), -float("inf")):
                with self.subTest(target=target, value=value):
                    payload = fixtures.report(target)
                    payload["audit"] = {"unused": value}
                    outcome = fixtures.invoke(target, payload)
                    self.terminal(outcome, "manifest_invalid")
                    self.assertIsNone(outcome.saved)

    def test_exponent_overflow_wire_float_is_refused_as_malformed_output(self):
        for target in ("staging", "production"):
            with self.subTest(target=target, field="extra_metric"):
                wire = json.dumps(fixtures.report(target))[:-1] + ', "extra_metric": 1e999}\n'
                outcome = fixtures.invoke(target, stdout=wire)
                self.terminal(outcome, "manifest_invalid")
                self.assertIsNone(outcome.saved)
            with self.subTest(target=target, field="audit.used"):
                payload = fixtures.report(target)
                payload["audit"] = {"connection_usage": {"used": 25, "max_connections": 25}}
                wire = json.dumps(payload).replace('"used": 25', '"used": 1e999', 1)
                self.assertIn("1e999", wire)
                outcome = fixtures.invoke(target, stdout=wire)
                self.terminal(outcome, "manifest_invalid")
                self.assertIsNone(outcome.saved)

    def test_invalid_connection_counts_cannot_break_a_valid_gate(self):
        for target in ("staging", "production"):
            for field in ("used", "max_connections", "effective_max_connections", "reserved_connections"):
                with self.subTest(target=target, field=field):
                    payload = fixtures.report(target)
                    usage = {"used": 25, "max_connections": 25}
                    usage[field] = 10**400
                    payload["audit"] = {"connection_usage": usage}
                    self.terminal(fixtures.invoke(target, payload), "schema_clean")

    def test_target_case_rules_match_binding_rules(self):
        for target in ("staging", "production"):
            pins = fixtures.environment(target, fixtures.PS_HOST, "postgres", fixtures.PS_BRANCH)
            payload = fixtures.report(target, fixtures.PS_HOST.upper(), "postgres", fixtures.PS_BRANCH.upper())
            with self.subTest(target=target, case="host and branch"):
                self.terminal(fixtures.invoke(target, payload, env=pins), "schema_clean")
            payload["target"]["database"] = "POSTGRES"
            with self.subTest(target=target, case="database"):
                outcome = fixtures.invoke(target, payload, env=pins)
                self.terminal(outcome, "manifest_invalid")
                self.assertIsNone(outcome.saved)

    def test_pending_i64_boundary_is_enforced_on_both_chains(self):
        for target in ("staging", "production"):
            for key in ("pending_before", "pending_store_before"):
                for value, code in ((2**63 - 1, "schema_drift"), (2**63, "manifest_invalid")):
                    with self.subTest(target=target, key=key, code=code):
                        payload = fixtures.report(target)
                        payload[key] = [value]
                        outcome = fixtures.invoke(target, payload)
                        self.terminal(outcome, code)
                        if code == "manifest_invalid":
                            self.assertIsNone(outcome.saved)

    def test_atomic_output_failure_keeps_prior_evidence_and_removes_owned_temporary(self):
        scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
        with tempfile.TemporaryDirectory(dir=scratch) as directory:
            path = Path(directory) / "manifest.json"
            path.write_text('{"status":"prior"}\n')
            output, errors = io.StringIO(), io.StringIO()
            original_replace = common.os.replace

            def fail_publish(source, destination):
                if Path(destination) == path:
                    raise OSError(fixtures.CANARY)
                return original_replace(source, destination)

            with mock.patch.object(common.os, "replace", side_effect=fail_publish), \
                    contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                code = common.cli(lambda: common.write_manifest(path, fixtures.report("staging")))
            self.assertEqual(code, 3)
            self.assertEqual(json.loads(output.getvalue())["code"], "manifest_write_failed")
            self.assertEqual(path.read_text(), '{"status":"prior"}\n')
            self.assertEqual(list(Path(directory).iterdir()), [path])
            self.assertNotIn(fixtures.CANARY, output.getvalue() + errors.getvalue())

    def test_waivers_remain_distinct_from_clean_and_require_canonical_sha(self):
        for extra in (("--mode", "rollback"), ("--allow-unstaged", "true")):
            with self.subTest(extra=extra):
                waived = fixtures.invoke("production", extra=extra, env={})
                self.terminal(waived, "recovery_waived")
                self.assertEqual(waived.calls, [])
                invalid = fixtures.invoke("production", extra=extra, env={}, sha="z" * 40)
                self.terminal(invalid, "source_sha_invalid")
                self.assertEqual(invalid.calls, [])
                self.assertIsNone(invalid.saved)

    def test_direct_boundary_type_errors_have_specific_safe_codes(self):
        for value in (None, True, 123, b"a" * 40):
            with self.subTest(kind=type(value).__name__):
                with self.assertRaises(common.Refused) as caught:
                    common.validate_source_sha(value)
                self.assertEqual(caught.exception.code, "source_sha_invalid")
        transport = mock.Mock(return_value=mock.Mock(returncode=0, stdout=None))
        with self.assertRaises(common.Refused) as caught:
            common.run_plan(["fixture-binary"], transport)
        self.assertEqual(caught.exception.code, "manifest_invalid")
        with tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as directory:
            path = Path(directory) / "manifest.json"
            with self.assertRaises(common.Refused) as caught:
                common.write_manifest(path, {"audit": {fixtures.CANARY}})
            self.assertEqual(caught.exception.code, "manifest_write_failed")
            self.assertNotIn(fixtures.CANARY, str(caught.exception))
            self.assertEqual(list(Path(directory).iterdir()), [])

    def test_unexpected_faults_have_a_bounded_terminal_error(self):
        output, errors = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
            code = common.cli(mock.Mock(side_effect=RuntimeError(fixtures.CANARY)))
        self.assertEqual(code, 3)
        self.assertEqual(json.loads(output.getvalue())["code"], "gate_internal_error")
        self.assertNotIn(fixtures.CANARY, output.getvalue() + errors.getvalue())
        self.assertNotIn("Traceback", output.getvalue() + errors.getvalue())

    def test_every_terminal_code_and_action_is_documented(self):
        contract = (fixtures.ROOT / "docs" / "runbook.md").read_text()
        for code, (_, action, _) in common.RESULTS.items():
            with self.subTest(code=code):
                self.assertIn(f"`{code}`", contract)
                self.assertIn(f"`{action}`", contract)


if __name__ == "__main__":
    unittest.main()
