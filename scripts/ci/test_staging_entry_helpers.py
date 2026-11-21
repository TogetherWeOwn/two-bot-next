"""Offline entry, routing and cleanup checks; not Actions scheduler evidence."""

import ast
import re
import subprocess
import unittest
from copy import deepcopy

from test_staging_admission_helpers import (
    assert_policy_mutations,
    masked_refusal_mutations,
)


def needs(job):
    value = job.get("needs", [])
    return {value} if isinstance(value, str) else set(value)


def descendants(jobs, selected):
    """Model the documented selected-job rerun closure, not the Actions scheduler."""
    found = {selected}
    while True:
        expanded = found | {name for name, job in jobs.items() if needs(job) & found}
        if expanded == found:
            return found
        found = expanded


def assert_parent_entries(test, jobs, entries, checker, workflow, diagnostic):
    """Require each protected entry to bind the current admission and reject mutations."""
    for entry in entries:
        guard = jobs[entry]["if"]
        for part in ("github.run_attempt == 1", "needs.soak-hold.outputs.run_id == github.run_id",
                     "needs.soak-hold.outputs.attempt == '1'", "needs.soak-hold.outputs.sha =="):
            test.assertIn(part, guard)
            changed = deepcopy(workflow)
            changed["jobs"][entry]["if"] = guard.replace(part, "true")
            errors = checker(changed)
            test.assertTrue(any(diagnostic(entry, part, error) for error in errors),
                            (entry, part, errors))


def assert_callee_entries(test, workflow, consumer):
    jobs = workflow["jobs"]
    prefix = "github.ref == 'refs/heads/main' && (!inputs.deploy_staging_call || "
    grant = " && ".join(f"inputs.{name} != ''" for name in
                        ("staging_use_expires_at", "staging_use_sha", "staging_use_deployment"))
    test.assertEqual(jobs["staging-window"]["if"], prefix + f"(github.run_attempt == 1 && {grant}))",
                     "staging-window guard")
    test.assertEqual(jobs[consumer]["if"], prefix + "github.run_attempt == 1)", f"{consumer} guard")
    test.assertEqual(workflow["on"]["workflow_call"]["inputs"]
                     ["deploy_staging_call"]["default"], "false", "callee default")


def assert_standalone_routing(test, workflow, selector):
    """Check the standalone and inherited branches, without evaluating Actions expressions."""
    window = workflow["jobs"]["staging-window"]
    steps = window["steps"]
    begin = next(step for step in steps if step.get("id") == "begin")
    ready = next(step for step in steps if step.get("name") == "Assert the caller's exact window and readiness")
    release = workflow["jobs"]["staging-use-release"]
    test.assertIn(f"(inputs.deploy_staging_call || {selector} != '')", workflow["concurrency"]["group"], "concurrency selector")
    test.assertEqual(begin["if"], f"{selector} == '' && !inputs.deploy_staging_call", "standalone begin guard")
    test.assertEqual(begin["run"], "node wrangler/scripts/staging-use-pipeline.mjs begin")
    test.assertEqual(ready["if"], f"(inputs.deploy_staging_call || {selector} != '')", "inherited ready guard")
    test.assertEqual(ready["run"], "node wrangler/scripts/staging-use.mjs ready")
    test.assertIn(f"{selector} == ''", release["if"])
    finish = next(step for step in release["steps"]
                  if step.get("name") == "Release only this standalone run's window")
    test.assertEqual(finish["if"], f"{selector} == '' && !inputs.deploy_staging_call", "standalone finish guard")
    test.assertEqual(finish["run"], "node wrangler/scripts/staging-use-pipeline.mjs finish")


def assert_cleanup(test, jobs, inherited):
    release = jobs["staging-use-release"]
    test.assertIn("always()", release["if"], "cleanup always")
    test.assertNotIn("github.run_attempt", release["if"], "cleanup attempt")
    if inherited:
        test.assertIn("!inputs.deploy_staging_call", release["if"], "cleanup ownership")
    test.assertEqual([step["run"] for step in release["steps"] if step.get("run")],
                     ["node wrangler/scripts/staging-use-pipeline.mjs finish"], "cleanup command")
    test.assertIn("rerun-refusal", descendants(jobs, "staging-use-release"), "cleanup refusal path")


