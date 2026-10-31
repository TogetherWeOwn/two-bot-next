"""Fail-closed-without-assert acceptance for the Docker dependency-layer gate.

Protected paired-acceptance battery for ``scripts/check-docker-manifests.py``.
Every validation path must refuse with fixed vocabulary instead of
``assert``/``AssertionError`` (which ``python -O`` strips into a silent pass),
a missing scratch env must refuse without ``KeyError``, and an optimized run
must still fail closed.

Contract the gate must satisfy (the builder implements to this file):

* ``Refusal``: an exception defined by the gate module, a subclass of
  ``Exception`` but of neither ``AssertionError`` nor ``KeyError``.
* ``require(condition, message)``: a helper raising ``Refusal`` when the
  condition is false, returning ``None`` when true.
* Every current validation message keeps its diagnostic vocabulary (the
  substrings pinned below), so any refusal maps to its next action.
* No ``assert`` statement and no ``AssertionError`` raise remain in the gate.
* ``main()`` resolves ``PAPERCLIP_RUN_SCRATCH_DIR`` then ``RUNNER_TEMP`` and
  raises ``Refusal`` when neither is set, before any gate work.

Failing-first: every refusal test fails on the pre-fix gate (``AssertionError``
or ``KeyError`` where ``Refusal`` is required; a ``FileNotFoundError`` crash
or a silent pass where an optimized refusal is required) except the single
in-process pinned-tree control, which passes before and after.
Run offline with stdlib only, no Docker, no network, no cargo::

    PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts \\
        -p 'test_check_docker_manifests_no_assert_acceptance.py' -v

Protected file: the paired builder builds on this head and must never edit
this file. The pre-existing ``test_check_docker_manifests.py`` is NOT
protected: its Drift/Replay ``AssertionError`` expectations must be migrated
to ``Refusal`` as part of the conversion.
"""

import ast
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).with_name("check-docker-manifests.py")
SPEC = importlib.util.spec_from_file_location(
    "check_docker_manifests_no_assert", SCRIPT)
manifests = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(manifests)

ROOT = Path(__file__).resolve().parents[1]

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

# Driver for the optimized-interpreter proof: no asserts of its own, so it
# behaves identically under -O; the gate under test must refuse drifted input
# with a nonzero exit instead of passing with its checks compiled out.
OPTIMIZED_DRIVER = """\
import importlib.util
import sys

script_path, docker_path, scratch_path = sys.argv[1], sys.argv[2], sys.argv[3]
spec = importlib.util.spec_from_file_location(
    "check_docker_manifests_optimized", script_path)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
docker_text = open(docker_path).read()
module.check_tree(
    docker_text, module.ROOT, scratch_dir=scratch_path,
    fetch=lambda dest: None)
"""

# Unit-level -O driver: a single input-set check with a missing member must
# refuse under optimization instead of returning silently.
OPTIMIZED_UNIT_DRIVER = """\
import importlib.util
import sys

spec = importlib.util.spec_from_file_location(
    "check_docker_manifests_optimized_unit", sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
module.check_sources(["Cargo.toml"], root=module.ROOT)
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
    if not (dest / "Cargo.toml").is_file():
        raise RuntimeError("acceptance stub reached without a replayed layer")


def refusal_type(test_case):
    """The gate's fixed-vocabulary refusal, or a clean failure naming it."""
    refusal = getattr(manifests, "Refusal", None)
    test_case.assertIsNotNone(
        refusal, "check-docker-manifests.py must define Refusal")
    return refusal


def run_optimized(docker_text):
    """Run the gate under `python -O` against one Dockerfile text."""
    with tempfile.TemporaryDirectory(prefix="manifest-no-assert-") as tmp:
        docker_file = Path(tmp) / "Dockerfile.drift"
        docker_file.write_text(docker_text)
        scratch = Path(tmp) / "scratch"
        scratch.mkdir()
        return subprocess.run(
            [sys.executable, "-O", "-c", OPTIMIZED_DRIVER,
             str(SCRIPT), str(docker_file), str(scratch)],
            capture_output=True, text=True, timeout=120)


