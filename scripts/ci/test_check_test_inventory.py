"""Offline tests for the CI test-target inventory HARD gate.

Fixtures build a miniature repo (workflows, one crate, one fuzz target) and
assert the gate passes a routed tree, fails an orphaned target, honours a
reasoned exemption, rejects an empty reason, and fails when the blanket
workspace runs go missing. The real tree is pinned to pass with exactly the
documented exemption.
"""

import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

import importlib.util

SPEC = importlib.util.spec_from_file_location(
    "inventory_gate", Path(__file__).with_name("check-test-inventory.py")
)
gate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gate)


CHECK_YML = """jobs:
  check:
    steps:
      - run: cargo test --workspace --lib --bins --locked
      - run: cargo test --workspace --doc --locked
      - run: cargo clippy --workspace --all-targets --locked
      - run: cargo test --workspace --test '*' --locked
      - run: cargo test -p my-crate --test db_store --locked -- --ignored
      - run: cargo +nightly-2026-10-01 fuzz build
"""

NIGHTLY_YML = """jobs:
  sweep:
    steps:
      - run: cargo test --workspace --locked
"""


def make_root(files):
    tmp = tempfile.TemporaryDirectory()
    root = Path(tmp.name)
    for path, content in files.items():
        full = root / path
        full.parent.mkdir(parents=True, exist_ok=True)
        full.write_text(content)
    return tmp, root


def base_files():
    return {
        ".github/workflows/check.yml": CHECK_YML,
        ".github/workflows/nightly.yml": NIGHTLY_YML,
        "fuzz/Cargo.toml": '[[bin]]\nname = "wiggle"\npath = "fuzz_targets/wiggle.rs"\n',
        "fuzz/fuzz_targets/wiggle.rs": "// target\n",
        "crates/my-crate/tests/plain.rs": "#[test]\nfn it() {}\n",
        "crates/my-crate/tests/db_store.rs": "#[ignore]\n#[test]\nfn it() {}\n",
    }


