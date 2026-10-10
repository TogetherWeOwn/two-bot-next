"""Offline workflow policy fixtures; never dispatch, deploy or probe staging."""

from copy import deepcopy
from pathlib import Path
import re
import tomllib
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
STATIC_FALSE = "${{ false }}"
JOB_INVENTORY = {
    # Branch keeps the moderation-db job; main #84 added the supply-chain job.
    # The pin must be the union of both sides.
    "check.yml": {"check", "moderation-db", "parity-docs", "self-role-store", "job-inputs", "container-inputs", "container",
                  "community-db", "feeds-db", "tickets-postgres", "worker", "supply-chain", "ci-ok",
                  # `check` is now the lint lane; the test steps it used to
                  # carry run in these three parallel lanes, all gated by ci-ok.
                  "rust-tests", "ignored-db-stores", "ignored-db-runtime"},
    "deploy-production.yml": {"guard", "production"},
    "deploy-staging.yml": {"deploy"},
    "nightly.yml": {"changes", "pipeline-benchmark", "advisories", "sweep"},
    "pipeline-benchmark.yml": {"benchmark"},
    "release.yml": {"release-please", "dispatch-checks", "sbom-target", "release-sbom",
                    "attach-sbom"},
    "staging-migrate.yml": {"plan", "claim", "apply"},
    "production-migrate.yml": {"plan", "claim", "apply"},
    # TOG-14008: manual staging-only Worker rollback drill; pinned shape below.
    "staging-rollback-drill.yml": {"drill"},
    # Container-image backout/restore drill: manual, staging-only, pinned shape below.
    "staging-container-drill.yml": {"container-drill"},
    # Manual read-only staging events read for the B2 soak; pinned shape below.
    "staging-events-read.yml": {"read"},
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


# Staging deploy push filter. Each deploy restarts the bot container and drops the
# gateway for ~2 minutes, so a push that touches only non-runtime paths must not
# start one (the B2 soak needs a stable staging). The filter is a deny-list so an
# unclassified path still deploys; these samples pin both sides of it.
RUNTIME_PATHS = (
    "crates/bot/src/main.rs", "crates/core/Cargo.toml", "crates/core/README.md",
    "crates/core/tests/fixtures/reference_settings.json", "src/main.rs",
    "sql/web_v1.sql", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "deny.toml",
    "migrations.lock", "Dockerfile", "Dockerfile.distroless", ".dockerignore",
    "wrangler/wrangler.toml", "wrangler/src/index.ts", "wrangler/package.json",
    "wrangler/package-lock.json", "wrangler/scripts/ownership-control.mjs",
    "wrangler/README.md", "scripts/staging_rollout.py", "scripts/container-smoke.py",
    ".github/workflows/deploy-staging.yml", ".github/workflows/check.yml",
    ".github/scripts/anything.py", "deploy/two-bot.service", "tests/voice_templates/corpus.json",
    "release-please-config.json", ".release-please-manifest.json", "fuzz/Cargo.toml",
    "a-new-top-level-dir/file.txt", ".cargo/config.toml",
)
NON_RUNTIME_PATHS = (
    "docs/runbook.md", "docs/parity-baseline.json", "docs/adr/0001-example.md",
    "docs/soak-checklist.json", "README.md", "CHANGELOG.md", "AGENTS.md",
    "CONTRIBUTING.md", "LICENSE", ".editorconfig", ".gitignore", ".gitleaks.toml",
    ".gitleaksignore", ".github/ISSUE_TEMPLATE/bug_report.yml",
    ".github/pull_request_template.md", ".github/CODEOWNERS", ".github/dependabot.yml",
)


def filter_pattern_matches(pattern, path):
    """GitHub path-filter semantics: anchored at the root, `*` stops at `/`, `**` does not."""
    regex, i = "", 0
    while i < len(pattern):
        if pattern.startswith("**", i):
            regex, i = regex + ".*", i + 2
        elif pattern[i] == "*":
            regex, i = regex + "[^/]*", i + 1
        elif pattern[i] == "?":
            regex, i = regex + "[^/]", i + 1
        else:
            regex, i = regex + re.escape(pattern[i]), i + 1
    return re.fullmatch(regex, path) is not None


def staging_push_filter_errors(name, push):
    """The push trigger is main-only with a deny-list of non-runtime paths.

    GitHub skips a push run only when every changed path matches `paths-ignore`,
    so a runtime path matching ANY pattern would be skipped only when it ships
    alone. Pin that no pattern matches a runtime path, that the known non-runtime
    set stays skipped, and that an allow-list (`paths`) never replaces the
    deny-list (an unclassified path must still deploy).
    """
    if not isinstance(push, dict) or push.get("branches") != ["main"]:
        return [f"{name}: push trigger must be branches [main]"]
    errors = []
    if set(push) - {"branches", "paths-ignore"}:
        errors.append(f"{name}: push trigger may only carry branches and paths-ignore")
    patterns = push.get("paths-ignore") or []
    if not isinstance(patterns, list) or not all(isinstance(p, str) for p in patterns):
        return errors + [f"{name}: paths-ignore must be a list of patterns"]
    for path in RUNTIME_PATHS:
        hits = [p for p in patterns if filter_pattern_matches(p, path)]
        if hits:
            errors.append(f"{name}: paths-ignore {hits} would skip a runtime path {path}")
    for path in NON_RUNTIME_PATHS:
        if not any(filter_pattern_matches(p, path) for p in patterns):
            errors.append(f"{name}: paths-ignore no longer skips non-runtime path {path}")
    return errors


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
    errors.extend(staging_push_filter_errors(name, on.get("push")))
    dispatch = on.get("workflow_dispatch") or {}
    if set(dispatch) != {"inputs"} or set(dispatch.get("inputs") or {}) != {"release_fence"}:
        errors.append(f"{name}: workflow_dispatch must carry only the release_fence input")
    else:
        fence = dispatch["inputs"]["release_fence"] or {}
        if fence.get("type") != "boolean" or fence.get("default") not in (False, "false"):
            errors.append(f"{name}: release_fence must be an opt-in boolean defaulting to false")
    return errors


def staging_claim_errors(claim, name="staging-migrate.yml",
                           job_name="staging-migrate (claim)",
                           manifest="staging-migrate-manifest",
                           claim_artifact="staging-migrate-apply-claim",
                           script="scripts/ci/staging_migrate_claim.py",
                           host_env="STAGING_HOST", host_input="staging_host",
                           db_env="STAGING_DATABASE", db_input="staging_database"):
    """Claim transport must complete before the protected apply job waits.

    Parametrized by workflow name, job identity, artifact names, publisher
    script and host/database env/input names so the production mirror pins the
    same shape with its own names.
    """
    errors = []
    prefix = f"{name}:claim:"
    if claim.get("name") != job_name:
        errors.append(f"{prefix} consumer job identity changed")
    if "environment" in claim or "secrets." in str(claim) or "uses" in claim:
        errors.append(f"{prefix} must be unprotected with no secrets or reusable job")
    if claim.get("permissions") != {"contents": "read"}:
        errors.append(f"{prefix} must keep contents:read only")
    if claim.get("needs") != "plan" or claim.get("if") != "github.ref == 'refs/heads/main' && inputs.mode == 'apply'":
        errors.append(f"{prefix} must wait for a green plan, main and mode=apply")
    if not runner_allowed("claim", claim.get("runs-on")):
        errors.append(f"{prefix} must use the claim routed runner")
    steps = claim.get("steps", [])
    if len(steps) != 4 or any("if" in s or "continue-on-error" in s for s in steps):
        errors.append(f"{prefix} must execute four fail-closed transport steps")
        return errors
    checkout, fetch, publish, upload = steps
    if (not str(checkout.get("uses", "")).startswith("actions/checkout@")
            or checkout.get("with") != {"persist-credentials": "false", "ref": "${{ github.sha }}"}):
        errors.append(f"{prefix} checkout must pin the workflow head without persisted credentials")
    if (not str(fetch.get("uses", "")).startswith("actions/download-artifact@")
            or fetch.get("with") != {"name": manifest, "path": "current-plan"}):
        errors.append(f"{prefix} must read this dispatch's plan artifact, not an arbitrary run")
    expected_env = {key: "${{ inputs." + value + " }}" for key, value in (
        ("MODE", "mode"), ("SOURCE_SHA", "source_sha"), (host_env, host_input),
        (db_env, db_input), ("RECOVERY_REF", "recovery_evidence_ref"),
        ("ACL_REF", "acl_plan_ref"), ("EXPECTED_PENDING", "expected_pending"),
        ("PLAN_MANIFEST_SHA256", "plan_manifest_sha256"), ("PLAN_RUN_ID", "plan_run_id"))}
    if publish.get("env") != expected_env or publish.get("shell") != "bash":
        errors.append(f"{prefix} must pass exactly the dispatch fields via env, in bash")
    script_text = str(publish.get("run", ""))
    for pin in ("set -o pipefail", f"python3 {script}",
                f"--manifest current-plan/{manifest}.json",
                f"--output {claim_artifact}.json"):
        if pin not in script_text:
            errors.append(f"{prefix} missing publisher pin {pin}")
    if not str(upload.get("uses", "")).startswith("actions/upload-artifact@"):
        errors.append(f"{prefix} must upload the claim")
    options = upload.get("with", {})
    if options != {"name": claim_artifact, "path": f"{claim_artifact}.json",
                   "if-no-files-found": "error", "retention-days": "14", "compression-level": "0"}:
        errors.append(f"{prefix} must publish the named claim with stored ZIP entries and fail if absent")
    return errors


def production_claim_errors(claim):
    """Production mirror of the claim transport (production-migrate.yml)."""
    return staging_claim_errors(
        claim,
        name="production-migrate.yml",
        job_name="production-migrate (claim)",
        manifest="production-migrate-manifest",
        claim_artifact="production-migrate-apply-claim",
        script="scripts/ci/production_migrate_claim.py",
        host_env="PRODUCTION_HOST",
        host_input="production_host",
        db_env="PRODUCTION_DATABASE",
        db_input="production_database",
    )


def job_env_text(job):
    """Job-level `env` plus every step's `env`: everywhere a binding can be exported."""
    return " ".join([str(job.get("env", ""))]
                    + [str(step.get("env", "")) for step in job.get("steps", [])])


def migrate_errors(workflow, *, name, plan_env, apply_env, plan_secret,
                     migrator_secret, host_input, db_input, manifest,
                     target_flag, host_flag, db_flag, other_host_flag,
                     other_db_flag, other_plan_secret, other_migrator_secret):
    """Parametrized migration-runner shape (staging and production mirrors).

    Dispatch-only with exactly the nine reviewed inputs, three jobs
    (plan always runs through the no-reviewer plan environment, unprotected
    claim transports the apply request, apply runs only for mode=apply after a
    green plan and claim through the reviewed apply environment), each job with
    its own routed runner and pipefail Run step. No push/pull_request/schedule
    trigger, no wrangler/probe markers. The caller supplies the workflow file
    name, environment names, secret names, host/database input names, artifact
    prefix, runner target/host/database flags and the other target's flags and
    secrets (which must never appear).
    """
    errors = []
    on = workflow.get("on") or {}
    if set(on) != {"workflow_dispatch"}:
        errors.append(f"{name}: must be dispatch-only (no push/pull_request/schedule)")
    inputs = ((on.get("workflow_dispatch") or {}).get("inputs") or {})
    expected = {"mode", "source_sha", host_input, db_input,
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
        host_desc = inputs.get(host_input) or {}
        if "binding must match" not in str(host_desc.get("description")).lower():
            errors.append(f"{name}: {host_input} must document the binding-match contract")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"plan", "claim", "apply"}:
        errors.append(f"{name}: jobs must be exactly plan, claim and apply")
        return errors
    plan, claim, apply = jobs.get("plan", {}), jobs.get("claim", {}), jobs.get("apply", {})
    if name == "staging-migrate.yml":
        errors.extend(staging_claim_errors(claim))
    else:
        errors.extend(production_claim_errors(claim))
    if plan.get("environment") != plan_env:
        errors.append(f"{name}:plan: must read the {plan_env} Environment binding")
    if apply.get("environment") != apply_env:
        errors.append(f"{name}:apply: must read the {apply_env} Environment binding")
    if plan.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:plan: must keep contents:read only (it produces the manifest)")
    if apply.get("permissions") != {"contents": "read", "actions": "read"}:
        errors.append(f"{name}:apply: must carry exactly contents:read plus actions:read "
                      "(provenance artifact fetch, nothing more)")
    if plan.get("if") != "github.ref == 'refs/heads/main'":
        errors.append(f"{name}:plan: must run only from main")
    if apply.get("if") != "github.ref == 'refs/heads/main' && inputs.mode == 'apply'":
        errors.append(f"{name}:apply: must run only from main for mode=apply")
    if apply.get("needs") != ["plan", "claim"]:
        errors.append(f"{name}:apply: must wait for a green plan and pre-approval claim")
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
    plan_env_text = job_env_text(plan)
    apply_env_text = job_env_text(apply)
    plan_job, apply_job = str(plan).lower(), str(apply).lower()
    if plan_secret not in plan_env_text:
        errors.append(f"{name}:plan: must read only the {plan_secret} binding")
    if migrator_secret.lower() in plan_job:
        errors.append(f"{name}:plan: must never read the migrator {migrator_secret} binding")
    if migrator_secret not in apply_env_text:
        errors.append(f"{name}:apply: must read only the {migrator_secret} binding")
    if plan_secret.lower() in apply_job:
        errors.append(f"{name}:apply: must never read the plan {plan_secret} binding")
    # Cross-target isolation: a staging job never reads a production binding
    # and a production job never reads a staging binding, in any case or key.
    for other in (other_plan_secret.lower(), other_migrator_secret.lower()):
        if other in plan_job:
            errors.append(f"{name}:plan: must never read the other target's {other} binding")
        if other in apply_job:
            errors.append(f"{name}:apply: must never read the other target's {other} binding")
    for job_id, text in (("plan", plan_job), ("apply", apply_job)):
        if "tojson(secrets" in text.replace(" ", "") or "secrets[" in text.replace(" ", ""):
            errors.append(f"{name}:{job_id}: must name each secret explicitly "
                          "(no toJSON(secrets) or indexed secrets access)")
    plan_runs = " ".join(str(step.get("run", "")) for step in plan.get("steps", []))
    apply_runs = " ".join(str(step.get("run", "")) for step in apply.get("steps", []))
    if target_flag not in plan_runs or target_flag not in apply_runs:
        errors.append(f"{name}: must run the migrator with {target_flag} on both jobs")
    if other_host_flag in plan_runs or other_host_flag in apply_runs:
        errors.append(f"{name}: must never pass the other target's {other_host_flag} flag")
    if other_db_flag in plan_runs or other_db_flag in apply_runs:
        errors.append(f"{name}: must never pass the other target's {other_db_flag} flag")
    if host_flag not in plan_runs or host_flag not in apply_runs:
        errors.append(f"{name}: must pass {host_flag} to the runner on both jobs")
    if db_flag not in plan_runs or db_flag not in apply_runs:
        errors.append(f"{name}: must pass {db_flag} to the runner on both jobs")
    if "--plan " not in plan_runs or "--apply" in plan_runs:
        errors.append(f"{name}:plan: must run the migrator with --plan only")
    if "--apply " not in apply_runs or "--plan " in apply_runs:
        errors.append(f"{name}:apply: must run the migrator with --apply only")
    if "--plan-manifest-sha256" not in apply_runs or "--plan-run-id" not in apply_runs:
        errors.append(f"{name}:apply: must pass the plan-bound manifest hash and run id to the runner")
    if "--plan-manifest-sha256" in plan_runs or "--plan-run-id" in plan_runs:
        errors.append(f"{name}:plan: must not take plan-bound inputs (it produces the manifest)")
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
        if fetch_with.get("name") != manifest:
            errors.append(f"{name}:apply: must fetch the {manifest} artifact")
        if "plan_run_id" not in str(fetch_with.get("run-id", "")):
            errors.append(f"{name}:apply: must fetch the artifact from the plan_run_id run")
        if fetch.get("continue-on-error") is True:
            errors.append(f"{name}:apply: the provenance fetch must fail the job, never continue-on-error")
    plan_uses = [step.get("uses", "") for step in plan.get("steps", [])]
    if not any(str(u).startswith("actions/upload-artifact@") for u in plan_uses):
        errors.append(f"{name}:plan: must upload the {manifest}.json run artifact")
    if any(str(step.get("uses", "")).startswith("actions/download-artifact@")
           for step in plan.get("steps", [])):
        errors.append(f"{name}:plan: must not fetch artifacts by run id (it produces the manifest)")
    plan_text = str(plan.get("steps", []))
    if manifest not in plan_text:
        errors.append(f"{name}:plan: must name the {manifest} artifact")
    uploads = [step for step in plan.get("steps", [])
               if str(step.get("uses", "")).startswith("actions/upload-artifact@")]
    if len(uploads) != 1 or uploads[0].get("with", {}).get("compression-level") != "0":
        errors.append(f"{name}:plan: must upload the manifest with stored ZIP entries")
    return errors


def staging_migrate_errors(workflow, name="staging-migrate.yml",
                           plan_env="staging-migrate-plan", apply_env="staging-migrate-apply",
                           plan_secret="TWO_BOT_STAGING_PLAN_DATABASE_URL",
                           migrator_secret="TWO_BOT_STAGING_MIGRATOR_DATABASE_URL",
                           host_input="staging_host", db_input="staging_database",
                           manifest="staging-migrate-manifest",
                           target_flag="--target staging", host_flag="--staging-host",
                           db_flag="--staging-database", other_host_flag="--production-host",
                           other_db_flag="--production-database",
                           other_plan_secret="TWO_BOT_PRODUCTION_PLAN_DATABASE_URL",
                           other_migrator_secret="TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL"):
    """Manual staging-only SQLx migration runner. Parametrized by name,
    environments and secret names so the production mirror pins the same shape.
    """
    return migrate_errors(workflow, name=name, plan_env=plan_env, apply_env=apply_env,
                          plan_secret=plan_secret, migrator_secret=migrator_secret,
                          host_input=host_input, db_input=db_input, manifest=manifest,
                          target_flag=target_flag, host_flag=host_flag, db_flag=db_flag,
                          other_host_flag=other_host_flag, other_db_flag=other_db_flag,
                          other_plan_secret=other_plan_secret,
                          other_migrator_secret=other_migrator_secret)


def production_migrate_errors(workflow):
    """Production mirror of the migration-runner shape."""
    return migrate_errors(workflow, name="production-migrate.yml",
                          plan_env="production-migrate-plan", apply_env="production-migrate-apply",
                          plan_secret="TWO_BOT_PRODUCTION_PLAN_DATABASE_URL",
                          migrator_secret="TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL",
                          host_input="production_host", db_input="production_database",
                          manifest="production-migrate-manifest",
                          target_flag="--target production", host_flag="--production-host",
                          db_flag="--production-database", other_host_flag="--staging-host",
                          other_db_flag="--staging-database",
                          other_plan_secret="TWO_BOT_STAGING_PLAN_DATABASE_URL",
                          other_migrator_secret="TWO_BOT_STAGING_MIGRATOR_DATABASE_URL")


ROLLBACK_DRILL_ENV = {
    "STAGING_WORKER_URL": "${{ vars.STAGING_WORKER_URL }}",
    "OWNERSHIP_CONTROL_TOKEN": "${{ secrets.STAGING_OWNERSHIP_CONTROL_TOKEN }}",
    "CLOUDFLARE_API_TOKEN": "${{ secrets.CLOUDFLARE_API_TOKEN }}",
    "CLOUDFLARE_ACCOUNT_ID": "${{ secrets.CLOUDFLARE_ACCOUNT_ID }}",
    "TARGET_VERSION": "${{ inputs.target_version }}",
}


def staging_rollback_drill_errors(workflow):
    """Manual staging-only Worker-version rollback drill (TOG-14008).

    Dispatch-only with the single required `target_version` input. One job runs
    from main through the `staging` environment and shares the deploy-staging
    concurrency group (never cancelling), so a push deploy cannot interleave.
    Exactly three steps: pinned checkout, pinned setup-node, and one Run step
    that passes the dispatch input through the environment (never an inline
    expression) to the reviewed script. The four existing staging bindings are
    scoped to that step alone. No wrangler CLI or action: the rollback is the
    script's one unforced Cloudflare deployment POST, so a changed-secret
    target is refused instead of being auto-confirmed.
    """
    name = "staging-rollback-drill.yml"
    errors = []
    on = workflow.get("on") or {}
    if set(on) != {"workflow_dispatch"}:
        errors.append(f"{name}: must be dispatch-only (no push/pull_request/schedule)")
    dispatch = on.get("workflow_dispatch") or {}
    inputs = dispatch.get("inputs") or {}
    if set(dispatch) != {"inputs"} or set(inputs) != {"target_version"}:
        errors.append(f"{name}: workflow_dispatch must carry exactly the target_version input")
    else:
        field = inputs["target_version"] or {}
        if (field.get("type") != "string" or str(field.get("required")).lower() != "true"
                or "default" in field):
            errors.append(f"{name}: target_version must be a required string with no default (never latest)")
    if workflow.get("concurrency") != {"group": "deploy-staging", "cancel-in-progress": "false"}:
        errors.append(f"{name}: must share the deploy-staging group without cancelling")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"drill"}:
        errors.append(f"{name}: jobs must be exactly drill")
        return errors
    job = jobs["drill"]
    if job.get("environment") != "staging":
        errors.append(f"{name}:drill: must stay scoped to the staging environment")
    if job.get("if") != "github.ref == 'refs/heads/main'":
        errors.append(f"{name}:drill: must run only from main")
    if not runner_allowed("drill", job.get("runs-on")):
        errors.append(f"{name}:drill: must use the routed runner expression for job 'drill'")
    for key in ("env", "needs", "uses", "services", "container", "continue-on-error", "strategy"):
        if key in job:
            errors.append(f"{name}:drill: must not set {key} (bindings stay on the single Run step)")
    text = str(job).lower()
    for marker in ("wrangler", "force", "production", "toJSON(secrets".lower(), "secrets["):
        if marker in text.replace(" ", ""):
            errors.append(f"{name}:drill: must not contain {marker!r} (script-only, unforced, staging-only)")
    steps = job.get("steps") or []
    if len(steps) != 3 or any(key in step for step in steps for key in ("if", "continue-on-error")):
        errors.append(f"{name}:drill: must run exactly checkout, setup-node and the drill, unconditionally")
        return errors
    checkout, node, run = steps
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or (checkout.get("with") or {}).get("persist-credentials") != "false":
        errors.append(f"{name}:drill: checkout must be pinned and must not persist credentials")
    if not str(node.get("uses", "")).startswith("actions/setup-node@"):
        errors.append(f"{name}:drill: second step must be the pinned setup-node")
    command = str(run.get("run", ""))
    if run.get("env") != ROLLBACK_DRILL_ENV:
        errors.append(f"{name}:drill: Run step must bind exactly the four staging bindings plus TARGET_VERSION")
    if ("scripts/staging_rollback_drill.py" not in command or '--target-version "$TARGET_VERSION"' not in command
            or "${{" in command or "uses" in run):
        errors.append(f"{name}:drill: Run step must call the script with the input via the environment only")
    return errors