def run_optimized_unit():
    """Run one input-set check under `python -O`; it must refuse."""
    return subprocess.run(
        [sys.executable, "-O", "-c", OPTIMIZED_UNIT_DRIVER, str(SCRIPT)],
        capture_output=True, text=True, timeout=120)


class ContractTests(unittest.TestCase):
    def test_refusal_vocabulary_exists(self):
        refusal = refusal_type(self)
        self.assertTrue(issubclass(refusal, Exception))

    def test_refusal_is_neither_assert_nor_key_error(self):
        refusal = refusal_type(self)
        self.assertFalse(
            issubclass(refusal, AssertionError),
            "Refusal must not be an AssertionError: -O strips asserts")
        self.assertFalse(
            issubclass(refusal, KeyError),
            "Refusal must not be a KeyError: missing env refuses, not crashes")

    def test_require_helper_refuses(self):
        refusal = refusal_type(self)
        require = getattr(manifests, "require", None)
        self.assertIsNotNone(
            require, "check-docker-manifests.py must define require()")
        with self.assertRaises(refusal):
            require(False, "probe refusal")
        self.assertIsNone(require(True, "probe pass"))

    def test_no_assert_or_assertion_error_remains(self):
        tree = ast.parse(SCRIPT.read_text())
        asserts = sorted(
            node.lineno for node in ast.walk(tree)
            if isinstance(node, ast.Assert))
        assertion_raises = sorted(
            node.lineno for node in ast.walk(tree)
            if isinstance(node, ast.Raise)
            and _raises_assertion_error(node))
        self.assertEqual(
            asserts, [],
            f"assert statements remain on validation paths: {asserts}")
        self.assertEqual(
            assertion_raises, [],
            f"AssertionError raises remain: {assertion_raises}")


def _raises_assertion_error(node):
    exc = node.exc
    if isinstance(exc, ast.Name):
        return exc.id == "AssertionError"
    return (isinstance(exc, ast.Call)
            and isinstance(exc.func, ast.Name)
            and exc.func.id == "AssertionError")


class InstructionRefusalTests(unittest.TestCase):
    """Every preamble/layer instruction check refuses with fixed vocabulary."""

    @classmethod
    def setUpClass(cls):
        cls.docker = (ROOT / "Dockerfile").read_text()

    def check(self, docker_text):
        with tempfile.TemporaryDirectory(prefix="manifest-fixture-") as scratch:
            manifests.check_tree(docker_text, ROOT, scratch_dir=scratch,
                                 fetch=stub_fetch)

    def test_lowercase_copy_refuses(self):
        mutated = self.docker.replace(
            "COPY . .", "copy rust-toolchain.toml ./\nCOPY . .", 1)
        with self.assertRaisesRegex(refusal_type(self), "uppercase COPY"):
            self.check(mutated)

    def test_preamble_copy_refuses(self):
        mutated = self.docker.replace(
            "WORKDIR /app", "COPY preamble-fixture.txt ./\nWORKDIR /app", 1)
        self.assertNotEqual(mutated, self.docker)
        with self.assertRaisesRegex(refusal_type(self), "preamble"):
            self.check(mutated)

    def test_lowercase_run_refuses(self):
        mutated = self.docker.replace("RUN mkdir", "run mkdir", 1)
        self.assertNotEqual(mutated, self.docker)
        with self.assertRaisesRegex(refusal_type(self), "uppercase RUN"):
            self.check(mutated)

    def test_preamble_run_refuses(self):
        mutated = self.docker.replace(
            "WORKDIR /app", "RUN echo preamble > preamble.txt\nWORKDIR /app", 1)
        with self.assertRaisesRegex(refusal_type(self), "preamble"):
            self.check(mutated)

    def test_lowercase_from_refuses(self):
        mutated = self.docker.replace("FROM rust", "from rust", 1)
        self.assertNotEqual(mutated, self.docker)
        with self.assertRaisesRegex(refusal_type(self), "uppercase FROM"):
            self.check(mutated)

    def test_second_from_refuses(self):
        mutated = self.docker.replace(
            "COPY . .", "FROM rust:1.98-trixie AS second\nCOPY . .", 1)
        with self.assertRaisesRegex(
                refusal_type(self), "unexpected dependency-layer preamble"):
            self.check(mutated)

    def test_unexpected_workdir_refuses(self):
        mutated = self.docker.replace(
            "COPY . .", "WORKDIR /app/sub\nCOPY . .", 1)
        with self.assertRaisesRegex(refusal_type(self), "WORKDIR"):
            self.check(mutated)

    def test_preamble_add_refuses(self):
        mutated = self.docker.replace(
            "WORKDIR /app", "ADD preamble-fixture.txt ./\nWORKDIR /app", 1)
        with self.assertRaisesRegex(
                refusal_type(self), "unsupported dependency-layer instruction"):
            self.check(mutated)

    def test_add_instruction_refuses(self):
        mutated = self.docker.replace(
            "COPY . .", "ADD rust-toolchain.toml ./\nCOPY . .", 1)
        with self.assertRaisesRegex(
                refusal_type(self), "unsupported dependency-layer instruction"):
            self.check(mutated)

    def test_unsupported_command_refuses(self):
        mutated = self.docker.replace(
            "COPY . .",
            "RUN curl -fsSL https://example.invalid/setup.sh | sh\nCOPY . .",
            1)
        with self.assertRaisesRegex(
                refusal_type(self), "unsupported dependency-layer command"):
            self.check(mutated)


