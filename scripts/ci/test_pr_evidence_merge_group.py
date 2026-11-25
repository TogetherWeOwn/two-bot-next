"""Offline regression for the PR evidence verifier's merge-group boundary."""

import os
from pathlib import Path
import runpy
import sys
import tempfile
import unittest
from unittest.mock import patch


class MergeGroupEvidenceTests(unittest.TestCase):
    def test_verify_refuses_merge_group_before_publishing(self):
        script = Path(__file__).resolve().parents[2] / ".github/scripts/pr_evidence.py"
        group_head = "a" * 40
        run = f"{group_head}\tmerge_group\t.github/workflows/pr-evidence.yml\t12\t1\n"
        job = f"34\t{group_head}\t12\t1\tin_progress\tpr-evidence\n"
        with tempfile.TemporaryDirectory() as directory:
            summary = Path(directory) / "summary"
            env = {
                "EVIDENCE_HEAD": group_head,
                "GITHUB_EVENT_NAME": "merge_group",
                "GITHUB_REPOSITORY": "example/repo",
                "GITHUB_RUN_ID": "12",
                "GITHUB_RUN_ATTEMPT": "1",
                "GITHUB_STEP_SUMMARY": str(summary),
            }
            with (patch.dict(os.environ, env, clear=True),
                  patch.object(sys, "argv", [str(script), "verify"]),
                  patch("subprocess.check_output", side_effect=[run.encode(), job.encode()]) as api):
                with self.assertRaisesRegex(SystemExit, "authoritative PR membership"):
                    runpy.run_path(str(script), run_name="__main__")
            api.assert_not_called()
            self.assertFalse(summary.exists())
