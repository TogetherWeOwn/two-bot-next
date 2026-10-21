"""Protected acceptance: rollback drill error envelopes refuse duplicate keys.

Offline only (stdlib, stubbed transports, no network). No implementation
source or diff is used to derive expectations. The combined fix must retain
this file byte-identically; the builder must make these tests pass without
editing it.

Contract (baseline 95e8145ad): DrillClient.post parses Cloudflare error
envelopes with bare json.loads, so a repeated key last-wins before
integer-code extraction into drill verdicts. Fixed code uses
object_pairs_hook=_reject_duplicate_keys (precedent:
rollback_readiness_probe.decode_strict) and refuses with the fixed shape
"status=<code> codes=none" (the existing malformed-envelope shape),
grading zero codes and echoing no raw key, value, envelope text or header.
Valid envelopes keep byte-identical code verdicts.
"""

import io
import json
import sys
import unittest
from pathlib import Path
from unittest.mock import MagicMock
from urllib.error import HTTPError

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_staging_rollback_drill as fixtures

drill = fixtures.drill
GateError = fixtures.GateError
ACCOUNT = fixtures.ACCOUNT
POST_PATH = "/workers/scripts/two-bot-next-staging/deployments"

SENTINEL = "SENTINEL DUP ROLLBACK ENV 7c1d9f"


def client_failing_with(body_bytes, status=409):
    client = drill.DrillClient(ACCOUNT, fixtures.SENTINEL)
    error = HTTPError("https://api.cloudflare.com", status, "Conflict", {},
                      io.BytesIO(body_bytes))
    client.opener = MagicMock()
    client.opener.open.side_effect = error
    return client


class RollbackEnvelopeDuplicateKeyAcceptanceTests(unittest.TestCase):
    def assert_refused(self, body_bytes, status=409):
        self.assertIn(SENTINEL, body_bytes.decode("utf-8", "replace"),
                      "each duplicate fixture must carry the sentinel for the no-echo pin")
        parsed = json.loads(body_bytes)
        self.assertIsInstance(parsed, dict)
        client = client_failing_with(body_bytes, status)
        with self.assertRaises(GateError) as caught:
            client.post(POST_PATH, {})
        self.assertEqual(str(caught.exception), "api_http_failure")
        self.assertEqual(caught.exception.detail, f"status={status} codes=none")
        self.assertNotIn(SENTINEL, repr(caught.exception.detail))
        self.assertNotIn("SENTINEL DUP", repr(caught.exception.detail))

    def assert_codes(self, body_bytes, expected, status=409):
        details = []
        for _ in range(2):
            client = client_failing_with(body_bytes, status)
            with self.assertRaises(GateError) as caught:
                client.post(POST_PATH, {})
            self.assertEqual(str(caught.exception), "api_http_failure")
            details.append(caught.exception.detail)
        self.assertEqual(details[0], f"status={status} codes={expected}")
        self.assertEqual(details[0], details[1])
        self.assertNotIn(fixtures.SENTINEL, repr(details[0]))

    # -- controls: valid envelopes keep byte-identical code verdicts ----------

    def test_valid_single_code_is_byte_identical(self):
        body = json.dumps({"errors": [{"code": 10220,
                                       "message": fixtures.SENTINEL}]}).encode()
        self.assert_codes(body, "10220")

    def test_valid_two_codes_are_byte_identical(self):
        body = json.dumps({"errors": [{"code": 10220, "message": "a"},
                                      {"code": 10000, "message": "b"}]}).encode()
        self.assert_codes(body, "10220,10000")

    def test_malformed_envelope_yields_codes_none(self):
        body = b"{not json"
        client = client_failing_with(body)
        with self.assertRaises(GateError) as caught:
            client.post(POST_PATH, {})
        self.assertEqual(caught.exception.detail, "status=409 codes=none")

    # -- top-level duplicates: conflicting both orders, identical -------------

    def test_top_level_errors_conflicting_both_orders_refused(self):
        for first, second in ((1111, 2222), (2222, 1111)):
            with self.subTest(first=first, second=second):
                body = ('{"errors": [{"code": %d, "message": "%s"}], '
                        '"errors": [{"code": %d, "message": "ok"}]}') % (
                            first, SENTINEL, second)
                body = body.encode()
                survivor = json.loads(body)["errors"][0]["code"]
                self.assertEqual(survivor, second)
                self.assert_refused(body)
                client = client_failing_with(body)
                with self.assertRaises(GateError) as caught:
                    client.post(POST_PATH, {})
                self.assertNotIn(str(first), caught.exception.detail)
                self.assertNotIn(str(second), caught.exception.detail)

    def test_top_level_errors_identical_values_refused(self):
        body = ('{"errors": [{"code": 10220, "message": "%s"}], '
                '"errors": [{"code": 10220, "message": "ok"}]}') % SENTINEL
        body = body.encode()
        self.assertEqual(json.loads(body)["errors"][0]["code"], 10220)
        self.assert_refused(body)

    # -- nested duplicates: code/message at any depth -------------------------

    def test_nested_code_conflicting_both_orders_refused(self):
        for first, second in ((1111, 2222), (2222, 1111)):
            with self.subTest(first=first, second=second):
                body = ('{"errors": [{"code": %d, "code": %d, '
                        '"message": "%s"}]}') % (first, second, SENTINEL)
                body = body.encode()
                self.assertEqual(json.loads(body)["errors"][0]["code"], second)
                self.assert_refused(body)

    def test_nested_code_identical_values_refused(self):
        body = ('{"errors": [{"code": 10220, "code": 10220, '
                '"message": "%s"}]}') % SENTINEL
        body = body.encode()
        self.assertEqual(json.loads(body)["errors"][0]["code"], 10220)
        self.assert_refused(body)

    def test_nested_message_duplicate_refused_despite_stable_code(self):
        body = ('{"errors": [{"code": 10220, "message": "%s", '
                '"message": "ok"}]}') % SENTINEL
        body = body.encode()
        self.assertEqual(json.loads(body)["errors"][0]["code"], 10220)
        self.assert_refused(body)

    def test_second_error_item_duplicate_refused(self):
        body = ('{"errors": [{"code": 10220, "message": "ok"}, '
                '{"code": 1111, "code": 2222, "message": "%s"}]}') % SENTINEL
        body = body.encode()
        self.assertEqual(json.loads(body)["errors"][1]["code"], 2222)
        self.assert_refused(body)

    # -- end-to-end: refused envelopes grade zero codes -----------------------

    def test_refused_envelope_grades_zero_codes_byte_identical(self):
        body = ('{"errors": [{"code": 1111, "message": "%s"}], '
                '"errors": [{"code": 2222, "message": "ok"}]}') % SENTINEL
        body = body.encode()
        details = []
        for _ in range(2):
            client = client_failing_with(body)
            with self.assertRaises(GateError) as caught:
                client.post(POST_PATH, {})
            details.append(caught.exception.detail)
        self.assertEqual(details, ["status=409 codes=none"] * 2)


if __name__ == "__main__":
    unittest.main()
