"""Protected offline duplicate-key refusal for the synthetic evidence reader.

Failing-first acceptance for `synthetic_alert.load_evidence`: bare
`json.load` parses duplicate object keys last-wins, so a corrupted evidence
receipt can mask a failing check name in a Discord alert. A refused receipt
must exit nonzero without posting; a missing or malformed (non-duplicate)
receipt keeps the existing unknown-build post. Offline only: the webhook
post is mocked, no network, no credentials.
"""

import importlib
import io
import json
import os
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
alert = importlib.import_module("synthetic_alert")

HOOK = "https://discord.com/api/webhooks/123456789012345678/synthetic-token-not-a-secret"
EXPECTED_STDERR = "synthetic_alert: evidence refused: duplicate JSON object key"
# Accepted seam shapes at load_evidence: the wrapped AlertError, or the raw
# hook ValueError when the builder converts it in post_event instead.
EXPECTED_ALERT = "evidence refused: duplicate JSON object key"
EXPECTED_RAW = "duplicate JSON object key"
SENTINEL = "hostile-dupkey-evidence-sentinel"

VALID = {
    "origin": "https://two-bot-next-staging.example.workers.dev",
    "utc": "2026-10-16T00:00:00Z",
    "checks": [
        {"check": "health", "verdict": "pass", "shape": "health 200"},
        {"check": "readyz", "verdict": "fail", "shape": "readyz 503: parked"},
        {"check": "build", "verdict": "pass", "shape": "build ok"},
    ],
    "build_revision": "abc123",
    "build_id": "build-1",
    "result": "fail",
}


def duplicate_bodies():
    conflicting = ('{"origin": "https://example.invalid", "utc": "2026-10-16T00:00:00Z",'
                   ' "checks": [{"check": "health", "verdict": "pass"},'
                   ' {"check": "readyz", "verdict": "fail", "verdict": "pass",'
                   ' "shape": "readyz 503: parked"}],'
                   ' "build_revision": "abc123", "build_id": "build-1", "result": "fail"}')
    reversed_order = conflicting.replace('"verdict": "fail", "verdict": "pass"',
                                         '"verdict": "pass", "verdict": "fail"')
    identical = ('{"origin": "https://example.invalid", "utc": "2026-10-16T00:00:00Z",'
                 ' "checks": [{"check": "readyz", "verdict": "fail"}],'
                 ' "build_revision": "abc123", "build_id": "build-1",'
                 ' "result": "fail", "result": "fail"}')
    nested = ('{"origin": "https://example.invalid", "utc": "2026-10-16T00:00:00Z",'
              ' "checks": [{"check": "readyz", "verdict": "fail",'
              ' "shape": {"text": "one", "text": "two"}}],'
              ' "build_revision": "abc123", "build_id": "build-1", "result": "fail"}')
    hostile = ('{"origin": "https://example.invalid", "utc": "2026-10-16T00:00:00Z",'
               f' "checks": [{{"check": "{SENTINEL}", "verdict": "fail",'
               f' "verdict": "{SENTINEL}", "shape": "{SENTINEL}"}}],'
               f' "{SENTINEL}": "{SENTINEL}", "{SENTINEL}": "{SENTINEL}",'
               ' "build_revision": "abc123", "build_id": "build-1", "result": "fail"}')
    return {"conflicting": conflicting, "reversed": reversed_order,
            "identical": identical, "nested": nested, "hostile": hostile}


