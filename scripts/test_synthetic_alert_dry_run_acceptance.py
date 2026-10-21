#!/usr/bin/env python3
"""Acceptance tests: preview synthetic-alert delivery with --dry-run and bound alert text.

Protected acceptance file: the implementation change must satisfy these
tests without editing them. Test defects are corrected here, not by
weakening assertions in the build.

Contract under test (``scripts/synthetic_alert.py`` validates the webhook
shape only inside ``post()`` immediately before sending, and ``summarize()``
flows evidence check names/shapes into Discord content with only
``isinstance(str)`` plus length caps):

* ``python3 scripts/synthetic_alert.py --env ENV --evidence FILE --dry-run``
  exits 0 and prints a stable plan to stdout naming the environment, the
  evidence receipt, the decided event and the webhook validity, without
  sending anything.
* The plan is stable: two invocations print byte-identical stdout.
* Zero mutations: no ``post()``, no opener/network call of any kind, even
  when history reports a live alert transition.
* The plan reflects webhook shape before any send: the valid-webhook plan
  differs from the invalid-webhook plan, while neither echoes the URL.
* A missing webhook or a missing probe sample still exits 0 (a preview is
  not a delivery).
* Crafted evidence control characters render inert: built alert content is
  printable-only and stays within the content cap.
* Well-formed runs pass through byte-identically (controls).

The webhook history read is mocked (it is a network read); every network
entry point refuses. No sockets, no subprocess, no Discord.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_synthetic_alert_dry_run_acceptance.py' -v
"""

import contextlib
import importlib
import io
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
alert = importlib.import_module("synthetic_alert")

# Obviously fake webhook shape; never a real credential.
HOOK = "https://discord.com/api/webhooks/234567890123456789/dry-run-plan-token-not-a-secret"
TOKEN_FRAGMENT = "dry-run-plan-token"

EVIDENCE = {
    "origin": "https://two-bot-next-staging.example.workers.dev",
    "utc": "2026-10-16T00:00:00Z",
    "checks": [
        {"check": "health", "verdict": "pass", "shape": "health 200"},
        {"check": "readyz", "verdict": "fail", "shape": "readyz 503: parked"},
        {"check": "build", "verdict": "pass", "shape": "build_revision abc build_id 1"},
    ],
    "build_revision": "abc123",
    "build_id": "build-1",
    "result": "fail",
}

EXPECTED_WELLFORMED_CONTENT = (
    "two-bot-next ALERT synthetic-user-flow:staging: "
    "readyz failed: readyz 503: parked. "
    "Runbook: https://github.com/TogetherWeOwn/two-bot-next"
    "/blob/main/docs/runbook-observability.md | "
    "build abc123 build-1 (git deadbeef) | "
    "logs https://github.com/TogetherWeOwn/two-bot-next/actions/runs/999"
)

EXPECTED_MISSING_EVIDENCE_CONTENT = (
    "two-bot-next ALERT synthetic-user-flow:staging: "
    "probe failed with no evidence receipt. "
    "Runbook: https://github.com/TogetherWeOwn/two-bot-next"
    "/blob/main/docs/runbook-observability.md | "
    "build unknown unknown (git unknown) | logs link"
)

EXPECTED_RECOVERY_CONTENT = (
    "two-bot-next RECOVERY synthetic-user-flow:prod: "
    "the probe is healthy again. "
    "Runbook: https://github.com/TogetherWeOwn/two-bot-next"
    "/blob/main/docs/runbook-observability.md | "
    "build unknown unknown (git unknown) | logs link"
)


def run_alert(argv, env):
    """Call alert.main() under a hermetic env; return (code, stdout, stderr)."""
    stdout, stderr = io.StringIO(), io.StringIO()
    with (
        patch.dict(os.environ, env, clear=True),
        contextlib.redirect_stdout(stdout),
        contextlib.redirect_stderr(stderr),
    ):
        try:
            code = alert.main(argv)
        except SystemExit as exited:
            code = exited.code if exited.code is not None else 0
    if code in (None, True):
        return 0, stdout.getvalue(), stderr.getvalue()
    return code, stdout.getvalue(), stderr.getvalue()


@contextlib.contextmanager
def no_network(history=()):
    """Mock the history read; refuse every network entry point."""
    with (
        patch.object(alert, "previous_probe_outcomes", return_value=list(history)),
        patch.object(alert, "post", side_effect=AssertionError("post() during --dry-run")),
        patch.object(alert.urllib.request, "build_opener",
                     side_effect=AssertionError("network opener during --dry-run")),
        patch.object(alert.urllib.request, "urlopen",
                     side_effect=AssertionError("urlopen during --dry-run")),
    ):
        yield


