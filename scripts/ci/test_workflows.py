"""Offline workflow policy fixtures; never dispatch, deploy or probe staging."""

from copy import deepcopy
from pathlib import Path
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
STATIC_FALSE = "${{ false }}"
JOB_INVENTORY = {
    # Branch keeps the moderation-db job; main #84 added the supply-chain job.
    # The pin must be the union of both sides.
    "check.yml": {"check", "moderation-db", "parity-docs", "self-role-store", "job-inputs", "container-inputs", "container",
                  "community-db", "feeds-db", "tickets-postgres", "worker", "supply-chain", "required-checks",
                  # TOG-14881: CI standard aggregator; required-checks stays
                  # until the protect-main ruleset flips to ci-ok.
                  "ci-ok"},
    "deploy-production.yml": {"guard", "production"},
    "deploy-staging.yml": {"deploy"},
    "nightly.yml": {"pipeline-benchmark", "advisories", "sweep"},
    "pipeline-benchmark.yml": {"benchmark"},
    "release.yml": {"release-please", "dispatch-checks", "sbom-target", "release-sbom",
                    "attach-sbom"},
    "staging-migrate.yml": {"plan", "apply"},
    "supply-chain.yml": {"pr-lint", "gitleaks"},
    # TOG-10893: read-only SBOM inventory/gates shared by the PR dry-run and releases.
    # `image` builds/scans the untrusted ref with pinned actions only; `verify`
    # runs the local validation/evidence/preflight scripts without ever
    # checking out inputs.ref (CodeQL cache-poisoning gate).
    "sbom.yml": {"image", "verify"},
}
# Reusable-workflow calls are allowed only to these non-deploy workflows.
# TOG-10893 registers the read-only sbom.yml calls alongside the benchmark one.
REUSABLE_CALLS = {("nightly.yml", "pipeline-benchmark"): "./.github/workflows/pipeline-benchmark.yml",
                   ("check.yml", "supply-chain"): "./.github/workflows/sbom.yml",
                   ("release.yml", "release-sbom"): "./.github/workflows/sbom.yml"}
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
    """The active staging workflow keeps exactly one fenced dispatch input.

    `release_fence` exists on main (TOG-11143) so post-handoff release of an
    uninitialized/parked singleton is explicit and auditable; it defaults to
    false and the deploy job itself runs unconditionally (TOG-12856). Anything
    else in the dispatch block (an arming switch, a deploy flag, a default of
    true) is an unreviewed deploy route outside the pinned shape.
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

    Dispatch-only with exactly the nine reviewed inputs (plan/apply
    defaulting to plan, the six identity/evidence inputs required, the
    plan-bound expected_pending list optional at dispatch but required by the
    runner for apply, and the plan_manifest_sha256/plan_run_id pair optional
    at dispatch but required by the runner for apply). Two jobs: `plan` always
    runs through the no-reviewer staging-migrate-plan environment and uploads
    the manifest artifact; `apply` runs only for mode=apply after a green plan
    through the reviewed staging-migrate-apply environment and passes the
    plan-bound inputs to the runner. Each job pins main-branch dispatch, its
    own routed runner, and the pipefail Run step. No push/pull_request/schedule
    trigger, no production path, no wrangler/probe markers: anything else is
    an activation route and must fail closed.
    """
    name = "staging-migrate.yml"
    errors = []
    on = workflow.get("on") or {}
    if set(on) != {"workflow_dispatch"}:
        errors.append(f"{name}: must be dispatch-only (no push/pull_request/schedule)")
    inputs = ((on.get("workflow_dispatch") or {}).get("inputs") or {})
    expected = {"mode", "source_sha", "staging_host", "staging_database",
                "recovery_evidence_ref", "acl_plan_ref", "expected_pending",
                "plan_manifest_sha256", "plan_run_id"}
    if set(inputs) != expected:
        errors.append(f"{name}: workflow_dispatch inputs must be exactly {sorted(expected)}")
    else:
        mode = inputs.get("mode") or {}
        if (mode.get("type") != "choice"
                or set(mode.get("options") or []) != {"plan", "apply"}
                or mode.get("default") != "plan"):
            errors.append(f"{name}: mode must be plan/apply defaulting to plan")
        for key in expected - {"mode", "expected_pending", "plan_manifest_sha256", "plan_run_id"}:
            field = inputs.get(key) or {}
            if str(field.get("required")).lower() != "true":
                errors.append(f"{name}: input {key} must be required")
        pending = inputs.get("expected_pending") or {}
        if (str(pending.get("required")).lower() != "false"
                or pending.get("default") != ""
                or "ascending" not in str(pending.get("description")).lower()):
            errors.append(f"{name}: expected_pending must stay optional, default empty, "
                          "and documented as the ascending reviewed plan list")
        for key in ("plan_manifest_sha256", "plan_run_id"):
            field = inputs.get(key) or {}
            if str(field.get("required")).lower() != "false" or field.get("default") != "":
                errors.append(f"{name}: {key} must stay optional and default empty "
                              "(the runner requires it for apply, not the dispatch)")
        digest = inputs.get("plan_manifest_sha256") or {}
        if "sha" not in str(digest.get("description")).lower():
            errors.append(f"{name}: plan_manifest_sha256 must document the manifest-hash contract "
                          "(apply refuses unless it matches on the same source_sha)")
        acl = inputs.get("acl_plan_ref") or {}
        if "bare" not in str(acl.get("description")).lower():
            errors.append(f"{name}: acl_plan_ref must document the bare reference contract "
                          "(refused before any DDL otherwise)")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"plan", "apply"}:
        errors.append(f"{name}: jobs must be exactly plan and apply")
        return errors
    plan, apply = jobs.get("plan", {}), jobs.get("apply", {})
    if plan.get("environment") != "staging-migrate-plan":
        errors.append(f"{name}:plan: must read the staging-migrate-plan Environment binding")
    if apply.get("environment") != "staging-migrate-apply":
        errors.append(f"{name}:apply: must read the staging-migrate-apply Environment binding")
    # Token-permission check (TOG-15157 gap 2): the plan job produces the
    # manifest with contents:read only, while apply additionally needs
    # actions:read -- and nothing more -- to fetch the producing plan run's
    # manifest artifact for the provenance gate.
    if plan.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:plan: must keep contents:read only (it produces the manifest)")
    if apply.get("permissions") != {"contents": "read", "actions": "read"}:
        errors.append(f"{name}:apply: must carry exactly contents:read plus actions:read "
                      "(provenance artifact fetch, nothing more)")
    if plan.get("if") != "github.ref == 'refs/heads/main'":
        errors.append(f"{name}:plan: must run only from main")
    if apply.get("if") != "github.ref == 'refs/heads/main' && inputs.mode == 'apply'":
        errors.append(f"{name}:apply: must run only from main for mode=apply")
    if apply.get("needs") != "plan":
        errors.append(f"{name}:apply: must wait for a green plan")
    for job_id, job in (("plan", plan), ("apply", apply)):
        if not runner_allowed(job_id, job.get("runs-on")):
            errors.append(f"{name}:{job_id}: must use the routed runner expression for job '{job_id}'")
        text = str(job).lower()
        if job.get("uses") is not None or any(marker in text for marker in DEPLOY_MARKERS):
            errors.append(f"{name}:{job_id}: deployment/probe alternative outside approved deploy workflows")
        run_steps = [step for step in job.get("steps", []) if "tee" in str(step.get("run", ""))]
        if not run_steps:
            errors.append(f"{name}:{job_id}: no Run step piping through tee")
        for step in run_steps:
            if step.get("shell") != "bash" or "set -o pipefail" not in str(step.get("run", "")):
                errors.append(f"{name}:{job_id}: Run step must use a pipefail shell so a "
                              "migrator refusal/failure fails the job instead of reporting green")
    plan_runs = " ".join(str(step.get("run", "")) for step in plan.get("steps", []))
    apply_runs = " ".join(str(step.get("run", "")) for step in apply.get("steps", []))
    # Match the standalone mode flag: the plan-binding flags
    # (--plan-manifest-sha256, --plan-run-id) share the --plan prefix.
    if "--plan " not in plan_runs or "--apply" in plan_runs:
        errors.append(f"{name}:plan: must run the migrator with --plan only")
    if "--apply " not in apply_runs or "--plan " in apply_runs:
        errors.append(f"{name}:apply: must run the migrator with --apply only")
    if "--plan-manifest-sha256" not in apply_runs or "--plan-run-id" not in apply_runs:
        errors.append(f"{name}:apply: must pass the plan-bound manifest hash and run id to the runner")
    if "--plan-manifest-sha256" in plan_runs or "--plan-run-id" in plan_runs:
        errors.append(f"{name}:plan: must not take plan-bound inputs (it produces the manifest)")
    # Provenance anchor (TOG-15157 gap 2): apply fetches the producing plan
    # run's manifest artifact by run id and hands it to the runner, which
    # refuses unless the artifact carries the bound hash. The fetch must fail
    # the job (no continue-on-error) so a wrong run id or missing artifact
    # fails before the runner -- and before any DDL -- ever starts. Plan must
    # not fetch by run id: it produces the manifest.
    if "--plan-manifest-path" not in apply_runs:
        errors.append(f"{name}:apply: must pass the producing run's downloaded manifest to the runner")
    if "--plan-manifest-path" in plan_runs:
        errors.append(f"{name}:plan: must not take the provenance manifest path (it produces the manifest)")
    apply_fetch = [step for step in apply.get("steps", [])
                   if str(step.get("uses", "")).startswith("actions/download-artifact@")]
    if len(apply_fetch) != 1:
        errors.append(f"{name}:apply: must fetch exactly one artifact (the producing plan manifest)")
    else:
        fetch = apply_fetch[0]
        fetch_with = fetch.get("with", {})
        if fetch_with.get("name") != "staging-migrate-manifest":
            errors.append(f"{name}:apply: must fetch the staging-migrate-manifest artifact")
        if "plan_run_id" not in str(fetch_with.get("run-id", "")):
            errors.append(f"{name}:apply: must fetch the artifact from the plan_run_id run")
        if fetch.get("continue-on-error") is True:
            errors.append(f"{name}:apply: the provenance fetch must fail the job, never continue-on-error")
    plan_uses = [step.get("uses", "") for step in plan.get("steps", [])]
    if not any(str(u).startswith("actions/upload-artifact@") for u in plan_uses):
        errors.append(f"{name}:plan: must upload the staging-migrate-manifest.json run artifact")
    if any(str(step.get("uses", "")).startswith("actions/download-artifact@")
           for step in plan.get("steps", [])):
        errors.append(f"{name}:plan: must not fetch artifacts by run id (it produces the manifest)")
    plan_text = str(plan.get("steps", []))
    if "staging-migrate-manifest" not in plan_text:
        errors.append(f"{name}:plan: must name the staging-migrate-manifest artifact")
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
    # Lexical regression pins on the guard script. deploy-staging runs
    # execute (TOG-12856), so every production SHA needs a staging deploy
    # run on that SHA that concluded `success` (a `skipped` run does not
    # satisfy the guard).
    script = "\n".join(step.get("run", "") for step in guard.get("steps", []))
    for required in ("actions/workflows/deploy-staging.yml/runs", 'status="success"',
                     'refuse(f"deploy-staging has no successful run on {sha}")',
                     'rule.get("type") == "required_reviewers"',
                     'refuse("the production Environment has no required reviewers")'):
        if required not in script:
            errors.append(f"{name}:guard: missing check {required}")
    return errors


