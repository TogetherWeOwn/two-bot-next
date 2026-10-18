"""Acceptance fixtures: the passive probe validates build identity shape.

Offline only (stdlib, fake fetches, no network). The canonical identity
contract is the one the deploy gates already use (scripts/production_deploy.py
and docs/production-deploy.md, "build identity"):

  build_revision  a 40-character lowercase hex commit, or the documented
                  unstamped marker "unknown" (crates/bot/src/server.rs:
                  option_env!("BOT_BUILD_REVISION").unwrap_or("unknown")).
  build_id        "<run id>-<attempt>", digits-digits with each part at most
                  20 digits, or the same unstamped marker "unknown".
                  The unstamped representation is the pair: both fields
                  "unknown". One stamped and one unstamped field is not a
                  documented form.

Anything else a readiness response carries in those fields is remote,
untrusted text: the build check must FAIL with a fixed diagnostic and the
value must never reach stdout, stderr, a Result reason, or the JSON receipt.
The independent readyz health classification must not change.
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
probe = importlib.import_module("prod_passive_probe")

PROD = "https://two-bot-next.example-sub.workers.dev"
SHA = "0123456789abcdef0123456789abcdef01234567"
BUILD_ID = "37512552064-1"

# Assembled at runtime so no credential-shaped literal sits in the source.
CREDENTIAL_URL = "".join(
    ("https://", "fixture-user", ":", "synthetic-fixture-secret",
     "@evil.example", "/collect?x=1"))
CREDENTIAL_FRAGMENT = "synthetic-fixture-secret"


def components(*pairs):
    return [[name, status] for name, status in pairs]


def body(value, status=200):
    return status, {}, json.dumps(value).encode()


def ready(revision=SHA, build_id=BUILD_ID):
    return {
        "components": components(("process", "ready"), ("gateway", "ready"),
                                 ("database", "ready"), ("token_invalid", "ready")),
        "jobs": {},
        "build_revision": revision,
        "build_id": build_id,
    }


class BuildIdentityAcceptanceTests(unittest.TestCase):
    def setUp(self):
        # Any real HTTP is a test failure: the probe only gets fake fetches here.
        guard = mock.patch.object(probe.urllib.request.OpenerDirector, "open",
                                  side_effect=AssertionError("network access in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)
        self.requested = []
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.evidence = str(Path(tmp.name) / "evidence.json")

    def fetch(self, routes):
        def fake(url):
            self.requested.append(url)
            if url not in routes:
                raise AssertionError(f"unexpected offline fixture route: {url}")
            return routes[url]
        return fake

    def drive(self, report, status=200):
        """Run the CLI against one ready report; capture stdout, stderr, receipt."""
        routes = {PROD + "/health": body({"status": "ok"}),
                  PROD + "/readyz": body(report, status=status)}
        out, err = io.StringIO(), io.StringIO()
        argv = ["--prod-url", PROD, "--evidence", self.evidence]
        with mock.patch.dict(os.environ, {}, clear=True), \
                redirect_stdout(out), redirect_stderr(err):
            code = probe.main(argv, fetch_fn=self.fetch(routes))
        receipt_text = Path(self.evidence).read_text()
        return code, out.getvalue(), err.getvalue(), receipt_text, json.loads(receipt_text)

    def verdicts(self, receipt):
        return {check["check"]: check["verdict"] for check in receipt["checks"]}

    def assert_rejected_without_echo(self, report, *sentinels, status=200):
        code, out, err, receipt_text, receipt = self.drive(report, status=status)
        self.assertEqual(code, 1)
        self.assertIn("PASS health", out)
        self.assertIn("FAIL build", out)
        self.assertEqual(self.verdicts(receipt)["build"], "fail")
        self.assertEqual(receipt["result"], "fail")
        for sentinel in sentinels:
            self.assertNotIn(sentinel, out)
            self.assertNotIn(sentinel, err)
            self.assertNotIn(sentinel, receipt_text)
        # The probe stays passive: exactly the two documented GET routes.
        self.assertEqual(self.requested, [PROD + "/health", PROD + "/readyz"])
        return out, receipt

    # -- controls: canonical identities keep passing -------------------------

    def test_valid_sha_and_run_attempt_identity_passes(self):
        code, out, err, _, receipt = self.drive(ready())
        self.assertEqual(code, 0)
        self.assertIn("PASS build", out)
        self.assertEqual(err, "")
        self.assertEqual(receipt["build_revision"], SHA)
        self.assertEqual(receipt["build_id"], BUILD_ID)
        self.assertEqual(self.verdicts(receipt),
                         {"health": "pass", "readyz": "pass", "build": "pass"})

    def test_build_id_parts_at_the_twenty_digit_bound_pass(self):
        boundary = "1" * 20 + "-" + "9" * 20
        code, out, _, _, receipt = self.drive(ready(build_id=boundary))
        self.assertEqual(code, 0)
        self.assertIn("PASS build", out)
        self.assertEqual(receipt["build_id"], boundary)

    def test_documented_unstamped_pair_is_preserved(self):
        code, out, _, _, receipt = self.drive(ready("unknown", "unknown"))
        self.assertEqual(code, 0)
        self.assertIn("PASS build", out)
        self.assertEqual(receipt["build_revision"], "unknown")
        self.assertEqual(receipt["build_id"], "unknown")

    # -- malformed identity: build fails, readyz classification unchanged ----

    def test_arbitrary_printable_revision_fails_build_without_echo(self):
        sentinel = "noncanonical-printable-sentinel"
        out, receipt = self.assert_rejected_without_echo(ready(sentinel), sentinel)
        self.assertIn("PASS readyz", out)
        self.assertEqual(self.verdicts(receipt)["readyz"], "pass")
        self.assertIsNone(receipt.get("build_revision"))

    def test_arbitrary_printable_build_id_fails_build_without_echo(self):
        sentinel = "build-sentinel-value"
        _, receipt = self.assert_rejected_without_echo(
            ready(build_id=sentinel), sentinel)
        self.assertIsNone(receipt.get("build_id"))

    def test_credential_shaped_url_revision_never_reaches_evidence(self):
        self.assert_rejected_without_echo(
            ready(CREDENTIAL_URL), CREDENTIAL_URL, CREDENTIAL_FRAGMENT)

    def test_credential_shaped_url_build_id_never_reaches_evidence(self):
        self.assert_rejected_without_echo(
            ready(build_id=CREDENTIAL_URL), CREDENTIAL_URL, CREDENTIAL_FRAGMENT)

    def test_oversized_printable_revision_fails_build_without_echo(self):
        sentinel = "a" * 5000
        self.assert_rejected_without_echo(ready(sentinel), sentinel)

    def test_oversized_printable_build_id_fails_build_without_echo(self):
        sentinel = "9" * 5000 + "-1"
        self.assert_rejected_without_echo(ready(build_id=sentinel), sentinel)

    def test_malformed_revision_shapes_fail_build_without_echo(self):
        for sentinel in ("0123456789abcdef0123456789abcdef0123456",   # 39 hex
                         "0123456789abcdef0123456789abcdef012345678",  # 41 hex
                         "0123456789ABCDEF0123456789ABCDEF01234567",   # uppercase
                         "ghij" * 10,                                   # 40 non-hex
                         "abc123"):                                     # short printable
            with self.subTest(revision=sentinel):
                self.requested = []
                self.assert_rejected_without_echo(ready(sentinel), sentinel)

    def test_malformed_build_id_shapes_fail_build_without_echo(self):
        for sentinel in ("1" * 21 + "-1",   # run part over the 20-digit bound
                         "1-" + "9" * 21,   # attempt part over the bound
                         "run-1", "1-2-3", "1_2", "12345"):
            with self.subTest(build_id=sentinel):
                self.requested = []
                self.assert_rejected_without_echo(ready(build_id=sentinel), sentinel)

    def test_mixed_unstamped_and_stamped_identity_fails_build(self):
        for revision, build_id in (("unknown", BUILD_ID), (SHA, "unknown")):
            with self.subTest(revision=revision, build_id=build_id):
                self.requested = []
                code, out, _, _, receipt = self.drive(ready(revision, build_id))
                self.assertEqual(code, 1)
                self.assertIn("PASS readyz", out)
                self.assertIn("FAIL build", out)
                self.assertEqual(self.verdicts(receipt)["build"], "fail")

    def test_malformed_identity_keeps_parked_readyz_classification(self):
        sentinel = "noncanonical-printable-sentinel"
        parked = ready(sentinel)
        parked["components"] = components(("process", "ready"), ("gateway", "starting"),
                                          ("database", "ready"), ("token_invalid", "ready"))
        code, out, err, receipt_text, receipt = self.drive(parked, status=503)
        self.assertEqual(code, 1)
        self.assertIn("FAIL readyz", out)
        self.assertIn("truthful, not a pass", out)
        self.assertIn("FAIL build", out)
        self.assertNotIn(sentinel, out)
        self.assertNotIn(sentinel, err)
        self.assertNotIn(sentinel, receipt_text)
        self.assertEqual(self.verdicts(receipt)["readyz"], "fail")

    # -- seams below the CLI: reasons and the receipt shape itself -----------

    def test_check_build_reason_never_carries_a_malformed_value(self):
        for sentinel in ("noncanonical-printable-sentinel", CREDENTIAL_URL, "b" * 500):
            with self.subTest(sentinel=sentinel[:24]):
                result = probe.check_build(ready(sentinel, sentinel))
                self.assertFalse(result.ok)
                self.assertNotIn(sentinel, result.reason)
                self.assertNotIn(CREDENTIAL_FRAGMENT, result.reason)

    def test_evidence_shape_never_serializes_a_malformed_value(self):
        results = [probe.Result("health", True, "health 200 with the exact liveness shape"),
                   probe.Result("readyz", True, "readyz 200: every component ready"),
                   probe.Result("build", False, "fixed diagnostic")]
        for sentinel in ("noncanonical-printable-sentinel", CREDENTIAL_URL, "c" * 500):
            with self.subTest(sentinel=sentinel[:24]):
                receipt = probe.evidence_shape(results, ready(sentinel, sentinel))
                self.assertNotIn(sentinel, json.dumps(receipt))
                self.assertNotIn(CREDENTIAL_FRAGMENT, json.dumps(receipt))
                self.assertIsNone(receipt.get("build_revision"))
                self.assertIsNone(receipt.get("build_id"))


if __name__ == "__main__":
    unittest.main()
