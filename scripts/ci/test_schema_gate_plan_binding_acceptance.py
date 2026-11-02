"""Protected offline acceptance: a clean schema verdict must attest this plan.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/ci \
    -p 'test_schema_gate_plan_binding_acceptance.py' -v

Only the plan subprocess is doubled; the real CLI entry point, argument/env
parsing, binding verification, JSON decoding, grading and artifact I/O execute.
No network, database, Actions dispatch, apply, credentials or production access.
The complete synthetic report follows staging_migrate's existing v1 fields.

Consumer contract: the last nonempty stdout line is a JSON object with verdict
(clean/drift/refused/error, plus waived for existing production exemptions), a
stable nonempty code token, and nonempty next_action. Code spelling is left to
the Builder; tests require stable classifications independent of hostile data.
These files are a RED handoff, not a standalone merge candidate. Existing
minimal-manifest success fixtures must be upgraded by the Builder, not treated
as a reason to retain acceptance of unbound reports.
"""

import contextlib
import copy
import io
import json
import os
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile
import types
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
SHA = "0123456789abcdef0123456789abcdef01234567"
OTHER_SHA = "abcdef0123456789abcdef0123456789abcdef01"
CANARY = "REJECTED_VALUE_MUST_NOT_ESCAPE"
STAGING_HOST = "ep-staging-fixture.us-east-2.aws.neon.tech"
PRODUCTION_HOST = "two-production-fixture.example.com"
PS_HOST = "myapp.pg.psdb.cloud"
PS_BRANCH = "cnlmx96ec5kw"


def report(target, host=None, database="two_bot", branch=""):
    """Hand-authored producer-shaped read-only report, not a gate-derived oracle."""
    if host is None:
        host = STAGING_HOST if target == "staging" else PRODUCTION_HOST
    return {
        "runner_version": 1,
        "tool": {
            "name": "two-bot-cutover staging_migrate",
            "crate_version": "0.4.0",
            "sqlx": "0.9.0",
            "invocation": "staging-migrate --target staging|production --plan|--apply",
        },
        "mode": "plan",
        "migration_target": target,
        "source_sha": SHA,
        "target": {"host": host, "database": database, "branch_id": branch},
        "recovery_evidence_ref": "fixture-recovery",
        "acl_plan_ref": "fixture-acl-plan",
        "role": "two_bot_migrator_ro",
        "role_verified_connections": 1,
        "source_migrations": [],
        "ledger_before": [],
        "ledger_after": [],
        "pending_before": [],
        "expected_pending": [],
        "plan_manifest_sha256": "a" * 64,
        "source_store_migrations": [],
        "ledger_store_before": [],
        "ledger_store_after": [],
        "pending_store_before": [],
        "expected_store_pending": [],
        "store_plan_manifest_sha256": "b" * 64,
        "plan_run_id": "fixture-plan",
        "plan_provenance_verified": False,
        "applied_count": 0,
        "applied_store_count": 0,
        "audit": None,
    }


def environment(target, host=None, database="two_bot", branch="", padded=False):
    if host is None:
        host = STAGING_HOST if target == "staging" else PRODUCTION_HOST
    login = f"two-bot-migrator-ro.{branch}" if branch else "two_bot_migrator_ro"
    prefix = f"TWO_BOT_{target.upper()}_"
    return {
        prefix + "PLAN_DATABASE_URL": f"postgresql://{login}:fixture@{host}:5432/{database}",
        prefix + "HOST": f"  {host.upper()}  " if padded else host,
        prefix + "DATABASE": f"  {database}  " if padded else database,
        prefix + "BRANCH_ID": f"  {branch}  " if padded else branch,
    }