CONTAINER_DRILL_ENV = {
    "STAGING_WORKER_URL": "${{ vars.STAGING_WORKER_URL }}",
    "OWNERSHIP_CONTROL_TOKEN": "${{ secrets.STAGING_OWNERSHIP_CONTROL_TOKEN }}",
    "CLOUDFLARE_API_TOKEN": "${{ secrets.CLOUDFLARE_API_TOKEN }}",
    "CLOUDFLARE_ACCOUNT_ID": "${{ secrets.CLOUDFLARE_ACCOUNT_ID }}",
    "BACKOUT_SOURCE_SHA": "${{ inputs.backout_source_sha }}",
    "BACKOUT_BUILD_ID": "${{ inputs.backout_build_id }}",
    "BACKOUT_IMAGE": "${{ inputs.backout_image }}",
    "BACKOUT_WORKER_VERSION": "${{ inputs.backout_worker_version }}",
    "BACKOUT_ROLLOUT_ID": "${{ inputs.backout_rollout_id }}",
    "BACKOUT_REVIEW_REF": "${{ inputs.backout_review_ref }}",
    "BACKOUT_STAGING_RUN_ID": "${{ inputs.backout_staging_run_id }}",
    "COMPATIBILITY_NOTE": "${{ inputs.compatibility_note }}",
    "SESSION_ATTESTATION": "${{ inputs.session_attestation }}",
}
# Dispatch input names are the full binding names lowercased
# (BACKOUT_SOURCE_SHA <- inputs.backout_source_sha, and so on).
CONTAINER_DRILL_INPUTS = {key.lower() for key in CONTAINER_DRILL_ENV if key not in
                          ("STAGING_WORKER_URL", "OWNERSHIP_CONTROL_TOKEN",
                           "CLOUDFLARE_API_TOKEN", "CLOUDFLARE_ACCOUNT_ID")}


