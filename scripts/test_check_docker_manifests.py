"""Offline fail-closed fixtures for the Docker dependency-layer validator.

``scripts/check-docker-manifests.py`` reconstructs the Dockerfile dependency
layer without a daemon; these fixtures prove it fails closed on
dependency-layer drift (missing/extra inputs, unsupported commands, an
incomplete layer, a refused locked fetch) and passes on the pinned tree.
Hermetic: no Docker, no network, no cargo — the locked fetch is stubbed and
the drift cases run against synthetic Dockerfile text and throwaway roots.
"""

import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "check_docker_manifests",
    Path(__file__).with_name("check-docker-manifests.py"))
manifests = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(manifests)

ROOT = Path(__file__).resolve().parents[1]
CHECK_YML = ROOT / ".github/workflows/check.yml"

# Minimal synthetic tree: two package targets plus the root manifest.
SYNTHETIC_MANIFEST = """\
[package]
name = "fixture"
version = "0.1.0"
edition = "2021"

[workspace]
members = ["crates/a"]
"""
SYNTHETIC_MEMBER = """\
[package]
name = "a"
version = "0.1.0"
edition = "2021"
"""
SYNTHETIC_DOCKER = """\
FROM rust:1.98-trixie AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates/a/Cargo.toml crates/a/
RUN mkdir -p src crates/a/src && echo '' > src/lib.rs \\
    && echo '' > crates/a/src/lib.rs && cargo fetch --locked
COPY . .
"""


def make_synthetic_root(parent):
    """A throwaway repo root with manifests but no toolchain needed."""
    root = Path(parent) / "root"
    (root / "crates" / "a").mkdir(parents=True)
    (root / "Cargo.toml").write_text(SYNTHETIC_MANIFEST)
    (root / "Cargo.lock").write_text("version = 4\n")
    (root / "crates" / "a" / "Cargo.toml").write_text(SYNTHETIC_MEMBER)
    return root


def stub_fetch(dest):
    """Stand-in for the locked fetch: proves the layer path, runs nothing."""
    assert (dest / "Cargo.toml").is_file()


class PinnedTreeTests(unittest.TestCase):
    def test_pinned_tree_passes(self):
        docker = (ROOT / "Dockerfile").read_text()
        with tempfile.TemporaryDirectory(prefix="manifest-fixture-") as scratch:
            with patch.dict(os.environ, {"RUNNER_TEMP": scratch}):
                # No cargo on the fixture path: the fetch is stubbed, so this
                # proves the pin, the replay and the completeness check pass
                # on the committed tree with stdlib only.
                manifests.check_tree(docker, ROOT, scratch_dir=scratch,
                                     fetch=stub_fetch)

    def test_pinned_sources_match_dockerfile(self):
        docker = (ROOT / "Dockerfile").read_text()
        self.assertEqual(set(manifests.dependency_sources(docker)),
                         set(manifests.EXPECTED_SOURCES))

    def test_expected_sources_follow_workspace_members(self):
        manifest = tomllib.loads((ROOT / "Cargo.toml").read_text())
        derived = {"Cargo.toml", "Cargo.lock",
                   *(f"{m}/Cargo.toml"
                     for m in manifest["workspace"]["members"])}
        self.assertEqual(set(manifests.EXPECTED_SOURCES), derived)

    def test_refused_locked_fetch_fails_closed(self):
        docker = (ROOT / "Dockerfile").read_text()

        def refused(dest):
            raise subprocess.CalledProcessError(101, ["cargo", "metadata"])

        with tempfile.TemporaryDirectory(prefix="manifest-fixture-") as scratch:
            with self.assertRaises(subprocess.CalledProcessError):
                manifests.check_tree(docker, ROOT, scratch_dir=scratch,
                                     fetch=refused)


class DriftTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.docker = (ROOT / "Dockerfile").read_text()

    def check(self, docker_text):
        with tempfile.TemporaryDirectory(prefix="manifest-fixture-") as scratch:
            manifests.check_tree(docker_text, ROOT, scratch_dir=scratch,
                                 fetch=stub_fetch)

    def test_missing_input_fails_closed(self):
        mutated = "\n".join(
            line for line in self.docker.splitlines()
            if line != "COPY crates/store/Cargo.toml crates/store/")
        self.assertNotEqual(mutated, self.docker)
        with self.assertRaisesRegex(AssertionError, "crates/store/Cargo.toml"):
            self.check(mutated)

    def test_extra_input_fails_closed(self):
        mutated = self.docker.replace(
            "COPY . .", "COPY extra-fixture.txt ./\nCOPY . .", 1)
        with self.assertRaisesRegex(AssertionError, "extra-fixture.txt"):
            self.check(mutated)

    def test_add_instruction_fails_closed(self):
        mutated = self.docker.replace(
            "COPY . .", "ADD rust-toolchain.toml ./\nCOPY . .", 1)
        with self.assertRaisesRegex(AssertionError, "ADD"):
            self.check(mutated)

    def test_lowercase_copy_fails_closed(self):
        mutated = self.docker.replace(
            "COPY . .", "copy rust-toolchain.toml ./\nCOPY . .", 1)
        with self.assertRaisesRegex(AssertionError, "uppercase COPY"):
            self.check(mutated)

    def test_unexpected_workdir_fails_closed(self):
        mutated = self.docker.replace(
            "COPY . .", "WORKDIR /app/sub\nCOPY . .", 1)
        with self.assertRaisesRegex(AssertionError, "WORKDIR"):
            self.check(mutated)

    def test_preamble_copy_fails_closed(self):
        mutated = self.docker.replace(
            "WORKDIR /app", "COPY preamble-fixture.txt ./\nWORKDIR /app", 1)
        self.assertNotEqual(mutated, self.docker)
        with self.assertRaisesRegex(AssertionError, "preamble"):
            self.check(mutated)

    def test_preamble_add_fails_closed(self):
        mutated = self.docker.replace(
            "WORKDIR /app", "ADD preamble-fixture.txt ./\nWORKDIR /app", 1)
        with self.assertRaisesRegex(AssertionError, "unsupported"):
            self.check(mutated)

    def test_preamble_run_fails_closed(self):
        mutated = self.docker.replace(
            "WORKDIR /app", "RUN echo preamble > preamble.txt\nWORKDIR /app", 1)
        with self.assertRaisesRegex(AssertionError, "preamble"):
            self.check(mutated)

    def test_unsupported_command_fails_closed(self):
        mutated = self.docker.replace(
            "COPY . .",
            "RUN curl -fsSL https://example.invalid/setup.sh | sh\nCOPY . .",
            1)
        with self.assertRaisesRegex(AssertionError, "unsupported.*curl"):
            self.check(mutated)


class ReplayTests(unittest.TestCase):
    """Synthetic roots: the replay itself fails closed without the pin."""

    def test_replay_builds_complete_layer(self):
        with tempfile.TemporaryDirectory(prefix="manifest-fixture-") as tmp:
            root = make_synthetic_root(tmp)
            dest = Path(tmp) / "layer"
            dest.mkdir()
            self.assertTrue(manifests.reconstruct(SYNTHETIC_DOCKER, root, dest))
            self.assertEqual(manifests.validate_layer(dest), 2)

    def test_missing_repo_file_fails_closed(self):
        with tempfile.TemporaryDirectory(prefix="manifest-fixture-") as tmp:
            root = make_synthetic_root(tmp)
            (root / "crates" / "a" / "Cargo.toml").unlink()
            dest = Path(tmp) / "layer"
            dest.mkdir()
            # The COPY names a file the checkout does not have.
            with self.assertRaises(OSError):
                manifests.reconstruct(SYNTHETIC_DOCKER, root, dest)

    def test_missing_stub_target_fails_closed(self):
        docker = SYNTHETIC_DOCKER.replace(
            " && echo '' > crates/a/src/lib.rs", "")
        with tempfile.TemporaryDirectory(prefix="manifest-fixture-") as tmp:
            root = make_synthetic_root(tmp)
            dest = Path(tmp) / "layer"
            dest.mkdir()
            self.assertTrue(manifests.reconstruct(docker, root, dest))
            with self.assertRaisesRegex(AssertionError, "crates/a"):
                manifests.validate_layer(dest)

    def test_layer_without_fetch_is_not_checked(self):
        docker = SYNTHETIC_DOCKER.replace(" && cargo fetch --locked", "")
        with tempfile.TemporaryDirectory(prefix="manifest-fixture-") as tmp:
            root = make_synthetic_root(tmp)
            dest = Path(tmp) / "layer"
            dest.mkdir()
            self.assertFalse(manifests.reconstruct(docker, root, dest))


class WorkflowSurfaceTests(unittest.TestCase):
    def test_fixtures_run_in_ci(self):
        self.assertIn("test_check_docker_manifests.py", CHECK_YML.read_text())


if __name__ == "__main__":
    unittest.main()
