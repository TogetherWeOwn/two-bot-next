"""Acceptance tests: the snowflake gate must scan top-level ``src/``.

The gate in ``check-src-snowflakes.py`` currently walks only
``crates/*/src/**/*.rs`` and its allowlist validation accepts only
``crates/.../src/...`` paths, so the workspace root crate (``src/lib.rs``
and anything under ``src/``) is production Rust that is never scanned,
and a legitimate literal there cannot be allowlisted either.

These tests pin the required behaviour with synthetic trees only:

- an unallowlisted snowflake under root ``src/`` (top-level or nested)
  fails the gate, reported with its ``src/...`` path;
- a root literal allowed by an exact (path, value, source) entry with a
  reason passes, and the allowance stays occurrence-scoped, path-scoped
  and enforced as stale once the literal disappears;
- a clean root ``src/`` passes, comments and exact ``#[cfg(test)]``
  modules under root ``src/`` stay excluded;
- existing crate controls are unchanged;
- allowance validation still refuses missing reasons and escaping paths.

The root-scan tests are RED against the current gate and must turn GREEN
without editing this file. The control tests pass before and after.
"""

import importlib.util
import os
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location(
    "snowflakes", Path(__file__).with_name("check-src-snowflakes.py")
)
snowflakes = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(snowflakes)

SOURCE = 'pub const LIVE_GUILD_ID: &str = "123456789012345678";'
VALUE = "123456789012345678"


def write(root, relative, text):
    path = root / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    return path


def scratch():
    return tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR"))


class RootSrcScanAcceptanceTests(unittest.TestCase):
    def test_unallowlisted_root_snowflake_fails(self):
        with scratch() as temp:
            root = Path(temp)
            write(root, "src/lib.rs", SOURCE + "\n")
            errors = snowflakes.check(root, [])
            self.assertEqual(len(errors), 1)
            self.assertTrue(errors[0].startswith("src/lib.rs:"), errors)
            self.assertIn(VALUE, errors[0])

    def test_unallowlisted_nested_root_snowflake_fails(self):
        with scratch() as temp:
            root = Path(temp)
            write(root, "src/nested/mod.rs", SOURCE + "\n")
            errors = snowflakes.check(root, [])
            self.assertEqual(len(errors), 1)
            self.assertTrue(errors[0].startswith("src/nested/mod.rs:"), errors)

    def test_root_and_crate_violations_are_both_reported(self):
        with scratch() as temp:
            root = Path(temp)
            write(root, "src/lib.rs", SOURCE + "\n")
            write(root, "crates/cutover/src/lib.rs", SOURCE + "\n")
            errors = snowflakes.check(root, [])
            self.assertEqual(len(errors), 2)
            self.assertTrue(any(e.startswith("src/lib.rs:") for e in errors), errors)
            self.assertTrue(
                any(e.startswith("crates/cutover/src/lib.rs:") for e in errors), errors
            )

    def test_root_allowance_with_reason_passes(self):
        with scratch() as temp:
            root = Path(temp)
            write(root, "src/lib.rs", SOURCE + "\n")
            allowance = [{
                "path": "src/lib.rs",
                "value": VALUE,
                "source": SOURCE,
                "reason": "live-guild refusal guard",
            }]
            self.assertEqual(snowflakes.check(root, allowance), [])

    def test_root_allowance_is_occurrence_scoped(self):
        with scratch() as temp:
            root = Path(temp)
            write(root, "src/lib.rs", SOURCE + "\n" + SOURCE + "\n")
            allowance = [{
                "path": "src/lib.rs",
                "value": VALUE,
                "source": SOURCE,
                "reason": "live-guild refusal guard",
            }]
            self.assertEqual(len(snowflakes.check(root, allowance)), 1)

    def test_crate_allowance_does_not_cover_identical_root_line(self):
        with scratch() as temp:
            root = Path(temp)
            write(root, "src/lib.rs", SOURCE + "\n")
            allowance = [{
                "path": "crates/cutover/src/lib.rs",
                "value": VALUE,
                "source": SOURCE,
                "reason": "live-guild refusal guard",
            }]
            # Root literal is unallowlisted (1) and the crate allowance is
            # stale because the crate file does not exist (1).
            self.assertEqual(len(snowflakes.check(root, allowance)), 2)

    def test_stale_root_allowance_fails(self):
        with scratch() as temp:
            root = Path(temp)
            write(root, "src/lib.rs", "pub const VERSION: u64 = 1;\n")
            allowance = [{
                "path": "src/lib.rs",
                "value": VALUE,
                "source": SOURCE,
                "reason": "live-guild refusal guard",
            }]
            errors = snowflakes.check(root, allowance)
            self.assertEqual(len(errors), 1)
            self.assertIn("stale allowance", errors[0])


class RootSrcControlTests(unittest.TestCase):
    def test_clean_root_src_passes(self):
        with scratch() as temp:
            root = Path(temp)
            write(root, "src/lib.rs", 'pub const VERSION: &str = env!("CARGO_PKG_VERSION");\n')
            write(root, "src/nested/mod.rs", 'pub const SHORT: &str = "1234567890123456";\n')
            self.assertEqual(snowflakes.check(root, []), [])

    def test_root_comments_and_test_module_stay_excluded(self):
        with scratch() as temp:
            root = Path(temp)
            write(root, "src/lib.rs", (
                "// 123456789012345678\n"
                "#[cfg(test)]\n"
                'mod tests { const ID: &str = "123456789012345679"; }\n'
            ))
            self.assertEqual(snowflakes.check(root, []), [])

    def test_crate_controls_unchanged(self):
        with scratch() as temp:
            root = Path(temp)
            path = write(root, "crates/cutover/src/lib.rs", SOURCE + "\n")
            self.assertEqual(len(snowflakes.check(root, [])), 1)
            allowance = [{
                "path": "crates/cutover/src/lib.rs",
                "value": VALUE,
                "source": SOURCE,
                "reason": "live-guild refusal guard",
            }]
            self.assertEqual(snowflakes.check(root, allowance), [])
            path.write_text(SOURCE + "\n" + SOURCE + "\n")
            self.assertEqual(len(snowflakes.check(root, allowance)), 1)

    def test_root_allowance_still_requires_reason(self):
        for reason in ["", "   "]:
            with self.assertRaises(ValueError):
                snowflakes.check(Path("."), [{
                    "path": "src/lib.rs",
                    "value": VALUE,
                    "source": SOURCE,
                    "reason": reason,
                }])

    def test_escaping_allowance_paths_stay_refused(self):
        for path in ["../outside.rs", "src/../../outside.rs"]:
            with self.assertRaises(ValueError):
                snowflakes.check(Path("."), [{
                    "path": path,
                    "value": VALUE,
                    "source": SOURCE,
                    "reason": "fixture",
                }])


if __name__ == "__main__":
    unittest.main()
