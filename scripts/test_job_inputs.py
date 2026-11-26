"""Offline fixtures for the check.yml job selector; no Docker, git or GitHub needed.

Mirrors the `scripts/test_container_inputs.py` pattern: pure decision logic
tested hermetically, plus workflow-yaml surface assertions parsed as text
(the same stdlib-only technique as `scripts/test-secret-scan.py`).

Every coupling claim in `scripts/job-inputs.py` is pinned by a test here,
so a drift in readers (new doc file read by Rust, new worker coupling) fails
this suite before it can silently skip coverage.
"""

import importlib.util
import os
from pathlib import Path
import re
import runpy
import subprocess
import tempfile
import textwrap
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "job_inputs", Path(__file__).with_name("job-inputs.py"))
inputs = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(inputs)

ROOT = Path(__file__).resolve().parents[1]
CHECK_YML = ROOT / ".github/workflows/check.yml"
DOCS_PREFIX = inputs.DOCS_PREFIX
RUST, WORKER, PARITY = inputs.RUST, inputs.WORKER, inputs.PARITY
SUPPLY, DOCS = inputs.SUPPLY, inputs.DOCS


def jobs(paths):
    return inputs.selection(paths)


class SelectionTests(unittest.TestCase):
    def test_gate_wiring_files_run_everything(self):
        for path in [".github/workflows/check.yml", "Cargo.toml",
                     "Cargo.lock", "rust-toolchain.toml", "deny.toml",
                     "migrations.lock"]:
            with self.subTest(path=path):
                self.assertEqual(inputs.classify(path),
                                 {RUST, WORKER, PARITY, SUPPLY}, path)

    def test_selector_files_run_everything(self):
        # CI standard rule 4 (TOG-14881): the change-detection filter
        # itself revalidates everything rather than risk a stale gate.
        for path in ["scripts/job-inputs.py",
                     "scripts/container-inputs.py",
                     "scripts/test_job_inputs.py",
                     "scripts/test_container_inputs.py"]:
            with self.subTest(path=path):
                self.assertEqual(inputs.classify(path),
                                 {RUST, WORKER, PARITY, SUPPLY}, path)

    def test_github_workflows_run_everything(self):
        # CI standard rule 4 (TOG-14881): `.github/**` edits change what
        # every other gate means, so they revalidate everything -- not just
        # the worker job's offline verifications.
        for path in [".github/workflows/supply-chain.yml",
                     ".github/workflows/release.yml",
                     ".github/workflows/deploy-production.yml",
                     ".github/workflows/deploy-staging.yml",
                     ".github/workflows/nightly.yml"]:
            with self.subTest(path=path):
                self.assertEqual(inputs.classify(path),
                                 {RUST, WORKER, PARITY, SUPPLY}, path)

    def test_image_inputs_select_supply_only(self):
        # The container selector owns the image build and the check job's
        # offline manifest step always runs, so no test job is needed -- but
        # the SBOM/vulnerability scan inventories exactly these files.
        for path in ["Dockerfile", ".dockerignore"]:
            with self.subTest(path=path):
                self.assertEqual(inputs.classify(path), {SUPPLY}, path)
        self.assertEqual(
            jobs(["Dockerfile"]),
            {RUST: False, WORKER: False, PARITY: False,
             SUPPLY: True, DOCS: False})

    def test_crate_changes_run_rust_and_worker(self):
        rust = ["crates/core/src/lib.rs", "crates/bot/src/main.rs",
                "crates/store/src/web.rs", "crates/cutover/build.rs",
                "crates/store/migrations/0400_initial.sql",
                "crates/core/tests/feeds.rs",
                "crates/core/tests/fixtures/feeds/rss.xml",
                "crates/store/build.rs", "src/lib.rs"]
        for path in rust:
            with self.subTest(path=path):
                selected = inputs.classify(path)
                self.assertIn(RUST, selected, path)
                self.assertIn(WORKER, selected, path)
                self.assertNotIn(PARITY, selected, path)

    def test_legacy_registry_fixture_also_gates_parity(self):
        path = "crates/core/tests/fixtures/legacy_registry.json"
        self.assertEqual(inputs.classify(path), {RUST, WORKER, PARITY})

    def test_rust_only_inputs(self):
        for path in ["sql/web_v1.sql", "sql/database_roles.sql",
                     "tests/voice_templates/corpus.json",
                     "tests/voice_templates/validate.py"]:
            with self.subTest(path=path):
                self.assertEqual(inputs.classify(path), {RUST}, path)

    def test_fuzz_workspace_inputs_run_everything(self):
        # The separate cargo-fuzz workspace resolves outside the root
        # lockfile, so any of its inputs runs the full matrix (including the
        # locked fuzz-compile gate) rather than silently skipping it. The
        # fuzz README stays docs-only: it selects no job.
        for path in ["fuzz/Cargo.toml", "fuzz/Cargo.lock",
                     "fuzz/fuzz_targets/rsvp.rs"]:
            with self.subTest(path=path):
                self.assertEqual(inputs.classify(path), inputs.ALL_JOBS, path)
        self.assertEqual(inputs.classify("fuzz/README.md"), set())

    def test_voice_template_assets_skip_heavy_jobs(self):
        # Coverage, validator tests and readme are validated by the check
        # job's hermetic offline step, which always runs. Only the corpus
        # (include_str! in Rust tests) and the validator itself need Rust.
        for path in ["tests/voice_templates/coverage.json",
                     "tests/voice_templates/test_validator.py",
                     "tests/voice_templates/README.md"]:
            with self.subTest(path=path):
                self.assertEqual(inputs.classify(path), set(), path)

    def test_changed_path_fixture_matrix(self):
        # Fixture matrix: docs-only vs code vs asset PRs. Docs/asset-only
        # skips the heavy Rust matrix and the supply-chain scan; the
        # always-run `check` and `ci-ok` aggregators still report success.
        # Docs-only: skips Rust and supply, keeps worker (docs/ listing
        # assertion). `docs` is informational only and gates nothing.
        self.assertEqual(
            jobs(["docs/gateway-recovery.md", "README.md"]),
            {RUST: False, WORKER: True, PARITY: False,
             SUPPLY: False, DOCS: True})
        # Asset-only: validator assets plus chrome select no job; the
        # markdown assets still raise the informational docs flag.
        self.assertEqual(
            jobs(["tests/voice_templates/coverage.json",
                  "tests/voice_templates/test_validator.py",
                  "tests/voice_templates/README.md",
                  "LICENSE", "PACKAGES.md"]),
            {RUST: False, WORKER: False, PARITY: False,
             SUPPLY: False, DOCS: True})
        # Docs + asset: still skips Rust and supply, keeps worker.
        self.assertEqual(
            jobs(["tests/voice_templates/coverage.json",
                  "tests/voice_templates/README.md",
                  "docs/gateway-recovery.md", "README.md"]),
            {RUST: False, WORKER: True, PARITY: False,
             SUPPLY: False, DOCS: True})
        # Code: runs Rust, worker and supply (the scan inventories the
        # shipped binary), skips parity, raises no docs flag.
        self.assertEqual(
            jobs(["crates/core/src/lib.rs"]),
            {RUST: True, WORKER: True, PARITY: False,
             SUPPLY: True, DOCS: False})

    def test_corpus_change_still_runs_rust(self):
        self.assertEqual(
            jobs(["tests/voice_templates/corpus.json"]),
            {RUST: True, WORKER: False, PARITY: False,
             SUPPLY: True, DOCS: False})
        self.assertEqual(
            jobs(["tests/voice_templates/coverage.json",
                  "tests/voice_templates/corpus.json"]),
            {RUST: True, WORKER: False, PARITY: False,
             SUPPLY: True, DOCS: False})

    def test_scripts_run_rust_and_worker(self):
        for path in ["scripts/check-migrations.py",
                     "scripts/check_soak_checklist.py",
                     "scripts/check-secret-debug.py",
                     "scripts/job-inputs.py", "scripts/test_job_inputs.py",
                     "scripts/test-release.cjs",
                     "scripts/export-legacy-registry.mjs"]:
            with self.subTest(path=path):
                selected = inputs.classify(path)
                self.assertIn(RUST, selected, path)
                self.assertIn(WORKER, selected, path)

    def test_parity_scripts_run_everything(self):
        for path in ["scripts/check_parity_baseline.py",
                     "scripts/test_parity_baseline.py"]:
            with self.subTest(path=path):
                selected = inputs.classify(path)
                self.assertEqual(selected, inputs.ALL_JOBS, path)

    def test_receiver_contract_doc_selects_its_worker_conformance_check(self):
        path = "docs/internal-actions-receiver.md"
        self.assertIn(path, inputs.WORKER_DOCS)
        self.assertEqual(inputs.classify(path), {WORKER})
        self.assertEqual(
            jobs([path]),
            {RUST: False, WORKER: True, PARITY: False,
             SUPPLY: False, DOCS: True})

    def test_worker_tree_selects_worker_only(self):
        for path in ["wrangler/src/index.ts",
                     "wrangler/test/container-env.test.ts",
                     "wrangler/scripts/check-env-bindings.py"]:
            with self.subTest(path=path):
                self.assertEqual(inputs.classify(path), {WORKER}, path)

    def test_rust_docs_select_rust_only(self):
        # Content reads only; existence-only preflight falls through to
        # the worker-only docs rule. The soak regression reads staging-soak.
        # docs/backup.md reads content in the voice owner-grant journal
        # regression, so it selects rust despite the preconditions
        # existence check. docs/tickets.md is embedded by the ticket
        # component admission acceptance suite.
        rust_docs = ["docs/cutover.md",
                     "docs/commands.md", "docs/configuration.md",
                     "docs/voice-rooms.md",
                     "docs/soak-checklist.json", "docs/soak-checklist.md",
                     "docs/staging-soak.md",
                     "docs/privacy.md", "docs/database-roles.md",
                     "docs/rollback-delta.md", "docs/backup.md",
                     "docs/tickets.md"]
        for path in rust_docs:
            with self.subTest(path=path):
                self.assertEqual(inputs.classify(path), {RUST}, path)

    def test_parity_doc_selects_rust_and_parity(self):
        self.assertEqual(inputs.classify("docs/parity.md"), {RUST, PARITY})
        self.assertEqual(inputs.classify("docs/parity-baseline.json"),
                         {PARITY})

    def test_metrics_doc_selects_rust_and_worker(self):
        # docs/metrics.md is read by both the Rust exposition-conformance
        # test and the worker event/alert catalog tests, so its edits run
        # the Rust matrix, the worker job and the supply scan (via rust).
        self.assertEqual(inputs.classify("docs/metrics.md"),
                         {RUST, WORKER})
        self.assertEqual(
            jobs(["docs/metrics.md"]),
            {RUST: True, WORKER: True, PARITY: False,
             SUPPLY: True, DOCS: True})

    def test_embedded_rust_inputs_select_rust_and_worker(self):
        self.assertEqual(inputs.classify("docs/startup-diagnostics.md"),
                         {RUST, WORKER})
        self.assertEqual(inputs.classify("wrangler/wrangler.toml"),
                         {RUST, WORKER})
        self.assertEqual(
            jobs(["docs/startup-diagnostics.md"]),
            {RUST: True, WORKER: True, PARITY: False,
             SUPPLY: True, DOCS: True})
        self.assertEqual(
            jobs(["wrangler/wrangler.toml"]),
            {RUST: True, WORKER: True, PARITY: False,
             SUPPLY: True, DOCS: False})

    def test_runbook_docs_select_worker_only(self):
        for path in ["docs/runbook.md", "docs/container-readiness.md"]:
            with self.subTest(path=path):
                self.assertEqual(inputs.classify(path), {WORKER}, path)

    def test_unrelated_docs_select_worker_only(self):
        # No file reader in PR-run code, but runbook.test.ts asserts on the
        # docs/ listing itself (case-collision + readiness links).
        # docs/database-roles.md left this list: the voice owner-grant
        # journal regression reads its content, so it selects rust.
        for path in ["docs/gateway-recovery.md",
                     "docs/leveling-port.md", "docs/feeds-port.md",
                     "docs/distroless-evaluation.md", "docs/build-cache.md",
                     "docs/member-journey-parity.md"]:
            with self.subTest(path=path):
                self.assertEqual(inputs.classify(path), {WORKER}, path)

    def test_release_config_selects_worker_only(self):
        for path in ["release-please-config.json",
                     ".release-please-manifest.json"]:
            with self.subTest(path=path):
                self.assertEqual(inputs.classify(path), {WORKER}, path)

    def test_release_contributor_example_selects_worker_only(self):
        # The native release fixture reads and synchronizes the copyable
        # testsupport example, so a root-doc edit must run its worker step.
        self.assertEqual(
            jobs(["CONTRIBUTING.md"]),
            {RUST: False, WORKER: True, PARITY: False,
             SUPPLY: False, DOCS: True})
        self.assertEqual(
            jobs(["README.md"]),
            {RUST: False, WORKER: False, PARITY: False,
             SUPPLY: False, DOCS: True})

    def test_repo_chrome_selects_no_job(self):
        # Image inputs moved out of this list (TOG-14881): Dockerfile and
        # friends select `supply` (see test_image_inputs_select_supply_only).
        # Markdown chrome still raises the informational docs flag, which
        # gates nothing.
        skip = ["LICENSE", "CHANGELOG.md", ".editorconfig", ".gitignore",
                ".github/pull_request_template.md",
                ".github/CODEOWNERS",
                "README.md", "AGENTS.md", "CONTEXT.md",
                "PACKAGES.md"]
        self.assertEqual(jobs(skip),
                         {RUST: False, WORKER: False, PARITY: False,
                          SUPPLY: False, DOCS: True})

    def test_unrecognized_paths_fail_closed(self):
        for path in ["brand-new-dir/thing.txt", "Dockerfile.new",
                     "scripts", "crates", "wrangler", "deploy/svc",
                     ".github/UNKNOWN", ".gitattributes", "odd-root-file.rs",
                     "docs/unknown.py", ".gitleaks.toml", ".gitleaksignore",
                     ".github/dependabot.yml"]:
            with self.subTest(path=path):
                self.assertEqual(inputs.classify(path),
                                 {RUST, WORKER, PARITY, SUPPLY}, path)

    def test_docs_only_pr_skips_rust_and_supply(self):
        self.assertEqual(
            jobs(["docs/leveling-port.md", "docs/gateway-recovery.md",
                  "README.md", ".github/CODEOWNERS"]),
            {RUST: False, WORKER: True, PARITY: False,
             SUPPLY: False, DOCS: True})

    def test_wrangler_only_pr_skips_rust_parity_and_supply(self):
        self.assertEqual(
            jobs(["wrangler/src/index.ts", "wrangler/test/x.test.ts"]),
            {RUST: False, WORKER: True, PARITY: False,
             SUPPLY: False, DOCS: False})

    def test_rust_only_pr_skips_parity(self):
        self.assertEqual(
            jobs(["crates/core/src/lib.rs"]),
            {RUST: True, WORKER: True, PARITY: False,
             SUPPLY: True, DOCS: False})

    def test_mixed_diff_runs_what_it_touches(self):
        self.assertEqual(
            jobs(["docs/runbook.md", "crates/core/src/lib.rs"]),
            {RUST: True, WORKER: True, PARITY: False,
             SUPPLY: True, DOCS: True})
        self.assertEqual(
            jobs(["docs/runbook.md", "docs/parity.md"]),
            {RUST: True, WORKER: True, PARITY: True,
             SUPPLY: True, DOCS: True})

    def test_empty_diff_selects_nothing(self):
        self.assertEqual(jobs([]),
                         {RUST: False, WORKER: False, PARITY: False,
                          SUPPLY: False, DOCS: False})

    def test_cli_prints_all_jobs(self):
        with patch.object(inputs, "git_changes", return_value=(["crates/core/src/lib.rs"], [])):
            with patch("builtins.print") as printed:
                self.assertEqual(inputs.main(["--base-ref", "a", "--head-ref", "b"]), 0)
                printed.assert_any_call("rust=true")
                printed.assert_any_call("worker=true")
                printed.assert_any_call("parity=false")
                printed.assert_any_call("supply=true")
                printed.assert_any_call("docs=false")
                printed.assert_any_call("db-compat=false")
                printed.assert_any_call("migrations=false")
                printed.assert_any_call("workflow-fixtures=true")

    def test_cli_job_flag_prints_single_value(self):
        with patch.object(inputs, "git_changes", return_value=(["docs/gateway-recovery.md"], [])):
            for job, want in ((RUST, "false"), (WORKER, "true"),
                              (PARITY, "false"), (SUPPLY, "false"), (DOCS, "true"),
                              (inputs.DB_COMPAT, "false"), (inputs.MIGRATIONS, "false"),
                              (inputs.WORKFLOW_FIXTURES, "false")):
                with self.subTest(job=job), patch("builtins.print") as printed:
                    self.assertEqual(inputs.main(["--base-ref", "a", "--head-ref", "b",
                                                  "--job", job]), 0)
                    printed.assert_called_once_with(want)

    def test_cli_selects_all_jobs_on_deletion(self):
        with patch.object(inputs, "git_changes",
                          return_value=(["docs/metrics.md"], ["docs/runbook.md"])):
            with patch("builtins.print") as printed:
                self.assertEqual(inputs.main(["--base-ref", "a", "--head-ref", "b"]), 0)
                for job in inputs.OUTPUTS:
                    printed.assert_any_call(f"{job}=true")

    def test_cli_fails_closed_when_diff_is_undecidable(self):
        with patch.object(inputs, "git_changes",
                          side_effect=subprocess.CalledProcessError(128, "git")):
            with patch("builtins.print"):
                self.assertEqual(inputs.main(["--base-ref", "a", "--head-ref", "b"]), 2)

    def test_workflow_plan_requires_all_lanes_for_shared_dependency_or_empty_diff(self):
        for paths in ([], ["wrangler/package-lock.json"], ["crates/core/Cargo.toml"],
                      [".cargo/config.toml"], [".github/actions/build/action.yml"],
                      [".github/workflows/supply-chain.yml"], ["scripts/smoke.py"],
                      ["scripts/job-inputs.py"], ["migrations.lock"],
                      ["docs/guide.md", "unknown/input.bin"]):
            with self.subTest(paths=paths):
                selection = inputs.workflow_selection(paths, [])
                self.assertEqual({job for job, selected in selection.items() if selected} - {DOCS},
                                 set(inputs.OUTPUTS) - {DOCS})

    def test_docs_keeps_real_consumers_and_sql_keeps_migration_lanes(self):
        docs = inputs.workflow_selection(["docs/production-deploy.md"], [])
        self.assertEqual({job for job, selected in docs.items() if selected}, {DOCS, WORKER})
        rollout = inputs.workflow_selection(["docs/ci-speed-rollout.md"], [])
        self.assertTrue(rollout[inputs.WORKFLOW_FIXTURES])
        db = inputs.workflow_selection(["crates/store/migrations/0500_fixture.sql"], [])
        self.assertTrue(db[inputs.MIGRATIONS])
        self.assertTrue(db[inputs.DB_COMPAT])
        self.assertTrue(db[RUST])

    def test_db_compatibility_scope_includes_removed_and_quoted_paths(self):
        for path in ("migrations.lock", "crates/cutover/migrations/0480_é.sql",
                     "crates/store/migrations/0480_fixture.sql", "crates/store/sql/web_v1.sql",
                     "sql/database_roles.sql", "sql/database_role_matrix.sql",
                     "crates/cutover/src/db.rs", "crates/store/src/migrations.rs",
                     "scripts/ci/fixtures/db_compatibility_smoke.rs", ".github/workflows/check.yml"):
            with self.subTest(path=path):
                self.assertTrue(inputs.db_compat_selected([path]))
        self.assertFalse(inputs.db_compat_selected(
            ["docs/database-roles.md", "wrangler/src/index.ts", "crates/core/src/lib.rs"]))


class GitCandidateSelectionTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.git("init", "-q", "-b", "main")

    def git(self, *args):
        return subprocess.check_output(
            ["git", "-C", str(self.root), "-c", "user.name=CI fixture",
             "-c", "user.email=ci@example.invalid", "-c", "commit.gpgSign=false", *args],
            text=True, timeout=5).strip()

    def commit(self, label):
        self.git("add", ".")
        self.git("commit", "-qm", label)
        return self.git("rev-parse", "HEAD")

    def test_mutation_planner_uses_real_selector(self):
        planner = runpy.run_path(str(ROOT / ".github/scripts/mutation_scope.py"))
        path = self.root / "wrangler/src/fixture.ts"
        path.parent.mkdir(parents=True)
        path.write_text("export const active = false;\n")
        base = self.commit("base")
        path.write_text("export const active = true;\n")
        head = self.commit("candidate")
        output = self.root / "outputs"
        with patch.dict(os.environ, BASE_SHA=base, HEAD_SHA=head,
                        GITHUB_OUTPUT=str(output), RUNNER_TEMP=str(self.root)):
            planner["main"](self.root)
        self.assertIn("worker=true", output.read_text().splitlines())
        self.assertEqual((self.root / "worker-mutation-plan.json").read_text(),
                         '{"wrangler/src/fixture.ts": [1]}')

    def test_diverged_pr_candidate_and_unknown_change_types(self):
        (self.root / "guide.md").write_text("original")
        common = self.commit("common")
        self.git("switch", "-qc", "topic")
        self.git("mv", "guide.md", "renamed.md")
        head = self.commit("rename")
        self.git("switch", "-q", "main")
        (self.root / "other.md").write_text("base-only")
        (self.root / "alias.md").symlink_to("other.md")
        base = self.commit("base advances")
        self.git("merge", "-q", "--no-ff", "topic", "-m", "candidate")
        merge = self.git("rev-parse", "HEAD")
        with self.assertRaises(inputs.JobInputsError):
            inputs.pr_comparison(self.root, head, base, merge)
        self.assertEqual(inputs.pr_comparison(self.root, base, head, merge), (common, merge))
        changed, uncertain = inputs.git_changes(common, merge, self.root)
        self.assertCountEqual(changed, ["other.md", "renamed.md"])
        self.assertCountEqual(uncertain, ["guide.md", "alias.md"])
        self.assertTrue(all(inputs.workflow_selection(changed, uncertain).values()))

    def test_workflow_bootstraps_selector_from_event_base(self):
        path = self.root / "scripts/job-inputs.py"
        path.parent.mkdir()
        path.write_bytes((ROOT / "scripts/job-inputs.py").read_bytes())
        guide = self.root / "guide.md"
        guide.write_text("original")
        base = self.commit("base selector")
        self.git("switch", "-qc", "topic")
        guide.write_text("edited")
        head = self.commit("guide change")
        self.git("switch", "-q", "main")
        self.git("merge", "-q", "--no-ff", "topic", "-m", "candidate")
        merge = self.git("rev-parse", "HEAD")

        selector_job = WorkflowSurfaceTests.job_text(CHECK_YML.read_text(), "job-inputs")
        step = selector_job.split("id: select\n", 1)[1].split("\n      - name:", 1)[0]
        script = textwrap.dedent(step.split("run: |\n", 1)[1])
        output = self.root / "outputs"
        env = {**os.environ, "EVENT_NAME": "pull_request", "BASE_SHA": base,
               "HEAD_SHA": head, "MERGE_SHA": merge, "RUNNER_TEMP": str(self.root),
               "GITHUB_WORKSPACE": str(self.root), "GITHUB_OUTPUT": str(output)}
        # A checkout-local copy cannot decide which suites run.
        path.write_text("raise SystemExit('checkout selector must not run')\n")
        for candidate, selected in ((merge, "false"), (head, "true")):
            with self.subTest(candidate=candidate):
                output.write_text("")
                run = subprocess.run(["bash", "-e", "-c", script], cwd=self.root,
                                     env={**env, "MERGE_SHA": candidate},
                                     capture_output=True, text=True, timeout=5)
                self.assertEqual(run.returncode, 0, run.stderr)
                plan = dict(line.split("=", 1) for line in output.read_text().splitlines())
                self.assertEqual(tuple(plan), inputs.OUTPUTS)
                for lane in (RUST, WORKER, inputs.MIGRATIONS, inputs.DB_COMPAT, inputs.WORKFLOW_FIXTURES):
                    self.assertEqual(plan[lane], selected, run.stdout)

    def test_database_proof_binds_base_only_migration_to_candidate(self):
        guide = self.root / "guide.md"
        guide.write_text("original")
        common = self.commit("common")
        self.git("switch", "-qc", "topic")
        guide.write_text("edited")
        head = self.commit("docs change")
        self.git("switch", "-q", "main")
        migration = self.root / "crates/store/migrations/0500_fixture.sql"
        migration.parent.mkdir(parents=True)
        migration.write_text("-- fixture\n")
        base = self.commit("migration on base")
        self.git("merge", "-q", "--no-ff", "topic", "-m", "candidate")
        merge = self.git("rev-parse", "HEAD")
        changed, uncertain = inputs.git_changes(common, merge, self.root)
        self.assertTrue(inputs.workflow_selection(changed, uncertain)[inputs.DB_COMPAT])

        # Exercise the actual identity check before the build and database work.
        script = (ROOT / "scripts/ci/db_compatibility.sh").read_text().split("scratch=$(mktemp", 1)[0]
        env = {**os.environ, "GITHUB_ACTIONS": "true", "GITHUB_EVENT_NAME": "pull_request",
               "GITHUB_SHA": merge, "PR_BASE_SHA": base, "PR_HEAD_SHA": head}
        for ref, expected in ((merge, 0), (head, 1)):
            with self.subTest(ref=ref):
                self.git("switch", "-q", "--detach", ref)
                run = subprocess.run(["bash", "-c", script], cwd=self.root,
                                     env=env, capture_output=True, text=True, timeout=5)
                self.assertEqual(run.returncode, expected, run.stderr)
                self.assertEqual(migration.is_file(), expected == 0)


