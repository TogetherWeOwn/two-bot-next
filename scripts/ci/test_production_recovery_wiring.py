"""Pin the protected workflow and legacy rollback entry points."""

from pathlib import Path
import sys
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
from production_recovery_artifact import FENCE_JOB, FENCE_STEP, JOB, UPLOAD_STEP, WRITE_STEP


class RecoveryWiringTests(unittest.TestCase):
    def test_workflow_keeps_secrets_scoped_and_all_three_rollbacks_share_the_guard(self):
        path = ROOT / ".github/workflows"
        workflow = yaml.safe_load((path / "deploy-production-recovery.yml").read_text())
        self.assertEqual(set(workflow[True]), {"workflow_call"})
        jobs = workflow["jobs"]
        self.assertEqual(jobs["inverse"]["environment"], "production-migrate-apply")
        self.assertEqual(jobs["inverse"]["needs"], "fence")
        self.assertEqual(jobs["restore"]["needs"], ["fence", "inverse"])
        self.assertNotIn("OWNERSHIP_CONTROL_TOKEN", str(jobs["inverse"]))
        for phase in ("fence", "restore"):
            self.assertNotIn("TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL", str(jobs[phase]))
        ordinary = yaml.safe_load((path / "deploy-production.yml").read_text())["jobs"]["production"]["steps"]
        for step_id in ("rollback", "auto_rollback", "watch_rollback"):
            command = next(step["run"] for step in ordinary if step.get("id") == step_id)
            self.assertIn("worker_container_rollback.py rollback", command)
            self.assertIn("--variant production", command)

    def test_callee_binds_the_caller_pins_and_exact_producer_jobs(self):
        path = ROOT / ".github/workflows/deploy-production-recovery.yml"
        callee = yaml.safe_load(path.read_text())
        call = callee[True]["workflow_call"]
        pins = {"PRODUCTION_HOST": "database-host", "PRODUCTION_DATABASE": "database-name",
                "PRODUCTION_BRANCH_ID": "database-branch-id"}
        self.assertEqual(call["inputs"], {
            key: {"type": "string", "required": True} for key in ("admission", *pins.values())
        })
        self.assertEqual(callee["env"], {
            variable: "${{ inputs." + pin + " }}" for variable, pin in pins.items()
        })
        jobs = callee["jobs"]
        prefix = "Production release / Recover production schema / "
        self.assertEqual(prefix + jobs["fence"]["name"], FENCE_JOB)
        self.assertEqual(prefix + jobs["inverse"]["name"], JOB)
        self.assertIn(FENCE_STEP, [step.get("name") for step in jobs["fence"]["steps"]])
        steps = [step.get("name") for step in jobs["inverse"]["steps"]]
        self.assertLess(steps.index(WRITE_STEP), steps.index(UPLOAD_STEP))
        self.assertEqual(jobs["inverse"]["outputs"]["archive_sha256"], "${{ steps.result.outputs.artifact-digest }}")
        self.assertEqual(jobs["restore"]["steps"][-1]["env"]["ARCHIVE_SHA256"],
                         "${{ needs.inverse.outputs.archive_sha256 }}")
