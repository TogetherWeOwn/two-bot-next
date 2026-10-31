"""Protected acceptance: watch checkpoints admit only the production origin.

Offline only (stdlib, fake fetches, no network). No implementation source
or diff is used to derive expectations. The combined fix must retain this
file byte-identically; the builder must make these tests pass without
editing it.

Contract: scripts/cutover_watch_checkpoint.py admits exactly the
production Worker origins the passive production probe admits, reusing its
canonical PROD_HOST allowlist and STAGING_LABEL objects (assertIs: never a
second regex). A missing origin, a staging host, or any other non-admitted
target (HTTP, userinfo, port, path/query/fragment, arbitrary or lookalike
host, unparseable input) is refused before any fetch with a stable
snake_case CheckpointError code: missing_origin, staging_refused, or
origin_not_admitted. Refused targets send zero fetches, exit 1 with an
EXTEND row that carries the code and never echoes the rejected input, and
no flag may relabel staging evidence as production.
"""

import contextlib
import io
import json
import re
import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import cutover_watch_checkpoint as checkpoint
import prod_passive_probe as prod_probe

SHA = "0123456789abcdef0123456789abcdef01234567"
BUILD_ID = "700-1"
PRODUCTION_A = "https://two-bot-next-production.example.workers.dev"
PRODUCTION_B = "https://two-bot-next.example.workers.dev"

STAGING = (
    "https://two-bot-next-staging.example.workers.dev",
    "https://two-bot-next-staging.example.workers.dev/",
    # Staging by host, like the canonical probe: extra defects do not change
    # the verdict class.
    "https://two-bot-next-staging.example.workers.dev:8443",
)

# Every non-staging refusal shares one stable code.
NOT_ADMITTED = (
    # Arbitrary hosts.
    "https://example.com",
    "https://example.com/",
    "https://attacker.example/",
    # Lookalikes: the allowlist is a fullmatch, so affixes fail.
    "https://two-bot-next-production.evil.com",
    "https://two-bot-next-production.example.workers.dev.evil.com",
    "https://two-bot-next.example.workers.dev.attacker.example",
    "https://two-bot-next-production-example.workers.dev",
    "https://sub.two-bot-next.example.workers.dev",
    "https://two-bot-next.example.workers.devx",
    # Production-shaped but not a bare https origin.
    "http://two-bot-next.example.workers.dev",
    "https://user@two-bot-next.example.workers.dev",
    "https://user:pass@two-bot-next.example.workers.dev",
    "https://two-bot-next-staging@two-bot-next.example.workers.dev",
    "https://two-bot-next.example.workers.dev:8443",
    "https://two-bot-next.example.workers.dev/readyz",
    "https://two-bot-next.example.workers.dev/?x=1",
    "https://two-bot-next.example.workers.dev#frag",
    # Unparseable or wrong scheme.
    "not a url",
    "ftp://two-bot-next.example.workers.dev",
    "https://",
    "::::",
)

REFUSAL_CODES = ("missing_origin", "staging_refused", "origin_not_admitted")


def readyz_body(*components, revision=SHA, build_id=BUILD_ID):
    return json.dumps({"components": [list(c) for c in components], "jobs": {},
                       "build_revision": revision, "build_id": build_id}).encode()


READY = (200, readyz_body(("process", "ready"), ("gateway", "ready"),
                          ("database", "ready"), ("token_invalid", "ready")))


def argv(*extra, sha=SHA, build_id=BUILD_ID, url=PRODUCTION_A, checkpoint_label="+15m"):
    return ["--checkpoint", checkpoint_label, "--expected-sha", sha,
            "--expected-build-id", build_id, "--production-url", url, *extra]


def refusal_code(error):
    """The stable code, or None when the seam is missing (clean FAIL, not ERROR)."""
    return getattr(error, "code", None)


class CanonicalOriginReuseTests(unittest.TestCase):
    def test_prod_host_is_the_canonical_probe_object(self):
        candidate = getattr(checkpoint, "PROD_HOST", None)
        self.assertIsNotNone(candidate,
                             "checkpoint must reuse prod_passive_probe.PROD_HOST, not a second regex")
        self.assertIs(candidate, prod_probe.PROD_HOST)

    def test_staging_label_is_the_canonical_probe_object(self):
        candidate = getattr(checkpoint, "STAGING_LABEL", None)
        self.assertIsNotNone(candidate,
                             "checkpoint must reuse prod_passive_probe.STAGING_LABEL")
        self.assertIs(candidate, prod_probe.STAGING_LABEL)


