"""Cutover watch-checkpoint regressions: canned /readyz only, no network or databases."""

import contextlib
import io
import json
from pathlib import Path
import sys
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import cutover_watch_checkpoint as checkpoint  # noqa: E402
import production_deploy as deploy_gate  # noqa: E402
import rollback_readiness_probe as probe  # noqa: E402

SHA = "0123456789abcdef0123456789abcdef01234567"
OTHER_SHA = "f" * 40
BUILD_ID = "700-1"
PRODUCTION = "https://two-bot-next-production.example.workers.dev"


def readyz_body(*components, revision=SHA, build_id=BUILD_ID):
    return json.dumps({"components": [list(c) for c in components], "jobs": {},
                       "build_revision": revision, "build_id": build_id}).encode()


READY = (200, readyz_body(("process", "ready"), ("gateway", "ready"),
                          ("database", "ready"), ("token_invalid", "ready")))
PARKED = (503, readyz_body(("process", "ready"), ("gateway", "starting"),
                           ("database", "ready"), ("token_invalid", "ready")))
DB_DOWN = (503, readyz_body(("process", "ready"), ("gateway", "ready"),
                            ("database", "down"), ("token_invalid", "ready")))


def argv(*extra, sha=SHA, build_id=BUILD_ID, url=PRODUCTION, checkpoint_label="+15m"):
    return ["--checkpoint", checkpoint_label, "--expected-sha", sha,
            "--expected-build-id", build_id, "--production-url", url, *extra]


