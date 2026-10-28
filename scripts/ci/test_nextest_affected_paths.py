"""Offline pins for the nextest lane's affected-paths gating.

The promoted required lane (`check.yml` job `nextest`, name `rust nextest`;
promotion itself lives in companion PR #984, read here read-only and never
edited) runs only when Rust-affecting paths change. Two selectors decide
that, and they must agree on the lane's inputs:

- `dorny/paths-filter` in `ci-speed.yml` (`changes` job): the affected-only
  pre-filter. Its `rust` set must cover every input that
  can change nextest behavior, or the advisory signal and the
  minutes-per-merged-PR report silently stop reflecting the lane.
- `scripts/job-inputs.py` (`rust` output): the authoritative selector the
  required lanes gate on. Every behavioral nextest input must select `rust`
  here, or `ci-ok` accepts a skip on a PR the lane should have gated.

Image-only inputs (`Dockerfile`, `.dockerignore`) are the deliberate
exception: they correctly skip the test lane (the container and supply
lanes cover them) while the advisory filter stays conservative.
"""

import importlib.util
from pathlib import Path
import subprocess
import unittest
from unittest import mock

import yaml

from test_workflows import filter_pattern_matches

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "job_inputs", ROOT / "scripts" / "job-inputs.py")
job_inputs = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(job_inputs)

# Pinned selector action: a moved tag would change what "affected" means.
DORNY_PIN = "dorny/paths-filter@ceb8a2b8f2d89434be7ff52d3de7ec3738c5cc9d"  # v4.0.3

# Explicit preparation produces this one ignored content input. Its renderer,
# materializer and fixture remain tracked behavioral inputs below; no other
# missing path is permitted before preparation.
GENERATED_RUST_CONTENT = "docs/generated/commands.md"
COMMAND_REFERENCE_SOURCES = (
    "scripts/generate_command_reference.py",
    "crates/core/examples/generate_command_reference.rs",
    "crates/core/tests/reference_docs.rs",
    "crates/core/tests/fixtures/reference_settings.json",
)

# Every path here can change what the nextest lane executes: sources, the
# compiled-in fixtures, manifests, the nextest profile, the lane definition
# itself, and the flake-ledger tooling that parses its output. All exist in
# the tree (asserted below) so the pins cannot rot into fiction.
NEXTEST_BEHAVIOR_INPUTS = (
    "crates/bot/src/voice_rooms.rs",
    "crates/core/src/lib.rs",
    "crates/discord/src/pipeline.rs",
    "src/lib.rs",
    "tests/voice_templates/corpus.json",
    "tests/voice_templates/validate.py",
    "sql/web_v1.sql",
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "deny.toml",
    "migrations.lock",
    ".config/nextest.toml",
    "scripts/ci/flake_ledger.py",
    "scripts/ci/test_flake_ledger.py",
    "fuzz/fuzz_targets/vote_kick.rs",
    ".github/workflows/reusable-rust-nextest.yml",
    ".github/workflows/ci-speed.yml",
    ".github/workflows/check.yml",
) + COMMAND_REFERENCE_SOURCES

# Content-read docs and config paths classified as Rust by the authoritative
# selector. Derive these from the selector itself so additions cannot be
# omitted from the advisory-filter coverage pin.
JOB_INPUT_RUST_CONTENT = (
    job_inputs.RUST_DOCS
    | job_inputs.RUST_WORKER_DOCS
    | job_inputs.RUST_WORKER_EXACT
)

# Image-only inputs: the advisory filter lists them under `rust`, but the
# authoritative selector correctly routes them to `supply` alone, so the
# required test lane skips image-only PRs instead of burning a compile.
IMAGE_ONLY_INPUTS = (
    "Dockerfile",
    ".dockerignore",
)

# Docs-only PRs must skip the lane: these match the `docs` filter and no
# `rust` pattern. (Content-read docs such as `docs/parity.md` are excluded
# on purpose: `job-inputs` intentionally selects `rust` for them because the
# Rust preconditions binary reads their content.)
DOCS_ONLY_INPUTS = (
    "docs/runbook.md",
    "docs/container-readiness.md",
    "README.md",
    "CHANGELOG.md",
)


