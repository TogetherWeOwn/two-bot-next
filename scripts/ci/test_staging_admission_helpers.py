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
        (sha, "", "1", False), ("", sha, "1", False),
        ("A" * 40, "A" * 40, "1", False), ("a" * 39, "a" * 39, "1", False),
        ("a" * 41, "a" * 41, "1", False), ("g" * 40, "g" * 40, "1", False),
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


def assert_policy_mutations(test, workflow, checker, diagnostic, mutations):
    """Require a clean fixture and a matching diagnostic per independent mutation."""
    test.assertEqual(checker(workflow), [])
    for label, mutate in mutations.items():
        changed = deepcopy(workflow)
        mutate(changed)
        errors = checker(changed)
        test.assertTrue(any(diagnostic(label, error) for error in errors), (label, errors))


def admission_step_mutations():
    """Mutate the uniquely named admission step, not a positional neighbor."""
    def mutate(workflow, field, value):
        steps = workflow["jobs"]["soak-hold"]["steps"]
        matches = [step for step in steps if step.get("id") == "admit"]
        if len(matches) != 1:
            raise ValueError("expected one admit step")
        if field == "remove":
            steps.remove(matches[0])
        else:
            matches[0][field] = value

    return {label: lambda workflow, field=field, value=value: mutate(workflow, field, value)
            for label, field, value in (("missing", "remove", None), ("run", "run", "true"),
                                        ("environment", "env", {}))}


def masked_refusal_mutations():
    """Replace the failing script or mask its job result."""
    return {"success": lambda w: w["jobs"]["rerun-refusal"]["steps"][0].update(run="exit 0"),
            "echo": lambda w: w["jobs"]["rerun-refusal"]["steps"][0].update(run="echo exit 1"),
            "comment": lambda w: w["jobs"]["rerun-refusal"]["steps"][0].update(run="# exit 1"),
            "continue": lambda w: w["jobs"]["rerun-refusal"].update({"continue-on-error": True})}


class PolicyMutationHelperTests(unittest.TestCase):
    def test_mutations_require_the_expected_diagnostic(self):
        admit = {"id": "admit", "run": "guard", "env": {"SOURCE": "sha"}}
        refusal = {"steps": [{"run": "exit 1"}]}
        workflow = {"jobs": {"soak-hold": {"steps": [{"run": "setup"}, deepcopy(admit)]},
                             "rerun-refusal": deepcopy(refusal)}}

        def checker(value):
            jobs = value["jobs"]
            errors = []
            if jobs["soak-hold"]["steps"] != [{"run": "setup"}, admit]:
                errors.append("admission")
            if jobs["rerun-refusal"] != refusal:
                errors.append("refusal")
            return errors

        for mutations, expected in ((admission_step_mutations(), "admission"),
                                    (masked_refusal_mutations(), "refusal")):
            def diagnostic(_label, error, expected=expected):
                return error == expected

            assert_policy_mutations(self, workflow, checker, diagnostic, mutations)
            with self.assertRaises(AssertionError):
                assert_policy_mutations(self, workflow, lambda _: [], diagnostic, mutations)
            with self.assertRaises(AssertionError):
                assert_policy_mutations(self, workflow, checker, lambda *_: False, mutations)


class AdmissionHelperTests(unittest.TestCase):
    def test_admission_fixture_and_corruption(self):
        script = '''
[[ "$SOURCE_SHA" =~ ^[0-9a-f]{40}$ ]]
[[ "$SOURCE_SHA" == "$ADMITTED_SHA" ]]
printf 'run_id=%s\\nattempt=%s\\nsha=%s\\n' "$GITHUB_RUN_ID" "$GITHUB_RUN_ATTEMPT" "$SOURCE_SHA" >> "$GITHUB_OUTPUT"
'''
        assert_admission(self, script)
        for guard in ('[[ "$SOURCE_SHA" =~ ^[0-9a-f]{40}$ ]]', '[[ "$SOURCE_SHA" == "$ADMITTED_SHA" ]]'):
            with self.subTest(guard=guard), self.assertRaises(AssertionError):
                assert_admission(self, script.replace(guard, 'true'))


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
