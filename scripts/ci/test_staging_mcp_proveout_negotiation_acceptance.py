"""Protected acceptance: unique JSON and the shipped MCP negotiation contract.

Test-only handoff; the Builder must preserve this file and its JSON fixture.
Expectations come from wrangler/src/mcp.ts:36-38,55-61,79-107,607-626, not
from the prove-out's validators. No production fix belongs in this branch.

Admission rejects repeated decoded member names at every object depth (even
identical values or escape-equivalent names). Bounded/redacted recording is
not admission. Both shipped string protocol versions may negotiate; truthy
non-strings and unknown versions may not. Each of the five unique advertised
tools must retain its zero-argument object schema. This is not a generic JSON
Schema engine and does not tighten tool-content compatibility.

Failures retain existing stage IDs, exit 1 and proof_disposition=failed.
The fixed codes are JSON_DUPLICATE_KEY, PROTOCOL_UNSUPPORTED and
TOOL_SCHEMA_INVALID; each has a bounded `next:` action, without remote text.
The Builder also updates the concise proof contract and its offline command.

Run alongside the earlier envelope/content/fence suite and legacy guards:
  PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/ci -p 'test_staging_mcp_proveout*.py' -v

All transport is doubled at do_post; real transport construction is forbidden.
Only synthetic bearers and response bodies are used. No live service calls.
"""

import io
import json
import os
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

from scripts.ci import test_staging_mcp_proveout_envelope_acceptance as framing

proveout = framing.proveout
ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "scripts/ci/fixtures/staging_mcp_shipped_contract.json"
STAGES = [
    "initialize", "tools/list", "tools/call get_status", "tools/call get_gateway",
    "tools/call get_backfill_progress", "tools/call get_config",
    "tools/call get_audit", "tools/call unknown", "wrong bearer", "no bearer",
]
REMOTE = "SENTINEL-remote-negotiation"
BEARER = "test-bearer-negotiation-acceptance-0000"


def shipped():
    return json.loads(FIXTURE.read_text())


def envelope(index, result):
    return json.dumps({"jsonrpc": "2.0", "id": index + 1, "result": result})


