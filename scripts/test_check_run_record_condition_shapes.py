#!/usr/bin/env python3
"""Offline regression tests for unsupported conditional schema shapes."""

import io
import json
import os
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

import check_run_record as checker


class ConditionShapeTests(unittest.TestCase):
    def test_non_object_outer_conditions_fail_before_then(self):
        for condition in (42, False, True, "condition-value", [], None):
            for then_fields in ({}, {"then": {}}, {"then": {"required": ["proof"]}}):
                with self.subTest(condition=condition, then_fields=then_fields):
                    schema = {"allOf": [{"if": condition} | then_fields]}
                    self.assertEqual(
                        checker.validate({}, schema),
                        ["$: unsupported condition in if "
                         "(supported: an object of properties conditions)"],
                    )

    def test_direct_non_object_condition_reports_error(self):
        for condition in (42, False, True, "condition-value", [], None):
            with self.subTest(condition=condition):
                errors = []
                self.assertFalse(checker.condition_holds({}, condition, "$", errors))
                self.assertEqual(
                    errors,
                    ["$: unsupported condition in if "
                     "(supported: an object of properties conditions)"],
                )

    def test_absent_if_and_supported_condition_without_then_remain_inert(self):
        for subschema in ({}, {"then": {"required": ["proof"]}}, {"if": {}}):
            with self.subTest(subschema=subschema):
                self.assertEqual(checker.validate({}, {"allOf": [subschema]}), [])

    def test_non_object_property_subschemas_fail_before_then(self):
        for subschema in (42, False, True, "condition-value", [], None):
            for record in ({"mock": True}, {}):
                for then in ({}, {"required": ["proof"]}):
                    with self.subTest(subschema=subschema, record=record, then=then):
                        schema = {"allOf": [{
                            "if": {"properties": {"mock": subschema}},
                            "then": then,
                        }]}
                        self.assertEqual(
                            checker.validate(record, schema, allow_mock=True),
                            ["$: unsupported condition in if.properties.mock "
                             "(supported: an object with const)"],
                        )

    def test_non_object_properties_fail_before_then(self):
        for props in (42, False, True, "condition-value", [], None):
            with self.subTest(properties=props):
                schema = {"allOf": [{
                    "if": {"properties": props},
                    "then": {"required": ["proof"]},
                }]}
                self.assertEqual(
                    checker.validate({"mock": True}, schema, allow_mock=True),
                    ["$: unsupported condition in if.properties "
                     "(supported: an object of const conditions)"],
                )

    def test_invalid_shape_reported_even_after_a_const_mismatch(self):
        schema = {"allOf": [{
            "if": {"properties": {"mock": {"const": False}, "other": 42}},
            "then": {},
        }]}
        self.assertEqual(
            checker.validate({"mock": True}, schema, allow_mock=True),
            ["$: unsupported condition in if.properties.other "
             "(supported: an object with const)"],
        )

    def test_non_object_subschema_diagnostic_redacts_key_and_value(self):
        key = "ghp_" + "syntheticConditionKey"
        value = "Bearer " + "syntheticConditionValue"
        schema = {"allOf": [{
            "if": {"properties": {key: value}},
            "then": {"required": ["proof"]},
        }]}
        self.assertEqual(
            checker.validate({}, schema),
            ["$: unsupported condition in if.properties.[redacted-key] "
             "(supported: an object with const)"],
        )

    def test_supported_conditions_preserve_then_behavior(self):
        for properties, applies in (
            ({"mock": {"const": True}}, True),
            ({"mock": {"const": False}}, False),
            ({"mock": {}}, True),
            ({}, True),
        ):
            with self.subTest(properties=properties):
                schema = {"allOf": [{
                    "if": {"properties": properties},
                    "then": {"required": ["proof"]},
                }]}
                self.assertEqual(
                    checker.validate({"mock": True}, schema, allow_mock=True),
                    ["$: missing required field 'proof'"] if applies else [],
                )

    def test_cli_refuses_non_object_outer_condition_without_echo(self):
        value = "Bearer " + "syntheticConditionValue"
        with tempfile.TemporaryDirectory(
            dir=os.getenv("PAPERCLIP_RUN_SCRATCH_DIR")
        ) as tmp:
            record_path = Path(tmp) / "record.json"
            schema_path = Path(tmp) / "schema.json"
            record_path.write_text("{}", encoding="utf-8")
            schema_path.write_text(json.dumps({"allOf": [{
                "if": value, "then": {"required": ["proof"]},
            }]}), encoding="utf-8")
            out, err = io.StringIO(), io.StringIO()
            with redirect_stdout(out), redirect_stderr(err):
                code = checker.main([
                    "--record", str(record_path), "--schema", str(schema_path),
                ])
            self.assertEqual(code, 1)
            self.assertEqual(err.getvalue(), "")
            self.assertEqual(
                out.getvalue(),
                f"{record_path}: $: unsupported condition in if "
                "(supported: an object of properties conditions)\n",
            )

    def test_cli_refuses_non_object_subschema_without_echo(self):
        key = "ghp_" + "syntheticConditionKey"
        value = "Bearer " + "syntheticConditionValue"
        with tempfile.TemporaryDirectory(
            dir=os.getenv("PAPERCLIP_RUN_SCRATCH_DIR")
        ) as tmp:
            record_path = Path(tmp) / "record.json"
            schema_path = Path(tmp) / "schema.json"
            record_path.write_text("{}", encoding="utf-8")
            schema_path.write_text(json.dumps({"allOf": [{
                "if": {"properties": {key: value}}, "then": {},
            }]}), encoding="utf-8")
            out, err = io.StringIO(), io.StringIO()
            with redirect_stdout(out), redirect_stderr(err):
                code = checker.main([
                    "--record", str(record_path), "--schema", str(schema_path),
                ])
            self.assertEqual(code, 1)
            self.assertEqual(err.getvalue(), "")
            self.assertEqual(
                out.getvalue(),
                f"{record_path}: $: unsupported condition in "
                "if.properties.[redacted-key] "
                "(supported: an object with const)\n",
            )


if __name__ == "__main__":
    unittest.main()