class DocsIndexWiringTests(unittest.TestCase):
    def test_docs_index_inputs_and_ungated_execution(self):
        for path in ("docs/README.md", "docs/nested/new-guide.md"):
            with self.subTest(path=path):
                selected = jobs([path])
                self.assertTrue(selected[DOCS])
                self.assertTrue(selected[WORKER])
        for path in ("scripts/check_docs_index.py", "scripts/test_docs_index.py"):
            with self.subTest(path=path):
                self.assertEqual(inputs.classify(path), inputs.ALL_JOBS)
        body = CHECK_YML.read_text()
        step = body.split("- name: Docs index, orphan and byte-budget conformance (offline)\n", 1)[1].split("\n      - ", 1)[0]
        self.assertIn("python3 -m unittest discover -s scripts -p 'test_docs_index.py' -v", step)
        self.assertNotIn("if:", step, "docs-only selection must never skip the guard")


class ReaderCoverageTests(unittest.TestCase):
    """Every repo-owned literal content reader must agree with the classifier.

    Rust literal include_str! paths resolve from their source file; dynamic
    macro arguments are intentionally outside this scan. The existence-only
    docs policy remains separate from content-reader classification.
    """

    # Docs the preconditions binary checks by existence only (is_file, never
    # content): a content edit cannot break Rust, and a deletion forces all
    # jobs via the deletion rule. Every other docs/ literal in Rust is a
    # content read and must select the rust job. docs/backup.md left this
    # set: the voice owner-grant journal regression reads its content, so a
    # content edit must select rust (the preconditions is_file check still
    # holds alongside the content read). staging-soak also left this set:
    # the soak checklist planned-outage regression reads its content.
    EXISTENCE_ONLY_DOCS = frozenset({
        "docs/preflight.md",
        "docs/runbook.md",
    })

    # Quoted path literals in Rust: bare repo-relative ("docs/x.md",
    # root.join("wrangler/src/x.ts")), manifest-relative
    # (.join("../../docs/x.md"), read_manifest_file("../../docs/x.md")) and
    # format-prefixed ("{}/../../docs/x.md" after CARGO_MANIFEST_DIR).
    PATH_LITERAL = re.compile(
        r'"(?:\{\}/)?((?:\.\./)*[A-Za-z0-9_][A-Za-z0-9_./-]*\.[A-Za-z0-9]+)"')
    INCLUDE_STR = re.compile(r'include_str!\(\s*"([A-Za-z0-9_./-]+)"\s*\)')
    # Rust-owned roots already select the rust job by prefix.
    RUST_ROOTS = ("crates/", "src/", "tests/")

    @staticmethod
    def manifest_dir(source):
        for parent in source.parents:
            if (parent / "Cargo.toml").is_file():
                return parent
        return ROOT

    @classmethod
    def rust_content_readers(cls, source, text):
        """Repo files outside Rust roots that one Rust source names by path.

        include_str! resolves from the source file; every other literal
        resolves from the crate's CARGO_MANIFEST_DIR and, when it does not
        climb with ``../``, from the repo root. Outside docs/ only literals
        that land on an existing repo file count, so prose and identifiers
        drop out; docs/*.md|json count even when absent, because ignored
        generated docs (docs/generated/commands.md) do not exist when the
        selector regressions run.
        """
        text = "\n".join(line for line in text.splitlines()
                         if not line.strip().startswith("//"))
        candidates = [(source.parent / path).resolve()
                      for path in cls.INCLUDE_STR.findall(text)]
        manifest = cls.manifest_dir(source)
        for literal in cls.PATH_LITERAL.findall(text):
            candidates.append((manifest / literal).resolve())
            if not literal.startswith("../"):
                candidates.append((ROOT / literal).resolve())
        readers = set()
        for resolved in candidates:
            if not resolved.is_relative_to(ROOT):
                continue
            relative = resolved.relative_to(ROOT).as_posix()
            is_doc = relative.startswith(DOCS_PREFIX) and \
                resolved.suffix in (".md", ".json")
            if (is_doc or resolved.is_file()) and \
                    not relative.startswith(cls.RUST_ROOTS):
                readers.add(relative)
        return readers

    def test_reader_scan_resolves_relative_literal_shapes(self):
        source = ROOT / "crates/core/tests/fixture.rs"
        shapes = {
            'Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/custom-commands.md")':
                "docs/custom-commands.md",
            'read_manifest_file("../../docs/cutover-migration-ledger.md")':
                "docs/cutover-migration-ledger.md",
            'root.join("wrangler/src/container-env.ts")':
                "wrangler/src/container-env.ts",
            'format!("{}/../../docs/observability-event-catalog.md", dir)':
                "docs/observability-event-catalog.md",
            'include_str!("../../../docs/tickets.md")': "docs/tickets.md",
        }
        for text, want in shapes.items():
            with self.subTest(shape=text):
                self.assertEqual(self.rust_content_readers(source, text), {want})
                self.assertIn(RUST, inputs.classify(want), want)
        # Ignored generated docs are absent on CI but stay in the scan.
        self.assertEqual(
            self.rust_content_readers(
                source, '.join("../../docs/generated/not-generated.md")'),
            {"docs/generated/not-generated.md"})
        for text in ('"../../wrangler/no-such-file.ts"', '// "docs/README.md"',
                     '"src/lib.rs"', '"not a path"'):
            with self.subTest(ignored=text):
                self.assertEqual(self.rust_content_readers(source, text), set())

    def test_rust_content_readers_are_classified_rust(self):
        content_readers = set()
        rust_sources = list((ROOT / "crates").rglob("*.rs")) + (
            [ROOT / "src/lib.rs"] if (ROOT / "src/lib.rs").exists() else [])
        for source in rust_sources:
            content_readers |= self.rust_content_readers(
                source, source.read_text())
        self.assertTrue(content_readers, "expected Rust content readers")
        for doc in self.EXISTENCE_ONLY_DOCS:
            self.assertIn(doc, content_readers, f"{doc} left Rust sources")
        for literal in sorted(content_readers):
            with self.subTest(path=literal):
                if literal in self.EXISTENCE_ONLY_DOCS:
                    self.assertNotIn(RUST, inputs.classify(literal), literal)
                else:
                    self.assertIn(RUST, inputs.classify(literal), literal)

    def test_parity_checker_inputs_are_classified_parity(self):
        literals = set()
        for name in ("check_parity_baseline.py", "test_parity_baseline.py",
                     "check_soak_checklist.py", "test_soak_checklist.py"):
            text = (ROOT / "scripts" / name).read_text()
            literals.update(
                re.findall(r'"(docs/[A-Za-z0-9_./-]+\.(?:md|json))"', text))
        self.assertTrue(literals, "expected parity-checker doc literals")
        for literal in sorted(literals):
            with self.subTest(doc=literal):
                selected = inputs.classify(literal)
                self.assertTrue(selected & {RUST, PARITY}, literal)

    def test_wrangler_doc_reads_are_classified_worker(self):
        literals = set()
        for path in list((ROOT / "wrangler/src").rglob("*.ts")) + \
                list((ROOT / "wrangler/test").rglob("*.ts")):
            literals.update(
                re.findall(r'"(docs/[A-Za-z0-9_./-]+\.md)"', path.read_text()))
        self.assertTrue(literals, "expected wrangler doc literals")
        for literal in ("docs/runbook.md", "docs/container-readiness.md"):
            with self.subTest(doc=literal):
                self.assertEqual(inputs.classify(literal), {WORKER}, literal)

    def test_soak_docs_reach_rust(self):
        for literal in ("docs/soak-checklist.json", "docs/soak-checklist.md",
                        "docs/parity.md"):
            with self.subTest(doc=literal):
                self.assertIn(RUST, inputs.classify(literal), literal)

    def test_every_crate_migration_is_a_rust_input(self):
        migrations = [p.as_posix() for p in (ROOT / "crates").glob("*/migrations/*")
                      if p.is_file()][:5]
        self.assertTrue(migrations, "expected crate migrations")
        for path in migrations:
            with self.subTest(path=path):
                self.assertIn(RUST, inputs.classify(path), path)


class WorkflowSurfaceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = CHECK_YML.read_text()
        cls.on_block = cls.text.split("\njobs:")[0]
        cls.selector = cls.text.split("  job-inputs:")[1].split("\n  container-inputs:")[0]

    def test_no_workflow_level_paths_filter(self):
        # A `paths:` filter under `on.pull_request` would skip the whole
        # workflow (including the always-reporting aggregator). Selection is
        # per-job via outputs.
        self.assertNotIn("paths:", self.on_block)

    def test_selector_job_exists_and_needs_nothing(self):
        self.assertRegex(self.text, r"(?m)^  job-inputs:\s*$")
        self.assertNotIn("needs:", self.selector.split("steps:")[0],
                         "selector must run without waiting on the test jobs")

    def test_selector_runs_before_dependent_jobs(self):
        self.assertLess(self.text.index("  job-inputs:"),
                        self.text.index("\n  container-inputs:"))

    def test_selector_exposes_all_lanes(self):
        outputs = self.selector.split("outputs:", 1)[1].split("steps:", 1)[0]
        for job in inputs.OUTPUTS:
            self.assertIn(job, outputs)

    def test_selector_checks_out_full_history(self):
        self.assertIn("fetch-depth: 0", self.selector)

    def test_selector_runs_the_classifier(self):
        self.assertIn("scripts/job-inputs.py", self.selector)
        self.assertIn("pull_request", self.selector)
        self.assertIn("base.sha", self.selector)
        self.assertIn("head.sha", self.selector)

    def test_selector_defaults_to_full_jobs(self):
        # Fail-closed end to end: undecidable diffs, missing SHAs and
        # non-PR events all select full jobs in the shell step, so a
        # classifier exit code can never silently skip coverage. `docs`
        # defaults to false: it is informational only and gates nothing.
        self.assertIn("rust=true", self.selector)
        self.assertIn("worker=true", self.selector)
        self.assertIn("parity=true", self.selector)
        self.assertIn("supply=true", self.selector)
        self.assertIn("docs=false", self.selector)
        self.assertIn("github.event_name", self.selector)

    def test_selector_regressions_run_in_ci(self):
        self.assertIn("test_job_inputs.py", self.selector)

    def test_consumer_jobs_wait_on_selector(self):
        for job in ("check", "rust-tests", "coverage-plan", "coverage-shard",
                    "patch-coverage", "ignored-db-stores",
                    "ignored-db-runtime", "community-db", "feeds-db",
                    "tickets-postgres", "worker", "supply-chain", "ci-ok",
                    "fuzz-compile"):
            head = self.text.split(f"\n  {job}:")[1].split("steps:", 1)[0]
            with self.subTest(job=job):
                self.assertIn("job-inputs", head)

    def test_supply_chain_job_is_selector_gated(self):
        # CI standard rule 6 (TOG-14881): the SBOM scan runs only when the
        # `supply` area changed; docs-only and worker-UI-only PRs skip it.
        head = self.text.split("\n  supply-chain:")[1].split("steps:", 1)[0]
        self.assertIn("needs.job-inputs.outputs.supply != 'false'", head)

    def test_ci_ok_supply_gate_is_conditional(self):
        # A skipped supply-chain job must not fail the aggregator; the gate
        # only tolerates the skip the selector itself produced. (The old
        # per-job guard in `check` was dropped when `check` stopped waiting
        # on the slow jobs; scripts/ci/test_required_checks.py executes the
        # aggregator for every result/selector combination.)
        body = self.text.split("\n  ci-ok:")[1]
        self.assertIn(
            '("supply-chain", "SUPPLY_CHAIN_RESULT", "SUPPLY_SELECTED")', body)
        self.assertIn('result == "skipped" and selected == "false"', body)

    def test_ci_ok_aggregator_exists_and_is_always_run(self):
        # CI standard rule 3 (TOG-14881): one required aggregator named
        # `ci-ok`, running on every event, passing only when every needed
        # job succeeded or was legitimately skipped.
        head = self.text.split("\n  ci-ok:")[1].split("steps:", 1)[0]
        self.assertIn("always()", head)
        for job in ("job-inputs", "container-inputs", "supply-chain", "check", "rust-tests",
                    "coverage-plan", "coverage-shard", "patch-coverage",
                    "ignored-db-stores", "ignored-db-runtime", "worker",
                    "parity-docs", "self-role-store", "community-db",
                    "feeds-db", "tickets-postgres", "moderation-db", "fuzz-compile"):
            self.assertIn(job, head)
        body = self.text.split("\n  ci-ok:")[1]
        self.assertIn("SUPPLY_SELECTED", body)
        self.assertIn("FUZZ_COMPILE_RESULT", body)
        self.assertIn("ci-ok passed", body)

    def test_weekly_full_run_schedule_exists(self):
        # CI standard rules 5-6 (TOG-14881): push to main plus a scheduled
        # full run (here weekly, alongside the nightly sweep workflow).
        # No `paths:` filter may gate the workflow itself (rule 2).
        self.assertIn("schedule:", self.on_block)
        self.assertIn("cron:", self.on_block)

    def test_worker_is_always_run_required_check(self):
        # `worker check` is required on main: it must never skip at the job
        # level (a `paths:` filter or job-level selector `if:` would strand
        # the gate). Fast-pass is per-step so the check always reports.
        head = self.text.split("\n  worker:")[1].split("steps:", 1)[0]
        self.assertIn("always()", head)
        self.assertNotIn("outputs.worker != 'false'", head,
                         "worker must not skip at the job level")

    def test_worker_fast_pass_and_selector_guard(self):
        body = self.text.split("\n  worker:")[1]
        self.assertIn("Docs/asset-only fast pass", body)
        self.assertIn("needs.job-inputs.outputs.worker == 'false'", body)
        self.assertIn("require job inputs selection to pass", body)
        self.assertIn("needs.job-inputs.result != 'success'", body)

    def test_worker_heavy_steps_are_gated(self):
        body = self.text.split("\n  worker:")[1]
        # setup-node, npm ci/typecheck/test, env bindings, verifications.
        gated = [line for line in body.splitlines()
                 if "needs.job-inputs.outputs.worker != 'false'" in line]
        self.assertGreaterEqual(len(gated), 10,
                                "every heavy worker step needs the selector guard")

    def test_staging_rollout_suite_stays_unconditional(self):
        # The offline suite pins its worker step with no `if:`
        # (test_required_worker_ci_runs_this_offline_suite): fast-pass
        # must not gate it, or worker check fails on every PR.
        worker = self.text.split("\n  worker:")[1]
        steps = re.split(r"(?m)^      - ", worker)[1:]
        matching = [step for step in steps if "test_staging_rollout.py" in step]
        self.assertEqual(len(matching), 1)
        self.assertNotRegex(matching[0], r"(?m)^\s*(?:if|continue-on-error):")

    def test_heavy_steps_are_gated(self):
        # Every gated step references a job-inputs output; the check job's
        # gates are per-step so skipped jobs still enter the aggregator and
        # pass its needs-result gates explicitly.
        gated = [line for line in self.text.splitlines()
                 if "job-inputs.outputs." in line]
        self.assertGreaterEqual(len(gated), 10)

    @staticmethod
    def job_text(text, job):
        """One top-level job block: its header line down to the next job."""
        body = text.split(f"\n  {job}:\n", 1)[1]
        return re.split(r"(?m)^  [A-Za-z0-9_-]+:\s*$", body, maxsplit=1)[0]

    @classmethod
    def job_steps(cls, text, job):
        """Yield (name, body) for each step of one job; comments dropped."""
        body = cls.job_text(text, job).split("\n    steps:\n", 1)[1]
        for block in re.split(r"(?m)^      - (?=\S)", body)[1:]:
            lines = [line for line in block.splitlines()
                     if not line.lstrip().startswith("#")]
            name = re.search(r"name:\s*(.+)", "\n".join(lines))
            yield (name.group(1).strip() if name else lines[0].strip(),
                   "\n".join(lines))

    # The `check` lane always runs, so its Rust/DB steps carry the selector
    # guard per step; the four Rust test lanes skip at the job level instead.
    # The coverage planner skips at the job level too but is too small for
    # the lane-shape pin below, and the merge uses a fail-closed `always()`
    # form; both are pinned by test_coverage_plan_and_merge_skip_without_rust.
    RUST_LANES = ("rust-tests", "coverage-shard", "ignored-db-stores", "ignored-db-runtime")
    LANE_GUARD = "if: needs.job-inputs.outputs.rust != 'false'"

    # What a step must not do on a rust=false PR: that run never installs the
    # toolchain (no `cargo`) and never creates `two_bot_test_ci`, so any step
    # that needs either must carry the selector guard. TOG-12945: send
    # admission, reengagement, staging migrate, the Docker manifests check
    # and the onboarding prompt-store/shared-runtime steps ran unguarded and
    # failed every non-Rust PR.
    NEEDS_RUST = re.compile(
        r"\bcargo\b|\bcreatedb\b|\bdropdb\b|two_bot_test_ci|TEST_DATABASE_URL"
        r"|rust-toolchain|rust-cache|cargo-deny|check-docker-manifests\.py")
    # Always-on gates that only touch the checkout and the `agent-testdb`
    # service; they never need the toolchain or the CI database.
    ALWAYS_ON = ("Job-container prerequisites",)
    # Name-pinned exemption from the guard above (deterministic quality
    # gates): the deny action ships its own toolchain and reads only the
    # lockfile plus the advisory DB, so its advisories run on every PR,
    # including docs-only, with no repo toolchain or database. Any other step
    # mentioning cargo still needs the guard.
    SELF_CONTAINED = ("cargo-deny",)

    def unguarded(self, text, job):
        return [name for name, body in self.job_steps(text, job)
                if not name.startswith(self.ALWAYS_ON)
                and name not in self.SELF_CONTAINED
                and self.NEEDS_RUST.search(body)
                and not re.search(r"(?m)^\s+if:.*needs\.job-inputs\.outputs\.rust != 'false'", body)]

    def test_every_rust_or_db_step_in_check_job_is_gated(self):
        steps = list(self.job_steps(self.text, "check"))
        self.assertGreater(len(steps), 20, "step parser lost the check job")
        self.assertEqual(self.unguarded(self.text, "check"), [],
                         "needs the Rust toolchain or the CI database but is not "
                         "guarded by the rust selector")

    def test_cargo_deny_runs_ungated_on_every_pr(self):
        # Deterministic quality gates: advisories run on every PR, so the
        # self-contained deny step carries no selector guard. The exemption
        # stays name-pinned to this one step.
        self.assertEqual(self.SELF_CONTAINED, ("cargo-deny",))
        body = dict(self.job_steps(self.text, "check"))["cargo-deny"]
        self.assertNotIn("needs.job-inputs.outputs.rust", body)

    def test_rust_test_lanes_skip_at_the_job_level(self):
        # No Rust inputs (docs-only, worker-UI-only): the lanes never start,
        # so no runner, container or service is spent; ci-ok accepts the skip
        # only because the selector deselected them.
        for job in self.RUST_LANES:
            with self.subTest(job=job):
                head = self.job_text(self.text, job).split("\n    steps:\n", 1)[0]
                self.assertIn(f"\n    {self.LANE_GUARD}\n", head)
                self.assertGreater(len(list(self.job_steps(self.text, job))), 10)
                for name, body in self.job_steps(self.text, job):
                    self.assertNotRegex(
                        body, r"(?m)^\s+if:.*outputs\.rust",
                        f"{name!r}: the lane is already gated at the job level")

    def test_coverage_plan_and_merge_skip_without_rust(self):
        # No Rust inputs (docs-only, worker-UI-only): the planner skips at
        # the job level like the test lanes, and the merge skips too — its
        # fail-closed `always()` form still requires the rust selector plus a
        # successful plan, so a partial shard set can never read as measured.
        # ci-ok accepts the skips only because the selector deselected them.
        plan = self.job_text(self.text, "coverage-plan").split("\n    steps:\n", 1)[0]
        self.assertIn(f"\n    {self.LANE_GUARD}\n", plan)
        self.assertIn("job-inputs", plan)
        merge = self.job_text(self.text, "patch-coverage").split("\n    steps:\n", 1)[0]
        self.assertIn("always()", merge)
        self.assertIn("!cancelled()", merge)
        self.assertIn("needs.job-inputs.outputs.rust != 'false'", merge)
        self.assertIn("needs.coverage-plan.result == 'success'", merge)
        self.assertIn("coverage-shard", merge)

    def test_fuzz_compile_lane_skips_at_the_job_level(self):
        # Compile-only libFuzzer lane follows the same rust selector as the
        # test lanes; ci-ok only accepts its skip when rust was deselected.
        head = self.job_text(self.text, "fuzz-compile").split("\n    steps:\n", 1)[0]
        self.assertIn(f"\n    {self.LANE_GUARD}\n", head)
        self.assertIn("job-inputs", head)
        body = self.job_text(self.text, "fuzz-compile")
        self.assertIn("fuzz build", body)
        # The `+nightly` selector is load-bearing: the repo-root
        # rust-toolchain.toml (stable) otherwise wins over the installed
        # nightly and `-Zsanitizer` fails.
        self.assertIn("cargo +nightly-2026-10-01 fuzz build", body)
        self.assertIn("cargo +nightly-2026-10-01 fetch --locked", body)
        self.assertNotIn("cargo fuzz run", body)

    def test_guard_scan_flags_an_unguarded_db_step(self):
        # Self-test of the scan: dropping the guard on a step that needs the
        # toolchain must be caught, and the real step must satisfy it.
        step = "      - name: channel moderation store clippy\n"
        guard = "        if: needs.job-inputs.outputs.rust != 'false'\n"
        self.assertIn(step + guard, self.text)
        weakened = self.text.replace(step + guard, step)
        self.assertEqual(self.unguarded(weakened, "check"),
                         ["channel moderation store clippy"])

    def test_check_lanes_start_with_the_selector_not_behind_slow_jobs(self):
        # The lint lane and the test lanes run in parallel: each waits
        # on the selector only, never on the SBOM scan or the DB jobs (the old
        # `check` started a median ~7.8 min after the run began because it
        # waited on them). The aggregators carry those gates instead.
        # Exception: coverage-shard also waits on coverage-plan, the fast
        # (~2 min) matrix it cannot start without.
        for job in ("check", *self.RUST_LANES):
            with self.subTest(job=job):
                head = self.job_text(self.text, job)
                if job == "coverage-shard":
                    self.assertRegex(head, r"(?m)^    needs: \[job-inputs, coverage-plan\]$")
                else:
                    self.assertRegex(head, r"(?m)^    needs: \[job-inputs\]$")

    def test_push_to_main_selects_everything(self):
        self.assertIn("pull_request", self.selector)
        self.assertIn("rust=true", self.selector)

