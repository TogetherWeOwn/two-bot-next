"""Companion pin for staging lifecycle ancestor lock forms.

Companion to the protected contract
``test_staging_voice_mutual_exclusion_acceptance.py`` (QA owned since the
full-window replacement). The previous protected oracle recognised only
dict-form literal groups on waiting ancestors, so a reusable caller holding
the lifecycle group via string shorthand (``concurrency: staging-lifecycle``)
or an expression-derived group kept it green while the called consumer
deadlocked behind its own waiting caller. The replacement oracle classifies
every form itself; this module pins that classification directly against the
oracle's own helpers, so a regression there cannot pass both files.

Waiting ancestors under the full-window contract are the two reusable caller
jobs in deploy-staging, the called voice/help workflows (and their jobs) in
called context, and the scheduled rollback-proof orchestrator.

Only YAML is read and only in-memory copies are mutated; no workflow is
dispatched and no repository file is changed. These tests prove the
declared lock scope, not GitHub scheduler execution or runtime repair.
"""

from copy import deepcopy
import importlib.util
from pathlib import Path
import unittest

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location(
    "staging_lifecycle_oracle", HERE / "test_staging_voice_mutual_exclusion_acceptance.py")
oracle = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(oracle)

EXPRESSION = oracle.EXPRESSION
LIFECYCLE = "fixture-staging-lifecycle"


def caller_holds(caller, canonical=LIFECYCLE):
    # Callers run in the deploy-staging window: standalone evaluation, own events.
    return oracle.may_hold(caller, "standalone", ("push", "workflow_dispatch"), canonical)


def callee_holds(node, canonical=LIFECYCLE):
    return oracle.may_hold(node, "called", (), canonical)


class CallerLockFormTests(unittest.TestCase):
    def callers(self):
        deploy = oracle.load_all()[oracle.DEPLOY]
        return {name: deploy["jobs"][name] for name in oracle.CHILDREN}

    def test_real_callers_declare_no_concurrency_in_any_form(self):
        for name, caller in self.callers().items():
            with self.subTest(caller=name):
                self.assertNotIn("concurrency", caller)

    def test_string_shorthand_caller_lock_is_rejected(self):
        for name, real in self.callers().items():
            for candidate in (LIFECYCLE, LIFECYCLE.upper(), LIFECYCLE.swapcase()):
                caller = deepcopy(real)
                caller["concurrency"] = candidate
                with self.subTest(caller=name, candidate=candidate):
                    self.assertTrue(caller_holds(caller))

    def test_expression_derived_caller_lock_is_rejected(self):
        expressions = (f"{EXPRESSION} '{LIFECYCLE}' }}}}",
                       f"{EXPRESSION} vars.LIFECYCLE_GROUP }}}}",
                       f"{LIFECYCLE}-{EXPRESSION} github.run_id }}}}",
                       f"{EXPRESSION} github.workflow }}}}",
                       oracle.called_group("staging-voice-consumer-", LIFECYCLE))
        for name, real in self.callers().items():
            for expression in expressions:
                for shape in (expression, {"group": expression, "cancel-in-progress": "false"}):
                    caller = deepcopy(real)
                    caller["concurrency"] = shape
                    with self.subTest(caller=name, expression=expression,
                                      shape="string" if isinstance(shape, str) else "dict"):
                        self.assertTrue(caller_holds(caller))

    def test_dict_literal_and_groupless_caller_locks_are_rejected(self):
        for name, real in self.callers().items():
            for shape in ({"group": LIFECYCLE, "cancel-in-progress": "false"},
                          {"cancel-in-progress": "false"}, ["not", "a", "lock"]):
                caller = deepcopy(real)
                caller["concurrency"] = shape
                with self.subTest(caller=name, shape=shape):
                    self.assertTrue(caller_holds(caller))

    def test_unrelated_fixed_caller_group_is_accepted(self):
        # Non-vacuousness: a disjoint fixed group cannot deadlock the window.
        for name, real in self.callers().items():
            for shape in ("unrelated-caller-group",
                          {"group": "unrelated-caller-group", "cancel-in-progress": "false"}):
                caller = deepcopy(real)
                caller["concurrency"] = shape
                with self.subTest(caller=name, shape=shape):
                    self.assertFalse(caller_holds(caller))


class NestedConsumerFormTests(unittest.TestCase):
    def test_proposed_called_form_is_disjoint_inside_and_canonical_outside(self):
        for prefix in ("staging-voice-consumer-", "staging-help-consumer-"):
            node = {"concurrency": {"group": oracle.called_group(prefix, LIFECYCLE),
                                    "cancel-in-progress": "false"}}
            with self.subTest(prefix=prefix):
                self.assertFalse(callee_holds(node))
                self.assertEqual(oracle.node_values(node, "standalone", ("workflow_dispatch",)),
                                 frozenset({("lit", LIFECYCLE)}))

    def test_called_form_whose_run_prefix_can_equal_canonical_is_rejected(self):
        canonical = "staging-run-12345"
        node = {"concurrency": oracle.called_group("staging-run-", "x")}
        self.assertTrue(callee_holds(node, canonical))
        self.assertFalse(callee_holds(node, "staging-run-x12345"))

    def test_unsafe_nested_forms_are_treated_as_holding(self):
        forms = (LIFECYCLE, LIFECYCLE.upper(), {"group": LIFECYCLE},
                 f"{EXPRESSION} github.workflow }}}}",
                 f"{EXPRESSION} inputs.staging_use_expires_at }}}}",
                 f"{LIFECYCLE}-{EXPRESSION} inputs.staging_use_sha }}}}",
                 f"{EXPRESSION} inputs.staging_use_expires_at == '' && "
                 f"format('x-{{0}}', github.run_id) || '{LIFECYCLE}' }}}}",
                 {"group": None}, {"group": ""}, " ")
        for form in forms:
            with self.subTest(form=form):
                self.assertTrue(callee_holds({"concurrency": deepcopy(form)}))

    def test_absent_or_unrelated_nested_group_is_accepted(self):
        for node in ({}, {"concurrency": "voice-synthetic"},
                     {"concurrency": {"group": "help-embed-synthetic",
                                      "cancel-in-progress": "false"}}):
            with self.subTest(node=node):
                self.assertFalse(callee_holds(node))

    def test_rollback_orchestrator_event_group_is_evaluated_per_trigger(self):
        node = {"concurrency": {"group": f"staging-rollback-proof-{EXPRESSION} github.event_name }}}}"}}
        triggers = ("schedule", "workflow_dispatch")
        self.assertFalse(oracle.may_hold(node, "standalone", triggers, LIFECYCLE))
        self.assertTrue(oracle.may_hold(node, "standalone", triggers,
                                        "staging-rollback-proof-schedule"))
        self.assertTrue(oracle.may_hold(node, "called", triggers, LIFECYCLE))


if __name__ == "__main__":
    unittest.main()