def staging_active_errors(workflow):
    """The deploy job is live (TOG-12856): no static suspension guard.

    The job must carry no `if:` at all — unconditional on both the push/main
    and workflow_dispatch triggers — while keeping the `staging` environment
    scope, the routed runner for job 'deploy', the fenced `release_fence`
    dispatch shape, default-deny top-level permissions and the least-privilege
    per-job grant. A job-level condition would silently skip deploys on some
    SHAs and leave the production guard refusing those SHAs; step-level
    guards are not an equivalent gate, so any job condition fails closed.
    """
    name = "deploy-staging.yml"
    errors = []
    job = (workflow.get("jobs") or {}).get("deploy", {})
    if "if" in job:
        errors.append(f"{name}:deploy: a job condition would silently skip staging deploys")
    if job.get("environment") != "staging":
        errors.append(f"{name}:deploy: must stay scoped to the staging environment")
    if not runner_allowed("deploy", job.get("runs-on")):
        errors.append(f"{name}:deploy: must use the routed runner expression for job 'deploy'")
    for step in job.get("steps", []):
        if step.get("if") == STATIC_FALSE:
            errors.append(f"{name}:deploy: a statically-disabled step would report success without deploying")
            break
    return errors


def workflow_policy_errors(workflows):
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
            errors.extend(staging_active_errors(workflow))
        if name == "staging-migrate.yml":
            # Manual runner (TOG-11572): pinned shape above, not the
            # staging-deploy policy. The generic environment/marker scan
            # below would flag its staging-migrate Environment binding.
            errors.extend(staging_migrate_errors(workflow))
            continue
        for job_id, job in jobs.items():
            if name == "deploy-staging.yml" and job_id == "deploy":
                continue  # the live deploy route: pinned by the staging checks above
            else:
                # Retargeting a known CI job must not create an unapproved
                # deploy/probe route outside the deploy workflows.
                text = str(job).lower()
                uses = job.get("uses")
                if ("environment" in job
                        or (uses is not None and REUSABLE_CALLS.get((name, job_id)) != uses)
                        or any(marker in text for marker in DEPLOY_MARKERS)):
                    errors.append(f"{name}:{job_id}: deployment/probe alternative outside approved deploy workflows")
    return errors


