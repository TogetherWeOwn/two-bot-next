"""Exercise disabled measurement admission without creating or dispatching a workflow."""

from copy import deepcopy
import importlib.util
from pathlib import Path
import unittest

from disabled_voice_measurement import GUARDS, QUEUE, WORKFLOW
import test_workflows as policy
import test_staging_voice_mutual_exclusion_acceptance as lifecycle

RUNNER_PATH = Path(__file__).resolve().parents[1] / "test-runner-routing.py"
SPEC = importlib.util.spec_from_file_location("runner_routing", RUNNER_PATH)
runner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runner)
TARGET = "./.github/workflows/voice-synthetic.yml"


def scaffold():
    jobs = {name: {"if": guard} for name, guard in GUARDS.items()}
    for name in ("staging-window", "measurement-before", "measurement-after", "measurement-final", "staging-use-release"):
        jobs[name].update({"environment": "staging", "runs-on": "ubuntu-latest"})
    jobs["rerun-refusal"]["runs-on"] = "ubuntu-latest"
    for name in ("voice-first", "voice-second"):
        jobs[name]["uses"] = TARGET
    return {"on": {"workflow_dispatch": ""}, "permissions": {},
            "concurrency": QUEUE.copy(), "jobs": jobs}


class DisabledMeasurementPolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workflows = policy.load_workflows()

    def test_complete_scaffold_is_admitted_without_changing_existing_inventory(self):
        wfs = deepcopy(self.workflows)
        wfs[WORKFLOW] = scaffold()
        self.assertEqual(policy.workflow_policy_errors(wfs), [])
        self.assertEqual(lifecycle.lifecycle_errors(wfs), [])

    def test_mutations_do_not_bypass_inventory_or_lifecycle(self):
        for mutation in ("activation", "queue", "job-lock", "rerun", "trigger", "permissions", "target"):
            with self.subTest(mutation=mutation):
                wfs = deepcopy(self.workflows)
                wfs[WORKFLOW] = candidate = scaffold()
                if mutation == "activation":
                    candidate["jobs"]["staging-window"]["if"] = "${{ true }}"
                elif mutation == "queue":
                    candidate["concurrency"]["group"] = "staging-lifecycle"
                elif mutation == "job-lock":
                    candidate["jobs"]["staging-window"]["concurrency"] = "staging-lifecycle"
                elif mutation == "rerun":
                    candidate["jobs"]["staging-window"]["if"] = "${{ false && github.ref == 'refs/heads/main' }}"
                elif mutation == "trigger":
                    candidate["on"]["push"] = ""
                elif mutation == "permissions":
                    candidate["permissions"]["contents"] = "write"
                else:
                    candidate["jobs"]["voice-first"]["uses"] = "./.github/workflows/other.yml"
                self.assertTrue(policy.workflow_policy_errors(wfs))
                self.assertTrue(lifecycle.lifecycle_errors(wfs))

    def test_each_guard_and_existing_caller_closure_remain_enforced(self):
        for job in GUARDS:
            with self.subTest(job=job):
                wfs = deepcopy(self.workflows)
                wfs[WORKFLOW] = scaffold()
                wfs[WORKFLOW]["jobs"][job]["if"] = "${{ true }}"
                self.assertTrue(policy.workflow_policy_errors(wfs))
                self.assertTrue(lifecycle.lifecycle_errors(wfs))
        wfs = deepcopy(self.workflows)
        wfs[WORKFLOW] = scaffold()
        wfs["check.yml"]["jobs"]["unexpected-caller"] = {"uses": TARGET}
        self.assertTrue(policy.workflow_policy_errors(wfs))
        self.assertTrue(lifecycle.lifecycle_errors(wfs))

    def test_repository_runner_policy_accepts_only_the_disabled_pinned_call(self):
        check = policy.WorkflowTests("test_public_hosted_runner_and_service_isolation")
        check.workflows = {WORKFLOW: scaffold()}
        check.test_public_hosted_runner_and_service_isolation()
        for mutation in ("activation", "target", "runner"):
            with self.subTest(mutation=mutation):
                candidate = scaffold()
                if mutation == "activation":
                    candidate["jobs"]["staging-window"]["if"] = "${{ true }}"
                elif mutation == "target":
                    candidate["jobs"]["voice-first"]["uses"] = "./.github/workflows/other.yml"
                else:
                    candidate["jobs"]["voice-first"]["runs-on"] = "ubuntu-latest"
                check.workflows = {WORKFLOW: candidate}
                with self.assertRaises(AssertionError):
                    check.test_public_hosted_runner_and_service_isolation()

    def test_second_call_mutations_fail_closed(self):
        other = "./.github/workflows/other.yml"
        check = policy.WorkflowTests("test_public_hosted_runner_and_service_isolation")
        for field, value in (("uses", other), ("runs-on", "ubuntu-latest"), ("if", "${{ true }}")):
            wfs = deepcopy(self.workflows)
            wfs[WORKFLOW] = candidate = scaffold()
            candidate["jobs"]["voice-second"][field] = value
            if field != "runs-on":
                self.assertTrue(policy.workflow_policy_errors(wfs), field)
                self.assertTrue(lifecycle.lifecycle_errors(wfs), field)
            check.workflows = {WORKFLOW: candidate}
            with self.assertRaises(AssertionError, msg=field):
                check.test_public_hosted_runner_and_service_isolation()
        text = f"jobs:\n  voice-second:\n    uses: {TARGET}\n"
        self.assertEqual(runner.violations(text, WORKFLOW), [])
        self.assertTrue(runner.violations(text.replace(TARGET, other), WORKFLOW))
        self.assertTrue(runner.violations(text + "    runs-on: ubuntu-latest\n", WORKFLOW))

    def test_runner_pin_rejects_changed_target_and_runner(self):
        text = ("jobs:\n  voice-first:\n"
                f"    uses: {TARGET}\n")
        self.assertEqual(runner.violations(text, WORKFLOW), [])
        annotated = text.replace(TARGET, TARGET + " # zizmor: ignore[self-repository]")
        self.assertEqual(runner.violations(annotated, WORKFLOW), [])
        self.assertTrue(runner.violations(annotated.replace(TARGET, "./.github/workflows/other.yml"), WORKFLOW))
        self.assertTrue(runner.violations(text.replace(TARGET, "./.github/workflows/other.yml"), WORKFLOW))
        self.assertTrue(runner.violations(text + "    runs-on: ubuntu-latest\n", WORKFLOW))


if __name__ == "__main__":
    unittest.main()
