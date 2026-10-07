"""Offline fixtures for the container-smoke selector; no Docker or GitHub needed.

Mirrors the `scripts/test_container_smoke.py` pattern: pure decision logic
tested hermetically. The workflow-yaml surface (selector job wiring, step
gating, push-always-build) is asserted by parsing check.yml as text, the
same stdlib-only technique as `scripts/test-secret-scan.py`. The one diff test
runs the real git binary on a throwaway repo and skips without it.
"""

import importlib.util
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "container_inputs", Path(__file__).with_name("container-inputs.py"))
inputs = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(inputs)

ROOT = Path(__file__).resolve().parents[1]
CHECK_YML = ROOT / ".github/workflows/check.yml"


class SelectionTests(unittest.TestCase):
    def test_image_sources_always_build(self):
        build = [
            "Dockerfile", ".dockerignore", "Cargo.toml", "Cargo.lock",
            "rust-toolchain.toml", "deny.toml", "migrations.lock",
            "wrangler/wrangler.toml",
            "crates/core/src/lib.rs", "crates/bot/src/main.rs",
            "crates/store/src/migrations.rs", "crates/cutover/build.rs",
            "src/lib.rs",
            "crates/store/migrations/0400_initial.sql",
            "crates/cutover/migrations/0001_funnel.sql",
            "crates/store/sql/web_v1.sql",
            "sql/database_role_matrix.sql",
            "sql/database_roles.sql",
            "sql/verify_database_roles.sql",
            "crates/core/tests/feeds.rs",
            "crates/cutover/tests/settings_db.rs",
            "crates/core/tests/fixtures/legacy_registry.json",
            "crates/store/build.rs",
            ".github/workflows/check.yml",
            ".github/workflows/nightly.yml",
            ".github/workflows/release.yml",
            ".github/workflows/deploy-staging.yml",
            ".github/workflows/secret-scan.yml",
            ".github/workflows/pr-lint.yml",
            "scripts/container-smoke.py",
            "scripts/container-inputs.py",
            "scripts/check-docker-manifests.py",
            "scripts/test-docker-deps.py",
            "scripts/test_check_docker_manifests.py",
            "scripts/test_container_smoke.py",
            "scripts/test-logging-container.py",
            "scripts/test_container_inputs.py",
        ]
        for path in build:
            with self.subTest(path=path):
                self.assertTrue(inputs.is_image_input(path), path)
                self.assertTrue(inputs.needs_image_build([path]), path)

    def test_non_image_changes_skip(self):
        skip = [
            "docs/voice-rooms.md", "docs/runbook.md", "docs/metrics.md",
            "deploy/two-bot-next-backup.service",
            "tests/voice_templates/validate.py",
            "tests/voice_templates/naming_golden.json",
            "wrangler/src/index.ts", "wrangler/src/alert-rules.ts",
            "wrangler/test/alert-rules.test.ts",
            "wrangler/test/container.test.ts",
            ".github/pull_request_template.md",
            ".github/CODEOWNERS", ".github/dependabot.yml",
            "LICENSE", "CHANGELOG.md", ".editorconfig", ".gitignore",
            ".gitleaks.toml", ".gitleaksignore",
            "release-please-config.json", ".release-please-manifest.json",
            "README.md", "AGENTS.md", "CONTEXT.md",
            "scripts/release-pr-state.cjs",
            "scripts/test-release-retry.py",
            "scripts/test-secret-scan.py",
            "scripts/check-migrations.py",
        ]
        for path in skip:
            with self.subTest(path=path):
                self.assertFalse(inputs.is_image_input(path), path)
        self.assertFalse(inputs.needs_image_build(skip))

    def test_mixed_diff_builds(self):
        self.assertTrue(inputs.needs_image_build(
            ["docs/runbook.md", "crates/core/src/lib.rs"]))

    def test_empty_diff_skips(self):
        self.assertFalse(inputs.needs_image_build([]))

    def test_unrecognized_paths_fail_closed(self):
        for path in ["brand-new-dir/thing.txt", "Dockerfile.new",
                     "scripts", "crates", "wrangler", ".gitattributes",
                     ".github/UNKNOWN"]:
            with self.subTest(path=path):
                self.assertTrue(inputs.is_image_input(path), path)

    def test_new_root_markdown_skips(self):
        # Markdown is never compiled into the image; a brand-new root doc
        # is safe to skip (unlike a brand-new root *directory*, which
        # builds via the fail-closed default).
        self.assertFalse(inputs.is_image_input("PACKAGES.md"))

    def test_root_rs_files_are_checked_not_skipped(self):
        # A Rust file at the repo root is outside crates//src/; the
        # fail-closed default owns it. This test pins the current
        # classification, not a claim that such files exist.
        self.assertTrue(inputs.is_image_input("odd-root-file.rs"))

    def test_every_dockerfile_dependency_source_is_an_image_input(self):
        sources = inputs.dockerfile_dependency_sources()
        self.assertTrue(sources, "expected COPY sources before COPY . .")
        self.assertIn("Cargo.toml", sources)
        self.assertIn("Cargo.lock", sources)
        self.assertIn("crates/core/Cargo.toml", sources)
        for source in sources:
            with self.subTest(source=source):
                self.assertTrue(inputs.is_image_input(source), source)

    def test_dockerfile_dependency_sources_match_manifest_checker(self):
        # Same region the manifest checker replays: everything before the
        # real-sources COPY. If the Dockerfile grows a second COPY layer,
        # both parsers must move together.
        docker = (ROOT / "Dockerfile").read_text()
        self.assertEqual(docker.count("COPY . ."), 1)
        layer, _rest = docker.split("COPY . .", 1)
        line_sources = [source
                        for line in layer.replace("\\\n", " ").splitlines()
                        if line.startswith("COPY ")
                        for source in __import__("shlex").split(line)[1:-1]]
        self.assertEqual(inputs.dockerfile_dependency_sources(), line_sources)

    def test_cli_prints_true_on_image_change(self):
        with patch.object(inputs, "changed_files",
                          return_value=["crates/core/src/lib.rs"]):
            with patch("builtins.print") as printed:
                self.assertEqual(
                    inputs.main(["--base-ref", "a", "--head-ref", "b"]), 0)
                printed.assert_called_once_with("true")

    def test_cli_prints_false_on_docs_only(self):
        with patch.object(inputs, "changed_files",
                          return_value=["docs/runbook.md"]):
            with patch("builtins.print") as printed:
                self.assertEqual(
                    inputs.main(["--base-ref", "a", "--head-ref", "b"]), 0)
                printed.assert_called_once_with("false")

    def test_cli_fails_closed_when_diff_is_undecidable(self):
        import subprocess
        with patch.object(inputs, "changed_files",
                          side_effect=subprocess.CalledProcessError(128, "git")):
            with patch("builtins.print"):
                self.assertEqual(
                    inputs.main(["--base-ref", "a", "--head-ref", "b"]), 2)