def base_env(**over):
    env = {
        "SYNTHETIC_ALERT_WEBHOOK_URL": HOOK,
        "RUN_ID": "999",
        "GITHUB_RUN_ID": "999",
        "PROBE_OUTCOME": "failure",
        "FORCE_FAIL": "true",
        "GIT_SHA": "deadbeef",
        "GITHUB_SERVER_URL": "https://github.com",
        "GITHUB_REPOSITORY": "TogetherWeOwn/two-bot-next",
    }
    env.update(over)
    return env


class DryRunPlanTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.evidence = str(Path(tmp.name) / "evidence.json")
        Path(self.evidence).write_text(json.dumps(EVIDENCE))

    def argv(self):
        return ["--env", "staging", "--evidence", self.evidence, "--dry-run"]

    def test_dry_run_alert_decision_exits_zero_with_stable_plan_and_zero_network(self):
        # Second consecutive failure: the decide step must compute a live
        # "alert" transition while the dry run sends nothing.
        with no_network(history=[("failure", "success")]):
            code1, out1, err1 = run_alert(self.argv(), base_env())
            code2, out2, _ = run_alert(self.argv(), base_env())
        self.assertEqual(code1, 0, f"--dry-run must exit 0; got {code1}: {err1[-300:]!r}")
        self.assertEqual(code2, 0, f"second --dry-run must exit 0; got {code2}")
        self.assertNotEqual(out1.strip(), "", "plan must print something to stdout")
        self.assertEqual(out1, out2, "dry-run plan must be byte-identical across runs")
        lowered = out1.lower()
        for token in ("staging", "evidence", "event", "webhook", "dry"):
            self.assertIn(token, lowered, f"plan must name {token!r}")
        self.assertIn("alert", lowered, "plan must name the decided alert event")
        self.assertNotIn(HOOK, out1, "plan must never echo the webhook URL")
        self.assertNotIn(TOKEN_FRAGMENT, out1, "plan must never echo webhook token material")

    def test_dry_run_without_webhook_still_exits_zero(self):
        env = base_env()
        del env["SYNTHETIC_ALERT_WEBHOOK_URL"]
        with no_network(history=[("failure", "success")]):
            code, out, err = run_alert(self.argv(), env)
        self.assertEqual(
            code, 0,
            f"a preview is not a delivery; a missing webhook must not fail it, "
            f"got {code}: {err[-300:]!r}",
        )
        self.assertIn("staging", out.lower(), "plan must still name the environment")

    def test_dry_run_plan_reflects_webhook_shape_before_any_send(self):
        with no_network(history=[("failure", "success")]):
            code_valid, plan_valid, err_valid = run_alert(self.argv(), base_env())
            code_invalid, plan_invalid, err_invalid = run_alert(
                self.argv(), base_env(SYNTHETIC_ALERT_WEBHOOK_URL="https://evil.invalid/hook/1"))
        self.assertEqual(code_valid, 0, f"valid-webhook plan exited {code_valid}: {err_valid[-200:]!r}")
        self.assertEqual(code_invalid, 0, f"invalid-webhook plan exited {code_invalid}: {err_invalid[-200:]!r}")
        self.assertNotEqual(
            plan_valid, plan_invalid,
            "the plan must surface webhook validity before any send, "
            "not only inside post()",
        )
        self.assertNotIn(HOOK, plan_valid, "plan must never echo the webhook URL")
        self.assertNotIn(TOKEN_FRAGMENT, plan_valid, "plan must never echo token material")

    def test_dry_run_without_probe_sample_exits_zero(self):
        env = base_env()
        del env["PROBE_OUTCOME"]
        with no_network():
            code, out, err = run_alert(self.argv(), env)
        self.assertEqual(code, 0, f"no-sample dry-run must exit 0, got {code}: {err[-300:]!r}")
        self.assertIn("staging", out.lower(), "plan must still name the environment")