def run_proof(overrides=None, fenced=False):
    responses = framing.valid_script()
    contract = shipped()
    responses[0] = (200, {}, envelope(0, contract["initialize_result"]))
    responses[1] = (200, {}, envelope(1, contract["tools_result"]))
    for index, body in (overrides or {}).items():
        responses[index] = body if isinstance(body, Exception) else (200, {}, body)
    env = {proveout.TOKEN_ENV: BEARER, proveout.URL_ENV: proveout.DEFAULT_URL}
    if fenced:
        env[proveout.EXPECT_FENCE_ENV] = "1"
    with tempfile.TemporaryDirectory(
            dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")) as tmp:
        output = Path(tmp) / "receipt.json"
        stdout = io.StringIO()
        with mock.patch.object(sys, "argv", ["proveout", "--output", str(output)]), \
                mock.patch.dict(os.environ, env, clear=True), \
                mock.patch.object(proveout, "do_post", side_effect=responses) as post, \
                mock.patch("urllib.request.build_opener", side_effect=AssertionError(
                    "real transport forbidden")), redirect_stdout(stdout):
            code = proveout.main()
        raw = output.read_bytes()
    return code, stdout.getvalue(), json.loads(raw), raw, post.call_args_list


def duplicate_pair(key, first, second, escaped=False):
    """Build raw JSON: dict construction would silently erase the defect."""
    left = json.dumps(key)
    right = ('"\\u%04x%s"' % (ord(key[0]), key[1:])) if escaped else left
    return f"{left}:{json.dumps(first)},{right}:{json.dumps(second)}"


class NegotiationHelpers(unittest.TestCase):
    def assert_receipt(self, receipt, expected_code):
        code, stdout, artifact, raw, calls = receipt
        self.assertEqual(code, expected_code)
        self.assertEqual(artifact["proof_disposition"],
                         "proved" if expected_code == 0 else "failed")
        self.assertEqual([probe["name"] for probe in artifact["probes"]], STAGES)
        self.assertEqual(len(calls), 10)
        self.assertNotIn(BEARER, stdout)
        self.assertNotIn(BEARER.encode(), raw)
        self.assertLess(len(raw), 2 * proveout.MAX_BODY_BYTES)
        if expected_code == 0:
            self.assertEqual(artifact["failures"], [])
        return artifact

    def assert_rejected(self, index, body, reason):
        receipt = run_proof({index: body})
        artifact = self.assert_receipt(receipt, 1)
        failures = artifact["failures"]
        self.assertEqual(len(failures), 1)
        failure = failures[0]
        self.assertTrue(failure.startswith(STAGES[index] + ":"))
        self.assertIn(f"[{reason}]", failure)
        self.assertIn("next:", failure)
        self.assertTrue(failure.split("next:", 1)[1].strip())
        self.assertLessEqual(len(failure), 512)
        self.assertLessEqual(len(failure.split("next:", 1)[1]), 240)
        self.assertNotIn(REMOTE, failure)
        self.assertIn("- " + failure, receipt[1])
        return artifact


class UniqueReplyTests(NegotiationHelpers):
    def test_duplicate_id_values_orders_and_escape_equivalence_fail(self):
        for first, second in ((1, 1), (999, 1), (1, 999)):
            for escaped in (False, True):
                with self.subTest(first=first, second=second, escaped=escaped):
                    pair = duplicate_pair("id", first, second, escaped)
                    body = ('{"jsonrpc":"2.0",' + pair + ',"result":' +
                            json.dumps(shipped()["initialize_result"]) + '}')
                    self.assert_rejected(0, body, "JSON_DUPLICATE_KEY")

    def test_duplicate_result_and_jsonrpc_members_fail(self):
        result = shipped()["initialize_result"]
        for key, good, bad in (("result", result, None),
                               ("jsonrpc", "2.0", "1.0")):
            for first, second in ((good, good), (good, bad), (bad, good)):
                for escaped in (False, True):
                    with self.subTest(key=key, first=first, escaped=escaped):
                        rest = ('"jsonrpc":"2.0","id":1' if key == "result"
                                else '"id":1,"result":' + json.dumps(result))
                        body = '{' + rest + ',' + duplicate_pair(
                            key, first, second, escaped) + '}'
                        self.assert_rejected(0, body, "JSON_DUPLICATE_KEY")

    def test_initialize_duplicate_protocol_is_not_last_key_wins(self):
        good = "2025-06-18"
        for first, second in ((good, good), (REMOTE, good), (good, REMOTE)):
            for escaped in (False, True):
                with self.subTest(first=first, second=second, escaped=escaped):
                    body = ('{"jsonrpc":"2.0","id":1,"result":{' +
                            duplicate_pair("protocolVersion", first, second, escaped) + '}}')
                    self.assert_rejected(0, body, "JSON_DUPLICATE_KEY")

    def test_duplicate_keys_inside_every_authenticated_stage_fail(self):
        # Unused metadata is deliberately duplicated: detection may not be
        # limited to fields the stage validator happens to inspect.
        for index in range(8):
            for escaped in (False, True):
                with self.subTest(stage=STAGES[index], escaped=escaped):
                    body = framing.valid_script()[index][2]
                    body = body[:-1] + ',"metadata":[{' + duplicate_pair(
                        "note", REMOTE, REMOTE, escaped) + '}]}'
                    self.assert_rejected(index, body, "JSON_DUPLICATE_KEY")

    def test_duplicate_schema_members_fail_even_when_last_value_is_valid(self):
        for field, good, bad in (("type", "object", "array"),
                                  ("properties", {}, {"remote": {"type": "string"}}),
                                  ("additionalProperties", False, True)):
            for first, second in ((good, good), (bad, good), (good, bad)):
                for escaped in (False, True):
                    with self.subTest(field=field, first=first, escaped=escaped):
                        result = shipped()["tools_result"]
                        schema = result["tools"][0]["inputSchema"]
                        schema.pop(field)
                        encoded = json.dumps(schema)
                        duplicate = encoded[:-1] + ',' + duplicate_pair(
                            field, first, second, escaped) + '}'
                        body = envelope(1, result).replace(encoded, duplicate, 1)
                        self.assert_rejected(1, body, "JSON_DUPLICATE_KEY")

    def test_duplicate_tool_name_and_schema_container_members_fail(self):
        for field, good, bad in (("name", "get_status", REMOTE),
                                ("inputSchema", {"type": "object", "properties": {},
                                                 "additionalProperties": False}, None)):
            for first, second in ((good, good), (bad, good), (good, bad)):
                for escaped in (False, True):
                    with self.subTest(field=field, first=first, escaped=escaped):
                        result = shipped()["tools_result"]
                        tool = result["tools"][0]
                        tool.pop(field)
                        encoded = json.dumps(tool)
                        duplicate = encoded[:-1] + ',' + duplicate_pair(
                            field, first, second, escaped) + '}'
                        body = envelope(1, result).replace(encoded, duplicate, 1)
                        self.assert_rejected(1, body, "JSON_DUPLICATE_KEY")

    def test_duplicates_in_nested_schema_definitions_fail(self):
        result = shipped()["tools_result"]
        result["tools"][4]["inputSchema"]["$defs"] = {"unused": "MARKER"}
        body = envelope(1, result).replace(
            '"MARKER"', '{' + duplicate_pair("description", REMOTE, REMOTE, True) + '}')
        self.assert_rejected(1, body, "JSON_DUPLICATE_KEY")

    def test_unique_key_check_covers_each_admissible_object_depth(self):
        # Root through depth 31; arrays between objects must not hide duplicates.
        for depth in range(32):
            for wrapper in ('{"child":%s}', '[%s]'):
                with self.subTest(depth=depth, wrapper=wrapper):
                    body = '{"note":1,"n\\u006fte":1}'
                    for _ in range(depth):
                        body = wrapper % body
                    self.assertEqual(proveout.load_bounded_json(body), (False, None))

    def test_bounded_display_does_not_admit_duplicate_json_as_a_dictionary(self):
        body = ('{"jsonrpc":"2.0","id":1,"result":{' +
                duplicate_pair("protocolVersion", BEARER, "2025-06-18", True) + '}}')
        receipt = run_proof({0: body})
        # Recording is allowed, but must retain ambiguity rather than silently
        # display the last-key-wins dict as an admissible result.
        recorded = receipt[2]["probes"][0]["body"]
        self.assertEqual(set(recorded), {"_text"})
        self.assertIn('"protocolVersion"', recorded["_text"])
        self.assertIn('"\\u0070rotocolVersion"', recorded["_text"])
        self.assertNotIn(BEARER, receipt[1])
        self.assertNotIn(BEARER.encode(), receipt[3])
        self.assert_rejected(0, body, "JSON_DUPLICATE_KEY")

    def test_same_keys_in_different_objects_and_json_in_text_are_not_duplicates(self):
        result = {"content": [{"type": "text", "text": '{"a":1,"a":2}'}],
                  "structuredContent": {"left": {"id": 1}, "right": {"id": 2}}}
        self.assert_receipt(run_proof({2: envelope(2, result)}), 0)


class ProtocolNegotiationTests(NegotiationHelpers):
    def test_each_shipped_supported_string_version_can_negotiate(self):
        for version in shipped()["supported_protocol_versions"]:
            with self.subTest(version=version):
                result = shipped()["initialize_result"]
                result["protocolVersion"] = version
                receipt = run_proof({0: envelope(0, result)})
                self.assert_receipt(receipt, 0)
                request = receipt[4][0].args[1]
                self.assertEqual(request["method"], "initialize")
                self.assertEqual(request["params"]["protocolVersion"], "2025-06-18")

    def test_non_string_empty_missing_and_unknown_versions_fail(self):
        for version in (True, False, 1, 0, None, {}, {"value": REMOTE},
                        [], ["2025-06-18"], "", "1999-01-01", REMOTE,
                        "2025-06-18 ", "2025-06-18\x00", "2025-03-26suffix"):
            with self.subTest(version=version):
                result = shipped()["initialize_result"]
                result["protocolVersion"] = version
                self.assert_rejected(0, envelope(0, result), "PROTOCOL_UNSUPPORTED")
        result = shipped()["initialize_result"]
        del result["protocolVersion"]
        self.assert_rejected(0, envelope(0, result), "PROTOCOL_UNSUPPORTED")

    def test_refused_initialize_is_not_successful_negotiation(self):
        body = json.dumps({"jsonrpc": "2.0", "id": 1, "error": {
            "code": -32602, "message": REMOTE}})
        self.assert_rejected(0, body, "PROTOCOL_UNSUPPORTED")

    def test_unknown_version_failure_is_fixed_not_reflected(self):
        failures = []
        for version in (REMOTE, "OTHER-remote-version", BEARER):
            result = shipped()["initialize_result"]
            result["protocolVersion"] = version
            artifact = self.assert_rejected(0, envelope(0, result), "PROTOCOL_UNSUPPORTED")
            failures.append(artifact["failures"])
        self.assertEqual(failures[0], failures[1])
        self.assertEqual(failures[0], failures[2])


class AdvertisedSchemaTests(NegotiationHelpers):
    def test_absent_or_wrong_typed_schema_fails_for_each_tool(self):
        for tool_index in range(5):
            for schema in ("absent", None, False, True, [], "object", 1):
                with self.subTest(tool=tool_index, schema=schema):
                    result = shipped()["tools_result"]
                    tool = result["tools"][tool_index]
                    if schema == "absent":
                        del tool["inputSchema"]
                    else:
                        tool["inputSchema"] = schema
                    self.assert_rejected(1, envelope(1, result), "TOOL_SCHEMA_INVALID")

    def test_missing_or_incompatible_zero_argument_contracts_fail(self):
        variants = [
            {}, {"type": "array", "properties": {}, "additionalProperties": False},
            {"type": ["object"], "properties": {}, "additionalProperties": False},
            {"type": "object", "properties": {}, "additionalProperties": True},
            {"type": "object", "properties": {}, "additionalProperties": 0},
            {"type": "object", "properties": {}, "additionalProperties": None},
            {"type": "object", "properties": {}, "additionalProperties": {}},
            {"type": "object", "properties": {}},
            {"type": "object", "additionalProperties": False},
            {"type": "object", "properties": None, "additionalProperties": False},
            {"type": "object", "properties": [], "additionalProperties": False},
            {"type": "object", "properties": {REMOTE: {"type": "string"}},
             "additionalProperties": False},
            {"type": "object", "properties": {}, "additionalProperties": False,
             "required": [REMOTE]},
        ]
        for tool_index in range(5):
            for schema in variants:
                with self.subTest(tool=tool_index, schema=schema):
                    result = shipped()["tools_result"]
                    result["tools"][tool_index]["inputSchema"] = schema
                    self.assert_rejected(1, envelope(1, result), "TOOL_SCHEMA_INVALID")

    def test_schema_failure_is_fixed_not_remote_field_or_bearer_text(self):
        failures = []
        for remote in (REMOTE, "OTHER-remote-property", BEARER):
            result = shipped()["tools_result"]
            result["tools"][0]["inputSchema"]["properties"] = {remote: {"type": "string"}}
            artifact = self.assert_rejected(1, envelope(1, result), "TOOL_SCHEMA_INVALID")
            failures.append(artifact["failures"])
        self.assertEqual(failures[0], failures[1])
        self.assertEqual(failures[0], failures[2])

    def test_exact_unique_names_remain_required_with_valid_schemas(self):
        for change in ("missing", "duplicate", "extra", "renamed"):
            with self.subTest(change=change):
                result = shipped()["tools_result"]
                tools = result["tools"]
                if change == "missing":
                    tools.pop()
                elif change == "duplicate":
                    tools[4] = tools[0]
                elif change == "extra":
                    tools.append(dict(tools[0], name=REMOTE))
                else:
                    tools[4]["name"] = REMOTE
                receipt = run_proof({1: envelope(1, result)})
                artifact = self.assert_receipt(receipt, 1)
                self.assertEqual(len(artifact["failures"]), 1)
                self.assertTrue(artifact["failures"][0].startswith("tools/list:"))
                self.assertNotIn(REMOTE, artifact["failures"][0])

    def test_shipped_schemas_order_and_harmless_annotations_pass(self):
        result = shipped()["tools_result"]
        result["tools"].reverse()
        for tool in result["tools"]:
            tool["inputSchema"].update({"title": "Read only", "required": []})
        self.assert_receipt(run_proof({1: envelope(1, result)}), 0)


class PreservedNegotiationControls(NegotiationHelpers):
    def test_shipped_normal_empty_and_compatible_typed_blocks_pass(self):
        for content in ([], [{"type": "text"}], [{"type": "future-block", "opaque": 1}]):
            with self.subTest(content=content):
                overrides = {}
                for index in range(2, 7):
                    result = {"content": content, "isError": None}
                    if index == 6:
                        result["structuredContent"] = {
                            "available": False, "rows": [], "truncated": False}
                    overrides[index] = envelope(index, result)
                artifact = self.assert_receipt(run_proof(overrides), 0)
                self.assertEqual(artifact["expected_tools"], [
                    "get_audit", "get_backfill_progress", "get_config",
                    "get_gateway", "get_status"])

    def test_corroborated_fence_does_not_override_bad_negotiation(self):
        overrides = {i: json.dumps({"jsonrpc": "2.0", "id": i + 1,
                     "error": {"code": -32000, "message": "container request failed"}})
                     for i in (2, 3, 4, 6)}
        self.assert_receipt(run_proof(overrides, fenced=True), 0)
        overrides[0] = envelope(0, {"protocolVersion": True})
        artifact = self.assert_receipt(run_proof(overrides, fenced=True), 1)
        self.assertEqual(len(artifact["failures"]), 1)
        self.assertIn("[PROTOCOL_UNSUPPORTED]", artifact["failures"][0])

    def test_unicode_display_fallback_does_not_reject_valid_negotiation(self):
        result = shipped()["initialize_result"]
        result["pad"] = "🧪" * 15000
        body = json.dumps({"jsonrpc": "2.0", "id": 1, "result": result},
                          ensure_ascii=False, separators=(",", ":"))
        self.assertLessEqual(len(body.encode()), 64 * 1024)
        artifact = self.assert_receipt(run_proof({0: body}), 0)
        self.assertEqual(set(artifact["probes"][0]["body"]), {"_text"})


if __name__ == "__main__":
    unittest.main()
