"""Duplicate-key refusal acceptance for scripts/staging_health_contract_probe.py.

The probe parses both staging bodies with a hookless ``json.loads``
(``get_json``), so a repeated object key currently keeps the last value and
the probe grades it: a repeated ``status`` can forge the liveness shape, and
a repeated ``build_revision`` can mask a stale container as this run's
build — or echo the hostile value into a verdict and the evidence receipt.

Contract pinned here: any duplicate object key in either body — conflicting
in both orders, identical values, nested, or hidden behind an escaped
spelling — refuses as malformed, exactly like a garbage body: the same
fixed-vocabulary reasons, the same nonzero exit, and no echo of the hostile
keys, values or bodies in verdicts or the receipt. Duplicate-free bodies
grade byte-identically to today.

Stdlib only: the probe only gets fake fetches here, and any real HTTP is a
test failure.

This file is the contract: satisfy it by changing the probe, not by
editing these tests.
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

sys.path.insert(0, str(Path(__file__).resolve().parent))
import staging_health_contract_probe as probe  # noqa: E402

STAGING = "https://two-bot-next-staging.example-sub.workers.dev"
SHA = "0123456789abcdef0123456789abcdef01234567"
OTHER_SHA = "f" * 40
BUILD_ID = "1-1"
GARBAGE = b"{not json"

# Hostile markers are assembled at run time so the file stays scanner-clean.
HOSTILE_STATUS = "hos" + "tile" + "-status-9"
HOSTILE_REVISION = "hos" + "tile" + "-revision-9"

READY = {
    "components": [["process", "ready"], ["gateway", "ready"],
                   ["database", "ready"], ["token_invalid", "ready"]],
    "jobs": {},
    "build_revision": SHA,
    "build_id": BUILD_ID,
}
HEALTH_OK = {"status": "ok"}
HEALTH_UNREADABLE = "health 200 without the exact status shape"
READYZ_UNREADABLE = ("readyz 200 without the bot's component breakdown "
                     "(ownership refusal or broken deploy)")
NO_BUILD_BODY = "no readyz body to read build identity from"


def components_json():
    return json.dumps(READY["components"])


# Duplicate-key /health bodies. Pre-fix the last value wins and is graded.
HEALTH_IDENTICAL = b'{"status": "ok", "status": "ok"}'
HEALTH_HOSTILE_FIRST = ('{"status": "%s", "status": "ok"}' % HOSTILE_STATUS).encode()
HEALTH_OK_FIRST = ('{"status": "ok", "status": "%s"}' % HOSTILE_STATUS).encode()
HEALTH_ESCAPED = b'{"status": "ok", "\\u0073tatus": "ok"}'

# Duplicate-key /readyz bodies. Pre-fix the last value wins and is graded.
READYZ_CONFLICT_OTHER_WINS = (
    '{"components": %s, "jobs": {}, "build_revision": "%s", "build_id": "%s", '
    '"build_revision": "%s"}' % (components_json(), SHA, BUILD_ID, OTHER_SHA)
).encode()
READYZ_CONFLICT_SHA_WINS = (
    '{"build_revision": "%s", "components": %s, "jobs": {}, '
    '"build_revision": "%s", "build_id": "%s"}' % (OTHER_SHA, components_json(), SHA, BUILD_ID)
).encode()
READYZ_IDENTICAL = (
    '{"components": %s, "jobs": {}, "build_revision": "%s", "build_id": "%s", '
    '"build_revision": "%s"}' % (components_json(), SHA, BUILD_ID, SHA)
).encode()
READYZ_NESTED_JOBS = (
    '{"components": %s, "jobs": {"counter": 1, "counter": 2}, '
    '"build_revision": "%s", "build_id": "%s"}' % (components_json(), SHA, BUILD_ID)
).encode()
READYZ_ESCAPED = (
    '{"components": %s, "jobs": {}, "build_revision": "%s", "build_id": "%s", '
    '"build_\\u0072evision": "%s"}' % (components_json(), SHA, BUILD_ID, OTHER_SHA)
).encode()
READYZ_HOSTILE_REVISION = (
    '{"components": %s, "jobs": {}, "build_revision": "%s", "build_id": "%s", '
    '"build_revision": "%s"}' % (components_json(), SHA, BUILD_ID, HOSTILE_REVISION)
).encode()


class DuplicateKeyProbeTests(unittest.TestCase):
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
            response = routes[url]
            if isinstance(response, Exception):
                raise response
            return response
        return fake

    def drive(self, routes, *extra):
        out = io.StringIO()
        argv = ["--staging-url", STAGING, "--evidence", self.evidence, *extra]
        with mock.patch.dict(os.environ, {}, clear=True), redirect_stdout(out):
            code = probe.main(argv, fetch_fn=self.fetch(routes))
        lines = out.getvalue().splitlines()
        named = {}
        for line in lines[:-1]:
            head, _, _ = line.partition(":")
            named[head] = line
        return code, named, lines[-1], lines

    def receipt(self):
        return json.loads(Path(self.evidence).read_text())

    def assert_health_refused_as_malformed(self, body):
        routes = {STAGING + "/health": (200, {}, body),
                  STAGING + "/readyz": (200, {}, json.dumps(READY).encode())}
        code, results, _, _ = self.drive(routes, "--expected-sha", SHA)
        self.assertEqual(code, 1)
        self.assertEqual(set(results), {"FAIL health", "PASS readyz", "PASS build"})
        self.assertIn(HEALTH_UNREADABLE, results["FAIL health"])

    def assert_readyz_refused_as_malformed(self, body):
        routes = {STAGING + "/health": (200, {}, json.dumps(HEALTH_OK).encode()),
                  STAGING + "/readyz": (200, {}, body)}
        code, results, _, _ = self.drive(routes, "--expected-sha", SHA)
        self.assertEqual(code, 1)
        self.assertEqual(set(results), {"PASS health", "FAIL readyz", "FAIL build"})
        self.assertIn(READYZ_UNREADABLE, results["FAIL readyz"])
        self.assertIn(NO_BUILD_BODY, results["FAIL build"])

    def test_health_identical_values_refuse_as_malformed(self):
        self.assert_health_refused_as_malformed(HEALTH_IDENTICAL)

    def test_health_conflicting_orders_refuse_as_malformed(self):
        # Hostile-first currently passes (last-wins "ok"); ok-first already
        # fails with the same reason and pins it plus the no-echo rule.
        self.assert_health_refused_as_malformed(HEALTH_HOSTILE_FIRST)
        self.assert_health_refused_as_malformed(HEALTH_OK_FIRST)

    def test_health_escaped_spelling_refuses(self):
        # The decoder resolves escapes before the hook runs, so this is a
        # repeat of "status", not a new key. A text scan would miss it.
        self.assert_health_refused_as_malformed(HEALTH_ESCAPED)

    def test_health_duplicate_matches_garbage_body_verdicts(self):
        # Duplicates are malformed: byte-identical verdicts, reasons and exit
        # code to a garbage body on the same route.
        readyz = (200, {}, json.dumps(READY).encode())

        def run(body):
            return self.drive({STAGING + "/health": (200, {}, body),
                               STAGING + "/readyz": readyz}, "--expected-sha", SHA)

        duplicate = run(HEALTH_IDENTICAL)
        garbage = run(GARBAGE)
        self.assertEqual((duplicate[0], duplicate[3]), (garbage[0], garbage[3]))

    def test_readyz_conflicting_revisions_refuse_before_grading(self):
        # Other-wins currently echoes the hostile revision into the build
        # verdict and receipt; sha-wins currently passes outright.
        self.assert_readyz_refused_as_malformed(READYZ_CONFLICT_OTHER_WINS)
        self.assert_readyz_refused_as_malformed(READYZ_CONFLICT_SHA_WINS)

    def test_readyz_identical_values_refuse(self):
        self.assert_readyz_refused_as_malformed(READYZ_IDENTICAL)

    def test_readyz_nested_duplicate_refuses(self):
        self.assert_readyz_refused_as_malformed(READYZ_NESTED_JOBS)

    def test_readyz_escaped_spelling_refuses(self):
        self.assert_readyz_refused_as_malformed(READYZ_ESCAPED)

    def test_readyz_duplicate_matches_garbage_body_verdicts(self):
        health = (200, {}, json.dumps(HEALTH_OK).encode())

        def run(body):
            return self.drive({STAGING + "/health": health,
                               STAGING + "/readyz": (200, {}, body)},
                              "--expected-sha", SHA)

        duplicate = run(READYZ_IDENTICAL)
        garbage = run(GARBAGE)
        self.assertEqual((duplicate[0], duplicate[3]), (garbage[0], garbage[3]))

    def test_hostile_values_never_echo_in_verdicts_or_receipt(self):
        bodies = {"health": HEALTH_OK_FIRST, "readyz_revision": READYZ_HOSTILE_REVISION,
                  "readyz_sha": READYZ_CONFLICT_OTHER_WINS}
        for name, body in bodies.items():
            with self.subTest(name):
                if name == "health":
                    routes = {STAGING + "/health": (200, {}, body),
                              STAGING + "/readyz": (200, {}, json.dumps(READY).encode())}
                else:
                    routes = {STAGING + "/health": (200, {}, json.dumps(HEALTH_OK).encode()),
                              STAGING + "/readyz": (200, {}, body)}
                code, _, _, lines = self.drive(routes, "--expected-sha", SHA)
                text = "\n".join(lines)
                dumped = json.dumps(self.receipt())
                self.assertEqual(code, 1, text)
                self.assertNotIn(HOSTILE_STATUS, text)
                self.assertNotIn(HOSTILE_REVISION, text)
                self.assertNotIn(HOSTILE_STATUS, dumped)
                self.assertNotIn(HOSTILE_REVISION, dumped)
                self.assertNotIn(OTHER_SHA, text)
                self.assertNotIn(OTHER_SHA, dumped)

    def test_readyz_duplicate_leaves_no_build_identity_in_receipt(self):
        routes = {STAGING + "/health": (200, {}, json.dumps(HEALTH_OK).encode()),
                  STAGING + "/readyz": (200, {}, READYZ_CONFLICT_SHA_WINS)}
        code, _, _, _ = self.drive(routes, "--expected-sha", SHA)
        self.assertEqual(code, 1)
        receipt = self.receipt()
        self.assertIsNone(receipt["build_revision"])
        self.assertIsNone(receipt["build_id"])
        self.assertIsNone(receipt["gateway_failure_class"])

    def test_unique_bodies_grade_unchanged(self):
        # Control: duplicate-free bodies pass exactly as today, pre- and post-fix.
        routes = {STAGING + "/health": (200, {}, json.dumps(HEALTH_OK).encode()),
                  STAGING + "/readyz": (200, {}, json.dumps(READY).encode())}
        code, results, summary, _ = self.drive(routes, "--expected-sha", SHA)
        self.assertEqual(code, 0)
        self.assertEqual(set(results), {"PASS health", "PASS readyz", "PASS build"})
        self.assertIn("3/3 checks passed", summary)
        receipt = self.receipt()
        self.assertEqual(receipt["build_revision"], SHA)
        self.assertEqual(receipt["build_id"], BUILD_ID)

    def test_get_json_unique_parses_exactly(self):
        # Control: the parse itself is byte-identical on unique objects.
        body = json.dumps(READY).encode()

        def fake(url):
            return 200, {}, body

        status, _, value = probe.get_json(fake, STAGING + "/readyz", "readyz")
        self.assertEqual(status, 200)
        self.assertEqual(value, json.loads(body))

    def test_get_json_duplicate_is_unparseable(self):
        def fake(url):
            return 200, {}, READYZ_IDENTICAL

        status, _, value = probe.get_json(fake, STAGING + "/readyz", "readyz")
        self.assertEqual(status, 200)
        self.assertIsNone(value)


if __name__ == "__main__":
    unittest.main()