class SourceRefusalTests(unittest.TestCase):
    """Input-set drift refuses with fixed vocabulary."""

    @classmethod
    def setUpClass(cls):
        cls.docker = (ROOT / "Dockerfile").read_text()

    def check(self, docker_text):
        with tempfile.TemporaryDirectory(prefix="manifest-fixture-") as scratch:
            manifests.check_tree(docker_text, ROOT, scratch_dir=scratch,
                                 fetch=stub_fetch)

    def test_missing_input_refuses(self):
        mutated = "\n".join(
            line for line in self.docker.splitlines()
            if line != "COPY crates/store/Cargo.toml crates/store/")
        self.assertNotEqual(mutated, self.docker)
        with self.assertRaisesRegex(
                refusal_type(self), r"lost pinned inputs.*crates/store"):
            self.check(mutated)

    def test_extra_input_refuses(self):
        mutated = self.docker.replace(
            "COPY . .", "COPY extra-fixture.txt ./\nCOPY . .", 1)
        with self.assertRaisesRegex(
                refusal_type(self),
                r"gained unpinned inputs.*extra-fixture\.txt"):
            self.check(mutated)

    def test_check_sources_missing_refuses(self):
        with self.assertRaisesRegex(
                refusal_type(self), "lost pinned inputs"):
            manifests.check_sources(["Cargo.toml"], root=ROOT)

    def test_check_sources_extra_refuses(self):
        with self.assertRaisesRegex(
                refusal_type(self), "gained unpinned inputs"):
            manifests.check_sources(
                [*manifests.EXPECTED_SOURCES, "extra-fixture.txt"], root=ROOT)