def paths_filter_step():
    """The dorny/paths-filter step and its parsed `filters:` mapping."""
    workflow = yaml.safe_load(
        (ROOT / ".github/workflows/ci-speed.yml").read_text())
    changes = workflow["jobs"]["changes"]
    step = next(step for step in changes["steps"]
                if str(step.get("uses", "")).startswith("dorny/paths-filter@"))
    return step["uses"], yaml.safe_load(step["with"]["filters"])


class NextestAffectedPathsTests(unittest.TestCase):
    def test_paths_filter_action_is_pinned(self):
        uses, _ = paths_filter_step()
        self.assertEqual(uses, DORNY_PIN)

    def test_behavior_inputs_exist(self):
        tracked = set(subprocess.check_output(
            ["git", "ls-files", "-z"], cwd=ROOT).decode().split("\0"))
        self.assertIn(GENERATED_RUST_CONTENT, job_inputs.RUST_DOCS)
        self.assertNotIn(GENERATED_RUST_CONTENT, tracked)
        for path in (NEXTEST_BEHAVIOR_INPUTS + tuple(JOB_INPUT_RUST_CONTENT)
                     + IMAGE_ONLY_INPUTS + DOCS_ONLY_INPUTS):
            with self.subTest(path=path):
                if path != GENERATED_RUST_CONTENT:
                    self.assertTrue((ROOT / path).exists(), f"{path} no longer exists")
                    self.assertIn(path, tracked, f"{path} must remain a tracked input")

    def test_absent_generated_reference_is_allowed_before_preparation(self):
        exists = Path.exists
        with mock.patch.object(Path, "exists", autospec=True, side_effect=lambda path:
                               path != ROOT / GENERATED_RUST_CONTENT and exists(path)):
            result = unittest.TestResult()
            NextestAffectedPathsTests("test_behavior_inputs_exist").run(result)
        self.assertEqual(result.testsRun, 1)
        self.assertEqual(result.errors, [])
        self.assertEqual(result.failures, [])

    def test_missing_authored_or_canonical_input_is_rejected(self):
        exists = Path.exists
        for missing in ("docs/parity.md", *COMMAND_REFERENCE_SOURCES):
            with self.subTest(missing=missing), mock.patch.object(
                    Path, "exists", autospec=True, side_effect=lambda path:
                    path != ROOT / missing and exists(path)):
                result = unittest.TestResult()
                NextestAffectedPathsTests("test_behavior_inputs_exist").run(result)
                self.assertEqual(result.testsRun, 1)
                self.assertEqual(result.errors, [])
                self.assertEqual(len(result.failures), 1)
                self.assertIn(f"{missing} no longer exists", result.failures[0][1])

    def test_rust_filter_covers_nextest_behavior_inputs(self):
        _, filters = paths_filter_step()
        for path in NEXTEST_BEHAVIOR_INPUTS + tuple(JOB_INPUT_RUST_CONTENT):
            with self.subTest(path=path):
                self.assertTrue(
                    any(filter_pattern_matches(pattern, path)
                        for pattern in filters["rust"]),
                    f"{path} matches no rust paths-filter pattern")

    def test_rust_filter_skips_docs_only_inputs(self):
        _, filters = paths_filter_step()
        for path in DOCS_ONLY_INPUTS:
            with self.subTest(path=path):
                self.assertTrue(
                    any(filter_pattern_matches(pattern, path)
                        for pattern in filters["docs"]),
                    f"{path} matches no docs pattern")
                self.assertFalse(
                    any(filter_pattern_matches(pattern, path)
                        for pattern in filters["rust"]),
                    f"{path} unexpectedly selects the rust lane")

    def test_job_inputs_selects_rust_for_nextest_behavior_inputs(self):
        for path in NEXTEST_BEHAVIOR_INPUTS + tuple(JOB_INPUT_RUST_CONTENT):
            with self.subTest(path=path):
                self.assertIn(job_inputs.RUST, job_inputs.classify(path), path)

    def test_job_inputs_routes_image_only_inputs_to_supply_alone(self):
        for path in IMAGE_ONLY_INPUTS:
            with self.subTest(path=path):
                self.assertEqual(job_inputs.classify(path),
                                 frozenset({job_inputs.SUPPLY}), path)


if __name__ == "__main__":
    unittest.main()