class CraftedEvidenceTests(unittest.TestCase):
    def test_control_characters_in_check_names_render_inert(self):
        evil = dict(EVIDENCE, checks=[
            {"check": "readyz\x00\x07\x1b[2J\x7f", "verdict": "fail", "shape": "readyz 503"},
        ])
        content = alert.build_content("staging", evil, "deadbeef", "https://example.invalid/logs/1")
        self.assertTrue(
            all(char.isprintable() for char in content),
            f"alert content must be printable-only, got {content!r}",
        )
        for control in ("\x00", "\x07", "\x1b", "\x7f"):
            self.assertNotIn(control, content)

    def test_control_characters_in_first_shape_render_inert(self):
        evil = dict(EVIDENCE, checks=[
            {"check": "readyz", "verdict": "fail", "shape": "x\x1b[2Jy\x00\r\n\tz"},
        ])
        content = alert.build_content("staging", evil, "deadbeef", "https://example.invalid/logs/1")
        self.assertTrue(
            all(char.isprintable() for char in content),
            f"alert content must be printable-only, got {content!r}",
        )
        for control in ("\x00", "\x1b", "\r", "\n", "\t"):
            self.assertNotIn(control, content)

    def test_crafted_evidence_stays_within_content_cap(self):
        evil = dict(EVIDENCE, checks=[
            {"check": "readyz\x00" + "A" * 500, "verdict": "fail",
             "shape": "B\x07" * 250},
        ])
        content = alert.build_content("staging", evil, "deadbeef", "https://example.invalid/logs/1")
        self.assertLessEqual(len(content), alert.MAX_CONTENT)
        self.assertTrue(
            all(char.isprintable() for char in content),
            f"bounded alert content must still be printable-only, got {content[:120]!r}...",
        )


class WellformedControls(unittest.TestCase):
    """Well-formed runs pass through byte-identically."""

    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.evidence = str(Path(tmp.name) / "evidence.json")
        Path(self.evidence).write_text(json.dumps(EVIDENCE))

    def test_wellformed_alert_content_is_byte_identical(self):
        content = alert.build_content(
            "staging", EVIDENCE, "deadbeef",
            "https://github.com/TogetherWeOwn/two-bot-next/actions/runs/999")
        self.assertEqual(content, EXPECTED_WELLFORMED_CONTENT)

    def test_missing_evidence_content_is_byte_identical(self):
        self.assertEqual(alert.build_content("staging", None, None, "link"),
                         EXPECTED_MISSING_EVIDENCE_CONTENT)

    def test_recovery_content_is_byte_identical(self):
        self.assertEqual(alert.build_content("prod", None, None, "link", event="recovery"),
                         EXPECTED_RECOVERY_CONTENT)

    def test_summarize_edge_cases_unchanged(self):
        self.assertEqual(alert.summarize(None), "probe failed with no evidence receipt")
        self.assertEqual(alert.summarize({}), "probe failed")
        self.assertEqual(alert.summarize({"checks": []}), "probe failed")
        self.assertEqual(alert.summarize({"checks": [{"check": "a", "verdict": "pass"}]}),
                         "probe failed")
        self.assertEqual(alert.summarize({"checks": [{"check": 123, "verdict": "fail"}]}),
                         "probe failed")
        self.assertEqual(alert.summarize(EVIDENCE), "readyz failed: readyz 503: parked")

    def test_unprintable_revision_fields_still_read_unknown(self):
        content = alert.build_content("staging", EVIDENCE, "dead\x00beef", "link")
        self.assertIn("build abc123 build-1 (git unknown)", content)
        evil = dict(EVIDENCE, build_revision="r\x07", build_id="b\x1b")
        content = alert.build_content("staging", evil, "deadbeef", "link")
        self.assertIn("build unknown unknown (git deadbeef)", content)

    def test_decide_and_post_flags_still_exclusive(self):
        code, _, _ = run_alert(["--env", "staging", "--evidence", self.evidence,
                                "--decide-only", "--post-only"], base_env())
        self.assertEqual(code, 2, "--decide-only and --post-only must stay exclusive")

    def test_decide_only_without_sample_stays_green_with_empty_event(self):
        env = base_env()
        del env["PROBE_OUTCOME"]
        del env["SYNTHETIC_ALERT_WEBHOOK_URL"]
        with no_network():
            code, out, _ = run_alert(["--env", "staging", "--evidence", self.evidence,
                                      "--decide-only"], env)
        self.assertEqual(code, 0, "decide-only with no sample must stay green")
        self.assertIn("event=", out, "decide-only must still write the event line")

    def test_post_still_refuses_non_discord_without_network(self):
        with patch.object(alert.urllib.request, "build_opener",
                          side_effect=AssertionError("network during refusal")):
            for bad in ("", None, "https://evil.invalid/hook/1",
                        "http://discord.com/api/webhooks/1/x"):
                with self.assertRaises(alert.AlertError, msg=f"webhook {bad!r} must refuse"):
                    alert.post(bad, "content")


if __name__ == "__main__":
    unittest.main()