def invoke(target, payload=None, *, stdout=None, stderr="", returncode=0,
           fault=None, env=None, sha=SHA, extra=(), unwritable=False):
    """Execute the real __main__ boundary while replacing only external work."""
    calls = []
    output = io.StringIO()
    errors = io.StringIO()
    escaped = None
    exit_code = 0
    written = []
    scratch = os.environ.get("PAPERCLIP_SCRATCH_DIR") or os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
    with tempfile.TemporaryDirectory(dir=scratch) as directory:
        artifact = Path(directory) / (CANARY + ".json" if unwritable else "manifest.json")
        if unwritable:
            # Real local I/O failure, without depending on uid or chmod behavior.
            artifact.mkdir()
        args = [
            str(ROOT / "scripts" / "ci" / f"{target}_schema_gate.py"),
            "--binary", "fixture-staging-migrate", "--source-sha", sha,
            "--manifest-out", str(artifact), *extra,
        ]
        if target == "production" and "--mode" not in extra:
            args.extend(["--mode", "deploy"])
        wire = stdout if stdout is not None else json.dumps(payload) + "\n"

        def transport(argv, **kwargs):
            calls.append((list(argv), kwargs))
            if fault is not None:
                raise fault
            return types.SimpleNamespace(returncode=returncode, stdout=wire, stderr=stderr)

        original_write = Path.write_text

        def observe_write(path, data, *args, **kwargs):
            count = original_write(path, data, *args, **kwargs)
            if path == artifact:
                written.append(data)
            return count

        with mock.patch.dict(os.environ, environment(target) if env is None else env, clear=True), \
                mock.patch.object(sys, "argv", args), \
                mock.patch.object(subprocess, "run", side_effect=transport), \
                mock.patch.object(Path, "write_text", observe_write), \
                contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
            try:
                runpy.run_path(args[0], run_name="__main__")
            except SystemExit as error:
                exit_code = error.code
            except Exception as error:
                escaped = error
        saved = artifact.read_text() if artifact.is_file() else None
    return types.SimpleNamespace(
        exit_code=exit_code, stdout=output.getvalue(), stderr=errors.getvalue(),
        escaped=escaped, saved=saved, written=written, calls=calls,
    )


