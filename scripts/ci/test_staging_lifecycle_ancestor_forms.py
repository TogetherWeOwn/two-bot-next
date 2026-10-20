"""Builder-owned companion pin for staging lifecycle ancestor lock forms.

Companion to the protected contract
``test_staging_voice_mutual_exclusion_acceptance.py`` (Test Author owned;
that blob stays byte-identical to its manifest and is never edited here).
The protected oracle recognises only dict-form literal groups on waiting
ancestors, so the reusable caller holding the shared lifecycle group via
string shorthand (``concurrency: staging-lifecycle``) or an
expression-derived group (``group: ${{ ... }}``) keeps the protected suite
green while the called synthetic deadlocks behind its own waiting caller.
Both forms are valid GitHub workflow syntax (actionlint-clean and schema
allowed). This module pins those forms rejected.

Only YAML is read and only in-memory copies are mutated; no workflow is
dispatched and no repository file is changed. These tests prove the
declared lock scope, not GitHub scheduler execution or runtime repair.
"""

from copy import deepcopy
from pathlib import Path
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
DEPLOY = ROOT / ".github/workflows/deploy-staging.yml"
VOICE = ROOT / ".github/workflows/voice-synthetic.yml"
EXPRESSION = "${{"


class UniqueKeyLoader(yaml.BaseLoader):
    # BaseLoader keeps Actions' `on` and booleans as strings, like the
    # protected contract and test_workflows.
    def construct_mapping(self, node, deep=False):
        result = {}
        for key_node, value_node in node.value:
            key = self.construct_object(key_node, deep=deep)
            if key in result:
                raise ValueError(f"duplicate workflow key: {key}")
            result[key] = self.construct_object(value_node, deep=deep)
        return result


def load(path):
    return yaml.load(path.read_text(), Loader=UniqueKeyLoader)


def real_workflows():
    return load(DEPLOY), load(VOICE)


def execution_group(job):
    """Fixed literal group of an execution job, or None when not provable."""
    lock = job.get("concurrency")
    if not isinstance(lock, dict):
        return None
    group = lock.get("group")
    if not isinstance(group, str) or not group.strip() or EXPRESSION in group:
        return None
    # GitHub concurrency group matching is case-insensitive.
    return group.casefold()


def ancestor_hold(ancestor):
    """Hold verdict for a waiting ancestor node.

    Returns "absent" when the node declares no concurrency,
    "fixed:<group>" for a literal group (string shorthand or dict), and
    "expression" when the group is expression-derived or otherwise not a
    provable literal, which cannot be proven disjoint from the shared
    lifecycle group.
    """
    lock = ancestor.get("concurrency")
    if lock is None:
        return "absent"
    if isinstance(lock, str):
        if not lock.strip():
            return "absent"
        if EXPRESSION in lock:
            return "expression"
        return f"fixed:{lock.casefold()}"
    if isinstance(lock, dict):
        group = lock.get("group")
        if group is None:
            return "absent"
        if not isinstance(group, str) or EXPRESSION in group:
            return "expression"
        return f"fixed:{group.casefold()}"
    return "expression"


def ancestor_holds_lifecycle(ancestor, lifecycle):
    return ancestor_hold(ancestor) in ("expression", f"fixed:{lifecycle}")


class CallerLockFormTests(unittest.TestCase):
    def lifecycle(self, deploy, voice):
        deploy_group = execution_group(deploy["jobs"]["deploy"])
        voice_group = execution_group(voice["jobs"]["synthetic"])
        self.assertTrue(deploy_group)
        self.assertEqual(voice_group, deploy_group)
        return deploy_group

    def test_real_caller_declares_no_concurrency_in_any_form(self):
        deploy, _ = real_workflows()
        self.assertNotIn("concurrency", deploy["jobs"]["voice-synthetic"])

    def test_real_ancestors_are_disjoint_from_lifecycle_group(self):
        deploy, voice = real_workflows()
        lifecycle = self.lifecycle(deploy, voice)
        ancestors = (("deploy workflow", deploy), ("voice workflow", voice),
                     ("reusable caller", deploy["jobs"]["voice-synthetic"]))
        for label, ancestor in ancestors:
            with self.subTest(ancestor=label):
                self.assertFalse(ancestor_holds_lifecycle(ancestor, lifecycle))

    def test_string_shorthand_caller_lock_is_rejected(self):
        deploy, voice = real_workflows()
        lifecycle = self.lifecycle(deploy, voice)
        spelling = deploy["jobs"]["deploy"]["concurrency"]["group"]
        for candidate in (spelling, spelling.upper(), spelling.swapcase()):
            caller = deepcopy(deploy["jobs"]["voice-synthetic"])
            caller["concurrency"] = candidate
            with self.subTest(candidate=candidate):
                self.assertTrue(ancestor_holds_lifecycle(caller, lifecycle))

    def test_expression_derived_caller_lock_is_rejected(self):
        deploy, voice = real_workflows()
        lifecycle = self.lifecycle(deploy, voice)
        spelling = deploy["jobs"]["deploy"]["concurrency"]["group"]
        expressions = (f"{EXPRESSION} '{spelling}' }}",
                       f"{EXPRESSION} vars.LIFECYCLE_GROUP }}",
                       f"{spelling}-{EXPRESSION} github.run_id }}",
                       f"{EXPRESSION} github.workflow }}")
        for expression in expressions:
            shapes = (expression, {"group": expression,
                                   "cancel-in-progress": "false"})
            for shape in shapes:
                caller = deepcopy(deploy["jobs"]["voice-synthetic"])
                caller["concurrency"] = shape
                with self.subTest(expression=expression,
                                   shape="string" if isinstance(shape, str) else "dict"):
                    self.assertTrue(ancestor_holds_lifecycle(caller, lifecycle))

    def test_dict_literal_caller_lock_is_rejected(self):
        # Agreement control with the protected oracle on the form it catches.
        deploy, voice = real_workflows()
        lifecycle = self.lifecycle(deploy, voice)
        caller = deepcopy(deploy["jobs"]["voice-synthetic"])
        caller["concurrency"] = {
            "group": deploy["jobs"]["deploy"]["concurrency"]["group"],
            "cancel-in-progress": "false",
        }
        self.assertTrue(ancestor_holds_lifecycle(caller, lifecycle))

    def test_unrelated_fixed_caller_group_is_accepted(self):
        # Non-vacuousness: a disjoint fixed group cannot deadlock the
        # lifecycle job, in either YAML shape.
        deploy, voice = real_workflows()
        lifecycle = self.lifecycle(deploy, voice)
        shapes = ("unrelated-caller-group", {"group": "unrelated-caller-group",
                                             "cancel-in-progress": "false"})
        for shape in shapes:
            caller = deepcopy(deploy["jobs"]["voice-synthetic"])
            caller["concurrency"] = shape
            with self.subTest(shape="string" if isinstance(shape, str) else "dict"):
                self.assertFalse(ancestor_holds_lifecycle(caller, lifecycle))


if __name__ == "__main__":
    unittest.main()