class EntryAndCleanupHelperTests(unittest.TestCase):
    def test_parent_guard_fixture_and_checker_mutations(self):
        parts = ("github.run_attempt == 1", "needs.soak-hold.outputs.run_id == github.run_id",
                 "needs.soak-hold.outputs.attempt == '1'", "needs.soak-hold.outputs.sha == source")
        workflow = {"jobs": {"one": {"if": " && ".join(parts)},
                             "two": {"if": " && ".join(parts)}}}

        def checker(value):
            return [name for name, job in value["jobs"].items()
                    if not all(part in job["if"] for part in parts)]

        diagnostic = lambda entry, part, error: error == entry
        assert_parent_entries(self, workflow["jobs"], {"one", "two"}, checker, workflow, diagnostic)
        changed = deepcopy(workflow)
        changed["jobs"]["two"]["if"] = "true"
        with self.assertRaises(AssertionError):
            assert_parent_entries(self, changed["jobs"], {"one", "two"}, checker, changed, diagnostic)
        with self.assertRaises(AssertionError):
            assert_parent_entries(self, workflow["jobs"], {"one", "two"},
                                  lambda _: ["unrelated diagnostic"], workflow, diagnostic)

    def test_callee_and_cleanup_fixtures_reject_corruption(self):
        guard = "github.ref == 'refs/heads/main' && (!inputs.deploy_staging_call || github.run_attempt == 1)"
        grant = " && ".join(f"inputs.{name} != ''" for name in
                            ("staging_use_expires_at", "staging_use_sha", "staging_use_deployment"))
        window_guard = "github.ref == 'refs/heads/main' && (!inputs.deploy_staging_call || " + f"(github.run_attempt == 1 && {grant}))"
        workflow = {"on": {"workflow_call": {"inputs": {"deploy_staging_call": {"default": "false"}}}},
                    "jobs": {"staging-window": {"if": window_guard}, "synthetic": {"if": guard},
                             "staging-use-release": {"if": "always() && !inputs.deploy_staging_call",
                                                     "steps": [{"run": "node wrangler/scripts/staging-use-pipeline.mjs finish"}]},
                             "rerun-refusal": {"needs": "staging-use-release"}}}
        assert_callee_entries(self, workflow, "synthetic")
        assert_cleanup(self, workflow["jobs"], True)
        for target, field, value, diagnostic in (("synthetic", "if", "true", "synthetic guard"),
                ("staging-window", "if", "true", "staging-window guard"),
                ("staging-use-release", "if", "always()", "cleanup ownership"),
                ("staging-use-release", "if", "always() && github.run_attempt == 1", "cleanup attempt"),
                ("staging-use-release", "steps", [{"run": "echo start"}], "cleanup command"),
                ("rerun-refusal", "needs", [], "cleanup refusal path")):
            with self.subTest(target=target, field=field):
                changed = deepcopy(workflow)
                changed["jobs"][target][field] = value
                with self.assertRaisesRegex(AssertionError, diagnostic):
                    if target in ("staging-window", "synthetic"):
                        assert_callee_entries(self, changed, "synthetic")
                    else:
                        assert_cleanup(self, changed["jobs"], True)
        changed = deepcopy(workflow)
        changed["on"]["workflow_call"]["inputs"]["deploy_staging_call"]["default"] = "true"
        with self.assertRaisesRegex(AssertionError, "callee default"):
            assert_callee_entries(self, changed, "synthetic")


def condition(test, expression, context):
    """Evaluate the limited guards used here, not the Actions scheduler."""
    expression = re.sub(r"\b(?:inputs|github|needs)\.[\w.-]+",
                        lambda match: repr(context.get(match[0], "")), expression)
    expression = expression.replace("always()", "True").replace("&&", " and ").replace("||", " or ")
    expression = re.sub(r"!(?!=)", " not ", expression).strip()
    tree = ast.parse(expression, mode="eval")
    allowed = (ast.Expression, ast.BoolOp, ast.And, ast.Or, ast.UnaryOp, ast.Not,
               ast.Compare, ast.Eq, ast.NotEq, ast.Gt, ast.Constant)
    test.assertTrue(all(isinstance(node, allowed) for node in ast.walk(tree)), expression)
    return bool(eval(compile(tree, "<workflow guard>", "eval"), {"__builtins__": {}}))