class CutoverWatchCheckpointTests(unittest.TestCase):
    def run_checkpoint(self, response, *extra, **kwargs):
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

    def test_constants_are_reused_not_duplicated(self):
        self.assertIs(checkpoint.USER_AGENT, probe.USER_AGENT)
        self.assertIs(checkpoint.READYZ_TIMEOUT_SECONDS, probe.READYZ_TIMEOUT_SECONDS)
        self.assertIs(checkpoint.READYZ_BODY_CAP, probe.READYZ_BODY_CAP)
        self.assertIs(checkpoint.READYZ_STATES, probe.READYZ_STATES)
        self.assertIs(checkpoint.READYZ_REQUIRED, probe.READYZ_REQUIRED)
        self.assertIs(checkpoint.READYZ_PARKED, probe.READYZ_PARKED)
        self.assertIs(checkpoint.SHA, deploy_gate.SHA)
        self.assertIs(checkpoint.DIGITS, deploy_gate.DIGITS)
        self.assertIs(checkpoint.UNSTAMPED, deploy_gate.UNSTAMPED)
        self.assertEqual(checkpoint.CHECKPOINTS, ("+15m", "+1h", "+6h", "+24h", "+48h"))

    def test_ready_with_matching_identity_is_go(self):
        code, row, requested = self.run_checkpoint(READY)
        self.assertEqual(code, 0)
        self.assertIn("| +15m checkpoint |", row)
        self.assertIn("`readyz` / `revision`", row)
        self.assertIn("revision matches the SHA", row)
        self.assertTrue(row.startswith("| "), row)
        self.assertTrue(row.endswith("| GO |"), row)
        self.assertEqual(requested, [PRODUCTION + "/readyz"])

    def test_every_checkpoint_label_emits_its_row(self):
        for label in ("+15m", "+1h", "+6h", "+24h", "+48h"):
            with self.subTest(label):
                code, row, _ = self.run_checkpoint(READY, checkpoint_label=label)
                self.assertEqual(code, 0)
                self.assertIn(f"| {label} checkpoint |", row)
                self.assertTrue(row.endswith("| GO |"), row)

    def test_revision_mismatch_is_extend(self):
        body = readyz_body(("process", "ready"), ("gateway", "ready"),
                           ("database", "ready"), ("token_invalid", "ready"),
                           revision=OTHER_SHA)
        code, row, _ = self.run_checkpoint((200, body))
        self.assertEqual(code, 1)
        self.assertIn("does not match the deployed SHA", row)
        self.assertTrue(row.endswith("| EXTEND |"), row)
        self.assertNotIn("\n", row)
        self.assertEqual(row.count("|"), 6, row)

    def test_malformed_remote_identity_is_fixed_extend_without_echo(self):
        # Runtime-built marker: it cannot match a literal in the script, so its
        # absence proves the remote value never reached the row.
        marker = "sg%s%s%d" % ("ni", "p", id(object()))
        evils = [
            {"revision": "ab|cd\nef" + marker, "build_id": BUILD_ID},
            {"revision": "line1\nline2|" + marker, "build_id": BUILD_ID},
            {"revision": "ghp_" + marker + "A" * 32, "build_id": BUILD_ID},
            {"revision": "github_pat_" + marker, "build_id": BUILD_ID},
            {"revision": SHA, "build_id": "1|2\n" + marker},
            {"revision": SHA, "build_id": "xoxb-" + marker},
            {"revision": "A" * 36 + marker[:4], "build_id": BUILD_ID},
            {"revision": SHA[:39] + "|", "build_id": BUILD_ID},
            {"revision": SHA, "build_id": "run-1"},
        ]
        for evil in evils:
            with self.subTest(revision=evil["revision"][:12], build_id=evil["build_id"][:12]):
                body = readyz_body(("process", "ready"), ("gateway", "ready"),
                                   ("database", "ready"), ("token_invalid", "ready"),
                                   **evil)
                code, row, _ = self.run_checkpoint((200, body))
                self.assertEqual(code, 1, row)
                self.assertTrue(row.endswith("| EXTEND |"), row)
                self.assertIn("unexpected shape", row)
                # One intact row: no injected columns or line breaks, no echo.
                self.assertNotIn("\n", row)
                self.assertEqual(row.count("|"), 6, row)
                self.assertNotIn(marker, row)

    def test_stale_build_of_the_same_sha_is_extend(self):
        body = readyz_body(("process", "ready"), ("gateway", "ready"),
                           ("database", "ready"), ("token_invalid", "ready"),
                           build_id="999-1")
        code, row, _ = self.run_checkpoint((200, body))
        self.assertEqual(code, 1)
        self.assertIn("is not this run's build", row)
        self.assertTrue(row.endswith("| EXTEND |"), row)
        self.assertNotIn("\n", row)
        self.assertEqual(row.count("|"), 6, row)

    def test_unstamped_build_is_extend(self):
        body = readyz_body(("process", "ready"), ("gateway", "ready"),
                           ("database", "ready"), ("token_invalid", "ready"),
                           revision="unknown", build_id="unknown")
        code, row, _ = self.run_checkpoint((200, body))
        self.assertEqual(code, 1)
        self.assertIn("was not stamped", row)
        self.assertTrue(row.endswith("| EXTEND |"), row)

    def test_http_failure_is_extend(self):
        code, row, requested = self.run_checkpoint(TimeoutError("timed out"))
        self.assertEqual(code, 1)
        self.assertIn("did not respond (TimeoutError)", row)
        self.assertTrue(row.endswith("| EXTEND |"), row)
        self.assertEqual(requested, [PRODUCTION + "/readyz"])

    def test_parked_503_with_matching_identity_is_extend_not_go(self):
        code, row, _ = self.run_checkpoint(PARKED)
        self.assertEqual(code, 1)
        self.assertIn("parked, not acceptance", row)
        self.assertIn("revision matches the SHA", row)
        self.assertTrue(row.endswith("| EXTEND |"), row)

    def test_database_fault_is_extend(self):
        code, row, _ = self.run_checkpoint(DB_DOWN)
        self.assertEqual(code, 1)
        self.assertIn("database down", row)
        self.assertTrue(row.endswith("| EXTEND |"), row)

    def test_non_json_and_refusal_bodies_are_extend(self):
        for status, body in ((503, b"<html>not the bot</html>"),
                             (503, b'{"error":"ownership_fenced"}')):
            with self.subTest(status=status, body=body[:20]):
                code, row, _ = self.run_checkpoint((status, body))
                self.assertEqual(code, 1, row)
                self.assertTrue(row.endswith("| EXTEND |"), row)

    def test_int_digit_overflow_is_extend_not_a_traceback(self):
        body = b'{"components":[],"n":' + b"1" * 4301 + b"}"
        code, row, requested = self.run_checkpoint((200, body))
        self.assertEqual(code, 1)
        self.assertIn("without the bot's component breakdown", row)
        self.assertTrue(row.endswith("| EXTEND |"), row)
        self.assertNotIn("\n", row)
        self.assertEqual(row.count("|"), 6, row)
        self.assertEqual(requested, [PRODUCTION + "/readyz"])

    def test_refuses_non_production_origins_without_a_request(self):
        for url in ("http://two-bot-next-production.example.workers.dev",
                    "https://two-bot-next-production.example.workers.dev:8443",
                    "https://user@two-bot-next-production.example.workers.dev",
                    "https://two-bot-next-production.example.workers.dev/readyz",
                    "https://two-bot-next-production.example.workers.dev/?x=1",
                    ""):
            with self.subTest(url=url):
                requested = []

                def fake(endpoint):
                    requested.append(endpoint)
                    return READY

                out = io.StringIO()
                with contextlib.redirect_stdout(out):
                    code = checkpoint.main(argv(url=url), fetch_fn=fake)
                self.assertEqual(code, 1)
                self.assertIn("EXTEND", out.getvalue())
                self.assertEqual(requested, [])

    def test_rejects_bad_sha_and_build_id_before_any_request(self):
        for label, kwargs in (("bad sha", {"sha": "main"}),
                              ("short sha", {"sha": SHA[:39]}),
                              ("bad build", {"build_id": "run-1"}),
                              ("bare build", {"build_id": "700"})):
            with self.subTest(label):
                with contextlib.redirect_stderr(io.StringIO()):
                    with self.assertRaises(SystemExit) as exit_:
                        checkpoint.main(argv(**kwargs), fetch_fn=lambda url: READY)
                self.assertEqual(exit_.exception.code, 2)

    def test_rejects_unknown_checkpoint_before_any_request(self):
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as exit_:
                checkpoint.main(argv(checkpoint_label="+2h"), fetch_fn=lambda url: READY)
        self.assertEqual(exit_.exception.code, 2)

    def test_production_url_defaults_to_the_environment(self):
        requested = []

        def fake(url):
            requested.append(url)
            return READY

        out = io.StringIO()
        args = ["--checkpoint", "+1h", "--expected-sha", SHA,
                "--expected-build-id", BUILD_ID]
        with mock.patch.dict("os.environ", {"PRODUCTION_WORKER_URL": PRODUCTION + "/"},
                             clear=True), contextlib.redirect_stdout(out):
            code = checkpoint.main(args, fetch_fn=fake)
        self.assertEqual(code, 0, out.getvalue())
        self.assertEqual(requested, [PRODUCTION + "/readyz"])
        self.assertIn("| +1h checkpoint |", out.getvalue())


