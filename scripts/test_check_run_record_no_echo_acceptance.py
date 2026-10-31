#!/usr/bin/env python3
"""Acceptance tests: run-record refusal diagnostics omit rejected values.

The recognized-secret scan can only stop values that match its finite marker
list. An unrecognized credential-shaped value (a database URL, a webhook URL,
any arbitrary string) must still never be copied into an enum, pattern, or
date-time refusal diagnostic. These tests therefore build synthetic sentinels
that provably match no entry in ``SECRET_MARKERS`` and require that:

* the record is still refused (validation is not loosened),
* at least one refusal names the field and the failure class, and
* no refusal, and no CLI stdout/stderr, contains the rejected scalar or any
  of its identifying fragments.

Passing controls pin the pre-existing protections (valid real/mock records,
mock refusal, recognized-secret short-circuit, duplicate keys, nesting
budget) so the fix cannot trade one protection for another.

Sentinels are assembled from fragments at runtime: no credential-shaped
literal is stored in this public repository, and failing assertions use fixed
messages so a red run never echoes a sentinel into test output.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_check_run_record_no_echo_acceptance.py' -v
"""

import copy
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import check_run_record as checker

ROOT = Path(__file__).resolve().parents[1]
SCHEMA_PATH = ROOT / "docs" / "staging-e2e-run-record.schema.json"
SCHEMA = json.loads(SCHEMA_PATH.read_text(encoding="utf-8"))
MOCK = json.loads(
    (ROOT / "scripts" / "fixtures" / "staging_e2e_run_record_mock.json").read_text(
        encoding="utf-8"
    )
)

# One unrecognized core per sentinel family, so a leak names its family from
# the fixed assertion label alone (never from printed test output).
DB_CORE = "DbUrl" + "NeverEcho" + "7Q"
HOOK_CORE = "Hook" + "NeverEcho" + "8R"
ARB_CORE = "ArbString" + "NeverEcho" + "9S"

DB_URL = (
    "postgres" + "://" + "fixture_user" + ":" + DB_CORE
    + "@" + "db.invalid:5432" + "/fixture_db"
)
WEBHOOK = "https" + "://" + "hooks.invalid" + "/services/" + HOOK_CORE
ARBITRARY = "arbitrary-fixture-" + ARB_CORE + "-tail"

SENTINELS = (DB_URL, WEBHOOK, ARBITRARY)
CORES = (DB_CORE, HOOK_CORE, ARB_CORE)


def fresh_record(**overrides):
    record = copy.deepcopy(MOCK)
    record.update(overrides)
    return record


