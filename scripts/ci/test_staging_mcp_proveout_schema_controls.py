"""Additional offline proof controls for schema constraints and recording."""

import json
import unittest

from scripts.ci.test_staging_mcp_proveout_negotiation_acceptance import (
    BEARER,
    NegotiationHelpers,
    envelope,
    run_proof,
    shipped,
)


class SchemaConstraintTests(NegotiationHelpers):
    def test_unimplemented_schema_constraints_cannot_claim_compatibility(self):
        for keyword, value in (
            ("required", None), ("required", False), ("required", {}),
            ("minProperties", 1), ("not", {}), ("allOf", [False]),
            ("$ref", "#/$defs/nonempty"), ("if", {}), ("unknownConstraint", True),
        ):
            with self.subTest(keyword=keyword, value=value):
                result = shipped()["tools_result"]
                result["tools"][0]["inputSchema"][keyword] = value
                self.assert_rejected(1, envelope(1, result), "TOOL_SCHEMA_INVALID")

    def test_nonasserting_schema_metadata_remains_compatible(self):
        result = shipped()["tools_result"]
        result["tools"][0]["inputSchema"].update({
            "title": "Status", "description": "No arguments", "$comment": "read only",
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.org/status", "default": {}, "examples": [{}],
            "$defs": {"unused": False}, "definitions": {"unused": False},
            "deprecated": False, "readOnly": True, "writeOnly": False,
            "required": [],
        })
        self.assert_receipt(run_proof({1: envelope(1, result)}), 0)


class DuplicateRecordingTests(NegotiationHelpers):
    def test_escaped_bearer_in_duplicate_json_is_redacted_without_erasing_ambiguity(self):
        encoded = '"' + ''.join('\\u%04x' % ord(c) for c in BEARER) + '"'
        for field in ('"note":' + encoded, encoded + ':"note"'):
            with self.subTest(field=field):
                body = ('{"jsonrpc":"2.0","id":1,"id":1,' + field +
                        ',"result":{"protocolVersion":"2025-06-18"}}')
                receipt = run_proof({0: body})
                artifact = self.assert_receipt(receipt, 1)
                recorded = artifact["probes"][0]["body"]["_text"]
                self.assertEqual(recorded.count('"id":1'), 2)
                self.assertNotIn(encoded, recorded)
                self.assertNotIn(BEARER, json.dumps(json.loads(recorded)))
                self.assertIn("[JSON_DUPLICATE_KEY]", artifact["failures"][0])


class MalformedRedactionTests(NegotiationHelpers):
    def test_malformed_bodies_redact_raw_and_escaped_bearer(self):
        lower = ''.join('\\u%04x' % ord(c) for c in BEARER)
        upper = ''.join('\\u%04X' % ord(c) for c in BEARER)
        bodies = {
            "stray-quote": 'upstream failure " {"note":"' + lower + '"}',
            "raw-control": '{"note":"a\n' + lower + '"}',
            "invalid-escape": '{"note":"' + lower + '\\q"}',
            "stray-quote-upper": 'upstream failure " {"note":"' + upper + '"}',
        }
        for name, body in bodies.items():
            with self.subTest(shape=name):
                receipt = run_proof({0: body})
                artifact = self.assert_receipt(receipt, 1)
                _code, stdout, _artifact, raw, _calls = receipt
                recorded = artifact["probes"][0]["body"]["_text"]
                for form in (BEARER, lower, upper):
                    with self.subTest(form=form[:12]):
                        self.assertNotIn(form, recorded)
                        self.assertNotIn(form, stdout)
                        self.assertNotIn(form.encode(), raw)
                        # stdout and the artifact JSON escape backslashes, so
                        # a leaked escaped bearer appears double-escaped there.
                        double = form.replace("\\", "\\\\")
                        self.assertNotIn(double, stdout)
                        self.assertNotIn(double.encode(), raw)


if __name__ == "__main__":
    unittest.main()