def assert_routing_behavior(test, workflow, called, grant, attempt, expected, *, begin_output="owned"):
    jobs = workflow["jobs"]
    context = {"inputs.deploy_staging_call": called, "github.run_attempt": attempt,
               "github.ref": "refs/heads/main", "needs.staging-window.outputs.expires_at": begin_output}
    context.update({f"inputs.{name}": value for name, value in grant.items()})
    window = jobs["staging-window"]
    begin = next(step for step in window["steps"] if step.get("id") == "begin")
    ready = next(step for step in window["steps"] if step.get("name") == "Assert the caller's exact window and readiness")
    release = jobs["staging-use-release"]
    finish = next(step for step in release["steps"] if step.get("name") == "Release only this standalone run's window")
    admitted = condition(test, window["if"], context)
    actual = (admitted, admitted and condition(test, begin["if"], context),
              admitted and condition(test, ready["if"], context),
              condition(test, release["if"], context) and condition(test, finish["if"], context),
              condition(test, jobs["rerun-refusal"]["if"], context))
    test.assertEqual(actual, expected)
    refusal = jobs["rerun-refusal"]
    test.assertNotIn("environment", refusal)
    test.assertEqual(refusal.get("permissions"), {})
    test.assertEqual(refusal["steps"], [{"run": "exit 1"}])


class CalleePolicyRegressionTests(unittest.TestCase):
    def test_actual_entries_and_terminal_refusal(self):
        from test_workflows import (
            help_embed_synthetic_errors,
            load_workflows,
            voice_synthetic_errors,
        )

        workflows = load_workflows()
        for name, consumer, checker in (
            ("voice-synthetic.yml", "synthetic", voice_synthetic_errors),
            ("help-embed-synthetic.yml", "help-embed-smoke", help_embed_synthetic_errors),
        ):
            workflow = workflows[name]
            with self.subTest(workflow=name):
                self.assertEqual(checker(workflow), [])
                assert_callee_entries(self, workflow, consumer)
                steps = workflow["jobs"]["rerun-refusal"]["steps"]
                self.assertEqual(steps, [{"run": "exit 1"}])
                result = subprocess.run(["bash", "-e", "-c", steps[0]["run"]], env={},
                                        capture_output=True, timeout=10, check=False)
                self.assertEqual(result.returncode, 1)

                mutations = masked_refusal_mutations()
                mutations["reordered needs"] = lambda w: w["jobs"]["rerun-refusal"]["needs"].reverse()
                assert_policy_mutations(self, workflow, checker,
                                        lambda _, error: "rerun-refusal: must be the secret-free" in error,
                                        mutations)

            for inventory in ("missing refusal", "missing window", "extra job"):
                changed = deepcopy(workflow)
                jobs = changed["jobs"]
                if inventory == "missing refusal":
                    del jobs["rerun-refusal"]
                elif inventory == "missing window":
                    del jobs["staging-window"]
                else:
                    jobs["unexpected"] = {}
                jobs[consumer]["environment"] = "wrong"
                if "staging-window" in jobs:
                    jobs["staging-window"]["environment"] = "wrong"
                with self.subTest(workflow=name, inventory=inventory):
                    errors = checker(changed)
                    self.assertTrue(any("jobs must be exactly" in error for error in errors), errors)
                    self.assertIn(f"{name}:{consumer}: must read only the voice-synthetic Environment", errors)
                    if "staging-window" in jobs:
                        self.assertIn(f"{name}:staging-window: must read the staging Environment", errors)