class SchemaGatePlanBindingAcceptance(unittest.TestCase):
    TARGETS = ("staging", "production")

    def assert_failed_evidence_only(self, result):
        # The refusal may save nothing or an explicitly failed envelope. It must
        # not publish even a transient unlabelled, apparently clean plan.
        artifacts = list(result.written)
        if result.saved is not None:
            artifacts.append(result.saved)
        for text in artifacts:
            self.assertNotIn(CANARY, text)
            try:
                evidence = json.loads(text)
            except (ValueError, TypeError):
                self.fail("saved refusal artifact must be valid failed-evidence JSON")
            self.assertIsInstance(evidence, dict)
            label = evidence.get("verdict", evidence.get("status"))
            self.assertIn(label, ("failed", "refused", "error"),
                          "invalid report was written as apparently successful evidence")

    def assert_no_disclosure(self, result):
        text = result.stdout + result.stderr + (result.saved or "")
        if result.escaped is not None:
            text += str(result.escaped)
        self.assertNotIn(CANARY, text)
        self.assertNotIn("Traceback (most recent call last)", text)

    def machine_result(self, result, verdict):
        self.assertIsNone(result.escaped, "CLI must classify failure, not raise an exception")
        lines = result.stdout.strip().splitlines()
        self.assertTrue(lines, "missing terminal machine-readable result on stdout")
        try:
            terminal = json.loads(lines[-1])
        except ValueError:
            self.fail("last stdout line must be the terminal JSON result")
        self.assertIsInstance(terminal, dict)
        self.assertEqual(terminal.get("verdict"), verdict)
        self.assertIsInstance(terminal.get("code"), str)
        self.assertRegex(terminal["code"], r"^[a-z][a-z0-9_]*$")
        self.assertIsInstance(terminal.get("next_action"), str)
        self.assertTrue(terminal["next_action"].strip())
        self.assert_no_disclosure(result)
        return terminal

    def assert_refused(self, result):
        self.assert_no_disclosure(result)
        self.assertIsNone(result.escaped, "CLI must return a bounded refusal")
        self.assertIsInstance(result.exit_code, int)
        self.assertNotEqual(result.exit_code, 0, "untrusted evidence was graded clean")
        self.assert_failed_evidence_only(result)

    def test_complete_bound_read_only_plan_is_clean(self):
        """Reject an implementation that refuses every report to pass negative cases."""
        for target in self.TARGETS:
            with self.subTest(target=target):
                result = invoke(target, report(target))
                self.assertIsNone(result.escaped)
                self.assertEqual(result.exit_code, 0)
                self.assertEqual(json.loads(result.saved)["source_sha"], SHA)
                self.assertEqual(len(result.calls), 1)
                argv = result.calls[0][0]
                self.assertIn("--plan", argv)
                self.assertNotIn("--apply", argv)
                self.assertEqual(argv[argv.index("--target") + 1], target)
                self.assertEqual(argv[argv.index("--source-sha") + 1], SHA)

    def test_minimal_pending_arrays_cannot_attest_a_clean_plan(self):
        for target in self.TARGETS:
            with self.subTest(target=target):
                self.assert_refused(invoke(target, {
                    "pending_before": [], "pending_store_before": [],
                }))

    def test_required_attestation_fields_cannot_be_missing(self):
        fields = ("runner_version", "mode", "migration_target", "source_sha",
                  "target", "role", "applied_count", "applied_store_count")
        for target in self.TARGETS:
            for field in fields:
                with self.subTest(target=target, missing=field):
                    payload = report(target)
                    del payload[field]
                    self.assert_refused(invoke(target, payload))

    def test_contradictory_or_mistyped_attestation_refuses(self):
        # bool is an int subclass in Python but not a runner version/apply count.
        variants = (
            ("runner_version", 2), ("runner_version", True), ("runner_version", "1"),
            ("runner_version", 1.0), ("runner_version", None),
            ("mode", "apply"), ("mode", None),
            ("source_sha", OTHER_SHA), ("source_sha", SHA.upper()),
            ("source_sha", CANARY), ("source_sha", None),
            ("role", "two_bot_migrator"), ("role", None),
            ("applied_count", 1), ("applied_store_count", 1),
            ("applied_count", -1), ("applied_store_count", -1),
            ("applied_count", False), ("applied_store_count", False),
            ("applied_count", "0"), ("applied_store_count", 0.0),
        )
        for target in self.TARGETS:
            for field, value in variants + (("migration_target", "production" if target == "staging" else "staging"),):
                with self.subTest(target=target, field=field, kind=type(value).__name__):
                    payload = report(target)
                    payload[field] = value
                    self.assert_refused(invoke(target, payload))

    def test_report_target_is_bound_to_independent_pins(self):
        for target in self.TARGETS:
            for field, value in (("host", "other.example.com"), ("database", "other_db"),
                                 ("branch_id", "otherbranch1"), ("host", CANARY)):
                with self.subTest(target=target, field=field, value_kind=type(value).__name__):
                    payload = report(target)
                    payload["target"][field] = value
                    self.assert_refused(invoke(target, payload))

    def test_target_fields_and_object_shape_are_required(self):
        for target in self.TARGETS:
            for value in (None, [], "target"):
                with self.subTest(target=target, target_shape=type(value).__name__):
                    payload = report(target)
                    payload["target"] = value
                    self.assert_refused(invoke(target, payload))
            for field in ("host", "database", "branch_id"):
                with self.subTest(target=target, missing=field):
                    payload = report(target)
                    del payload["target"][field]
                    self.assert_refused(invoke(target, payload))

    def test_normalized_verified_pins_not_raw_environment_are_the_oracle(self):
        for target in self.TARGETS:
            with self.subTest(target=target):
                result = invoke(target, report(target), env=environment(target, padded=True))
                self.assertIsNone(result.escaped)
                self.assertEqual(result.exit_code, 0)
                self.assertEqual(json.loads(result.saved)["target"], report(target)["target"])

    def test_planetscale_branch_pin_is_verified_and_compared(self):
        for target in self.TARGETS:
            pins = environment(target, PS_HOST, "postgres", PS_BRANCH, padded=True)
            payload = report(target, PS_HOST, "postgres", PS_BRANCH)
            with self.subTest(target=target, branch="matching"):
                result = invoke(target, payload, env=pins)
                self.assertIsNone(result.escaped)
                self.assertEqual(result.exit_code, 0)
                self.assertEqual(json.loads(result.saved)["target"]["branch_id"], PS_BRANCH)
            for branch in ("", "otherbranch1", None):
                with self.subTest(target=target, branch_kind=type(branch).__name__):
                    wrong = copy.deepcopy(payload)
                    wrong["target"]["branch_id"] = branch
                    self.assert_refused(invoke(target, wrong, env=pins))

    def test_report_cannot_supply_a_replacement_for_bad_environment_binding(self):
        for target in self.TARGETS:
            with self.subTest(target=target):
                pins = environment(target)
                pins[f"TWO_BOT_{target.upper()}_HOST"] = "other.example.com"
                result = invoke(target, report(target), env=pins)
                self.assert_refused(result)
                self.assertEqual(result.calls, [], "binding must refuse before plan execution")

    def test_noncanonical_sha_input_refuses_before_subprocess_or_artifact(self):
        for target in self.TARGETS:
            for sha in ("z" * 40, SHA.upper(), " " + SHA, SHA + " ", "abc", CANARY):
                with self.subTest(target=target, sha_kind="noncanonical"):
                    result = invoke(target, report(target), sha=sha)
                    self.assert_refused(result)
                    self.assertEqual(result.calls, [], "invalid SHA reached plan execution")

    def test_both_pending_arrays_require_strict_positive_increasing_integers(self):
        for target in self.TARGETS:
            for field in ("pending_before", "pending_store_before"):
                for value in (None, {}, "", True, [True], ["400"], [400.0],
                              [0], [-1], [401, 400], [400, 400]):
                    with self.subTest(target=target, field=field, kind=repr(value)):
                        payload = report(target)
                        payload[field] = value
                        self.assert_refused(invoke(target, payload))
                with self.subTest(target=target, field=field, missing=True):
                    payload = report(target)
                    del payload[field]
                    self.assert_refused(invoke(target, payload))

    def test_invalid_wire_reports_never_publish_successful_evidence(self):
        for target in self.TARGETS:
            for wire in ("", "not-json-" + CANARY, "null", "[]", "true", "42", "\"text\""):
                with self.subTest(target=target, shape=wire[:8]):
                    self.assert_refused(invoke(target, stdout=wire))

    def test_valid_drift_in_either_or_both_chains_is_not_clean(self):
        for target in self.TARGETS:
            for cutover, store in (([424, 425], []), ([], [400, 401]), ([424], [400])):
                with self.subTest(target=target, cutover=cutover, store=store):
                    payload = report(target)
                    payload["pending_before"] = cutover
                    payload["pending_store_before"] = store
                    result = invoke(target, payload)
                    self.assertIsNone(result.escaped)
                    self.assertEqual(result.exit_code, 1)
                    saved = json.loads(result.saved)
                    self.assertEqual(saved["pending_before"], cutover)
                    self.assertEqual(saved["pending_store_before"], store)
                    self.assertIn(f"{target}-migrate", result.stdout + result.stderr)

    def test_connection_warning_does_not_waive_or_fail_a_valid_clean_plan(self):
        for target in self.TARGETS:
            with self.subTest(target=target):
                payload = report(target)
                payload["audit"] = {"connection_usage": {"used": 25, "max_connections": 25}}
                result = invoke(target, payload)
                self.assertIsNone(result.escaped)
                self.assertEqual(result.exit_code, 0)
                self.assertIn("25/25", result.stdout + result.stderr)

    def test_terminal_clean_and_drift_results_have_stable_codes_and_actions(self):
        for target in self.TARGETS:
            with self.subTest(target=target):
                clean = self.machine_result(invoke(target, report(target)), "clean")
                payload = report(target)
                payload["pending_store_before"] = [400]
                drift = self.machine_result(invoke(target, payload), "drift")
                self.assertNotEqual(clean["code"], drift["code"])
                again = self.machine_result(invoke(target, payload), "drift")
                self.assertEqual(drift["code"], again["code"])
                self.assertEqual(drift["next_action"], again["next_action"])

    def test_terminal_manifest_refusal_is_machine_readable(self):
        for target in self.TARGETS:
            with self.subTest(target=target):
                payload = report(target)
                payload["mode"] = "apply"
                result = invoke(target, payload)
                self.assert_refused(result)
                self.machine_result(result, "refused")

    def test_nonzero_subprocess_does_not_echo_its_last_stderr_line(self):
        for target in self.TARGETS:
            with self.subTest(target=target):
                result = invoke(target, report(target), returncode=2,
                                stderr="driver diagnostic\n" + CANARY)
                self.assert_refused(result)

    def test_execution_io_decode_and_subprocess_failures_have_stable_classifications(self):
        for target in self.TARGETS:
            codes = []
            cases = (
                {"returncode": 2, "stderr": CANARY},
                {"fault": OSError(CANARY)},
                {"stdout": "{" + CANARY},
                {"unwritable": True},
            )
            for index, kwargs in enumerate(cases):
                with self.subTest(target=target, failure=index):
                    first = invoke(target, report(target), **kwargs)
                    self.assert_refused(first)
                    terminal = self.machine_result(first, "refused" if index == 2 else "error")
                    second = self.machine_result(invoke(target, report(target), **kwargs),
                                                 "refused" if index == 2 else "error")
                    self.assertEqual(terminal["code"], second["code"])
                    self.assertEqual(terminal["next_action"], second["next_action"])
                    codes.append(terminal["code"])
            if len(codes) == len(cases):
                self.assertEqual(len(set(codes)), len(cases),
                                 "subprocess, execution, decode and output-write failures need distinct codes")

    def test_production_existing_recovery_waivers_remain_explicit_and_read_free(self):
        for args, mode in ((("--mode", "rollback"), "rollback"),
                           (("--mode", "deploy", "--allow-unstaged", "true"), "deploy")):
            with self.subTest(mode=mode):
                result = invoke("production", report("production"), extra=args,
                                fault=AssertionError("waived flow attempted a plan"))
                self.assertIsNone(result.escaped)
                self.assertEqual(result.exit_code, 0)
                self.assertEqual(result.calls, [])
                saved = json.loads(result.saved)
                self.assertTrue(saved.get("waived"))
                self.assertEqual(saved["mode"], mode)


if __name__ == "__main__":
    unittest.main()