class NoEchoAcceptanceTests(unittest.TestCase):
    # --- helpers ---------------------------------------------------------

    def assert_clean(self, fragments, texts, label):
        for fragment in fragments:
            for text in texts:
                # Fixed message only: printing the fragment or the text here
                # would itself leak the sentinel into the test log.
                self.assertFalse(
                    fragment in text, f"rejected value echoed in case {label}"
                )

    def assert_refusal_at(self, errors, path, class_words, label):
        self.assertTrue(errors, f"record must still be refused in case {label}")
        at_path = [e for e in errors if e.startswith(path + ":")]
        self.assertTrue(at_path, f"no refusal names the field in case {label}")
        joined = " ".join(at_path).lower()
        self.assertTrue(
            any(word in joined for word in class_words),
            f"no refusal names the failure class in case {label}",
        )

    def cli(self, record):
        with tempfile.TemporaryDirectory(
            dir=os.getenv("PAPERCLIP_RUN_SCRATCH_DIR")
        ) as tmp:
            path = Path(tmp) / "record.json"
            path.write_text(json.dumps(record), encoding="utf-8")
            out, err = io.StringIO(), io.StringIO()
            with redirect_stdout(out), redirect_stderr(err):
                code = checker.main(["--record", str(path), "--allow-mock"])
            return code, out.getvalue(), err.getvalue()

    def check_validate_case(self, record, path, class_words, fragments, label):
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            errors = checker.validate(record, SCHEMA, allow_mock=True)
        self.assert_refusal_at(errors, path, class_words, label)
        self.assert_clean(fragments, ["\n".join(errors), out.getvalue(), err.getvalue()], label)

    # --- precondition ----------------------------------------------------

    def test_acceptance_sentinels_match_no_recognized_secret_marker(self):
        # If a sentinel matched a marker, the early public-safety return
        # would make every case below pass vacuously without ever reaching
        # the enum/pattern/date-time diagnostics under test.
        for label, sentinel in zip(("db-url", "webhook", "arbitrary"), SENTINELS):
            for marker in checker.SECRET_MARKERS:
                self.assertIsNone(
                    marker.search(sentinel),
                    f"sentinel {label} must stay unrecognized",
                )

    # --- pattern refusals -------------------------------------------------

    def test_acceptance_pattern_refusals_omit_rejected_values(self):
        cases = []
        record = fresh_record()
        record["deployment"]["revision"] = DB_URL
        cases.append((
            "pattern-revision-db-url", record, "$.deployment.revision",
            [DB_URL, DB_CORE, "fixture_user", "db.invalid"],
        ))
        record = fresh_record()
        record["environment"]["guild_id"] = WEBHOOK
        cases.append((
            "pattern-guild-webhook", record, "$.environment.guild_id",
            [WEBHOOK, HOOK_CORE, "hooks.invalid"],
        ))
        record = fresh_record()
        record["deployment"]["deploy_staging_run_id"] = ARBITRARY
        cases.append((
            "pattern-run-id-arbitrary", record,
            "$.deployment.deploy_staging_run_id", [ARBITRARY, ARB_CORE],
        ))
        for label, record, path, fragments in cases:
            with self.subTest(case=label):
                self.check_validate_case(
                    record, path,
                    ("pattern", "does not match", "must match", "invalid"),
                    fragments, label,
                )

    # --- enum refusals ------------------------------------------------------

    def test_acceptance_enum_refusals_omit_rejected_values(self):
        cases = []
        record = fresh_record()
        record["commands"][0]["result"] = DB_URL
        cases.append((
            "enum-command-result-db-url", record, "$.commands[0].result",
            [DB_URL, DB_CORE, "fixture_user", "db.invalid"],
        ))
        record = fresh_record()
        record["verdict"]["disposition"] = WEBHOOK
        cases.append((
            "enum-disposition-webhook", record, "$.verdict.disposition",
            [WEBHOOK, HOOK_CORE, "hooks.invalid"],
        ))
        record = fresh_record()
        record["cleanup"]["result"] = ARBITRARY
        cases.append((
            "enum-cleanup-arbitrary", record, "$.cleanup.result",
            [ARBITRARY, ARB_CORE],
        ))
        for label, record, path, fragments in cases:
            with self.subTest(case=label):
                self.check_validate_case(
                    record, path,
                    ("enum", "not one of", "must be one of", "allowed", "invalid"),
                    fragments, label,
                )

    # --- date-time refusals -------------------------------------------------

    def test_acceptance_malformed_datetime_refusals_omit_rejected_values(self):
        cases = []
        record = fresh_record()
        record["window"]["start_utc"] = DB_URL
        cases.append((
            "datetime-window-db-url", record, "$.window.start_utc",
            [DB_URL, DB_CORE, "fixture_user", "db.invalid"],
        ))
        record = fresh_record()
        record["commands"][0]["started_utc"] = WEBHOOK
        cases.append((
            "datetime-command-webhook", record, "$.commands[0].started_utc",
            [WEBHOOK, HOOK_CORE, "hooks.invalid"],
        ))
        record = fresh_record()
        record["deployment"]["first_readyz_utc"] = ARBITRARY
        cases.append((
            "datetime-readyz-arbitrary", record, "$.deployment.first_readyz_utc",
            [ARBITRARY, ARB_CORE],
        ))
        for label, record, path, fragments in cases:
            with self.subTest(case=label):
                self.check_validate_case(
                    record, path,
                    ("date-time", "datetime", "iso-8601", "timestamp", "invalid"),
                    fragments, label,
                )

    def test_acceptance_impossible_datetime_refusal_omits_rejected_value(self):
        # Regex-shaped but calendrically impossible: the whole rejected
        # scalar is the timestamp text itself, and it must not be echoed.
        for label, stamp in (
            ("impossible-feb-30", "2026-02-30T00:00:00Z"),
            ("impossible-month-13", "2026-13-01T00:00:00Z"),
        ):
            with self.subTest(case=label):
                record = fresh_record()
                record["deployment"]["first_readyz_utc"] = stamp
                self.check_validate_case(
                    record, "$.deployment.first_readyz_utc",
                    ("date", "invalid", "calendar"), [stamp], label,
                )

    def test_acceptance_utc_suffix_refusals_omit_rejected_value(self):
        # The naive / non-UTC-offset refusals are date-time diagnostics
        # under the module's no-value-echo contract as well.
        for label, stamp, words in (
            ("naive-no-timezone", "2026-10-04T00:00:41",
             ("naive", "timezone", "utc", "invalid")),
            ("non-utc-offset", "2026-10-04T00:00:41+03:00",
             ("utc", "offset", "invalid")),
        ):
            with self.subTest(case=label):
                record = fresh_record()
                record["deployment"]["first_readyz_utc"] = stamp
                self.check_validate_case(
                    record, "$.deployment.first_readyz_utc", words, [stamp], label,
                )

    # --- CLI surface ---------------------------------------------------------

    def test_acceptance_cli_omits_rejected_values_and_keeps_contract(self):
        cases = []
        record = fresh_record()
        record["deployment"]["revision"] = DB_URL
        cases.append(("cli-pattern-db-url", record, "$.deployment.revision",
                      [DB_URL, DB_CORE, "fixture_user", "db.invalid"]))
        record = fresh_record()
        record["verdict"]["disposition"] = WEBHOOK
        cases.append(("cli-enum-webhook", record, "$.verdict.disposition",
                      [WEBHOOK, HOOK_CORE, "hooks.invalid"]))
        record = fresh_record()
        record["window"]["start_utc"] = ARBITRARY
        cases.append(("cli-datetime-arbitrary", record, "$.window.start_utc",
                      [ARBITRARY, ARB_CORE]))
        for label, record, path, fragments in cases:
            with self.subTest(case=label):
                code, out, err = self.cli(record)
                # Existing CLI contract: exit 1, diagnostics on stdout,
                # nothing on stderr.
                self.assertEqual(code, 1, f"CLI must refuse in case {label}")
                self.assertEqual(err, "", f"CLI stderr must stay empty in case {label}")
                self.assertIn(path, out, f"CLI must name the field in case {label}")
                self.assert_clean(fragments, [out, err], label)

    def test_acceptance_cli_subprocess_omits_rejected_values(self):
        record = fresh_record()
        record["deployment"]["revision"] = DB_URL
        with tempfile.TemporaryDirectory(
            dir=os.getenv("PAPERCLIP_RUN_SCRATCH_DIR")
        ) as tmp:
            path = Path(tmp) / "record.json"
            path.write_text(json.dumps(record), encoding="utf-8")
            env = dict(os.environ, PYTHONDONTWRITEBYTECODE="1")
            result = subprocess.run(
                [
                    sys.executable,
                    str(ROOT / "scripts" / "check_run_record.py"),
                    "--record", str(path),
                    "--allow-mock",
                ],
                capture_output=True, text=True, env=env, timeout=60,
            )
        self.assertEqual(result.returncode, 1, "subprocess CLI must refuse")
        self.assertEqual(result.stderr, "", "subprocess stderr must stay empty")
        self.assertIn("$.deployment.revision", result.stdout)
        self.assert_clean(
            [DB_URL, DB_CORE, "fixture_user", "db.invalid"],
            [result.stdout, result.stderr], "cli-subprocess-db-url",
        )

    def test_acceptance_key_only_secret_is_refused_with_redacted_path(self):
        sentinel = "gh" + "p_" + "KeyOnlyNeverEcho42"
        record = {"outer": [{sentinel: "innocent"}]}
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            errors = checker.validate(record, {}, allow_mock=True)
        self.assert_clean(
            [sentinel, "KeyOnlyNeverEcho42"],
            ["\n".join(errors), out.getvalue(), err.getvalue()], "key-only",
        )
        self.assertEqual(len(errors), 1, "key alone must trigger public-safety refusal")
        self.assertTrue(errors[0].startswith(
            "$.outer[0].[redacted-key]: public-safety scan hit "))
        self.assertEqual(out.getvalue(), "")
        self.assertEqual(err.getvalue(), "")

    def test_acceptance_cli_subprocess_secret_key_never_echoes(self):
        sentinel = "xo" + "xb-" + "KeyOnlyNeverEcho42"
        with tempfile.TemporaryDirectory(
            dir=os.getenv("PAPERCLIP_RUN_SCRATCH_DIR")
        ) as tmp:
            record, schema = Path(tmp) / "record.json", Path(tmp) / "schema.json"
            record.write_text(json.dumps({"outer": [{sentinel: "innocent"}]}), encoding="utf-8")
            # Permissive schema prevents an unknown-field refusal from masking
            # the missing key scan; the CLI must fail for public safety alone.
            schema.write_text("{}", encoding="utf-8")
            result = subprocess.run(
                [sys.executable, str(ROOT / "scripts" / "check_run_record.py"),
                 "--record", str(record), "--schema", str(schema), "--allow-mock"],
                capture_output=True, text=True,
                env=dict(os.environ, PYTHONDONTWRITEBYTECODE="1"), timeout=60,
            )
        self.assert_clean([sentinel, "KeyOnlyNeverEcho42"],
                          [result.stdout, result.stderr], "key-only-subprocess")
        self.assertEqual(result.returncode, 1, "CLI must refuse a secret in a key alone")
        self.assertEqual(result.stderr, "")
        self.assertIn("$.outer[0].[redacted-key]: public-safety scan hit ", result.stdout)

    # --- passing controls: behavior and protections unchanged ---------------

    def test_control_valid_records_still_validate(self):
        self.assertEqual(checker.validate(MOCK, SCHEMA, allow_mock=True), [])
        self.assertEqual(checker.validate(fresh_record(mock=False), SCHEMA), [])
        code, out, err = self.cli(fresh_record(mock=False))
        self.assertEqual(code, 0)
        self.assertIn("valid staging E2E run record", out)
        self.assertEqual(err, "")

    def test_control_mock_record_still_refused_without_flag(self):
        errors = checker.validate(MOCK, SCHEMA)
        self.assertTrue(any(e.startswith("$.mock:") for e in errors))

    def test_control_recognized_secret_still_short_circuits_value_free(self):
        sentinel = "gh" + "p_" + "RecognizedPayloadNeverEcho42"
        record = fresh_record()
        record["commands"][0]["result"] = sentinel
        errors = checker.validate(record, SCHEMA, allow_mock=True)
        self.assertEqual(len(errors), 1)
        self.assertTrue(
            errors[0].startswith("$.commands[0].result: public-safety scan hit ")
        )
        self.assert_clean(
            [sentinel, "RecognizedPayloadNeverEcho42"], ["\n".join(errors)],
            "control-recognized-secret",
        )

    def test_control_duplicate_key_still_refused_value_free(self):
        text = '{"notes":' + json.dumps(DB_URL) + ',"notes":"ok"}'
        with self.assertRaises(checker.DuplicateKeyError) as caught:
            checker.load_json(text)
        self.assertEqual(str(caught.exception), "$.notes: duplicate object key")
        self.assert_clean(
            [DB_URL, DB_CORE], [str(caught.exception)], "control-duplicate-key"
        )

    def test_control_nesting_budget_still_refused_value_free(self):
        nested = []
        for _ in range(checker.MAX_NESTING_DEPTH):
            nested = [nested]
        self.assertEqual(
            checker.validate(nested, {}), [f"$: {checker.NESTING_MESSAGE}"]
        )


if __name__ == "__main__":
    unittest.main()