class EvidenceDuplicateKeyAcceptanceTests(unittest.TestCase):
    def setUp(self):
        guard = mock.patch.object(alert.urllib.request, "build_opener",
                                  side_effect=AssertionError(
                                      "offline acceptance must not post"))
        guard.start()
        self.addCleanup(guard.stop)
        tmp = tempfile.TemporaryDirectory(
            dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR")
            or os.environ.get("PAPERCLIP_SCRATCH_DIR"))
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        self.counter = 0

    def write_body(self, body):
        self.counter += 1
        path = self.dir / f"evidence-{self.counter}.json"
        path.write_text(body, encoding="utf-8")
        return str(path)

    def base_env(self):
        return {"SYNTHETIC_ALERT_WEBHOOK_URL": HOOK, "RUN_ID": "999",
                "GITHUB_RUN_ID": "999", "GITHUB_TOKEN": "test-token",
                "PROBE_OUTCOME": "failure", "FORCE_FAIL": "false",
                "GIT_SHA": "deadbeef", "GITHUB_SERVER_URL": "https://github.com",
                "GITHUB_REPOSITORY": "TogetherWeOwn/two-bot-next"}

    def expect_load_refusal(self, body):
        path = self.write_body(body)
        try:
            result = alert.load_evidence(path)
        except alert.AlertError as error:
            self.assertEqual(str(error), EXPECTED_ALERT)
            return
        except ValueError as error:
            self.assertEqual(str(error), EXPECTED_RAW)
            return
        except Exception as error:
            self.fail(f"expected a fixed-vocabulary refusal, got {type(error).__name__}")
        if result is None:
            self.fail("duplicate keys must raise, not read as missing evidence (None)")
        self.fail(f"duplicate keys must raise, not parse last-wins: {result!r}")

    def run_post_event(self, body):
        path = self.write_body(body)
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err), \
                mock.patch.dict(os.environ, self.base_env(), clear=True), \
                mock.patch.object(alert, "post") as post:
            code = alert.post_event(self.base_env(), "staging", path, "alert")
        return code, post, out.getvalue(), err.getvalue()

    def test_load_evidence_refuses_conflicting_verdicts_both_orders(self):
        bodies = duplicate_bodies()
        for family in ("conflicting", "reversed"):
            with self.subTest(family=family):
                self.expect_load_refusal(bodies[family])

    def test_load_evidence_refuses_identical_duplicate_values(self):
        self.expect_load_refusal(duplicate_bodies()["identical"])

    def test_load_evidence_refuses_nested_duplicate_keys(self):
        self.expect_load_refusal(duplicate_bodies()["nested"])

    def test_load_evidence_refusal_echoes_no_hostile_bytes(self):
        self.expect_load_refusal(duplicate_bodies()["hostile"])

    def test_post_event_refuses_duplicate_evidence_without_posting(self):
        for family, body in duplicate_bodies().items():
            with self.subTest(family=family):
                code, post, out, err = self.run_post_event(body)
                self.assertEqual(code, 1)
                post.assert_not_called()
                self.assertEqual(err.strip(), EXPECTED_STDERR)
                self.assertNotIn(SENTINEL, out)
                self.assertNotIn(SENTINEL, err)

    def test_main_refuses_duplicate_evidence_without_posting(self):
        for family, body in duplicate_bodies().items():
            with self.subTest(family=family):
                path = self.write_body(body)
                out, err = io.StringIO(), io.StringIO()
                with redirect_stdout(out), redirect_stderr(err), \
                        mock.patch.dict(os.environ, self.base_env(), clear=True), \
                        mock.patch.object(alert, "previous_probe_outcomes",
                                          return_value=[("failure", "success")]), \
                        mock.patch.object(alert, "post") as post:
                    code = alert.main(["--env", "staging", "--evidence", path])
                self.assertEqual(code, 1)
                post.assert_not_called()
                self.assertEqual(err.getvalue().strip(), EXPECTED_STDERR)
                self.assertNotIn(SENTINEL, out.getvalue())
                self.assertNotIn(SENTINEL, err.getvalue())

    def test_main_post_only_refuses_duplicate_evidence(self):
        for family in ("conflicting", "hostile"):
            with self.subTest(family=family):
                path = self.write_body(duplicate_bodies()[family])
                out, err = io.StringIO(), io.StringIO()
                env = {**self.base_env(), "ALERT_EVENT": "alert"}
                with redirect_stdout(out), redirect_stderr(err), \
                        mock.patch.dict(os.environ, env, clear=True), \
                        mock.patch.object(alert, "post") as post:
                    code = alert.main(["--env", "staging", "--evidence", path,
                                       "--post-only"])
                self.assertEqual(code, 1)
                post.assert_not_called()
                self.assertEqual(err.getvalue().strip(), EXPECTED_STDERR)

    def test_missing_and_malformed_evidence_keep_unknown_build_posts(self):
        # Controls: only duplicate keys refuse. Missing or malformed
        # receipts keep the existing unknown-build alert.
        for path in (str(self.dir / "absent.json"),
                     self.write_body("{not valid json")):
            with self.subTest(path=path):
                self.assertIsNone(alert.load_evidence(path))
                out, err = io.StringIO(), io.StringIO()
                with redirect_stdout(out), redirect_stderr(err), \
                        mock.patch.dict(os.environ, self.base_env(), clear=True), \
                        mock.patch.object(alert, "post") as post:
                    code = alert.post_event(self.base_env(), "staging", path, "alert")
                self.assertEqual(code, 0)
                post.assert_called_once()
                content = post.call_args.args[1]
                self.assertIn("unknown", content)
                self.assertEqual(out.getvalue(), content + "\n")

    def test_valid_evidence_posts_byte_identical_alert(self):
        path = self.write_body(json.dumps(VALID))
        self.assertEqual(alert.load_evidence(path), VALID)
        self.assertEqual(alert.load_evidence(path), json.loads(Path(path).read_text()))
        posted = []
        for _ in range(2):
            out, err = io.StringIO(), io.StringIO()
            with redirect_stdout(out), redirect_stderr(err), \
                    mock.patch.dict(os.environ, self.base_env(), clear=True), \
                    mock.patch.object(alert, "post") as post:
                code = alert.post_event(self.base_env(), "staging", path, "alert")
            self.assertEqual(code, 0)
            content = post.call_args.args[1]
            posted.append(content)
            self.assertEqual(out.getvalue(), content + "\n")
        self.assertEqual(posted[0], posted[1])
        self.assertIn("readyz", posted[0])
        self.assertIn("build abc123 build-1 (git deadbeef)", posted[0])


if __name__ == "__main__":
    unittest.main()