class OriginAdmissionTests(unittest.TestCase):
    def test_production_origins_are_admitted(self):
        """Positive control: both documented production shapes pass through."""
        for url in (PRODUCTION_A, PRODUCTION_A + "/", PRODUCTION_B, PRODUCTION_B + "/"):
            with self.subTest(url=url):
                self.assertEqual(checkpoint.production_readyz_url(url),
                                 url.rstrip("/") + "/readyz")

    def test_missing_origin_refused_with_stable_code(self):
        for url in ("", None):
            with self.subTest(url=url):
                with self.assertRaises(checkpoint.CheckpointError) as caught:
                    checkpoint.production_readyz_url(url)
                self.assertEqual(refusal_code(caught.exception), "missing_origin")

    def test_staging_refused_with_stable_code(self):
        for url in STAGING:
            with self.subTest(url=url):
                with self.assertRaises(checkpoint.CheckpointError) as caught:
                    checkpoint.production_readyz_url(url)
                self.assertEqual(refusal_code(caught.exception), "staging_refused")

    def test_arbitrary_lookalike_and_malformed_refused_with_stable_code(self):
        for url in NOT_ADMITTED:
            with self.subTest(url=url):
                with self.assertRaises(checkpoint.CheckpointError) as caught:
                    checkpoint.production_readyz_url(url)
                self.assertEqual(refusal_code(caught.exception), "origin_not_admitted")

    def test_refusal_codes_are_stable_snake_case(self):
        seen = set()
        for url in ("", *STAGING, *NOT_ADMITTED):
            with self.assertRaises(checkpoint.CheckpointError) as caught:
                checkpoint.production_readyz_url(url)
            code = refusal_code(caught.exception)
            self.assertIsInstance(code, str)
            self.assertTrue(re.fullmatch(r"[a-z][a-z_]*", code or ""),
                            f"refusal code must be stable snake_case, got {code!r}")
            seen.add(code)
        self.assertEqual(seen, set(REFUSAL_CODES))

    def test_admission_matches_the_canonical_probe(self):
        """The checkpoint admits exactly what prod_origin admits (no second policy)."""
        for url in (PRODUCTION_A, PRODUCTION_A + "/", PRODUCTION_B, "",
                    *STAGING, *NOT_ADMITTED):
            with self.subTest(url=url):
                try:
                    endpoint = checkpoint.production_readyz_url(url)
                except checkpoint.CheckpointError:
                    checkpoint_admits = False
                else:
                    checkpoint_admits = True
                try:
                    origin = prod_probe.prod_origin(url)
                except prod_probe.ProbeError:
                    probe_admits = False
                else:
                    probe_admits = True
                self.assertEqual(checkpoint_admits, probe_admits,
                                 f"admission diverges from the canonical probe for {url!r}")
                if checkpoint_admits:
                    self.assertEqual(endpoint, origin + "/readyz")


class ZeroFetchOnRefusalTests(unittest.TestCase):
    def run_main(self, args, response=READY):
        requested = []

        def fake(url):
            requested.append(url)
            if isinstance(response, Exception):
                raise response
            return response

        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = checkpoint.main(args, fetch_fn=fake)
        return code, out.getvalue().strip(), requested

    def test_refused_origins_send_zero_fetches(self):
        for url in (*STAGING, *NOT_ADMITTED):
            with self.subTest(url=url):
                code, row, requested = self.run_main(argv(url=url))
                self.assertEqual(code, 1, row)
                self.assertTrue(row.endswith("| EXTEND |"), row)
                self.assertEqual(requested, [])

    def test_missing_origin_sends_zero_fetches(self):
        code, row, requested = self.run_main(argv(url=""))
        self.assertEqual(code, 1, row)
        self.assertTrue(row.endswith("| EXTEND |"), row)
        self.assertEqual(requested, [])
        args = ["--checkpoint", "+15m", "--expected-sha", SHA,
                "--expected-build-id", BUILD_ID]
        with mock.patch.dict("os.environ", {}, clear=True):
            code, row, requested = self.run_main(args)
        self.assertEqual(code, 1, row)
        self.assertTrue(row.endswith("| EXTEND |"), row)
        self.assertEqual(requested, [])

    def test_refused_rows_carry_the_code_without_echo(self):
        cases = [("", "missing_origin"),
                 *[(url, "staging_refused") for url in STAGING],
                 *[(url, "origin_not_admitted") for url in NOT_ADMITTED]]
        for url, code_ in cases:
            with self.subTest(url=url):
                _code, row, _requested = self.run_main(argv(url=url))
                self.assertIn(code_, row)
                self.assertNotIn("\n", row)
                self.assertEqual(row.count("|"), 6, row)
                if url:
                    self.assertNotIn(url, row)

    def test_admitted_origin_fetches_exactly_once(self):
        """Positive control: the admitted path still sends its single GET."""
        code, row, requested = self.run_main(argv(url=PRODUCTION_A))
        self.assertEqual(code, 0, row)
        self.assertTrue(row.endswith("| GO |"), row)
        self.assertEqual(requested, [PRODUCTION_A + "/readyz"])


class NoRelabelTests(unittest.TestCase):
    def test_no_flag_combination_admits_staging(self):
        for extra in ([], ["--format", "json"], ["--format", "markdown"]):
            with self.subTest(extra=extra):
                requested = []

                def fake(url):
                    requested.append(url)
                    return READY

                out = io.StringIO()
                with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
                    try:
                        code = checkpoint.main(argv(url=STAGING[0], *extra), fetch_fn=fake)
                    except SystemExit as exit_:
                        self.fail("--format must be accepted "
                                  f"(exited {exit_.code})")
                row = out.getvalue().strip()
                self.assertEqual(code, 1, row)
                self.assertEqual(requested, [])
                self.assertNotIn(STAGING[0], row)
                if extra == ["--format", "json"]:
                    self.assertNotIn("\n", row)
                    try:
                        receipt = json.loads(row)
                    except ValueError:
                        self.fail(f"stdout must be one JSON object, got: {row[:200]!r}")
                    self.assertEqual(receipt.get("verdict"), "EXTEND")
                    self.assertEqual(receipt.get("reason"), "staging_refused")
                    self.assertIsNone(receipt.get("origin"))
                    self.assertIsNone(receipt.get("observed"))
                else:
                    self.assertTrue(row.endswith("| EXTEND |"), row)
                    self.assertIn("staging_refused", row)

    def test_cli_offers_exactly_the_documented_flags(self):
        """No origin-override escape hatch may exist beside the five flags."""
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            with self.assertRaises(SystemExit) as exit_:
                checkpoint.main(["--help"])
        self.assertEqual(exit_.exception.code, 0)
        flags = set(re.findall(r"--[a-z0-9][a-z0-9-]*", out.getvalue()))
        self.assertEqual(flags, {"--help", "--checkpoint", "--expected-sha",
                                 "--expected-build-id", "--production-url", "--format"})


if __name__ == "__main__":
    unittest.main()
