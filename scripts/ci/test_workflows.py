"""Offline workflow policy fixtures; never dispatch, deploy or probe staging."""

from copy import deepcopy
from pathlib import Path
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
STATIC_FALSE = "${{ false }}"
JOB_INVENTORY = {
    "check.yml": {"check", "moderation-db", "parity-docs", "self-role-store", "job-inputs", "container-inputs", "container",
                  "community-db", "feeds-db", "tickets-postgres", "worker"},
    "deploy-production.yml": {"guard", "production"},
    "deploy-staging.yml": {"deploy"},
    "nightly.yml": {"pipeline-benchmark", "advisories", "sweep"},
    "pipeline-benchmark.yml": {"benchmark"},
    "release.yml": {"release-please", "dispatch-checks"},
    "staging-migrate.yml": {"migrate"},
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


def staging_dispatch_errors(workflow):
    """The suspended staging workflow keeps exactly one fenced dispatch input.

    `release_fence` exists on main (TOG-11143) so post-handoff re-activation
    is explicit and auditable; it defaults to false and the job-level
    `if: ${{ false }}` stays authoritative while suspension holds. Anything
    else in the dispatch block (an arming switch, a deploy flag, a default of
    true) is an activation route around the static suspension.
    """
    name = "deploy-staging.yml"
    errors = []
    on = workflow.get("on") or {}
    if set(on) != {"push", "workflow_dispatch"}:
        errors.append(f"{name}: triggers changed")
    if on.get("push") != {"branches": ["main"]}:
        errors.append(f"{name}: push trigger changed")
    dispatch = on.get("workflow_dispatch") or {}
    if set(dispatch) != {"inputs"} or set(dispatch.get("inputs") or {}) != {"release_fence"}:
        errors.append(f"{name}: workflow_dispatch must carry only the release_fence input")
    else:
        fence = dispatch["inputs"]["release_fence"] or {}
        if fence.get("type") != "boolean" or fence.get("default") not in (False, "false"):
            errors.append(f"{name}: release_fence must be an opt-in boolean defaulting to false")
    return errors


def staging_migrate_errors(workflow):
    """Manual staging-only SQLx migration runner (TOG-11572).

    Dispatch-only with exactly the six reviewed inputs (plan/apply defaulting
    to plan, everything else required), reading the pre-existing
    staging-migrate Environment binding, main-branch dispatches only, and the
    routed runner for job 'migrate'. No push/pull_request/schedule trigger, no
    production path, no wrangler/probe markers: anything else is an activation
    route and must fail closed.
    """
    name = "staging-migrate.yml"
    errors = []
    on = workflow.get("on") or {}
    if set(on) != {"workflow_dispatch"}:
        errors.append(f"{name}: must be dispatch-only (no push/pull_request/schedule)")
    inputs = ((on.get("workflow_dispatch") or {}).get("inputs") or {})
    expected = {"mode", "source_sha", "staging_host", "staging_database",
                "recovery_evidence_ref", "acl_plan_ref"}
    if set(inputs) != expected:
        errors.append(f"{name}: workflow_dispatch inputs must be exactly {sorted(expected)}")
    else:
        mode = inputs.get("mode") or {}
        if (mode.get("type") != "choice"
                or set(mode.get("options") or []) != {"plan", "apply"}
                or mode.get("default") != "plan"):
            errors.append(f"{name}: mode must be plan/apply defaulting to plan")
        for key in expected - {"mode"}:
            field = inputs.get(key) or {}
            if str(field.get("required")).lower() != "true":
                errors.append(f"{name}: input {key} must be required")
    job = (workflow.get("jobs") or {}).get("migrate", {})
    if job.get("environment") != "staging-migrate":
        errors.append(f"{name}:migrate: must read the staging-migrate Environment binding")
    if job.get("if") != "github.ref == 'refs/heads/main'":
        errors.append(f"{name}:migrate: must run only from main")
    if not runner_allowed("migrate", job.get("runs-on")):
        errors.append(f"{name}:migrate: must use the routed runner expression for job 'migrate'")
    text = str(job).lower()
    if job.get("uses") is not None or any(marker in text for marker in DEPLOY_MARKERS):
        errors.append(f"{name}:migrate: deployment/probe alternative outside suspended workflow")
    return errors


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
        if name == "deploy-staging.yml":
            errors.extend(staging_dispatch_errors(workflow))
        if name == "staging-migrate.yml":
            # Manual runner (TOG-11572): pinned shape above, not the
            # suspended-deploy policy. The generic environment/marker scan
            # below would flag its staging-migrate Environment binding.
            errors.extend(staging_migrate_errors(workflow))
            continue
        for job_id, job in jobs.items():
            if name == "deploy-staging.yml":
                if job.get("if") != STATIC_FALSE:
                    errors.append(f"{name}:{job_id}: deployment/probes are not statically disabled")
                if job_id == "deploy":
                    continue  # the dispatch shape is pinned by staging_dispatch_errors above
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
        self.assertEqual(staging_dispatch_errors(staging), [])
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

    def test_dispatch_inputs_cannot_arm_the_release_fence(self):
        # The workflow_dispatch inputs block must not gain an arming switch:
        # an attacker-readable input that the (suspended) steps could consult
        # would be an activation route around the static `if: false`. Each
        # mutation below must trip staging_dispatch_errors (surfaced through
        # suspension_errors on the real inventory).
        for dispatch in ({"inputs": {"deploy": {"description": "deploy now", "type": "boolean", "default": False}}},
                         {"inputs": {"release_fence": {"description": "x", "type": "boolean", "default": True}}},
                         {"inputs": {"enable": {"description": "x", "type": "boolean", "default": False}}},
                         {"inputs": {}},
                         "just-a-string"):
            with self.subTest(dispatch=dispatch):
                workflows = deepcopy(self.workflows)
                workflows["deploy-staging.yml"]["on"]["workflow_dispatch"] = dispatch
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

    def test_staging_migrate_runner_shape_is_pinned(self):
        # TOG-11572: the slice's own workflow must satisfy its carve-out, and
        # every widening (extra trigger, dropped input, lost environment,
        # retargeted runner, wrangler/probe step) must fail closed here.
        migrate = self.workflows["staging-migrate.yml"]
        self.assertEqual(staging_migrate_errors(migrate), [])
        self.assertEqual(suspension_errors(self.workflows), [])

        def mutated(change):
            workflow = deepcopy(migrate)
            change(workflow)
            return staging_migrate_errors(workflow)

        for trigger in ("push", "pull_request", "schedule", "workflow_call"):
            with self.subTest(trigger=trigger):
                self.assertTrue(mutated(lambda w, t=trigger: w["on"].update({t: ""})))
        with self.subTest(missing="acl_plan_ref"):
            def drop(w):
                del w["on"]["workflow_dispatch"]["inputs"]["acl_plan_ref"]
            self.assertTrue(mutated(drop))
        with self.subTest(mode="apply-default"):
            def widen(w):
                mode = w["on"]["workflow_dispatch"]["inputs"]["mode"]
                mode["default"] = "apply"
            self.assertTrue(mutated(widen))
        for key, value in (("environment", None), ("environment", "production"),
                           ("if", None), ("if", "${{ always() }}"),
                           ("runs-on", "ubuntu-latest")):
            with self.subTest(job_key=key, value=value):
                def change(w, key=key, value=value):
                    job = w["jobs"]["migrate"]
                    if value is None:
                        job.pop(key, None)
                    else:
                        job[key] = value
                self.assertTrue(mutated(change))
        for step in ({"run": "npx wrangler deploy"},
                     {"run": 'curl "$STAGING_WORKER_URL/readyz"'}):
            with self.subTest(deploy_step=step):
                self.assertTrue(mutated(lambda w, s=step: w["jobs"]["migrate"]["steps"].append(s)))

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
                    if (name, job_id) == ("deploy-staging.yml", "deploy"):
                        # The suspended staging job carries no per-job grant:
                        # top-level `permissions: {}` is the default-deny and
                        # nothing on a statically-disabled job needs a token.
                        self.assertNotIn("permissions", job)
                        continue
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
