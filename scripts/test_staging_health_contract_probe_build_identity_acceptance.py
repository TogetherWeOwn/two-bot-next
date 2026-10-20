"""Acceptance fixtures: the staging probe validates build identity shape.

Offline only (stdlib, fake fetches, no network). The canonical identity
contract is the one the production passive probe and the deploy gates
already use (scripts/build_identity.py, scripts/prod_passive_probe.py):

  build_revision  a 40-character lowercase hex commit, or the documented
                  unstamped marker "unknown" (crates/bot/src/server.rs:
                  option_env!("BOT_BUILD_REVISION").unwrap_or("unknown")).
  build_id        "<run id>-<attempt>", digits-digits with each part at
                  most 20 digits, or the same unstamped marker "unknown".
                  The unstamped representation is the pair: both fields
                  "unknown". One stamped and one unstamped field is not
                  a documented form.

Anything else a readiness response carries in those fields is remote,
untrusted text: the build check must FAIL with a fixed diagnostic, and
the value must never reach stdout, stderr, a Result reason, or the JSON
evidence (negative receipts carry null identity fields).

An optional expected SHA (--expected-sha / $EXPECTED_SHA) is local
input of the same shape. A malformed pin is an input refusal in the
origin-fence class: exit 2, zero network calls, no evidence written,
and the pin value is never echoed. A supplied pin can only be matched
by a canonical stamped revision: never by the unstamped pair or by a
malformed remote identity, and a stamped revision whose build_id is
malformed is not a matched deployment either.

The unstamped pair with no pin keeps passing, but only as explicitly
unstamped evidence (the reason names it unstamped and never claims a
matched deployment); its receipt keeps the fixed "unknown" pair. The
independent readyz classification (parked and checkpoint-read wording,
including the unverified-root-cause evidence) must not change.
"""

import io
import json
import os
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest import mock

import staging_health_contract_probe as probe

STAGING = "https://two-bot-next-staging.example-sub.workers.dev"
SHA = "0123456789abcdef0123456789abcdef01234567"
OTHER_SHA = "fedcba9876543210fedcba9876543210fedcba98"
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


def with_field(field, value):
    """A ready report with one JSON identity field replaced verbatim."""
    report = ready()
    report[field] = value
    return report


