"""Offline entry, routing and cleanup checks; not Actions scheduler evidence."""

import unittest
from copy import deepcopy


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
    for entry in ("staging-window", consumer):
        test.assertEqual(jobs[entry]["if"], "github.ref == 'refs/heads/main' && "
                         "((!inputs.deploy_staging_call) || github.run_attempt == 1)", f"{entry} guard")
    test.assertEqual(workflow["on"]["workflow_call"]["inputs"]
                     ["deploy_staging_call"]["default"], "false", "callee default")


def assert_standalone_routing(test, workflow, selector):
    """Check the standalone and inherited branches, without evaluating Actions expressions."""
    window = workflow["jobs"]["staging-window"]
    steps = window["steps"]
    begin = next(step for step in steps if step.get("id") == "begin")
    ready = next(step for step in steps if step.get("name") == "Assert the caller's exact window and readiness")
    release = workflow["jobs"]["staging-use-release"]
    test.assertIn(selector, workflow["concurrency"]["group"], "concurrency selector")
    test.assertEqual(begin["if"], f"{selector} == ''", "standalone begin guard")
    test.assertEqual(begin["run"], "node wrangler/scripts/staging-use-pipeline.mjs begin")
    test.assertEqual(ready["if"], f"{selector} != ''", "inherited ready guard")
    test.assertEqual(ready["run"], "node wrangler/scripts/staging-use.mjs ready")
    test.assertIn(f"{selector} == ''", release["if"])
    finish = next(step for step in release["steps"]
                  if step.get("name") == "Release only this standalone run's window")
    test.assertEqual(finish["if"], f"{selector} == ''", "standalone finish guard")
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
        guard = "github.ref == 'refs/heads/main' && ((!inputs.deploy_staging_call) || github.run_attempt == 1)"
        workflow = {"on": {"workflow_call": {"inputs": {"deploy_staging_call": {"default": "false"}}}},
                    "jobs": {"staging-window": {"if": guard}, "synthetic": {"if": guard},
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


class StandaloneRoutingTests(unittest.TestCase):
    def test_real_legacy_routing_and_mutations(self):
        from test_workflows import load_workflows

        selector = "inputs.staging_use_expires_at"
        for name in ("voice-synthetic.yml", "help-embed-synthetic.yml"):
            workflow = load_workflows()[name]
            assert_standalone_routing(self, workflow, selector)
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
