"""Offline workflow policy fixtures; never dispatch, deploy or probe staging."""

from copy import deepcopy
import importlib.util
from pathlib import Path
import re
import tomllib
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
_HISTORY_SPEC = importlib.util.spec_from_file_location(
    "synthetic_alert_history", ROOT / "scripts/synthetic_alert_history.py")
_HISTORY_MODULE = importlib.util.module_from_spec(_HISTORY_SPEC)
_HISTORY_SPEC.loader.exec_module(_HISTORY_MODULE)
ALERT_STEPS = _HISTORY_MODULE.ALERT_STEPS
DECIDE_STEPS = _HISTORY_MODULE.DECIDE_STEPS
JOB_NAMES = _HISTORY_MODULE.JOB_NAMES
PROBE_STEPS = _HISTORY_MODULE.PROBE_STEPS
STATIC_FALSE = "${{ false }}"
JOB_INVENTORY = {
    # The nextest branch adds its required lane; main adds the production schema gate.
    # The inventory pin must retain both changes.
    "check.yml": {"check", "moderation-db", "parity-docs", "self-role-store", "job-inputs", "container-inputs", "container",
                  "community-db", "feeds-db", "tickets-postgres", "worker", "supply-chain", "ci-ok",
                  # `check` is now the lint lane; the test steps it used to
                  # carry run in these three parallel lanes, all gated by ci-ok.
                  "rust-tests", "ignored-db-stores", "ignored-db-runtime",
                  # Compile-only libFuzzer build, gated by ci-ok like the Rust lanes.
                  "fuzz-compile",
                  # Promoted nextest lane (shared reusable), gated by ci-ok
                  # like the Rust lanes.
                  "nextest"},
    "deploy-production.yml": {"guard", "schema-gate", "roles-gate", "production", "release"},
    "deploy-staging.yml": {"deploy", "schema-gate", "roles-gate", "voice-synthetic"},
    "nightly.yml": {"changes", "pipeline-benchmark", "advisories", "sweep"},
    "pipeline-benchmark.yml": {"benchmark"},
    "release.yml": {"tag", "sbom-target", "release-sbom", "attach-sbom"},
    "staging-migrate.yml": {"plan", "claim", "apply"},
    "production-migrate.yml": {"plan", "claim", "apply"},
    # TOG-14008: manual staging-only Worker rollback drill; pinned shape below.
    "staging-rollback-drill.yml": {"drill"},
    # Container-image backout/restore drill: manual, staging-only, pinned shape below.
    "staging-container-drill.yml": {"container-drill"},
    # Manual read-only staging events read for the B2 soak; pinned shape below.
    "staging-events-read.yml": {"read"},
    # Manual read-only authenticated MCP prove-out (TOG-23456); pinned shape below.
    "staging-mcp-proveout.yml": {"proveout"},
    # Manual read-only staging ownership status read; pinned shape below.
    "staging-ownership-status.yml": {"status"},
    # Manual gradual-promote proof caller (passive until the synthetic
    # fixture lands); pinned shape below.
    "gradual-promote-proof.yml": {"proof"},
    "supply-chain.yml": {"pr-lint", "gitleaks"},
    # TOG-10893: read-only SBOM inventory/gates shared by the PR dry-run and releases.
    # `image` builds/scans the untrusted ref with pinned actions only; `verify`
    # runs the local validation/evidence/preflight scripts without ever
    # checking out inputs.ref (CodeQL cache-poisoning gate).
    "sbom.yml": {"image", "verify"},
    # Live staging voice synthetic (docs/voice-synthetic.md); pinned shape below.
    "voice-synthetic.yml": {"synthetic"},
    # Five-minute user-flow synthetic (docs/synthetic-user-flow.md): staging
    # health-contract plus passive production probes; pinned shape below.
    "synthetic-user-flow.yml": {"staging", "prod-passive"},
    # SDLC separation gate (org test-guard reusable, warn mode): one
    # reusable call, least-privilege grant pinned below.
    "test-guard.yml": {"test-guard"},
    # Advisory changed-file review rules; no deploy route or new required gate.
    "review-rules.yml": {"review-rules"},
    # TOG-21027: manual, bounded vote_kick libFuzzer smoke on a GitHub-hosted
    # runner. Read-only, no secrets, no deploy route: the generic
    # deployment/probe scan already passes, so only the inventory pin is needed.
    "vote-kick-fuzz-smoke.yml": {"vote-kick-smoke"},
    # Manual, bounded Helix token parser fuzz smoke; read-only and secret-free.
    "helix-token-fuzz-smoke.yml": {"helix-token-smoke"},
    # SDLC 2.3 CI speed evidence (the shared nextest lane graduated to a
    # required check.yml job): dorny/paths-filter affected-only selection
    # and the minutes-per-merged-PR report.
    "ci-speed.yml": {"changes", "report"},
    # Shared nextest lane, called by check.yml; org-level home is
    # TogetherWeOwn/.github with a pinned SHA like test-guard.
    "reusable-rust-nextest.yml": {"nextest"},
    # Advisory PR-size labels (org reusable, advisory only, never required):
    # thin caller to the central labeller + weekly tally.
    "pr-size.yml": {"pr-size", "pr-size-weekly"},
}
# Reusable-workflow calls are allowed only to these non-deploy workflows.
# TOG-10893 registers the read-only sbom.yml calls alongside the benchmark one.
REUSABLE_CALLS = {("nightly.yml", "pipeline-benchmark"): "./.github/workflows/pipeline-benchmark.yml",
                   ("check.yml", "supply-chain"): "./.github/workflows/sbom.yml",
                   ("release.yml", "release-sbom"): "./.github/workflows/sbom.yml",
                   # After every staging deploy; its failure fails deploy-staging (promotion gate).
                   ("deploy-staging.yml", "voice-synthetic"): "./.github/workflows/voice-synthetic.yml",
                   # SDLC separation gate: org reusable, warn mode, pinned SHA.
                   ("test-guard.yml", "test-guard"): "TogetherWeOwn/.github/.github/workflows/test-guard.yml@3fffeb9f9465f371763fcc3c789ed4baefe38f95",
                   # Read-only shadow engine, pinned to the published central merge commit.
                   ("review-rules.yml", "review-rules"): "TogetherWeOwn/.github/.github/workflows/review-rules.yml@63699075b492914c137281acffd4c89e51cc1c4c",
                   # SDLC 2.3 CI speed pilot, promoted: required check.yml
                   # caller to the shared nextest lane (local path until
                   # the central move).
                   ("check.yml", "nextest"): "./.github/workflows/reusable-rust-nextest.yml",
                   # Advisory PR-size labels: thin caller to the central
                   # reusable, pinned SHA (repin both refs together).
                   ("pr-size.yml", "pr-size"): "TogetherWeOwn/.github/.github/workflows/pr-size.yml@82c5b367c6e3e8032666029dc22d206fff39c3f2",
                   ("pr-size.yml", "pr-size-weekly"): "TogetherWeOwn/.github/.github/workflows/pr-size-weekly.yml@82c5b367c6e3e8032666029dc22d206fff39c3f2"}
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
    "migrations.lock", "Dockerfile", ".dockerignore",
    "wrangler/wrangler.toml", "wrangler/src/index.ts", "wrangler/package.json",
    "wrangler/package-lock.json", "wrangler/scripts/ownership-control.mjs",
    "wrangler/README.md", "scripts/staging_rollout.py", "scripts/container-smoke.py",
    ".github/workflows/deploy-staging.yml", ".github/workflows/check.yml",
    ".github/scripts/anything.py", "deploy/two-bot.service", "tests/voice_templates/corpus.json",
    "release-please-config.json", ".release-please-manifest.json", "fuzz/Cargo.toml",
    "fuzz/Cargo.lock",
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
    """The active staging workflow keeps exactly five fenced dispatch inputs.

    `release_fence` exists on main (TOG-11143) so post-handoff release of an
    uninitialized/parked singleton is explicit and auditable; it defaults to
    false and the deploy job itself runs unconditionally (TOG-12856).
    `inject_gateway_failure` is the staging rollback-drill switch: also an
    opt-in boolean defaulting to false, readable only through the pinned
    dispatch-only expression in staging_gateway_injection_errors.
    `inject_error_rate_spike` is the error-rate rollback-drill switch with
    the same shape, readable only through the pinned dispatch-only
    expression in staging_error_rate_gate_errors. `inject_voice_stall` and
    `inject_unpaired_disconnect` are the remediation-drill switches with the
    same shape, readable only through the pinned dispatch-only expressions
    in staging_fault_injection_errors. Anything else in the
    dispatch block (an arming switch, a deploy flag, a default of
    true) is an unreviewed deploy route outside the pinned shape.
    """
    name = "deploy-staging.yml"
    errors = []
    on = workflow.get("on") or {}
    if set(on) != {"push", "workflow_dispatch"}:
        errors.append(f"{name}: triggers changed")
    errors.extend(staging_push_filter_errors(name, on.get("push")))
    dispatch = on.get("workflow_dispatch") or {}
    expected = {"release_fence", "inject_gateway_failure", "inject_error_rate_spike", "inject_voice_stall", "inject_unpaired_disconnect"}
    if set(dispatch) != {"inputs"} or set(dispatch.get("inputs") or {}) != expected:
        errors.append(f"{name}: workflow_dispatch must carry only the release_fence, inject_gateway_failure, inject_error_rate_spike, inject_voice_stall and inject_unpaired_disconnect inputs")
    else:
        for input_name in expected:
            spec = dispatch["inputs"][input_name] or {}
            if spec.get("type") != "boolean" or spec.get("default") not in (False, "false"):
                errors.append(f"{name}: {input_name} must be an opt-in boolean defaulting to false")
    return errors


INJECTION_EXPRESSION = "${{ github.event_name == 'workflow_dispatch' && inputs.inject_gateway_failure && 'true' || 'false' }}"
INJECTION_MARKERS = (
    "inject_gateway_failure", "INJECT_GATEWAY_FAILURE",
    "TWO_GATEWAY_READINESS_FAIL_INJECTION", "--inject-gateway-failure",
)
FAULT_EXPRESSIONS = {
    "INJECT_VOICE_STALL": "${{ github.event_name == 'workflow_dispatch' && inputs.inject_voice_stall && 'true' || 'false' }}",
    "INJECT_UNPAIRED_DISCONNECT": "${{ github.event_name == 'workflow_dispatch' && inputs.inject_unpaired_disconnect && 'true' || 'false' }}",
}
FAULT_MARKERS = (
    "inject_voice_stall", "INJECT_VOICE_STALL",
    "TWO_VOICE_STALL_FAULT_INJECTION", "--inject-voice-stall",
    "inject_unpaired_disconnect", "INJECT_UNPAIRED_DISCONNECT",
    "TWO_GATEWAY_UNPAIRED_FAULT_INJECTION", "--inject-unpaired-disconnect",
)


def staging_gateway_injection_errors(workflows):
    """The drill injection is dispatch-only, staging-only plumbing.

    Exactly one expression in exactly one staging step may read the input,
    it must also require `workflow_dispatch` (a push never carries inputs,
    and the guard makes that structural rather than implied), and the step
    may only hand the prepare flag to the generated staging deploy config.
    Every other workflow — deploy-production above all — must not mention
    the switch at all, so no route can forward it to production.
    """
    errors = []
    for name, workflow in workflows.items():
        if name == "deploy-staging.yml":
            continue
        text = str(workflow)
        for marker in INJECTION_MARKERS:
            if marker in text:
                errors.append(f"{name}: gateway-failure injection marker {marker} outside deploy-staging")
    staging = workflows.get("deploy-staging.yml") or {}
    text = str(staging)
    if text.count("inputs.inject_gateway_failure") != 1:
        errors.append("deploy-staging.yml: inject_gateway_failure must be read exactly once")
    steps = ((staging.get("jobs") or {}).get("deploy") or {}).get("steps") or []
    armed = [step for step in steps
             if isinstance(step, dict) and "INJECT_GATEWAY_FAILURE" in str(step)]
    if len(armed) != 1:
        errors.append("deploy-staging.yml: exactly one deploy step may carry the injection switch")
        return errors
    step = armed[0]
    env = step.get("env") or {}
    if env.get("INJECT_GATEWAY_FAILURE") != INJECTION_EXPRESSION:
        errors.append("deploy-staging.yml: injection must use the pinned dispatch-only expression")
    run = str(step.get("run") or "")
    for needle in ("staging_rollout.py prepare", "--inject-gateway-failure",
                   '[ "$INJECT_GATEWAY_FAILURE" = "true" ]'):
        if run.count(needle) != 1:
            errors.append(f"deploy-staging.yml: injection step must contain exactly one {needle}")
    return errors


ERROR_RATE_EXPRESSION = "${{ github.event_name == 'workflow_dispatch' && inputs.inject_error_rate_spike && 'true' || 'false' }}"
ERROR_RATE_MARKERS = (
    "inject_error_rate_spike", "INJECT_ERROR_RATE_SPIKE",
    "promotion-error-gate.mjs", "error-rate-gate.json",
)


def staging_error_rate_gate_errors(workflows):
    """The error-rate promotion gate is dispatch-gated rehearsal plus live plumbing.

    Exactly one expression in exactly one staging step may read the rehearsal
    input, it must also require `workflow_dispatch` (a push never carries
    inputs), and that step must be the error-rate gate step: it judges the
    rehearsal firing when armed and scrapes live staging /ops/metrics
    otherwise, failing the stage so auto-rollback runs. The scrape token is
    optional (absent means a recorded skip, pinned in the gate script's
    tests); the rehearsal path needs no secret. Every other workflow —
    deploy-production above all — must not mention the switch at all, so no
    route can forward it to production.
    """
    errors = []
    for name, workflow in workflows.items():
        if name == "deploy-staging.yml":
            continue
        text = str(workflow)
        for marker in ERROR_RATE_MARKERS:
            if marker in text:
                errors.append(f"{name}: error-rate gate marker {marker} outside deploy-staging")
    staging = workflows.get("deploy-staging.yml") or {}
    text = str(staging)
    if text.count("inputs.inject_error_rate_spike") != 1:
        errors.append("deploy-staging.yml: inject_error_rate_spike must be read exactly once")
    steps = ((staging.get("jobs") or {}).get("deploy") or {}).get("steps") or []
    armed = [step for step in steps
             if isinstance(step, dict) and "INJECT_ERROR_RATE_SPIKE" in str(step)]
    if len(armed) != 1:
        errors.append("deploy-staging.yml: exactly one deploy step may carry the error-rate drill switch")
        return errors
    step = armed[0]
    if step.get("id") != "error_rate_gate":
        errors.append("deploy-staging.yml: the error-rate drill switch must live on the error_rate_gate step")
    env = step.get("env") or {}
    if env.get("INJECT_ERROR_RATE_SPIKE") != ERROR_RATE_EXPRESSION:
        errors.append("deploy-staging.yml: error-rate rehearsal must use the pinned dispatch-only expression")
    if "METRICS_SCRAPE_TOKEN" not in env:
        errors.append("deploy-staging.yml: error_rate_gate must bind the optional scrape token by name")
    run = str(step.get("run") or "")
    for needle in ("scripts/promotion-error-gate.mjs", "--metrics-url", "--evidence",
                   "--rehearsal-firing",
                   '[ "$INJECT_ERROR_RATE_SPIKE" = "true" ]'):
        if run.count(needle) != 1:
            errors.append(f"deploy-staging.yml: error_rate_gate step must contain exactly one {needle}")
    # The evidence filename appears exactly twice: the --evidence write path
    # and the read-back print, which must name the same file.
    if run.count("error-rate-gate.json") != 2:
        errors.append("deploy-staging.yml: error_rate_gate must write and read back the same error-rate-gate.json")
    return errors


def staging_fault_injection_errors(workflows):
    """The remediation-drill switches are dispatch-only, staging-only plumbing.

    Same shape as the gateway-failure injection above: each switch is read by
    exactly one pinned dispatch-only expression in exactly one staging step,
    the step hands only the prepare flags to the generated staging deploy
    config, and every other workflow — deploy-production above all — must not
    mention any fault marker at all, so no route can forward a drill to
    production.
    """
    errors = []
    for name, workflow in workflows.items():
        if name == "deploy-staging.yml":
            continue
        text = str(workflow)
        for marker in FAULT_MARKERS:
            if marker in text:
                errors.append(f"{name}: remediation-fault marker {marker} outside deploy-staging")
    staging = workflows.get("deploy-staging.yml") or {}
    text = str(staging)
    for input_name in ("inject_voice_stall", "inject_unpaired_disconnect"):
        if text.count(f"inputs.{input_name}") != 1:
            errors.append(f"deploy-staging.yml: {input_name} must be read exactly once")
    steps = ((staging.get("jobs") or {}).get("deploy") or {}).get("steps") or []
    armed = [step for step in steps
             if isinstance(step, dict) and ("INJECT_VOICE_STALL" in str(step)
                                            or "INJECT_UNPAIRED_DISCONNECT" in str(step))]
    if len(armed) != 1:
        errors.append("deploy-staging.yml: exactly one deploy step may carry the fault switches")
        return errors
    step = armed[0]
    env = step.get("env") or {}
    for var, expression in FAULT_EXPRESSIONS.items():
        if env.get(var) != expression:
            errors.append(f"deploy-staging.yml: {var} must use the pinned dispatch-only expression")
    run = str(step.get("run") or "")
    for needle in ("--inject-voice-stall", "--inject-unpaired-disconnect",
                   '[ "$INJECT_VOICE_STALL" = "true" ]',
                   '[ "$INJECT_UNPAIRED_DISCONNECT" = "true" ]'):
        if run.count(needle) != 1:
            errors.append(f"deploy-staging.yml: fault step must contain exactly one {needle}")
    return errors


def staging_claim_errors(claim, name="staging-migrate.yml",
                           job_name="staging-migrate (claim)",
                           manifest="staging-migrate-manifest",
                           claim_artifact="staging-migrate-apply-claim",
                           script="scripts/ci/staging_migrate_claim.py",
                           host_env="STAGING_HOST", host_input="staging_host",
                           db_env="STAGING_DATABASE", db_input="staging_database",
                           branch_env="STAGING_BRANCH_ID", branch_input="staging_branch_id"):
    """Claim transport must complete before the protected apply job waits.

    Parametrized by workflow name, job identity, artifact names, publisher
    script and host/database/branch env/input names so the production mirror
    pins the same shape with its own names.
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
    pairs = [("MODE", "mode"), ("SOURCE_SHA", "source_sha"), (host_env, host_input),
             (db_env, db_input)]
    if branch_env is not None and branch_input is not None:
        pairs.append((branch_env, branch_input))
    pairs.extend([("RECOVERY_REF", "recovery_evidence_ref"),
                  ("ACL_REF", "acl_plan_ref"), ("EXPECTED_PENDING", "expected_pending"),
                  ("EXPECTED_STORE_PENDING", "expected_store_pending"),
                  ("PLAN_MANIFEST_SHA256", "plan_manifest_sha256"),
                  ("STORE_PLAN_MANIFEST_SHA256", "store_plan_manifest_sha256"),
                  ("PLAN_RUN_ID", "plan_run_id")])
    expected_env = {key: "${{ inputs." + value + " }}" for key, value in pairs}
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
        branch_env="PRODUCTION_BRANCH_ID",
        branch_input="production_branch_id",
    )


def job_env_text(job):
    """Job-level `env` plus every step's `env`: everywhere a binding can be exported."""
    return " ".join([str(job.get("env", ""))]
                    + [str(step.get("env", "")) for step in job.get("steps", [])])