class VoiceArtifactNamingTests(unittest.TestCase):
    def test_invocations_are_distinct_and_confined_to_upload_names(self):
        from test_workflows import load_workflows

        workflow = load_workflows()["voice-synthetic.yml"]
        self.assertEqual(workflow["on"]["workflow_call"]["inputs"]["artifact_invocation"],
                         {"type": "string", "required": "false", "default": ""})
        jobs = workflow["jobs"]
        start, = (s["with"] for s in jobs["staging-window"]["steps"] if s.get("uses", "").startswith("actions/upload-artifact@"))
        voice, = (s["with"] for s in jobs["synthetic"]["steps"] if s.get("uses", "").startswith("actions/upload-artifact@"))
        suffix = "${{ inputs.artifact_invocation != '' && format('-{0}', inputs.artifact_invocation) || '' }}"
        bases = ("staging-use-voice-start-${{ github.run_id }}-${{ github.run_attempt }}",
                 "voice-synthetic-${{ github.run_id }}")
        self.assertEqual((start["name"], voice["name"]), tuple(base + suffix for base in bases))
        self.assertEqual(start["path"], "${{ runner.temp }}/staging-use.jsonl")
        self.assertEqual(voice["path"], "voice-synthetic-${{ github.run_id }}-*.json")
        self.assertEqual(str(jobs).count("inputs.artifact_invocation"), 4)
        self.assertNotIn("artifact_invocation", str(jobs["staging-use-release"]))

        # Model the pinned expression for one run/attempt; this is not an Actions upload.
        def names(label):
            endings = f"-{label}" if label else ""
            return (f"staging-use-voice-start-42-1{endings}", f"voice-synthetic-42{endings}")

        self.assertEqual(names(None), names(""))
        self.assertEqual(names(""), ("staging-use-voice-start-42-1", "voice-synthetic-42"))
        first, second = names("voice-1"), names("voice-2")
        self.assertEqual(len(set(first + second)), 4)
        self.assertEqual(len(set(names("") + names(""))), 2)  # immutable-name collision


