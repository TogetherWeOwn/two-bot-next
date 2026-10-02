"""Offline workflow policy fixtures; never dispatch, deploy or probe staging."""

from copy import deepcopy
from pathlib import Path
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
STATIC_FALSE = "${{ false }}"
JOB_INVENTORY = {
    "check.yml": {"check", "parity-docs", "self-role-store", "container-inputs", "container",
                  "community-db", "feeds-db", "tickets-postgres", "worker"},
    "deploy-production.yml": {"guard", "production"},
    "deploy-staging.yml": {"deploy"},
    "nightly.yml": {"pipeline-benchmark", "advisories", "sweep"},
    "pipeline-benchmark.yml": {"benchmark"},
    "release.yml": {"release-please", "dispatch-checks"},
    "supply-chain.yml": {"pr-lint", "gitleaks"},
}
# Reusable-workflow calls are allowed only to these non-deploy workflows.
REUSABLE_CALLS = {("nightly.yml", "pipeline-benchmark"): "./.github/workflows/pipeline-benchmark.yml"}
# Main's runner routing (#265, 2026-10-02): the repo is public and the org's
# self-hosted runner group refuses public repos, so every job routes through
# one expression — public repo -> GitHub-hosted, private -> CI_OVERFLOW_* switch
# for this named job, then the self-hosted pool. Mirrors scripts/test-runner-routing.py.
ROUTED_RUNNER = (
    "${{{{ fromJSON((!github.event.repository.private && '[\"ubuntu-latest\"]') || "
    "(contains(fromJSON(vars.CI_OVERFLOW_JOBS || '[]'), '{job}') && "
    "contains(fromJSON(vars.CI_OVERFLOW_EVENTS || '[]'), github.event_name) && "
    "vars.CI_OVERFLOW_RUNNER) || '[\"self-hosted\",\"two-selfhosted\"]') }}}}"
)
DEPLOY_MARKERS = (
    "cloudflare/wrangler-action", "wrangler deploy", "wrangler publish",
    "cloudflare_api_token", "staging_worker_url", "production_worker_url", "/health", "/readyz",
)


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


def runner_allowed(job_id, runs_on):
    # The routing expression must name this job, so one job's switch cannot move another.
    return runs_on == ROUTED_RUNNER.format(job=job_id)


