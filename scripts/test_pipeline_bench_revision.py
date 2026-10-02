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
