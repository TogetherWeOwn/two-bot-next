"""Mutation checks for the disabled-only admission predicate."""

from copy import deepcopy
import unittest

from disabled_voice_measurement import GUARDS, is_disabled_scaffold


class DisabledVoiceMeasurementTests(unittest.TestCase):
    def test_only_literal_disabled_graph_is_admitted(self):
        scaffold = {
            "on": {"workflow_dispatch": None},
            "permissions": {},
            "concurrency": {
                "group": "${{ false && github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main' "
                "&& github.run_attempt == 1 && 'staging-lifecycle' || "
                "format('voice-rest-measurement-disabled-{0}-{1}', github.run_id, github.run_attempt) }}",
                "cancel-in-progress": "false", "queue": "max",
            },
            "jobs": {name: {"if": guard} for name, guard in GUARDS.items()},
        }
        self.assertTrue(is_disabled_scaffold(scaffold))
        for name in GUARDS:
            with self.subTest(job=name):
                changed = deepcopy(scaffold)
                changed["jobs"][name]["if"] = GUARDS[name].replace("false && ", "", 1)
                self.assertFalse(is_disabled_scaffold(changed))
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
                self.assertFalse(is_disabled_scaffold(changed))
        queue = scaffold["concurrency"]
        for guard in ("false", "github.event_name == 'workflow_dispatch'",
                      "github.ref == 'refs/heads/main'", "github.run_attempt == 1"):
            with self.subTest(lock_guard=guard):
                changed = dict(scaffold, concurrency=dict(queue, group=queue["group"].replace(guard + " && ", "", 1)))
                self.assertFalse(is_disabled_scaffold(changed))
        for key, value in (("queue", "single"), ("cancel-in-progress", "true")):
            with self.subTest(lock_key=key):
                changed = dict(scaffold, concurrency=dict(queue, **{key: value}))
                self.assertFalse(is_disabled_scaffold(changed))
        for triggers in (None, 42, ["workflow_dispatch"], "workflow_dispatch"):
            with self.subTest(malformed_triggers=triggers):
                changed = deepcopy(scaffold)
                changed["on"] = triggers
                self.assertFalse(is_disabled_scaffold(changed))
        self.assertFalse(is_disabled_scaffold(None))

if __name__ == "__main__":
    unittest.main()