def migrate_errors(workflow, *, name, plan_env, apply_env, plan_secret,
                     migrator_secret, host_input, db_input, manifest,
                     target_flag, host_flag, db_flag, other_host_flag,
                     other_db_flag, other_plan_secret, other_migrator_secret,
                     branch_input="staging_branch_id", branch_flag="--staging-branch-id",
                     branch_label="staging", other_branch_flag=None):
    """Parametrized migration-runner shape (staging and production mirrors).

    Dispatch-only with exactly the reviewed inputs (each target carries its
    own PlanetScale branch pin), three jobs
    (plan always runs through the no-reviewer plan environment, unprotected
    claim transports the apply request, apply runs only for mode=apply after a
    green plan and claim through the reviewed apply environment), each job with
    its own routed runner and pipefail Run step. No push/pull_request/schedule
    trigger, no wrangler/probe markers. The caller supplies the workflow file
    name, environment names, secret names, host/database/branch input names,
    artifact prefix, runner target/host/database/branch flags and the other
    target's flags and secrets (which must never appear). A None branch pin
    skips the branch assertions for that target.
    """
    errors = []
    on = workflow.get("on") or {}
    if set(on) != {"workflow_dispatch"}:
        errors.append(f"{name}: must be dispatch-only (no push/pull_request/schedule)")
    inputs = ((on.get("workflow_dispatch") or {}).get("inputs") or {})
    optional = {"mode", "expected_pending", "expected_store_pending",
                "plan_manifest_sha256", "store_plan_manifest_sha256", "plan_run_id"}
    expected = {"mode", "source_sha", host_input, db_input,
                "recovery_evidence_ref", "acl_plan_ref", "expected_pending",
                "expected_store_pending", "plan_manifest_sha256",
                "store_plan_manifest_sha256", "plan_run_id"}
    if branch_input is not None:
        expected.add(branch_input)
        optional.add(branch_input)
    if set(inputs) != expected:
        errors.append(f"{name}: workflow_dispatch inputs must be exactly {sorted(expected)}")
    else:
        mode = inputs.get("mode") or {}
        if (mode.get("type") != "choice"
                or set(mode.get("options") or []) != {"plan", "apply"}
                or mode.get("default") != "plan"):
            errors.append(f"{name}: mode must be plan/apply defaulting to plan")
        for key in expected - optional:
            field = inputs.get(key) or {}
            if str(field.get("required")).lower() != "true":
                errors.append(f"{name}: input {key} must be required")
        if branch_input is not None:
            branch = inputs.get(branch_input) or {}
            if (str(branch.get("required")).lower() != "false"
                    or branch.get("default") != ""
                    or "branch" not in str(branch.get("description")).lower()):
                errors.append(f"{name}: {branch_input} must stay optional, default empty, "
                              "and documented as the PlanetScale branch pin")
        pending = inputs.get("expected_pending") or {}
        if (str(pending.get("required")).lower() != "false"
                or pending.get("default") != ""
                or "ascending" not in str(pending.get("description")).lower()):
            errors.append(f"{name}: expected_pending must stay optional, default empty, "
                          "and documented as the ascending reviewed plan list")
        store_pending = inputs.get("expected_store_pending") or {}
        if (str(store_pending.get("required")).lower() != "false"
                or store_pending.get("default") != ""
                or "ascending" not in str(store_pending.get("description")).lower()):
            errors.append(f"{name}: expected_store_pending must stay optional, default empty, "
                          "and documented as the ascending reviewed store plan list")
        for key in ("plan_manifest_sha256", "store_plan_manifest_sha256", "plan_run_id"):
            field = inputs.get(key) or {}
            if str(field.get("required")).lower() != "false" or field.get("default") != "":
                errors.append(f"{name}: {key} must stay optional and default empty "
                              "(the runner requires it for apply, not the dispatch)")
        digest = inputs.get("plan_manifest_sha256") or {}
        if "sha" not in str(digest.get("description")).lower():
            errors.append(f"{name}: plan_manifest_sha256 must document the manifest-hash contract "
                          "(apply refuses unless it matches on the same source_sha)")
        store_digest = inputs.get("store_plan_manifest_sha256") or {}
        if "sha" not in str(store_digest.get("description")).lower():
            errors.append(f"{name}: store_plan_manifest_sha256 must document the store "
                          "manifest-hash contract (apply refuses unless it matches)")
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
    if other_branch_flag is not None and (other_branch_flag in plan_runs
                                          or other_branch_flag in apply_runs):
        errors.append(f"{name}: must never pass the other target's {other_branch_flag} flag")
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
    if "--store-plan-manifest-sha256" not in apply_runs or "--expected-store-pending" not in apply_runs:
        errors.append(f"{name}:apply: must pass the store plan-bound manifest hash and pending list to the runner")
    if "--plan-manifest-sha256" in plan_runs or "--plan-run-id" in plan_runs:
        errors.append(f"{name}:plan: must not take plan-bound inputs (it produces the manifest)")
    if "--store-plan-manifest-sha256" in plan_runs:
        errors.append(f"{name}:plan: must not take the store plan-bound hash (it produces the manifest)")
    if "--plan-manifest-path" not in apply_runs:
        errors.append(f"{name}:apply: must pass the producing run's downloaded manifest to the runner")
    if "--plan-manifest-path" in plan_runs:
        errors.append(f"{name}:plan: must not take the provenance manifest path (it produces the manifest)")
    # PlanetScale branch pin: a target that declares the pin passes the
    # non-secret branch id to the runner on both jobs; the runner requires it
    # for `*.psdb.cloud` hosts and refuses pooled ports, `|bouncer` usernames
    # and branch mismatches before any DDL.
    if branch_flag is not None:
        if branch_flag not in plan_runs:
            errors.append(f"{name}:plan: must pass the {branch_label} branch pin to the runner")
        if branch_flag not in apply_runs:
            errors.append(f"{name}:apply: must pass the {branch_label} branch pin to the runner")
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
                          other_migrator_secret=other_migrator_secret,
                          other_branch_flag="--production-branch-id")


def production_migrate_errors(workflow):
    """Production mirror of the migration-runner shape.

    Production carries its own PlanetScale branch pin
    (`production_branch_id` / `--production-branch-id`), mirroring staging
    with production names; the staging pin must never appear.
    """
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
                          other_migrator_secret="TWO_BOT_STAGING_MIGRATOR_DATABASE_URL",
                          branch_input="production_branch_id",
                          branch_flag="--production-branch-id",
                          branch_label="production",
                          other_branch_flag="--staging-branch-id")


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


MCP_PROVEOUT_ENV = {
    "STAGING_MCP_TOKEN": "${{ secrets.STAGING_MCP_TOKEN }}",
    "STAGING_MCP_URL": "https://two-bot-next-staging.5150.workers.dev/mcp",
    "RUN_ID": "${{ github.run_id }}",
}
MCP_PROVEOUT_COMMAND = ('python3 scripts/staging_mcp_proveout.py '
                        '--output "staging-mcp-proveout-$RUN_ID.json"')


def staging_mcp_proveout_errors(workflow):
    """Manual staging-only read-only authenticated MCP prove-out (TOG-23456).

    Dispatch-only with no inputs. The bearer arrives only as the `staging`
    environment secret STAGING_MCP_TOKEN into one Run step (never echoed,
    stored or uploaded; responses carry no credential). One job runs only
    from main through the `staging` environment, on the routed runner for
    job `proveout`, with its own non-cancelling concurrency group and a 10
    minute timeout. Exactly three steps: pinned checkout without persisted
    credentials, one Run step that calls the reviewed stdlib script with
    the bearer via the environment only, and the 14 day artifact upload of
    the sanitized prove-out file with `if: always()` so failures keep
    their evidence. No wrangler, no production path, no staging Worker URL
    variable, no `set -x`.
    """
    name = "staging-mcp-proveout.yml"
    errors = []
    if "env" in workflow:
        errors.append(f"{name}: workflow-level env must stay absent; secrets belong only on the Run step")
    on = workflow.get("on") or {}
    if set(on) != {"workflow_dispatch"}:
        errors.append(f"{name}: must be dispatch-only (no push/pull_request/schedule)")
    elif on.get("workflow_dispatch") not in (None, {}, ""):
        errors.append(f"{name}: workflow_dispatch must take no inputs")
    if workflow.get("permissions") != {}:
        errors.append(f"{name}: top-level permissions must stay empty")
    if workflow.get("concurrency") != {"group": "staging-mcp-proveout", "cancel-in-progress": "false"}:
        errors.append(f"{name}: must use its own staging-mcp-proveout group without cancelling")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"proveout"}:
        errors.append(f"{name}: jobs must be exactly proveout")
        return errors
    job = jobs["proveout"]
    if job.get("environment") != "staging":
        errors.append(f"{name}:proveout: must read the staging Environment binding for STAGING_MCP_TOKEN")
    if job.get("if") != "github.ref == 'refs/heads/main'":
        errors.append(f"{name}:proveout: must run only from main")
    if job.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:proveout: must keep contents:read only")
    if job.get("timeout-minutes") != "10":
        errors.append(f"{name}:proveout: timeout must stay 10 minutes")
    if not runner_allowed("proveout", job.get("runs-on")):
        errors.append(f"{name}:proveout: must use the routed runner expression for job 'proveout'")
    for key in ("env", "needs", "uses", "services", "container", "continue-on-error", "strategy"):
        if key in job:
            errors.append(f"{name}:proveout: must not set {key} (bindings stay on the single Run step)")
    text = str(job).lower().replace(" ", "")
    for marker in ("wrangler", "production", "tojson(secrets", "secrets[", "set-x", "setx", "xtrace",
                   "cloudflare_api_token", "ownership_control", "staging_worker_url",
                   "production_worker_url", "/health", "/readyz", "vars.staging", "inputs."):
        if marker in text:
            errors.append(f"{name}:proveout: must not contain {marker!r}")
    secrets = set(re.findall(r"secrets\.([a-z0-9_]+)", text))
    if secrets != {"staging_mcp_token"}:
        errors.append(f"{name}:proveout: must read exactly the STAGING_MCP_TOKEN secret")
    steps = job.get("steps") or []
    if len(steps) != 3 or any("continue-on-error" in step for step in steps):
        errors.append(f"{name}:proveout: must run exactly checkout, the prove-out and the upload, unconditionally")
        return errors
    checkout, run, upload = steps
    if "if" in checkout or "if" in run:
        errors.append(f"{name}:proveout: only the upload may carry a condition (always())")
        return errors
    if upload.get("if") != "always()":
        errors.append(f"{name}:proveout: upload must run with if: always() so failures keep their artifact")
        return errors
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or (checkout.get("with") or {}).get("persist-credentials") != "false" \
            or (checkout.get("with") or {}).get("fetch-depth") != "1":
        errors.append(f"{name}:proveout: checkout must be pinned, shallow and must not persist credentials")
    if run.get("env") != MCP_PROVEOUT_ENV:
        errors.append(f"{name}:proveout: Run step must bind exactly the bearer, the staging URL and the run id")
    if run.get("run") != MCP_PROVEOUT_COMMAND or "uses" in run or "shell" in run:
        errors.append(f"{name}:proveout: Run step must call the reviewed script with the run id via the environment only")
    if not str(upload.get("uses", "")).startswith("actions/upload-artifact@") or upload.get("with") != {
            "name": "staging-mcp-proveout-${{ github.run_id }}",
            "path": "staging-mcp-proveout-${{ github.run_id }}.json",
            "if-no-files-found": "error", "retention-days": "14"}:
        errors.append(f"{name}:proveout: must upload only the run's json for 14 days and fail if absent")
    return errors


OWNERSHIP_STATUS_ENV = {
    "STAGING_WORKER_URL": "${{ vars.STAGING_WORKER_URL }}",
    "OWNERSHIP_CONTROL_TOKEN": "${{ secrets.STAGING_OWNERSHIP_CONTROL_TOKEN }}",
}


def staging_ownership_status_errors(workflow):
    """Manual staging-only read-only ownership status read.

    Dispatch-only with no inputs. One job runs only from main through the
    `staging` environment, on the routed runner for job `status`, with its
    own non-cancelling concurrency group and a 10 minute timeout. Exactly
    four unconditional steps: pinned checkout without persisted credentials,
    pinned setup-node, the ownership-control preflight, and one Run step that
    calls the reviewed status script with the existing staging pair via the
    environment only and validates the allowlisted owner/timestamp output.
    No deploy, drill, takeover, fence, production or Cloudflare route.
    """
    name = "staging-ownership-status.yml"
    errors = []
    if "env" in workflow:
        errors.append(f"{name}: workflow-level env must stay absent; secrets belong only on the Run steps")
    on = workflow.get("on") or {}
    if set(on) != {"workflow_dispatch"}:
        errors.append(f"{name}: must be dispatch-only (no push/pull_request/schedule)")
    elif on.get("workflow_dispatch") not in (None, {}, ""):
        errors.append(f"{name}: workflow_dispatch must take no inputs")
    if workflow.get("permissions") != {}:
        errors.append(f"{name}: top-level permissions must stay empty")
    if workflow.get("concurrency") != {"group": "staging-ownership-status", "cancel-in-progress": "false"}:
        errors.append(f"{name}: must use its own staging-ownership-status group without cancelling")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"status"}:
        errors.append(f"{name}: jobs must be exactly status")
        return errors
    job = jobs["status"]
    if job.get("name") != "staging ownership status (read-only)":
        errors.append(f"{name}:status: job name must stay read-only")
    if job.get("environment") != "staging":
        errors.append(f"{name}:status: must read only the staging Environment binding")
    if job.get("if") != "github.ref == 'refs/heads/main'":
        errors.append(f"{name}:status: must run only from main")
    if job.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:status: must keep contents:read only")
    if job.get("timeout-minutes") != "10":
        errors.append(f"{name}:status: timeout must stay 10 minutes")
    if job.get("defaults") != {"run": {"working-directory": "wrangler"}}:
        errors.append(f"{name}:status: must run steps from the wrangler directory")
    if not runner_allowed("status", job.get("runs-on")):
        errors.append(f"{name}:status: must use the routed runner expression for job 'status'")
    for key in ("env", "needs", "uses", "services", "container", "continue-on-error", "strategy"):
        if key in job:
            errors.append(f"{name}:status: must not set {key} (bindings stay on the Run steps)")
    text = str(job).lower().replace(" ", "")
    for marker in ("wrangler-action", "wranglerdeploy", "wranglerpublish", "production",
                   "cloudflare_api_token", "cloudflare_account_id", "tojson(secrets", "secrets[",
                   "set-x", "setx", "xtrace", "takeover", "fence", "transition", "inputs."):
        if marker in text:
            errors.append(f"{name}:status: must not contain {marker!r}")
    secrets = set(re.findall(r"secrets\.([a-z0-9_]+)", text))
    if secrets != {"staging_ownership_control_token"}:
        errors.append(f"{name}:status: must read exactly the staging ownership-control secret")
    if "vars.staging_worker_url" not in text:
        errors.append(f"{name}:status: must read the staging Worker URL variable")
    steps = job.get("steps") or []
    if len(steps) != 4 or any(key in step for step in steps for key in ("if", "continue-on-error")):
        errors.append(f"{name}:status: must run exactly checkout, setup-node, preflight and the read, unconditionally")
        return errors
    checkout, node, preflight, run = steps
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or (checkout.get("with") or {}).get("persist-credentials") != "false" \
            or (checkout.get("with") or {}).get("fetch-depth") != "1":
        errors.append(f"{name}:status: checkout must be pinned, shallow and must not persist credentials")
    if not str(node.get("uses", "")).startswith("actions/setup-node@"):
        errors.append(f"{name}:status: second step must be the pinned setup-node")
    if preflight.get("env") != OWNERSHIP_STATUS_ENV \
            or str(preflight.get("run", "")).strip() != "node scripts/ownership-control.mjs preflight":
        errors.append(f"{name}:status: preflight must bind exactly the staging pair and run the preflight")
    if run.get("env") != OWNERSHIP_STATUS_ENV or "uses" in run:
        errors.append(f"{name}:status: read step must bind exactly the staging pair via the environment only")
    command = str(run.get("run", ""))
    if "node scripts/staging-ownership-status.mjs" not in command or "${{" in command:
        errors.append(f"{name}:status: read step must call the reviewed status script without inline expressions")
    if '{"owner_id","observed_at"}' not in command.replace(" ", ""):
        errors.append(f"{name}:status: read step must validate the allowlisted owner/timestamp output")
    return errors


# Pinned central gradual-promote action (verified 2026-10-18; re-verify HEAD
# before repinning). Every error-rate input stays explicit so a central
# default change cannot silently widen the gate.
GRADUAL_PROOF_ACTION = ("TogetherWeOwn/.github/actions/gradual-promote"
                        "@89c290be20c0044efaf6916f28341c76e75fff06")
GRADUAL_PROOF_WORKER = "two-bot-next-gradual-proof"
GRADUAL_PROOF_WITH = {
    "worker-name": GRADUAL_PROOF_WORKER,
    "version-tag": "${{ inputs.candidate_tag }}",
    "domain": "${{ inputs.domain }}",
    "api-token": "${{ secrets.CLOUDFLARE_API_TOKEN }}",
    "account-id": "${{ secrets.CLOUDFLARE_ACCOUNT_ID }}",
    "allow-indexable": "false",
    "error-rate-gate": "true",
    "error-rate-threshold": "0.05",
    "error-rate-min-requests": "20",
    "error-rate-window-minutes": "10",
    "error-rate-soak-seconds": "60",
}