class WorkflowTests(unittest.TestCase):
    def setUp(self):
        self.workflows = load_workflows()

    def test_staging_runs_unconditionally_for_push_and_dispatch(self):
        staging = self.workflows["deploy-staging.yml"]
        self.assertEqual(staging_dispatch_errors(staging), [])
        self.assertEqual(staging_active_errors(staging), [])
        self.assertEqual(workflow_policy_errors(self.workflows), [])
        self.assertEqual(staging["jobs"]["deploy"]["environment"], "staging")
        self.assertNotIn("if", staging["jobs"]["deploy"])

    def test_any_job_condition_on_staging_deploy_fails(self):
        # The live deploy job carries no `if` at all: a job-level condition
        # would silently skip deploys on some SHAs (including the old
        # `if: ${{ false }}` suspension), leaving the production guard
        # refusing those SHAs with no deploy ever running.
        for guard in ("${{ false }}", "true", "${{ true }}", "${{ vars.ENABLE_STAGING }}",
                      "${{ github.ref == 'refs/heads/main' }}", "${{ inputs.deploy }}",
                      "${{ always() }}"):
            with self.subTest(guard=guard):
                workflows = deepcopy(self.workflows)
                workflows["deploy-staging.yml"]["jobs"]["deploy"]["if"] = guard
                self.assertTrue(workflow_policy_errors(workflows))
                self.assertTrue(staging_active_errors(workflows["deploy-staging.yml"]))

    def test_dispatch_inputs_cannot_arm_the_release_fence(self):
        # The workflow_dispatch inputs block must not gain an arming switch:
        # an attacker-readable input the steps could consult would be an
        # unreviewed deploy route outside the pinned dispatch shape. Each
        # mutation below must trip staging_dispatch_errors (surfaced through
        # workflow_policy_errors on the real inventory).
        for dispatch in ({"inputs": {"deploy": {"description": "deploy now", "type": "boolean", "default": False}}},
                         {"inputs": {"release_fence": {"description": "x", "type": "boolean", "default": True}}},
                         {"inputs": {"enable": {"description": "x", "type": "boolean", "default": False}}},
                         {"inputs": {}},
                         "just-a-string"):
            with self.subTest(dispatch=dispatch):
                workflows = deepcopy(self.workflows)
                workflows["deploy-staging.yml"]["on"]["workflow_dispatch"] = dispatch
                self.assertTrue(workflow_policy_errors(workflows))

    def test_step_level_skip_is_not_a_deploy(self):
        # Neutering the live job step-by-step would report success without
        # deploying, which the production guard would then accept as a
        # successful staging run. Any step-level static disable fails closed.
        workflows = deepcopy(self.workflows)
        job = workflows["deploy-staging.yml"]["jobs"]["deploy"]
        for step in job["steps"]:
            step["if"] = STATIC_FALSE
        self.assertTrue(workflow_policy_errors(workflows))

    def test_new_job_or_workflow_is_not_an_unapproved_deploy_route(self):
        for name in self.workflows:
            with self.subTest(name=name):
                workflows = deepcopy(self.workflows)
                workflows[name]["jobs"]["alternate"] = {"uses": "./.github/workflows/deploy-staging.yml"}
                self.assertTrue(workflow_policy_errors(workflows))
        workflows = deepcopy(self.workflows)
        workflows["alternate.yml"] = {"jobs": {"deploy": {"run": "wrangler deploy"}}}
        self.assertTrue(workflow_policy_errors(workflows))

    def test_retargeting_known_job_fails(self):
        for change in ({"environment": "staging"}, {"uses": "./.github/workflows/deploy-staging.yml"},
                       {"steps": [{"run": 'curl "$STAGING_WORKER_URL/readyz"'}]},
                       {"steps": [{"uses": "cloudflare/wrangler-action@pinned"}]}):
            with self.subTest(change=change):
                workflows = deepcopy(self.workflows)
                workflows["check.yml"]["jobs"]["worker"].update(change)
                self.assertTrue(workflow_policy_errors(workflows))

    def test_reusable_call_cannot_be_retargeted(self):
        for target in ("./.github/workflows/deploy-staging.yml", "./.github/workflows/deploy-production.yml",
                       "org/repo/.github/workflows/pipeline-benchmark.yml@main"):
            with self.subTest(target=target):
                workflows = deepcopy(self.workflows)
                workflows["nightly.yml"]["jobs"]["pipeline-benchmark"]["uses"] = target
                self.assertTrue(workflow_policy_errors(workflows))

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
        self.assertEqual(workflow_policy_errors(self.workflows), [])

        def mutated(change):
            workflow = deepcopy(migrate)
            change(workflow)
            return staging_migrate_errors(workflow)

        for trigger in ("push", "pull_request", "schedule", "workflow_call"):
            with self.subTest(trigger=trigger):
                self.assertTrue(mutated(lambda w, t=trigger: w["on"].update({t: ""})))
        for missing in ("acl_plan_ref", "plan_manifest_sha256", "plan_run_id"):
            with self.subTest(missing=missing):
                def drop(w, missing=missing):
                    del w["on"]["workflow_dispatch"]["inputs"][missing]
                self.assertTrue(mutated(drop))
        with self.subTest(widened="plan-required"):
            def require(w):
                w["on"]["workflow_dispatch"]["inputs"]["plan_manifest_sha256"]["required"] = True
            self.assertTrue(mutated(require))
        with self.subTest(mode="apply-default"):
            def widen(w):
                mode = w["on"]["workflow_dispatch"]["inputs"]["mode"]
                mode["default"] = "apply"
            self.assertTrue(mutated(widen))
        with self.subTest(missing="apply-job"):
            def drop(w):
                del w["jobs"]["apply"]
            self.assertTrue(mutated(drop))
        for job_id in ("plan", "apply"):
            for key, value in (("environment", None), ("environment", "production"),
                               ("if", None), ("if", "${{ always() }}"),
                               ("runs-on", "ubuntu-latest")):
                with self.subTest(job=job_id, job_key=key, value=value):
                    def change(w, key=key, value=value, job_id=job_id):
                        job = w["jobs"][job_id]
                        if value is None:
                            job.pop(key, None)
                        else:
                            job[key] = value
                    self.assertTrue(mutated(change))
        for job_id in ("plan", "apply"):
            for step in ({"run": "npx wrangler deploy"},
                         {"run": 'curl "$STAGING_WORKER_URL/readyz"'}):
                with self.subTest(job=job_id, deploy_step=step):
                    def add(w, s=step, job_id=job_id):
                        w["jobs"][job_id]["steps"].append(s)
                    self.assertTrue(mutated(add))
        with self.subTest(apply="no-needs"):
            def drop(w):
                del w["jobs"]["apply"]["needs"]
            self.assertTrue(mutated(drop))
        with self.subTest(apply="always-runs"):
            def widen(w):
                w["jobs"]["apply"]["if"] = "github.ref == 'refs/heads/main'"
            self.assertTrue(mutated(widen))
        with self.subTest(environments="swapped"):
            def swap(w):
                w["jobs"]["plan"]["environment"] = "staging-migrate-apply"
                w["jobs"]["apply"]["environment"] = "staging-migrate-plan"
            self.assertTrue(mutated(swap))
        # The plan hash binds apply to the reviewed plan: dropping either
        # runner flag from apply, or the plan manifest upload, must fail.
        with self.subTest(apply="no-plan-hash-flag"):
            def drop_hash(w):
                for step in w["jobs"]["apply"]["steps"]:
                    if "--plan-manifest-sha256" in str(step.get("run", "")):
                        step["run"] = step["run"].replace("--plan-manifest-sha256 \"$PLAN_MANIFEST_SHA256\" ", "")
            self.assertTrue(mutated(drop_hash))
        with self.subTest(apply="no-plan-run-flag"):
            def drop_run(w):
                for step in w["jobs"]["apply"]["steps"]:
                    if "--plan-run-id" in str(step.get("run", "")):
                        step["run"] = step["run"].replace("--plan-run-id \"$PLAN_RUN_ID\" ", "")
            self.assertTrue(mutated(drop_run))
        with self.subTest(plan="hash-flags"):
            def widen(w):
                for step in w["jobs"]["plan"]["steps"]:
                    if "--expected-pending" in str(step.get("run", "")):
                        step["run"] = step["run"].replace(
                            "--expected-pending \"$EXPECTED_PENDING\"",
                            "--expected-pending \"$EXPECTED_PENDING\" --plan-manifest-sha256 \"$PLAN_MANIFEST_SHA256\"")
            self.assertTrue(mutated(widen))
        with self.subTest(plan="no-manifest-upload"):
            def drop_upload(w):
                w["jobs"]["plan"]["steps"] = [
                    step for step in w["jobs"]["plan"]["steps"]
                    if "upload-artifact" not in str(step.get("uses", ""))
                ]
            self.assertTrue(mutated(drop_upload))
        # Provenance anchor (TOG-15157 gap 2): dropping the producing-run
        # fetch, its run-id binding, its failure-closed posture or the
        # runner's manifest-path flag must fail; widening plan with either
        # end of the anchor, or widening the apply token grant, must fail.
        with self.subTest(apply="no-provenance-fetch"):
            def drop_fetch(w):
                w["jobs"]["apply"]["steps"] = [
                    step for step in w["jobs"]["apply"]["steps"]
                    if "download-artifact" not in str(step.get("uses", ""))
                ]
            self.assertTrue(mutated(drop_fetch))
        with self.subTest(apply="no-provenance-run-id"):
            def drop_run_id(w):
                for step in w["jobs"]["apply"]["steps"]:
                    with_ = step.get("with", {})
                    if "download-artifact" in str(step.get("uses", "")) and "run-id" in with_:
                        del with_["run-id"]
            self.assertTrue(mutated(drop_run_id))
        with self.subTest(apply="provenance-fetch-continues-on-error"):
            def soften(w):
                for step in w["jobs"]["apply"]["steps"]:
                    if "download-artifact" in str(step.get("uses", "")):
                        step["continue-on-error"] = True
            self.assertTrue(mutated(soften))
        with self.subTest(apply="no-provenance-path-flag"):
            def drop_path(w):
                for step in w["jobs"]["apply"]["steps"]:
                    if "--plan-manifest-path" in str(step.get("run", "")):
                        step["run"] = step["run"].replace(
                            " --plan-manifest-path producing-plan/staging-migrate-manifest.json", "")
            self.assertTrue(mutated(drop_path))
        with self.subTest(plan="provenance-fetch"):
            def widen(w):
                w["jobs"]["plan"]["steps"].append(
                    {"uses": "actions/download-artifact@pinned",
                     "with": {"name": "staging-migrate-manifest"}})
            self.assertTrue(mutated(widen))
        with self.subTest(plan="provenance-path-flag"):
            def widen(w):
                for step in w["jobs"]["plan"]["steps"]:
                    if "--expected-pending" in str(step.get("run", "")):
                        step["run"] = step["run"].replace(
                            "--expected-pending \"$EXPECTED_PENDING\"",
                            "--expected-pending \"$EXPECTED_PENDING\" "
                            "--plan-manifest-path producing-plan/staging-migrate-manifest.json")
            self.assertTrue(mutated(widen))
        # Each mutation must differ from that job's pinned grant: plan keeps
        # contents:read only, apply carries exactly contents:read plus
        # actions:read. Narrowing apply (losing the provenance fetch) or
        # widening either job must fail.
        for job_id, permissions in (
                ("plan", {"contents": "read", "actions": "read"}),
                ("plan", {}),
                ("apply", {"contents": "read"}),
                ("apply", {"contents": "read", "actions": "read", "checks": "read"}),
                ("apply", {"contents": "write", "actions": "read"})):
            with self.subTest(job=job_id, permissions=permissions):
                def change(w, job_id=job_id, permissions=permissions):
                    w["jobs"][job_id]["permissions"] = permissions
                self.assertTrue(mutated(change))

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
                    elif (name, job_id) == ("staging-migrate.yml", "apply"):
                        # TOG-15157: read-only fetch of the producing plan
                        # run's manifest artifact for the provenance gate.
                        expected = {"contents": "read", "actions": "read"}
                    elif (name, job_id) == ("deploy-production.yml", "guard"):
                        expected = {"contents": "read", "actions": "read", "checks": "read"}
                    elif (name, job_id) == ("release.yml", "release-please"):
                        expected = {"contents": "write", "pull-requests": "write"}
                    elif (name, job_id) == ("release.yml", "dispatch-checks"):
                        expected = {"actions": "write"}
                    elif (name, job_id) == ("release.yml", "attach-sbom"):
                        # TOG-10893: uploads verified SBOMs to the published tag.
                        expected = {"contents": "write"}
                    self.assertEqual(job["permissions"], expected)
                    for step in job.get("steps", []):
                        if step.get("uses", "").startswith("actions/checkout@"):
                            self.assertEqual(step["with"]["persist-credentials"], "false")


if __name__ == "__main__":
    unittest.main()
