"""Protected acceptance: every checkpoint binds a structured receipt.

Offline only (stdlib, fake fetches, no network). No implementation source
or diff is used to derive expectations. The combined fix must retain this
file byte-identically; the builder must make these tests pass without
editing it.

Contract: every recorded markdown checkpoint row names the validated
production origin, the checkpoint label, and the expected/observed
revision/build identity inside the single observation cell (one line, six
pipes, existing GO/EXTEND phrases and same-run matching preserved; never
an echo of unvalidated input). A --format {markdown,json} mode (default
markdown) adds a machine receipt: exactly one JSON object with the stable
keys checkpoint, verdict, reason, reason_detail, origin, expected,
observed and next_action. Reasons are stable snake_case ids from the
pinned table; origin/observed are null when nothing was validated. The
watch guide documents the JSON mode with an example.
"""

import contextlib
import io
import json
import re
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import cutover_watch_checkpoint as checkpoint

SHA = "0123456789abcdef0123456789abcdef01234567"
OTHER_SHA = "f" * 40
BUILD_ID = "700-1"
STALE_BUILD = "999-1"
PRODUCTION = "https://two-bot-next-production.example.workers.dev"
STAGING = "https://two-bot-next-staging.example.workers.dev"

RECEIPT_KEYS = {"checkpoint", "verdict", "reason", "reason_detail",
                "origin", "expected", "observed", "next_action"}

ROOT = Path(__file__).resolve().parents[1]


def readyz_body(*components, revision=SHA, build_id=BUILD_ID):
    return json.dumps({"components": [list(c) for c in components], "jobs": {},
                       "build_revision": revision, "build_id": build_id}).encode()


READY = (200, readyz_body(("process", "ready"), ("gateway", "ready"),
                          ("database", "ready"), ("token_invalid", "ready")))
PARKED = (503, readyz_body(("process", "ready"), ("gateway", "starting"),
                           ("database", "ready"), ("token_invalid", "ready")))
DB_DOWN = (503, readyz_body(("process", "ready"), ("gateway", "ready"),
                            ("database", "down"), ("token_invalid", "ready")))
UNSTAMPED = (200, readyz_body(("process", "ready"), ("gateway", "ready"),
                              ("database", "ready"), ("token_invalid", "ready"),
                              revision="unknown", build_id="unknown"))


def argv(*extra, sha=SHA, build_id=BUILD_ID, url=PRODUCTION, checkpoint_label="+15m"):
    return ["--checkpoint", checkpoint_label, "--expected-sha", sha,
            "--expected-build-id", build_id, "--production-url", url, *extra]


def section_text(doc_name, heading):
    text = (ROOT / "docs" / doc_name).read_text()
    start = text.index(heading)
    try:
        end = text.index("\n## ", start + len(heading))
    except ValueError:
        end = len(text)
    return text[start:end]


