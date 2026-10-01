"""Offline workflow policy fixtures; never dispatch, deploy or probe staging."""

from copy import deepcopy
from pathlib import Path
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
STATIC_FALSE = "${{ false }}"
JOB_INVENTORY = {
    "check.yml": {"check", "self-role-store", "container", "community-db", "feeds-db", "worker"},
    "deploy-staging.yml": {"deploy"},
    "nightly.yml": {"advisories", "sweep"},
    "pr-lint.yml": {"pr-lint"},
    "release.yml": {"release-please", "dispatch-checks"},
    "secret-scan.yml": {"gitleaks"},
}


class UniqueKeyLoader(yaml.BaseLoader):
    def construct_mapping(self, node, deep=False):
        mapping = {}
        for key_node, value_node in node.value:
            key = self.construct_object(key_node, deep=deep)
            if key in mapping:
                raise ValueError(f"duplicate workflow key: {key}")
            mapping[key] = self.construct_object(value_node, deep=deep)
        return mapping


def load_workflows():
    return {path.name: yaml.load(path.read_text(), Loader=UniqueKeyLoader)
            for path in (ROOT / ".github/workflows").glob("*.y*ml")}


def suspension_errors(workflows):
    errors = []
    # Fail closed on new workflows/jobs, including reusable-workflow alternatives.
    # An intentional addition needs an explicit policy change and independent review.
    if set(workflows) != set(JOB_INVENTORY):
        errors.append("workflow inventory changed")
    for name, workflow in workflows.items():
        jobs = workflow.get("jobs", {})
        if set(jobs) != JOB_INVENTORY.get(name, set()):
            errors.append(f"{name}: job inventory changed")
        for job_id, job in jobs.items():
            if name == "deploy-staging.yml":
                if job.get("if") != STATIC_FALSE:
                    errors.append(f"{name}:{job_id}: deployment/probes are not statically disabled")
            else:
                # Retargeting a known CI job must not create an activation bypass.
                text = str(job).lower()
                if ("environment" in job or "uses" in job
                        or any(marker in text for marker in (
                            "cloudflare/wrangler-action", "wrangler deploy", "wrangler publish",
                            "cloudflare_api_token", "staging_worker_url", "/health", "/readyz",
                        ))):
                    errors.append(f"{name}:{job_id}: deployment/probe alternative outside suspended workflow")
    return errors


class WorkflowTests(unittest.TestCase):
    def setUp(self):
        self.workflows = load_workflows()

    def test_staging_is_suspended_for_push_and_dispatch(self):
        staging = self.workflows["deploy-staging.yml"]
        self.assertEqual(staging["on"], {"push": {"branches": ["main"]}, "workflow_dispatch": ""})
        self.assertEqual(suspension_errors(self.workflows), [])
        self.assertEqual(staging["jobs"]["deploy"]["environment"], "staging")

    def test_removing_or_mutating_guard_fails(self):
        for guard in (None, "true", "${{ true }}", "${{ vars.ENABLE_STAGING }}",
                      "${{ github.ref == 'refs/heads/main' }}", "${{ inputs.deploy }}"):
            with self.subTest(guard=guard):
                workflows = deepcopy(self.workflows)
                job = workflows["deploy-staging.yml"]["jobs"]["deploy"]
                if guard is None:
                    job.pop("if", None)
                else:
                    job["if"] = guard
                self.assertTrue(suspension_errors(workflows))

    def test_step_level_guard_is_not_sufficient(self):
        workflows = deepcopy(self.workflows)
        job = workflows["deploy-staging.yml"]["jobs"]["deploy"]
        job.pop("if", None)
        for step in job["steps"]:
            step["if"] = STATIC_FALSE
        self.assertTrue(suspension_errors(workflows))

    def test_new_job_or_workflow_is_not_an_activation_route(self):
        for name in self.workflows:
            with self.subTest(name=name):
                workflows = deepcopy(self.workflows)
                workflows[name]["jobs"]["alternate"] = {"uses": "./.github/workflows/deploy-staging.yml"}
                self.assertTrue(suspension_errors(workflows))
        workflows = deepcopy(self.workflows)
        workflows["alternate.yml"] = {"jobs": {"deploy": {"run": "wrangler deploy"}}}
        self.assertTrue(suspension_errors(workflows))

    def test_retargeting_known_job_fails(self):
        for change in ({"environment": "staging"}, {"uses": "./.github/workflows/deploy-staging.yml"},
                       {"steps": [{"run": 'curl "$STAGING_WORKER_URL/readyz"'}]},
                       {"steps": [{"uses": "cloudflare/wrangler-action@pinned"}]}):
            with self.subTest(change=change):
                workflows = deepcopy(self.workflows)
                workflows["check.yml"]["jobs"]["worker"].update(change)
                self.assertTrue(suspension_errors(workflows))

    def test_duplicate_keys_cannot_hide_an_enabled_job(self):
        with self.assertRaises(ValueError):
            yaml.load("jobs:\n  deploy:\n    if: '${{ false }}'\n    if: true\n", Loader=UniqueKeyLoader)

    def test_private_runner_and_service_isolation(self):
        for name, workflow in self.workflows.items():
            for job_id, job in workflow["jobs"].items():
                with self.subTest(workflow=name, job=job_id):
                    self.assertEqual(job["runs-on"], ["self-hosted", "two-selfhosted"])
                    if job.get("services"):
                        self.assertIn("container", job)
                        for service in job["services"].values():
                            self.assertNotIn("ports", service)

    def test_minimal_grants_and_no_persisted_checkout_credentials(self):
        for name, workflow in self.workflows.items():
            self.assertEqual(workflow["permissions"], {})
            for job_id, job in workflow["jobs"].items():
                with self.subTest(workflow=name, job=job_id):
                    expected = {"contents": "read"}
                    if (name, job_id) == ("pr-lint.yml", "pr-lint"):
                        expected["pull-requests"] = "read"
                    elif (name, job_id) == ("release.yml", "release-please"):
                        expected = {"contents": "write", "pull-requests": "write"}
                    elif (name, job_id) == ("release.yml", "dispatch-checks"):
                        expected = {"actions": "write"}
                    self.assertEqual(job["permissions"], expected)
                    for step in job.get("steps", []):
                        if step.get("uses", "").startswith("actions/checkout@"):
                            self.assertEqual(step["with"]["persist-credentials"], "false")


if __name__ == "__main__":
    unittest.main()
