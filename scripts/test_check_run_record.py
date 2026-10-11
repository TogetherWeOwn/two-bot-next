#!/usr/bin/env python3
"""Offline tests for the staging E2E run-record validator (stdlib only)."""

import copy
import io
import json
from contextlib import redirect_stderr, redirect_stdout
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import check_run_record as checker  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]
SCHEMA = json.loads(
    (ROOT / "docs" / "staging-e2e-run-record.schema.json").read_text(
        encoding="utf-8"
    )
)
MOCK = json.loads(
    (
        ROOT / "scripts" / "fixtures" / "staging_e2e_run_record_mock.json"
    ).read_text(encoding="utf-8")
)


def fresh_record(**overrides):
    record = copy.deepcopy(MOCK)
    record.update(overrides)
    return record


class RunRecordValidatorTests(unittest.TestCase):
    def test_mock_fixture_valid_with_allow_mock(self):
        self.assertEqual(checker.validate(MOCK, SCHEMA, allow_mock=True), [])

    def test_mock_flag_rejected_as_evidence_without_allow_mock(self):
        errors = checker.validate(MOCK, SCHEMA)
        self.assertTrue(
            any("$.mock" in error for error in errors),
            f"mock record must fail closed without --allow-mock: {errors}",
        )

    def test_real_shaped_record_valid(self):
        record = fresh_record(mock=False)
        self.assertEqual(checker.validate(record, SCHEMA), [])

    def test_missing_revision_fails(self):
        record = fresh_record()
        del record["deployment"]["revision"]
        errors = checker.validate(record, SCHEMA)
        self.assertTrue(
            any("revision" in error for error in errors),
            f"missing revision must fail: {errors}",
        )

    def test_short_revision_fails(self):
        record = fresh_record()
        record["deployment"]["revision"] = "c3efe26b"
        errors = checker.validate(record, SCHEMA)
        self.assertTrue(
            any("revision" in error for error in errors),
            f"short revision must fail: {errors}",
        )

    def test_fail_without_failure_signature_fails(self):
        record = fresh_record()
        for command in record["commands"]:
            if command["result"] == "fail":
                del command["failure_signature"]
        errors = checker.validate(record, SCHEMA)
        self.assertTrue(
            any("failure_signature" in error for error in errors),
            f"fail without signature must fail: {errors}",
        )

    def test_negative_duration_fails(self):
        record = fresh_record()
        record["commands"][0]["duration_ms"] = -5
        errors = checker.validate(record, SCHEMA)
        self.assertTrue(
            any("duration_ms" in error for error in errors),
            f"negative duration must fail: {errors}",
        )

    def test_internal_tracker_id_rejected(self):
        record = fresh_record()
        record["verdict"]["summary"] = "See TOG-12345 for the follow-up."
        errors = checker.validate(record, SCHEMA)
        self.assertTrue(
            any("public-safety" in error for error in errors),
            f"internal tracker ID must fail: {errors}",
        )

    def test_secret_marker_rejected_without_echo(self):
        # Construct synthetic markers at runtime; no token-shaped literals.
        for prefix in ("gh" + "p_", "xox" + "b-", "BEGIN " + "PRIVATE KEY "):
            with self.subTest(kind=prefix.split()[0]):
                sentinel = prefix + "SyntheticPayloadNeverEcho42"
                record = fresh_record()
                record["tester"]["identity"] = "tester with " + sentinel
                marker = next(m for m in checker.SECRET_MARKERS if m.search(sentinel))
                out, err = io.StringIO(), io.StringIO()
                with redirect_stdout(out), redirect_stderr(err):
                    errors = checker.validate(record, SCHEMA, allow_mock=True)
                self.assert_no_sentinel(sentinel, "\n".join(errors), out.getvalue(), err.getvalue())
                self.assertEqual(errors, [
                    f"$.tester.identity: public-safety scan hit {marker.pattern!r}",
                ])

    def assert_no_sentinel(self, sentinel, *texts):
        for text in texts:
            # Do not echo even a synthetic credential in a failing assertion.
            self.assertFalse(sentinel in text, "secret-shaped sentinel leaked")
            self.assertFalse("SyntheticPayloadNeverEcho42" in text, "secret payload leaked")

    def cli(self, record_text, schema_text=None, allow_mock=True):
        with tempfile.TemporaryDirectory(dir=os.getenv("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            record = Path(tmp) / "record.json"
            record.write_text(record_text, encoding="utf-8")
            argv = ["--record", str(record)]
            if allow_mock:
                argv.append("--allow-mock")
            if schema_text is not None:
                schema = Path(tmp) / "schema.json"
                schema.write_text(schema_text, encoding="utf-8")
                argv.extend(["--schema", str(schema)])
            out, err = io.StringIO(), io.StringIO()
            with redirect_stdout(out), redirect_stderr(err):
                code = checker.main(argv)
            return code, out.getvalue(), err.getvalue()

    def test_cli_and_schema_failures_never_echo_secret_values(self):
        sentinel = "gh" + "p_" + "SyntheticPayloadNeverEcho42"
        # These fields also fail pattern, enum and date-time validation.
        for section, key in (("deployment", "revision"), ("commands", "result"),
                             ("window", "start_utc"), ("tester", "identity")):
            with self.subTest(section=section, key=key):
                record = fresh_record()
                target = record[section][0] if section == "commands" else record[section]
                target[key] = sentinel
                errors = checker.validate(record, SCHEMA, allow_mock=True)
                code, out, err = self.cli(json.dumps(record))
                self.assert_no_sentinel(sentinel, "\n".join(errors), out, err)
                self.assertEqual(code, 1)
                path = f"$.{section}{'[0]' if section == 'commands' else ''}.{key}"
                self.assertTrue(any(e.startswith(path + ": public-safety scan hit ") for e in errors))

    def test_scan_handles_root_strings_and_nested_arrays(self):
        sentinel = "xox" + "b-" + "SyntheticPayloadNeverEcho42"
        for record, path in ((sentinel, "$"), ({"extra": [sentinel]}, "$.extra[0]")):
            with self.subTest(path=path):
                errors = checker.validate(record, SCHEMA)
                self.assert_no_sentinel(sentinel, "\n".join(errors))
                self.assertEqual(len(errors), 1)
                self.assertTrue(errors[0].startswith(path + ": public-safety scan hit "))

    def test_duplicate_keys_fail_before_values_can_be_hidden(self):
        sentinel = "gh" + "p_" + "SyntheticPayloadNeverEcho42"
        cases = (
            ('{"notes":' + json.dumps(sentinel) + ',"notes":"ok"}', "$.notes"),
            ('{"mock":true,"mock":false}', "$.mock"),
            ('{"cleanup":{"notes":' + json.dumps(sentinel) + ',"notes":"ok"}}',
             "$.cleanup.notes"),
            ('{"commands":[{"notes":' + json.dumps(sentinel) + ',"notes":"ok"}]}',
             "$.commands[0].notes"),
            ('{"notes":"ok","no\\u0074es":"also ok"}', "$.notes"),
        )
        for text, path in cases:
            for allow_mock in (False, True):
                with self.subTest(path=path, allow_mock=allow_mock):
                    with self.assertRaises(checker.DuplicateKeyError) as caught:
                        checker.load_json(text)
                    self.assertEqual(str(caught.exception), f"{path}: duplicate object key")
                    code, out, err = self.cli(text, allow_mock=allow_mock)
                    self.assert_no_sentinel(sentinel, str(caught.exception), out, err)
                    self.assertEqual(code, 1)
                    self.assertTrue(out.rstrip().endswith(
                        f"cannot load record: {path}: duplicate object key"))
                    self.assertEqual(err, "")

    def test_duplicate_schema_keys_fail_at_any_depth(self):
        sentinel = "xox" + "b-" + "SyntheticPayloadNeverEcho42"
        for schema, path in (
            ('{"type":' + json.dumps(sentinel) + ',"type":"object"}', "$.type"),
            ('{"properties":{"notes":{"type":"string","type":' + json.dumps(sentinel)
             + '}}}', "$.properties.notes.type"),
            ('{"allOf":[{"type":"object","type":"string"}]}', "$.allOf[0].type"),
        ):
            with self.subTest(path=path):
                code, out, err = self.cli(json.dumps(MOCK), schema)
                self.assert_no_sentinel(sentinel, out, err)
                self.assertEqual(code, 1)
                self.assertTrue(out.rstrip().endswith(
                    f"cannot load schema: {path}: duplicate object key"))
                self.assertEqual(err, "")

    def test_unique_json_retains_objects_arrays_and_values(self):
        document = {"empty": {}, "items": [[], {"value": None}, True, 7, "ok"]}
        self.assertEqual(checker.load_json(json.dumps(document)), document)
        self.assertEqual(checker.load_json(json.dumps(MOCK)), MOCK)

    def test_duplicate_path_and_scan_path_do_not_echo_secret_shaped_keys(self):
        sentinel = "gh" + "p_" + "SyntheticPayloadNeverEcho42"
        text = '{' + json.dumps(sentinel) + ':{"notes":0,"notes":1}}'
        with self.assertRaises(checker.DuplicateKeyError) as caught:
            checker.load_json(text)
        self.assert_no_sentinel(sentinel, str(caught.exception))
        self.assertEqual(str(caught.exception), "$.[redacted-key].notes: duplicate object key")
        errors = checker.validate({sentinel: sentinel}, SCHEMA)
        self.assert_no_sentinel(sentinel, "\n".join(errors))
        self.assertTrue(errors[0].startswith("$.[redacted-key]: public-safety scan hit "))

    def test_schema_diagnostics_redact_secret_shaped_keys(self):
        sentinel = "gh" + "p_" + "SyntheticPayloadNeverEcho42"
        record = fresh_record()
        record["tester"][sentinel] = "ok"
        code, out, err = self.cli(json.dumps(record))
        errors = checker.validate(record, SCHEMA, allow_mock=True)
        self.assert_no_sentinel(sentinel, "\n".join(errors), out, err)
        self.assertEqual(code, 1)
        self.assertEqual(errors, ["$.tester: unexpected field '[redacted-key]'"])
        # Custom schemas may name such a key as a property or required field.
        schema = {"required": [sentinel], "properties": {sentinel: {"type": "object"}}}
        errors = checker.validate({}, schema)
        self.assert_no_sentinel(sentinel, "\n".join(errors))
        self.assertEqual(errors, ["$: missing required field '[redacted-key]'"])
        errors = checker.validate({sentinel: "ok"}, schema)
        self.assert_no_sentinel(sentinel, "\n".join(errors))
        self.assertEqual(errors, ["$.[redacted-key]: expected object, got str"])

    def test_production_target_rejected(self):
        record = fresh_record()
        record["environment"]["target"] = "production"
        errors = checker.validate(record, SCHEMA)
        self.assertTrue(errors, "production target must fail")

    def test_cli_accepts_mock_with_flag(self):
        fixture = (
            ROOT / "scripts" / "fixtures" / "staging_e2e_run_record_mock.json"
        )
        with tempfile.TemporaryDirectory(
            dir=os.getenv("PAPERCLIP_RUN_SCRATCH_DIR")
        ) as tmp:
            out = Path(tmp) / "out.txt"
            code = checker.main(
                ["--record", str(fixture), "--allow-mock"]
            )
            out.write_text(str(code), encoding="utf-8")
            self.assertEqual(code, 0)
            self.assertEqual(out.read_text(encoding="utf-8"), "0")

    def test_cli_rejects_mock_without_flag(self):
        fixture = (
            ROOT / "scripts" / "fixtures" / "staging_e2e_run_record_mock.json"
        )
        self.assertEqual(checker.main(["--record", str(fixture)]), 1)


if __name__ == "__main__":
    unittest.main()