def gradual_promote_proof_errors(workflow):
    """Manual gradual-promote proof caller (passive until the fixture lands).

    Dispatch-only with exactly the mode, confirm_worker, candidate_tag and
    domain inputs. One job runs only from main through the `staging`
    environment (staging credentials only; production secret names never
    appear), on the routed runner for job `proof`, with its own
    non-cancelling concurrency group and a 30 minute timeout. Exactly five
    steps: pinned checkout, pinned setup-node, the target-lock step (which
    refuses before any Cloudflare call until the synthetic fixture lands),
    the pinned central action with every error-rate input explicit, and the
    always() evidence upload. No wrangler, no production path, no `set -x`.
    """
    name = "gradual-promote-proof.yml"
    errors = []
    if "env" in workflow:
        errors.append(f"{name}: workflow-level env must stay absent")
    on = workflow.get("on") or {}
    if set(on) != {"workflow_dispatch"}:
        errors.append(f"{name}: must be dispatch-only (no push/pull_request/schedule)")
    else:
        dispatch = on.get("workflow_dispatch") or {}
        inputs = dispatch.get("inputs") or {}
        if set(dispatch) != {"inputs"} or set(inputs) != {"mode", "confirm_worker", "candidate_tag", "domain"}:
            errors.append(f"{name}: workflow_dispatch must carry exactly mode, confirm_worker, candidate_tag and domain")
        else:
            mode = inputs.get("mode") or {}
            if (mode.get("type") != "choice"
                    or set(mode.get("options") or []) != {"success", "rollback"}
                    or mode.get("default") != "success"):
                errors.append(f"{name}: mode must be success/rollback defaulting to success")
            for key in ("confirm_worker", "candidate_tag", "domain"):
                field = inputs.get(key) or {}
                if (field.get("type") != "string"
                        or str(field.get("required")).lower() != "true"
                        or "default" in field):
                    errors.append(f"{name}: {key} must be a required string with no default")
    if workflow.get("permissions") != {}:
        errors.append(f"{name}: top-level permissions must stay empty")
    if workflow.get("concurrency") != {"group": "gradual-promote-proof", "cancel-in-progress": "false"}:
        errors.append(f"{name}: must use its own gradual-promote-proof group without cancelling")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"proof"}:
        errors.append(f"{name}: jobs must be exactly proof")
        return errors
    job = jobs["proof"]
    if job.get("name") != "gradual-promote-proof":
        errors.append(f"{name}:proof: job name must stay gradual-promote-proof")
    if job.get("if") != "github.ref == 'refs/heads/main'":
        errors.append(f"{name}:proof: must run only from main")
    if job.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:proof: must keep contents:read only")
    if job.get("environment") != "staging":
        errors.append(f"{name}:proof: must read only the staging Environment binding")
    if job.get("timeout-minutes") != "30":
        errors.append(f"{name}:proof: timeout must stay 30 minutes")
    if not runner_allowed("proof", job.get("runs-on")):
        errors.append(f"{name}:proof: must use the routed runner expression for job 'proof'")
    for key in ("env", "needs", "uses", "services", "container", "continue-on-error", "strategy"):
        if key in job:
            errors.append(f"{name}:proof: must not set {key}")
    steps = job.get("steps") or []
    if len(steps) != 5:
        errors.append(f"{name}:proof: must run exactly checkout, setup-node, target lock, the pinned action and the upload")
        return errors
    checkout, node, guard, gradual, upload = steps
    for step, label in ((checkout, "checkout"), (node, "setup-node"),
                        (guard, "target-lock"), (gradual, "central action")):
        if "if" in step:
            errors.append(f"{name}:proof: {label} step must not set if (fail closed)")
        if "continue-on-error" in step:
            errors.append(f"{name}:proof: {label} step must not set continue-on-error (fail closed)")
    if "continue-on-error" in upload:
        errors.append(f"{name}:proof: upload step must not set continue-on-error (fail closed)")
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or (checkout.get("with") or {}).get("persist-credentials") != "false" \
            or (checkout.get("with") or {}).get("fetch-depth") != "1":
        errors.append(f"{name}:proof: checkout must be pinned, shallow and must not persist credentials")
    if not str(node.get("uses", "")).startswith("actions/setup-node@"):
        errors.append(f"{name}:proof: second step must be the pinned setup-node")
    guard_env = guard.get("env") or {}
    if guard_env != {"CONFIRM_WORKER": "${{ inputs.confirm_worker }}",
                     "CANDIDATE_TAG": "${{ inputs.candidate_tag }}",
                     "PROOF_DOMAIN": "${{ inputs.domain }}",
                     "RUN_MODE": "${{ inputs.mode }}"}:
        errors.append(f"{name}:proof: target-lock step must bind exactly the four dispatch inputs via env")
    guard_run = str(guard.get("run") or "")
    if guard.get("shell") != "bash" or "set -euo pipefail" not in guard_run:
        errors.append(f"{name}:proof: target-lock step must run fail-closed bash")
    for needle in (GRADUAL_PROOF_WORKER, "passive", "No Cloudflare call",
                     "${#CANDIDATE_TAG}", "-gt 128",
                     "two-bot-next-gradual-proof.*.*.workers.dev",
                     "single-label synthetic host"):
        if needle not in guard_run:
            errors.append(f"{name}:proof: target-lock step must pin {needle}")
    if "${{" in guard_run:
        errors.append(f"{name}:proof: target-lock step must read inputs via the environment only")
    if gradual.get("id") != "gradual":
        errors.append(f"{name}:proof: the central action step must carry id gradual")
    if gradual.get("uses") != GRADUAL_PROOF_ACTION:
        errors.append(f"{name}:proof: must call the pinned central gradual-promote action")
    if (gradual.get("with") or {}) != GRADUAL_PROOF_WITH:
        errors.append(f"{name}:proof: central action inputs must stay pinned with every error-rate input explicit")
    if upload.get("if") != "always()":
        errors.append(f"{name}:proof: upload must run with if: always() so refusals keep their evidence")
        return errors
    if not str(upload.get("uses", "")).startswith("actions/upload-artifact@") or upload.get("with") != {
            "name": "gradual-promote-proof-${{ github.run_id }}-${{ github.run_attempt }}",
            "path": "proof-evidence/",
            "if-no-files-found": "warn", "retention-days": "14"}:
        errors.append(f"{name}:proof: must upload the run evidence artifact for 14 days and warn if absent")
    text = str(job).lower().replace(" ", "")
    for marker in ("production", "tojson(secrets", "secrets[", "set-x", "setx", "xtrace",
                   "wrangler", "/health", "/readyz"):
        if marker in text:
            errors.append(f"{name}:proof: must not contain {marker!r}")
    # workers.dev appears only as the synthetic host shape in the guard's
    # domain case pattern (accept plus single-label rejection); anywhere else
    # it would be a deploy route.
    if text.count("workers.dev") != 2 \
            or "two-bot-next-gradual-proof.*.workers.dev" not in guard_run \
            or "two-bot-next-gradual-proof.*.*.workers.dev" not in guard_run:
        errors.append(f"{name}:proof: workers.dev may appear only in the guard domain pattern")
    secrets = set(re.findall(r"secrets\.([a-z0-9_]+)", text))
    if secrets != {"cloudflare_api_token", "cloudflare_account_id"}:
        errors.append(f"{name}:proof: must read only the staging Cloudflare pair")
    return errors


def production_errors(workflow):
    """deploy-production is the one live route: dispatch-only, human-gated, chained to staging."""
    name = "deploy-production.yml"
    errors = []
    if set(workflow.get("on") or {}) != {"workflow_dispatch"}:
        errors.append(f"{name}: production must be dispatch-only")
    jobs = workflow.get("jobs", {})
    production, guard = jobs.get("production", {}), jobs.get("guard", {})
    needs = production.get("needs")
    needs = [needs] if isinstance(needs, str) else (needs or [])
    expected_production_if = (
        "always() && needs.guard.result == 'success' && "
        "(needs.guard.outputs.mode == 'rollback' || needs.guard.outputs.allow_unstaged == 'true' || "
        "(needs.schema-gate.result == 'success' && needs.roles-gate.result == 'success'))"
    )
    if production.get("environment") != "production" or set(needs) != {"guard", "schema-gate", "roles-gate"}:
        errors.append(f"{name}:production: must wait for the guard, the schema gate, the roles gate and the protected Environment")
    if production.get("if") != expected_production_if:
        errors.append(f"{name}:production: rollback may bypass skipped gates; deploy requires both gates")
    expected_gate_if = "needs.guard.outputs.mode == 'deploy' && needs.guard.outputs.allow_unstaged != 'true'"
    for gate_name in ("schema-gate", "roles-gate"):
        if jobs.get(gate_name, {}).get("if") != expected_gate_if:
            errors.append(f"{name}:{gate_name}: must run only for ordinary deploys")
    guard_text = str(guard).lower()
    guard_markers = tuple(marker for marker in DEPLOY_MARKERS if marker not in ("production_worker_url", "/readyz"))
    if ("environment" in guard or "uses" in guard or "curl " in guard_text
            or any(marker in guard_text for marker in guard_markers)):
        errors.append(f"{name}:guard: deployment/probe alternative outside the production job")
    guard_script = "\n".join(step.get("run", "") for step in guard.get("steps", []))
    for required in ('urllib.request.Request(',
                     'worker_url + "/readyz"',
                     '"User-Agent": "two-bot-next-production-rollout/1.0"',
                     'urllib.request.urlopen(request, timeout=10)',
                     'live_report.get("build_revision") != sha'):
        if required not in guard_script:
            errors.append(f"{name}:guard: missing live-SHA check {required}")
    if guard_script.count("urllib.request.urlopen(") != 1:
        errors.append(f"{name}:guard: unexpected live production probe")
    if guard_script.count("urllib.request.Request(") != 1:
        errors.append(f"{name}:guard: unexpected live production probe")
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


# Pinned build image shared with the migration plan jobs (same runner
# binary, same toolchain). A digest bump must move all consumers together.
SCHEMA_GATE_IMAGE = ("rust:1.98-trixie@sha256:"
                     "a8a5f0a1e5fe7dfe1d352591e4a1c7dd2c08fd70475cae872cf3458ba0df0546")


def schema_gate_errors(workflow, name="deploy-staging.yml"):
    """Pre-deploy schema-drift gate: read-only plan, fail closed, no new privilege.

    One job runs on every deploy-staging run (no `if:`, no `needs:` of its own)
    through the no-reviewer `staging-migrate-plan` environment, builds the
    reviewed `staging-migrate` binary and fails the run with a `schema drift`
    error when `pending_before` is non-empty. The deploy job must `needs` the
    gate, so drift can never deploy silently. Plan-only: no migrator binding,
    no `--apply`, no role/grant route, no wrangler, no secret indexing.
    """
    errors = []
    jobs = workflow.get("jobs") or {}
    gate = jobs.get("schema-gate", {})
    if gate.get("environment") != "staging-migrate-plan":
        errors.append(f"{name}:schema-gate: must read only the staging-migrate-plan Environment binding")
    if gate.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:schema-gate: must keep contents:read only")
    if "if" in gate:
        errors.append(f"{name}:schema-gate: a job condition would bypass the stage gate")
    if "needs" in gate:
        errors.append(f"{name}:schema-gate: must wait for nothing (it is the wait)")
    if not runner_allowed("schema-gate", gate.get("runs-on")):
        errors.append(f"{name}:schema-gate: must use the routed runner expression for job 'schema-gate'")
    if gate.get("timeout-minutes") != "30":
        errors.append(f"{name}:schema-gate: timeout must stay 30 minutes")
    if (gate.get("container") or {}).get("image") != SCHEMA_GATE_IMAGE:
        errors.append(f"{name}:schema-gate: must build on the pinned plan-job image")
    deploy = jobs.get("deploy", {})
    needs = deploy.get("needs")
    needs = [needs] if isinstance(needs, str) else (needs or [])
    if "schema-gate" not in needs:
        errors.append(f"{name}:deploy: must needs the schema-gate (drift must block the deploy)")
    steps = gate.get("steps") or []
    if len(steps) != 4:
        errors.append(f"{name}:schema-gate: must run exactly checkout, SHA check, build and the gate")
        return errors
    checkout, sha_check, build, run = steps
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or (checkout.get("with") or {}).get("persist-credentials") != "false" \
            or (checkout.get("with") or {}).get("fetch-depth") != "0":
        errors.append(f"{name}:schema-gate: checkout must be pinned, credentialless and full-depth")
    if ("${{ github.sha }}" not in str(sha_check.get("env", ""))
            or "git rev-parse HEAD" not in str(sha_check.get("run", ""))):
        errors.append(f"{name}:schema-gate: SHA check must pin the step to the deploy SHA")
    if "cargo build -p two-bot-cutover --bin staging-migrate --locked" not in str(build.get("run", "")):
        errors.append(f"{name}:schema-gate: build must compile the reviewed staging-migrate binary locked")
    if (run.get("shell") != "bash" or "set -o pipefail" not in str(run.get("run", ""))
            or "scripts/ci/staging_schema_gate.py" not in str(run.get("run", ""))
            or "--source-sha" not in str(run.get("run", ""))
            or "TWO_BOT_STAGING_PLAN_DATABASE_URL" not in str(run.get("env", ""))):
        errors.append(f"{name}:schema-gate: gate step must run the reviewed script over pipefail "
                      "with the plan binding and the deploy SHA")
    # The expected staging identity is pinned independently of the secret
    # binding (vars, never derived from the URL): a wrong-target binding must
    # refuse instead of reading the wrong database.
    env_text = str(run.get("env", ""))
    for pin, var in (("TWO_BOT_STAGING_HOST", "vars.TWO_BOT_STAGING_HOST"),
                     ("TWO_BOT_STAGING_DATABASE", "vars.TWO_BOT_STAGING_DATABASE"),
                     ("TWO_BOT_STAGING_BRANCH_ID", "vars.TWO_BOT_STAGING_BRANCH_ID")):
        if pin not in env_text or var not in env_text:
            errors.append(f"{name}:schema-gate: gate step must pin {pin} from {var}")
    text = str(gate).lower().replace(" ", "")
    for marker in ("wrangler", "--apply", "migrator", "roles", "tojson(secrets", "secrets["):
        if marker in text:
            errors.append(f"{name}:schema-gate: must not contain {marker!r} (plan-only, no apply/grant route)")
    return errors


def production_schema_gate_errors(workflow, name="deploy-production.yml"):
    """Pre-deploy production schema-drift gate: read-only plan, fail closed.

    Ordinary deploys run one post-guard job (it needs the guarded SHA: a
    dispatch SHA is an input, not github.sha) through the no-reviewer
    `production-migrate-plan` environment. The job builds the reviewed
    `staging-migrate` binary and fails with `schema drift` when the guarded
    source is ahead of the production ledger. Rollback and live-SHA takeover
    modes explicitly skip it; the production job accepts those guard-validated
    waivers but otherwise requires this gate to succeed. Plan-only: no
    migrator binding, no `--apply`, no role/grant route, no wrangler, no
    secret indexing.
    """
    errors = []
    jobs = workflow.get("jobs") or {}
    gate = jobs.get("schema-gate", {})
    if gate.get("environment") != "production-migrate-plan":
        errors.append(f"{name}:schema-gate: must read only the production-migrate-plan Environment binding")
    if gate.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:schema-gate: must keep contents:read only")
    if gate.get("if") != "needs.guard.outputs.mode == 'deploy' && needs.guard.outputs.allow_unstaged != 'true'":
        errors.append(f"{name}:gate: must run only for ordinary deploys")
    if gate.get("needs") != "guard":
        errors.append(f"{name}:schema-gate: must wait for the guard (it gates the guarded SHA)")
    if not runner_allowed("schema-gate", gate.get("runs-on")):
        errors.append(f"{name}:schema-gate: must use the routed runner expression for job 'schema-gate'")
    if gate.get("timeout-minutes") != "30":
        errors.append(f"{name}:schema-gate: timeout must stay 30 minutes")
    if (gate.get("container") or {}).get("image") != SCHEMA_GATE_IMAGE:
        errors.append(f"{name}:schema-gate: must build on the pinned plan-job image")
    production = jobs.get("production", {})
    needs = production.get("needs")
    needs = [needs] if isinstance(needs, str) else (needs or [])
    if "schema-gate" not in needs or "guard" not in needs:
        errors.append(f"{name}:production: must needs the guard and the schema-gate (drift must block the deploy)")
    steps = gate.get("steps") or []
    if len(steps) != 5:
        errors.append(f"{name}:schema-gate: must run exactly checkout, tooling checkout, SHA check, build and the gate")
        return errors
    checkout, tooling, sha_check, build, run = steps
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or (checkout.get("with") or {}).get("persist-credentials") != "false" \
            or (checkout.get("with") or {}).get("fetch-depth") != "0" \
            or "needs.guard.outputs.sha" not in str((checkout.get("with") or {}).get("ref", "")):
        errors.append(f"{name}:schema-gate: checkout must be pinned, credentialless, full-depth and on the guarded SHA")
    if not str(tooling.get("uses", "")).startswith("actions/checkout@") \
            or (tooling.get("with") or {}).get("persist-credentials") != "false" \
            or (tooling.get("with") or {}).get("path") != "tooling" \
            or "github.sha" not in str((tooling.get("with") or {}).get("ref", "")):
        errors.append(f"{name}:schema-gate: tooling checkout must be pinned, credentialless and on the workflow commit")
    if ("needs.guard.outputs.sha" not in str(sha_check.get("env", ""))
            or "git rev-parse HEAD" not in str(sha_check.get("run", ""))):
        errors.append(f"{name}:schema-gate: SHA check must pin the step to the guarded SHA")
    if "cargo build -p two-bot-cutover --bin staging-migrate --locked" not in str(build.get("run", "")):
        errors.append(f"{name}:schema-gate: build must compile the reviewed staging-migrate binary locked")
    if (run.get("shell") != "bash" or "set -o pipefail" not in str(run.get("run", ""))
            or "scripts/ci/production_schema_gate.py" not in str(run.get("run", ""))
            or "--source-sha" not in str(run.get("run", ""))
            or "--mode" not in str(run.get("run", ""))
            or "--allow-unstaged" not in str(run.get("run", ""))
            or "TWO_BOT_PRODUCTION_PLAN_DATABASE_URL" not in str(run.get("env", ""))):
        errors.append(f"{name}:schema-gate: gate step must run the reviewed script over pipefail "
                      "with the plan binding, the guarded SHA and the guard-verified waive flags")
    # The expected production identity is pinned independently of the secret
    # binding (vars, never derived from the URL): a wrong-target binding must
    # refuse instead of reading the wrong database. A PlanetScale host also
    # takes the branch pin, exactly as production-migrate does.
    env_text = str(run.get("env", ""))
    for pin, var in (("TWO_BOT_PRODUCTION_HOST", "vars.TWO_BOT_PRODUCTION_HOST"),
                     ("TWO_BOT_PRODUCTION_DATABASE", "vars.TWO_BOT_PRODUCTION_DATABASE"),
                     ("TWO_BOT_PRODUCTION_BRANCH_ID", "vars.TWO_BOT_PRODUCTION_BRANCH_ID")):
        if pin not in env_text or var not in env_text:
            errors.append(f"{name}:schema-gate: gate step must pin {pin} from {var}")
    if "inputs." in str(gate):
        errors.append(f"{name}:schema-gate: must read only guard-verified outputs, never raw inputs")
    text = str(gate).lower().replace(" ", "")
    for marker in ("wrangler", "--apply", "migrator", "roles", "tojson(secrets", "secrets["):
        if marker in text:
            errors.append(f"{name}:schema-gate: must not contain {marker!r} (plan-only, no apply/grant route)")
    return errors


# Markers banned from the roles-gate job text (lowercased, spaces removed).
# `roles` itself is legitimate here (`db roles verify`); instead the ban
# targets the write binding (`migrator_database_url`), the plan-print
# subcommand (`rolesplan`: verify-only, never `db roles plan`), apply and
# wrangler routes, and secret indexing.
ROLES_GATE_MARKERS = ("wrangler", "--apply", "rolesplan", "migrator_database_url",
                      "tojson(secrets", "secrets[")