class VoiceSelectionTest(unittest.TestCase):
    """Owner policy 2026-10-11: the voice synthetic runs on PRs only for voice inputs."""

    def test_voice_inputs_select_the_synthetic(self):
        for path in (
            "crates/bot/src/voice_rooms.rs",
            "crates/bot/src/voice_rooms/join_requests.rs",
            "crates/core/src/voice_name_filter.rs",
            "crates/bot/src/gateway.rs",
            "crates/discord/src/voice_rooms.rs",
            "crates/cutover/src/voice_rooms.rs",
            "crates/discord/src/voice_rooms_http_tests.rs",
            "./crates/cutover/src/voice_rooms.rs",
            "tests/voice_templates/corpus.json",
            "Cargo.lock",
            "scripts/staging_voice_synthetic.py",
            "scripts/test_staging_voice_synthetic_dry_run_acceptance.py",
            "scripts/test_staging_voice_synthetic_gateway_intake_acceptance.py",
            "scripts/test_staging_voice_synthetic_owned_room_acceptance.py",
            ".github/workflows/voice-synthetic.yml",
            "./crates/bot/src/voice_room_plan.rs",
        ):
            self.assertTrue(inputs.voice_selected([path]), path)

    def test_unrelated_changes_skip_the_synthetic(self):
        for path in (
            "crates/bot/src/tickets.rs",
            "crates/core/src/settings.rs",
            "crates/discord/src/automod.rs",
            "crates/cutover/src/tickets.rs",
            "crates/cutover/src/voice_config_store.rs",
            "crates/cutover/src/voice_ghosts.rs",
            "crates/cutover/src/voice_reconcile.rs",
            "docs/voice-rooms.md",
            "wrangler/src/index.ts",
            "README.md",
        ):
            self.assertFalse(inputs.voice_selected([path]), path)
        self.assertFalse(inputs.voice_selected([]))

    def test_any_deletion_selects_the_synthetic(self):
        self.assertTrue(inputs.voice_selected([], ["docs/old.md"]))
        for path in (
            "crates/discord/src/voice_rooms.rs",
            "crates/cutover/src/voice_rooms.rs",
        ):
            self.assertTrue(inputs.voice_selected([], [path]), path)

    def test_voice_is_never_a_ci_job(self):
        self.assertNotIn(inputs.VOICE, inputs.ALL_JOBS)


if __name__ == "__main__":
    unittest.main()
