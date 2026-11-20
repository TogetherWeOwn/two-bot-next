"""Mutation checks for the measurement admission predicate."""

from copy import deepcopy
import unittest

from voice_measurement_policy import CALLS, GUARDS, STAGING_JOBS, TARGET, is_measurement_caller


class VoiceMeasurementPolicyTests(unittest.TestCase):
    def test_only_the_expected_live_graph_is_admitted(self):
        scaffold = {
            "on": {"workflow_dispatch": ""},
            "permissions": {},
            "concurrency": {
                "group": "${{ github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main' "
                "&& github.run_attempt == 1 && 'staging-lifecycle' || "
                "format('voice-rest-measurement-refused-{0}-{1}', github.run_id, github.run_attempt) }}",
                "cancel-in-progress": "false", "queue": "max",
            },
            "jobs": {name: {"if": guard, "permissions": {} if name == "rerun-refusal" else {"contents": "read"}}
                     for name, guard in GUARDS.items()},
        }
        for name in STAGING_JOBS:
            scaffold["jobs"][name]["environment"] = "staging"
        for name in CALLS:
            scaffold["jobs"][name]["uses"] = TARGET
            scaffold["jobs"][name]["with"] = {
                "deploy_staging_call": "true", "artifact_invocation": name.removeprefix("voice-"),
                "staging_use_expires_at": "${{ needs.staging-window.outputs.expires_at }}",
                "staging_use_sha": "${{ needs.staging-window.outputs.sha }}",
                "staging_use_deployment": "${{ needs.staging-window.outputs.deployment_id }}",
            }
        self.assertTrue(is_measurement_caller(scaffold, "staging-lifecycle"))
        for name in GUARDS:
            with self.subTest(job=name):
                changed = deepcopy(scaffold)
                changed["jobs"][name]["if"] = "${{ true }}"
                self.assertFalse(is_measurement_caller(changed, "staging-lifecycle"))
        for change in ("job", "trigger", "queue", "permissions"):
            with self.subTest(change=change):
                changed = deepcopy(scaffold)
                if change == "job":
                    changed["jobs"]["other"] = {"if": "${{ false }}"}
                elif change == "trigger":
                    changed["on"]["push"] = None
                elif change == "queue":
                    changed["concurrency"]["group"] = "staging-lifecycle"
                else:
                    changed["permissions"] = {"contents": "read"}
                self.assertFalse(is_measurement_caller(changed, "staging-lifecycle"))
        queue = scaffold["concurrency"]
        for guard in ("github.event_name == 'workflow_dispatch'",
                      "github.ref == 'refs/heads/main'", "github.run_attempt == 1"):
            with self.subTest(lock_guard=guard):
                changed = dict(scaffold, concurrency=dict(queue, group=queue["group"].replace(guard + " && ", "", 1)))
                self.assertFalse(is_measurement_caller(changed, "staging-lifecycle"))
        for key, value in (("queue", "single"), ("cancel-in-progress", "true")):
            with self.subTest(lock_key=key):
                changed = dict(scaffold, concurrency=dict(queue, **{key: value}))
                self.assertFalse(is_measurement_caller(changed, "staging-lifecycle"))
        for triggers in (None, 42, ["workflow_dispatch"], "workflow_dispatch"):
            with self.subTest(malformed_triggers=triggers):
                changed = deepcopy(scaffold)
                changed["on"] = triggers
                self.assertFalse(is_measurement_caller(changed, "staging-lifecycle"))
        self.assertFalse(is_measurement_caller(None, "staging-lifecycle"))

if __name__ == "__main__":
    unittest.main()