class ChangedFilesTests(unittest.TestCase):
    """`changed_files` against a throwaway repo; needs only the git binary."""

    @staticmethod
    def git(root, *args):
        subprocess.run(
            ["git", "-C", str(root), "-c", "commit.gpgsign=false", *args],
            check=True, capture_output=True)

    @unittest.skipUnless(shutil.which("git"), "git binary required")
    def test_rename_into_docs_lists_both_paths(self):
        # With rename detection, `git mv src/lib.rs docs/lib.md` lists only the
        # destination, the diff looks docs-only and the image build is skipped.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.git(root, "init", "-q")
            self.git(root, "config", "user.email", "ci@example.invalid")
            self.git(root, "config", "user.name", "ci")
            # Rename detection on, whatever the host's global git config says.
            self.git(root, "config", "diff.renames", "true")
            (root / "src").mkdir()
            (root / "docs").mkdir()
            (root / "src" / "lib.rs").write_text("pub fn one() -> u32 {\n    1\n}\n" * 4)
            self.git(root, "add", "-A")
            self.git(root, "commit", "-q", "-m", "base")
            self.git(root, "mv", "src/lib.rs", "docs/lib.md")
            self.git(root, "commit", "-q", "-m", "move")
            changed = inputs.changed_files("HEAD~1", "HEAD", root=root)
            self.assertCountEqual(changed, ["src/lib.rs", "docs/lib.md"])
            self.assertTrue(inputs.needs_image_build(changed))


class WorkflowSurfaceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = CHECK_YML.read_text()
        cls.on_block = cls.text.split("\njobs:")[0]
        cls.selector = cls.text.split("  container-inputs:")[1].split("\n  container:")[0]
        cls.container = cls.text.split("\n  container:")[1].split("\n  community-db:")[0]

    def test_no_workflow_level_paths_filter(self):
        # A `paths:` filter under `on.pull_request` would skip the whole
        # workflow (including required checks). Selection is per-job.
        self.assertNotIn("paths:", self.on_block)

    def test_selector_job_exists_and_needs_nothing(self):
        self.assertRegex(self.text, r"(?m)^  container-inputs:\s*$")
        self.assertNotIn("needs:", self.selector.split("steps:")[0],
                         "selector must run without waiting on the test jobs")

    def test_selector_runs_before_container_job(self):
        self.assertLess(self.text.index("  container-inputs:"),
                        self.text.index("\n  container:"))

    def test_selector_exposes_build_output(self):
        self.assertRegex(self.selector, r"(?m)^\s+outputs:\s*$")
        self.assertIn("build:", self.selector)

    def test_selector_checks_out_full_history(self):
        # The base/head SHAs must resolve locally for the diff.
        self.assertIn("fetch-depth: 0", self.selector)

    def test_selector_runs_the_classifier(self):
        self.assertIn("scripts/container-inputs.py", self.selector)
        self.assertIn("pull_request", self.selector)
        self.assertIn("base.sha", self.selector)
        self.assertIn("head.sha", self.selector)

    def test_selector_defaults_to_build(self):
        # Fail-closed end to end: undecidable diffs, missing SHAs and
        # non-PR events all select a full build in the shell step, so a
        # classifier exit code can never silently skip the image.
        self.assertIn("build=true", self.selector)
        self.assertIn("github.event_name", self.selector)

    def test_selector_regressions_run_in_ci(self):
        self.assertIn("test_container_inputs.py", self.selector)

    def test_container_job_waits_on_selector(self):
        self.assertIn("container-inputs", self.container.split("steps:")[0])

    def test_heavy_steps_are_gated_on_build_output(self):
        gated = [line for line in self.container.splitlines()
                 if "container-inputs.outputs.build" in line
                 and "== 'true'" in line]
        # BuildKit, image build, smoke gate, negative proof: all gated.
        self.assertGreaterEqual(len(gated), 4)

    def test_checkout_and_offline_tests_always_run(self):
        head, steps = self.container.split("steps:", 1)
        self.assertNotRegex(head, r"(?m)^\s+if:\s",
                            "job-level if would hide the check; steps gate instead")
        first_gated = min(self.container.index("container-inputs.outputs.build"),
                          len(self.container))
        preamble = self.container[:first_gated]
        self.assertIn("actions/checkout", preamble)
        self.assertIn("test_container_smoke.py", preamble)

    def test_skip_is_announced(self):
        self.assertIn("container-inputs.outputs.build", self.container)
        self.assertIn("!= 'true'", self.container)

    def test_push_to_main_always_builds(self):
        self.assertIn("github.event_name", self.selector)
        self.assertIn("pull_request", self.selector)
        self.assertIn("build=true", self.selector)

    def test_cache_scope_is_unchanged(self):
        self.assertIn("type=gha,scope=two-bot-runtime-amd64", self.text)


if __name__ == "__main__":
    unittest.main()
