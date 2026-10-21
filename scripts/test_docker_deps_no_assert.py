"""Fail-closed-without-assert regression for the Docker dependency-layer verifier.

``scripts/test-docker-deps.py`` validates the reconstructed dependency layer
with bare ``assert`` (the locked-fetch suffix pin and the per-member
manifest/target checks). ``python -O`` strips asserts into a silent pass, so
a broken layer would print PASS under optimization — the same failure mode
the manifest gate (``scripts/check-docker-manifests.py``) was converted away
from.

These tests pin the fix: no ``assert`` statement remains in the verifier, and
a broken fixture layer fails (nonzero exit, no PASS) both normally and under
``python -O``. The verifier derives its repo root from its own path, so each
case copies the committed script into a throwaway fixture root — the real
Dockerfile is never mutated. Hermetic: stdlib only, no Docker, no network, no
cargo (without ``--cargo`` the fetch suffix is parsed, never executed; the
replay needs only ``sh``, as does the verifier itself)::

    PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts \\
        -p 'test_docker_deps_no_assert.py' -v
"""

import ast
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("test-docker-deps.py")

FIXTURE_MANIFEST = """\
[package]
name = "fixture"
version = "0.1.0"
edition = "2021"

[workspace]
members = ["crates/a"]
"""
FIXTURE_MEMBER = """\
[package]
name = "a"
version = "0.1.0"
edition = "2021"
"""
FIXTURE_DOCKER = """\
FROM rust:1.98-trixie AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates/a/Cargo.toml crates/a/
RUN mkdir -p src crates/a/src && echo '' > src/lib.rs \\
    && echo '' > crates/a/src/lib.rs && cargo fetch --locked
COPY . .
"""
# Same layer, but the member target is never created: the per-member
# target check must refuse it.
BROKEN_TARGET_DOCKER = FIXTURE_DOCKER.replace(
    " && echo '' > crates/a/src/lib.rs", "")
# Same layer, but the member manifest is never copied: the per-member
# manifest check must refuse it.
BROKEN_MANIFEST_DOCKER = FIXTURE_DOCKER.replace(
    "COPY crates/a/Cargo.toml crates/a/\n", "")
# Same layer, but the RUN no longer ends in the pinned locked-fetch suffix:
# the suffix pin must refuse it before running anything.
BROKEN_SUFFIX_DOCKER = FIXTURE_DOCKER.replace(
    " && cargo fetch --locked", " && true")


def make_fixture_root(parent, docker_text):
    """A throwaway repo root running the committed verifier against one layer."""
    root = Path(parent) / "root"
    scripts = root / "scripts"
    scripts.mkdir(parents=True)
    shutil.copy2(SCRIPT, scripts / "test-docker-deps.py")
    (root / "Dockerfile").write_text(docker_text)
    (root / "Cargo.toml").write_text(FIXTURE_MANIFEST)
    (root / "Cargo.lock").write_text("version = 4\n")
    (root / "crates" / "a").mkdir(parents=True)
    (root / "crates" / "a" / "Cargo.toml").write_text(FIXTURE_MEMBER)
    return root


def run_verifier(root, *extra_args):
    """Run the fixture verifier, normally or under ``python -O``."""
    scratch = root / "scratch"
    scratch.mkdir(exist_ok=True)
    env = os.environ | {"PAPERCLIP_RUN_SCRATCH_DIR": str(scratch)}
    return subprocess.run(
        [sys.executable, *extra_args,
         str(root / "scripts" / "test-docker-deps.py")],
        capture_output=True, text=True, timeout=120, env=env)


class NoAssertTests(unittest.TestCase):
    def test_no_assert_remains_in_verifier(self):
        tree = ast.parse(SCRIPT.read_text())
        asserts = sorted(
            node.lineno for node in ast.walk(tree)
            if isinstance(node, ast.Assert))
        self.assertEqual(
            asserts, [],
            f"assert statements remain on validation paths: {asserts}")


class BrokenLayerTests(unittest.TestCase):
    """A broken layer fails closed, normally and under ``python -O``."""

    def check_broken(self, docker_text, *extra_args):
        with tempfile.TemporaryDirectory(prefix="docker-deps-no-assert-") as tmp:
            root = make_fixture_root(tmp, docker_text)
            proc = run_verifier(root, *extra_args)
        self.assertNotEqual(
            proc.returncode, 0,
            "broken layer passed silently "
            f"(stdout={proc.stdout!r} stderr={proc.stderr[-2000:]!r})")
        self.assertNotIn("PASS", proc.stdout)
        return proc

    def test_missing_target_refuses(self):
        proc = self.check_broken(BROKEN_TARGET_DOCKER)
        self.assertIn("Missing dependency-layer target: crates/a", proc.stderr)

    def test_missing_target_refuses_under_optimization(self):
        # RED before the fix: -O strips the asserts and the broken layer
        # prints PASS.
        proc = self.check_broken(BROKEN_TARGET_DOCKER, "-O")
        self.assertIn("Missing dependency-layer target: crates/a", proc.stderr)

    def test_missing_manifest_refuses(self):
        proc = self.check_broken(BROKEN_MANIFEST_DOCKER)
        self.assertIn("Refusal: crates/a", proc.stderr)

    def test_missing_manifest_refuses_under_optimization(self):
        # RED on a neutralised manifest require: -O or not, the mutant
        # prints PASS for the missing manifest.
        proc = self.check_broken(BROKEN_MANIFEST_DOCKER, "-O")
        self.assertIn("Refusal: crates/a", proc.stderr)

    def test_unpinned_suffix_refuses(self):
        self.check_broken(BROKEN_SUFFIX_DOCKER)

    def test_unpinned_suffix_refuses_under_optimization(self):
        # RED before the fix: -O strips the suffix pin and the replay runs
        # an unpinned command, then prints PASS.
        self.check_broken(BROKEN_SUFFIX_DOCKER, "-O")


class PinnedLayerTests(unittest.TestCase):
    """Control: the valid layer passes, normally and under ``python -O``."""

    def check_valid(self, *extra_args):
        with tempfile.TemporaryDirectory(prefix="docker-deps-no-assert-") as tmp:
            root = make_fixture_root(tmp, FIXTURE_DOCKER)
            proc = run_verifier(root, *extra_args)
        self.assertEqual(proc.returncode, 0, msg=proc.stderr[-2000:])
        self.assertIn("PASS", proc.stdout)

    def test_valid_layer_passes(self):
        self.check_valid()

    def test_valid_layer_passes_under_optimization(self):
        self.check_valid("-O")


if __name__ == "__main__":
    unittest.main()
