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
from unittest.mock import patch

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


def as_bytes(document):
    return document if isinstance(document, bytes) else document.encode("utf-8")


def nested_arrays(levels):
    document = []
    for _ in range(levels - 1):
        document = [document]
    return document


def nested_dicts(levels):
    document = {}
    for _ in range(levels - 1):
        document = {"a": document}
    return document


def nested_array_text(levels):
    return "[" * levels + "]" * levels


def nested_object_text(levels):
    return '{"a":' * (levels - 1) + "{}" + "}" * (levels - 1)


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

    def test_pass_disposition_with_failed_command_rejected(self):
        for mock in (True, False):
            with self.subTest(mock=mock):
                record = fresh_record(mock=mock)
                record["verdict"]["disposition"] = "PASS"
                errors = checker.validate(record, SCHEMA, allow_mock=True)
                self.assertTrue(
                    any("$.verdict.disposition" in error for error in errors),
                    f"PASS over a failed command must fail (mock={mock}): {errors}",
                )
                self.assertTrue(
                    any("NEEDS WORK" in error for error in errors),
                    f"rejection must point at NEEDS WORK: {errors}",
                )

    def test_cli_rejects_pass_with_failed_command(self):
        record = fresh_record()
        record["verdict"]["disposition"] = "PASS"
        code, out, err = self.cli(json.dumps(record))
        self.assertEqual(code, 1)
        self.assertIn("$.verdict.disposition", out)
        self.assertEqual(err, "")

    def test_needs_work_with_failed_command_valid(self):
        record = fresh_record(mock=False)
        failed = [c for c in record["commands"] if c["result"] == "fail"]
        self.assertTrue(failed, "fixture must contain a failed command")
        self.assertEqual(record["verdict"]["disposition"], "NEEDS WORK")
        self.assertEqual(checker.validate(record, SCHEMA), [])

    def test_pass_with_all_passing_commands_valid(self):
        record = fresh_record(mock=False)
        for command in record["commands"]:
            command["result"] = "pass"
            command.pop("failure_signature", None)
        record["verdict"]["disposition"] = "PASS"
        record["verdict"]["summary"] = "All commands passed."
        self.assertEqual(checker.validate(record, SCHEMA), [])

    def test_pass_with_skipped_commands_valid(self):
        record = fresh_record(mock=False)
        for command in record["commands"]:
            if command["result"] == "fail":
                command["result"] = "skipped"
                command.pop("failure_signature", None)
        record["verdict"]["disposition"] = "PASS"
        record["verdict"]["summary"] = "Passing commands plus skipped gated surfaces."
        self.assertEqual(checker.validate(record, SCHEMA), [])

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
            record.write_bytes(as_bytes(record_text))
            argv = ["--record", str(record)]
            if allow_mock:
                argv.append("--allow-mock")
            if schema_text is not None:
                schema = Path(tmp) / "schema.json"
                schema.write_bytes(as_bytes(schema_text))
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
        self.assertEqual(len(errors), 1)
        self.assertTrue(errors[0].startswith(
            "$.tester.[redacted-key]: public-safety scan hit "))
        # Custom schemas may name such a key as a property or required field.
        schema = {"required": [sentinel], "properties": {sentinel: {"type": "object"}}}
        errors = checker.validate({}, schema)
        self.assert_no_sentinel(sentinel, "\n".join(errors))
        self.assertEqual(errors, ["$: missing required field '[redacted-key]'"])
        errors = checker.validate({sentinel: "ok"}, schema)
        self.assert_no_sentinel(sentinel, "\n".join(errors))
        self.assertEqual(len(errors), 1)
        self.assertTrue(errors[0].startswith(
            "$.[redacted-key]: public-safety scan hit "))

    def test_secret_keys_rejected_across_every_marker_family_and_depth(self):
        # Keys, not their innocent values, must trigger the same public-safety
        # refusal. Split synthetic markers so no credential literal is stored.
        prefixes = (
            ("tracker-one", "TO" + "G-12345 "),
            ("tracker-two", "PA" + "P-12345 "),
            ("github-classic", "gh" + "p_"),
            ("github-oauth", "gh" + "o_"),
            ("github-fine", "github" + "_pat_"),
            *(("slack-" + kind, "xo" + "x" + kind + "-") for kind in "bpras"),
            *(("private-" + kind.strip(), "BEGIN " + kind + "PRIVATE KEY ")
              for kind in ("", "RSA ", "EC ", "OPENSSH ")),
            ("discord", "Discord " + "token="),
            ("discord-lower", "discord " + "Token:"),
        )
        for label, prefix in prefixes:
            sentinel = prefix + "KeyPayloadNeverEcho42"
            self.assertTrue(any(marker.search(sentinel) for marker in checker.SECRET_MARKERS),
                            "every key fixture must match an existing marker")
            for depth, document, path in (
                ("root", {sentinel: "innocent"}, "$.[redacted-key]"),
                ("object", {"outer": {sentinel: None}}, "$.outer.[redacted-key]"),
                ("array", {"outer": [{sentinel: 7}]}, "$.outer[0].[redacted-key]"),
            ):
                with self.subTest(family=label, depth=depth):
                    errors = checker.validate(document, {}, allow_mock=True)
                    joined = "\n".join(errors)
                    self.assertFalse(sentinel in joined, "secret key leaked")
                    self.assertFalse("KeyPayloadNeverEcho42" in joined, "key payload leaked")
                    self.assertEqual(len(errors), 1, "secret key must be refused once")
                    self.assertTrue(errors[0].startswith(
                        path + ": public-safety scan hit "), "refusal must name a redacted path")

    def test_safe_keys_remain_valid_and_values_remain_scanned(self):
        self.assertEqual(checker.validate({"outer": [{"safe_key": "innocent"}]}, {}), [])
        sentinel = "gh" + "p_" + "KeyPayloadNeverEcho42"
        errors = checker.validate({"outer": [{"safe_key": sentinel}]}, {})
        self.assertEqual(len(errors), 1)
        self.assertTrue(errors[0].startswith("$.outer[0].safe_key: public-safety scan hit "))
        self.assertFalse(sentinel in "\n".join(errors), "secret value leaked")

    def document_byte_limit(self):
        # Default handoff budget: 1 MiB, far above either checked-in fixture.
        # A published MAX_DOCUMENT_BYTES lets the implementation choose its
        # finite budget without changing protected boundary tests.
        limit = getattr(checker, "MAX_DOCUMENT_BYTES", 1024 * 1024)
        self.assertIsInstance(limit, int)
        self.assertGreater(limit, max(len(json.dumps(MOCK)), len(json.dumps(SCHEMA))))
        self.assertLessEqual(limit, 16 * 1024 * 1024, "offline fixture budget must stay bounded")
        return limit

    def assert_byte_limit_refusal(self, record, schema, kind):
        code, out, err = self.cli(record, schema)
        # Fixed, content-free diagnostic, not a parse error or traceback.
        self.assertEqual(code, 1)
        self.assertTrue(out.rstrip().endswith(
            f"cannot load {kind}: document exceeds byte limit"),
            "oversize input must have a fixed byte-limit refusal before parsing")
        self.assertEqual(err, "")
        self.assertFalse("ReadPayloadNeverEcho42" in out, "oversize content leaked")

    def test_document_reader_never_requests_an_unbounded_read(self):
        limit = self.document_byte_limit()
        test = self

        class BudgetedReader:
            # Wrap real file IO, not the decoder. Even a file that was small
            # at stat time can grow: a post-read length check is too late.
            def __init__(self, stream):
                self.stream = stream
                self.consumed = 0

            def __enter__(self):
                self.stream.__enter__()
                return self

            def __exit__(self, *args):
                return self.stream.__exit__(*args)

            def read(self, size=-1):
                test.assertGreaterEqual(size, 0, "document reader requested an unbounded read")
                test.assertLessEqual(size, limit + 1, "read request exceeds cap plus EOF witness")
                data = self.stream.read(size)
                self.consumed += len(data)
                test.assertLessEqual(self.consumed, limit + 1, "document reader consumed beyond cap")
                return data

            def __getattr__(self, name):
                return getattr(self.stream, name)

        with tempfile.TemporaryDirectory(dir=os.getenv("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
            path = Path(tmp) / "document.json"
            path.write_bytes(b'{"safe": true}')
            real_open = Path.open

            def budgeted_open(open_path, *args, **kwargs):
                return BudgetedReader(real_open(open_path, *args, **kwargs))

            with patch.object(Path, "open", budgeted_open):
                self.assertEqual(checker.load_document(path), {"safe": True})

    def test_record_read_refuses_one_byte_over_cap_before_parsing(self):
        limit = self.document_byte_limit()
        # Invalid UTF-8 at the last byte must not win over the size refusal.
        oversized = b" " * limit + b"\xff"
        self.assert_byte_limit_refusal(oversized, None, "record")

    def test_schema_read_refuses_one_byte_over_cap_before_parsing(self):
        limit = self.document_byte_limit()
        self.assert_byte_limit_refusal(json.dumps(MOCK), b" " * limit + b"\xff", "schema")

    def test_oversized_valid_record_and_schema_are_not_accepted(self):
        limit = self.document_byte_limit()
        for kind in ("record", "schema"):
            with self.subTest(kind=kind):
                document = copy.deepcopy(MOCK if kind == "record" else SCHEMA)
                padding = "ReadPayloadNeverEcho42" + "a" * limit
                if kind == "record":
                    document["verdict"]["summary"] = padding
                else:
                    document["description"] = padding
                text = json.dumps(document)
                record, schema = (text, None) if kind == "record" else (json.dumps(MOCK), text)
                self.assert_byte_limit_refusal(record, schema, kind)

    def test_exact_byte_cap_and_one_below_still_load(self):
        limit = self.document_byte_limit()
        record = json.dumps(MOCK).encode("utf-8")
        schema = json.dumps(SCHEMA).encode("utf-8")
        for size in (limit - 1, limit):
            for kind in ("record", "schema"):
                with self.subTest(size=size, kind=kind):
                    record_bytes = record + b" " * (size - len(record)) if kind == "record" else record
                    schema_bytes = schema + b" " * (size - len(schema)) if kind == "schema" else schema
                    code, out, err = self.cli(record_bytes, schema_bytes)
                    self.assertEqual(code, 0)
                    self.assertIn("valid staging E2E run record", out)
                    self.assertEqual(err, "")

    def test_byte_cap_counts_encoded_bytes_not_unicode_characters(self):
        limit = self.document_byte_limit()
        for kind in ("record", "schema"):
            with self.subTest(kind=kind):
                document = copy.deepcopy(MOCK if kind == "record" else SCHEMA)
                # Fewer characters than the cap but more UTF-8 bytes. Use a
                # known string field so schema rejection cannot fake the cap.
                if kind == "record":
                    document["verdict"]["summary"] = "é" * (limit // 2)
                else:
                    document["description"] = "é" * (limit // 2)
                text = json.dumps(document, ensure_ascii=False)
                self.assertLess(len(text), limit)
                self.assertGreater(len(text.encode("utf-8")), limit)
                record, schema = (text, None) if kind == "record" else (json.dumps(MOCK), text)
                self.assert_byte_limit_refusal(record, schema, kind)

    def test_unsupported_property_condition_operators_are_loudly_refused(self):
        constraints = (
            ("enum", {"enum": [True]}),
            ("pattern", {"pattern": "^pass$"}),
            ("type", {"type": "boolean"}),
            ("unknown", {"futureOperator": "ConditionPayloadNeverEcho42"}),
            ("mixed", {"const": True, "enum": [True]}),
            ("nonmatching-const", {"const": False, "enum": [True]}),
        )
        for label, constraint in constraints:
            with self.subTest(operator=label):
                schema = {"allOf": [{
                    "if": {"properties": {"mock": constraint}}, "then": {},
                }]}
                errors = checker.validate(MOCK, schema, allow_mock=True)
                self.assertTrue(errors, "unsupported condition must not silently satisfy or skip")
                joined = "\n".join(errors)
                self.assertIn("unsupported", joined.lower())
                self.assertIn("condition", joined.lower())
                self.assertFalse("ConditionPayloadNeverEcho42" in joined, "condition value leaked")

    def test_unsupported_outer_condition_operator_is_refused(self):
        schema = {"allOf": [{"if": {"required": ["mock"]}, "then": {}}]}
        errors = checker.validate(MOCK, schema, allow_mock=True)
        self.assertTrue(errors, "unsupported outer condition must not silently satisfy")
        self.assertIn("unsupported", "\n".join(errors).lower())
        self.assertIn("condition", "\n".join(errors).lower())

    def test_cli_reports_unsupported_condition_without_traceback(self):
        schema = {"allOf": [{
            "if": {"properties": {"mock": {"enum": [True]}}}, "then": {},
        }]}
        code, out, err = self.cli(json.dumps(MOCK), json.dumps(schema))
        self.assertEqual(code, 1)
        self.assertIn("unsupported", out.lower())
        self.assertIn("condition", out.lower())
        self.assertEqual(err, "")

    def test_supported_const_condition_keeps_true_and_false_branches(self):
        for value, expected in (
            (True, ["$: missing required field 'proof'"]), (False, []),
        ):
            with self.subTest(const=value):
                schema = {"allOf": [{
                    "if": {"properties": {"mock": {"const": value}}},
                    "then": {"required": ["proof"]},
                }]}
                self.assertEqual(checker.validate(MOCK, schema, allow_mock=True), expected)
        self.assertEqual(checker.validate(MOCK, SCHEMA, allow_mock=True), [])

    def test_production_target_rejected(self):
        record = fresh_record()
        record["environment"]["target"] = "production"
        errors = checker.validate(record, SCHEMA)
        self.assertTrue(errors, "production target must fail")

    def test_reversed_window_rejected(self):
        record = fresh_record(mock=False)
        record["window"]["start_utc"] = "2026-10-04T00:18:30Z"
        record["window"]["end_utc"] = "2026-10-04T00:12:00Z"
        errors = checker.validate(record, SCHEMA)
        self.assertTrue(
            any("$.window.end_utc" in error for error in errors),
            f"reversed window must fail at $.window.end_utc: {errors}",
        )

    def test_zero_length_window_with_instant_command_valid(self):
        record = fresh_record(mock=False)
        record["window"]["start_utc"] = "2026-10-04T00:12:00Z"
        record["window"]["end_utc"] = "2026-10-04T00:12:00Z"
        record["commands"] = [{
            "name": "/rank",
            "started_utc": "2026-10-04T00:12:00Z",
            "duration_ms": 0,
            "result": "pass",
        }]
        self.assertEqual(checker.validate(record, SCHEMA), [])

    def test_command_start_before_window_rejected(self):
        record = fresh_record(mock=False)
        record["commands"][0]["started_utc"] = "2026-10-04T00:11:59Z"
        record["commands"][0]["duration_ms"] = 0
        errors = checker.validate(record, SCHEMA)
        self.assertTrue(
            any("$.commands[0].started_utc" in error for error in errors),
            f"before-window start must fail: {errors}",
        )

    def test_command_start_after_window_rejected(self):
        record = fresh_record(mock=False)
        record["commands"][0]["started_utc"] = "2026-10-04T00:18:31Z"
        record["commands"][0]["duration_ms"] = 0
        errors = checker.validate(record, SCHEMA)
        self.assertTrue(
            any("$.commands[0].started_utc" in error for error in errors),
            f"after-window start must fail: {errors}",
        )

    def test_command_completion_beyond_end_rejected(self):
        record = fresh_record(mock=False)
        record["commands"][2]["started_utc"] = "2026-10-04T00:18:29Z"
        record["commands"][2]["duration_ms"] = 2000
        errors = checker.validate(record, SCHEMA)
        self.assertTrue(
            any("$.commands[2]" in error for error in errors),
            f"completion beyond end must fail: {errors}",
        )

    def test_exact_boundary_starts_and_completion_valid(self):
        record = fresh_record(mock=False)
        record["commands"] = [
            {
                "name": "/rank",
                "started_utc": "2026-10-04T00:12:00Z",
                "duration_ms": 0,
                "result": "pass",
            },
            {
                "name": "/leaderboard",
                "started_utc": "2026-10-04T00:18:30Z",
                "duration_ms": 0,
                "result": "pass",
            },
            {
                "name": "/help",
                "started_utc": "2026-10-04T00:18:29Z",
                "duration_ms": 1000,
                "result": "pass",
            },
        ]
        record["verdict"]["disposition"] = "PASS"
        record["verdict"]["summary"] = "Boundary commands pass."
        self.assertEqual(checker.validate(record, SCHEMA), [])

    def test_second_precision_end_allows_subsecond_truncation_overrun(self):
        # Producer emits second-precision bounds; a completion <1s past a
        # second-precision end is truncation, not an impossible window.
        record = fresh_record(mock=False)
        record["commands"] = [{
            "name": "/rank",
            "started_utc": "2026-10-04T00:18:30Z",
            "duration_ms": 500,
            "result": "pass",
        }]
        record["verdict"]["disposition"] = "PASS"
        record["verdict"]["summary"] = "Truncation allowance."
        self.assertEqual(checker.validate(record, SCHEMA), [])

    def test_second_precision_end_rejects_overrun_beyond_truncation(self):
        record = fresh_record(mock=False)
        record["commands"] = [{
            "name": "/rank",
            "started_utc": "2026-10-04T00:18:30Z",
            "duration_ms": 1500,
            "result": "pass",
        }]
        record["verdict"]["disposition"] = "PASS"
        record["verdict"]["summary"] = "Beyond truncation."
        errors = checker.validate(record, SCHEMA)
        self.assertTrue(
            any("$.commands[0]" in error for error in errors),
            f"1.5s overrun must fail: {errors}",
        )

    def test_fractional_end_uses_exact_completion_comparison(self):
        record = fresh_record(mock=False)
        record["window"]["end_utc"] = "2026-10-04T00:18:30.000Z"
        record["commands"] = [{
            "name": "/rank",
            "started_utc": "2026-10-04T00:18:30Z",
            "duration_ms": 500,
            "result": "pass",
        }]
        record["verdict"]["disposition"] = "PASS"
        record["verdict"]["summary"] = "Exact fractional end."
        errors = checker.validate(record, SCHEMA)
        self.assertTrue(
            any("$.commands[0]" in error for error in errors),
            f"0.5s past a fractional end must fail: {errors}",
        )

    def test_same_instant_zero_offset_representations_valid(self):
        record = fresh_record(mock=False)
        record["window"]["start_utc"] = "2026-10-04T00:12:00Z"
        record["window"]["end_utc"] = "2026-10-04T00:18:30+00:00"
        record["commands"] = [{
            "name": "/rank",
            "started_utc": "2026-10-04T00:12:03-00:00",
            "duration_ms": 842,
            "result": "pass",
        }]
        record["verdict"]["disposition"] = "PASS"
        record["verdict"]["summary"] = "Same instant, zero-offset spellings."
        self.assertEqual(checker.validate(record, SCHEMA), [])

    def test_naive_and_malformed_window_timestamps_fail_safely(self):
        cases = [
            ("2026-10-04T00:12:00", "2026-10-04T00:18:30Z"),
            ("2026-10-04T00:12:00Z", "not-a-timestamp"),
            ("2026-13-01T00:00:00Z", "2026-10-04T00:18:30Z"),
        ]
        for start, end in cases:
            with self.subTest(start=start, end=end):
                record = fresh_record(mock=False)
                record["window"]["start_utc"] = start
                record["window"]["end_utc"] = end
                errors = checker.validate(record, SCHEMA)
                self.assertTrue(errors, f"malformed window {start}/{end} must fail")

    def test_naive_command_start_fails_safely_without_crash(self):
        record = fresh_record(mock=False)
        record["commands"][0]["started_utc"] = "2026-10-04T00:12:03"
        errors = checker.validate(record, SCHEMA)
        self.assertTrue(
            any("$.commands[0].started_utc" in error for error in errors),
            f"naive command start must fail safely: {errors}",
        )
        window_errors = [e for e in errors if "window ordering" in e]
        self.assertTrue(window_errors, f"window ordering must report: {errors}")
        self.assertNotIn(
            "2026-10-04T00:12:03", "\n".join(window_errors)
        )

    def test_first_readyz_may_predate_or_follow_window(self):
        for first_readyz in ("2026-10-03T23:59:00Z", "2026-10-04T00:20:00Z"):
            with self.subTest(first_readyz=first_readyz):
                record = fresh_record(mock=False)
                record["deployment"]["first_readyz_utc"] = first_readyz
                self.assertEqual(
                    checker.validate(record, SCHEMA), [],
                    f"first_readyz {first_readyz} must not gate the window",
                )

    def test_cli_rejects_reversed_window(self):
        record = fresh_record(mock=False)
        record["window"]["start_utc"] = "2026-10-04T00:18:30Z"
        record["window"]["end_utc"] = "2026-10-04T00:12:00Z"
        code, out, err = self.cli(json.dumps(record))
        self.assertEqual(code, 1)
        self.assertIn("$.window.end_utc", out)
        self.assertEqual(err, "")

    def test_utc_fields_require_timezone_aware_utc(self):
        # *_utc fields accept Z or an explicit zero offset (negative zero
        # normalizes to UTC); naive, nonzero-offset and impossible dates fail.
        # Bases stay inside the mock execution window so the window-ordering
        # rule (PR #856) does not mask the UTC-representation rule: window
        # 00:12:00Z-00:18:30Z, first command at 00:12:03Z.
        suffixes = [
            "Z",
            "+00:00",
            "-00:00",
            ".123Z",
            ".123456+00:00",
            "+0000",
            "-0000",
        ]
        bases = {
            "window.start_utc": "2026-10-04T00:12:00",
            "window.end_utc": "2026-10-04T00:18:30",
            "commands.started_utc": "2026-10-04T00:12:03",
            "deployment.first_readyz_utc": "2026-10-04T00:00:41",
        }
        valid_by_field = {
            field: [base + suffix for suffix in suffixes]
            for field, base in bases.items()
        }
        valid = [
            "2026-10-11T00:00:00Z",
            "2026-10-11T00:00:00+00:00",
            "2026-10-11T00:00:00-00:00",
            "2026-10-11T00:00:00.123Z",
            "2026-10-11T00:00:00.123456+00:00",
            "2026-10-11T00:00:00+0000",
            "2026-10-11T00:00:00-0000",
        ]
        invalid = [
            "2026-10-11T00:00:00",
            "2026-10-11T00:00:00.123",
            "2026-10-11T00:00:00+03:00",
            "2026-10-11T00:00:00-05:00",
            "2026-10-11T00:00:00+00:30",
            "2026-02-30T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "not-a-timestamp",
        ]

        def set_field(record, field, value):
            if field == "window.start_utc":
                record["window"]["start_utc"] = value
            elif field == "window.end_utc":
                record["window"]["end_utc"] = value
            elif field == "commands.started_utc":
                record["commands"][0]["started_utc"] = value
            elif field == "deployment.first_readyz_utc":
                record["deployment"]["first_readyz_utc"] = value
            else:
                raise AssertionError(f"unknown field {field}")

        def json_path(field):
            return {
                "window.start_utc": "$.window.start_utc",
                "window.end_utc": "$.window.end_utc",
                "commands.started_utc": "$.commands[0].started_utc",
                "deployment.first_readyz_utc": "$.deployment.first_readyz_utc",
            }[field]

        fields = (
            "window.start_utc",
            "window.end_utc",
            "commands.started_utc",
            "deployment.first_readyz_utc",
        )
        for field in fields:
            for stamp in valid_by_field[field]:
                with self.subTest(field=field, stamp=stamp, kind="valid"):
                    record = fresh_record(mock=False)
                    set_field(record, field, stamp)
                    errors = checker.validate(record, SCHEMA)
                    self.assertEqual(errors, [], f"{field} {stamp} must validate: {errors}")
            for stamp in invalid:
                with self.subTest(field=field, stamp=stamp, kind="invalid"):
                    record = fresh_record(mock=False)
                    set_field(record, field, stamp)
                    errors = checker.validate(record, SCHEMA, allow_mock=True)
                    self.assertTrue(
                        any(json_path(field) in error for error in errors),
                        f"{field} {stamp} must fail at {json_path(field)}: {errors}",
                    )

    def test_cli_rejects_naive_utc_timestamp(self):
        record = fresh_record(mock=False)
        record["window"]["start_utc"] = "2026-10-11T00:00:00"
        code, out, err = self.cli(json.dumps(record))
        self.assertEqual(code, 1)
        self.assertIn("$.window.start_utc", out)
        self.assertEqual(err, "")

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

    def test_nesting_budget_admits_limit_and_refuses_one_deeper(self):
        limit = checker.MAX_NESTING_DEPTH
        for text_for, document_for in (
            (nested_array_text, nested_arrays),
            (nested_object_text, nested_dicts),
        ):
            with self.subTest(kind=text_for.__name__):
                self.assertEqual(
                    checker.load_json(text_for(limit)), document_for(limit)
                )
                with self.assertRaises(checker.DecoderLimitError) as caught:
                    checker.load_json(text_for(limit + 1))
                self.assertEqual(str(caught.exception), checker.NESTING_MESSAGE)

    def test_decoder_recursion_overflow_refused_as_nesting_limit(self):
        for text in (nested_array_text(100_000), nested_object_text(100_000)):
            with self.subTest(prefix=text[:6]):
                with self.assertRaises(checker.DecoderLimitError) as caught:
                    checker.load_json(text)
                self.assertEqual(str(caught.exception), checker.NESTING_MESSAGE)

    def test_oversized_integer_literal_refused_without_echoing_digits(self):
        with self.assertRaises(checker.DecoderLimitError) as caught:
            checker.load_json('{"x": ' + "9" * 5000 + "}")
        self.assertEqual(str(caught.exception), checker.DECODER_DIGITS_MESSAGE)
        self.assertNotIn("9999", str(caught.exception))

    def test_validator_refuses_directly_supplied_deep_structures(self):
        expected = [f"$: {checker.NESTING_MESSAGE}"]
        self.assertEqual(checker.validate(nested_arrays(100_000), {}), expected)
        self.assertEqual(checker.validate(nested_dicts(100_000), {}), expected)
        self.assertEqual(checker.validate(MOCK, nested_dicts(100_000)), expected)

    def test_validator_accepts_structures_at_nesting_limit(self):
        limit = checker.MAX_NESTING_DEPTH
        self.assertEqual(checker.validate(nested_arrays(limit), {}), [])
        self.assertEqual(checker.validate(nested_dicts(limit), {}), [])
        self.assertEqual(
            checker.validate(nested_dicts(limit + 1), {}),
            [f"$: {checker.NESTING_MESSAGE}"],
        )

    def test_duplicate_key_at_nesting_limit_still_refused(self):
        limit = checker.MAX_NESTING_DEPTH
        text = '{"a":' * (limit - 1) + '{"k":1,"k":2}' + "}" * (limit - 1)
        with self.assertRaises(checker.DuplicateKeyError) as caught:
            checker.load_json(text)
        self.assertTrue(str(caught.exception).endswith(".k: duplicate object key"))

    def test_cli_refuses_deep_record_and_schema_without_traceback(self):
        for name, text in (
            ("array", nested_array_text(100_000)),
            ("object", nested_object_text(100_000)),
        ):
            with self.subTest(record=name):
                code, out, err = self.cli(text)
                self.assertEqual(code, 1)
                self.assertTrue(out.rstrip().endswith(
                    f"cannot load record: {checker.NESTING_MESSAGE}"))
                self.assertEqual(err, "")
        code, out, err = self.cli(json.dumps(MOCK), nested_array_text(100_000))
        self.assertEqual(code, 1)
        self.assertTrue(out.rstrip().endswith(
            f"cannot load schema: {checker.NESTING_MESSAGE}"))
        self.assertEqual(err, "")

    def test_cli_refuses_oversized_integer_without_traceback(self):
        code, out, err = self.cli('{"x": ' + "9" * 5000 + "}")
        self.assertEqual(code, 1)
        self.assertTrue(out.rstrip().endswith(
            f"cannot load record: {checker.DECODER_DIGITS_MESSAGE}"))
        self.assertEqual(err, "")

    def test_cli_refuses_invalid_utf8_record_without_echoing_bytes(self):
        sentinel = "gh" + "p_" + "SyntheticPayloadNeverEcho42"
        record_bytes = b'{"notes": "' + sentinel.encode("ascii") + b'\xff"}'
        offset = record_bytes.index(b"\xff")
        code, out, err = self.cli(record_bytes)
        self.assertEqual(code, 1)
        self.assertTrue(out.rstrip().endswith(
            f"cannot load record: invalid UTF-8 at byte offset {offset}"))
        self.assert_no_sentinel(sentinel, out, err)
        self.assertNotIn("\\xff", out)
        self.assertEqual(err, "")

    def test_cli_refuses_invalid_utf8_schema_without_traceback(self):
        schema_bytes = b'{"type": "object", "description": "caf\xc3"}'
        offset = schema_bytes.index(b"\xc3")
        code, out, err = self.cli(json.dumps(MOCK), schema_bytes)
        self.assertEqual(code, 1)
        self.assertTrue(out.rstrip().endswith(
            f"cannot load schema: invalid UTF-8 at byte offset {offset}"))
        self.assertEqual(err, "")

    def test_valid_utf8_non_ascii_still_loads(self):
        code, out, err = self.cli(json.dumps({"café": 1}).encode("utf-8"))
        self.assertNotIn("cannot load", out)
        self.assertEqual(err, "")


if __name__ == "__main__":
    unittest.main()