class MarkdownBindingTests(unittest.TestCase):
    def run_markdown(self, response, *extra, **kwargs):
        requested = []

        def fake(url):
            requested.append(url)
            if isinstance(response, Exception):
                raise response
            return response

        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = checkpoint.main(argv(*extra, **kwargs), fetch_fn=fake)
        return code, out.getvalue().strip(), requested

    def assert_one_row(self, row, verdict):
        self.assertNotIn("\n", row)
        self.assertEqual(row.count("|"), 6, row)
        self.assertTrue(row.endswith(f"| {verdict} |"), row)

    def test_go_row_binds_origin_and_identity(self):
        code, row, requested = self.run_markdown(READY)
        self.assertEqual(code, 0, row)
        self.assert_one_row(row, "GO")
        self.assertIn("| +15m checkpoint |", row)
        self.assertIn("revision matches the SHA", row)
        self.assertIn(PRODUCTION, row)
        self.assertIn(SHA, row)
        self.assertIn(BUILD_ID, row)
        self.assertEqual(requested, [PRODUCTION + "/readyz"])

    def test_revision_mismatch_binds_expected_and_observed(self):
        body = readyz_body(("process", "ready"), ("gateway", "ready"),
                           ("database", "ready"), ("token_invalid", "ready"),
                           revision=OTHER_SHA)
        code, row, requested = self.run_markdown((200, body))
        self.assertEqual(code, 1, row)
        self.assert_one_row(row, "EXTEND")
        self.assertIn("does not match the deployed SHA", row)
        self.assertIn(PRODUCTION, row)
        self.assertIn(OTHER_SHA, row)
        self.assertIn(SHA, row)
        self.assertEqual(requested, [PRODUCTION + "/readyz"])

    def test_stale_build_binds_expected_and_observed(self):
        body = readyz_body(("process", "ready"), ("gateway", "ready"),
                           ("database", "ready"), ("token_invalid", "ready"),
                           build_id=STALE_BUILD)
        code, row, _requested = self.run_markdown((200, body))
        self.assertEqual(code, 1, row)
        self.assert_one_row(row, "EXTEND")
        self.assertIn("is not this run's build", row)
        self.assertIn(PRODUCTION, row)
        self.assertIn(STALE_BUILD, row)
        self.assertIn(BUILD_ID, row)

    def test_component_fault_binds_origin_and_identity(self):
        code, row, _requested = self.run_markdown(DB_DOWN)
        self.assertEqual(code, 1, row)
        self.assert_one_row(row, "EXTEND")
        self.assertIn("database down", row)
        self.assertIn(PRODUCTION, row)
        self.assertIn(SHA, row)
        self.assertIn(BUILD_ID, row)

    def test_fetch_failure_binds_origin_and_expected_identity(self):
        code, row, requested = self.run_markdown(TimeoutError("timed out"))
        self.assertEqual(code, 1, row)
        self.assert_one_row(row, "EXTEND")
        self.assertIn("did not respond (TimeoutError)", row)
        self.assertIn(PRODUCTION, row)
        self.assertIn(SHA, row)
        self.assertIn(BUILD_ID, row)
        self.assertEqual(requested, [PRODUCTION + "/readyz"])

    def test_hostile_fetch_errors_leave_a_clean_row(self):
        """Positive control: only the exception class name reaches the row."""
        marker = "sg%s%s%d" % ("ni", "p", id(object()))
        evil = OSError("conn\n| " + marker + " https://evil.example/x?k=v")
        code, row, _requested = self.run_markdown(evil)
        self.assertEqual(code, 1, row)
        self.assert_one_row(row, "EXTEND")
        self.assertIn("did not respond (OSError)", row)
        self.assertNotIn(marker, row)
        self.assertNotIn("evil.example", row)

    def test_duplicate_keys_stay_extend_with_bound_origin(self):
        """Existing duplicate-key control, now bound to the validated origin."""
        marker = "sg%s%s%d" % ("ni", "p", id(object()))
        comps = json.dumps([[name, "ready"] for name in
                            ("process", "gateway", "database", "token_invalid")])
        raw = ('{"components": %s, "build_revision": "%s", "build_id": "%s", '
               '"build_revision": "%s"}') % (comps, SHA, BUILD_ID, marker)
        code, row, requested = self.run_markdown((200, raw.encode()))
        self.assertEqual(code, 1, row)
        self.assert_one_row(row, "EXTEND")
        self.assertIn("carries duplicate JSON object keys", row)
        self.assertIn(PRODUCTION, row)
        self.assertNotIn(marker, row)
        self.assertEqual(requested, [PRODUCTION + "/readyz"])


