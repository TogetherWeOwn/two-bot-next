#!/usr/bin/env python3
"""Offline tests for scripts/vendor_brand_tokens.py (no network, no real writes)."""

import contextlib
import importlib.util
import io
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[1] / "vendor_brand_tokens.py"
spec = importlib.util.spec_from_file_location("vendor_brand_tokens", SCRIPT)
vbt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(vbt)

REAL_BRAND = vbt.BRAND_RS
REAL_TOKENS = vbt.TOKENS_RS

TOKENS = (
    b"//! DO NOT EDIT: generated fixture.\n\n"
    b"pub const BRAND_PAGE: u32 = 0x15100F;\n"
    b"pub const BRAND_ACCENT: u32 = 0xFF9C36;\n"
)
NEW_TOKENS = TOKENS + b"pub const BRAND_COLOR_OK: u32 = 0x5FD39A;\n"


def brand_source(tag: str, digest: int) -> str:
    return (
        f'pub const BRAND_KIT_TAG: &str = "{tag}";\n'
        f"pub const BRAND_TOKENS_FNV1A64: u64 = 0x{digest:016X};\n"
    )


class VendorBrandTokensTest(unittest.TestCase):
    def setUp(self):
        base = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or None
        tmp = tempfile.TemporaryDirectory(dir=base)
        self.addCleanup(tmp.cleanup)
        self.dir = Path(tmp.name)
        self.brand = self.dir / "brand.rs"
        self.tokens = self.dir / "brand_tokens.rs"
        self.tokens.write_bytes(TOKENS)
        self.brand.write_text(brand_source("v0.2.0", vbt.fnv1a64(TOKENS)), encoding="utf-8")
        for name, path in (
            ("ROOT", self.dir),
            ("BRAND_RS", self.brand),
            ("TOKENS_RS", self.tokens),
        ):
            patcher = mock.patch.object(vbt, name, path)
            patcher.start()
            self.addCleanup(patcher.stop)
        # Any network use must be explicit; fail loudly otherwise.
        patcher = mock.patch.object(
            vbt.urllib.request, "urlopen", side_effect=AssertionError("network used")
        )
        patcher.start()
        self.addCleanup(patcher.stop)

    def run_main(self, *argv):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                code = vbt.main(list(argv))
            except SystemExit as exc:
                code = exc.code
        return code, out.getvalue(), err.getvalue()

    def test_committed_pin_matches_offline(self):
        before = (REAL_BRAND.read_bytes(), REAL_TOKENS.read_bytes())
        with mock.patch.object(vbt, "BRAND_RS", REAL_BRAND), mock.patch.object(
            vbt, "TOKENS_RS", REAL_TOKENS
        ):
            code, out, _ = self.run_main("--check")
        self.assertEqual(code, 0)
        self.assertIn("offline digest", out)
        self.assertEqual(before, (REAL_BRAND.read_bytes(), REAL_TOKENS.read_bytes()))

    def test_fixture_check_succeeds(self):
        code, out, _ = self.run_main("--check")
        self.assertEqual(code, 0)
        self.assertIn("v0.2.0", out)

    def test_mutated_colour_fails_without_rewriting(self):
        mutated = TOKENS.replace(b"0xFF9C36", b"0xFF9C37")
        self.assertNotEqual(mutated, TOKENS)
        self.tokens.write_bytes(mutated)
        brand_before = self.brand.read_bytes()
        code, _, err = self.run_main("--check")
        self.assertEqual(code, 1)
        self.assertIn("drifted", err)
        self.assertEqual(self.tokens.read_bytes(), mutated)
        self.assertEqual(self.brand.read_bytes(), brand_before)

    def test_missing_generator_marker_fails(self):
        body = b"// hand written\n" + TOKENS
        self.tokens.write_bytes(body)
        self.brand.write_text(brand_source("v0.2.0", vbt.fnv1a64(body)), encoding="utf-8")
        code, _, _ = self.run_main("--check")
        self.assertEqual(code, 1)

    def test_missing_hash_refuses(self):
        self.brand.write_text('pub const BRAND_KIT_TAG: &str = "v0.2.0";\n', encoding="utf-8")
        code, _, _ = self.run_main("--check")
        self.assertIsInstance(code, str)
        self.assertIn("BRAND_TOKENS_FNV1A64 not found", code)

    def test_check_upstream_byte_mismatch_fails_read_only(self):
        tokens_before = self.tokens.read_bytes()
        brand_before = self.brand.read_bytes()
        with mock.patch.object(vbt, "fetch", return_value=NEW_TOKENS) as fetch:
            code, _, err = self.run_main("--check-upstream")
        fetch.assert_called_once_with("v0.2.0")
        self.assertEqual(code, 1)
        self.assertIn("drifted", err)
        self.assertEqual(self.tokens.read_bytes(), tokens_before)
        self.assertEqual(self.brand.read_bytes(), brand_before)

    def test_check_upstream_match_succeeds(self):
        with mock.patch.object(vbt, "fetch", return_value=TOKENS):
            code, out, _ = self.run_main("--check-upstream")
        self.assertEqual(code, 0)
        self.assertIn("upstream bytes", out)

    def test_revendor_refreshes_tag_and_hash(self):
        with mock.patch.object(vbt, "fetch", return_value=NEW_TOKENS) as fetch:
            code, out, _ = self.run_main("--tag", "v0.3.0")
        fetch.assert_called_once_with("v0.3.0")
        self.assertEqual(code, 0)
        self.assertIn("vendored v0.3.0", out)
        self.assertEqual(self.tokens.read_bytes(), NEW_TOKENS)
        self.assertEqual(
            self.brand.read_text(encoding="utf-8"),
            brand_source("v0.3.0", vbt.fnv1a64(NEW_TOKENS)),
        )
        code, _, _ = self.run_main("--check")
        self.assertEqual(code, 0)

    def test_revendor_default_uses_pinned_tag(self):
        with mock.patch.object(vbt, "fetch", return_value=NEW_TOKENS) as fetch:
            code, _, _ = self.run_main()
        fetch.assert_called_once_with("v0.2.0")
        self.assertEqual(code, 0)
        self.assertEqual(self.tokens.read_bytes(), NEW_TOKENS)

    def test_malformed_source_refuses_before_rewriting_tokens(self):
        bad = 'pub const BRAND_KIT_TAG: &str = "v0.2.0";\n'  # no hash const
        self.brand.write_text(bad, encoding="utf-8")
        with mock.patch.object(vbt, "fetch", return_value=NEW_TOKENS):
            code, _, _ = self.run_main("--tag", "v0.3.0")
        self.assertIsInstance(code, str)
        self.assertIn("did not patch exactly once", code)
        self.assertEqual(self.tokens.read_bytes(), TOKENS)
        self.assertEqual(self.brand.read_text(encoding="utf-8"), bad)

    def test_fetch_rejects_payload_without_marker(self):
        class Response:
            def __enter__(self):
                return self

            def __exit__(self, *exc):
                return False

            def read(self):
                return b"<html>not tokens</html>"

        with mock.patch.object(vbt.urllib.request, "urlopen", return_value=Response()):
            with self.assertRaises(SystemExit) as ctx:
                vbt.fetch("v0.2.0")
        self.assertIn("missing generator marker", str(ctx.exception))

    def test_tag_validation_and_exclusive_flags(self):
        for argv in (
            ("--tag", "main"),
            ("--tag", "v1.2"),
            ("--check", "--tag", "v0.3.0"),
            ("--check-upstream", "--tag", "v0.3.0"),
            ("--check", "--check-upstream"),
        ):
            with self.subTest(argv=argv):
                code, _, _ = self.run_main(*argv)
                self.assertEqual(code, 2)
        self.assertEqual(self.tokens.read_bytes(), TOKENS)

    def test_bad_pinned_tag_refuses_check(self):
        self.brand.write_text(brand_source("main", vbt.fnv1a64(TOKENS)), encoding="utf-8")
        code, _, _ = self.run_main("--check")
        self.assertEqual(code, 2)


if __name__ == "__main__":
    unittest.main()