def staging_container_drill_errors(workflow):
    """Manual staging-only reviewed-image container backout/restore drill.

    Dispatch-only with seven required backout pins, one required compatibility
    note and one optional session attestation. One job runs from main through the `staging`
    environment and shares the deploy-staging concurrency group (never
    cancelling), so a push deploy cannot interleave. Exactly four steps:
    pinned checkout, pinned setup-node, `npm ci` in wrangler/ (the repo-pinned
    wrangler binary the reviewed script drives for the full-container
    deploy), and one Run step that passes every dispatch input through the
    environment (never an inline expression) to the reviewed script. All
    thirteen bindings are scoped to that step alone. No wrangler action, no
    inline deploy command, no force flag: the only deploy route is the
    script's pinned-binary full-container deploy of the validated image pin.
    """
    name = "staging-container-drill.yml"
    errors = []
    on = workflow.get("on") or {}
    if set(on) != {"workflow_dispatch"}:
        errors.append(f"{name}: must be dispatch-only (no push/pull_request/schedule)")
    dispatch = on.get("workflow_dispatch") or {}
    inputs = dispatch.get("inputs") or {}
    if set(dispatch) != {"inputs"} or set(inputs) != CONTAINER_DRILL_INPUTS:
        errors.append(f"{name}: workflow_dispatch must carry exactly the backout pins, note and attestation")
    else:
        for key, field in inputs.items():
            field = field or {}
            if key == "session_attestation":
                if (field.get("type") != "string" or str(field.get("required")).lower() != "false"
                        or field.get("default") != ""):
                    errors.append(f"{name}: session_attestation must be optional with an empty default")
            elif (field.get("type") != "string" or str(field.get("required")).lower() != "true"
                    or "default" in field):
                errors.append(f"{name}: {key} must be a required string with no default (never latest)")
    if workflow.get("concurrency") != {"group": "deploy-staging", "cancel-in-progress": "false"}:
        errors.append(f"{name}: must share the deploy-staging group without cancelling")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"container-drill"}:
        errors.append(f"{name}: jobs must be exactly container-drill")
        return errors
    job = jobs["container-drill"]
    if job.get("environment") != "staging":
        errors.append(f"{name}:container-drill: must stay scoped to the staging environment")
    if job.get("if") != "github.ref == 'refs/heads/main'":
        errors.append(f"{name}:container-drill: must run only from main")
    if not runner_allowed("container-drill", job.get("runs-on")):
        errors.append(f"{name}:container-drill: must use the routed runner expression for job 'container-drill'")
    for key in ("env", "needs", "uses", "services", "container", "continue-on-error", "strategy"):
        if key in job:
            errors.append(f"{name}:container-drill: must not set {key} (bindings stay on the single Run step)")
    steps = job.get("steps") or []
    if len(steps) != 4 or any(key in step for step in steps for key in ("if", "continue-on-error")):
        errors.append(f"{name}:container-drill: must run exactly checkout, setup-node, npm ci and the drill")
        return errors
    checkout, node, install, run = steps
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or (checkout.get("with") or {}).get("persist-credentials") != "false":
        errors.append(f"{name}:container-drill: checkout must be pinned and must not persist credentials")
    if not str(node.get("uses", "")).startswith("actions/setup-node@"):
        errors.append(f"{name}:container-drill: second step must be the pinned setup-node")
    if install.get("run") != "npm ci" or install.get("working-directory") != "wrangler" \
            or "uses" in install:
        errors.append(f"{name}:container-drill: third step must be the repo-pinned wrangler install only")
    command = str(run.get("run", ""))
    if run.get("env") != CONTAINER_DRILL_ENV:
        errors.append(f"{name}:container-drill: Run step must bind exactly the staging bindings plus the pins")
    expected_flags = ['--backout-source-sha "$BACKOUT_SOURCE_SHA"',
                      '--backout-build-id "$BACKOUT_BUILD_ID"',
                      '--backout-image "$BACKOUT_IMAGE"',
                      '--backout-worker-version "$BACKOUT_WORKER_VERSION"',
                      '--backout-rollout-id "$BACKOUT_ROLLOUT_ID"',
                      '--backout-review-ref "$BACKOUT_REVIEW_REF"',
                      '--backout-staging-run-id "$BACKOUT_STAGING_RUN_ID"',
                      '--compatibility-note "$COMPATIBILITY_NOTE"',
                      '--session-attestation "$SESSION_ATTESTATION"']
    if ("scripts/staging_container_drill.py" not in command
            or any(flag not in command for flag in expected_flags)
            or "${{" in command or "uses" in run):
        errors.append(f"{name}:container-drill: Run step must call the script with the inputs via the environment only")
    text = str(steps).lower()
    for marker in ("wrangler-action", "wrangler deploy", "wrangler publish", "force",
                   "production", "tojson(secrets", "secrets["):
        if marker in text.replace(" ", ""):
            errors.append(f"{name}:container-drill: must not contain {marker!r} (script-only, unforced, staging-only)")
    return errors


