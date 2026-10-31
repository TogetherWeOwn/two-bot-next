"""Protected acceptance: rollout Cloudflare reads refuse duplicate keys.

Offline only (stdlib, stubbed transports, no network). No implementation
source or diff is used to derive expectations. The combined fix must retain
this file byte-identically; the builder must make these tests pass without
editing it.

Contract (baseline 95e8145ad): staging_rollout.decode uses bare json.loads,
so a repeated key last-wins before application-identity and rollout-read
grading. Fixed code uses object_pairs_hook=_reject_duplicate_keys (precedent:
rollback_readiness_probe.decode_strict, qa_cutover_probes) and refuses with
the fixed shape token "invalid_json" (the existing malformed-body code,
consistent with the depth/recursion precedent), echoing no raw key, value,
body or header. A refused body grades nothing and proves no identity; valid
bodies keep byte-identical proofs.
"""

import json
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_staging_rollout as fixtures

rollout = fixtures.rollout
GateError = rollout.GateError
ACCOUNT = fixtures.ACCOUNT
APPLICATION_ID = fixtures.APPLICATION_ID
NAMESPACE = fixtures.NAMESPACE
APPLICATION = "two-bot-next-twobotcontainer-staging"

SENTINEL = "SENTINEL DUP ROLLOUT 9f3a7c1d"


def envelope(body_bytes):
    return body_bytes


def api_client_with(body_bytes):
    client = rollout.Client(ACCOUNT, fixtures.SENTINEL)
    patcher = patch.object(client, "request", return_value=(200, {}, body_bytes))
    patcher.start()
    return client, patcher