def roles_gate_errors(workflow, name="deploy-staging.yml"):
    """Pre-deploy role-drift gate: read-only verify, fail closed, no new privilege.

    One job runs on every deploy-staging run (no `if:`, no `needs:` of its own)
    through the no-reviewer `staging-migrate-plan` environment, builds the
    reviewed `two-bot` binary and fails the run when `db roles verify` reports
    any finding. The deploy job must `needs` the gate, so drift can never
    deploy silently. Verify-only: no migrator write binding, no `--apply`, no
    `roles plan` subcommand, no wrangler, no secret indexing.
    """
    errors = []
    jobs = workflow.get("jobs") or {}
    gate = jobs.get("roles-gate", {})
    if gate.get("environment") != "staging-migrate-plan":
        errors.append(f"{name}:roles-gate: must read only the staging-migrate-plan Environment binding")
    if gate.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:roles-gate: must keep contents:read only")
    if "if" in gate:
        errors.append(f"{name}:roles-gate: a job condition would let drift deploy silently")
    if "needs" in gate:
        errors.append(f"{name}:roles-gate: must wait for nothing (it is the wait)")
    if not runner_allowed("roles-gate", gate.get("runs-on")):
        errors.append(f"{name}:roles-gate: must use the routed runner expression for job 'roles-gate'")
    if gate.get("timeout-minutes") != "30":
        errors.append(f"{name}:roles-gate: timeout must stay 30 minutes")
    if (gate.get("container") or {}).get("image") != SCHEMA_GATE_IMAGE:
        errors.append(f"{name}:roles-gate: must build on the pinned plan-job image")
    deploy = jobs.get("deploy", {})
    needs = deploy.get("needs")
    needs = [needs] if isinstance(needs, str) else (needs or [])
    if "roles-gate" not in needs:
        errors.append(f"{name}:deploy: must needs the roles-gate (drift must block the deploy)")
    steps = gate.get("steps") or []
    if "continue-on-error" in gate:
        errors.append(f"{name}:roles-gate: job must not tolerate failure")
    if any("if" in step or "continue-on-error" in step for step in steps):
        errors.append(f"{name}:roles-gate: gate steps must not skip or tolerate failure")
    if len(steps) != 4:
        errors.append(f"{name}:roles-gate: must run exactly checkout, SHA check, build and the gate")
        return errors
    checkout, sha_check, build, run = steps
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or (checkout.get("with") or {}).get("persist-credentials") != "false" \
            or (checkout.get("with") or {}).get("fetch-depth") != "0":
        errors.append(f"{name}:roles-gate: checkout must be pinned, credentialless and full-depth")
    if ("${{ github.sha }}" not in str(sha_check.get("env", ""))
            or "git rev-parse HEAD" not in str(sha_check.get("run", ""))):
        errors.append(f"{name}:roles-gate: SHA check must pin the step to the deploy SHA")
    if "cargo build -p two-bot --bin two-bot --locked" not in str(build.get("run", "")):
        errors.append(f"{name}:roles-gate: build must compile the reviewed two-bot binary locked")
    expected_run = """set -o pipefail
python3 scripts/ci/roles_gate.py \\
  --binary ./target/debug/two-bot --target staging \\
  --expected-host "$TWO_BOT_STAGING_HOST" --expected-database "$TWO_BOT_STAGING_DATABASE" \\
  --expected-branch-id "$TWO_BOT_STAGING_BRANCH_ID"
"""
    if (run.get("shell") != "bash" or str(run.get("run", "")).strip() != expected_run.strip()
            or (run.get("env") or {}).get("TWO_DATABASE_URL")
            != "${{ secrets.TWO_BOT_STAGING_PLAN_DATABASE_URL }}"):
        errors.append(f"{name}:roles-gate: gate step must pin the exact fail-closed staging command and plan binding")
    # The expected staging identity is pinned independently of the secret
    # binding (vars, never derived from the URL): a wrong-target binding must
    # refuse instead of reading the wrong database.
    env_text = str(run.get("env", ""))
    for pin, var in (("TWO_BOT_STAGING_HOST", "vars.TWO_BOT_STAGING_HOST"),
                     ("TWO_BOT_STAGING_DATABASE", "vars.TWO_BOT_STAGING_DATABASE"),
                     ("TWO_BOT_STAGING_BRANCH_ID", "vars.TWO_BOT_STAGING_BRANCH_ID")):
        if pin not in env_text or var not in env_text:
            errors.append(f"{name}:roles-gate: gate step must pin {pin} from {var}")
    text = str(gate).lower().replace(" ", "")
    for marker in ROLES_GATE_MARKERS:
        if marker in text:
            errors.append(f"{name}:roles-gate: must not contain {marker!r} (verify-only, no apply/grant route)")
    return errors


def production_roles_gate_errors(workflow, name="deploy-production.yml"):
    """Pre-deploy production role-drift gate: read-only verify, fail closed.

    Ordinary deploys run one post-guard job (it needs the guarded SHA: a
    dispatch SHA is an input, not github.sha) through the no-reviewer
    `production-migrate-plan` environment. It builds the reviewed `two-bot`
    binary and fails when `db roles verify` reports any finding. Rollback and
    live-SHA takeover modes explicitly skip it; the production job accepts
    those guard-validated waivers but otherwise requires this gate to succeed.
    Verify-only: no migrator write binding, no `--apply`, no `roles plan`
    subcommand, no wrangler, no secret indexing.
    """
    errors = []
    jobs = workflow.get("jobs") or {}
    gate = jobs.get("roles-gate", {})
    if gate.get("environment") != "production-migrate-plan":
        errors.append(f"{name}:roles-gate: must read only the production-migrate-plan Environment binding")
    if gate.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:roles-gate: must keep contents:read only")
    if gate.get("if") != "needs.guard.outputs.mode == 'deploy' && needs.guard.outputs.allow_unstaged != 'true'":
        errors.append(f"{name}:roles-gate: must run only for ordinary deploys")
    if gate.get("needs") != "guard":
        errors.append(f"{name}:roles-gate: must wait for the guard (it gates the guarded SHA)")
    if not runner_allowed("roles-gate", gate.get("runs-on")):
        errors.append(f"{name}:roles-gate: must use the routed runner expression for job 'roles-gate'")
    if gate.get("timeout-minutes") != "30":
        errors.append(f"{name}:roles-gate: timeout must stay 30 minutes")
    if (gate.get("container") or {}).get("image") != SCHEMA_GATE_IMAGE:
        errors.append(f"{name}:roles-gate: must build on the pinned plan-job image")
    production = jobs.get("production", {})
    needs = production.get("needs")
    needs = [needs] if isinstance(needs, str) else (needs or [])
    if "roles-gate" not in needs or "guard" not in needs:
        errors.append(f"{name}:production: must needs the guard and the roles-gate (drift must block the deploy)")
    steps = gate.get("steps") or []
    if "continue-on-error" in gate:
        errors.append(f"{name}:roles-gate: job must not tolerate failure")
    if any("if" in step or "continue-on-error" in step for step in steps):
        errors.append(f"{name}:roles-gate: gate steps must not skip or tolerate failure")
    if len(steps) != 5:
        errors.append(f"{name}:roles-gate: must run exactly checkout, tooling checkout, SHA check, build and the gate")
        return errors
    checkout, tooling, sha_check, build, run = steps
    if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
            or (checkout.get("with") or {}).get("persist-credentials") != "false" \
            or (checkout.get("with") or {}).get("fetch-depth") != "0" \
            or "needs.guard.outputs.sha" not in str((checkout.get("with") or {}).get("ref", "")):
        errors.append(f"{name}:roles-gate: checkout must be pinned, credentialless, full-depth and on the guarded SHA")
    if not str(tooling.get("uses", "")).startswith("actions/checkout@") \
            or (tooling.get("with") or {}).get("persist-credentials") != "false" \
            or (tooling.get("with") or {}).get("path") != "tooling" \
            or "github.sha" not in str((tooling.get("with") or {}).get("ref", "")):
        errors.append(f"{name}:roles-gate: tooling checkout must be pinned, credentialless and on the workflow commit")
    if ("needs.guard.outputs.sha" not in str(sha_check.get("env", ""))
            or "git rev-parse HEAD" not in str(sha_check.get("run", ""))):
        errors.append(f"{name}:roles-gate: SHA check must pin the step to the guarded SHA")
    if "cargo build -p two-bot --bin two-bot --locked" not in str(build.get("run", "")):
        errors.append(f"{name}:roles-gate: build must compile the reviewed two-bot binary locked")
    expected_run = """set -o pipefail
python3 tooling/scripts/ci/roles_gate.py \\
  --binary ./target/debug/two-bot --target production \\
  --expected-host "$TWO_BOT_PRODUCTION_HOST" --expected-database "$TWO_BOT_PRODUCTION_DATABASE" \\
  --expected-branch-id "$TWO_BOT_PRODUCTION_BRANCH_ID" \\
  --mode "$MODE" --allow-unstaged "$ALLOW_UNSTAGED"
"""
    run_env = run.get("env") or {}
    if (run.get("shell") != "bash" or str(run.get("run", "")).strip() != expected_run.strip()
            or run_env.get("TWO_DATABASE_URL")
            != "${{ secrets.TWO_BOT_PRODUCTION_PLAN_DATABASE_URL }}"
            or run_env.get("MODE") != "${{ needs.guard.outputs.mode }}"
            or run_env.get("ALLOW_UNSTAGED") != "${{ needs.guard.outputs.allow_unstaged }}"):
        errors.append(f"{name}:roles-gate: gate step must pin the exact production command and guard-verified values")
    # The expected production identity is pinned independently of the secret
    # binding (vars, never derived from the URL): a wrong-target binding must
    # refuse instead of reading the wrong database. A PlanetScale host also
    # takes the branch pin, exactly as production-migrate does.
    env_text = str(run.get("env", ""))
    for pin, var in (("TWO_BOT_PRODUCTION_HOST", "vars.TWO_BOT_PRODUCTION_HOST"),
                     ("TWO_BOT_PRODUCTION_DATABASE", "vars.TWO_BOT_PRODUCTION_DATABASE"),
                     ("TWO_BOT_PRODUCTION_BRANCH_ID", "vars.TWO_BOT_PRODUCTION_BRANCH_ID")):
        if pin not in env_text or var not in env_text:
            errors.append(f"{name}:roles-gate: gate step must pin {pin} from {var}")
    if "inputs." in str(gate):
        errors.append(f"{name}:roles-gate: must read only guard-verified outputs, never raw inputs")
    text = str(gate).lower().replace(" ", "")
    for marker in ROLES_GATE_MARKERS:
        if marker in text:
            errors.append(f"{name}:roles-gate: must not contain {marker!r} (verify-only, no apply/grant route)")
    return errors


def voice_synthetic_errors(workflow):
    """Live staging voice synthetic (docs/voice-synthetic.md).

    Runs on workflow_call (deploy-staging), a 6-hourly schedule and dispatch,
    only from main, through the `voice-synthetic` environment whose single
    secret is the dedicated test bot token, on the routed runner for job
    `synthetic`, with its own non-cancelling concurrency group and a 10 minute
    timeout. Exactly three steps: pinned checkout without persisted
    credentials, one Run step (the only holder of the token) that runs the
    reviewed script, and the evidence upload. No wrangler, no production path,
    no staging Worker URL, no `set -x`.
    """
    name = "voice-synthetic.yml"
    errors = []
    if "env" in workflow:
        errors.append(f"{name}: workflow-level env must stay absent")
    on = workflow.get("on") or {}
    if set(on) != {"workflow_call", "schedule", "workflow_dispatch"}:
        errors.append(f"{name}: triggers must be exactly workflow_call, schedule and workflow_dispatch")
    call = on.get("workflow_call") or {}
    if on.get("workflow_dispatch") or set(call) - {"secrets"} or (
        call.get("secrets") is not None
        and call["secrets"] != {"TWO_VOICE_SYNTHETIC_BOT_TOKEN": {"required": "false"}}
    ):
        errors.append(
            f"{name}: workflow_call takes only the optional TWO_VOICE_SYNTHETIC_BOT_TOKEN secret; "
            "workflow_dispatch takes no inputs"
        )
    if workflow.get("permissions") != {}:
        errors.append(f"{name}: top-level permissions must stay empty")
    if workflow.get("concurrency") != {"group": "voice-synthetic", "cancel-in-progress": "false"}:
        errors.append(f"{name}: must use its own voice-synthetic group without cancelling")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"synthetic"}:
        errors.append(f"{name}: jobs must be exactly synthetic")
        return errors
    job = jobs["synthetic"]
    if job.get("environment") != "voice-synthetic":
        errors.append(f"{name}:synthetic: must read only the voice-synthetic Environment")
    if job.get("if") != "github.ref == 'refs/heads/main'":
        errors.append(f"{name}:synthetic: must run only from main")
    if job.get("permissions") != {"contents": "read"}:
        errors.append(f"{name}:synthetic: must keep contents:read only")
    if job.get("timeout-minutes") != "10":
        errors.append(f"{name}:synthetic: timeout must stay 10 minutes")
    if not runner_allowed("synthetic", job.get("runs-on")):
        errors.append(f"{name}:synthetic: must use the routed runner expression for job 'synthetic'")
    for key in ("env", "needs", "uses", "services", "container", "continue-on-error", "strategy"):
        if key in job:
            errors.append(f"{name}:synthetic: must not set {key}")
    steps = job.get("steps") or []
    runs = [step for step in steps if "run" in step]
    if len(steps) != 3 or len(runs) != 1 or "scripts/staging_voice_synthetic.py" not in runs[0]["run"]:
        errors.append(f"{name}:synthetic: must be checkout, one Run step of the reviewed script, evidence upload")
    for step in steps:
        if "secrets." in str(step) and step is not (runs[0] if runs else None):
            errors.append(f"{name}:synthetic: only the Run step may hold a secret")
    text = str(job).lower().replace(" ", "")
    for marker in ("wrangler", "production", "staging_worker_url", "tojson(secrets", "secrets[", "set-x",
                   "setx", "xtrace", "cloudflare_api_token", "ownership_control"):
        if marker in text:
            errors.append(f"{name}:synthetic: must not contain {marker!r}")
    return errors


def synthetic_user_flow_errors(workflow):
    """Five-minute user-flow synthetic (docs/synthetic-user-flow.md).

    Runs on a 5-minute schedule and manual dispatch, only from main, through
    the `synthetic-alerts` environment. Two jobs: `staging` runs the reviewed
    staging health-contract probe, `prod-passive` runs the reviewed passive
    production probe (observe and alert only, never a deploy gate). Each job
    has an optional target probe, a notice when its binding is absent, an
    always() evidence upload, an always() decide step, and a gated post step.
    The decide step reads recent run history with actions:read, alerts after
    two failures, and writes the event to GITHUB_OUTPUT; the post step runs
    only when an event is due so a no-op is skipped (never success), and only
    a successful post counts as delivered. No wrangler, no deploy route, no
    `set -x`.
    """
    name = "synthetic-user-flow.yml"
    errors = []
    if "env" in workflow:
        errors.append(f"{name}: workflow-level env must stay absent")
    on = workflow.get("on") or {}
    if set(on) != {"schedule", "workflow_dispatch"}:
        errors.append(f"{name}: triggers must be exactly schedule and workflow_dispatch")
    else:
        schedules = on.get("schedule") or []
        if schedules != [{"cron": "*/5 * * * *"}]:
            errors.append(f"{name}: schedule must stay every 5 minutes")
        dispatch = on.get("workflow_dispatch") or {}
        inputs = (dispatch.get("inputs") or {}) if isinstance(dispatch, dict) else {}
        if set(dispatch) != {"inputs"} or set(inputs) != {"force_fail"}:
            errors.append(f"{name}: workflow_dispatch must carry only the force_fail input")
        else:
            field = inputs["force_fail"] or {}
            if field.get("type") != "boolean" or field.get("default") not in (False, "false"):
                errors.append(f"{name}: force_fail must be an opt-in boolean defaulting to false")
    if workflow.get("permissions") != {}:
        errors.append(f"{name}: top-level permissions must stay empty")
    if workflow.get("concurrency") != {"group": "synthetic-user-flow", "cancel-in-progress": "false"}:
        errors.append(f"{name}: must use its own synthetic-user-flow group without cancelling")
    jobs = workflow.get("jobs") or {}
    if set(jobs) != {"staging", "prod-passive"}:
        errors.append(f"{name}: jobs must be exactly staging and prod-passive")
        return errors
    expected_script = {"staging": "scripts/staging_health_contract_probe.py",
                       "prod-passive": "scripts/prod_passive_probe.py"}
    for job_id in ("staging", "prod-passive"):
        job = jobs[job_id]
        if job.get("environment") != "synthetic-alerts":
            errors.append(f"{name}:{job_id}: must read only the synthetic-alerts Environment")
        if job.get("if") != "github.ref == 'refs/heads/main'":
            errors.append(f"{name}:{job_id}: must run only from main")
        if job.get("permissions") != {"actions": "read", "contents": "read"}:
            errors.append(f"{name}:{job_id}: must keep actions:read plus contents:read only")
        if job.get("timeout-minutes") != "10":
            errors.append(f"{name}:{job_id}: timeout must stay 10 minutes")
        if not runner_allowed(job_id, job.get("runs-on")):
            errors.append(f"{name}:{job_id}: must use the routed runner expression for job '{job_id}'")
        for key in ("env", "needs", "uses", "services", "container", "continue-on-error", "strategy"):
            if key in job:
                errors.append(f"{name}:{job_id}: must not set {key}")
        steps = job.get("steps") or []
        if len(steps) != 6:
            errors.append(f"{name}:{job_id}: must run checkout, probe, notice, upload, decide and alert")
            continue
        checkout, probe_step, notice, upload, decide_step, alert_step = steps
        if not str(checkout.get("uses", "")).startswith("actions/checkout@") \
                or (checkout.get("with") or {}).get("persist-credentials") != "false":
            errors.append(f"{name}:{job_id}: checkout must be pinned and must not persist credentials")
        if expected_script[job_id] not in str(probe_step.get("run", "")) or "uses" in probe_step:
            errors.append(f"{name}:{job_id}: probe step must call {expected_script[job_id]}")
        expected_target = "STAGING_WORKER_URL" if job_id == "staging" else "PRODUCTION_WORKER_URL"
        if probe_step.get("id") != "probe" or expected_target not in str(probe_step.get("if", "")):
            errors.append(f"{name}:{job_id}: probe must be skipped when {expected_target} is unset")
        expected_notice = expected_target + " is unset"
        if expected_notice not in str(notice.get("run", "")) or "::warning::" not in str(notice.get("run", "")):
            errors.append(f"{name}:{job_id}: missing-binding skip must emit a notice")
        if upload.get("if") != "always()":
            errors.append(f"{name}:{job_id}: upload must run with if: always() so failures keep their artifact")
        if not str(upload.get("uses", "")).startswith("actions/upload-artifact@"):
            errors.append(f"{name}:{job_id}: must upload the run evidence artifact")
        if decide_step.get("if") != "always()":
            errors.append(f"{name}:{job_id}: decide step must run with if: always()")
        if "scripts/synthetic_alert.py" not in str(decide_step.get("run", "")) \
                or "--decide-only" not in str(decide_step.get("run", "")):
            errors.append(f"{name}:{job_id}: decide step must call scripts/synthetic_alert.py --decide-only")
        if decide_step.get("id") != "decide":
            errors.append(f"{name}:{job_id}: decide step must expose id decide for the post gate")
        if alert_step.get("if") != "${{ !cancelled() && steps.decide.outputs.event != '' }}":
            errors.append(f"{name}:{job_id}: post step must run only when steps.decide.outputs.event != ''")
        # Without a status function GitHub adds an implicit success() &&, so
        # a failed probe would skip the post step and no ALERT would go out
        # during an outage. Keep this check even if the exact pin above moves.
        if not any(func in str(alert_step.get("if", ""))
                   for func in ("!cancelled()", "always()", "failure()", "success()")):
            errors.append(f"{name}:{job_id}: post step gate must carry a status function so GitHub does not add implicit success()")
        if "scripts/synthetic_alert.py" not in str(alert_step.get("run", "")) \
                or "--post-only" not in str(alert_step.get("run", "")):
            errors.append(f"{name}:{job_id}: alert step must call scripts/synthetic_alert.py --post-only")
        environment = "staging" if job_id == "staging" else "prod"
        job_name = JOB_NAMES[environment]
        probe_name = PROBE_STEPS[environment]
        alert_name = ALERT_STEPS[environment]
        decide_name = DECIDE_STEPS[environment]
        if job.get("name") != job_name:
            errors.append(f"{name}:{job_id}: job name must match synthetic alert history lookup")
        if probe_step.get("name") != probe_name:
            errors.append(f"{name}:{job_id}: probe step name must match synthetic alert history lookup")
        if decide_step.get("name") != decide_name:
            errors.append(f"{name}:{job_id}: decide step name must match synthetic alert history lookup")
        if alert_step.get("name") != alert_name:
            errors.append(f"{name}:{job_id}: alert step name must match synthetic alert history lookup")
        decide_env = decide_step.get("env") or {}
        if (decide_env.get("GITHUB_TOKEN") != "${{ secrets.GITHUB_TOKEN }}"
                or decide_env.get("PROBE_OUTCOME") != "${{ steps.probe.outcome }}"
                or "SYNTHETIC_ALERT_WEBHOOK_URL" in decide_env
                or "secrets." in str(decide_step.get("run", ""))):
            errors.append(f"{name}:{job_id}: decide step must pass token and outcome via env only")
        alert_env = alert_step.get("env") or {}
        if (alert_env.get("SYNTHETIC_ALERT_WEBHOOK_URL") != "${{ secrets.SYNTHETIC_ALERT_WEBHOOK_URL }}"
                or alert_env.get("ALERT_EVENT") != "${{ steps.decide.outputs.event }}"
                or "GITHUB_TOKEN" in alert_env
                or "PROBE_OUTCOME" in alert_env
                or "secrets." in str(alert_step.get("run", ""))):
            errors.append(f"{name}:{job_id}: post step must pass webhook and decided event via env only")
        for step in steps:
            if step not in (decide_step, alert_step) and "secrets." in str(step):
                errors.append(f"{name}:{job_id}: only the decide and alert steps may hold a secret")
    text = str(jobs).lower().replace(" ", "")
    for marker in ("wrangler", "tojson(secrets", "secrets[", "set-x", "setx", "xtrace",
                   "cloudflare_api_token", "ownership_control"):
        if marker in text:
            errors.append(f"{name}: must not contain {marker!r}")
    secrets = set(re.findall(r"secrets\.([a-z0-9_]+)", text))
    if secrets != {"synthetic_alert_webhook_url", "github_token"}:
        errors.append(f"{name}: must read only the webhook secret and read-only GitHub token")
    return errors


