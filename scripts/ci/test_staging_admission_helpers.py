"""Reusable offline checks for staging admission; not scheduler evidence."""

import os
import subprocess
import tempfile
import unittest
from copy import deepcopy
from pathlib import Path


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


def assert_refusal(test, jobs, condition):
    """Check terminal refusal shape, closure and its executable failure."""
    sentinel = jobs["rerun-refusal"]
    test.assertEqual(needs(sentinel), set(jobs) - {"rerun-refusal"}, "refusal dependencies")
    test.assertEqual(sentinel["permissions"], {}, "refusal permissions")
    test.assertNotIn("environment", sentinel, "refusal environment")
    test.assertNotIn("env", sentinel, "refusal env")
    test.assertEqual(sentinel["if"], condition, "refusal condition")
    test.assertNotIn("continue-on-error", sentinel, "refusal continue-on-error")
    for selected in jobs:
        test.assertIn("rerun-refusal", descendants(jobs, selected), selected)
    steps = sentinel["steps"]
    test.assertEqual(len(steps), 1)
    test.assertEqual(set(steps[0]), {"run"})
    result = subprocess.run(["bash", "-e", "-c", steps[0]["run"]],
                            capture_output=True, text=True, timeout=10, check=False)
    test.assertNotEqual(result.returncode, 0, "refusal must fail")


def assert_admission(test, script):
    """Exercise the admission shell and exact outputs with synthetic inputs."""
    sha = "a" * 40
    for source, admitted, attempt, accepted in (
        (sha, sha, "1", True), (sha, sha, "2", True),
        (sha, "b" * 40, "1", False), ("", "", "1", False),
        ("A" * 40, "A" * 40, "1", False), ("a" * 39, "a" * 39, "1", False),
    ):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "output"
            env = {**os.environ, "SOURCE_SHA": source, "ADMITTED_SHA": admitted,
                   "GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": attempt,
                   "GITHUB_OUTPUT": str(output)}
            result = subprocess.run(["bash", "-e", "-c", script], env=env,
                                    capture_output=True, text=True, timeout=10, check=False)
            test.assertEqual(result.returncode == 0, accepted, result.stderr)
            contents = output.read_text() if output.exists() else ""
            test.assertEqual(contents, f"run_id=123\nattempt={attempt}\nsha={source}\n"
                             if accepted else "")


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
    finish = next(step for step in release["steps"] if step.get("run"))
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
            for job, index, diagnostic in (("staging-window", 2, "standalone begin guard"),
                                           ("staging-window", 3, "inherited ready guard"),
                                           ("staging-use-release", 2, "standalone finish guard")):
                changed = deepcopy(workflow)
                changed["jobs"][job]["steps"][index]["if"] = "true"
                with self.subTest(workflow=name, job=job, index=index), self.assertRaisesRegex(AssertionError, diagnostic):
                    assert_standalone_routing(self, changed, selector)


class AdmissionHelperTests(unittest.TestCase):
    def test_admission_fixture_and_corruption(self):
        script = '''
[[ "$SOURCE_SHA" =~ ^[0-9a-f]{40}$ ]]
[[ "$SOURCE_SHA" == "$ADMITTED_SHA" ]]
printf 'run_id=%s\\nattempt=%s\\nsha=%s\\n' "$GITHUB_RUN_ID" "$GITHUB_RUN_ATTEMPT" "$SOURCE_SHA" >> "$GITHUB_OUTPUT"
'''
        assert_admission(self, script)
        with self.assertRaises(AssertionError):
            assert_admission(self, script.replace('[[ "$SOURCE_SHA" == "$ADMITTED_SHA" ]]', 'true'))


class RefusalHelperTests(unittest.TestCase):
    def test_valid_fixture_and_corruptions(self):
        condition = "always() && github.run_attempt != 1"
        jobs = {
            "admit": {},
            "deploy": {"needs": "admit"},
            "rerun-refusal": {"needs": ["admit", "deploy"], "permissions": {},
                              "if": condition, "steps": [{"run": "exit 1"}]},
        }
        assert_refusal(self, jobs, condition)
        for field, value, diagnostic in (("needs", ["admit"], "refusal dependencies"),
                                         ("permissions", {"contents": "read"}, "refusal permissions"),
                                         ("environment", "staging", "refusal environment"),
                                         ("env", {}, "refusal env"),
                                         ("if", "always()", "refusal condition"),
                                         ("continue-on-error", True, "refusal continue-on-error"),
                                         ("steps", [{"run": "exit 0"}], "refusal must fail")):
            with self.subTest(field=field):
                changed = deepcopy(jobs)
                changed["rerun-refusal"][field] = value
                with self.assertRaisesRegex(AssertionError, diagnostic):
                    assert_refusal(self, changed, condition)


if __name__ == "__main__":
    unittest.main()
