import os
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/pipeline_bench_revision.sh"


class RevisionTests(unittest.TestCase):
    def setUp(self):
        scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("PAPERCLIP_SCRATCH_DIR")
        self.directory = tempfile.TemporaryDirectory(dir=scratch)
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root, text=True).strip()

    def checkout(self):
        self.git("init", "-q")
        self.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                 "commit", "-q", "--allow-empty", "-m", "fixture")
        return self.git("rev-parse", "HEAD")

    def record(self, pr_head=None):
        env = os.environ.copy()
        env.pop("PR_HEAD_SHA", None)
        if pr_head is not None:
            env["PR_HEAD_SHA"] = pr_head
        return subprocess.run(["sh", str(SCRIPT)], cwd=self.root, env=env,
                              capture_output=True, text=True, check=False)

    def test_checkout_and_pr_head_are_distinct_receipts(self):
        source = self.checkout()
        pr_head = "a" * 40
        self.assertNotEqual(source, pr_head)
        result = self.record(pr_head)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / "pipeline-benchmark-revision.txt").read_text(), source + "\n")
        self.assertEqual((self.root / "pipeline-benchmark-pr-head.txt").read_text(), pr_head + "\n")

    def test_non_pr_job_has_source_without_invented_pr_head(self):
        source = self.checkout()
        result = self.record()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / "pipeline-benchmark-revision.txt").read_text(), source + "\n")
        self.assertEqual((self.root / "pipeline-benchmark-pr-head.txt").read_text(), "\n")

    def test_missing_checkout_fails_without_event_metadata_fallback(self):
        result = self.record("a" * 40)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((self.root / "pipeline-benchmark-revision.txt").read_text(), "")
        self.assertFalse((self.root / "pipeline-benchmark-pr-head.txt").exists())

    def test_benchmark_and_normal_sweeps_provide_disposable_log_collectors(self):
        for filename, job in [("pipeline-benchmark.yml", "benchmark"),
                              ("check.yml", "rust-tests"), ("check.yml", "coverage-shard"),
                              ("nightly.yml", "sweep")]:
            workflow = (ROOT / ".github/workflows" / filename).read_text()
            service = workflow.split(f"  {job}:\n", 1)[1].split("    services:\n", 1)[1]
            service = service.split("    steps:\n", 1)[0]
            with self.subTest(workflow=filename, job=job):
                self.assertIn("POSTGRES_INITDB_ARGS:", service)
                self.assertIn("-c logging_collector=on -c log_destination=jsonlog", service)
                self.assertIn("-c log_rotation_age=0 -c log_rotation_size=0", service)
                if job == "benchmark":
                    self.assertIn("-c autovacuum=off", service)
                    self.assertIn("-c log_min_duration_statement=0", service)
                else:
                    self.assertNotIn("-c autovacuum=off", service)
                    self.assertNotIn("-c log_min_duration_statement=0", service)

    def test_pipeline_database_guards_run_without_ignored_test_opt_ins(self):
        durability = (ROOT / "crates/cutover/tests/pipeline_durability.rs").read_text()
        self.assertNotIn("#[ignore", durability)
        self.assertNotIn('"--ignored"', durability)
        integrity = (ROOT / "crates/testsupport/tests/pipeline_integrity.rs").read_text()
        self.assertNotIn("#[ignore", integrity)
        self.assertEqual(integrity.count("#[tokio::test]"), 3)
        workflow = (ROOT / ".github/workflows/check.yml").read_text()
        normal = workflow.split("  rust-tests:\n", 1)[1].split("  coverage-plan:\n", 1)[0]
        self.assertIn("TWO_TEST_DATABASE_URL:", normal)
        self.assertIn("cargo test --workspace --test '*'", normal)
        benchmark = (ROOT / ".github/workflows/pipeline-benchmark.yml").read_text()
        build = "cargo test -p two-bot-cutover --test pipeline_durability --locked --no-run"
        run = "timeout 120s cargo test -p two-bot-cutover --test pipeline_durability --locked -- --test-threads=1"
        self.assertIn(build, benchmark)
        self.assertIn(run, benchmark)
        self.assertLess(benchmark.index(build), benchmark.index(run))
        self.assertIn("cargo test -p two-bot-testsupport --test pipeline_integrity --locked", benchmark)

    def test_completion_log_processing_follows_historical_rss_sample(self):
        driver = (ROOT / "crates/discord/examples/pipeline_bench.rs").read_text()
        measure = driver.split("async fn measure(", 1)[1].split("#[tokio::main", 1)[0]
        peak = measure.index("let peak = peak_rss_mib()?;")
        # Keep all historical row/journal/checkpoint verification inside VmHWM.
        for operation in ("SELECT count(*) FROM events", "SELECT count(*) FROM members",
                          "SELECT count(*), count(DISTINCT e.id)",
                          'checkpoint.sequence == sequence'):
            self.assertLess(measure.index(operation), peak)
        # JSON buffering/decoding can be as large as the 64 MiB log budget.
        self.assertGreater(measure.index("completed_statement_profile("), peak)
        self.assertGreater(measure.index("server.verify(config.events(), statements, commits)?;"), peak)
        self.assertLess(peak, measure.index('"peak_rss_mib":peak'))

    def test_only_new_key_oracle_is_excluded_from_handler_latency(self):
        driver = (ROOT / "crates/discord/examples/pipeline_bench.rs").read_text()
        commit = driver.split("async fn commit(", 1)[1].split("fn percentiles(", 1)[0]
        oracle = commit.split("let oracle_started = Instant::now();", 1)[1]
        oracle, persistence = oracle.split("let oracle_elapsed = oracle_started.elapsed();", 1)
        self.assertEqual(oracle.strip(),
                         "expected.extend(batch.events.iter().map(two_bot_core::idempotency_key));")
        self.assertIn("pipeline.handle_at(", commit.split("let oracle_started", 1)[0])
        self.assertIn("let checkpoint = GatewaySession", commit.split("let oracle_started", 1)[0])
        self.assertIn("let batch = pipeline.handlers().store().take_batch();",
                      commit.split("let oracle_started", 1)[0])
        self.assertIn("store.commit_dispatch(&checkpoint, batch).await?", persistence)
        self.assertIn("Ok(oracle_elapsed)", persistence)
        timing = driver.split("let started = Instant::now();\n            let oracle_elapsed", 1)[1]
        timing = timing.split("let profile = event_profile.entry", 1)[0]
        self.assertIn("commit(&pipeline, &store, event, sequence, at_ms, &mut expected).await?;", timing)
        self.assertIn("started.elapsed().saturating_sub(oracle_elapsed).as_secs_f64()",
                      "".join(timing.split()))

    def test_workflow_runs_regressions_and_uploads_both_receipts(self):
        workflow = (ROOT / ".github/workflows/pipeline-benchmark.yml").read_text()
        self.assertIn("-p 'test*pipeline_bench*.py'", workflow)
        self.assertIn("PR_HEAD_SHA: ${{ github.event.pull_request.head.sha }}", workflow)
        self.assertIn("run: sh scripts/pipeline_bench_revision.sh", workflow)
        upload = workflow.split("- uses: actions/upload-artifact@", 1)[1]
        self.assertIn("            pipeline-benchmark-revision.txt\n", upload)
        self.assertIn("            pipeline-benchmark-pr-head.txt\n", upload)
        self.assertNotIn("github.event.pull_request.head.sha || github.sha", workflow)


if __name__ == "__main__":
    unittest.main()
