"""Scheduled staging observes finite authority; idle is not a health sample."""

from copy import deepcopy
import unittest
from unittest.mock import patch

import test_workflows as policy


class ScheduledStagingIdleTests(unittest.TestCase):
    def setUp(self):
        self.workflow = policy.load_workflows()["synthetic-user-flow.yml"]

    def test_observer_and_alert_credential_scopes_are_disjoint(self):
        self.assertEqual(policy.synthetic_user_flow_errors(self.workflow), [])
        job = self.workflow["jobs"]["staging"]
        self.assertEqual(job["environment"], {"name": "staging", "deployment": "false"})
        for step in job["steps"]:
            if step.get("id") != "window":
                self.assertNotIn("OWNERSHIP_CONTROL_TOKEN", str(step))
        prod = self.workflow["jobs"]["prod-passive"]
        self.assertNotIn("environment", prod)
        self.assertNotIn("needs", prod)
        self.assertNotIn("window", str(prod))

    def test_observer_gates_and_evidence_cannot_be_weakened(self):
        mutations = {
            "acquire instead of observe": lambda s: s[2].update(
                run="node wrangler/scripts/staging-use.mjs acquire"),
            "conditional observer": lambda s: s[2].update(**{"if": "false"}),
            "tolerated refusal": lambda s: s[2].update(**{"continue-on-error": "true"}),
            "control token in probe": lambda s: s[3]["env"].update(
                TOKEN="${{ secrets.STAGING_OWNERSHIP_CONTROL_TOKEN }}"),
            "probe while idle": lambda s: s[3].update(**{"if": "vars.STAGING_WORKER_URL != ''"}),
            "decide recovery while idle": lambda s: s[6].update(**{"if": "always()"}),
            "no receipt on failure": lambda s: s[8].update(**{"if": "success()"}),
            "missing receipt passes": lambda s: s[8]["with"].update(**{"if-no-files-found": "ignore"}),
            "secret in setup": lambda s: s[1].update(env={"TOKEN": "${{ secrets.STAGING_OWNERSHIP_CONTROL_TOKEN }}"}),
        }
        for label, mutate in mutations.items():
            with self.subTest(label=label):
                changed = deepcopy(self.workflow)
                mutate(changed["jobs"]["staging"]["steps"])
                self.assertTrue(policy.synthetic_user_flow_errors(changed))

    def test_callers_forward_exactly_the_two_named_tokens(self):
        workflows = policy.load_workflows()
        expected = {
            "TWO_VOICE_SYNTHETIC_BOT_TOKEN": "${{ secrets.TWO_VOICE_SYNTHETIC_BOT_TOKEN }}",
            "STAGING_OWNERSHIP_CONTROL_TOKEN": "${{ secrets.STAGING_OWNERSHIP_CONTROL_TOKEN }}",
        }
        for job_id in ("voice-synthetic", "help-embed-synthetic"):
            with self.subTest(job=job_id):
                self.assertEqual(workflows["deploy-staging.yml"]["jobs"][job_id]["secrets"], expected)

    def test_callee_control_declaration_is_required(self):
        workflows = policy.load_workflows()
        for name, consumer in (("voice-synthetic.yml", "synthetic"),
                               ("help-embed-synthetic.yml", "help-embed-smoke")):
            real = workflows[name]
            self.assertEqual(policy.staging_callee_window_errors(name, real, consumer), [])
            for required in (None, "false"):
                changed = deepcopy(real)
                secrets = changed["on"]["workflow_call"]["secrets"]
                if required is None:
                    secrets.pop("STAGING_OWNERSHIP_CONTROL_TOKEN")
                else:
                    secrets["STAGING_OWNERSHIP_CONTROL_TOKEN"]["required"] = required
                with self.subTest(workflow=name, required=required):
                    self.assertTrue(policy.staging_callee_window_errors(name, changed, consumer))

    def test_caller_token_wiring_mutations_fail_closed(self):
        real = policy.load_workflows()["deploy-staging.yml"]
        self.assertEqual(policy.staging_full_window_errors(real), [])
        mutations = {
            "inherit": lambda j: j.update(secrets="inherit"),
            "missing control": lambda j: j["secrets"].pop("STAGING_OWNERSHIP_CONTROL_TOKEN"),
            "missing test bot": lambda j: j["secrets"].pop("TWO_VOICE_SYNTHETIC_BOT_TOKEN"),
            "extra secret": lambda j: j["secrets"].update(UNREVIEWED_TOKEN="${{ secrets.UNREVIEWED_TOKEN }}"),
            "control from test bot": lambda j: j["secrets"].update(
                STAGING_OWNERSHIP_CONTROL_TOKEN="${{ secrets.TWO_VOICE_SYNTHETIC_BOT_TOKEN }}"),
            "test bot from control": lambda j: j["secrets"].update(
                TWO_VOICE_SYNTHETIC_BOT_TOKEN="${{ secrets.STAGING_OWNERSHIP_CONTROL_TOKEN }}"),
            "renamed control": lambda j: j["secrets"].update(
                STAGING_OWNERSHIP_CONTROL_TOKEN="${{ secrets.OTHER_CONTROL_TOKEN }}"),
        }
        for job_id in ("voice-synthetic", "help-embed-synthetic"):
            for label, mutate in mutations.items():
                with self.subTest(job=job_id, mutation=label):
                    changed = deepcopy(real)
                    mutate(changed["jobs"][job_id])
                    self.assertTrue(policy.staging_full_window_errors(changed))

    def test_idle_history_never_creates_sample_or_closed_checkpoint(self):
        history = policy._HISTORY_MODULE
        run = {"id": 100, "run_number": 100, "event": "schedule", "conclusion": "success"}
        for conclusion in ("success", "failure", "cancelled"):
            job = {"name": history.JOB_NAMES["staging"], "conclusion": conclusion,
                   "steps": [{"name": history.PROBE_STEPS["staging"], "conclusion": "skipped"},
                             {"name": history.DECIDE_STEPS["staging"], "conclusion": "skipped"},
                             {"name": history.ALERT_STEPS["staging"], "conclusion": "skipped"}]}
            with self.subTest(conclusion=conclusion), patch.object(history, "_get", return_value={"jobs": [job]}):
                self.assertIsNone(history._run_outcome("fixture/repo", run, "staging", "fixture"))
        previous = [("failure", "success"), ("failure", "success")]
        self.assertEqual(history.incident_state(previous), (2, True, True))
        self.assertEqual(history.transition("success", previous), "recovery")


if __name__ == "__main__":
    unittest.main()