class StagingBuildIdentityAcceptanceTests(unittest.TestCase):
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

    def reset(self):
        self.requested = []
        Path(self.evidence).unlink(missing_ok=True)

    def drive(self, report, *extra, status=200, env=None, raw_readyz=None):
        """Run the CLI against one readyz report; capture out, err, receipt."""
        routes = {STAGING + "/health": body({"status": "ok"}),
                  STAGING + "/readyz": (raw_readyz if raw_readyz is not None
                                        else body(report, status=status))}
        out, err = io.StringIO(), io.StringIO()
        argv = ["--staging-url", STAGING, "--evidence", self.evidence, *extra]
        with mock.patch.dict(os.environ, env or {}, clear=True), \
                redirect_stdout(out), redirect_stderr(err):
            try:
                code = probe.main(argv, fetch_fn=self.fetch(routes))
            except SystemExit as exit_:
                # An argparse-style input refusal is still an exit code.
                code = exit_.code if isinstance(exit_.code, int) else 1
        receipt_text = (Path(self.evidence).read_text()
                        if Path(self.evidence).exists() else None)
        receipt = json.loads(receipt_text) if receipt_text is not None else None
        return code, out.getvalue(), err.getvalue(), receipt_text, receipt

    def verdicts(self, receipt):
        return {check["check"]: check["verdict"] for check in receipt["checks"]}

    def assert_rejected_without_echo(self, report, *sentinels, status=200, extra=()):
        code, out, err, receipt_text, receipt = self.drive(report, status=status,
                                                           *extra)
        self.assertEqual(code, 1)
        self.assertIn("PASS health", out)
        self.assertIn("FAIL build", out)
        self.assertIsNotNone(receipt, "a failed build check still writes a receipt")
        self.assertEqual(self.verdicts(receipt)["build"], "fail")
        self.assertEqual(receipt["result"], "fail")
        # Negative receipts retain no rejected identity value in either field.
        self.assertIsNone(receipt.get("build_revision"))
        self.assertIsNone(receipt.get("build_id"))
        for sentinel in sentinels:
            # Short shapes ("-1", "1_2") occur in dates and summaries by
            # chance; their FAIL + null-receipt assertions above still bind.
            if not sentinel or len(sentinel) < 8:
                continue
            self.assertNotIn(sentinel, out)
            self.assertNotIn(sentinel, err)
            self.assertNotIn(sentinel, receipt_text)
        # The probe stays read-only: exactly the two documented GET routes.
        self.assertEqual(self.requested, [STAGING + "/health", STAGING + "/readyz"])
        return out, receipt

    # -- controls: canonical identities keep their current behaviour ---------

    def test_canonical_identity_without_a_pin_passes_and_is_recorded(self):
        code, out, err, _, receipt = self.drive(ready())
        self.assertEqual(code, 0)
        self.assertIn("PASS build", out)
        self.assertEqual(err, "")
        self.assertEqual(receipt["build_revision"], SHA)
        self.assertEqual(receipt["build_id"], BUILD_ID)
        self.assertIsNone(receipt["expected_sha"])
        self.assertEqual(self.verdicts(receipt),
                         {"health": "pass", "readyz": "pass", "build": "pass"})

    def test_build_id_parts_at_the_twenty_digit_bound_pass(self):
        boundary = "1" * 20 + "-" + "9" * 20
        code, out, _, _, receipt = self.drive(ready(build_id=boundary))
        self.assertEqual(code, 0)
        self.assertIn("PASS build", out)
        self.assertEqual(receipt["build_id"], boundary)

    def test_canonical_identity_matching_the_pin_passes_as_matched(self):
        code, out, _, _, receipt = self.drive(ready(), "--expected-sha", SHA)
        self.assertEqual(code, 0)
        self.assertIn("PASS build", out)
        self.assertIn("matches the expected SHA", out)
        self.assertEqual(receipt["build_revision"], SHA)
        self.assertEqual(receipt["build_id"], BUILD_ID)
        self.assertEqual(receipt["expected_sha"], SHA)
        self.assertEqual(receipt["result"], "pass")

    def test_canonical_identity_from_the_env_pin_passes_as_matched(self):
        code, out, _, _, receipt = self.drive(ready(), env={"EXPECTED_SHA": SHA})
        self.assertEqual(code, 0)
        self.assertIn("matches the expected SHA", out)
        self.assertEqual(receipt["expected_sha"], SHA)

    def test_canonical_mismatch_fails_build_only_and_keeps_safe_identity(self):
        code, out, _, _, receipt = self.drive(ready(), "--expected-sha", OTHER_SHA)
        self.assertEqual(code, 1)
        self.assertIn("PASS health", out)
        self.assertIn("PASS readyz", out)
        self.assertIn("FAIL build", out)
        self.assertIn("does not match the expected SHA", out)
        # Both SHAs are canonical, so the receipt may keep them as evidence.
        self.assertEqual(receipt["build_revision"], SHA)
        self.assertEqual(receipt["build_id"], BUILD_ID)
        self.assertEqual(receipt["expected_sha"], OTHER_SHA)
        self.assertEqual(self.verdicts(receipt),
                         {"health": "pass", "readyz": "pass", "build": "fail"})
        self.assertEqual(receipt["result"], "fail")

    def test_parked_readyz_with_canonical_identity_is_unchanged(self):
        parked = ready()
        parked["components"] = components(("process", "ready"), ("gateway", "starting"),
                                          ("database", "ready"), ("token_invalid", "ready"))
        code, out, _, _, receipt = self.drive(parked, status=503)
        self.assertEqual(code, 1)
        self.assertIn("FAIL readyz", out)
        self.assertIn("truthful, not E2E approval", out)
        self.assertIn("PASS build", out)
        self.assertEqual(receipt["build_revision"], SHA)
        self.assertIsNone(receipt["gateway_failure_root_cause"])

    def test_checkpoint_read_failure_wording_survives_canonical_identity(self):
        failed_read = ready()
        failed_read["components"] = components(("process", "ready"), ("gateway", "starting"),
                                               ("database", "ready"), ("token_invalid", "ready"))
        failed_read["gateway_failure"] = {"phase": "durable_gateway",
                                          "class": "checkpoint_load_failed"}
        code, out, _, _, receipt = self.drive(failed_read, status=503)
        self.assertEqual(code, 1)
        self.assertIn("checkpoint read failed", out)
        self.assertIn("root cause unverified; require separate reviewed "
                      "schema/ACL/connectivity evidence", out)
        self.assertIn("PASS build", out)
        self.assertEqual(receipt["gateway_failure_class"], "checkpoint_load_failed")
        self.assertEqual(receipt["gateway_failure_root_cause"], "unverified")
        self.assertEqual(receipt["build_revision"], SHA)

    # -- the unstamped pair: permitted, explicit, never a matched pin -------

    def test_unstamped_pair_without_a_pin_passes_as_unstamped_only(self):
        code, out, _, _, receipt = self.drive(ready("unknown", "unknown"))
        self.assertEqual(code, 0)
        self.assertIn("PASS build", out)
        self.assertIn("unstamped", out.lower())
        self.assertNotIn("matches the expected SHA", out)
        self.assertEqual(receipt["build_revision"], "unknown")
        self.assertEqual(receipt["build_id"], "unknown")
        self.assertIsNone(receipt["expected_sha"])
        self.assertEqual(receipt["result"], "pass")

    def test_unstamped_pair_never_matches_a_supplied_pin(self):
        code, out, _, _, receipt = self.drive(ready("unknown", "unknown"),
                                              "--expected-sha", SHA)
        self.assertEqual(code, 1)
        self.assertIn("PASS readyz", out)
        self.assertIn("FAIL build", out)
        self.assertNotIn("matches the expected SHA", out)
        # The fixed unstamped pair is safe evidence; the pin stays unmatched.
        self.assertEqual(receipt["build_revision"], "unknown")
        self.assertEqual(receipt["build_id"], "unknown")
        self.assertEqual(receipt["expected_sha"], SHA)
        self.assertEqual(self.verdicts(receipt)["build"], "fail")
        self.assertEqual(receipt["result"], "fail")

    def test_mixed_unstamped_and_stamped_pairs_fail_without_retention(self):
        for revision, build_id in (("unknown", BUILD_ID), (SHA, "unknown")):
            with self.subTest(revision=revision, build_id=build_id):
                self.reset()
                out, _receipt = self.assert_rejected_without_echo(
                    ready(revision, build_id))
                self.assertIn("PASS readyz", out)
                self.assertNotIn("matches the expected SHA", out)

    def test_mixed_pair_with_a_matching_revision_still_fails(self):
        code, out, _, _, receipt = self.drive(ready(SHA, "unknown"),
                                              "--expected-sha", SHA)
        self.assertEqual(code, 1)
        self.assertIn("FAIL build", out)
        self.assertNotIn("PASS build", out)
        self.assertEqual(self.verdicts(receipt)["build"], "fail")
        self.assertIsNone(receipt.get("build_revision"))
        self.assertIsNone(receipt.get("build_id"))

    # -- malformed remote identity: build fails, nothing is echoed ----------

    def test_arbitrary_printable_revision_fails_build_without_echo(self):
        sentinel = "noncanonical-printable-sentinel"
        out, receipt = self.assert_rejected_without_echo(ready(sentinel), sentinel)
        self.assertIn("PASS readyz", out)
        self.assertEqual(self.verdicts(receipt)["readyz"], "pass")

    def test_arbitrary_printable_build_id_fails_build_without_echo(self):
        sentinel = "build-sentinel-value"
        self.assert_rejected_without_echo(ready(build_id=sentinel), sentinel)

    def test_credential_shaped_identity_never_reaches_output_or_evidence(self):
        for field in ("build_revision", "build_id"):
            with self.subTest(field=field):
                self.reset()
                self.assert_rejected_without_echo(
                    with_field(field, CREDENTIAL_URL),
                    CREDENTIAL_URL, CREDENTIAL_FRAGMENT)

    def test_oversized_printable_identity_fails_build_without_echo(self):
        for field, sentinel in (("build_revision", "a" * 5000),
                                ("build_id", "9" * 5000 + "-1")):
            with self.subTest(field=field):
                self.reset()
                self.assert_rejected_without_echo(with_field(field, sentinel),
                                                  sentinel)

    def test_malformed_revision_shapes_fail_build_without_echo(self):
        for sentinel in ("0123456789abcdef0123456789abcdef0123456",    # 39 hex
                         "0123456789abcdef0123456789abcdef012345678",   # 41 hex
                         "0123456789ABCDEF0123456789ABCDEF01234567",    # uppercase
                         "ghij" * 10,                                    # 40 non-hex
                         "abc123",                                       # short printable
                         " " + SHA,                                      # padded
                         SHA + " ",                                      # padded
                         ""):                                            # empty
            with self.subTest(revision=sentinel[:24]):
                self.reset()
                self.assert_rejected_without_echo(ready(sentinel), sentinel.strip())

    def test_malformed_build_id_shapes_fail_build_without_echo(self):
        for sentinel in ("1" * 21 + "-1",    # run part over the 20-digit bound
                         "1-" + "9" * 21,    # attempt part over the bound
                         "1" * 30 + "-" + "9" * 30,  # both parts over the bound
                         "run-1", "1-2-3", "1_2", "12345", "-1", "1-", "-",
                         " 37512552064-1", "37512552064-1 ", ""):
            with self.subTest(build_id=sentinel[:24]):
                self.reset()
                self.assert_rejected_without_echo(ready(build_id=sentinel),
                                                  sentinel.strip())

    def test_control_and_non_ascii_identity_fails_build_without_echo(self):
        for field, value, fragment in (
                ("build_revision", "abc\nPASS health: forged-sentinel", "forged-sentinel"),
                ("build_revision", "rev\tforged-sentinel", "forged-sentinel"),
                ("build_revision", "日本語forged-sentinel", "forged-sentinel"),
                ("build_revision", "\x1b[31mforged-sentinel", "forged-sentinel"),
                ("build_id", "1\n-1 forged-sentinel", "forged-sentinel")):
            with self.subTest(field=field, value=value[:16]):
                self.reset()
                self.assert_rejected_without_echo(with_field(field, value), fragment)

    def test_wrong_json_types_fail_build_without_echo_or_retention(self):
        cases = (
            ("build_revision", 98765432198765, "98765432198765"),
            ("build_id", 98765432198765, "98765432198765"),
            ("build_revision", True, None),
            ("build_id", False, None),
            ("build_revision", None, None),
            ("build_id", None, None),
            ("build_revision", ["nested-sentinel-value"], "nested-sentinel-value"),
            ("build_id", {"value": "nested-sentinel-value"}, "nested-sentinel-value"),
            ("build_revision", 12.5, "12.5"),
        )
        for field, value, fragment in cases:
            with self.subTest(field=field, value=repr(value)[:24]):
                self.reset()
                sentinels = (fragment,) if fragment else ()
                self.assert_rejected_without_echo(with_field(field, value), *sentinels)

    def test_missing_identity_fields_fail_build_with_null_receipt(self):
        for field in ("build_revision", "build_id"):
            with self.subTest(field=field):
                self.reset()
                report = ready()
                del report[field]
                self.assert_rejected_without_echo(report)

    def test_matching_revision_with_malformed_build_id_is_not_matched(self):
        sentinel = "malformed-attempt-sentinel"
        code, out, _, receipt_text, receipt = self.drive(
            ready(SHA, sentinel), "--expected-sha", SHA)
        self.assertEqual(code, 1)
        self.assertIn("FAIL build", out)
        self.assertNotIn("PASS build", out)
        self.assertNotIn(sentinel, out)
        self.assertNotIn(sentinel, receipt_text)
        self.assertIsNone(receipt.get("build_revision"))
        self.assertIsNone(receipt.get("build_id"))
        self.assertEqual(receipt["expected_sha"], SHA)

    def test_malformed_remote_identity_never_matches_the_same_pin_text(self):
        # Even byte-identical non-canonical text on both sides is no match:
        # the pin itself is refused before any request (see pin tests), and
        # a canonical pin cannot equal a malformed revision.
        sentinel = "noncanonical-printable-sentinel"
        self.assert_rejected_without_echo(ready(sentinel), sentinel,
                                          extra=("--expected-sha", SHA))

    def test_malformed_identity_keeps_parked_readyz_classification(self):
        sentinel = "noncanonical-printable-sentinel"
        parked = ready(sentinel)
        parked["components"] = components(("process", "ready"), ("gateway", "starting"),
                                          ("database", "ready"), ("token_invalid", "ready"))
        out, receipt = self.assert_rejected_without_echo(parked, sentinel, status=503)
        self.assertIn("FAIL readyz", out)
        self.assertIn("truthful, not E2E approval", out)
        self.assertEqual(self.verdicts(receipt)["readyz"], "fail")

    def test_malformed_identity_keeps_checkpoint_read_wording(self):
        sentinel = "noncanonical-printable-sentinel"
        failed_read = ready(sentinel)
        failed_read["components"] = components(("process", "ready"), ("gateway", "starting"),
                                               ("database", "ready"), ("token_invalid", "ready"))
        failed_read["gateway_failure"] = {"phase": "durable_gateway",
                                          "class": "checkpoint_load_failed"}
        out, receipt = self.assert_rejected_without_echo(failed_read, sentinel,
                                                         status=503)
        self.assertIn("checkpoint read failed", out)
        self.assertIn("root cause unverified; require separate reviewed "
                      "schema/ACL/connectivity evidence", out)
        self.assertEqual(receipt["gateway_failure_class"], "checkpoint_load_failed")
        self.assertEqual(receipt["gateway_failure_root_cause"], "unverified")

    # -- malformed local pin: input refusal before any request --------------

    def assert_pin_refused(self, pin, *extra, env=None):
        code, out, err, receipt_text, receipt = self.drive(ready(), *extra, env=env)
        self.assertEqual(code, 2, f"malformed pin must refuse like the origin fence: {out} {err}")
        self.assertEqual(self.requested, [], "no request may follow a malformed pin")
        self.assertIsNone(receipt_text, "no evidence may be written on a pin refusal")
        self.assertIsNone(receipt)
        fragment = pin.strip()
        # Short pins ("unknown", "abc123") are generic vocabulary; the
        # exit-2 + zero-request + no-evidence assertions above still bind.
        if fragment and len(fragment) >= 8:
            self.assertNotIn(fragment, out)
            self.assertNotIn(fragment, err)
            self.assertNotIn(CREDENTIAL_FRAGMENT, out + err)

    def test_malformed_cli_pins_refuse_before_any_request(self):
        for pin in ("abc123",                                            # short
                    "deadbeef",                                          # short hex
                    "0123456789abcdef0123456789abcdef0123456",           # 39 hex
                    "0123456789abcdef0123456789abcdef012345678",         # 41 hex
                    "0123456789ABCDEF0123456789ABCDEF01234567",          # uppercase
                    "ghij" * 10,                                         # non-hex
                    "unknown",                                           # unstamped marker
                    " " + SHA,                                           # padded
                    "malformed-pin-sentinel",
                    CREDENTIAL_URL,
                    "a" * 5000):
            with self.subTest(pin=pin[:24]):
                self.reset()
                self.assert_pin_refused(pin, "--expected-sha", pin)

    def test_pin_with_controls_refuses_without_echo(self):
        pin = "abc\nPASS build: forged-pin-sentinel"
        code, out, err, receipt_text, _ = self.drive(ready(), "--expected-sha", pin)
        self.assertEqual(code, 2)
        self.assertEqual(self.requested, [])
        self.assertIsNone(receipt_text)
        self.assertNotIn("forged-pin-sentinel", out + err)

    def test_malformed_env_pin_refuses_before_any_request(self):
        for pin in ("abc123", "malformed-env-pin-sentinel", "unknown"):
            with self.subTest(pin=pin):
                self.reset()
                self.assert_pin_refused(pin, env={"EXPECTED_SHA": pin})

    def test_valid_pin_still_reaches_both_routes(self):
        # Control for the refusal tests: a canonical pin is not refused.
        code, _, _, _, _ = self.drive(ready(), "--expected-sha", SHA)
        self.assertEqual(code, 0)
        self.assertEqual(self.requested, [STAGING + "/health", STAGING + "/readyz"])

    # -- seams below the CLI: reasons and the receipt shape itself -----------

    def test_check_build_reason_never_carries_a_malformed_value(self):
        for sentinel in ("noncanonical-printable-sentinel", CREDENTIAL_URL, "b" * 500):
            for pin in (None, SHA):
                with self.subTest(sentinel=sentinel[:24], pin=pin):
                    result = probe.check_build(ready(sentinel, sentinel), pin)
                    self.assertFalse(result.ok)
                    self.assertNotIn(sentinel, result.reason)
                    self.assertNotIn(CREDENTIAL_FRAGMENT, result.reason)

    def test_check_build_unstamped_pair_semantics(self):
        no_pin = probe.check_build(ready("unknown", "unknown"), None)
        self.assertTrue(no_pin.ok)
        self.assertIn("unstamped", no_pin.reason.lower())
        self.assertNotIn("matches the expected SHA", no_pin.reason)
        pinned = probe.check_build(ready("unknown", "unknown"), SHA)
        self.assertFalse(pinned.ok)
        self.assertNotIn("matches the expected SHA", pinned.reason)

    def test_check_build_without_a_report_is_unchanged(self):
        result = probe.check_build(None, SHA)
        self.assertFalse(result.ok)
        self.assertIn("no readyz body", result.reason)

    def test_evidence_shape_never_serializes_a_malformed_value(self):
        results = [probe.Result("health", True, "health 200 with the exact liveness shape"),
                   probe.Result("readyz", True, "readyz 200: every component ready"),
                   probe.Result("build", False, "fixed diagnostic")]
        for report in (ready("noncanonical-printable-sentinel", BUILD_ID),
                       ready(SHA, CREDENTIAL_URL),
                       ready(SHA, "unknown"),
                       ready("unknown", BUILD_ID),
                       ready(98765432198765, ["nested-sentinel-value"])):
            with self.subTest(report=(repr(report["build_revision"])[:24],
                                      repr(report["build_id"])[:24])):
                receipt = probe.evidence_shape(results, report, SHA)
                text = json.dumps(receipt)
                self.assertNotIn("noncanonical-printable-sentinel", text)
                self.assertNotIn(CREDENTIAL_FRAGMENT, text)
                self.assertNotIn("98765432198765", text)
                self.assertNotIn("nested-sentinel-value", text)
                self.assertIsNone(receipt.get("build_revision"))
                self.assertIsNone(receipt.get("build_id"))

    def test_evidence_shape_keeps_canonical_and_unstamped_identities(self):
        results = [probe.Result("build", True, "fixed diagnostic")]
        receipt = probe.evidence_shape(results, ready(), None)
        self.assertEqual(receipt["build_revision"], SHA)
        self.assertEqual(receipt["build_id"], BUILD_ID)
        receipt = probe.evidence_shape(results, ready("unknown", "unknown"), None)
        self.assertEqual(receipt["build_revision"], "unknown")
        self.assertEqual(receipt["build_id"], "unknown")


if __name__ == "__main__":
    unittest.main()