def production_errors(workflow):
    """deploy-production is the one live route: dispatch-only, human-gated, chained to staging."""
    name = "deploy-production.yml"
    errors = []
    if set(workflow.get("on") or {}) != {"workflow_dispatch"}:
        errors.append(f"{name}: production must be dispatch-only")
    jobs = workflow.get("jobs", {})
    production, guard = jobs.get("production", {}), jobs.get("guard", {})
    if production.get("environment") != "production" or production.get("needs") != "guard":
        errors.append(f"{name}:production: must wait for the guard and the protected Environment")
    if "if" in production:
        errors.append(f"{name}:production: a job condition could bypass the guard outputs")
    if ("environment" in guard or "uses" in guard
            or any(marker in str(guard).lower() for marker in DEPLOY_MARKERS)):
        errors.append(f"{name}:guard: deployment/probe alternative outside the production job")
    # Lexical regression pins on the guard script. A suspended staging run
    # concludes `skipped`, not `success`, so while deploy-staging is statically
    # disabled every production SHA still needs a staging deploy that succeeded.
    script = "\n".join(step.get("run", "") for step in guard.get("steps", []))
    for required in ("actions/workflows/deploy-staging.yml/runs", 'status="success"',
                     'refuse(f"deploy-staging has no successful run on {sha}")',
                     'rule.get("type") == "required_reviewers"',
                     'refuse("the production Environment has no required reviewers")'):
        if required not in script:
            errors.append(f"{name}:guard: missing check {required}")
    return errors


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
        if name == "deploy-production.yml":
            errors.extend(production_errors(workflow))
            continue
        for job_id, job in jobs.items():
            if name == "deploy-staging.yml":
                if job.get("if") != STATIC_FALSE:
                    errors.append(f"{name}:{job_id}: deployment/probes are not statically disabled")
            else:
                # Retargeting a known CI job must not create an activation bypass.
                text = str(job).lower()
                uses = job.get("uses")
                if ("environment" in job
                        or (uses is not None and REUSABLE_CALLS.get((name, job_id)) != uses)
                        or any(marker in text for marker in DEPLOY_MARKERS)):
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

    def test_reusable_call_cannot_be_retargeted(self):
        for target in ("./.github/workflows/deploy-staging.yml", "./.github/workflows/deploy-production.yml",
                       "org/repo/.github/workflows/pipeline-benchmark.yml@main"):
            with self.subTest(target=target):
                workflows = deepcopy(self.workflows)
                workflows["nightly.yml"]["jobs"]["pipeline-benchmark"]["uses"] = target
                self.assertTrue(suspension_errors(workflows))

    def test_production_route_is_dispatch_only_and_gated(self):
        production = self.workflows["deploy-production.yml"]
        self.assertEqual(production_errors(production), [])
        self.assertEqual(production["jobs"]["production"]["environment"], "production")

        def mutated(change):
            workflow = deepcopy(production)
            change(workflow)
            return production_errors(workflow)

        for trigger in ("push", "pull_request", "schedule", "workflow_run", "workflow_call"):
            with self.subTest(trigger=trigger):
                self.assertTrue(mutated(lambda w, t=trigger: w["on"].update({t: ""})))
        for key, value in (("environment", None), ("environment", "staging"), ("needs", None),
                           ("needs", "other"), ("if", "${{ always() }}")):
            with self.subTest(job_key=key, value=value):
                def change(w, key=key, value=value):
                    job = w["jobs"]["production"]
                    if value is None:
                        job.pop(key, None)
                    else:
                        job[key] = value
                self.assertTrue(mutated(change))
        for step in ({"run": "npx wrangler deploy"}, {"uses": "cloudflare/wrangler-action@pinned"},
                     {"run": 'curl "$PRODUCTION_WORKER_URL/readyz"'}):
            with self.subTest(guard_step=step):
                self.assertTrue(mutated(lambda w, s=step: w["jobs"]["guard"]["steps"].append(s)))
        with self.subTest(guard="environment"):
            self.assertTrue(mutated(lambda w: w["jobs"]["guard"].update({"environment": "production"})))
        for pin in ('status="success"', "deploy-staging.yml/runs", '"required_reviewers"'):
            with self.subTest(dropped=pin):
                def drop(w, pin=pin):
                    for step in w["jobs"]["guard"]["steps"]:
                        if "run" in step:
                            step["run"] = step["run"].replace(pin, "")
                self.assertTrue(mutated(drop))

    def test_overflow_runner_must_name_its_own_job(self):
        self.assertTrue(runner_allowed("worker", self.workflows["check.yml"]["jobs"]["worker"]["runs-on"]))
        for runs_on in (ROUTED_RUNNER.format(job="check"), "ubuntu-latest", ["self-hosted"],
                        "${{ vars.CI_OVERFLOW_RUNNER }}"):
            with self.subTest(runs_on=runs_on):
                self.assertFalse(runner_allowed("worker", runs_on))

    def test_duplicate_keys_cannot_hide_an_enabled_job(self):
        with self.assertRaises(ValueError):
            yaml.load("jobs:\n  deploy:\n    if: '${{ false }}'\n    if: true\n", Loader=UniqueKeyLoader)

    def test_private_runner_and_service_isolation(self):
        for name, workflow in self.workflows.items():
            for job_id, job in workflow["jobs"].items():
                with self.subTest(workflow=name, job=job_id):
                    if "uses" in job:
                        # The called workflow's own jobs are checked here too.
                        self.assertNotIn("runs-on", job)
                        self.assertEqual(job["uses"], REUSABLE_CALLS[(name, job_id)])
                        continue
                    # Main #183 returned container smoke to the shared pool.
                    self.assertTrue(runner_allowed(job_id, job["runs-on"]))
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
                    if (name, job_id) == ("supply-chain.yml", "pr-lint"):
                        expected["pull-requests"] = "read"
                    elif (name, job_id) == ("deploy-production.yml", "guard"):
                        expected = {"contents": "read", "actions": "read", "checks": "read"}
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
