"""Mutation checks for the disabled-only admission predicate."""

from copy import deepcopy
import unittest

from disabled_voice_measurement import GUARDS, is_disabled_scaffold


class DisabledVoiceMeasurementTests(unittest.TestCase):
    def test_only_literal_disabled_graph_is_admitted(self):
        scaffold = {
            "on": {"workflow_dispatch": None},
            "permissions": {},
            "concurrency": {"group": "voice-rest-measurement-disabled",
                            "cancel-in-progress": "false"},
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
        for triggers in (None, 42, ["workflow_dispatch"], "workflow_dispatch"):
            with self.subTest(malformed_triggers=triggers):
                changed = deepcopy(scaffold)
                changed["on"] = triggers
                self.assertFalse(is_disabled_scaffold(changed))
        self.assertFalse(is_disabled_scaffold(None))

if __name__ == "__main__":
    unittest.main()