class LayerRefusalTests(unittest.TestCase):
    """Replay completeness and the fetch proof refuse with fixed vocabulary."""

    def test_missing_lock_refuses(self):
        with tempfile.TemporaryDirectory(prefix="manifest-fixture-") as tmp:
            root = make_synthetic_root(tmp)
            dest = Path(tmp) / "layer"
            dest.mkdir()
            self.assertTrue(manifests.reconstruct(SYNTHETIC_DOCKER, root, dest))
            (dest / "Cargo.lock").unlink()
            with self.assertRaisesRegex(
                    refusal_type(self), "Cargo.lock missing"):
                manifests.validate_layer(dest)

    def test_missing_member_manifest_refuses(self):
        with tempfile.TemporaryDirectory(prefix="manifest-fixture-") as tmp:
            root = make_synthetic_root(tmp)
            dest = Path(tmp) / "layer"
            dest.mkdir()
            self.assertTrue(manifests.reconstruct(SYNTHETIC_DOCKER, root, dest))
            (dest / "crates" / "a" / "Cargo.toml").unlink()
            with self.assertRaisesRegex(
                    refusal_type(self), "Missing dependency-layer manifest"):
                manifests.validate_layer(dest)

    def test_missing_member_target_refuses(self):
        docker = SYNTHETIC_DOCKER.replace(
            " && echo '' > crates/a/src/lib.rs", "")
        with tempfile.TemporaryDirectory(prefix="manifest-fixture-") as tmp:
            root = make_synthetic_root(tmp)
            dest = Path(tmp) / "layer"
            dest.mkdir()
            self.assertTrue(manifests.reconstruct(docker, root, dest))
            with self.assertRaisesRegex(
                    refusal_type(self),
                    r"Missing dependency-layer target.*crates/a"):
                manifests.validate_layer(dest)

    def test_no_copy_sources_refuses(self):
        docker = ("FROM rust:1.98-trixie AS builder\n"
                  "WORKDIR /app\n"
                  "COPY . .\n")
        with self.assertRaisesRegex(
                refusal_type(self), "no dependency-layer COPY sources found"):
            manifests.check_tree(docker, ROOT, fetch=stub_fetch)

    def test_unchecked_fetch_refuses(self):
        docker = SYNTHETIC_DOCKER.replace(" && cargo fetch --locked", "")
        with tempfile.TemporaryDirectory(prefix="manifest-fixture-") as tmp:
            root = make_synthetic_root(tmp)
            with self.assertRaisesRegex(
                    refusal_type(self),
                    "dependency fetch layer was not checked"):
                manifests.check_tree(docker, root, scratch_dir=tmp,
                                     fetch=stub_fetch)


class ScratchEnvRefusalTests(unittest.TestCase):
    def test_missing_scratch_env_refuses_without_key_error(self):
        refusal = refusal_type(self)
        with patch.dict(os.environ):
            os.environ.pop("PAPERCLIP_RUN_SCRATCH_DIR", None)
            os.environ.pop("RUNNER_TEMP", None)
            with self.assertRaises(refusal) as raised:
                manifests.main()
        self.assertTrue(
            str(raised.exception),
            "the scratch-env refusal must name its next action")


class OptimizedRunTests(unittest.TestCase):
    """The gate fails closed under `python -O`, when asserts are stripped."""

    @classmethod
    def setUpClass(cls):
        cls.docker = (ROOT / "Dockerfile").read_text()

    def test_optimized_run_still_refuses_drift(self):
        # RED today: the replay call sits inside its own assert, so -O
        # strips the replay too and the run crashes with FileNotFoundError
        # instead of refusing with fixed vocabulary.
        mutated = "\n".join(
            line for line in self.docker.splitlines()
            if line != "COPY crates/store/Cargo.toml crates/store/")
        self.assertNotEqual(mutated, self.docker)
        proc = run_optimized(mutated)
        self.assertNotEqual(
            proc.returncode, 0,
            "optimized gate passed drifted manifests silently "
            f"(stdout={proc.stdout!r} stderr={proc.stderr!r})")
        self.assertIn("Refusal", proc.stderr)

    def test_optimized_unit_check_still_refuses(self):
        # RED today: a bare input-set check silently passes under -O.
        proc = run_optimized_unit()
        self.assertNotEqual(
            proc.returncode, 0,
            "optimized input-set check passed silently "
            f"(stdout={proc.stdout!r} stderr={proc.stderr!r})")
        self.assertIn("Refusal", proc.stderr)

    def test_optimized_run_passes_pinned_tree(self):
        # RED today: the stripped replay crashes even the valid tree with
        # FileNotFoundError. After the fix the checks actually run, so the
        # pinned tree passes under -O exactly as it does unoptimized.
        proc = run_optimized(self.docker)
        self.assertEqual(proc.returncode, 0, msg=proc.stderr[-2000:])

    def test_pinned_tree_passes_without_refusal(self):
        # Control: the committed tree passes in-process, before and after.
        with tempfile.TemporaryDirectory(prefix="manifest-fixture-") as scratch:
            manifests.check_tree(self.docker, ROOT, scratch_dir=scratch,
                                 fetch=stub_fetch)


if __name__ == "__main__":
    unittest.main()