class FixtureTests(unittest.TestCase):
    def test_routed_tree_passes(self):
        tmp, root = make_root(base_files())
        try:
            self.assertEqual(gate.check(root), [])
        finally:
            tmp.cleanup()

    def test_ignored_target_without_a_step_is_an_orphan(self):
        files = base_files()
        files["crates/my-crate/tests/db_store.rs"] = "#[ignore]\n#[test]\nfn it() {}\n"
        files[".github/workflows/check.yml"] = CHECK_YML.replace(
            "cargo test -p my-crate --test db_store --locked -- --ignored", "true")
        tmp, root = make_root(files)
        try:
            errors = gate.check(root)
        finally:
            tmp.cleanup()
        self.assertEqual(len(errors), 1)
        self.assertIn("db_store", errors[0])

    def test_block_run_followed_by_name_key_does_not_cover(self):
        files = base_files()
        files[".github/workflows/check.yml"] = CHECK_YML.replace(
            "- run: cargo test -p my-crate --test db_store --locked -- --ignored",
            "- run: |\n"
            "          true\n"
            "        name: cargo test -p my-crate --test db_store --locked -- --ignored")
        tmp, root = make_root(files)
        try:
            errors = gate.check(root)
        finally:
            tmp.cleanup()
        self.assertTrue(any("db_store" in error for error in errors),
                        f"name key after a block run must not cover: {errors}")

    def test_block_run_followed_by_env_key_does_not_cover(self):
        files = base_files()
        files[".github/workflows/check.yml"] = CHECK_YML.replace(
            "- run: cargo test -p my-crate --test db_store --locked -- --ignored",
            "- run: |\n"
            "          true\n"
            "        env:\n"
            "          CMD: cargo test -p my-crate --test db_store --locked -- --ignored")
        tmp, root = make_root(files)
        try:
            errors = gate.check(root)
        finally:
            tmp.cleanup()
        self.assertTrue(any("db_store" in error for error in errors),
                        f"env key after a block run must not cover: {errors}")

    def test_block_run_with_command_inside_covers(self):
        files = base_files()
        files[".github/workflows/check.yml"] = CHECK_YML.replace(
            "- run: cargo test -p my-crate --test db_store --locked -- --ignored",
            "- run: |\n"
            "          cargo test -p my-crate --test db_store --locked -- --ignored\n"
            "        name: ignored db_store lane")
        tmp, root = make_root(files)
        try:
            self.assertEqual(gate.check(root), [])
        finally:
            tmp.cleanup()

    def test_skip_only_reference_does_not_cover(self):
        files = base_files()
        files[".github/workflows/nightly.yml"] = (
            NIGHTLY_YML + "      - run: cargo test --workspace --locked -- --skip db_store\n")
        files[".github/workflows/check.yml"] = CHECK_YML.replace(
            "cargo test -p my-crate --test db_store --locked -- --ignored", "true")
        tmp, root = make_root(files)
        try:
            errors = gate.check(root)
        finally:
            tmp.cleanup()
        self.assertTrue(any("db_store" in error for error in errors),
                        f"skip-only reference must not cover: {errors}")

    def test_unignored_target_rides_the_blanket_run(self):
        files = base_files()
        tmp, root = make_root(files)
        try:
            self.assertEqual(gate.check(root), [])
        finally:
            tmp.cleanup()

    def test_blanket_run_removal_orphans_plain_targets(self):
        files = base_files()
        files[".github/workflows/check.yml"] = CHECK_YML.replace("--test '*'", "--test nothing")
        tmp, root = make_root(files)
        try:
            errors = gate.check(root)
        finally:
            tmp.cleanup()
        self.assertTrue(any("plain" in error for error in errors), errors)

    def test_missing_fuzz_source_is_an_orphan(self):
        files = base_files()
        del files["fuzz/fuzz_targets/wiggle.rs"]
        tmp, root = make_root(files)
        try:
            errors = gate.check(root)
        finally:
            tmp.cleanup()
        self.assertTrue(any("wiggle" in error for error in errors), errors)

    def test_missing_fuzz_build_step_fails(self):
        files = base_files()
        files[".github/workflows/check.yml"] = CHECK_YML.replace(
            "cargo +nightly-2026-10-01 fuzz build", "true")
        tmp, root = make_root(files)
        try:
            errors = gate.check(root)
        finally:
            tmp.cleanup()
        self.assertTrue(any("fuzz" in error for error in errors), errors)

    def test_reasoned_exemption_passes(self):
        files = base_files()
        files["crates/my-crate/tests/db_store.rs"] = "#[ignore]\n#[test]\nfn it() {}\n"
        files[".github/workflows/check.yml"] = CHECK_YML.replace(
            "cargo test -p my-crate --test db_store --locked -- --ignored", "true")
        real_exempt = gate.EXEMPT
        gate.EXEMPT = {**real_exempt, "db_store": "fixture reason"}
        try:
            tmp, root = make_root(files)
            try:
                errors = gate.check(root)
            finally:
                tmp.cleanup()
        finally:
            gate.EXEMPT = real_exempt
        self.assertEqual(errors, [])

    def test_empty_exemption_reason_fails(self):
        real_exempt = gate.EXEMPT
        gate.EXEMPT = {"plain": "  "}
        try:
            tmp, root = make_root(base_files())
            try:
                errors = gate.check(root)
            finally:
                tmp.cleanup()
        finally:
            gate.EXEMPT = real_exempt
        self.assertTrue(any("plain" in error for error in errors), errors)


class RealTreeTests(unittest.TestCase):
    def test_real_tree_passes_with_the_documented_exemption(self):
        self.assertEqual(gate.check(ROOT), [])
        self.assertEqual(set(gate.EXEMPT), {"reference_docs"})


if __name__ == "__main__":
    unittest.main()