def workflow_policy_errors(workflows):
    errors = []
    errors.extend(staging_gateway_injection_errors(workflows))
    errors.extend(staging_error_rate_gate_errors(workflows))
    errors.extend(staging_fault_injection_errors(workflows))
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
            errors.extend(production_schema_gate_errors(workflow))
            errors.extend(production_roles_gate_errors(workflow))
            continue
        if name == "deploy-staging.yml":
            errors.extend(staging_dispatch_errors(workflow))
            errors.extend(staging_active_errors(workflow))
            errors.extend(schema_gate_errors(workflow))
            errors.extend(roles_gate_errors(workflow))
        if name == "voice-synthetic.yml":
            # Pinned shape above; the generic environment scan below would
            # flag its voice-synthetic Environment binding.
            errors.extend(voice_synthetic_errors(workflow))
            continue
        if name == "synthetic-user-flow.yml":
            # Pinned shape above; the generic environment/marker scan below
            # would flag its synthetic-alerts binding and probe paths.
            errors.extend(synthetic_user_flow_errors(workflow))
            continue
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
        if name == "staging-mcp-proveout.yml":
            # Manual read-only authenticated MCP prove-out: pinned shape
            # above; the generic environment scan below would flag its
            # staging Environment binding.
            errors.extend(staging_mcp_proveout_errors(workflow))
            continue
        if name == "gradual-promote-proof.yml":
            # Manual gradual-promote proof caller (passive until the fixture
            # lands): pinned shape above; the generic environment/marker
            # scan below would flag its staging binding and Cloudflare pair.
            errors.extend(gradual_promote_proof_errors(workflow))
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
        if name == "staging-ownership-status.yml":
            # Manual read-only ownership status read: pinned shape above;
            # the generic environment scan below would flag its staging
            # Environment binding.
            errors.extend(staging_ownership_status_errors(workflow))
            continue
        for job_id, job in jobs.items():
            if name == "deploy-staging.yml" and job_id == "schema-gate":
                continue  # pre-deploy drift gate: pinned by schema_gate_errors above;
                # the generic environment scan below would flag its plan binding
            if name == "deploy-staging.yml" and job_id == "roles-gate":
                continue  # pre-deploy role-drift gate: pinned by roles_gate_errors above;
                # the generic environment scan below would flag its plan binding
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

    def test_merge_queue_reports_required_checks_without_pr_metadata(self):
        for name in ("check.yml", "supply-chain.yml"):
            workflow = self.workflows[name]
            self.assertEqual(workflow["on"]["merge_group"], {"types": ["checks_requested"]})
            self.assertEqual(workflow["concurrency"]["cancel-in-progress"],
                             "${{ github.event_name == 'pull_request' }}")
        gates = self.workflows["supply-chain.yml"]["jobs"]
        self.assertEqual(gates["pr-lint"]["name"], "pr-lint")
        self.assertEqual(gates["gitleaks"]["name"], "gitleaks")
        self.assertEqual(gates["gitleaks"]["if"], "${{ !cancelled() }}")
        steps = gates["pr-lint"]["steps"]
        for name in ("Resolve PR title/body", "Check title, body, branch and commits"):
            step = next(s for s in steps if s.get("name") == name)
            self.assertEqual(step["if"], "github.event_name != 'merge_group'")
        receipt = next(s for s in steps if s.get("name") == "Accept metadata already linted on the PR")
        self.assertEqual(receipt["if"], "github.event_name == 'merge_group'")
        self.assertIn("echo ", receipt["run"])
        self.assertEqual(self.workflows["check.yml"]["jobs"]["ci-ok"]["name"], "ci-ok")
        # Queue validation never becomes a deployment entry point.
        for name in ("deploy-staging.yml", "deploy-production.yml"):
            self.assertNotIn("merge_group", self.workflows[name]["on"])

    def test_quality_gates_diff_merge_checkout_against_its_first_parent(self):
        steps = self.workflows["check.yml"]["jobs"]["check"]["steps"]
        expected_base = "${{ github.event.merge_group.base_sha || (github.event.pull_request && format('{0}^1', github.sha)) || github.event.before || 'origin/main' }}"
        expected_head = "${{ github.sha }}"
        for name in (
            "Floor guard (HARD, CONSTRAINTS.md floor)",
            "Unwrap in request paths (HARD)",
            "Semgrep Rust rules (HARD)",
        ):
            with self.subTest(name=name):
                step = next(step for step in steps if step.get("name") == name)
                base_key = "FLOOR_BASE" if name.startswith("Floor guard") else "QUALITY_BASE"
                head_key = "FLOOR_HEAD" if name.startswith("Floor guard") else "QUALITY_HEAD"
                self.assertEqual(step["env"][base_key], expected_base)
                self.assertEqual(step["env"][head_key], expected_head)

    def test_rust_tests_timeout_leaves_headroom_for_coverage_measurement(self):
        # The HARD patch-coverage step re-runs the suite under llvm-cov
        # (measured 30 min of a 42m16s job; four earlier runs of this design
        # took 36-42 min), so every event keeps 60 minutes: a slow runner
        # reports a coverage verdict, not a job timeout.
        job = self.workflows["check.yml"]["jobs"]["rust-tests"]
        # BaseLoader keeps scalars as strings; "60" is the 60-minute limit.
        self.assertEqual(job["timeout-minutes"], "60")

    def test_rust_test_container_pins_disposable_database_service_address(self):
        job = self.workflows["check.yml"]["jobs"]["rust-tests"]
        prerequisites = next(
            step for step in job["steps"]
            if step.get("name") == "Job-container prerequisites (loopback forward to agent-testdb)"
        )
        self.assertEqual(prerequisites.get("shell"), "bash")
        run = prerequisites["run"]
        self.assertIn("set -e", run)
        hosts_tmp = run.index('hosts_tmp="${RUNNER_TEMP:?RUNNER_TEMP must be set}/hosts"')
        resolve = run.index("getent ahostsv4 agent-testdb")
        remove_stale_pin = run.index("sed -E 's/(^|[[:space:]])agent-testdb([[:space:]]|$)/ /g' /etc/hosts > \"$hosts_tmp\"")
        self.assertNotIn("sed -i -E", run)
        self.assertLess(hosts_tmp, remove_stale_pin)
        copy_hosts = run.index('cat "$hosts_tmp" > /etc/hosts')
        cleanup_hosts = run.index('rm -f "$hosts_tmp"')
        self.assertLess(remove_stale_pin, copy_hosts)
        self.assertLess(copy_hosts, cleanup_hosts)
        pin = run.index("printf '%s agent-testdb\\n' \"$service_ip\" >> /etc/hosts")
        verify_pin = run.index('getent ahostsv4 agent-testdb | awk \'NR == 1 { print $1; exit }\'')
        forward = run.index("TCP:agent-testdb:5432")
        ready = run.index("pg_isready -q -h 127.0.0.1")
        self.assertIn("for attempt in $(seq 1 30)", run[:resolve])
        self.assertLess(remove_stale_pin, resolve)
        self.assertLess(resolve, pin)
        self.assertLess(pin, verify_pin)
        mismatch_check = run.index('if [ "$pinned_ip" != "$service_ip" ]; then')
        mismatch_exit = run.index("exit 1", mismatch_check)
        self.assertLess(verify_pin, mismatch_check)
        self.assertLess(mismatch_exit, forward)
        self.assertLess(verify_pin, forward)
        self.assertLess(forward, ready)
        self.assertIn("sleep 1", run)
        fail_closed = run.index('if [ -z "$service_ip" ]; then')
        self.assertLess(fail_closed, pin)
        self.assertLess(run.index("exit 1", fail_closed), pin)
        self.assertIn("[[ \"$service_ip\" =~ ^([0-9]{1,3}\\.){3}[0-9]{1,3}$ ]]", run)
        self.assertIn('echo "agent-testdb did not resolve to an IPv4 address"', run)

    def test_rust_test_postgres_wal_budget_is_bounded(self):
        job = self.workflows["check.yml"]["jobs"]["rust-tests"]
        prepare = next(
            step for step in job["steps"]
            if step.get("name") == "Prepare isolated test databases"
        )
        verify = next(
            step for step in job["steps"]
            if step.get("name") == "Verify disposable Postgres defaults"
        )
        self.assertIn("ALTER SYSTEM SET max_wal_size = '512MB'", prepare["run"])
        self.assertIn("ALTER SYSTEM SET min_wal_size = '64MB'", prepare["run"])
        self.assertIn("pg_reload_conf()", prepare["run"])
        self.assertIn("'max_wal_size': '512MB'", verify["run"])
        self.assertIn("'min_wal_size': '64MB'", verify["run"])

    def test_db_lane_postgres_wal_budget_is_bounded(self):
        # Every 1 GiB tmpfs lane needs the rust-tests WAL bound: unbounded WAL
        # plus disposable-database data peaks exhaust the service disk late in
        # the ignored-db-stores job (main runs 39149193652 and 39197217315 both
        # PANIC'd with No space left on device in internal_member_store). The
        # required nextest lane runs the same DB-backed suites on the same 1
        # GiB tmpfs service via the reusable workflow, so it is the same
        # defect, not a separate one. Lanes are derived from the tmpfs
        # services rather than enumerated, so a new 1 GiB lane fails closed
        # until its bound is pinned here.
        lanes = []
        for workflow_name, workflow in self.workflows.items():
            for job_id, job in workflow.get("jobs", {}).items():
                services = job.get("services") or {}
                options = " ".join(
                    str(service.get("options", ""))
                    for service in services.values()
                    if isinstance(service, dict)
                )
                if "size=1073741824" in options:
                    lanes.append((workflow_name, job_id))
        # Sentinel: an enumeration that silently found nothing would pass
        # every subtest vacuously.
        self.assertEqual(
            sorted(lanes),
            sorted([
                ("check.yml", "rust-tests"),
                ("check.yml", "ignored-db-stores"),
                ("check.yml", "ignored-db-runtime"),
                ("reusable-rust-nextest.yml", "nextest"),
            ]),
        )
        for workflow_name, lane in lanes:
            with self.subTest(lane=f"{workflow_name}:{lane}"):
                job = self.workflows[workflow_name]["jobs"][lane]
                prepare = next(
                    step for step in job["steps"]
                    if step.get("name") == "Prepare isolated test databases"
                )
                verify = next(
                    step for step in job["steps"]
                    if step.get("name") == "Verify disposable Postgres defaults"
                )
                self.assertIn("ALTER SYSTEM SET max_wal_size = '512MB'", prepare["run"])
                self.assertIn("ALTER SYSTEM SET min_wal_size = '64MB'", prepare["run"])
                self.assertIn("pg_reload_conf()", prepare["run"])
                self.assertIn("'max_wal_size': '512MB'", verify["run"])
                self.assertIn("'min_wal_size': '64MB'", verify["run"])

    def test_patch_coverage_runs_with_db_and_includes_ignored_tests(self):
        job = self.workflows["check.yml"]["jobs"]["rust-tests"]
        step = next(
            step for step in job["steps"]
            if step.get("name") == "Patch coverage (HARD, 80% of added lines)"
        )
        expected_base = "${{ github.event.merge_group.base_sha || (github.event.pull_request && format('{0}^1', github.sha)) || github.event.before || 'origin/main' }}"
        self.assertEqual(step["env"]["QUALITY_BASE"], expected_base)
        self.assertEqual(step["env"]["QUALITY_HEAD"], "${{ github.sha }}")
        self.assertIn("--ignore-run-fail", step["run"])
        self.assertIn("--features two-bot-core/db", step["run"])
        self.assertIn("--lib --bins --tests", step["run"])
        self.assertIn(
            "--no-report -- --ignored --test-threads=1 --skip regenerate_reference_docs",
            step["run"],
        )
        self.assertIn(
            "--locked -- channel_moderation --include-ignored --test-threads=1",
            step["run"],
        )
        self.assertIn('report_path="$GITHUB_WORKSPACE/lcov.info"', step["run"])
        self.assertIn('[ ! -s "$report_path" ]', step["run"])
        self.assertIn(
            'llvm-cov report --workspace --lcov --output-path "$report_path"',
            step["run"],
        )
        self.assertIn('retrying via stdout', step["run"])
        self.assertIn(
            'llvm-cov report --workspace --lcov > "$report_path"', step["run"]
        )
        self.assertNotIn("/dev/stdout", step["run"])
        # `report` without --workspace scopes to the current (root) package
        # only and excludes every crates/* member directory from the export;
        # the root library is a single const with no coverable lines, so the
        # merged report comes out empty. Every report invocation must carry
        # --workspace; the bare form must never reappear.
        self.assertNotIn("llvm-cov report --lcov", step["run"])
        # Guarded suites are measured per suite with their accepted URL and
        # merged: --no-report per run, one final report. Selected ignored runs
        # are serial and never execute the reference-docs regen.
        self.assertIn("--no-report", step["run"])
        self.assertIn("llvm-cov report --workspace --lcov", step["run"])
        # Non-ignored tests stay parallel; only the ignored sweep is serial.
        self.assertIn("-- --ignored --test-threads=1", step["run"])
        self.assertIn("--test-threads=1", step["run"])
        self.assertIn("--skip regenerate_reference_docs", step["run"])
        self.assertIn(
            "TWO_TEST_DATABASE_URL=postgres://agent_test@agent-testdb:5432/agent_test",
            step["run"],
        )
        self.assertIn(
            "TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/agent_test",
            step["run"],
        )
        self.assertIn("MEMBER_TESTDB=agent-testdb", step["run"])
        for target in (
            "channel_moderation_runtime",
            "internal_channel_moderation",
            "feeds_store",
            "legacy_verify_db",
            "staging_migrate_db",
            "internal_events",
            "raid_list_db",
            "reengagement_list_db",
            "member_moderation_db",
        ):
            with self.subTest(target=target):
                self.assertIn(target, step["run"])
        # Per-invocation pins: whole-script substring checks let one surviving
        # occurrence mask a flag dropped from another invocation (e.g.
        # --ignore-run-fail removed from either sweep). Join line
        # continuations, then assert flags on each cov_run separately.
        logical_lines = []
        pending = ""
        for raw_line in step["run"].splitlines():
            stripped = raw_line.strip()
            pending = f"{pending} {stripped}".strip() if pending else stripped
            if stripped.endswith("\\"):
                pending = pending[:-1].strip()
                continue
            logical_lines.append(pending)
            pending = ""
        if pending:
            logical_lines.append(pending)
        cov_runs = [line for line in logical_lines if line.startswith("cov_run ")]
        # cov_run's failure annotation must go to stderr: the stdout report
        # retry redirects cov_run's stdout into the lcov file.
        self.assertIn(
            'echo "::error::patch coverage measurement failed to build or report" >&2',
            step["run"],
        )
        sweeps = [run for run in cov_runs if "--workspace --lib --bins --tests" in run]
        self.assertEqual(len(sweeps), 2)
        for sweep in sweeps:
            with self.subTest(sweep=sweep):
                # A failing test must never suppress coverage collection.
                self.assertIn("--ignore-run-fail", sweep)
                self.assertIn("--no-report", sweep)
                self.assertIn("--features two-bot-core/db", sweep)
                self.assertIn("--locked", sweep)
        ignored_sweeps = [run for run in sweeps if "-- --ignored" in run]
        self.assertEqual(len(ignored_sweeps), 1)
        self.assertIn("--test-threads=1", ignored_sweeps[0])
        self.assertIn("--skip regenerate_reference_docs", ignored_sweeps[0])
        self.assertEqual(len([run for run in sweeps if "-- --ignored" not in run]), 1)
        main_sweep = [run for run in sweeps if "-- --ignored" not in run][0]
        # Guarded suites panic under the bootstrap URL; they run only in
        # their dedicated agent_test steps below, never in the main sweep.
        self.assertIn(
            "--skip cli_write_failure_and_crash_never_publish_prune_or_upload_a_partial_dump",
            main_sweep,
        )
        self.assertIn(
            "--skip seeded_aggregates_render_exactly_guild_scoped_and_read_only",
            main_sweep,
        )
        guarded = [run for run in cov_runs if "llvm-cov --no-report -p " in run]
        self.assertEqual(len(guarded), 12)
        for run in guarded:
            with self.subTest(guarded=run):
                self.assertIn("--test-threads=1", run)
                self.assertIn("--skip regenerate_reference_docs", run)
                self.assertIn("--locked", run)
                # Deliberate: a guard panic in a guarded suite fails the step.
                self.assertNotIn("--ignore-run-fail", run)
        # needle -> carries `--features db` (core/discord suites need the db
        # feature to build their guarded targets; cutover suites do not).
        guarded_needles = {
            "-- channel_moderation --include-ignored": True,
            "--test channel_moderation_runtime": True,
            "--test internal_channel_moderation": True,
            "--test feeds_store": True,
            "--test legacy_verify_db": False,
            "--test legacy_tls_fence": False,
            "--test staging_migrate_db": False,
            "--test internal_events": True,
            "--test raid_list_db": False,
            "--test reengagement_list_db": False,
            "--test game_activity_report_db": False,
            "--test member_moderation_db": True,
        }
        for needle, wants_db_feature in guarded_needles.items():
            with self.subTest(needle=needle):
                matches = [run for run in guarded if needle in run]
                self.assertEqual(len(matches), 1)
                self.assertIn("--test-threads=1", matches[0])
                if wants_db_feature:
                    self.assertIn("--features db", matches[0])
                else:
                    self.assertNotIn("--features db", matches[0])
        env_wrapped = [
            run for run in cov_runs
            if run.startswith('cov_run env -u PGPASSWORD -u PGPASSFILE -u PGOPTIONS "$COV"')
        ]
        self.assertEqual(len(env_wrapped), 1)
        self.assertIn("--test channel_moderation_runtime", env_wrapped[0])
        reports = [run for run in cov_runs if "llvm-cov report" in run]
        self.assertEqual(len(reports), 2)
        for report in reports:
            with self.subTest(report=report):
                self.assertIn("llvm-cov report --workspace --lcov", report)
        self.assertEqual(
            step["env"]["TWO_TEST_DATABASE_URL"],
            "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci",
        )
        self.assertIn("agent-testdb", job["services"])
        self.assertEqual(
            step["if"],
            "always() && !cancelled() && steps.prepare_test_databases.outcome == 'success' "
            "&& steps.verify_disposable_postgres.outcome == 'success'",
        )
        test_steps = [
            index for index, candidate in enumerate(job["steps"])
            if "cargo test" in str(candidate.get("run", ""))
        ]
        self.assertTrue(test_steps)
        self.assertGreater(job["steps"].index(step), max(test_steps))
        self.assertEqual(
            next(candidate for candidate in job["steps"]
                 if candidate.get("name") == "Prepare isolated test databases")["id"],
            "prepare_test_databases",
        )
        self.assertEqual(
            next(candidate for candidate in job["steps"]
                 if candidate.get("name") == "Verify disposable Postgres defaults")["id"],
            "verify_disposable_postgres",
        )
        checkout = next(step for step in job["steps"] if step.get("uses", "").startswith("actions/checkout@"))
        self.assertEqual(checkout["with"]["fetch-depth"], "0")

    def test_voice_synthetic_shape_is_pinned_and_mutations_fail(self):
        import copy
        synthetic = self.workflows["voice-synthetic.yml"]
        self.assertEqual(voice_synthetic_errors(synthetic), [])
        for mutate in (
            lambda w: w["jobs"]["synthetic"].pop("if"),
            lambda w: w["jobs"]["synthetic"].__setitem__("environment", "staging"),
            lambda w: w["on"].__setitem__("push", None),
            lambda w: w["jobs"]["synthetic"]["steps"][1].__setitem__("run", "wrangler deploy"),
            lambda w: w["jobs"]["synthetic"]["steps"][0].__setitem__("env", {"T": "${{ secrets.TWO_VOICE_SYNTHETIC_BOT_TOKEN }}"}),
        ):
            changed = copy.deepcopy(synthetic)
            mutate(changed)
            self.assertNotEqual(voice_synthetic_errors(changed), [])

    def test_staging_deploy_gates_on_the_voice_synthetic(self):
        job = self.workflows["deploy-staging.yml"]["jobs"]["voice-synthetic"]
        self.assertEqual(job.get("needs"), "deploy")
        self.assertEqual(job.get("uses"), "./.github/workflows/voice-synthetic.yml")
        self.assertNotIn("if", job)
        # An environment secret is empty in a called workflow unless the caller
        # passes it; pass exactly the test bot token, never `inherit`.
        self.assertEqual(
            job.get("secrets"),
            {"TWO_VOICE_SYNTHETIC_BOT_TOKEN": "${{ secrets.TWO_VOICE_SYNTHETIC_BOT_TOKEN }}"},
        )

    def test_schema_gate_shape_is_pinned_and_mutations_fail(self):
        import copy
        staging = self.workflows["deploy-staging.yml"]
        self.assertEqual(schema_gate_errors(staging), [])
        for mutate in (
            lambda w: w["jobs"]["schema-gate"].__setitem__("environment", "staging-migrate-apply"),
            lambda w: w["jobs"]["schema-gate"].__setitem__("if", "github.ref == 'refs/heads/main'"),
            lambda w: w["jobs"]["schema-gate"].__setitem__("needs", "deploy"),
            lambda w: w["jobs"]["deploy"].pop("needs"),
            lambda w: w["jobs"]["schema-gate"]["steps"][3].__setitem__("run", "wrangler deploy"),
            lambda w: w["jobs"]["schema-gate"]["steps"][3].get("env", {}).__setitem__(
                "LEAK", "${{ secrets.TWO_BOT_STAGING_MIGRATOR_DATABASE_URL }}"),
        ):
            changed = copy.deepcopy(staging)
            mutate(changed)
            self.assertNotEqual(schema_gate_errors(changed), [])

    def test_staging_deploy_needs_schema_gate(self):
        staging = self.workflows["deploy-staging.yml"]
        self.assertEqual(schema_gate_errors(staging), [])
        deploy, gate = staging["jobs"]["deploy"], staging["jobs"]["schema-gate"]
        self.assertIn("schema-gate", [deploy["needs"]] if isinstance(deploy["needs"], str) else deploy["needs"])
        self.assertEqual(gate.get("environment"), "staging-migrate-plan")
        self.assertNotIn("if", gate)
        self.assertNotIn("needs", gate)
        # The chain stays ordered: gate first, deploy second, synthetic last.
        self.assertEqual(staging["jobs"]["voice-synthetic"].get("needs"), "deploy")

    def test_roles_gate_shape_is_pinned_and_mutations_fail(self):
        import copy
        staging = self.workflows["deploy-staging.yml"]
        self.assertEqual(roles_gate_errors(staging), [])
        for mutate in (
            lambda w: w["jobs"]["roles-gate"].__setitem__("environment", "staging-migrate-apply"),
            lambda w: w["jobs"]["roles-gate"].__setitem__("if", "github.ref == 'refs/heads/main'"),
            lambda w: w["jobs"]["roles-gate"].__setitem__("needs", "deploy"),
            lambda w: w["jobs"]["deploy"].pop("needs"),
            lambda w: w["jobs"]["roles-gate"]["steps"][3].__setitem__("run", "wrangler deploy"),
            lambda w: w["jobs"]["roles-gate"]["steps"][3].get("env", {}).__setitem__(
                "LEAK", "${{ secrets.TWO_BOT_STAGING_MIGRATOR_DATABASE_URL }}"),
            lambda w: w["jobs"]["roles-gate"]["steps"][3].__setitem__("if", "${{ false }}"),
            lambda w: w["jobs"]["roles-gate"].__setitem__("if", "${{ false }}"),
            lambda w: w["jobs"]["roles-gate"]["steps"][3].__setitem__("continue-on-error", True),
            lambda w: w["jobs"]["roles-gate"].__setitem__("continue-on-error", True),
            lambda w: w["jobs"]["roles-gate"]["steps"][3].__setitem__(
                "run", w["jobs"]["roles-gate"]["steps"][3]["run"] + " || true"),
        ):
            changed = copy.deepcopy(staging)
            mutate(changed)
            self.assertNotEqual(roles_gate_errors(changed), [])

    def test_staging_deploy_needs_roles_gate(self):
        staging = self.workflows["deploy-staging.yml"]
        self.assertEqual(roles_gate_errors(staging), [])
        deploy, gate = staging["jobs"]["deploy"], staging["jobs"]["roles-gate"]
        needs = [deploy["needs"]] if isinstance(deploy["needs"], str) else deploy["needs"]
        self.assertIn("schema-gate", needs)
        self.assertIn("roles-gate", needs)
        self.assertEqual(gate.get("environment"), "staging-migrate-plan")
        self.assertNotIn("if", gate)
        self.assertNotIn("needs", gate)
        # The chain stays ordered: gates first, deploy second, synthetic last.
        self.assertEqual(staging["jobs"]["voice-synthetic"].get("needs"), "deploy")

    def test_staging_takeover_fence_hint_is_scoped_to_the_fenced_refusal(self):
        # Execute the actual workflow shell with a stub control client. This
        # catches swapped case bodies as well as an unscoped fence hint.
        import os
        import subprocess
        import tempfile

        steps = self.workflows["deploy-staging.yml"]["jobs"]["deploy"]["steps"]
        takeover = next(step for step in steps if step.get("id") == "takeover")
        run = takeover["run"]
        self.assertIn("deployment-takeover", run)
        self.assertIn("Singleton is intentionally fenced or uninitialized", run)
        self.assertIn("Staging singleton fenced", run)
        self.assertIn("not a fence refusal", run)
        with tempfile.TemporaryDirectory() as temp_dir:
            temp = Path(temp_dir)
            rollout_dir = temp / "rollout"
            rollout_dir.mkdir()
            (rollout_dir / "receipt.json").write_text('{"worker_version":"test-version"}', encoding="utf-8")
            bin_dir = temp / "bin"
            bin_dir.mkdir()
            node = bin_dir / "node"
            node.write_text('#!/bin/sh\nprintf \'%s\\n\' "$MOCK_CONTROL_OUTPUT"\nexit "${MOCK_CONTROL_EXIT:-1}"\n', encoding="utf-8")
            node.chmod(0o755)
            bash_env = temp / "bash-env"
            bash_env.write_text(f'export PATH="{bin_dir}:$PATH"\n', encoding="utf-8")
            base_env = dict(os.environ, ROLLOUT_DIR=str(rollout_dir),
                            PATH=f"{bin_dir}:{os.environ.get('PATH', '')}", BASH_ENV=str(bash_env))
            shell = run
            cases = (
                ("Singleton is intentionally fenced or uninitialized; explicit staging release required",
                 "Staging singleton fenced", True, 1),
                ("HTTP 401 Unauthorized", "Staging ownership takeover failed (not a fence refusal)", False, 1),
                ("", "Staging ownership takeover timed out after 390 seconds", False, 124),
            )
            for output, expected, is_fenced, exit_code in cases:
                with self.subTest(output=output, exit_code=exit_code):
                    env = dict(base_env, MOCK_CONTROL_OUTPUT=output, MOCK_CONTROL_EXIT=str(exit_code))
                    proc = subprocess.run(
                        ["bash", "-c", shell], cwd=ROOT / "wrangler", env=env,
                        capture_output=True, text=True, timeout=10,
                    )
                    output_text = proc.stdout + proc.stderr
                    self.assertEqual(proc.returncode, 1, output_text)
                    self.assertIn(expected, output_text)
                    self.assertEqual("release_fence" in output_text, is_fenced)

    def test_staging_takeover_outer_timeout_exceeds_the_client_retry_window(self):
        # The workflow's `timeout` must never kill the control client inside
        # its own retry window: a final attempt may start at the window edge
        # and still spend one GET plus one POST, each bounded by the client's
        # AbortSignal. Killing it there could terminate it after the POST
        # committed an epoch and roll back a healthy deploy. Pin both sides:
        # above window + one in-flight attempt, yet close enough that a true
        # hang still fails fast instead of fencing staging.
        steps = self.workflows["deploy-staging.yml"]["jobs"]["deploy"]["steps"]
        run = next(step for step in steps if step.get("id") == "takeover")["run"]
        outer = re.search(r"timeout --signal=TERM --kill-after=\d+s (\d+)s", run)
        self.assertIsNotNone(outer, run)
        client = (ROOT / "wrangler/scripts/ownership-control.mjs").read_text(encoding="utf-8")
        window = re.search(r"takeoverWindowMs = (\d+)", client)
        abort = re.search(r"AbortSignal\.timeout\((\d+)\)", client)
        self.assertIsNotNone(window)
        self.assertIsNotNone(abort)
        outer_ms = int(outer.group(1)) * 1000
        minimum_ms = int(window.group(1)) + 2 * int(abort.group(1))
        self.assertGreater(outer_ms, minimum_ms)
        self.assertLess(outer_ms, minimum_ms + 120_000)
        self.assertIn(f"timed out after {outer.group(1)} seconds", run)

    def test_staging_runs_unconditionally_for_push_and_dispatch(self):
        staging = self.workflows["deploy-staging.yml"]
        self.assertEqual(staging_dispatch_errors(staging), [])
        self.assertEqual(staging_active_errors(staging), [])
        self.assertEqual(schema_gate_errors(staging), [])
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

    def test_gateway_failure_injection_is_dispatch_only_and_never_forwarded(self):
        workflows = self.workflows
        self.assertEqual(staging_gateway_injection_errors(workflows), [])
        self.assertEqual(staging_dispatch_errors(workflows["deploy-staging.yml"]), [])
        steps = workflows["deploy-staging.yml"]["jobs"]["deploy"]["steps"]
        armed = [s for s in steps if "INJECT_GATEWAY_FAILURE" in str(s)]
        self.assertEqual(len(armed), 1)
        index = steps.index(armed[0])
        mutations = {
            # Push could read it / default armed.
            "expression without dispatch guard": lambda w: w["jobs"]["deploy"]["steps"][index]["env"]
                .__setitem__("INJECT_GATEWAY_FAILURE", "${{ inputs.inject_gateway_failure }}"),
            "expression constant true": lambda w: w["jobs"]["deploy"]["steps"][index]["env"]
                .__setitem__("INJECT_GATEWAY_FAILURE", "true"),
            "prepare flag unconditional": lambda w: w["jobs"]["deploy"]["steps"][index]
                .__setitem__("run", "python3 ../scripts/staging_rollout.py prepare --inject-gateway-failure"),
            "input defaults true": lambda w: w["on"]["workflow_dispatch"]["inputs"]["inject_gateway_failure"]
                .__setitem__("default", True),
            "input removed": lambda w: w["on"]["workflow_dispatch"]["inputs"].pop("inject_gateway_failure"),
            "second reader step": lambda w: w["jobs"]["deploy"]["steps"][0]
                .setdefault("env", {}).__setitem__("INJECT_GATEWAY_FAILURE", "true"),
        }
        for label, mutate in mutations.items():
            with self.subTest(label):
                changed = deepcopy(workflows)
                mutate(changed["deploy-staging.yml"])
                self.assertTrue(
                    staging_gateway_injection_errors(changed)
                    or staging_dispatch_errors(changed["deploy-staging.yml"]),
                    label,
                )
                self.assertTrue(workflow_policy_errors(changed), label)
        # Production (or any other workflow) mentioning the switch at all
        # is a forwarding route, whatever the surrounding shape.
        for name in ("deploy-production.yml", "check.yml"):
            with self.subTest(forwarded=name):
                changed = deepcopy(workflows)
                job = next(iter(changed[name]["jobs"].values()))
                job.setdefault("env", {})["FORWARD"] = "${{ inputs.inject_gateway_failure }}"
                self.assertTrue(staging_gateway_injection_errors(changed))
                self.assertTrue(workflow_policy_errors(changed))

    def test_error_rate_gate_is_dispatch_gated_and_never_forwarded(self):
        workflows = self.workflows
        self.assertEqual(staging_error_rate_gate_errors(workflows), [])
        self.assertEqual(staging_dispatch_errors(workflows["deploy-staging.yml"]), [])
        steps = workflows["deploy-staging.yml"]["jobs"]["deploy"]["steps"]
        armed = [s for s in steps if "INJECT_ERROR_RATE_SPIKE" in str(s)]
        self.assertEqual(len(armed), 1)
        self.assertEqual(armed[0].get("id"), "error_rate_gate")
        index = steps.index(armed[0])
        mutations = {
            # Push could read it / default armed.
            "expression without dispatch guard": lambda w: w["jobs"]["deploy"]["steps"][index]["env"]
                .__setitem__("INJECT_ERROR_RATE_SPIKE", "${{ inputs.inject_error_rate_spike }}"),
            "expression constant true": lambda w: w["jobs"]["deploy"]["steps"][index]["env"]
                .__setitem__("INJECT_ERROR_RATE_SPIKE", "true"),
            "rehearsal flag unconditional": lambda w: w["jobs"]["deploy"]["steps"][index]
                .__setitem__("run", "node scripts/promotion-error-gate.mjs --rehearsal-firing '[\"rest_5xx_rate\"]'"),
            "input defaults true": lambda w: w["on"]["workflow_dispatch"]["inputs"]["inject_error_rate_spike"]
                .__setitem__("default", True),
            "input removed": lambda w: w["on"]["workflow_dispatch"]["inputs"].pop("inject_error_rate_spike"),
            "second reader step": lambda w: w["jobs"]["deploy"]["steps"][0]
                .setdefault("env", {}).__setitem__("INJECT_ERROR_RATE_SPIKE", "true"),
            "switch off the gate step": lambda w: w["jobs"]["deploy"]["steps"][index]
                .__setitem__("id", "unrelated"),
        }
        for label, mutate in mutations.items():
            with self.subTest(label):
                changed = deepcopy(workflows)
                mutate(changed["deploy-staging.yml"])
                self.assertTrue(
                    staging_error_rate_gate_errors(changed)
                    or staging_dispatch_errors(changed["deploy-staging.yml"]),
                    label,
                )
                self.assertTrue(workflow_policy_errors(changed), label)
        # Production (or any other workflow) mentioning the gate at all
        # is a forwarding route, whatever the surrounding shape.
        for name in ("deploy-production.yml", "check.yml"):
            with self.subTest(forwarded=name):
                changed = deepcopy(workflows)
                job = next(iter(changed[name]["jobs"].values()))
                job.setdefault("env", {})["FORWARD"] = "${{ inputs.inject_error_rate_spike }}"
                self.assertTrue(staging_error_rate_gate_errors(changed))
                self.assertTrue(workflow_policy_errors(changed))

    def test_remediation_fault_injection_is_dispatch_only_and_never_forwarded(self):
        workflows = self.workflows
        self.assertEqual(staging_fault_injection_errors(workflows), [])
        self.assertEqual(staging_dispatch_errors(workflows["deploy-staging.yml"]), [])
        steps = workflows["deploy-staging.yml"]["jobs"]["deploy"]["steps"]
        armed = [s for s in steps if "INJECT_VOICE_STALL" in str(s)]
        self.assertEqual(len(armed), 1)
        index = steps.index(armed[0])
        mutations = {
            # Push could read it / default armed.
            "expression without dispatch guard": lambda w: w["jobs"]["deploy"]["steps"][index]["env"]
                .__setitem__("INJECT_VOICE_STALL", "${{ inputs.inject_voice_stall }}"),
            "expression constant true": lambda w: w["jobs"]["deploy"]["steps"][index]["env"]
                .__setitem__("INJECT_UNPAIRED_DISCONNECT", "true"),
            "prepare flag unconditional": lambda w: w["jobs"]["deploy"]["steps"][index]
                .__setitem__("run", "python3 ../scripts/staging_rollout.py prepare --inject-voice-stall"),
            "input defaults true": lambda w: w["on"]["workflow_dispatch"]["inputs"]["inject_voice_stall"]
                .__setitem__("default", True),
            "input removed": lambda w: w["on"]["workflow_dispatch"]["inputs"].pop("inject_unpaired_disconnect"),
            "second reader step": lambda w: w["jobs"]["deploy"]["steps"][0]
                .setdefault("env", {}).__setitem__("INJECT_VOICE_STALL", "true"),
        }
        for label, mutate in mutations.items():
            with self.subTest(label):
                changed = deepcopy(workflows)
                mutate(changed["deploy-staging.yml"])
                self.assertTrue(
                    staging_fault_injection_errors(changed)
                    or staging_dispatch_errors(changed["deploy-staging.yml"]),
                    label,
                )
                self.assertTrue(workflow_policy_errors(changed), label)
        # Production (or any other workflow) mentioning any fault marker at
        # all is a forwarding route, whatever the surrounding shape.
        for name in ("deploy-production.yml", "check.yml"):
            with self.subTest(forwarded=name):
                changed = deepcopy(workflows)
                job = next(iter(changed[name]["jobs"].values()))
                job.setdefault("env", {})["FORWARD"] = "${{ inputs.inject_voice_stall }}"
                self.assertTrue(staging_fault_injection_errors(changed))
                self.assertTrue(workflow_policy_errors(changed))

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
        for pin in ('status="success"', "deploy-staging.yml/runs", '"required_reviewers"',
                      '"User-Agent": "two-bot-next-production-rollout/1.0"',
                      "urllib.request.Request("):
            with self.subTest(dropped=pin):
                def drop(w, pin=pin):
                    for step in w["jobs"]["guard"]["steps"]:
                        if "run" in step:
                            step["run"] = step["run"].replace(pin, "")
                self.assertTrue(mutated(drop))

    def test_production_schema_gate_shape_is_pinned_and_mutations_fail(self):
        import copy
        production = self.workflows["deploy-production.yml"]
        self.assertEqual(production_schema_gate_errors(production), [])
        self.assertEqual(production_errors(production), [])
        for mutate in (
            lambda w: w["jobs"]["schema-gate"].__setitem__("environment", "production-migrate-apply"),
            lambda w: w["jobs"]["schema-gate"].__setitem__("if", "github.ref == 'refs/heads/main'"),
            lambda w: w["jobs"]["schema-gate"].__setitem__("needs", "production"),
            lambda w: w["jobs"]["production"].__setitem__("needs", "guard"),
            lambda w: w["jobs"]["production"].__setitem__("needs", ["schema-gate"]),
            lambda w: w["jobs"]["schema-gate"]["steps"][4].__setitem__("run", "wrangler deploy"),
            lambda w: w["jobs"]["schema-gate"]["steps"][4].get("env", {}).__setitem__(
                "LEAK", "${{ secrets.TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL }}"),
            lambda w: w["jobs"]["schema-gate"]["steps"][4].get("env", {}).__setitem__(
                "TWO_BOT_PRODUCTION_BRANCH_ID", ""),
        ):
            changed = copy.deepcopy(production)
            mutate(changed)
            self.assertNotEqual(production_schema_gate_errors(changed), [])

    def test_production_deploy_needs_prod_schema_gate(self):
        production = self.workflows["deploy-production.yml"]
        self.assertEqual(production_schema_gate_errors(production), [])
        needs = production["jobs"]["production"]["needs"]
        needs = [needs] if isinstance(needs, str) else needs
        self.assertIn("guard", needs)
        self.assertIn("schema-gate", needs)
        self.assertEqual(production["jobs"]["schema-gate"].get("needs"), "guard")
        self.assertEqual(production["jobs"]["schema-gate"].get("environment"),
                         "production-migrate-plan")
        self.assertEqual(production["jobs"]["schema-gate"].get("if"),
                         "needs.guard.outputs.mode == 'deploy' && needs.guard.outputs.allow_unstaged != 'true'")

    def test_production_roles_gate_shape_is_pinned_and_mutations_fail(self):
        import copy
        production = self.workflows["deploy-production.yml"]
        self.assertEqual(production_roles_gate_errors(production), [])
        self.assertEqual(production_errors(production), [])
        for mutate in (
            lambda w: w["jobs"]["roles-gate"].__setitem__("environment", "production-migrate-apply"),
            lambda w: w["jobs"]["roles-gate"].__setitem__("if", "github.ref == 'refs/heads/main'"),
            lambda w: w["jobs"]["roles-gate"].__setitem__("needs", "production"),
            lambda w: w["jobs"]["production"].__setitem__("needs", ["guard", "schema-gate"]),
            lambda w: w["jobs"]["roles-gate"]["steps"][4].__setitem__("run", "wrangler deploy"),
            lambda w: w["jobs"]["roles-gate"]["steps"][4].get("env", {}).__setitem__(
                "LEAK", "${{ secrets.TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL }}"),
            lambda w: w["jobs"]["roles-gate"]["steps"][4].get("env", {}).__setitem__(
                "TWO_BOT_PRODUCTION_BRANCH_ID", ""),
            lambda w: w["jobs"]["roles-gate"]["steps"][4].__setitem__("if", "${{ false }}"),
            lambda w: w["jobs"]["roles-gate"].__setitem__("if", "${{ false }}"),
            lambda w: w["jobs"]["roles-gate"]["steps"][4].__setitem__("continue-on-error", True),
            lambda w: w["jobs"]["roles-gate"].__setitem__("continue-on-error", True),
            lambda w: w["jobs"]["roles-gate"]["steps"][4].__setitem__(
                "run", w["jobs"]["roles-gate"]["steps"][4]["run"] + " || true"),
            lambda w: w["jobs"]["roles-gate"]["steps"][4].get("env", {}).__setitem__(
                "MODE", "rollback"),
            lambda w: w["jobs"]["roles-gate"]["steps"][4].get("env", {}).__setitem__(
                "ALLOW_UNSTAGED", "true"),
        ):
            changed = copy.deepcopy(production)
            mutate(changed)
            self.assertNotEqual(production_roles_gate_errors(changed), [])

    def test_production_deploy_needs_prod_roles_gate(self):
        production = self.workflows["deploy-production.yml"]
        self.assertEqual(production_roles_gate_errors(production), [])
        needs = production["jobs"]["production"]["needs"]
        needs = [needs] if isinstance(needs, str) else needs
        self.assertIn("guard", needs)
        self.assertIn("schema-gate", needs)
        self.assertIn("roles-gate", needs)
        self.assertEqual(production["jobs"]["roles-gate"].get("needs"), "guard")
        self.assertEqual(production["jobs"]["roles-gate"].get("environment"),
                         "production-migrate-plan")
        self.assertEqual(production["jobs"]["roles-gate"].get("if"),
                         "needs.guard.outputs.mode == 'deploy' && needs.guard.outputs.allow_unstaged != 'true'")

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
                        "production_host", "production_database",
                        "production_branch_id"):
            with self.subTest(missing=missing):
                def drop(w, missing=missing):
                    del w["on"]["workflow_dispatch"]["inputs"][missing]
                self.assertTrue(mutated(drop))
        # Staging inputs must not appear on the production workflow.
        for staging_input in ("staging_host", "staging_database", "staging_branch_id"):
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
        for wrong in ("--target staging", "--staging-host", "--staging-database",
                      "--staging-branch-id"):
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

    def test_staging_ownership_status_shape_is_pinned(self):
        workflows = self.workflows
        self.assertEqual(staging_ownership_status_errors(workflows["staging-ownership-status.yml"]), [])

        def mutate(change):
            copy = deepcopy(workflows["staging-ownership-status.yml"])
            change(copy)
            return staging_ownership_status_errors(copy)

        def job(copy):
            return copy["jobs"]["status"]

        changes = {
            "push trigger": lambda w: w["on"].update({"push": {"branches": ["main"]}}),
            "schedule trigger": lambda w: w["on"].update({"schedule": [{"cron": "0 * * * *"}]}),
            "with inputs": lambda w: w["on"].update({"workflow_dispatch": {"inputs": {"force": {"type": "boolean"}}}}),
            "top-level permissions": lambda w: w.update({"permissions": {"contents": "read"}}),
            "workflow-level secret": lambda w: w.update(
                {"env": {"T": "${{ secrets.STAGING_OWNERSHIP_CONTROL_TOKEN }}"}}),
            "cancelling concurrency": lambda w: w["concurrency"].update({"cancel-in-progress": "true"}),
            "shared concurrency group": lambda w: w["concurrency"].update({"group": "deploy-staging"}),
            "second job": lambda w: w["jobs"].update({"again": deepcopy(job(w))}),
            "wrong environment": lambda w: job(w).update({"environment": "staging-events-read"}),
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
                {"env": {"T": "${{ secrets.STAGING_OWNERSHIP_CONTROL_TOKEN }}"}}),
            "step condition": lambda w: job(w)["steps"][3].update({"if": "always()"}),
            "continue on error": lambda w: job(w)["steps"][3].update({"continue-on-error": "true"}),
            "extra step": lambda w: job(w)["steps"].append({"run": "echo done"}),
            "persisted credentials": lambda w: job(w)["steps"][0]["with"].update(
                {"persist-credentials": "true"}),
            "extra secret": lambda w: job(w)["steps"][3]["env"].update(
                {"X": "${{ secrets.CLOUDFLARE_API_TOKEN }}"}),
            "dropped token": lambda w: job(w)["steps"][3]["env"].pop("OWNERSHIP_CONTROL_TOKEN"),
            "dropped worker url": lambda w: job(w)["steps"][3]["env"].pop("STAGING_WORKER_URL"),
            "blanket secrets": lambda w: job(w)["steps"][3]["env"].update({"ALL": "${{ toJSON(secrets) }}"}),
            "inline expression": lambda w: job(w)["steps"][3].update(
                {"run": 'node scripts/staging-ownership-status.mjs "${{ vars.STAGING_WORKER_URL }}"'}),
            "shell trace": lambda w: job(w)["steps"][3].update(
                {"run": "set -x\nnode scripts/staging-ownership-status.mjs"}),
            "other script": lambda w: job(w)["steps"][3].update(
                {"run": "node scripts/other.mjs"}),
            "takeover step": lambda w: job(w)["steps"][3].update(
                {"run": "node scripts/ownership-control.mjs deployment-takeover"}),
            "wrangler action": lambda w: job(w)["steps"].insert(
                2, {"uses": "cloudflare/wrangler-action@pinned"}),
        }
        for label, change in changes.items():
            with self.subTest(change=label):
                self.assertTrue(mutate(change))
        # The inventory pin fails closed if the workflow disappears or gains a job.
        missing = deepcopy(workflows)
        del missing["staging-ownership-status.yml"]
        self.assertTrue(workflow_policy_errors(missing))
        self.assertEqual(workflow_policy_errors(workflows), [])

    def test_gradual_promote_proof_shape_is_pinned(self):
        workflows = self.workflows
        self.assertEqual(gradual_promote_proof_errors(workflows["gradual-promote-proof.yml"]), [])

        def mutate(change):
            copy = deepcopy(workflows["gradual-promote-proof.yml"])
            change(copy)
            return gradual_promote_proof_errors(copy)

        def job(copy):
            return copy["jobs"]["proof"]

        changes = {
            "push trigger": lambda w: w["on"].update({"push": {"branches": ["main"]}}),
            "rollback-only mode": lambda w: w["on"]["workflow_dispatch"]["inputs"]["mode"].update(
                {"options": ["rollback"]}),
            "mode default rollback": lambda w: w["on"]["workflow_dispatch"]["inputs"]["mode"].update(
                {"default": "rollback"}),
            "dropped domain input": lambda w: w["on"]["workflow_dispatch"]["inputs"].pop("domain"),
            "extra input": lambda w: w["on"]["workflow_dispatch"]["inputs"].update(
                {"extra": {"type": "string", "required": "false"}}),
            "confirm default": lambda w: w["on"]["workflow_dispatch"]["inputs"]["confirm_worker"].update(
                {"default": "two-bot-next-gradual-proof"}),
            "top-level permissions": lambda w: w.update({"permissions": {"contents": "read"}}),
            "cancelling concurrency": lambda w: w["concurrency"].update({"cancel-in-progress": "true"}),
            "shared concurrency group": lambda w: w["concurrency"].update({"group": "deploy-staging"}),
            "second job": lambda w: w["jobs"].update({"again": deepcopy(job(w))}),
            "production environment": lambda w: job(w).update({"environment": "production"}),
            "no environment": lambda w: job(w).pop("environment"),
            "branch condition removed": lambda w: job(w).pop("if"),
            "write grant": lambda w: job(w).update({"permissions": {"contents": "write"}}),
            "longer timeout": lambda w: job(w).update({"timeout-minutes": "60"}),
            "unrouted runner": lambda w: job(w).update({"runs-on": "ubuntu-latest"}),
            "other job's routing": lambda w: job(w).update(
                {"runs-on": ROUTED_RUNNER.format(job="check")}),
            "repinned action": lambda w: job(w)["steps"][3].update(
                {"uses": "TogetherWeOwn/.github/actions/gradual-promote@main"}),
            "dropped error-rate threshold": lambda w: job(w)["steps"][3]["with"].pop("error-rate-threshold"),
            "widened allow-indexable": lambda w: job(w)["steps"][3]["with"].update({"allow-indexable": "true"}),
            "production secret": lambda w: job(w)["steps"][3]["with"].update(
                {"api-token": "${{ secrets.PRODUCTION_CLOUDFLARE_API_TOKEN }}"}),
            "inline expression": lambda w: job(w)["steps"][2].update(
                {"run": 'echo "${{ inputs.confirm_worker }}"'}),
            "upload may be missing": lambda w: job(w)["steps"][4]["with"].update({"if-no-files-found": "error"}),
            "upload not always": lambda w: job(w)["steps"][4].pop("if"),
            "checkout conditional": lambda w: job(w)["steps"][0].update({"if": "always()"}),
            "guard conditional": lambda w: job(w)["steps"][2].update({"if": "always()"}),
            "gradual continue-on-error": lambda w: job(w)["steps"][3].update({"continue-on-error": "true"}),
            "guard without length gate": lambda w: job(w)["steps"][2].update(
                {"run": job(w)["steps"][2]["run"].replace("${#CANDIDATE_TAG}", "CANDIDATE_TAG")}),
            "guard without single-label rejection": lambda w: job(w)["steps"][2].update(
                {"run": job(w)["steps"][2]["run"].replace("two-bot-next-gradual-proof.*.*.workers.dev",
                                                         "two-bot-next-gradual-proof-rejected")}),
        }
        for label, change in changes.items():
            with self.subTest(change=label):
                self.assertTrue(mutate(change))
        # The inventory pin fails closed if the workflow disappears or gains a job.
        missing = deepcopy(workflows)
        del missing["gradual-promote-proof.yml"]
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
            "fuzz-compile",
        })
        for job_id, steps in installers.items():
            with self.subTest(job=job_id):
                self.assertEqual(len(steps), 1)
                if job_id == "fuzz-compile":
                    # libFuzzer needs nightly; pinned date, not floating stable.
                    self.assertEqual(steps[0].get("with", {}).get("toolchain"), "nightly-2026-10-01")
                    self.assertEqual(set(steps[0]["with"]["components"].replace(" ", "").split(",")),
                                     {"rust-src"})
                    continue
                # Floating stable can install a different fmt/clippy than Cargo
                # selects from the repository pin inside the job container.
                self.assertEqual(steps[0].get("with", {}).get("toolchain"), channel)
        self.assertEqual(set(installers["check"][0]["with"]["components"].replace(" ", "").split(",")),
                         {"rustfmt", "clippy"})

    def pinned_toolchain_lanes(self, channel):
        # Every PR-gating job that installs the repository pin: all of
        # check.yml (the ci-ok aggregate) and the shared nextest lane it
        # calls. The nightly-pinned fuzz lane installs a different toolchain
        # and is excluded by the channel match.
        lanes = []
        for workflow_name in ("check.yml", "reusable-rust-nextest.yml"):
            for job_id, job in self.workflows[workflow_name]["jobs"].items():
                if any(str(step.get("uses", "")).startswith("dtolnay/rust-toolchain@")
                       and step.get("with", {}).get("toolchain") == channel
                       for step in job.get("steps", [])):
                    lanes.append((workflow_name, job_id))
        # Sentinels: an enumeration that silently found nothing would pass
        # every subtest vacuously.
        for lane in (("check.yml", "ignored-db-runtime"), ("reusable-rust-nextest.yml", "nextest")):
            self.assertIn(lane, lanes)
        self.assertNotIn(("check.yml", "fuzz-compile"), lanes)
        return lanes

    def test_rust_toolchain_installs_are_retry_guarded(self):
        # `rustup toolchain install` inside the installer failed once on main
        # with `Network is unreachable` syncing the channel manifest, failing
        # the job before any test ran (green on re-run). The upstream
        # installer retries only checksum-rotation failures, so the installer
        # continues on error and the next step retries the same pin and fails
        # closed. The retry must request the repository pin and every
        # component the installer requests, or a green retry can mask a drift.
        # Every PR-gating lane that installs the pin must carry the guard: a
        # 4-second install failure on ignored-db-runtime reddened PR #984
        # while the identical step passed on the other lanes of the same
        # head. The lanes are enumerated from the workflows, not listed, so a
        # new lane cannot install the pin unguarded. Scheduled and manual
        # workflows (nightly, benchmark, fuzz smoke) do not gate a PR and
        # stay out of scope.
        channel = tomllib.loads((ROOT / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
        targets = self.pinned_toolchain_lanes(channel)
        for workflow_name, job_id in targets:
            with self.subTest(workflow=workflow_name, job=job_id):
                steps = self.workflows[workflow_name]["jobs"][job_id]["steps"]
                installers = [index for index, step in enumerate(steps)
                              if str(step.get("uses", "")).startswith("dtolnay/rust-toolchain@")]
                self.assertEqual(len(installers), 1)
                index = installers[0]
                installer = steps[index]
                self.assertTrue(installer.get("continue-on-error"),
                                "toolchain installer must continue so the retry step can run")
                self.assertIn("id", installer)
                retry = steps[index + 1]
                self.assertNotIn("uses", retry)
                self.assertIn("run", retry)
                # Container steps default to sh, where `set -euo pipefail`
                # aborts before any retry runs (b7753a4). The retry needs bash.
                self.assertEqual(retry.get("shell"), "bash",
                                 "toolchain retry must run under bash, not container sh")
                self.assertEqual(retry.get("if"), f"steps.{installer['id']}.outcome == 'failure'")
                self.assertNotEqual(retry.get("continue-on-error"), True,
                                    "the retry must fail the job when the toolchain stays missing")
                self.assertIn(channel, retry["run"])
                components = installer.get("with", {}).get("components", "")
                for component in filter(None, components.replace(" ", "").split(",")):
                    with self.subTest(component=component):
                        self.assertIn(component, retry["run"])
                with self.subTest(change="retry step removed"):
                    workflows = deepcopy(self.workflows)
                    del workflows[workflow_name]["jobs"][job_id]["steps"][index + 1]
                    retry_steps = workflows[workflow_name]["jobs"][job_id]["steps"]
                    still_guarded = (
                        len(retry_steps) > index + 1
                        and "run" in retry_steps[index + 1]
                        and retry_steps[index + 1].get("if") == f"steps.{installer['id']}.outcome == 'failure'"
                    )
                    self.assertFalse(still_guarded, "removing the retry step must fail this guard")

    def test_rust_toolchain_retry_fails_closed_behaviorally(self):
        # Behavioral pin for the retry guard above: substrings alone cannot
        # pin branch selection (deleting `[ "$attempt" -lt 5 ] || exit 1`
        # keeps every substring while the step exits 0 with no toolchain
        # installed) or the exact pin (a `rustup default stable` keeps the
        # install-line substring). Run each retry step under bash with
        # function-shimmed fake `rustup`/`sleep` (a function shim, not a
        # PATH shim, so the pin also runs in harnesses that sanitize PATH
        # on exec): always-failing rustup exits 1 after exactly 5 installs
        # with no default, and rustup succeeding on attempt 2 exits 0 having
        # defaulted the exact repository pin.
        import os
        import subprocess
        import tempfile

        channel = tomllib.loads((ROOT / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
        targets = self.pinned_toolchain_lanes(channel)
        fake_prelude = (
            "rustup() {\n"
            "  printf '%s\\n' \"$*\" >> \"$FAKE_RUSTUP_CALLS\"\n"
            "  FAKE_N=$((${FAKE_N:-0} + 1))\n"
            "  if [ \"$FAKE_N\" -lt \"$FAKE_SUCCEED_ON\" ]; then return 1; fi\n"
            "  return 0\n"
            "}\n"
            "sleep() { :; }\n"
        )
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            calls_path = tmp_path / "rustup-calls.txt"
            for workflow_name, job_id in targets:
                steps = self.workflows[workflow_name]["jobs"][job_id]["steps"]
                installers = [index for index, step in enumerate(steps)
                              if str(step.get("uses", "")).startswith("dtolnay/rust-toolchain@")]
                self.assertEqual(len(installers), 1)
                run_text = str(steps[installers[0] + 1]["run"])
                for case_name, succeed_on, expected_status, expected_installs in (
                    ("always-fails", "99", 1, 5),
                    ("succeeds-on-2", "2", 0, 2),
                ):
                    with self.subTest(workflow=workflow_name, job=job_id, case=case_name):
                        calls_path.write_text("", encoding="utf-8")
                        run_env = dict(os.environ)
                        run_env.update(
                            {
                                "FAKE_RUSTUP_CALLS": str(calls_path),
                                "FAKE_SUCCEED_ON": succeed_on,
                            }
                        )
                        proc = subprocess.run(
                            ["bash", "-c", fake_prelude + run_text],
                            env=run_env,
                            capture_output=True,
                            text=True,
                            timeout=30,
                        )
                        self.assertEqual(
                            proc.returncode, expected_status,
                            f"unexpected retry status: {proc.stderr}",
                        )
                        calls = calls_path.read_text(encoding="utf-8").splitlines()
                        installs = [line for line in calls if line.startswith("toolchain install ")]
                        self.assertEqual(len(installs), expected_installs,
                                         f"expected {expected_installs} installs, got: {calls}")
                        for line in installs:
                            self.assertIn(channel, line,
                                          f"retry must install the exact pin, got: {line!r}")
                        defaults = [line for line in calls if line.startswith("default ")]
                        if expected_status == 0:
                            self.assertIn(f"default {channel} --force-non-host", defaults,
                                          f"retry must default the exact pin, got: {calls}")
                        else:
                            self.assertEqual(defaults, [],
                                             f"failed retry must not default anything, got: {calls}")

    def test_overflow_runner_must_name_its_own_job(self):
        self.assertTrue(runner_allowed("worker", self.workflows["check.yml"]["jobs"]["worker"]["runs-on"]))
        for runs_on in (ROUTED_RUNNER.format(job="check"), "ubuntu-latest", ["self-hosted"],
                        "${{ vars.CI_OVERFLOW_RUNNER }}"):
            with self.subTest(runs_on=runs_on):
                self.assertFalse(runner_allowed("worker", runs_on))

    def test_duplicate_keys_cannot_hide_an_enabled_job(self):
        with self.assertRaises(ValueError):
            yaml.load("jobs:\n  deploy:\n    if: '${{ false }}'\n    if: true\n", Loader=UniqueKeyLoader)

    def test_review_rules_stays_read_only_and_shadow(self):
        workflow = self.workflows["review-rules.yml"]
        self.assertEqual(set(workflow["on"]), {"pull_request", "merge_group"})
        self.assertEqual(workflow["permissions"], {})
        job = workflow["jobs"]["review-rules"]
        self.assertEqual(job["permissions"], {"contents": "read"})
        self.assertEqual(job["with"], {"packs": "rust,security,python-shell-ci", "mode": "warn"})
        self.assertNotIn("secrets", job)
        self.assertNotIn("environment", job)
        self.assertEqual(job["uses"], REUSABLE_CALLS[("review-rules.yml", "review-rules")])

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

    def test_nextest_lane_declares_bash_and_keeps_strict_artifacts(self):
        # SDLC 2.3 CI speed pilot: the reusable lane runs in a job container
        # whose default shell is sh, so a pipefail script without an explicit
        # shell exits 2, no tests run, and the downstream parse/upload steps
        # fail on the missing files and mask the root cause. Every run step
        # declares bash, the parse names its upstream on a missing JUnit, and
        # the artifact uploads keep failing when their files are absent.
        job = self.workflows["reusable-rust-nextest.yml"]["jobs"]["nextest"]
        run_steps = [step for step in job.get("steps", []) if "run" in step]
        self.assertGreater(len(run_steps), 0)
        for step in run_steps:
            with self.subTest(step=step.get("name")):
                self.assertEqual(step.get("shell"), "bash")
        select = next(step for step in run_steps
                      if str(step.get("name", "")).startswith("Select sccache backend"))
        self.assertIn("pipefail", str(select.get("run", "")))
        run_nextest = next(step for step in run_steps
                           if str(step.get("name", "")).startswith("cargo nextest"))
        run_text = str(run_nextest.get("run", ""))
        # The JUnit path in .config/nextest.toml is profile-store relative
        # (target/nextest/ci/junit.xml), not workspace-root: the step must
        # resolve it and copy to the workspace root, keep failing when truly
        # absent, and preserve a failing nextest status with its evidence.
        self.assertIn("target/nextest/ci/junit.xml", run_text)
        self.assertIn("cp ", run_text)
        self.assertIn("test -f junit.xml", run_text)
        self.assertIn("exit $status", run_text)
        parse = next(step for step in run_steps
                     if str(step.get("name", "")).startswith("Parse JUnit"))
        self.assertIn("-f junit.xml", str(parse.get("run", "")))
        uploads = [step for step in job.get("steps", [])
                   if str(step.get("uses", "")).startswith("actions/upload-artifact@")]
        self.assertGreater(len(uploads), 0)
        for step in uploads:
            with self.subTest(upload=step.get("name")):
                self.assertEqual(step["with"]["if-no-files-found"], "error")

    def test_nextest_ci_profile_disables_fail_fast(self):
        # Fail-fast cancels not-yet-started tests after the first exhausted
        # failure, so junit.xml omits them and per-run ledger rows stop
        # being comparable. The evidence lane must run everything.
        import tomllib
        config = tomllib.loads((ROOT / ".config/nextest.toml").read_text(encoding="utf-8"))
        profile = config["profile"]["ci"]
        self.assertEqual(profile.get("retries"), 1)
        self.assertIs(profile.get("fail-fast"), False)

    def test_nextest_shared_backends_stay_off_pull_requests(self):
        # R2 write credentials must never reach PR- or merge-queue-built code
        # (build scripts and tests execute at a head that is not on `main`
        # yet, and a shared R2 bucket has no per-PR scope: a PR could poison
        # objects main later links). check.yml runs on merge_group, so a
        # pull_request-only denylist is not enough. Only push-to-main and
        # workflow_dispatch runs may use shared backends; every other event
        # uses sccache on local disk only. The GitHub-cache backend has the
        # same quota shape: the lane's entries share the repo Actions cache
        # with the warm `check` entry, so non-main runs must not grow it.
        lane = self.workflows["reusable-rust-nextest.yml"]["jobs"]["nextest"]
        select = next(step for step in lane.get("steps", [])
                      if str(step.get("name", "")).startswith("Select sccache backend"))
        run_text = str(select.get("run", ""))
        self.assertIn("workflow_dispatch", run_text)
        self.assertIn("github.event_name", str(select.get("env", "")))
        self.assertIn("local disk", run_text)
        # The allowlist branch must come FIRST: a denylist (`!=`) or a
        # reordered R2 branch ahead of it would let shared-backend
        # credentials reach merge_group runs as soon as any secret is set,
        # while every substring above stays present. Nothing
        # credential-shaped may precede the guard.
        lines = run_text.splitlines()
        gate = next(i for i, line in enumerate(lines)
                    if "NEXTEST_EVENT" in line)
        # Exact pin: a denylist, dead (&& false), or reordered guard all keep
        # the `if ` prefix, so pin the full first line.
        self.assertEqual(
            lines[gate].strip(),
            'if [ "$NEXTEST_EVENT" = "push" ] || [ "$NEXTEST_EVENT" = "workflow_dispatch" ]; then',
            f"push/workflow_dispatch allowlist must be the exact first branch, got: {lines[gate]!r}",
        )
        before = "\n".join(lines[:gate])
        self.assertNotIn("GITHUB_ENV", before)
        self.assertNotIn("R2_", before)
        self.assertNotIn("SCCACHE_", before)
        indent = lines[gate][: len(lines[gate]) - len(lines[gate].lstrip())]

        def outer_boundary(start, word):
            return next(
                i
                for i in range(start, len(lines))
                if lines[i].strip() == word
                and lines[i][: len(lines[i]) - len(lines[i].lstrip())] == indent
            )

        outer_else = outer_boundary(gate + 1, "else")
        outer_fi = outer_boundary(outer_else + 1, "fi")
        # The local-disk body must write nothing shared: an extra
        # SCCACHE_/R2_/GITHUB_ENV write inside it keeps every substring above
        # while leaking credentials on PR and merge-queue runs.
        disk_body = "\n".join(lines[outer_else + 1 : outer_fi])
        self.assertIn("local disk", disk_body)
        self.assertNotIn("GITHUB_ENV", disk_body)
        self.assertNotIn("R2_", disk_body)
        self.assertNotIn("SCCACHE_", disk_body)
        self.assertNotIn("AWS_", disk_body)
        # The shared backends are still configured on main: this guards
        # against gutting the step instead of guarding it.
        self.assertIn("GITHUB_ENV", "\n".join(lines[gate + 1 : outer_else]))
        # Behavioral pin: run the extracted step under bash. On
        # pull_request, merge_group, and other events the env file stays
        # empty even with all R2 vars set; on push and workflow_dispatch
        # the R2 lines appear. Catches inverted/dead guards and disk-branch
        # writes that structural pins miss.
        import os
        import subprocess
        import tempfile

        r2_env = {
            "R2_BUCKET": "test-bucket",
            "R2_ENDPOINT": "https://test.example.com",
            "R2_REGION": "auto",
            "R2_KEY": "test-key",
            "R2_SECRET": "test-secret",
        }
        with tempfile.NamedTemporaryFile(mode="r", delete=False) as tmp:
            backend_env_path = tmp.name
        try:
            base = dict(os.environ)
            for event in ("pull_request", "merge_group", "schedule"):
                with self.subTest(event=event):
                    run_env = dict(base)
                    run_env.update(
                        {
                            "NEXTEST_EVENT": event,
                            **r2_env,
                            "GITHUB_ENV": backend_env_path,
                        }
                    )
                    Path(backend_env_path).write_text("", encoding="utf-8")
                    proc = subprocess.run(
                        ["bash", "-c", run_text],
                        env=run_env,
                        capture_output=True,
                        text=True,
                        timeout=30,
                    )
                    self.assertEqual(proc.returncode, 0, f"{event} backend step failed: {proc.stderr}")
                    self.assertEqual(
                        Path(backend_env_path).read_text(encoding="utf-8"),
                        "",
                        f"{event} run must not write shared-backend credentials",
                    )
            for event in ("push", "workflow_dispatch"):
                with self.subTest(event=event):
                    run_env = dict(base)
                    run_env.update(
                        {
                            "NEXTEST_EVENT": event,
                            **r2_env,
                            "GITHUB_ENV": backend_env_path,
                        }
                    )
                    Path(backend_env_path).write_text("", encoding="utf-8")
                    proc = subprocess.run(
                        ["bash", "-c", run_text],
                        env=run_env,
                        capture_output=True,
                        text=True,
                        timeout=30,
                    )
                    self.assertEqual(proc.returncode, 0, f"{event} backend step failed: {proc.stderr}")
                    written = Path(backend_env_path).read_text(encoding="utf-8")
                    self.assertIn("SCCACHE_BUCKET=test-bucket", written)
                    self.assertIn("AWS_ACCESS_KEY_ID=test-key", written)
        finally:
            try:
                os.unlink(backend_env_path)
            except FileNotFoundError:
                pass
        raw = (ROOT / ".github/workflows/reusable-rust-nextest.yml").read_text(encoding="utf-8")
        self.assertNotIn("adds no cache footprint", raw)

    def test_required_nextest_withholds_r2_secrets_on_pull_request(self):
        # Even with the callee-side guard, the caller must forward R2 write
        # secrets only on push to main and workflow_dispatch. check.yml runs
        # on merge_group too, so a `!= 'pull_request'` gate would forward the
        # secrets to queued code that is not on `main` yet. The gate must
        # read `(== 'push' || == 'workflow_dispatch') && secrets.X || ''`:
        # the seemingly equivalent `== 'pull_request' && '' || secrets.X`
        # forwards the secret anyway, because '' is falsy in GitHub
        # expressions.
        job = self.workflows["check.yml"]["jobs"]["nextest"]
        r2_keys = ("SCCACHE_BUCKET", "SCCACHE_ENDPOINT", "SCCACHE_REGION",
                   "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY")
        for key in r2_keys:
            with self.subTest(secret=key):
                # Exact pin: the allowlist form withholds the secret on
                # pull_request AND merge_group. A `!= 'pull_request'` form or
                # a leading `== ... && ''` form forwards it on some event
                # while keeping every substring present, so substrings
                # cannot pin this gate.
                self.assertEqual(
                    str(job["secrets"][key]),
                    "${{ (github.event_name == 'push' || github.event_name == 'workflow_dispatch') && secrets." + key + " || '' }}",
                )

    def test_required_nextest_caller_shape_is_pinned(self):
        # The promoted lane is a required check.yml job: least-privilege
        # grant, rust-selector gate like the other Rust test lanes, exact
        # head SHA into the reusable lane, and R2 secrets only off-PR
        # (pinned by the withholding test above).
        job = self.workflows["check.yml"]["jobs"]["nextest"]
        self.assertEqual(job.get("name"), "rust nextest")
        self.assertEqual(job.get("permissions"), {"contents": "read"})
        self.assertIn("job-inputs", job.get("needs", []))
        self.assertIn("needs.job-inputs.outputs.rust != 'false'", job.get("if", ""))
        self.assertEqual(job.get("uses"), "./.github/workflows/reusable-rust-nextest.yml")
        self.assertEqual(job["with"]["ref"], "${{ github.event.pull_request.head.sha || github.sha }}")
        self.assertNotIn("runs-on", job)
        agg = self.workflows["check.yml"]["jobs"]["ci-ok"]
        self.assertIn("nextest", agg["needs"])
        self.assertIn("NEXTEST_RESULT", "\n".join(step.get("run", "") for step in agg["steps"]))

    def test_required_nextest_timeout_ceiling_is_pinned(self):
        # Gate ceiling: the required lane must keep its 45-minute ceiling.
        # Raising it hides a wedged runner; the pin forces a reviewed change.
        job = self.workflows["reusable-rust-nextest.yml"]["jobs"]["nextest"]
        # BaseLoader keeps scalars as strings; "45" is the 45-minute ceiling.
        self.assertEqual(job.get("timeout-minutes"), "45")

    def test_required_nextest_quarantine_ejects_without_revert(self):
        # Red-group ejection path: the `quarantine` input must exist with an
        # empty default, and the nextest step must turn each filter into a
        # `--skip` so the rest of the suite still gates. Pinned
        # cargo-nextest 0.9.99 only accepts `--skip` AFTER the `--`
        # separator, so the pin asserts the exact argv order: a `--skip`
        # before `--` fails the whole lane with a usage error.
        workflow = self.workflows["reusable-rust-nextest.yml"]
        quarantine = workflow["on"]["workflow_call"]["inputs"]["quarantine"]
        self.assertEqual(quarantine.get("default"), "")
        job = workflow["jobs"]["nextest"]
        run_nextest = next(step for step in job.get("steps", [])
                           if str(step.get("name", "")).startswith("cargo nextest"))
        self.assertEqual(run_nextest["env"]["QUARANTINE"], "${{ inputs.quarantine }}")
        run_text = str(run_nextest.get("run", ""))
        self.assertIn("--skip", run_text)
        self.assertIn("$QUARANTINE", run_text)
        self.assertIn('-- "${skip_args[@]}"', run_text)
        self.assertNotIn("--profile ci $skip_args", run_text)
        # Behavioral pin: substrings alone cannot pin branch selection (an
        # inverted empty/non-empty guard, or a flag dropped from one branch,
        # keeps every substring above while silently disabling ejection).
        # Run the extracted step under bash with a function-shimmed fake
        # `cargo` that records its argv and emits the JUnit file the rest of
        # the step copies, then pin the exact argv for both quarantine
        # states. (A function shim, not a PATH shim, so the pin also runs
        # in harnesses that sanitize PATH on exec.)
        import os
        import subprocess
        import tempfile

        base_argv = [
            "nextest", "run", "--workspace", "--lib", "--bins",
            "--features", "two-bot-core/db", "--locked", "--profile", "ci",
        ]
        cases = (
            ("empty-quarantine", "", base_argv, 0, 0),
            (
                "two-quarantine-filters",
                "red_group other_red",
                base_argv + ["--", "--skip", "red_group", "--skip", "other_red"],
                0,
                0,
            ),
            # A real nextest failure must remain the step result after the
            # JUnit report is copied for the always-run ledger/upload steps.
            ("nextest-failure", "", base_argv, 37, 37),
        )
        fake_cargo = (
            "cargo() {\n"
            "  printf '%s\\n' \"$@\" > \"$FAKE_CARGO_ARGV\"\n"
            "  mkdir -p target/nextest/ci\n"
            "  printf '<testsuites/>\\n' > target/nextest/ci/junit.xml\n"
            "  return \"${FAKE_CARGO_STATUS:-0}\"\n"
            "}\n"
        )
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            argv_path = tmp_path / "cargo-argv.txt"
            for case_name, quarantine_value, expected_argv, cargo_status, expected_status in cases:
                with self.subTest(case=case_name):
                    work_dir = tmp_path / ("work-" + case_name)
                    work_dir.mkdir()
                    run_env = dict(os.environ)
                    run_env.update(
                        {
                            "QUARANTINE": quarantine_value,
                            "FAKE_CARGO_ARGV": str(argv_path),
                            "FAKE_CARGO_STATUS": str(cargo_status),
                        }
                    )
                    proc = subprocess.run(
                        ["bash", "-eo", "pipefail", "-c", fake_cargo + run_text],
                        cwd=work_dir,
                        env=run_env,
                        capture_output=True,
                        text=True,
                        timeout=30,
                    )
                    self.assertEqual(
                        proc.returncode, expected_status,
                        f"unexpected step status for {case_name}: {proc.stderr}",
                    )
                    recorded = argv_path.read_text(encoding="utf-8").splitlines()
                    self.assertEqual(recorded, expected_argv)
                    report = work_dir / "junit.xml"
                    self.assertTrue(report.is_file(), f"nextest report was not copied for {case_name}")
                    self.assertEqual(report.read_text(encoding="utf-8"), "<testsuites/>\n")

    def test_ci_speed_rollout_documents_pr_local_cache(self):
        rollout = (ROOT / "docs/ci-speed-rollout.md").read_text(encoding="utf-8")
        # Markdown wraps mid-phrase, so match on whitespace-normalized text.
        flat = " ".join(rollout.split())
        self.assertIn("local disk", flat)
        self.assertNotIn("adds no cache footprint", flat)

    def test_minimal_grants_and_no_persisted_checkout_credentials(self):
        for name, workflow in self.workflows.items():
            self.assertEqual(workflow["permissions"], {})
            for job_id, job in workflow["jobs"].items():
                with self.subTest(workflow=name, job=job_id):
                    expected = {"contents": "read"}
                    if (name, job_id) in (("synthetic-user-flow.yml", "staging"),
                                          ("synthetic-user-flow.yml", "prod-passive")):
                        # Read workflow/job history to debounce alerts and detect recovery.
                        expected = {"contents": "read", "actions": "read"}
                    elif (name, job_id) == ("supply-chain.yml", "pr-lint"):
                        expected["pull-requests"] = "read"
                    elif (name, job_id) == ("test-guard.yml", "test-guard"):
                        # Marker-label actor audit via the issue-events API.
                        expected["pull-requests"] = "read"
                    elif (name, job_id) in (("staging-migrate.yml", "apply"),
                                                 ("production-migrate.yml", "apply")):
                        # Read-only fetch of the producing plan
                        # run's manifest artifact for the provenance gate.
                        expected = {"contents": "read", "actions": "read"}
                    elif (name, job_id) == ("deploy-production.yml", "guard"):
                        expected = {"contents": "read", "actions": "read", "checks": "read"}
                    elif (name, job_id) == ("release.yml", "tag"):
                        # Tags the promoted commit and publishes its release.
                        expected = {"contents": "write"}
                    elif (name, job_id) == ("deploy-production.yml", "release"):
                        # Only dispatches release.yml for the deployed SHA.
                        expected = {"actions": "write"}
                    elif (name, job_id) == ("release.yml", "attach-sbom"):
                        # TOG-10893: uploads verified SBOMs to the published tag.
                        expected = {"contents": "write"}
                    elif (name, job_id) == ("pr-size.yml", "pr-size"):
                        # Advisory labeller: writes the size/* label and keeps
                        # the one advisory comment on the caller's PR.
                        expected = {"contents": "read", "issues": "write",
                                    "pull-requests": "write"}
                    elif (name, job_id) == ("pr-size.yml", "pr-size-weekly"):
                        # Advisory weekly tally: read-only count of merged PRs
                        # by size label.
                        expected = {"contents": "read", "issues": "read",
                                    "pull-requests": "read"}
                    self.assertEqual(job["permissions"], expected)
                    for step in job.get("steps", []):
                        if step.get("uses", "").startswith("actions/checkout@"):
                            self.assertEqual(step["with"]["persist-credentials"], "false")


if __name__ == "__main__":
    unittest.main()