EVENTS_READ_ENV = {
    "TWO_BOT_STAGING_EVENTS_RO_DATABASE_URL": "${{ secrets.TWO_BOT_STAGING_EVENTS_RO_DATABASE_URL }}",
    "STAGING_EVENTS_READ_EXPECTED_HOST": "${{ secrets.STAGING_EVENTS_READ_EXPECTED_HOST }}",
    "STAGING_FIXTURE_MEMBER_ID": "${{ secrets.STAGING_FIXTURE_MEMBER_ID }}",
    "WINDOW_START": "${{ inputs.window_start }}",
    "WINDOW_END": "${{ inputs.window_end }}",
    "RUN_ID": "${{ github.run_id }}",
}
EVENTS_READ_COMMAND = ('python3 scripts/staging_events_read.py --window-start "$WINDOW_START" '
                       '--window-end "$WINDOW_END" --output "staging-events-read-$RUN_ID.json"')


def staging_events_read_errors(workflow):
    """Manual staging-only read-only events read for the B2 soak.

    Dispatch-only with exactly two required string inputs, the UTC window. The
    fixture member is never an input and never a variable (a step's env block
    prints variable values on the public run page). The read-only database
    login, the independently pinned staging host and the fixture member are
    environment secrets. One job runs only from main
    through the `staging-events-read` environment, on the routed runner for job
    `read`, with its own non-cancelling concurrency group and a 5 minute
    timeout. Exactly three unconditional steps: pinned checkout without
    persisted credentials, one Run step that passes the inputs through the
    environment (never an inline expression) to the reviewed script, and the
    14 day artifact upload of the sanitized file. All three environment
    secrets are scoped to that Run step alone. No wrangler, no
    production path, no `set -x`.
    """
    name = "staging-events-read.yml"
    errors = []
    if "env" in workflow:
        errors.append(f"{name}: workflow-level env must stay absent; secrets belong only on the Run step")
    on = workflow.get("on") or {}
    if set(on) != {"workflow_dispatch"}:
        errors.append(f"{name}: must be dispatch-only (no push/pull_request/schedule)")
    dispatch = on.get("workflow_dispatch") or {}
    inputs = dispatch.get("inputs") or {}
    if set(dispatch) != {"inputs"} or set(inputs) != {"window_start", "window_end"}:
        errors.append(f"{name}: workflow_dispatch must carry exactly window_start and window_end")
    else:
        for key, field in inputs.items():
            field = field or {}
            if (field.get("type") != "string" or str(field.get("required")).lower() != "true"
                    or "default" in field):
                errors.append(f"{name}: {key} must be a required string with no default")
    if workflow.get("permissions") != {}:
        errors.append(f"{name}: top-level permissions must stay empty")
    if workflow.get("concurrency") != {"group": "staging-events-read", "cancel-in-progress": "false"}:
        errors.append(f"{name}: must use its own staging-events-read group without cancelling")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"read"}:
        errors.append(f"{name}: jobs must be exactly read")
        return errors
    job = jobs["read"]
    if job.get("environment") != "staging-events-read":
        errors.append(f"{name}:read: must read the staging-events-read Environment bindings")
    if job.get("if") != "github.ref == 'refs/heads/main'":
        errors.append(f"{name}:read: must run only from main")
    if job.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:read: must keep contents:read only")
    if job.get("timeout-minutes") != "5":
        errors.append(f"{name}:read: timeout must stay 5 minutes")
    if not runner_allowed("read", job.get("runs-on")):
        errors.append(f"{name}:read: must use the routed runner expression for job 'read'")
    for key in ("env", "needs", "uses", "services", "container", "continue-on-error", "strategy"):
        if key in job:
            errors.append(f"{name}:read: must not set {key} (bindings stay on the single Run step)")
    text = str(job).lower().replace(" ", "")
    for marker in ("wrangler", "production", "tojson(secrets", "secrets[", "set-x", "setx", "xtrace",
                   "vars.staging_fixture_member_id", "inputs.member", "inputs.fixture"):
        if marker in text:
            errors.append(f"{name}:read: must not contain {marker!r}")
    secrets = set(re.findall(r"secrets\.([a-z0-9_]+)", text))
    if secrets != {"two_bot_staging_events_ro_database_url", "staging_events_read_expected_host",
                   "staging_fixture_member_id"}:
        errors.append(f"{name}:read: must read exactly the three events-read secrets")
    if set(re.findall(r"inputs\.([a-z0-9_]+)", text)) != {"window_start", "window_end"}:
        errors.append(f"{name}:read: may reference only the two window inputs")
    steps = job.get("steps") or []
    if len(steps) != 3 or any(key in step for step in steps for key in ("if", "continue-on-error")):
        errors.append(f"{name}:read: must run exactly checkout, the read and the upload, unconditionally")
        return errors
    checkout, run, upload = steps
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or (checkout.get("with") or {}).get("persist-credentials") != "false":
        errors.append(f"{name}:read: checkout must be pinned and must not persist credentials")
    if run.get("env") != EVENTS_READ_ENV:
        errors.append(f"{name}:read: Run step must bind exactly the three secrets, the two windows and the run id")
    if run.get("run") != EVENTS_READ_COMMAND or "uses" in run or "shell" in run:
        errors.append(f"{name}:read: Run step must call the script with the inputs via the environment only")
    if not str(upload.get("uses", "")).startswith("actions/upload-artifact@") or upload.get("with") != {
            "name": "staging-events-read-${{ github.run_id }}",
            "path": "staging-events-read-${{ github.run_id }}.json",
            "if-no-files-found": "error", "retention-days": "14"}:
        errors.append(f"{name}:read: must upload only the run's json for 14 days and fail if absent")
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
        if name == "staging-rollback-drill.yml":
            # Manual drill (TOG-14008): pinned shape above; the generic
            # environment/marker scan below would flag its staging binding.
            errors.extend(staging_rollback_drill_errors(workflow))
            continue
        if name == "staging-container-drill.yml":
            # Manual container drill: pinned shape above; the generic
            # environment/marker scan below would flag its staging binding
            # and the wrangler binary path the reviewed script drives.
            errors.extend(staging_container_drill_errors(workflow))
            continue
        if name == "staging-events-read.yml":
            # Manual read-only events read: pinned shape above; the generic
            # environment scan below would flag its Environment binding.
            errors.extend(staging_events_read_errors(workflow))
            continue
        if name == "staging-migrate.yml":
            # Manual runner: pinned shape above, not the
            # staging-deploy policy. The generic environment/marker scan
            # below would flag its staging-migrate Environment binding.
            errors.extend(staging_migrate_errors(workflow))
            continue
        if name == "production-migrate.yml":
            # Manual production runner: pinned shape above, not the
            # staging-deploy policy. The generic environment/marker scan
            # below would flag its production-migrate Environment binding.
            errors.extend(production_migrate_errors(workflow))
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

    def test_docs_only_push_skips_staging_but_runtime_paths_deploy(self):
        # A docs-only merge to main must start no run (the soak needs a stable
        # gateway), while every runtime path still deploys. Pinned against
        # sample paths with GitHub's filter semantics, not just the pattern text.
        push = self.workflows["deploy-staging.yml"]["on"]["push"]
        self.assertEqual(push["branches"], ["main"])
        self.assertNotIn("paths", push)
        for path in NON_RUNTIME_PATHS:
            with self.subTest(skipped=path):
                self.assertTrue(any(filter_pattern_matches(p, path) for p in push["paths-ignore"]))
        for path in RUNTIME_PATHS:
            with self.subTest(deploys=path):
                self.assertFalse(any(filter_pattern_matches(p, path) for p in push["paths-ignore"]))
        # Manual dispatch is unfiltered.
        self.assertEqual(set(self.workflows["deploy-staging.yml"]["on"]["workflow_dispatch"]), {"inputs"})

    def test_push_filter_cannot_widen_into_runtime_or_become_an_allow_list(self):
        ignore = self.workflows["deploy-staging.yml"]["on"]["push"]["paths-ignore"]
        mutations = {
            "allow-list": {"branches": ["main"], "paths": ["docs/**"]},
            "no filter list": {"branches": ["main"], "paths-ignore": []},
            "other branches": {"branches": ["main", "release/**"], "paths-ignore": ignore},
            "tags": {"branches": ["main"], "tags": ["v*"], "paths-ignore": ignore},
        }
        for widened in ("**", "*", "*.json", "**/*.md", "crates/**", "wrangler/**", "scripts/**",
                        ".github/**", ".github/workflows/**", "sql/**", "Cargo.*", "Dockerfile*",
                        "src/**", "deploy/**", "tests/**"):
            mutations[f"ignore {widened}"] = {"branches": ["main"], "paths-ignore": ignore + [widened]}
        for label, push in mutations.items():
            with self.subTest(label):
                workflows = deepcopy(self.workflows)
                workflows["deploy-staging.yml"]["on"]["push"] = push
                self.assertTrue(staging_dispatch_errors(workflows["deploy-staging.yml"]))
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
        # Credential split holds for the whole job mapping, not just step env:
        # a binding exported at job level (or smuggled through any other key,
        # in any case) reaches every step of the wrong job.
        migrator_secret = "${{ secrets.TWO_BOT_STAGING_MIGRATOR_DATABASE_URL }}"
        plan_secret = "${{ secrets.TWO_BOT_STAGING_PLAN_DATABASE_URL }}"
        for job_id, wrong_binding, wrong_secret in (
                ("plan", "TWO_BOT_STAGING_MIGRATOR_DATABASE_URL", migrator_secret),
                ("apply", "TWO_BOT_STAGING_PLAN_DATABASE_URL", plan_secret)):
            with self.subTest(job=job_id, wrong_binding="job-level-env"):
                def leak(w, job_id=job_id, wrong_binding=wrong_binding, wrong_secret=wrong_secret):
                    w["jobs"][job_id].setdefault("env", {})[wrong_binding] = wrong_secret
                self.assertTrue(mutated(leak))
            with self.subTest(job=job_id, wrong_binding="lowercase-job-level-env"):
                def leak(w, job_id=job_id, wrong_binding=wrong_binding, wrong_secret=wrong_secret):
                    w["jobs"][job_id].setdefault("env", {})["DB"] = wrong_secret.lower()
                self.assertTrue(mutated(leak))
            with self.subTest(job=job_id, wrong_binding="step-with"):
                def leak(w, job_id=job_id, wrong_secret=wrong_secret):
                    w["jobs"][job_id]["steps"].append(
                        {"uses": "actions/cache@pinned", "with": {"key": wrong_secret}})
                self.assertTrue(mutated(leak))
            with self.subTest(job=job_id, wrong_binding="step-run"):
                def leak(w, job_id=job_id, wrong_secret=wrong_secret):
                    w["jobs"][job_id]["steps"].append({"run": f"echo {wrong_secret}"})
                self.assertTrue(mutated(leak))
            for blanket in ("${{ toJSON(secrets) }}", "${{ secrets['TWO_BOT_STAGING_X'] }}"):
                with self.subTest(job=job_id, blanket=blanket):
                    def leak(w, job_id=job_id, blanket=blanket):
                        w["jobs"][job_id].setdefault("env", {})["ALL"] = blanket
                    self.assertTrue(mutated(leak))
        with self.subTest(plan="own-binding-at-job-level"):
            # Positive control: relocating the job's own binding to job `env`
            # is still that job reading only its own credential.
            def relocate(w):
                job = w["jobs"]["plan"]
                job["env"] = {"TWO_BOT_STAGING_PLAN_DATABASE_URL": plan_secret}
                for step in job["steps"]:
                    step.get("env", {}).pop("TWO_BOT_STAGING_PLAN_DATABASE_URL", None)
            self.assertEqual(mutated(relocate), [])
        with self.subTest(plan="no-own-binding"):
            def drop(w):
                job = w["jobs"]["plan"]
                for step in job["steps"]:
                    step.get("env", {}).pop("TWO_BOT_STAGING_PLAN_DATABASE_URL", None)
            self.assertTrue(mutated(drop))
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

    def test_production_migrate_runner_shape_is_pinned(self):
        # Production mirror of the staging runner: same three jobs, same pinned
        # actions, routed runner, pipefail and provenance gates, with
        # production names, environments, secrets and --target production.
        migrate = self.workflows["production-migrate.yml"]
        self.assertEqual(production_migrate_errors(migrate), [])
        self.assertEqual(staging_migrate_errors(self.workflows["staging-migrate.yml"]), [])
        self.assertEqual(workflow_policy_errors(self.workflows), [])

        def mutated(change):
            workflow = deepcopy(migrate)
            change(workflow)
            return production_migrate_errors(workflow)

        for trigger in ("push", "pull_request", "schedule", "workflow_call"):
            with self.subTest(trigger=trigger):
                self.assertTrue(mutated(lambda w, t=trigger: w["on"].update({t: ""})))
        for missing in ("acl_plan_ref", "plan_manifest_sha256", "plan_run_id",
                        "production_host", "production_database"):
            with self.subTest(missing=missing):
                def drop(w, missing=missing):
                    del w["on"]["workflow_dispatch"]["inputs"][missing]
                self.assertTrue(mutated(drop))
        # Staging inputs must not appear on the production workflow.
        for staging_input in ("staging_host", "staging_database"):
            with self.subTest(staging_input=staging_input):
                def add(w, staging_input=staging_input):
                    w["on"]["workflow_dispatch"]["inputs"][staging_input] = {"required": True}
                self.assertTrue(mutated(add))
        with self.subTest(mode="apply-default"):
            def widen(w):
                w["on"]["workflow_dispatch"]["inputs"]["mode"]["default"] = "apply"
            self.assertTrue(mutated(widen))
        with self.subTest(missing="apply-job"):
            def drop(w):
                del w["jobs"]["apply"]
            self.assertTrue(mutated(drop))
        for job_id in ("plan", "apply"):
            for key, value in (("environment", None),
                               ("environment", "staging-migrate-plan"),
                               ("environment", "staging-migrate-apply"),
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
        # Target isolation: the runner flag and host/database flags must be
        # production, never staging; dropping them or swapping them fails.
        with self.subTest(target="missing"):
            def drop_target(w):
                for job in w["jobs"].values():
                    for step in job.get("steps", []):
                        if "--target production" in str(step.get("run", "")):
                            step["run"] = step["run"].replace("--target production", "")
            self.assertTrue(mutated(drop_target))
        for wrong in ("--target staging", "--staging-host", "--staging-database"):
            with self.subTest(wrong=wrong):
                def widen(w, wrong=wrong):
                    for step in w["jobs"]["plan"]["steps"]:
                        if "tee" in str(step.get("run", "")):
                            step["run"] = step["run"].replace("--production-host", wrong) \
                                if "host" in wrong else step["run"].replace("--target production", wrong)
                self.assertTrue(mutated(widen))
        # Credential split holds for the whole job mapping, including
        # cross-target leaks: a production job never reads a staging binding.
        prod_migrator = "${{ secrets.TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL }}"
        prod_plan = "${{ secrets.TWO_BOT_PRODUCTION_PLAN_DATABASE_URL }}"
        staging_migrator = "${{ secrets.TWO_BOT_STAGING_MIGRATOR_DATABASE_URL }}"
        staging_plan = "${{ secrets.TWO_BOT_STAGING_PLAN_DATABASE_URL }}"
        for job_id, wrong_secret in (("plan", prod_migrator), ("apply", prod_plan),
                                     ("plan", staging_migrator), ("plan", staging_plan),
                                     ("apply", staging_migrator), ("apply", staging_plan)):
            with self.subTest(job=job_id, wrong_binding="leak"):
                def leak(w, job_id=job_id, wrong_secret=wrong_secret):
                    w["jobs"][job_id].setdefault("env", {})["LEAK"] = wrong_secret
                self.assertTrue(mutated(leak))
        with self.subTest(plan="no-own-binding"):
            def drop(w):
                job = w["jobs"]["plan"]
                for step in job["steps"]:
                    step.get("env", {}).pop("TWO_BOT_PRODUCTION_PLAN_DATABASE_URL", None)
            self.assertTrue(mutated(drop))
        with self.subTest(apply="no-needs"):
            def drop(w):
                del w["jobs"]["apply"]["needs"]
            self.assertTrue(mutated(drop))
        with self.subTest(environments="swapped"):
            def swap(w):
                w["jobs"]["plan"]["environment"] = "production-migrate-apply"
                w["jobs"]["apply"]["environment"] = "production-migrate-plan"
            self.assertTrue(mutated(swap))
        with self.subTest(apply="no-plan-hash-flag"):
            def drop_hash(w):
                for step in w["jobs"]["apply"]["steps"]:
                    if "--plan-manifest-sha256" in str(step.get("run", "")):
                        step["run"] = step["run"].replace("--plan-manifest-sha256 \"$PLAN_MANIFEST_SHA256\" ", "")
            self.assertTrue(mutated(drop_hash))
        with self.subTest(apply="no-provenance-fetch"):
            def drop_fetch(w):
                w["jobs"]["apply"]["steps"] = [
                    step for step in w["jobs"]["apply"]["steps"]
                    if "download-artifact" not in str(step.get("uses", ""))
                ]
            self.assertTrue(mutated(drop_fetch))
        with self.subTest(apply="no-provenance-path-flag"):
            def drop_path(w):
                for step in w["jobs"]["apply"]["steps"]:
                    if "--plan-manifest-path" in str(step.get("run", "")):
                        step["run"] = step["run"].replace(
                            " --plan-manifest-path producing-plan/production-migrate-manifest.json", "")
            self.assertTrue(mutated(drop_path))
        # Production and staging stay mirrors: same job ids, same step counts,
        # same pinned actions and container, differing only by the documented
        # production prefix (names, envs, secrets, artifacts, flags).
        staging = self.workflows["staging-migrate.yml"]
        self.assertEqual(set(migrate["jobs"]), set(staging["jobs"]))
        for job_id in ("plan", "claim", "apply"):
            with self.subTest(mirror=job_id):
                prod_job, stag_job = migrate["jobs"][job_id], staging["jobs"][job_id]
                self.assertEqual(len(prod_job.get("steps", [])), len(stag_job.get("steps", [])))
                self.assertEqual(prod_job.get("permissions"), stag_job.get("permissions"))
                self.assertEqual(prod_job.get("if"), stag_job.get("if"))
                self.assertEqual(prod_job.get("runs-on"), stag_job.get("runs-on").replace("staging", "production") if "staging" in str(stag_job.get("runs-on")) else stag_job.get("runs-on"))

    def test_staging_rollback_drill_shape_is_pinned(self):
        workflows = self.workflows
        self.assertEqual(staging_rollback_drill_errors(workflows["staging-rollback-drill.yml"]), [])

        def mutate(change):
            copy = deepcopy(workflows["staging-rollback-drill.yml"])
            change(copy)
            return staging_rollback_drill_errors(copy)

        def job(copy):
            return copy["jobs"]["drill"]

        changes = {
            "push trigger": lambda w: w["on"].update({"push": {"branches": ["main"]}}),
            "extra input": lambda w: w["on"]["workflow_dispatch"]["inputs"].update({"force": {"type": "boolean"}}),
            "target default": lambda w: w["on"]["workflow_dispatch"]["inputs"]["target_version"].update(
                {"default": "latest"}),
            "optional target": lambda w: w["on"]["workflow_dispatch"]["inputs"]["target_version"].update(
                {"required": "false"}),
            "cancelling concurrency": lambda w: w["concurrency"].update({"cancel-in-progress": "true"}),
            "own concurrency group": lambda w: w["concurrency"].update({"group": "staging-rollback-drill"}),
            "second job": lambda w: w["jobs"].update({"again": deepcopy(w["jobs"]["drill"])}),
            "production environment": lambda w: job(w).update({"environment": "production"}),
            "no environment": lambda w: job(w).pop("environment"),
            "branch condition removed": lambda w: job(w).pop("if"),
            "any branch": lambda w: job(w).update({"if": "github.ref != ''"}),
            "unrouted runner": lambda w: job(w).update({"runs-on": "ubuntu-latest"}),
            "job-level secret": lambda w: job(w).update({"env": {"T": "${{ secrets.CLOUDFLARE_API_TOKEN }}"}}),
            "wrangler action": lambda w: job(w)["steps"].insert(
                2, {"uses": "cloudflare/wrangler-action@pinned"}),
            "step condition": lambda w: job(w)["steps"][2].update({"if": "always()"}),
            "continue on error": lambda w: job(w)["steps"][2].update({"continue-on-error": "true"}),
            "persisted credentials": lambda w: job(w)["steps"][0]["with"].update({"persist-credentials": "true"}),
            "extra binding": lambda w: job(w)["steps"][2]["env"].update(
                {"X": "${{ secrets.TWO_BOT_STAGING_MIGRATOR_DATABASE_URL }}"}),
            "inline expression": lambda w: job(w)["steps"][2].update(
                {"run": 'python3 scripts/staging_rollback_drill.py --target-version "${{ inputs.target_version }}"'}),
            "forced rollback": lambda w: job(w)["steps"][2].update(
                {"run": 'python3 scripts/staging_rollback_drill.py --target-version "$TARGET_VERSION" --force'}),
            "other script": lambda w: job(w)["steps"][2].update(
                {"run": 'python3 scripts/other.py --target-version "$TARGET_VERSION"'}),
        }
        for label, change in changes.items():
            with self.subTest(change=label):
                self.assertTrue(mutate(change))
        # The inventory pin fails closed if the workflow disappears or gains a job.
        missing = deepcopy(workflows)
        del missing["staging-rollback-drill.yml"]
        self.assertTrue(workflow_policy_errors(missing))
        self.assertEqual(workflow_policy_errors(workflows), [])

    def test_staging_container_drill_shape_is_pinned(self):
        workflows = self.workflows
        self.assertEqual(staging_container_drill_errors(workflows["staging-container-drill.yml"]), [])

        def mutate(change):
            copy = deepcopy(workflows["staging-container-drill.yml"])
            change(copy)
            return staging_container_drill_errors(copy)

        def job(copy):
            return copy["jobs"]["container-drill"]

        changes = {
            "push trigger": lambda w: w["on"].update({"push": {"branches": ["main"]}}),
            "extra input": lambda w: w["on"]["workflow_dispatch"]["inputs"].update({"force": {"type": "boolean"}}),
            "pin default": lambda w: w["on"]["workflow_dispatch"]["inputs"]["backout_image"].update(
                {"default": "latest"}),
            "optional pin": lambda w: w["on"]["workflow_dispatch"]["inputs"]["backout_image"].update(
                {"required": "false"}),
            "cancelling concurrency": lambda w: w["concurrency"].update({"cancel-in-progress": "true"}),
            "own concurrency group": lambda w: w["concurrency"].update({"group": "staging-container-drill"}),
            "second job": lambda w: w["jobs"].update({"again": deepcopy(w["jobs"]["container-drill"])}),
            "production environment": lambda w: job(w).update({"environment": "production"}),
            "no environment": lambda w: job(w).pop("environment"),
            "branch condition removed": lambda w: job(w).pop("if"),
            "any branch": lambda w: job(w).update({"if": "github.ref != ''"}),
            "unrouted runner": lambda w: job(w).update({"runs-on": "ubuntu-latest"}),
            "job-level secret": lambda w: job(w).update({"env": {"T": "${{ secrets.CLOUDFLARE_API_TOKEN }}"}}),
            "wrangler action": lambda w: job(w)["steps"].insert(
                2, {"uses": "cloudflare/wrangler-action@pinned"}),
            "step condition": lambda w: job(w)["steps"][3].update({"if": "always()"}),
            "continue on error": lambda w: job(w)["steps"][3].update({"continue-on-error": "true"}),
            "persisted credentials": lambda w: job(w)["steps"][0]["with"].update({"persist-credentials": "true"}),
            "extra binding": lambda w: job(w)["steps"][3]["env"].update(
                {"X": "${{ secrets.TWO_BOT_STAGING_MIGRATOR_DATABASE_URL }}"}),
            "inline expression": lambda w: job(w)["steps"][3].update(
                {"run": 'python3 scripts/staging_container_drill.py --backout-image "${{ inputs.backout_image }}"'}),
            "other script": lambda w: job(w)["steps"][3].update(
                {"run": 'python3 scripts/other.py --backout-image "$BACKOUT_IMAGE"'}),
            "dropped npm ci": lambda w: job(w)["steps"].__delitem__(2),
        }
        for label, change in changes.items():
            with self.subTest(change=label):
                self.assertTrue(mutate(change))
        # The inventory pin fails closed if the workflow disappears or gains a job.
        missing = deepcopy(workflows)
        del missing["staging-container-drill.yml"]
        self.assertTrue(workflow_policy_errors(missing))
        self.assertEqual(workflow_policy_errors(workflows), [])

    def test_staging_events_read_shape_is_pinned(self):
        workflows = self.workflows
        self.assertEqual(staging_events_read_errors(workflows["staging-events-read.yml"]), [])

        def mutate(change):
            copy = deepcopy(workflows["staging-events-read.yml"])
            change(copy)
            return staging_events_read_errors(copy)

        def job(copy):
            return copy["jobs"]["read"]

        changes = {
            "push trigger": lambda w: w["on"].update({"push": {"branches": ["main"]}}),
            "schedule trigger": lambda w: w["on"].update({"schedule": [{"cron": "0 * * * *"}]}),
            "member input": lambda w: w["on"]["workflow_dispatch"]["inputs"].update(
                {"fixture_member": {"type": "string", "required": "true"}}),
            "guild input": lambda w: w["on"]["workflow_dispatch"]["inputs"].update(
                {"guild": {"type": "string", "required": "true"}}),
            "dropped end input": lambda w: w["on"]["workflow_dispatch"]["inputs"].pop("window_end"),
            "window default": lambda w: w["on"]["workflow_dispatch"]["inputs"]["window_start"].update(
                {"default": "2026-10-07T00:00:00Z"}),
            "optional window": lambda w: w["on"]["workflow_dispatch"]["inputs"]["window_end"].update(
                {"required": "false"}),
            "top-level permissions": lambda w: w.update({"permissions": {"contents": "read"}}),
            "workflow-level secret": lambda w: w.update(
                {"env": {"T": "${{ secrets.TWO_BOT_STAGING_EVENTS_RO_DATABASE_URL }}"}}),
            "cancelling concurrency": lambda w: w["concurrency"].update({"cancel-in-progress": "true"}),
            "shared concurrency group": lambda w: w["concurrency"].update({"group": "deploy-staging"}),
            "second job": lambda w: w["jobs"].update({"again": deepcopy(job(w))}),
            "wrong environment": lambda w: job(w).update({"environment": "staging"}),
            "production environment": lambda w: job(w).update({"environment": "production"}),
            "no environment": lambda w: job(w).pop("environment"),
            "branch condition removed": lambda w: job(w).pop("if"),
            "any branch": lambda w: job(w).update({"if": "github.ref != ''"}),
            "write grant": lambda w: job(w).update({"permissions": {"contents": "write"}}),
            "longer timeout": lambda w: job(w).update({"timeout-minutes": "30"}),
            "unrouted runner": lambda w: job(w).update({"runs-on": "ubuntu-latest"}),
            "other job's routing": lambda w: job(w).update(
                {"runs-on": ROUTED_RUNNER.format(job="check")}),
            "job-level secret": lambda w: job(w).update(
                {"env": {"T": "${{ secrets.TWO_BOT_STAGING_EVENTS_RO_DATABASE_URL }}"}}),
            "container": lambda w: job(w).update({"container": {"image": "postgres"}}),
            "step condition": lambda w: job(w)["steps"][1].update({"if": "always()"}),
            "continue on error": lambda w: job(w)["steps"][1].update({"continue-on-error": "true"}),
            "extra step": lambda w: job(w)["steps"].append({"run": "echo done"}),
            "persisted credentials": lambda w: job(w)["steps"][0]["with"].update(
                {"persist-credentials": "true"}),
            "extra secret": lambda w: job(w)["steps"][1]["env"].update(
                {"X": "${{ secrets.TWO_BOT_STAGING_MIGRATOR_DATABASE_URL }}"}),
            "dropped login secret": lambda w: job(w)["steps"][1]["env"].pop(
                "TWO_BOT_STAGING_EVENTS_RO_DATABASE_URL"),
            "dropped host pin": lambda w: job(w)["steps"][1]["env"].pop(
                "STAGING_EVENTS_READ_EXPECTED_HOST"),
            "blanket secrets": lambda w: job(w)["steps"][1]["env"].update({"ALL": "${{ toJSON(secrets) }}"}),
            "member as variable": lambda w: job(w)["steps"][1]["env"].update(
                {"STAGING_FIXTURE_MEMBER_ID": "${{ vars.STAGING_FIXTURE_MEMBER_ID }}"}),
            "member as input": lambda w: job(w)["steps"][1]["env"].update(
                {"STAGING_FIXTURE_MEMBER_ID": "${{ inputs.member }}"}),
            "inline expression": lambda w: job(w)["steps"][1].update(
                {"run": 'python3 scripts/staging_events_read.py --window-start "${{ inputs.window_start }}" '
                        '--window-end "$WINDOW_END" --output "staging-events-read-$RUN_ID.json"'}),
            "shell trace": lambda w: job(w)["steps"][1].update({"run": "set -x\n" + EVENTS_READ_COMMAND}),
            "other script": lambda w: job(w)["steps"][1].update(
                {"run": EVENTS_READ_COMMAND.replace("staging_events_read", "other")}),
            "wrangler action": lambda w: job(w)["steps"].insert(
                1, {"uses": "cloudflare/wrangler-action@pinned"}),
            "upload other file": lambda w: job(w)["steps"][2]["with"].update({"path": "*.json"}),
            "upload without retention": lambda w: job(w)["steps"][2]["with"].pop("retention-days"),
            "upload longer retention": lambda w: job(w)["steps"][2]["with"].update({"retention-days": "90"}),
            "upload may be empty": lambda w: job(w)["steps"][2]["with"].update({"if-no-files-found": "warn"}),
        }
        for label, change in changes.items():
            with self.subTest(change=label):
                self.assertTrue(mutate(change))
        # The inventory pin fails closed if the workflow disappears or gains a job.
        missing = deepcopy(workflows)
        del missing["staging-events-read.yml"]
        self.assertTrue(workflow_policy_errors(missing))
        self.assertEqual(workflow_policy_errors(workflows), [])

    def test_check_toolchains_match_the_repository_pin(self):
        channel = tomllib.loads((ROOT / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
        self.assertRegex(channel, r"^\d+\.\d+\.\d+$")
        installers = {
            job_id: [step for step in job.get("steps", [])
                     if step.get("uses", "").startswith("dtolnay/rust-toolchain@")]
            for job_id, job in self.workflows["check.yml"]["jobs"].items()
        }
        installers = {job: steps for job, steps in installers.items() if steps}
        self.assertEqual(set(installers), {
            "check", "rust-tests", "ignored-db-stores", "ignored-db-runtime",
            "moderation-db", "self-role-store", "community-db", "tickets-postgres", "feeds-db",
        })
        for job_id, steps in installers.items():
            with self.subTest(job=job_id):
                self.assertEqual(len(steps), 1)
                # Floating stable can install a different fmt/clippy than Cargo
                # selects from the repository pin inside the job container.
                self.assertEqual(steps[0].get("with", {}).get("toolchain"), channel)
        self.assertEqual(set(installers["check"][0]["with"]["components"].replace(" ", "").split(",")),
                         {"rustfmt", "clippy"})

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
                    elif (name, job_id) in (("staging-migrate.yml", "apply"),
                                                 ("production-migrate.yml", "apply")):
                        # Read-only fetch of the producing plan
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
