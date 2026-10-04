"""Offline fixtures for the check.yml job selector; no Docker, git or GitHub needed.

Mirrors the `scripts/test_container_inputs.py` pattern: pure decision logic
tested hermetically, plus workflow-yaml surface assertions parsed as text
(the same stdlib-only technique as `scripts/test-secret-scan.py`).

Every coupling claim in `scripts/job-inputs.py` is pinned by a test here,
so a drift in readers (new doc file read by Rust, new worker coupling) fails
this suite before it can silently skip coverage.
"""

import importlib.util
from pathlib import Path
import re
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "job_inputs", Path(__file__).with_name("job-inputs.py"))
inputs = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(inputs)

ROOT = Path(__file__).resolve().parents[1]
CHECK_YML = ROOT / ".github/workflows/check.yml"
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
        for path in ["Dockerfile", "Dockerfile.distroless",
                     ".dockerignore"]:
            with self.subTest(path=path):
                self.assertEqual(inputs.classify(path), {SUPPLY}, path)
        self.assertEqual(
            jobs(["Dockerfile"]),
            {RUST: False, WORKER: False, PARITY: False,
             SUPPLY: True, DOCS: False})

    def test_crate_changes_run_rust_and_worker(self):
        rust = ["crates/core/src/lib.rs", "crates/bot/src/main.rs",
                "crates/store/src/web.rs", "crates/cutover/build.rs",
                "crates/core/Cargo.toml",
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
                     ".cargo/config.toml",
                     "tests/voice_templates/corpus.json",
                     "tests/voice_templates/validate.py"]:
            with self.subTest(path=path):
                self.assertEqual(inputs.classify(path), {RUST}, path)

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
            jobs(["docs/metrics.md", "README.md"]),
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
                  "docs/metrics.md", "README.md"]),
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

    def test_parity_scripts_also_gate_parity(self):
        for path in ["scripts/check_parity_baseline.py",
                     "scripts/test_parity_baseline.py"]:
            with self.subTest(path=path):
                selected = inputs.classify(path)
                self.assertEqual(selected, {RUST, WORKER, PARITY}, path)

    def test_worker_tree_selects_worker_only(self):
        for path in ["wrangler/src/index.ts",
                     "wrangler/test/container-env.test.ts",
                     "wrangler/package.json", "wrangler/wrangler.toml",
                     "wrangler/scripts/check-env-bindings.py"]:
            with self.subTest(path=path):
                self.assertEqual(inputs.classify(path), {WORKER}, path)

    def test_rust_docs_select_rust_only(self):
        # Content reads only; existence-only docs (staging-soak/backup/
        # preflight) fall through to the worker-only docs rule.
        rust_docs = ["docs/cutover.md",
                     "docs/commands.md", "docs/configuration.md",
                     "docs/voice-rooms.md",
                     "docs/soak-checklist.json", "docs/soak-checklist.md"]
        for path in rust_docs:
            with self.subTest(path=path):
                self.assertEqual(inputs.classify(path), {RUST}, path)

    def test_parity_doc_selects_rust_and_parity(self):
        self.assertEqual(inputs.classify("docs/parity.md"), {RUST, PARITY})
        self.assertEqual(inputs.classify("docs/parity-baseline.json"),
                         {PARITY})

    def test_runbook_docs_select_worker_only(self):
        for path in ["docs/runbook.md", "docs/container-readiness.md"]:
            with self.subTest(path=path):
                self.assertEqual(inputs.classify(path), {WORKER}, path)

    def test_unrelated_docs_select_worker_only(self):
        # No file reader in PR-run code, but runbook.test.ts asserts on the
        # docs/ listing itself (case-collision + readiness links).
        for path in ["docs/metrics.md", "docs/gateway-recovery.md",
                     "docs/leveling-port.md", "docs/feeds-port.md",
                     "docs/distroless-evaluation.md", "docs/build-cache.md",
                     "docs/database-roles.md",
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
                ".gitleaks.toml", ".gitleaksignore",
                ".github/pull_request_template.md",
                ".github/CODEOWNERS", ".github/dependabot.yml",
                "README.md", "AGENTS.md", "CONTEXT.md",
                "PACKAGES.md"]
        self.assertEqual(jobs(skip),
                         {RUST: False, WORKER: False, PARITY: False,
                          SUPPLY: False, DOCS: True})

    def test_unrecognized_paths_fail_closed(self):
        for path in ["brand-new-dir/thing.txt", "Dockerfile.new",
                     "scripts", "crates", "wrangler", "deploy/svc",
                     ".github/UNKNOWN", ".gitattributes", "odd-root-file.rs"]:
            with self.subTest(path=path):
                self.assertEqual(inputs.classify(path),
                                 {RUST, WORKER, PARITY, SUPPLY}, path)

    def test_docs_only_pr_skips_rust_and_supply(self):
        self.assertEqual(
            jobs(["docs/metrics.md", "docs/gateway-recovery.md",
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

    @staticmethod
    def fake_diff(changed):
        def diff(base, head, root=None, diff_filter=None):
            return list(changed) if diff_filter != "D" else []
        return diff

    def test_cli_prints_all_jobs(self):
        with patch.object(inputs, "git_diff_names",
                          side_effect=self.fake_diff(["crates/core/src/lib.rs"])):
            with patch("builtins.print") as printed:
                self.assertEqual(
                    inputs.main(["--base-ref", "a", "--head-ref", "b"]), 0)
                printed.assert_any_call("rust=true")
                printed.assert_any_call("worker=true")
                printed.assert_any_call("parity=false")
                printed.assert_any_call("supply=true")
                printed.assert_any_call("docs=false")

    def test_cli_job_flag_prints_single_value(self):
        with patch.object(inputs, "git_diff_names",
                          side_effect=self.fake_diff(["docs/metrics.md"])):
            for job, want in ((RUST, "false"), (WORKER, "true"),
                              (PARITY, "false"), (SUPPLY, "false"),
                              (DOCS, "true")):
                with self.subTest(job=job), patch("builtins.print") as printed:
                    self.assertEqual(
                        inputs.main(["--base-ref", "a", "--head-ref", "b",
                                     "--job", job]), 0)
                    printed.assert_called_once_with(want)

    def test_cli_selects_all_jobs_on_deletion(self):
        def diff(base, head, root=None, diff_filter=None):
            return ["docs/metrics.md"] if diff_filter != "D" \
                else ["docs/runbook.md"]
        with patch.object(inputs, "git_diff_names", side_effect=diff):
            with patch("builtins.print") as printed:
                self.assertEqual(
                    inputs.main(["--base-ref", "a", "--head-ref", "b"]), 0)
                printed.assert_any_call("rust=true")
                printed.assert_any_call("worker=true")
                printed.assert_any_call("parity=true")
                printed.assert_any_call("supply=true")
                printed.assert_any_call("docs=true")

    def test_cli_fails_closed_when_diff_is_undecidable(self):
        import subprocess
        with patch.object(inputs, "git_diff_names",
                          side_effect=subprocess.CalledProcessError(128, "git")):
            with patch("builtins.print"):
                self.assertEqual(
                    inputs.main(["--base-ref", "a", "--head-ref", "b"]), 2)


class ReaderCoverageTests(unittest.TestCase):
    """Every doc-literal reader must agree with the classifier.

    If a new docs/ read lands in Rust/scripts/wrangler code, this suite
    forces the RUST_DOCS/WORKER_DOCS/PARITY_DOCS sets to follow it.
    """

    # Docs the preconditions binary checks by existence only (is_file, never
    # content): a content edit cannot break Rust, and a deletion forces all
    # jobs via the deletion rule. Every other docs/ literal in Rust is a
    # content read and must select the rust job.
    EXISTENCE_ONLY_DOCS = frozenset({
        "docs/staging-soak.md",
        "docs/backup.md",
        "docs/preflight.md",
        "docs/runbook.md",
    })

    def test_rust_doc_literals_are_classified_rust(self):
        literals = set()
        for path in list((ROOT / "crates").rglob("*.rs")) + (
                [ROOT / "src/lib.rs"] if (ROOT / "src/lib.rs").exists() else []):
            for line in path.read_text().splitlines():
                if line.strip().startswith("//"):
                    continue
                literals.update(
                    re.findall(r'"(docs/[A-Za-z0-9_./-]+\.md)"', line))
        self.assertTrue(literals, "expected Rust doc literals")
        for doc in self.EXISTENCE_ONLY_DOCS:
            self.assertIn(doc, literals, f"{doc} left Rust sources")
        for literal in sorted(literals):
            with self.subTest(doc=literal):
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

    def test_selector_exposes_five_outputs(self):
        outputs = self.selector.split("outputs:", 1)[1].split("steps:", 1)[0]
        for job in ("rust", "worker", "parity", "supply", "docs"):
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
        for job in ("check", "community-db", "feeds-db", "tickets-postgres",
                    "worker", "required-checks", "supply-chain", "ci-ok"):
            head = self.text.split(f"\n  {job}:")[1].split("steps:", 1)[0]
            with self.subTest(job=job):
                self.assertIn("job-inputs", head)

    def test_supply_chain_job_is_selector_gated(self):
        # CI standard rule 6 (TOG-14881): the SBOM scan runs only when the
        # `supply` area changed; docs-only and worker-UI-only PRs skip it.
        head = self.text.split("\n  supply-chain:")[1].split("steps:", 1)[0]
        self.assertIn("needs.job-inputs.outputs.supply != 'false'", head)

    def test_check_job_supply_guard_is_conditional(self):
        # A skipped supply-chain job must not fail the always-run `check`
        # job; the guard only enforces the gate when supply was selected.
        body = self.text.split("\n  check:", 1)[1].split(
            "\n  parity-docs:", 1)[0]
        self.assertIn(
            "needs.supply-chain.result != 'success' "
            "&& needs.job-inputs.outputs.supply != 'false'", body)

    def test_ci_ok_aggregator_exists_and_is_always_run(self):
        # CI standard rule 3 (TOG-14881): one required aggregator named
        # `ci-ok`, running on every event, passing only when every needed
        # job succeeded or was legitimately skipped.
        head = self.text.split("\n  ci-ok:")[1].split("steps:", 1)[0]
        self.assertIn("always()", head)
        for job in ("job-inputs", "supply-chain", "check", "worker",
                    "parity-docs", "self-role-store", "community-db",
                    "feeds-db", "tickets-postgres", "moderation-db"):
            self.assertIn(job, head)
        body = self.text.split("\n  ci-ok:")[1]
        self.assertIn("SUPPLY_SELECTED", body)
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
    def check_job_steps(text):
        """Yield (name, body) for each `check` job step; comments dropped."""
        body = text.split("\n  check:", 1)[1].split("\n  parity-docs:", 1)[0]
        body = body.split("\n    steps:\n", 1)[1]
        for block in re.split(r"(?m)^      - (?=\S)", body)[1:]:
            lines = [line for line in block.splitlines()
                     if not line.lstrip().startswith("#")]
            name = re.search(r"name:\s*(.+)", "\n".join(lines))
            yield (name.group(1).strip() if name else lines[0].strip(),
                   "\n".join(lines))

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

    def test_every_rust_or_db_step_in_check_job_is_gated(self):
        steps = list(self.check_job_steps(self.text))
        self.assertGreater(len(steps), 40, "step parser lost the check job")
        for name, body in steps:
            if name.startswith(self.ALWAYS_ON) or not self.NEEDS_RUST.search(body):
                continue
            with self.subTest(step=name):
                self.assertRegex(
                    body,
                    r"(?m)^\s+if:.*needs\.job-inputs\.outputs\.rust != 'false'",
                    f"{name!r} needs the Rust toolchain or the CI database "
                    "but is not guarded by the rust selector")

    def test_guard_scan_flags_an_unguarded_db_step(self):
        # Self-test of the scan: dropping the guard on the step that failed
        # PR #346 must be caught, and the real step must satisfy it.
        needle = "      - name: Durable Discord send admission"
        head, tail = self.text.split(needle, 1)
        step, rest = tail.split("\n      - name:", 1)
        guard = "        if: needs.job-inputs.outputs.rust != 'false'\n"
        self.assertIn(guard, step)
        weakened = head + needle + step.replace(guard, "") + "\n      - name:" + rest
        flagged = [name for name, body in self.check_job_steps(weakened)
                   if self.NEEDS_RUST.search(body)
                   and not re.search(r"(?m)^\s+if:.*outputs\.rust != 'false'", body)]
        self.assertEqual(
            flagged, ["Durable Discord send admission (isolated DBs and mock HTTP only)"])

    def test_check_job_needs_parity_docs(self):
        head = self.text.split("\n  check:")[1].split("steps:", 1)[0]
        self.assertIn("parity-docs", head)
        self.assertIn("always()", head)

    def test_check_job_still_needs_self_role_store(self):
        head = self.text.split("\n  check:")[1].split("steps:", 1)[0]
        self.assertIn("self-role-store", head)

    def test_push_to_main_selects_everything(self):
        self.assertIn("pull_request", self.selector)
        self.assertIn("rust=true", self.selector)


if __name__ == "__main__":
    unittest.main()