class RolloutDuplicateKeyAcceptanceTests(unittest.TestCase):
    def assert_refused(self, body_bytes):
        self.assertIn(SENTINEL, body_bytes.decode("utf-8", "replace"),
                      "each duplicate fixture must carry the sentinel for the no-echo pin")
        # Sanity: bare loads succeeds, proving the only defect is the duplicate.
        parsed = json.loads(body_bytes)
        self.assertIsInstance(parsed, dict)
        with self.assertRaises(GateError) as caught:
            rollout.decode(body_bytes)
        self.assertEqual(str(caught.exception), "invalid_json")
        detail = getattr(caught.exception, "detail", None)
        blob = str(caught.exception) + (str(detail) if detail else "")
        self.assertNotIn(SENTINEL, blob)
        self.assertNotIn("SENTINEL DUP", blob)

    def assert_api_refused(self, body_bytes):
        self.assertIn(SENTINEL, body_bytes.decode("utf-8", "replace"),
                      "each duplicate fixture must carry the sentinel for the no-echo pin")
        client, patcher = api_client_with(body_bytes)
        try:
            with self.assertRaises(GateError) as caught:
                client.api("/containers/applications")
            self.assertEqual(str(caught.exception), "invalid_json")
            detail = getattr(caught.exception, "detail", None)
            blob = str(caught.exception) + (str(detail) if detail else "")
            self.assertNotIn(SENTINEL, blob)
        finally:
            patcher.stop()

    # -- controls: valid bodies keep byte-identical proofs -------------------

    def test_decode_valid_envelope_is_byte_identical(self):
        body = b'{"success": true, "result": [{"id": "x"}]}'
        first = rollout.decode(body)
        second = rollout.decode(body)
        self.assertEqual(first, second)
        self.assertEqual(json.dumps(first, sort_keys=True),
                         json.dumps(second, sort_keys=True))

    def test_application_valid_body_proves_identity_byte_identical(self):
        body = json.dumps({"success": True, "result": [fixtures.app()]}).encode()
        proofs = []
        for _ in range(2):
            client, patcher = api_client_with(body)
            try:
                proofs.append(rollout.application(client))
            finally:
                patcher.stop()
        self.assertEqual(proofs[0]["id"], APPLICATION_ID)
        self.assertEqual(proofs[0]["durable_objects"]["namespace_id"], NAMESPACE)
        self.assertEqual(json.dumps(proofs[0], sort_keys=True),
                         json.dumps(proofs[1], sort_keys=True))

    def test_rollouts_valid_body_proves_read_byte_identical(self):
        body = json.dumps({"success": True, "result": [fixtures.old_row()]}).encode()
        reads = []
        for _ in range(2):
            client, patcher = api_client_with(body)
            try:
                reads.append(rollout.rollouts(client, APPLICATION_ID))
            finally:
                patcher.stop()
        self.assertEqual(len(reads[0]), 1)
        self.assertEqual(json.dumps(reads[0], sort_keys=True),
                         json.dumps(reads[1], sort_keys=True))

    def test_client_api_valid_envelope_returns_result(self):
        body = b'{"success": true, "result": {"id": "x"}}'
        client, patcher = api_client_with(body)
        try:
            self.assertEqual(client.api("/x"), {"id": "x"})
        finally:
            patcher.stop()

    # -- direct decode: top-level duplicates ---------------------------------

    def test_decode_top_level_conflicting_both_orders_refused(self):
        for first, second in (("1", "2"), ("2", "1")):
            with self.subTest(first=first, second=second):
                body = ('{"a": %s, "a": %s, "probe": "%s"}' % (
                    first, second, SENTINEL)).encode()
                # Bare loads grades the survivor.
                self.assertEqual(json.loads(body)["a"], int(second))
                self.assert_refused(body)

    def test_decode_top_level_identical_values_refused(self):
        body = ('{"a": 1, "a": 1, "probe": "%s"}' % SENTINEL).encode()
        self.assertEqual(json.loads(body)["a"], 1)
        self.assert_refused(body)

    def test_decode_top_level_success_conflicting_refused(self):
        for first, second in (("true", "false"), ("false", "true")):
            with self.subTest(first=first, second=second):
                body = ('{"success": %s, "success": %s, "result": [], '
                        '"probe": "%s"}') % (first, second, SENTINEL)
                body = body.encode()
                self.assertEqual(json.loads(body)["success"], second == "true")
                self.assert_refused(body)

    # -- direct decode: nested duplicates ------------------------------------

    def test_decode_nested_conflicting_refused(self):
        body = ('{"success": true, "result": {"name": "a", "name": "b"}, '
                '"probe": "%s"}' % SENTINEL).encode()
        self.assertEqual(json.loads(body)["result"]["name"], "b")
        self.assert_refused(body)

    def test_decode_nested_identical_refused(self):
        body = ('{"success": true, "result": {"name": "a", "name": "a"}, '
                '"probe": "%s"}' % SENTINEL).encode()
        self.assertEqual(json.loads(body)["result"]["name"], "a")
        self.assert_refused(body)

    def test_decode_deeply_nested_duplicate_refused(self):
        body = ('{"success": true, "result": [{"durable_objects": '
                '{"namespace_id": "%s", "namespace_id": "%s"}}], '
                '"probe": "%s"}') % (SENTINEL, NAMESPACE, SENTINEL)
        body = body.encode()
        survivor = json.loads(body)["result"][0]["durable_objects"]["namespace_id"]
        self.assertEqual(survivor, NAMESPACE)
        self.assert_refused(body)

    # -- identity proof: duplicate name/id/namespace refuse ------------------

    def test_application_duplicate_name_refused(self):
        cases = [
            (SENTINEL, APPLICATION, "survivor valid, current code proves identity"),
            (APPLICATION, SENTINEL, "survivor invalid, current code fails wrong reason"),
            (APPLICATION, APPLICATION, "identical valid, current code proves identity"),
        ]
        for first, second, note in cases:
            with self.subTest(note=note):
                app = ('{"id": "%s", "name": "%s", "name": "%s", '
                       '"durable_objects": {"namespace_id": "%s"}, '
                       '"comment": "%s"}') % (
                           APPLICATION_ID, first, second, NAMESPACE, SENTINEL)
                body = ('{"success": true, "result": [%s]}' % app).encode()
                # Bare-loads sanity documents the last-wins grading.
                survivor = json.loads(body)["result"][0]["name"]
                self.assertEqual(survivor, second)
                self.assert_api_refused(body)
                # And the application seam never proves from the refused body.
                client, patcher = api_client_with(body)
                try:
                    with self.assertRaises(GateError) as caught:
                        rollout.application(client)
                    self.assertEqual(str(caught.exception), "invalid_json")
                finally:
                    patcher.stop()

    def test_application_duplicate_id_refused(self):
        cases = [
            (SENTINEL, APPLICATION_ID),
            (APPLICATION_ID, SENTINEL),
            (APPLICATION_ID, APPLICATION_ID),
        ]
        for first, second in cases:
            with self.subTest(first=first[:12], second=second[:12]):
                app = ('{"id": "%s", "id": "%s", "name": "%s", '
                       '"durable_objects": {"namespace_id": "%s"}, '
                       '"comment": "%s"}') % (
                           first, second, APPLICATION, NAMESPACE, SENTINEL)
                body = ('{"success": true, "result": [%s]}' % app).encode()
                self.assertEqual(json.loads(body)["result"][0]["id"], second)
                self.assert_api_refused(body)

    def test_application_duplicate_namespace_nested_refused(self):
        for first, second in ((SENTINEL, NAMESPACE), (NAMESPACE, SENTINEL),
                              (NAMESPACE, NAMESPACE)):
            with self.subTest(first=first[:12], second=second[:12]):
                app = ('{"id": "%s", "name": "%s", "durable_objects": '
                       '{"namespace_id": "%s", "namespace_id": "%s"}, '
                       '"comment": "%s"}') % (
                           APPLICATION_ID, APPLICATION, first, second, SENTINEL)
                body = ('{"success": true, "result": [%s]}' % app).encode()
                survivor = json.loads(body)["result"][0]["durable_objects"]["namespace_id"]
                self.assertEqual(survivor, second)
                self.assert_api_refused(body)

    # -- rollout reads: duplicate row keys refuse ----------------------------

    def test_rollouts_duplicate_row_id_refused(self):
        row_id = "rollout-dup-1"
        for first, second in ((SENTINEL, row_id), (row_id, SENTINEL),
                              (row_id, row_id)):
            with self.subTest(first=first[:12], second=second[:12]):
                row = '{"id": "%s", "id": "%s", "comment": "%s"}' % (
                    first, second, SENTINEL)
                body = ('{"success": true, "result": [%s]}' % row).encode()
                self.assertEqual(json.loads(body)["result"][0]["id"], second)
                self.assert_api_refused(body)

    def test_client_api_duplicate_result_refused(self):
        bodies = [
            '{"success": true, "result": [], "result": [{"id": "x"}], "probe": "%s"}' % SENTINEL,
            '{"success": true, "result": [{"id": "x"}], "result": [], "probe": "%s"}' % SENTINEL,
            '{"success": true, "result": [], "result": [], "probe": "%s"}' % SENTINEL,
        ]
        for raw in bodies:
            with self.subTest(raw=raw[:48]):
                self.assert_api_refused(raw.encode())

    # -- end-to-end: refused bodies prove nothing ----------------------------

    def test_refused_envelope_grades_no_identity_proof(self):
        app = ('{"id": "%s", "name": "%s", "name": "%s", '
               '"durable_objects": {"namespace_id": "%s"}}') % (
                   APPLICATION_ID, SENTINEL, APPLICATION, NAMESPACE)
        body = ('{"success": true, "result": [%s]}' % app).encode()
        # Last-wins today: the valid survivor proves identity.
        bare = json.loads(body)["result"][0]
        self.assertEqual(bare["name"], APPLICATION)
        client, patcher = api_client_with(body)
        try:
            with self.assertRaises(GateError) as caught:
                rollout.application(client)
            self.assertEqual(str(caught.exception), "invalid_json")
            detail = getattr(caught.exception, "detail", None)
            blob = str(caught.exception) + (str(detail) if detail else "")
            self.assertNotIn(SENTINEL, blob)
        finally:
            patcher.stop()


if __name__ == "__main__":
    unittest.main()