class StandaloneRoutingTests(unittest.TestCase):
    def test_deploy_callers_route_real_grants_and_refuse_empty_grants(self):
        from test_workflows import load_workflows

        workflows = load_workflows()
        jobs = workflows["deploy-staging.yml"]["jobs"]
        fields = ("staging_use_expires_at", "staging_use_sha", "staging_use_deployment")
        for job, filename in (("voice-synthetic", "voice-synthetic.yml"),
                              ("help-embed-synthetic", "help-embed-synthetic.yml")):
            with self.subTest(caller=job):
                caller = jobs[job]
                self.assertEqual(caller["uses"], f"./.github/workflows/{filename}")
                self.assertEqual(caller["with"]["deploy_staging_call"], "true")
                for field in fields:
                    self.assertEqual(caller["with"][field], f"${{{{ needs.deploy.outputs.{field} }}}}")
                    self.assertIn(field, jobs["deploy"]["outputs"])
                callee = workflows[filename]
                assert_standalone_routing(self, callee, "inputs.staging_use_expires_at")
                for value, expected in (("valid", (True, False, True, False, False)),
                                        ("", (False, False, False, False, True))):
                    assert_routing_behavior(self, callee, True, dict.fromkeys(fields, value), 1, expected)

    def test_actual_callee_routing_and_refusal(self):
        from test_workflows import load_workflows

        fields = ("staging_use_expires_at", "staging_use_sha", "staging_use_deployment")
        valid = dict.fromkeys(fields, "valid")
        empty = dict.fromkeys(fields, "")
        cases = [(False, empty, 1, (True, True, False, True, False)),
                 (False, valid, 1, (True, False, True, False, False)),
                 (True, valid, 1, (True, False, True, False, False)),
                 (True, valid, 2, (False, False, False, False, True))]
        cases += [(True, {**valid, field: ""}, 1, (False, False, False, False, True))
                  for field in fields]
        workflows = load_workflows()
        for name in ("voice-synthetic.yml", "help-embed-synthetic.yml"):
            workflow = workflows[name]
            for called, grant, attempt, expected in cases:
                with self.subTest(workflow=name, called=called, grant=grant, attempt=attempt):
                    assert_routing_behavior(self, workflow, called, grant, attempt, expected)
            for attempt in (1, 2):
                with self.subTest(workflow=name, attempt=attempt, begin_output=""):
                    assert_routing_behavior(self, workflow, False, empty, attempt,
                                            (True, True, False, False, False), begin_output="")
            assert_cleanup(self, workflow["jobs"], True)
            for mutation in ("environment", "permissions", "steps"):
                changed = deepcopy(workflow)
                refusal = changed["jobs"]["rerun-refusal"]
                refusal[mutation] = {"environment": "staging", "permissions": {"contents": "read"},
                                     "steps": [{"run": "exit 0"}]}[mutation]
                with self.subTest(workflow=name, refusal_mutation=mutation), self.assertRaises(AssertionError):
                    assert_routing_behavior(self, changed, True, empty, 1,
                                            (False, False, False, False, True))
            changed = deepcopy(workflow)
            release = changed["jobs"]["staging-use-release"]
            release["if"] = release["if"].replace(" && !inputs.deploy_staging_call", "")
            with self.subTest(workflow=name, cleanup="job ownership"), self.assertRaisesRegex(AssertionError, "cleanup ownership"):
                assert_cleanup(self, changed["jobs"], True)
            release["if"] = workflow["jobs"]["staging-use-release"]["if"].replace(
                " && needs.staging-window.outputs.expires_at != ''", "")
            with self.subTest(workflow=name, cleanup="begin output"), self.assertRaises(AssertionError):
                assert_routing_behavior(self, changed, False, empty, 2,
                                        (True, True, False, False, False), begin_output="")
            for field in fields:
                changed = deepcopy(workflow)
                guard = changed["jobs"]["staging-window"]["if"]
                changed["jobs"]["staging-window"]["if"] = guard.replace(f"inputs.{field} != ''", "1 == 1")
                with self.subTest(workflow=name, removed=field), self.assertRaises(AssertionError):
                    assert_routing_behavior(self, changed, True, {**valid, field: ""}, 1,
                                            (False, False, False, False, True))
                changed = deepcopy(workflow)
                guard = changed["jobs"]["rerun-refusal"]["if"]
                changed["jobs"]["rerun-refusal"]["if"] = guard.replace(f"inputs.{field} == ''", "1 == 0")
                with self.subTest(workflow=name, refusal_removed=field), self.assertRaises(AssertionError):
                    assert_routing_behavior(self, changed, True, {**valid, field: ""}, 1,
                                            (False, False, False, False, True))

    def test_real_legacy_routing_and_mutations(self):
        from test_workflows import load_workflows

        selector = "inputs.staging_use_expires_at"
        workflows = load_workflows()
        for name in ("voice-synthetic.yml", "help-embed-synthetic.yml"):
            workflow = workflows[name]
            assert_standalone_routing(self, workflow, selector)
            changed = deepcopy(workflow)
            changed["concurrency"]["group"] = changed["concurrency"]["group"].replace(
                "inputs.deploy_staging_call || ", "")
            with self.subTest(workflow=name, mutation="concurrency caller"), self.assertRaisesRegex(AssertionError, "concurrency selector"):
                assert_standalone_routing(self, changed, selector)
            for job, command, diagnostic in (
                    ("staging-window", "node wrangler/scripts/staging-use-pipeline.mjs begin", "standalone begin guard"),
                    ("staging-window", "node wrangler/scripts/staging-use.mjs ready", "inherited ready guard"),
                    ("staging-use-release", "node wrangler/scripts/staging-use-pipeline.mjs finish", "standalone finish guard")):
                changed = deepcopy(workflow)
                step = next(step for step in changed["jobs"][job]["steps"] if step.get("run") == command)
                step["if"] = "true"
                with self.subTest(workflow=name, command=command), self.assertRaisesRegex(AssertionError, diagnostic):
                    assert_standalone_routing(self, changed, selector)

            changed = deepcopy(workflow)
            changed["jobs"]["staging-use-release"]["steps"].insert(0, {"run": "echo setup"})
            with self.subTest(workflow=name, mutation="extra setup"):
                assert_standalone_routing(self, changed, selector)

            release = next(step for step in changed["jobs"]["staging-use-release"]["steps"]
                           if step.get("name") == "Release only this standalone run's window")
            release["run"] = "echo wrong release"
            with self.subTest(workflow=name, mutation="wrong release"), self.assertRaises(AssertionError):
                assert_standalone_routing(self, changed, selector)
