#!/usr/bin/env python3
"""Builder-owned pins: dry-run exits return exactly int 0.

Companion to the protected acceptance files
``test_synthetic_alert_dry_run_acceptance.py`` and
``test_staging_voice_synthetic_dry_run_acceptance.py`` (Test Author oracle):
their ``run_*`` helpers normalize ``code in (None, True)`` to 0, and since
``True == 1`` in Python, a dry-run regression returning exit 1 would still
report success. Those files are manifest-pinned and stay byte-identical;
this builder-owned module pins the RAW ``main()`` return with identity, so
an exit-1 regression (or a refusal, or a fall-through None) turns the suite
red. Each pin below is proven by an exit-1 mutant going red.

Contract pinned here:

* ``synthetic_alert --dry-run`` raw return ``is 0`` with a valid webhook,
  a missing webhook, and a missing probe sample.
* ``synthetic_alert --dry-run`` on refused evidence returns ``is 1`` with
  the live-path fixed-vocabulary refusal and no traceback.
* ``staging_voice_synthetic --dry-run`` raw return ``is 0`` with no token.
* Every network/Discord entry point refuses during the run.

Offline only: the webhook history read is mocked, every network entry
point refuses, no sockets, no subprocess, no Discord.

Run: PYTHONDONTWRITEBYTECODE=1 python3 scripts/test_dry_run_exit_code_pins.py -v
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
synth = importlib.import_module("staging_voice_synthetic")

# Obviously fake webhook shape; never a real credential.
HOOK = "https://discord.com/api/webhooks/234567890123456789/dry-run-plan-token-not-a-secret"

EVIDENCE = {
    "origin": "https://two-bot-next-staging.example.workers.dev",
    "utc": "2026-10-16T00:00:00Z",
    "checks": [
        {"check": "health", "verdict": "pass", "shape": "health 200"},
        {"check": "readyz", "verdict": "fail", "shape": "readyz 503: parked"},
    ],
    "build_revision": "abc123",
    "build_id": "build-1",
    "result": "fail",
}


def raw_alert(argv, env):
    """Call alert.main() directly; return the RAW code (no normalization).

    Unlike the protected helper, SystemExit(None) is NOT mapped to 0: any
    fall-through is a contract surprise these pins must catch.
    """
    stdout, stderr = io.StringIO(), io.StringIO()
    with (
        patch.dict(os.environ, env, clear=True),
        contextlib.redirect_stdout(stdout),
        contextlib.redirect_stderr(stderr),
    ):
        try:
            code = alert.main(argv)
        except SystemExit as exited:
            code = exited.code
    return code, stdout.getvalue(), stderr.getvalue()


def raw_synth(argv, env):
    """Call synth.main() directly; return the RAW code (no normalization)."""
    stdout, stderr = io.StringIO(), io.StringIO()
    with (
        patch.dict(os.environ, env, clear=True),
        contextlib.redirect_stdout(stdout),
        contextlib.redirect_stderr(stderr),
    ):
        try:
            code = synth.main(argv)
        except SystemExit as exited:
            code = exited.code
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


@contextlib.contextmanager
def no_discord():
    """Refuse every Discord/gateway/metrics/network entry point."""
    with (
        patch.object(synth, "rest", side_effect=AssertionError("REST GET during --dry-run")),
        patch.object(synth, "rest_patch", side_effect=AssertionError("REST PATCH during --dry-run")),
        patch.object(synth, "Session", side_effect=AssertionError("gateway session during --dry-run")),
        patch.object(synth, "WebSocket", side_effect=AssertionError("websocket during --dry-run")),
        patch.object(synth, "_open_no_redirect",
                     side_effect=AssertionError("token-bearing request during --dry-run")),
        patch("socket.create_connection", side_effect=AssertionError("socket during --dry-run")),
        patch("urllib.request.build_opener", side_effect=AssertionError("urlopener during --dry-run")),
        patch("urllib.request.urlopen", side_effect=AssertionError("urlopen during --dry-run")),
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


class AlertExitCodePins(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.evidence = str(Path(tmp.name) / "evidence.json")
        Path(self.evidence).write_text(json.dumps(EVIDENCE))

    def argv(self):
        return ["--env", "staging", "--evidence", self.evidence, "--dry-run"]

    def assertExactZero(self, code, err, what):
        # Identity, not equality: True == 1 would mask an exit-1 regression.
        self.assertIs(code, 0, f"{what} raw return must be exactly int 0; got {code!r}: {err[-300:]!r}")

    def test_dry_run_alert_decision_returns_exact_zero(self):
        # Second consecutive failure: the decide step computes a live "alert"
        # transition while the dry run sends nothing.
        with no_network(history=[("failure", "success")]):
            code, out, err = raw_alert(self.argv(), base_env())
        self.assertExactZero(code, err, "alert-transition --dry-run")
        self.assertIn("dry-run plan", out, "must print the plan to stdout")

    def test_dry_run_without_webhook_returns_exact_zero(self):
        env = base_env()
        del env["SYNTHETIC_ALERT_WEBHOOK_URL"]
        with no_network(history=[("failure", "success")]):
            code, out, err = raw_alert(self.argv(), env)
        self.assertExactZero(code, err, "missing-webhook --dry-run")
        self.assertIn("staging", out.lower(), "plan must still name the environment")

    def test_dry_run_without_probe_sample_returns_exact_zero(self):
        env = base_env()
        del env["PROBE_OUTCOME"]
        with no_network():
            code, out, err = raw_alert(self.argv(), env)
        self.assertExactZero(code, err, "no-sample --dry-run")
        self.assertIn("dry-run plan", out, "must print the plan to stdout")


class AlertDryRunRefusedEvidence(unittest.TestCase):
    """--dry-run fails closed on refused evidence, like the live post path.

    The duplicate-key refusal lives on main (load_evidence raises
    AlertError); on this base the loader never raises, so the refusal is
    injected to prove the handler. The test stays green across the merge
    either way: it pins the handler, not the loader.
    """

    def test_dry_run_refused_evidence_exits_one_without_traceback(self):
        argv = ["--env", "staging", "--evidence", "dup.json", "--dry-run"]
        with (
            no_network(history=[("failure", "success")]),
            patch.object(alert, "load_evidence", side_effect=alert.AlertError(
                "evidence refused: duplicate JSON object key")),
        ):
            code, out, err = raw_alert(argv, base_env())
        self.assertIs(code, 1,
                      f"refused evidence must exit exactly int 1; got {code!r}: {err[-300:]!r}")
        self.assertIn("synthetic_alert: evidence refused: duplicate JSON object key", err,
                      "refusal must use the live-path fixed vocabulary")
        self.assertNotIn("Traceback", out + err, "refusal must never print a traceback")
        self.assertEqual(out, "", "refusal must print no plan to stdout")


class SynthExitCodePins(unittest.TestCase):
    def test_dry_run_returns_exact_zero_without_token(self):
        with no_discord():
            code, out, err = raw_synth(["--dry-run"], {})
        self.assertIs(code, 0, f"--dry-run raw return must be exactly int 0; got {code!r}: {err[-300:]!r}")
        self.assertIn("dry-run plan", out, "must print the plan to stdout")

    def test_dry_run_with_token_returns_exact_zero_and_never_echoes_it(self):
        token = "dry-run-pin-fake-token-not-a-secret"
        with no_discord():
            code, out, err = raw_synth(["--dry-run"], {"TWO_VOICE_SYNTHETIC_BOT_TOKEN": token})
        self.assertIs(code, 0, f"--dry-run raw return must be exactly int 0; got {code!r}: {err[-300:]!r}")
        self.assertNotIn(token, out, "plan must never echo the bot token")


if __name__ == "__main__":
    unittest.main()