class JsonModeTests(unittest.TestCase):
    maxDiff = 4000

    def run_json(self, response, *extra, **kwargs):
        requested = []

        def fake(url):
            requested.append(url)
            if isinstance(response, Exception):
                raise response
            return response

        out = io.StringIO()
        err = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                code = checkpoint.main(argv("--format", "json", *extra, **kwargs),
                                       fetch_fn=fake)
            except SystemExit as exit_:
                self.fail("--format json must be accepted "
                          f"(exited {exit_.code})")
        return code, out.getvalue().strip(), requested

    def parse_single_receipt(self, text):
        self.assertNotIn("\n", text, "the JSON receipt is exactly one line")
        try:
            receipt = json.loads(text)
        except ValueError:
            self.fail(f"stdout must be one JSON object, got: {text[:200]!r}")
        self.assertIsInstance(receipt, dict)
        self.assertEqual(set(receipt), RECEIPT_KEYS)
        return receipt

    def assert_receipt(self, receipt, *, checkpoint_label, verdict, reason,
                       origin, expected_sha=SHA, expected_build=BUILD_ID, observed):
        self.assertEqual(receipt["checkpoint"], checkpoint_label)
        self.assertEqual(receipt["verdict"], verdict)
        self.assertEqual(receipt["reason"], reason)
        self.assertTrue(re.fullmatch(r"[a-z][a-z_]*", reason or ""),
                        f"reason must be a stable snake_case id, got {reason!r}")
        self.assertIsInstance(receipt["reason_detail"], str)
        self.assertTrue(receipt["reason_detail"].strip(), "reason_detail names the outcome")
        self.assertNotIn("\n", receipt["reason_detail"])
        self.assertEqual(receipt["origin"], origin)
        self.assertEqual(receipt["expected"],
                         {"build_revision": expected_sha, "build_id": expected_build})
        self.assertEqual(receipt["observed"], observed)
        self.assertIsInstance(receipt["next_action"], str)
        self.assertTrue(receipt["next_action"].strip(), "next_action tells the operator what to do")
        self.assertNotIn("\n", receipt["next_action"])

    def observed(self, revision=SHA, build_id=BUILD_ID):
        return {"build_revision": revision, "build_id": build_id}

    def test_json_go_carries_stable_ids(self):
        code, text, requested = self.run_json(READY)
        self.assertEqual(code, 0, text)
        receipt = self.parse_single_receipt(text)
        self.assert_receipt(receipt, checkpoint_label="+15m", verdict="GO",
                            reason="ready_match", origin=PRODUCTION,
                            observed=self.observed())
        self.assertEqual(requested, [PRODUCTION + "/readyz"])

    def test_json_go_names_its_checkpoint_label(self):
        code, text, _requested = self.run_json(READY, checkpoint_label="+48h")
        self.assertEqual(code, 0, text)
        receipt = self.parse_single_receipt(text)
        self.assert_receipt(receipt, checkpoint_label="+48h", verdict="GO",
                            reason="ready_match", origin=PRODUCTION,
                            observed=self.observed())

    def test_json_extend_reasons(self):
        mismatch_body = readyz_body(("process", "ready"), ("gateway", "ready"),
                                    ("database", "ready"), ("token_invalid", "ready"),
                                    revision=OTHER_SHA)
        stale_body = readyz_body(("process", "ready"), ("gateway", "ready"),
                                 ("database", "ready"), ("token_invalid", "ready"),
                                 build_id=STALE_BUILD)
        cases = [
            ("revision_mismatch", (200, mismatch_body),
             self.observed(revision=OTHER_SHA)),
            ("build_mismatch", (200, stale_body),
             self.observed(build_id=STALE_BUILD)),
            ("components_not_ready", DB_DOWN, self.observed()),
            ("components_not_ready", PARKED, self.observed()),
            ("fetch_failed", TimeoutError("timed out"), None),
            ("duplicate_keys", (200, b'{"components": [], "build_revision": "%s", '
                                     b'"build_id": "%s", "build_revision": "%s"}'
                                % (SHA.encode(), BUILD_ID.encode(), SHA.encode())), None),
            ("identity_unstamped", UNSTAMPED, None),
            ("readyz_unexpected", (503, b"<html>not the bot</html>"), None),
            ("readyz_unexpected", (503, b'{"error":"ownership_fenced"}'), None),
        ]
        for reason, response, observed in cases:
            with self.subTest(reason=reason):
                code, text, requested = self.run_json(response)
                self.assertEqual(code, 1, text)
                receipt = self.parse_single_receipt(text)
                self.assert_receipt(receipt, checkpoint_label="+15m", verdict="EXTEND",
                                    reason=reason, origin=PRODUCTION, observed=observed)
                self.assertEqual(requested, [PRODUCTION + "/readyz"])

    def test_json_malformed_identity_observes_nothing(self):
        """Unvalidated remote identity never echoes: observed null, marker absent."""
        marker = "sg%s%s%d" % ("ni", "p", id(object()))
        body = readyz_body(("process", "ready"), ("gateway", "ready"),
                           ("database", "ready"), ("token_invalid", "ready"),
                           revision="ab|cd\nef" + marker)
        code, text, _requested = self.run_json((200, body))
        self.assertEqual(code, 1, text)
        receipt = self.parse_single_receipt(text)
        self.assert_receipt(receipt, checkpoint_label="+15m", verdict="EXTEND",
                            reason="identity_shape_unexpected", origin=PRODUCTION,
                            observed=None)
        self.assertNotIn(marker, text)

    def test_json_hostile_fetch_error_observes_nothing(self):
        marker = "sg%s%s%d" % ("ni", "p", id(object()))
        evil = OSError("conn\n| " + marker + " https://evil.example/x?k=v")
        code, text, requested = self.run_json(evil)
        self.assertEqual(code, 1, text)
        receipt = self.parse_single_receipt(text)
        self.assert_receipt(receipt, checkpoint_label="+15m", verdict="EXTEND",
                            reason="fetch_failed", origin=PRODUCTION, observed=None)
        self.assertIn("OSError", receipt["reason_detail"])
        self.assertNotIn(marker, text)
        self.assertNotIn("evil.example", text)
        self.assertEqual(requested, [PRODUCTION + "/readyz"])

    def test_json_origin_refusal_carries_code_and_null_origin(self):
        for url, reason in ((STAGING, "staging_refused"),
                            ("https://example.com", "origin_not_admitted"),
                            ("", "missing_origin")):
            with self.subTest(url=url):
                code, text, requested = self.run_json(READY, url=url)
                self.assertEqual(code, 1, text)
                receipt = self.parse_single_receipt(text)
                self.assert_receipt(receipt, checkpoint_label="+15m", verdict="EXTEND",
                                    reason=reason, origin=None, observed=None)
                self.assertEqual(requested, [])
                if url:
                    self.assertNotIn(url, text)

    def test_default_output_stays_markdown(self):
        """Positive control: the default caller contract is unchanged markdown."""
        requested = []

        def fake(url):
            requested.append(url)
            return READY

        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = checkpoint.main(argv(), fetch_fn=fake)
        row = out.getvalue().strip()
        self.assertEqual(code, 0, row)
        self.assertNotIn("\n", row)
        self.assertEqual(row.count("|"), 6, row)
        self.assertIn("| +15m checkpoint |", row)
        self.assertTrue(row.endswith("| GO |"), row)
        self.assertEqual(requested, [PRODUCTION + "/readyz"])

    def test_explicit_markdown_matches_default(self):
        requested = []

        def fake(url):
            requested.append(url)
            return READY

        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            try:
                code = checkpoint.main(argv("--format", "markdown"), fetch_fn=fake)
            except SystemExit as exit_:
                self.fail("--format markdown must be accepted "
                          f"(exited {exit_.code})")
        row = out.getvalue().strip()
        self.assertEqual(code, 0, row)
        self.assertTrue(row.endswith("| GO |"), row)
        self.assertEqual(requested, [PRODUCTION + "/readyz"])

    def test_unknown_format_refused_before_any_request(self):
        """Positive control: usage errors exit 2 with zero fetches."""
        requested = []

        def fake(url):
            requested.append(url)
            return READY

        out = io.StringIO()
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as exit_:
                with contextlib.redirect_stdout(out):
                    checkpoint.main(argv("--format", "yaml"), fetch_fn=fake)
        self.assertEqual(exit_.exception.code, 2)
        self.assertEqual(requested, [])


class WatchGuideDocTests(unittest.TestCase):
    def test_production_deploy_watch_log_documents_json_mode(self):
        section = section_text("production-deploy.md", "## 48-hour watch log")
        self.assertIn("--format json", section,
                      "the watch log documents the JSON receipt mode")
        fences = re.findall(r"```json(.*?)```", section, re.DOTALL)
        self.assertTrue(any('"verdict"' in fence for fence in fences),
                        "the watch log shows a JSON receipt example")

    def test_cutover_sequence_watch_handoff_mentions_json_mode(self):
        section = section_text("cutover-sequence.md", "## 5. Watch handoff")
        self.assertIn("--format", section,
                      "the watch handoff names the --format mode flag")


if __name__ == "__main__":
    unittest.main()