class DuplicateJsonKeysTests(unittest.TestCase):
    """Duplicate /readyz object keys refuse before any identity grading.

    Plain json.loads keeps the last duplicate, so a repeated
    build_revision could mask a stale container as this run's build.
    Every object at any depth is strict: identical repeats and both
    conflicting orders fail closed with fixed vocabulary.
    """

    COMPONENTS = [["process", "ready"], ["gateway", "ready"],
                  ["database", "ready"], ["token_invalid", "ready"]]

    def test_top_level_build_revision_duplicate_refused(self):
        comps = json.dumps(self.COMPONENTS)
        bodies = [
            # Conflicting: last-wins would read OTHER_SHA.
            '{"components": %s, "build_revision": "%s", "build_id": "%s", "build_revision": "%s"}'
            % (comps, SHA, BUILD_ID, OTHER_SHA),
            # Reversed: last-wins would read SHA and pass.
            '{"build_revision": "%s", "components": %s, "build_revision": "%s", "build_id": "%s"}'
            % (OTHER_SHA, comps, SHA, BUILD_ID),
            # Identical repeat still refuses.
            '{"components": %s, "build_revision": "%s", "build_id": "%s", "build_revision": "%s"}'
            % (comps, SHA, BUILD_ID, SHA),
        ]
        for raw in bodies:
            with self.subTest(raw=raw[:64]):
                with self.assertRaises(checkpoint.CheckpointError) as caught:
                    checkpoint.check_build_identity(raw.encode(), SHA, BUILD_ID)
                self.assertEqual(str(caught.exception),
                                 "/readyz carries duplicate JSON object keys")

    def test_nested_jobs_duplicate_refused(self):
        comps = json.dumps(self.COMPONENTS)
        raw = ('{"components": %s, "jobs": {"counter": {"parked": true, "parked": false}}, '
               '"build_revision": "%s", "build_id": "%s"}') % (comps, SHA, BUILD_ID)
        with self.assertRaises(checkpoint.CheckpointError) as caught:
            checkpoint.check_build_identity(raw.encode(), SHA, BUILD_ID)
        self.assertEqual(str(caught.exception),
                         "/readyz carries duplicate JSON object keys")

    def test_escaped_key_spelling_counts_as_duplicate(self):
        comps = json.dumps(self.COMPONENTS)
        raw = ('{"components": %s, "jobs": {"a": 1, "\\u0061": 2}, '
               '"build_revision": "%s", "build_id": "%s"}') % (comps, SHA, BUILD_ID)
        with self.assertRaises(checkpoint.CheckpointError) as caught:
            checkpoint.check_build_identity(raw.encode(), SHA, BUILD_ID)
        self.assertEqual(str(caught.exception),
                         "/readyz carries duplicate JSON object keys")

    def test_duplicate_fails_closed_end_to_end_as_extend(self):
        marker = "sg%s%s%d" % ("ni", "p", id(object()))
        comps = json.dumps(self.COMPONENTS)
        raw = ('{"components": %s, "build_revision": "%s", "build_id": "%s", '
               '"build_revision": "%s"}') % (comps, SHA, BUILD_ID, marker)
        requested = []

        def fake(url):
            requested.append(url)
            return (200, raw.encode())

        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = checkpoint.main(argv(), fetch_fn=fake)
        row = out.getvalue().strip()
        self.assertEqual(code, 1, row)
        self.assertTrue(row.endswith("| EXTEND |"), row)
        self.assertIn("carries duplicate JSON object keys", row)
        self.assertNotIn("\n", row)
        self.assertEqual(row.count("|"), 6, row)
        self.assertNotIn(marker, row)
        self.assertEqual(requested, [PRODUCTION + "/readyz"])

    def test_unique_nested_jobs_still_go(self):
        comps = json.dumps(self.COMPONENTS)
        raw = ('{"components": %s, "jobs": {"counter": {"parked": false, "running": true}}, '
               '"build_revision": "%s", "build_id": "%s"}') % (comps, SHA, BUILD_ID)
        requested = []

        def fake(url):
            requested.append(url)
            return (200, raw.encode())

        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = checkpoint.main(argv(), fetch_fn=fake)
        row = out.getvalue().strip()
        self.assertEqual(code, 0, row)
        self.assertTrue(row.endswith("| GO |"), row)
        self.assertEqual(requested, [PRODUCTION + "/readyz"])


if __name__ == "__main__":
    unittest.main()
