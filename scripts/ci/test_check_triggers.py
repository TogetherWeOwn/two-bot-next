"""Lock the check workflow's minimal pull_request trigger surface.

With no `types`, GitHub fires `pull_request` only on opened/synchronize/
reopened: label, assignment, review-request and close events never run this
workflow, so there is no label-event waste to remove. The list is written out
explicitly (identical to the bare-trigger default) so this test can lock it:
check.yml never reads the PR title or body, so adding `edited` would re-run
(and, via cancel-in-progress, restart) the full matrix on an unchanged SHA,
and adding `ready_for_review` would re-run the same SHA with no draft gating.
Title re-lint on `edited` lives in supply-chain.yml, where it belongs.

Guards (indentation aware, stdlib only, hermetic, no GitHub needed):
- pull_request fires exactly on opened/synchronize/reopened and nothing
  else: no `edited` (title/body edits cannot change a result here) and no
  `ready_for_review` (same SHA, no draft gating).
- no paths/branches filter on the trigger: a filtered trigger leaves
  required checks pending (see scripts/ci/test_required_checks.py).
- push to main, the weekly schedule and the release dispatch are untouched.
- concurrency still cancels superseded PR runs instead of stacking them.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github/workflows/check.yml"

REQUIRED_PR_TYPES = ["opened", "synchronize", "reopened"]
# Same-SHA metadata events that must never re-run the matrix here.
BANNED_PR_TYPES = {"edited", "ready_for_review"}
# A paths/branches filter on the trigger leaves required checks pending.
PENDING_FILTERS = {"paths", "paths-ignore", "branches", "branches-ignore"}


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


class CheckTriggerSurfaceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = WORKFLOW.read_text()

    def on_block(self):
        on_block, _ = section_lines(self.text, "on:", 0)
        return on_block

    def pr_types(self):
        on_block = self.on_block()
        pr_line = next(line for line in on_block if re.match(r"^\s*pull_request:\s*$", line))
        pr_indent = len(pr_line) - len(pr_line.lstrip())
        types_line = next(
            line for line in on_block
            if len(line) - len(line.lstrip()) > pr_indent and line.strip().startswith("types:")
        )
        return flow_list(types_line.split("types:", 1)[1])

    def test_pull_request_types_are_the_minimal_default_triple(self):
        self.assertEqual(self.pr_types(), REQUIRED_PR_TYPES)

    def test_same_sha_metadata_events_do_not_rerun_the_matrix(self):
        # `edited` and `ready_for_review` keep the head SHA and check.yml
        # reads neither the title nor the body, so those runs can only burn
        # minutes and, via cancel-in-progress, delay time-to-green.
        self.assertFalse(set(self.pr_types()) & BANNED_PR_TYPES)

    def test_new_head_sha_events_are_covered(self):
        # Required checks report per head SHA; a new SHA that triggers no run
        # leaves the gate pending. Every new SHA arrives via opened (new PR)
        # or synchronize (new push), so both must stay.
        types = self.pr_types()
        self.assertIn("opened", types)
        self.assertIn("synchronize", types)

    def test_trigger_has_no_pending_forever_filters(self):
        on_block = self.on_block()
        start = next(i for i, line in enumerate(on_block)
                     if re.match(r"^\s*pull_request:\s*$", line))
        pr_indent = len(on_block[start]) - len(on_block[start].lstrip())
        keys = set()
        for line in on_block[start + 1:]:
            depth = len(line) - len(line.lstrip())
            if depth <= pr_indent:
                break
            if depth == pr_indent + 2 and re.match(r"^\s*[a-z-]+:", line):
                keys.add(line.strip().split(":", 1)[0])
        self.assertFalse(keys & PENDING_FILTERS, f"filtered trigger leaves checks pending: {keys}")

    def test_push_schedule_and_dispatch_untouched(self):
        on_block = self.on_block()
        push_line = next(line for line in on_block if re.match(r"^\s*push:\s*$", line))
        push_indent = len(push_line) - len(push_line.lstrip())
        branches_line = next(
            line for line in on_block
            if len(line) - len(line.lstrip()) > push_indent and line.strip().startswith("branches:")
        )
        self.assertEqual(flow_list(branches_line.split("branches:", 1)[1]), ["main"])
        self.assertTrue(
            any(re.match(r"^\s*schedule:\s*$", line) for line in on_block),
            "weekly full-matrix schedule must stay",
        )
        self.assertTrue(
            any(re.match(r"^\s*workflow_dispatch:\s*$", line) for line in on_block),
            "release workflow dispatches check on release-please branches",
        )

    def test_concurrency_cancels_superseded_runs(self):
        concurrency_block, _ = section_lines(self.text, "concurrency:", 0)
        group_line = next(line for line in concurrency_block if line.strip().startswith("group:"))
        group = group_line.split("group:", 1)[1]
        self.assertIn("github.event.pull_request.number", group)
        cancel_line = next(line for line in concurrency_block if line.strip().startswith("cancel-in-progress:"))
        self.assertEqual(cancel_line.split("cancel-in-progress:", 1)[1].strip(), "true")


if __name__ == "__main__":
    unittest.main()
