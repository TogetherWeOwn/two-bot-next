"""Lock the secret-scan trigger/concurrency surface; no GitHub or PyYAML needed.

TOG-12059 landed this guard on the standalone secret-scan.yml; TOG-11810
folded that workflow into supply-chain.yml, so the guard now reads the
folded workflow. The workflow has no inline script to exec (unlike
pr-lint), so this test parses the YAML as text the way
scripts/test-pr-lint.py does: indentation aware, stdlib only, hermetic.

Guards:
- pull_request fires on opened/synchronize/reopened/ready_for_review, plus
  `edited` for the co-hosted pr-lint job (a title/body edit must re-run
  the convention check).
- gitleaks is never skipped by an edit or a failed pr-lint. An `edited` run
  shares the concurrency group, so it can cancel the synchronize run's scan;
  a skipped gitleaks would then pass the required check with the head never
  scanned. Only `!cancelled()` may gate the job.
- push to main is untouched: every main commit still gets a scan.
- workflow_dispatch is untouched: release-please PRs opened with
  GITHUB_TOKEN trigger no workflows, so the release dispatch in
  release.yml is the only path that runs the required checks there.
- concurrency keys on the PR number (or dispatch input) with
  cancel-in-progress, so superseded pushes cancel the stale scan instead
  of stacking duplicates; push-to-main falls back to the commit SHA, so
  back-to-back merges never cancel each other's scan.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github/workflows/supply-chain.yml"

REQUIRED_PR_TYPES = ["opened", "edited", "synchronize", "reopened", "ready_for_review"]


def section_lines(text, header, indent):
    """Return the raw lines belonging to the block opened by `header`."""
    lines = text.splitlines()
    start = next(i for i, line in enumerate(lines) if line.strip() == header)
    base = len(lines[start]) - len(lines[start].lstrip())
    body = []
    for line in lines[start + 1:]:
        if not line.strip():
            continue
        depth = len(line) - len(line.lstrip())
        if depth <= base and re.match(r"^\s*\S", line):
            break
        body.append(line)
    return body, base


def flow_list(value):
    return [item.strip().strip("'\"") for item in value.strip().strip("[]").split(",")]


class SecretScanSurfaceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = WORKFLOW.read_text()

    def test_pull_request_types_cover_code_activity_and_edits(self):
        on_block, _ = section_lines(self.text, "on:", 0)
        pr_line = next(line for line in on_block if re.match(r"^\s*pull_request:\s*$", line))
        pr_indent = len(pr_line) - len(pr_line.lstrip())
        types_line = next(
            line for line in on_block
            if len(line) - len(line.lstrip()) > pr_indent and line.strip().startswith("types:")
        )
        self.assertEqual(flow_list(types_line.split("types:", 1)[1]), REQUIRED_PR_TYPES)

    def test_gitleaks_never_skipped_by_edit_or_failed_lint(self):
        # An `edited` run can cancel the synchronize run's scan (same
        # concurrency group), and a skipped job passes a required check, so
        # the edit run must rescan. `needs: pr-lint` alone would also skip
        # the scan whenever pr-lint fails, including on a main push.
        job_block, _ = section_lines(self.text, "gitleaks:", 2)
        keys = [line.strip() for line in job_block
                if len(line) - len(line.lstrip()) == 4 and not line.strip().startswith("#")]
        self.assertFalse(any("edited" in key or "event.action" in key for key in keys))
        if_lines = [key for key in keys if key.startswith("if:")]
        self.assertEqual(if_lines, ["if: ${{ !cancelled() }}"])

    def test_push_to_main_untouched(self):
        on_block, _ = section_lines(self.text, "on:", 0)
        push_line = next(line for line in on_block if re.match(r"^\s*push:\s*$", line))
        push_indent = len(push_line) - len(push_line.lstrip())
        branches_line = next(
            line for line in on_block
            if len(line) - len(line.lstrip()) > push_indent and line.strip().startswith("branches:")
        )
        self.assertEqual(flow_list(branches_line.split("branches:", 1)[1]), ["main"])

    def test_workflow_dispatch_release_path_untouched(self):
        on_block, _ = section_lines(self.text, "on:", 0)
        self.assertTrue(
            any(re.match(r"^\s*workflow_dispatch:\s*$", line) for line in on_block),
            "release.yml dispatches supply-chain.yml on release-please branches",
        )

    def test_concurrency_cancels_superseded_scans(self):
        concurrency_block, _ = section_lines(self.text, "concurrency:", 0)
        group_line = next(line for line in concurrency_block if line.strip().startswith("group:"))
        group = group_line.split("group:", 1)[1]
        self.assertIn("github.event.pull_request.number", group)
        # #217: main pushes key on the SHA so every main commit keeps its scan.
        self.assertRegex(group, r"\|\|\s*github\.sha\s*\}\}\s*$")
        self.assertNotIn("github.ref", group)
        cancel_line = next(line for line in concurrency_block if line.strip().startswith("cancel-in-progress:"))
        self.assertEqual(cancel_line.split("cancel-in-progress:", 1)[1].strip(), "true")

    def test_job_still_named_gitleaks_required_check(self):
        self.assertRegex(self.text, r"(?m)^\s*name:\s*gitleaks\s*$")


if __name__ == "__main__":
    unittest.main()
