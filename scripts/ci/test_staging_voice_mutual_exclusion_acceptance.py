"""Protected offline contract for the full-window staging lifecycle lock.

QA-owned replacement of the Test Author contract that pinned job-level
``deploy`` + voice ``synthetic`` locks. Every guarantee of that contract is
kept (fixed, nonempty, non-expression, explicitly non-cancelling, case-
insensitive group; no waiting ancestor may hold it; the called voice gate
waits for a successful deploy and propagates failure; the live voice entry
point and its deletion budget are unchanged) and the exclusion window grows:

* deploy-staging holds one fixed repository-wide group at workflow level, so
  baseline, deploy, takeover, readiness, rollback, every in-run consumer,
  certify-promotion and the final staging-use release share one window;
* standalone consumers (voice, help, user-flow staging probe), the staging
  drills and the manual MCP prove-out hold that same group for their whole
  start/proof/stop window, so no split lock survives a rename;
* reusable voice/help invoked inside that window take a provably disjoint
  per-run nested group, never re-hold the caller's group (no deadlock), and
  bind the caller's exact SHA/deployment/expiry before live consumption
  without acquiring or releasing the caller's grant;
* the scheduled rollback-proof orchestrator and production passive probes
  hold nothing that waits on staging.

Run with check.yml's scripts/ci unittest discovery. Only YAML and Python
source are read; no workflow is dispatched and no Discord, database or deploy
is used. Controls mutate in-memory copies only. These tests prove the
declared lock scope, not GitHub scheduler execution, queue fairness (GitHub
keeps one pending run per group and replaces it), runtime expiry, or that a
runtime deletion failure has been repaired.
"""

import ast
from copy import deepcopy
from functools import lru_cache
import importlib.util
import os
from pathlib import Path
import re
import unittest
from unittest import mock

import yaml

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github/workflows"
DEPLOY = "deploy-staging.yml"
VOICE = "voice-synthetic.yml"
HELP = "help-embed-synthetic.yml"
USER_FLOW = "synthetic-user-flow.yml"
CONTAINER = "staging-container-drill.yml"
ROLLBACK = "staging-rollback-drill.yml"
MCP = "staging-mcp-proveout.yml"
INVOCATIONS = ("schedule", "workflow_dispatch", "workflow_call")
EXPRESSION = "${{"

# Both historical spellings of the staging lock. Whichever deploy-staging
# holds is canonical; any other holder of either spelling is a split lock.
LEGACY_GROUPS = ("deploy-staging", "staging-lifecycle")
# deploy-staging jobs that must sit inside the full window (all jobs of the
# workflow are covered by its workflow-level lock; these must exist).
WINDOW_JOBS = ("deploy", "voice-synthetic", "help-embed-synthetic",
               "user-flow-synthetic", "certify-promotion")
# Reusable caller job in deploy-staging -> called workflow.
CHILDREN = {"voice-synthetic": VOICE, "help-embed-synthetic": HELP}
# Called workflow -> (live consumer job, live entry point).
CONSUMERS = {VOICE: ("synthetic", "scripts/staging_voice_synthetic.py"),
             HELP: ("help-embed-smoke", "scripts/staging_help_embed_smoke.py")}
# workflow_call-only inputs carrying the caller's grant -> client env name.
INHERITED = {"staging_use_expires_at": "STAGING_USE_EXPIRES_AT",
             "staging_use_sha": "STAGING_USE_SHA",
             "staging_use_deployment": "STAGING_USE_DEPLOYMENT"}
OWNERSHIP_TOKEN = "${{ secrets.STAGING_OWNERSHIP_CONTROL_TOKEN }}"
STANDALONE_GUARD = "inputs.staging_use_expires_at == ''"
INHERITED_GUARD = "inputs.staging_use_expires_at != ''"
MAIN_ONLY = "github.ref == 'refs/heads/main'"
# (workflow, job) that must hold the canonical group when run standalone,
# and whether the hold must be job-level (to keep sibling prod probes free).
STANDALONE_HOLDERS = {(VOICE, "synthetic"): False,
                      (HELP, "help-embed-smoke"): False,
                      (USER_FLOW, "staging"): True,
                      (CONTAINER, "container-drill"): False,
                      (ROLLBACK, "drill"): True,
                      (MCP, "proveout"): False}
# Jobs that must hold nothing that can wait on the staging lifecycle.
FREE_JOBS = {(USER_FLOW, "prod-passive"): "production passive probe",
             (ROLLBACK, "proof"): "rollback-proof orchestrator"}
# Staging-touching jobs outside the lifecycle lock, each with a reason. A new
# staging-touching job fails the inventory until it is classified here or
# made a holder. Reclassify (do not silently extend) when one goes live.
EXEMPT = {
    ("deploy-production.yml", "soak"): "production guard reads staging evidence",
    ("deploy-production.yml", "staging-soak-evidence"): "production guard reads staging evidence",
    ("deploy-production.yml", "production"): "production deploy; staging origin is evidence input only",
    ("gradual-promote-proof.yml", "proof"): "passive: target lock refuses before any Cloudflare call",
    ("slow-burn-tracker.yml", "tracker"): "read-only /ops/metrics scrape",
    ("staging-ownership-status.yml", "status"): "read-only ownership GET; never starts a container",
    ("staging-events-read.yml", "read"): "database read only",
    ("staging-fixture-census.yml", "fixture-census"): "database read only",
    ("staging-migrate.yml", "plan"): "database plan only",
    ("staging-migrate.yml", "apply"): "database migration; staging-migrate lane",
    ("staging-privacy-verify-read.yml", "read"): "database read only",
    ("staging-retention-census.yml", "census"): "database read only",
}
STAGING_TOUCH = re.compile(r"STAGING_WORKER_URL|STAGING_OWNERSHIP_CONTROL_TOKEN|"
                           r"TWO_VOICE_SYNTHETIC_BOT_TOKEN|staging-use\.mjs")
# The only accepted expression group forms. Anything else is unprovable and is
# rejected on a holder and treated as holding the lock on an ancestor.
CALLED_FORM = re.compile(
    r"\$\{\{\s*inputs\.staging_use_expires_at\s*!=\s*''\s*&&\s*"
    r"format\('([a-z0-9][a-z0-9-]*)\{0\}',\s*github\.run_id\)\s*\|\|\s*"
    r"'([A-Za-z0-9_.-]+)'\s*\}\}")
EVENT_FORM = re.compile(r"([A-Za-z0-9_.-]*)\$\{\{\s*github\.event_name\s*\}\}")
GRANT_OUTPUT = re.compile(r"\$\{\{\s*needs\.deploy\.outputs\.([A-Za-z_][A-Za-z0-9_-]*)\s*\}\}")
# Same semantic actions through the redacting composition client. Keep the
# direct-client fixture controls and classify begin/finish just as strictly.
STAGING_USE = re.compile(r"staging-use(?:-pipeline)?\.mjs\s+([a-z]+)")
ACTION_NAMES = {"begin": "acquire", "rebind": "acquire", "finish": "release", "consume": "ready"}


def staging_use_actions(command):
    return [ACTION_NAMES.get(action, action) for action in STAGING_USE.findall(command)]


class UniqueKeyLoader(yaml.BaseLoader):
    # BaseLoader keeps Actions' `on` and booleans as strings, like test_workflows.
    def construct_mapping(self, node, deep=False):
        result = {}
        for key_node, value_node in node.value:
            key = self.construct_object(key_node, deep=deep)
            if key in result:
                raise ValueError(f"duplicate workflow key: {key}")
            result[key] = self.construct_object(value_node, deep=deep)
        return result


@lru_cache(maxsize=None)
def _parsed(root):
    return {path.name: yaml.load(path.read_text(), Loader=UniqueKeyLoader)
            for path in sorted(root.glob("*.yml"))}


def load_all(root=WORKFLOWS):
    # Parse once; callers may mutate their copy.
    return deepcopy(_parsed(root))


def events(workflow):
    on = workflow.get("on") or {}
    return (on,) if isinstance(on, str) else tuple(on)


def group_values(raw, context, triggers):
    """Possible casefolded values of a group, or None when not provable.

    Values are ("lit", text) or ("run", prefix) for prefix + github.run_id.
    ``context`` is "standalone" (own triggers; workflow_call-only inputs are
    empty) or "called" (inside deploy-staging, inherited inputs non-empty).
    """
    if raw is None:
        return frozenset()
    if not isinstance(raw, str) or not raw.strip():
        return None
    if EXPRESSION not in raw:
        return frozenset({("lit", raw.casefold())})
    called = CALLED_FORM.fullmatch(raw.strip())
    if called:
        prefix, fallback = called.groups()
        return frozenset({("run", prefix.casefold())} if context == "called"
                         else {("lit", fallback.casefold())})
    event = EVENT_FORM.fullmatch(raw.strip())
    if event and context == "standalone" and triggers:
        return frozenset(("lit", (event.group(1) + name).casefold()) for name in triggers)
    return None


def lock_parts(node):
    """(group, cancel, form) of a node's concurrency, form None when absent."""
    lock = node.get("concurrency")
    if lock is None:
        return None, None, None
    if isinstance(lock, str):
        return lock, None, "shorthand"
    if isinstance(lock, dict):
        return lock.get("group"), lock.get("cancel-in-progress"), "mapping"
    return lock, None, "invalid"


def node_values(node, context, triggers):
    group, _, form = lock_parts(node)
    if form is None:
        return frozenset()
    if form == "mapping" and group is None:
        return None
    return group_values(group, context, triggers) if form != "invalid" else None


def may_equal(value, canonical):
    kind, text = value
    if kind == "lit":
        return text == canonical
    return re.fullmatch(re.escape(text) + r"[0-9]+", canonical) is not None


def may_hold(node, context, triggers, canonical):
    values = node_values(node, context, triggers)
    return values is None or any(may_equal(v, canonical) for v in values)


def holds_exactly(node, context, triggers, canonical):
    """Error text unless the node holds exactly the canonical group, non-cancelling."""
    group, cancel, form = lock_parts(node)
    if form != "mapping":
        return "lifecycle lock must be a mapping with an explicit cancel policy"
    if group_values(group, context, triggers) != frozenset({("lit", canonical)}):
        return "lifecycle group must resolve to the fixed canonical group"
    if cancel != "false":
        return "lifecycle lock must explicitly preserve the running job"
    return None


def canonical_group(deploy, errors):
    group, cancel, form = lock_parts(deploy)
    if form is None:
        errors.append("deploy-staging: missing workflow-level full-window lock")
        return None
    if form != "mapping":
        errors.append("deploy-staging: full-window lock must explicitly preserve the running run")
        return None
    if not isinstance(group, str) or not group.strip() or EXPRESSION in group:
        errors.append("deploy-staging: full-window group must be fixed, nonempty, "
                      "not expression-derived")
        return None
    if cancel != "false":
        errors.append("deploy-staging: full-window lock must explicitly preserve the running run")
    # GitHub concurrency group matching is case-insensitive, repository-scoped.
    return group.casefold()


def step_runs(job):
    return [step for step in job.get("steps") or [] if isinstance(step.get("run"), str)]


def staging_use_steps(job, action):
    return [step for step in step_runs(job)
            if action in staging_use_actions(step["run"])]


def bare(condition):
    text = (condition or "").strip()
    if text.startswith(EXPRESSION) and text.endswith("}}"):
        text = text[len(EXPRESSION):-2].strip()
    return text


def window_errors(wfs, canonical):
    errors = []
    deploy = wfs[DEPLOY]
    jobs = deploy.get("jobs", {})
    triggers = events(deploy)
    for name in WINDOW_JOBS:
        if name not in jobs:
            errors.append(f"window: missing {name}")
    # The workflow holds the group while every job runs; re-holding it at job
    # level, in any form, deadlocks (or is cancelled by) the waiting run.
    for name, job in jobs.items():
        if may_hold(job, "standalone", triggers, canonical):
            errors.append(f"{name}: job-level lock re-holds the full-window group")
    if not any(staging_use_steps(job, "acquire") for job in jobs.values()):
        errors.append("window: deploy-staging never acquires the staging-use grant")
    releases = [(name, job) for name, job in jobs.items() if staging_use_steps(job, "release")]
    if not releases:
        errors.append("release: deploy-staging never releases the staging-use grant")
    for name, job in releases:
        needs = job.get("needs")
        needs = {needs} if isinstance(needs, str) else set(needs or ())
        if not set(WINDOW_JOBS) <= needs:
            errors.append(f"release {name}: must wait for every window consumer")
        if "always()" not in (job.get("if") or ""):
            errors.append(f"release {name}: must run even after a failed or cancelled consumer")
        if "continue-on-error" in job or any(
                "continue-on-error" in step for step in staging_use_steps(job, "release")):
            errors.append(f"release {name}: release failure must fail the run")
    return errors


def child_errors(wfs, canonical):
    errors = []
    deploy = wfs[DEPLOY]
    jobs = deploy.get("jobs", {})
    outputs = (jobs.get("deploy") or {}).get("outputs") or {}
    for caller_name, callee_name in CHILDREN.items():
        caller = jobs.get(caller_name, {})
        label = f"caller {caller_name}"
        if caller.get("uses") != f"./.github/workflows/{callee_name}":
            errors.append(f"{label}: must invoke the local {callee_name}")
        needs = caller.get("needs")
        if needs != "deploy" and needs != ["deploy"]:
            errors.append(f"{label}: must wait for deploy completion")
        if "if" in caller or "continue-on-error" in caller:
            errors.append(f"{label}: must retain default success-only, failure-propagating gate")
        if "concurrency" in caller:
            errors.append(f"{label}: waiting reusable caller declares concurrency")
        passed = caller.get("with") or {}
        for name in INHERITED:
            match = GRANT_OUTPUT.fullmatch(str(passed.get(name, "")).strip())
            if not match or match.group(1) not in outputs:
                errors.append(f"{label}: {name} must come from a declared deploy job output")

        callee = wfs.get(callee_name, {})
        on = callee.get("on") or {}
        call_inputs = (on.get("workflow_call") or {}).get("inputs") or {}
        dispatch_inputs = (on.get("workflow_dispatch") or {}).get("inputs") or {}
        for name in INHERITED:
            spec = call_inputs.get(name) or {}
            if spec.get("type") != "string" or spec.get("required") != "true":
                errors.append(f"{callee_name}: {name} must be a required workflow_call string")
            if name in dispatch_inputs:
                errors.append(f"{callee_name}: standalone dispatch can inject {name}")
        # Nested groups inside the caller's window (workflow- or job-level, or
        # none): provable, disjoint from the caller's group, non-cancelling.
        if may_hold(callee, "called", (), canonical):
            errors.append(f"{callee_name}: called nested group not provably disjoint")
        for job_name, job in (callee.get("jobs") or {}).items():
            if may_hold(job, "called", (), canonical):
                errors.append(f"{callee_name} {job_name}: called job re-holds the caller's group")
        for job_name, node in [("workflow", callee)] + list((callee.get("jobs") or {}).items()):
            _, cancel, form = lock_parts(node)
            if form is not None and cancel != "false":
                errors.append(f"{callee_name} {job_name}: nested lock must explicitly "
                              "preserve the running run")
        errors += consumer_errors(callee_name, callee)
    return errors


def consumer_errors(callee_name, callee):
    errors = []
    jobs = callee.get("jobs") or {}
    for job_name, job in jobs.items():
        for action in ("acquire", "release"):
            for step in staging_use_steps(job, action):
                guard = bare(step.get("if"))
                if guard != STANDALONE_GUARD and not guard.startswith(STANDALONE_GUARD + " && "):
                    errors.append(f"{callee_name} {job_name}: {action} can touch the caller's grant")
    consumer_name, entry = CONSUMERS[callee_name]
    consumer = jobs.get(consumer_name, {})
    live = [i for i, step in enumerate(consumer.get("steps") or [])
            if entry in str(step.get("run", ""))]
    if not live:
        errors.append(f"{callee_name}: live consumer {entry} missing")
        return errors
    # Credential separation (test_workflows pins the same for voice): the
    # live consumer job never holds the staging control credential or client.
    text = str(consumer).lower()
    if "ownership_control" in text or "staging-use.mjs" in text:
        errors.append(f"{callee_name}: live consumer job holds the staging control credential")
    if bare(consumer.get("if")) not in ("", MAIN_ONLY) or "continue-on-error" in consumer:
        errors.append(f"{callee_name}: live consumer must keep the main-only, "
                      "success-gated, failure-propagating gate")
    needs = consumer.get("needs")
    needs = {needs} if isinstance(needs, str) else set(needs or ())
    asserted = False
    for owner_name, owner in jobs.items():
        for step in owner.get("steps") or []:
            actions = staging_use_actions(str(step.get("run", "")))
            if not {"assert", "ready"} & set(actions):
                continue
            before = owner_name in needs and owner_name != consumer_name
            env = step.get("env") or {}
            bound = all(str(env.get(var, "")).replace(" ", "") ==
                        f"${{{{inputs.{name}}}}}" for name, var in INHERITED.items())
            # Only the inherited-path guard may skip the assert; its job keeps
            # the plain main-only gate so a skip cannot silently skip consumers.
            gated_off = (bare(step.get("if")) not in ("", INHERITED_GUARD)
                         or bare(owner.get("if")) not in ("", MAIN_ONLY))
            if (before and bound
                    and str(env.get("OWNERSHIP_CONTROL_TOKEN", "")) == OWNERSHIP_TOKEN
                    and "continue-on-error" not in step and "continue-on-error" not in owner
                    and not gated_off):
                asserted = True
    if not asserted:
        errors.append(f"{callee_name}: no authenticated window/readiness assert "
                      "bound to the caller's grant before live consumption")
    return errors


def standalone_errors(wfs, canonical):
    errors = []
    for (file, job_name), job_level in STANDALONE_HOLDERS.items():
        workflow = wfs.get(file, {})
        job = (workflow.get("jobs") or {}).get(job_name)
        label = f"{file} {job_name}"
        if job is None:
            errors.append(f"{label}: standalone consumer missing")
            continue
        triggers = tuple(t for t in events(workflow) if t != "workflow_call")
        holders = [node for node in (workflow, job)
                   if may_hold(node, "standalone", triggers, canonical)]
        if len(holders) != 1:
            errors.append(f"{label}: must hold the lifecycle group exactly once "
                          f"(held {len(holders)} times)")
            continue
        if job_level and holders[0] is workflow:
            errors.append(f"{label}: must hold the lifecycle group at job level")
        problem = holds_exactly(holders[0], "standalone", triggers, canonical)
        if problem:
            errors.append(f"{label}: {problem}")
    for (file, job_name), role in FREE_JOBS.items():
        workflow = wfs.get(file, {})
        job = (workflow.get("jobs") or {}).get(job_name)
        if job is None:
            errors.append(f"{file} {job_name}: {role} missing")
            continue
        triggers = events(workflow)
        if any(may_hold(node, "standalone", triggers, canonical) for node in (workflow, job)):
            errors.append(f"{file} {job_name}: {role} holds the lifecycle group")
    # Even an unrelated workflow-level group is unsafe here: a previous run
    # retains it while staging waits, indirectly queueing later prod samples.
    if "concurrency" in wfs.get(USER_FLOW, {}):
        errors.append("prod-passive: shared workflow queue couples production to staging waits")
    passive = (wfs.get(USER_FLOW, {}).get("jobs") or {}).get("prod-passive") or {}
    needs = passive.get("needs")
    if needs == "staging" or (isinstance(needs, list) and "staging" in needs):
        errors.append("prod-passive: production passive probe waits on the staging probe")
    voice_on = wfs.get(VOICE, {}).get("on") or {}
    for invocation in INVOCATIONS:
        if invocation not in voice_on:
            errors.append(f"voice: missing {invocation} invocation")
    return errors


def repository_errors(wfs, canonical):
    """Split locks, caller closure and the staging-consumer inventory."""
    errors = []
    callers = {f"./.github/workflows/{name}" for name in CHILDREN.values()}
    holders = set(STANDALONE_HOLDERS)
    for file, workflow in wfs.items():
        triggers = tuple(t for t in events(workflow) if t != "workflow_call")
        nodes = [("workflow", workflow)] + list((workflow.get("jobs") or {}).items())
        for name, node in nodes:
            for kind, text in node_values(node, "standalone", triggers) or ():
                if kind == "lit" and text in LEGACY_GROUPS and text != canonical:
                    errors.append(f"split lifecycle lock: {file} {name} holds {text}")
            if name == "workflow":
                continue
            if node.get("uses") in callers and (file != DEPLOY or name not in CHILDREN):
                errors.append(f"caller closure: {file} {name} calls a window consumer")
            env = str(node.get("environment") or "")
            touches = STAGING_TOUCH.search(str(node)) or "staging" in env
            classified = (file in (DEPLOY, *CONSUMERS) or (file, name) in holders
                          or (file, name) in EXEMPT)
            if touches and not classified:
                errors.append(f"inventory: unclassified staging consumer {file} {name}")
    for file, name in EXEMPT:
        if name not in (wfs.get(file, {}).get("jobs") or {}):
            errors.append(f"inventory: stale exemption {file} {name}")
    return errors


def lifecycle_errors(wfs):
    errors = []
    canonical = canonical_group(wfs.get(DEPLOY, {}), errors)
    if canonical is None:
        return errors
    for check in (window_errors, child_errors, standalone_errors, repository_errors):
        errors += check(wfs, canonical)
    return errors


# ---------------------------------------------------------------------------
# Conceptual target fixture. It is built from the checked-in workflows and
# rewired in memory to the accepted shape, so the controls below are
# executable today and do not depend on the final #2004 spelling. The
# repository assertions (RepositoryContractTests) read only checked-in YAML.
# ---------------------------------------------------------------------------

FIXTURE_GROUP = "fixture-staging-lifecycle"


def called_group(prefix, fallback):
    return (f"${{{{ inputs.staging_use_expires_at != '' && "
            f"format('{prefix}{{0}}', github.run_id) || '{fallback}' }}}}")


def assert_step():
    env = {var: f"${{{{ inputs.{name} }}}}" for name, var in INHERITED.items()}
    env["OWNERSHIP_CONTROL_TOKEN"] = OWNERSHIP_TOKEN
    return {"name": "Assert the caller's staging window",
            "env": env, "run": "node wrangler/scripts/staging-use.mjs ready"}


def strip_staging_use(workflow):
    """Drop checked-in staging-use wiring so the fixture is the only wiring.

    Keeps every control meaningful before and after #2004 lands: a mutation
    that removes the fixture's step cannot be masked by a real one.
    """
    jobs = workflow["jobs"]
    for name in [n for n, job in jobs.items()
                 if staging_use_steps(job, "release") and n not in WINDOW_JOBS]:
        del jobs[name]
    for job in jobs.values():
        if "steps" in job:
            job["steps"] = [step for step in job["steps"]
                            if not STAGING_USE.search(str(step.get("run", "")))]


def target_fixture(group=FIXTURE_GROUP):
    wfs = load_all()
    lock = {"group": group, "cancel-in-progress": "false"}
    deploy = wfs[DEPLOY]
    deploy["concurrency"] = dict(lock)
    jobs = deploy["jobs"]
    for workflow in (deploy, wfs[VOICE], wfs[HELP]):
        strip_staging_use(workflow)
    for job in jobs.values():
        job.pop("concurrency", None)
    jobs["deploy"].setdefault("outputs", {}).update(
        {"staging_use_expires_at": "${{ steps.use.outputs.expires_at }}",
         "staging_use_sha": "${{ steps.use.outputs.sha }}",
         "staging_use_deployment": "${{ steps.use.outputs.deployment }}"})
    jobs["deploy"]["steps"].insert(0, {"id": "use", "run": "node wrangler/scripts/staging-use.mjs acquire"})
    for caller in CHILDREN:
        jobs[caller]["with"] = {name: f"${{{{ needs.deploy.outputs.{name} }}}}" for name in INHERITED}
    jobs["staging-use-release"] = {
        "needs": list(WINDOW_JOBS), "if": "always()",
        "steps": [{"run": "node wrangler/scripts/staging-use.mjs release"}]}
    for name, prefix in ((VOICE, "staging-voice-consumer-"), (HELP, "staging-help-consumer-")):
        callee = wfs[name]
        callee["concurrency"] = {"group": called_group(prefix, group), "cancel-in-progress": "false"}
        callee["on"]["workflow_call"]["inputs"] = {
            input_name: {"type": "string", "required": "true"} for input_name in INHERITED}
        consumer_name, _ = CONSUMERS[name]
        consumer = callee["jobs"][consumer_name]
        consumer.pop("concurrency", None)
        # Separate staging-environment job; the consumer keeps only its bot token.
        step = assert_step()
        step["if"] = INHERITED_GUARD
        callee["jobs"]["staging-window"] = {"if": MAIN_ONLY, "environment": "staging",
                                            "steps": [step]}
        consumer["needs"] = "staging-window"
    wfs[USER_FLOW]["jobs"]["staging"]["concurrency"] = dict(lock)
    wfs[CONTAINER]["concurrency"] = dict(lock)
    wfs[ROLLBACK]["jobs"]["drill"]["concurrency"] = dict(lock)
    wfs[MCP]["concurrency"] = dict(lock)
    return wfs


class RepositoryContractTests(unittest.TestCase):
    """Executable against the checked-in workflows; red until #2004 wires them."""

    def test_checked_in_workflows_meet_the_full_window_contract(self):
        self.assertEqual(lifecycle_errors(load_all()), [])


class LockOracleControls(unittest.TestCase):
    def test_composition_actions_keep_the_same_mutation_and_readiness_guards(self):
        self.assertEqual(staging_use_actions("node staging-use-pipeline.mjs begin"), ["acquire"])
        self.assertEqual(staging_use_actions("node staging-use-pipeline.mjs finish"), ["release"])
        self.assertEqual(staging_use_actions("node staging-use-pipeline.mjs consume"), ["ready"])
        self.assertEqual(staging_use_actions("node staging-use-pipeline.mjs rebind"), ["acquire"])
        wfs = target_fixture()
        for workflow in wfs.values():
            for job in workflow.get("jobs", {}).values():
                for step in step_runs(job):
                    step["run"] = (step["run"].replace("staging-use.mjs acquire", "staging-use-pipeline.mjs begin")
                                   .replace("staging-use.mjs release", "staging-use-pipeline.mjs finish"))
        self.assertEqual(lifecycle_errors(wfs), [])
        # The composed client cannot bypass either the final-release dependency
        # closure or the inherited path's prohibition on acquire/release.
        release = next(job for job in wfs[DEPLOY]["jobs"].values() if staging_use_steps(job, "release"))
        release["needs"] = ["deploy"]
        self.assert_rejected(wfs, "must wait for every window consumer")
        wfs = target_fixture()
        wfs[VOICE]["jobs"]["staging-window"]["steps"].append({
            "run": "node staging-use-pipeline.mjs finish", "if": INHERITED_GUARD})
        self.assert_rejected(wfs, "release can touch the caller's grant")

    def assert_rejected(self, wfs, claim):
        errors = lifecycle_errors(wfs)
        self.assertTrue(any(claim in e for e in errors), (claim, errors))

    def test_target_shape_is_accepted_without_pinning_spelling(self):
        for group in (FIXTURE_GROUP, "another-valid-lifecycle-key", *LEGACY_GROUPS):
            with self.subTest(group=group):
                self.assertEqual(lifecycle_errors(target_fixture(group)), [])

    def test_production_probe_cannot_wait_on_an_unrelated_shared_workflow_queue(self):
        wfs = target_fixture()
        wfs[USER_FLOW]["jobs"]["prod-passive"]["concurrency"] = {
            "group": "production-samples", "cancel-in-progress": "false"}
        self.assertEqual(lifecycle_errors(wfs), [])
        wfs[USER_FLOW]["concurrency"] = {"group": "synthetic-user-flow", "cancel-in-progress": "false"}
        self.assert_rejected(wfs, "shared workflow queue couples production")

    def test_group_comparison_is_case_insensitive(self):
        wfs = target_fixture()
        wfs[USER_FLOW]["jobs"]["staging"]["concurrency"]["group"] = FIXTURE_GROUP.upper()
        wfs[DEPLOY]["concurrency"]["group"] = FIXTURE_GROUP.swapcase()
        self.assertEqual(lifecycle_errors(wfs), [])

    # Old guarantee: fixed, nonempty, non-expression, non-cancelling group.
    def test_missing_empty_or_expression_full_window_group_is_rejected(self):
        for key in (None, "", " ", "${{ github.run_id }}", "${{ github.workflow }}-staging",
                    "staging-${{ github.event_name }}", "staging-${{ github.ref }}",
                    "${{ inputs.lock }}", "${{ vars.LIFECYCLE_GROUP }}",
                    called_group("staging-x-", FIXTURE_GROUP)):
            wfs = target_fixture()
            wfs[DEPLOY]["concurrency"]["group"] = key
            with self.subTest(key=key):
                self.assert_rejected(wfs, "must be fixed")
        wfs = target_fixture()
        del wfs[DEPLOY]["concurrency"]
        self.assert_rejected(wfs, "missing workflow-level full-window lock")
        wfs = target_fixture()
        wfs[DEPLOY]["concurrency"] = FIXTURE_GROUP
        self.assert_rejected(wfs, "full-window lock must explicitly preserve")

    def test_cancelling_or_implicit_cancel_policy_is_rejected(self):
        for value in ("true", "${{ github.event_name == 'push' }}", None):
            for target in ("deploy", "voice", "help", "user-flow", "mcp"):
                wfs = target_fixture()
                lock = {"deploy": wfs[DEPLOY]["concurrency"],
                        "voice": wfs[VOICE]["concurrency"],
                        "help": wfs[HELP]["concurrency"],
                        "user-flow": wfs[USER_FLOW]["jobs"]["staging"]["concurrency"],
                        "mcp": wfs[MCP]["concurrency"]}[target]
                if value is None:
                    lock.pop("cancel-in-progress")
                else:
                    lock["cancel-in-progress"] = value
                with self.subTest(target=target, value=value):
                    self.assert_rejected(wfs, "preserve the running")

    # Expanded window: the old job-level-only shape is now insufficient.
    def test_old_job_level_only_shape_is_rejected(self):
        # The pre-#2004 shape: deploy-staging serializes on its own group while
        # only the deploy and voice execution jobs share staging-lifecycle.
        wfs = target_fixture("deploy-staging")
        old = {"group": "staging-lifecycle", "cancel-in-progress": "false"}
        wfs[DEPLOY]["jobs"]["deploy"]["concurrency"] = dict(old)
        wfs[VOICE]["concurrency"] = {"group": "voice-synthetic", "cancel-in-progress": "false"}
        wfs[VOICE]["jobs"]["synthetic"]["concurrency"] = dict(old)
        self.assert_rejected(wfs, "split lifecycle lock")
        self.assert_rejected(wfs, f"{VOICE} synthetic: must hold the lifecycle group exactly once")

    def test_each_window_job_must_remain(self):
        for name in WINDOW_JOBS:
            wfs = target_fixture()
            del wfs[DEPLOY]["jobs"][name]
            with self.subTest(job=name):
                self.assert_rejected(wfs, f"window: missing {name}")

    def test_job_level_rehold_inside_window_is_rejected_in_every_form(self):
        forms = (FIXTURE_GROUP, FIXTURE_GROUP.upper(),
                 {"group": FIXTURE_GROUP, "cancel-in-progress": "false"},
                 "${{ vars.LIFECYCLE_GROUP }}", {"group": "${{ github.workflow }}"})
        for job in ("deploy", "user-flow-synthetic", "certify-promotion", "staging-use-release"):
            for form in forms:
                wfs = target_fixture()
                wfs[DEPLOY]["jobs"][job]["concurrency"] = deepcopy(form)
                with self.subTest(job=job, form=form):
                    self.assert_rejected(wfs, f"{job}: job-level lock re-holds")

    def test_unrelated_job_level_group_inside_window_is_accepted(self):
        wfs = target_fixture()
        wfs[DEPLOY]["jobs"]["certify-promotion"]["concurrency"] = {
            "group": "certify-promotion", "cancel-in-progress": "false"}
        self.assertEqual(lifecycle_errors(wfs), [])

    def test_missing_acquire_or_release_is_rejected(self):
        wfs = target_fixture()
        del wfs[DEPLOY]["jobs"]["deploy"]["steps"][0]
        self.assert_rejected(wfs, "never acquires")
        wfs = target_fixture()
        del wfs[DEPLOY]["jobs"]["staging-use-release"]
        self.assert_rejected(wfs, "never releases")

    def test_release_must_close_the_window_and_fail_honestly(self):
        changes = (({"needs": ["deploy"]}, "every window consumer"),
                   ({"if": "success()"}, "even after a failed"),
                   ({"continue-on-error": "true"}, "must fail the run"))
        for change, claim in changes:
            wfs = target_fixture()
            wfs[DEPLOY]["jobs"]["staging-use-release"].update(change)
            with self.subTest(change=change):
                self.assert_rejected(wfs, claim)

    # Old guarantee: called gate waits for a successful deploy, local call,
    # failure propagates, waiting caller holds nothing.
    def test_called_consumer_requires_successful_deploy_and_local_call(self):
        changes = ({"needs": "schema-gate"}, {"uses": "./wrong.yml"},
                   {"if": "always()"}, {"continue-on-error": "true"},
                   {"concurrency": "unrelated-caller-group"})
        for caller in CHILDREN:
            for change in changes:
                wfs = target_fixture()
                wfs[DEPLOY]["jobs"][caller].update(change)
                with self.subTest(caller=caller, change=change):
                    self.assert_rejected(wfs, f"caller {caller}:")

    def test_grant_must_come_from_deploy_outputs(self):
        for value in ("", "1767225600000", "${{ inputs.sha }}", "${{ github.sha }}",
                      "${{ needs.deploy.outputs.undeclared }}"):
            wfs = target_fixture()
            wfs[DEPLOY]["jobs"]["voice-synthetic"]["with"]["staging_use_sha"] = value
            with self.subTest(value=value):
                self.assert_rejected(wfs, "staging_use_sha must come from")

    def test_inherited_inputs_are_call_only_and_required(self):
        wfs = target_fixture()
        wfs[VOICE]["on"]["workflow_dispatch"] = {"inputs": {
            "staging_use_expires_at": {"type": "string"}}}
        self.assert_rejected(wfs, "standalone dispatch can inject")
        wfs = target_fixture()
        wfs[HELP]["on"]["workflow_call"]["inputs"]["staging_use_deployment"]["required"] = "false"
        self.assert_rejected(wfs, "must be a required workflow_call string")

    # Unsafe nested ancestor forms on the reusable workflow itself.
    def test_unsafe_nested_groups_are_rejected(self):
        unsafe = (FIXTURE_GROUP, FIXTURE_GROUP.upper(), "${{ github.workflow }}",
                  "${{ vars.LIFECYCLE_GROUP }}",
                  f"${{{{ inputs.staging_use_expires_at == '' && "
                  f"format('x-{{0}}', github.run_id) || '{FIXTURE_GROUP}' }}}}",
                  f"${{{{ inputs.staging_use_expires_at != '' && '{FIXTURE_GROUP}' || 'x' }}}}",
                  f"${{{{ inputs.staging_use_expires_at != '' && "
                  f"format('{{0}}', github.run_id) || '{FIXTURE_GROUP}' }}}}",
                  f"${{{{ inputs.staging_use_expires_at != '' && "
                  f"format('x-{{0}}', github.run_attempt) || '{FIXTURE_GROUP}' }}}}")
        for callee in (VOICE, HELP):
            for group in unsafe:
                wfs = target_fixture()
                wfs[callee]["concurrency"]["group"] = group
                with self.subTest(callee=callee, group=group):
                    self.assert_rejected(wfs, "not provably disjoint")

    def test_job_level_called_form_without_nested_workflow_group_is_accepted(self):
        # "No nested global group": the consumer job itself carries the form.
        wfs = target_fixture()
        for callee, (job, _) in CONSUMERS.items():
            wfs[callee]["jobs"][job]["concurrency"] = wfs[callee].pop("concurrency")
        self.assertEqual(lifecycle_errors(wfs), [])

    def test_shorthand_nested_group_is_rejected(self):
        wfs = target_fixture()
        wfs[VOICE]["concurrency"] = FIXTURE_GROUP
        self.assert_rejected(wfs, "not provably disjoint")

    def test_called_job_rehold_is_rejected(self):
        for callee, (job, _) in CONSUMERS.items():
            for form in (FIXTURE_GROUP, {"group": FIXTURE_GROUP.upper(),
                                         "cancel-in-progress": "false"}):
                wfs = target_fixture()
                wfs[callee]["jobs"][job]["concurrency"] = deepcopy(form)
                with self.subTest(callee=callee, form=form):
                    self.assert_rejected(wfs, "called job re-holds")

    def test_consumer_must_assert_the_caller_grant_first(self):
        for callee, (job, _) in CONSUMERS.items():
            for mutation in ("drop", "unordered", "unbound", "token", "gated", "soft",
                             "never", "job-never", "job-soft"):
                wfs = target_fixture()
                jobs = wfs[callee]["jobs"]
                step = jobs["staging-window"]["steps"][0]
                if mutation == "drop":
                    del jobs["staging-window"]
                    del jobs[job]["needs"]
                elif mutation == "unordered":
                    del jobs[job]["needs"]
                elif mutation == "unbound":
                    step["env"]["STAGING_USE_EXPIRES_AT"] = "${{ github.run_id }}"
                elif mutation == "token":
                    step["env"]["OWNERSHIP_CONTROL_TOKEN"] = "${{ secrets.OTHER }}"
                elif mutation == "gated":
                    step["if"] = STANDALONE_GUARD
                elif mutation == "never":
                    step["if"] = "false"
                elif mutation == "job-never":
                    jobs["staging-window"]["if"] = f"{MAIN_ONLY} && {STANDALONE_GUARD}"
                elif mutation == "job-soft":
                    jobs["staging-window"]["continue-on-error"] = "true"
                else:
                    step["continue-on-error"] = "true"
                with self.subTest(callee=callee, mutation=mutation):
                    self.assert_rejected(wfs, "no authenticated window/readiness assert")

    def test_consumer_gate_cannot_tolerate_a_failed_or_skipped_assert(self):
        for callee, (job, _) in CONSUMERS.items():
            for change in ({"if": f"always() && {MAIN_ONLY}"}, {"if": "!cancelled()"},
                           {"continue-on-error": "true"}):
                wfs = target_fixture()
                wfs[callee]["jobs"][job].update(change)
                with self.subTest(callee=callee, change=change):
                    self.assert_rejected(wfs, "live consumer must keep the main-only")

    def test_assert_inside_consumer_job_mixes_credentials_and_is_rejected(self):
        for callee, (job, entry) in CONSUMERS.items():
            wfs = target_fixture()
            jobs = wfs[callee]["jobs"]
            step = jobs.pop("staging-window")["steps"][0]
            del jobs[job]["needs"]
            steps = jobs[job]["steps"]
            index = next(i for i, s in enumerate(steps) if entry in str(s.get("run", "")))
            steps.insert(index, step)
            with self.subTest(callee=callee):
                self.assert_rejected(wfs, "holds the staging control credential")
                self.assert_rejected(wfs, "no authenticated window/readiness assert")

    def test_called_consumer_cannot_acquire_or_release_caller_grant(self):
        for callee in CONSUMERS:
            for action in ("acquire", "release"):
                for guard in (None, "always()", f"always() || {STANDALONE_GUARD}"):
                    wfs = target_fixture()
                    step = {"run": f"node wrangler/scripts/staging-use.mjs {action}"}
                    if guard:
                        step["if"] = guard
                    wfs[callee]["jobs"]["staging-window"]["steps"].append(step)
                    with self.subTest(callee=callee, action=action, guard=guard):
                        self.assert_rejected(wfs, f"{action} can touch the caller's grant")
                wfs = target_fixture()
                wfs[callee]["jobs"]["staging-window"]["steps"].append(
                    {"if": f"${{{{ {STANDALONE_GUARD} }}}}",
                     "run": f"node wrangler/scripts/staging-use.mjs {action}"})
                self.assertEqual(lifecycle_errors(wfs), [])

    # Standalone consumers and drills: same group, exactly once.
    def test_missing_standalone_holder_is_rejected(self):
        for (file, job), job_level in STANDALONE_HOLDERS.items():
            wfs = target_fixture()
            (wfs[file]["jobs"][job] if job_level else wfs[file]).pop("concurrency", None)
            if file in (VOICE, HELP):
                wfs[file]["concurrency"] = {"group": called_group("x-", "unrelated"),
                                            "cancel-in-progress": "false"}
            with self.subTest(file=file, job=job):
                self.assert_rejected(wfs, f"{file} {job}: must hold the lifecycle group exactly once")

    def test_double_hold_by_standalone_consumer_is_rejected(self):
        wfs = target_fixture()
        wfs[VOICE]["jobs"]["synthetic"]["concurrency"] = {"group": FIXTURE_GROUP,
                                                          "cancel-in-progress": "false"}
        self.assert_rejected(wfs, "exactly once (held 2 times)")

    def test_mcp_proveout_holds_the_full_window_group(self):
        # Formerly exempt as read-only; it now starts and stops staging, so it
        # is a full-window holder, checked by this oracle independently of the
        # builder's exact-shape pin.
        self.assertNotIn((MCP, "proveout"), EXEMPT)
        wfs = target_fixture()
        wfs[MCP]["jobs"]["proveout"]["concurrency"] = wfs[MCP].pop("concurrency")
        self.assertEqual(lifecycle_errors(wfs), [])
        label = f"{MCP} proveout:"
        wrong = ({"group": "staging-mcp-proveout", "cancel-in-progress": "false"},
                 {"group": f"{FIXTURE_GROUP}-${{{{ github.event_name }}}}",
                  "cancel-in-progress": "false"})
        for lock in (None, *wrong):
            wfs = target_fixture()
            if lock is None:
                del wfs[MCP]["concurrency"]
            else:
                wfs[MCP]["concurrency"] = lock
            with self.subTest(lock=lock):
                self.assert_rejected(wfs, f"{label} must hold the lifecycle group exactly once")
        for change, claim in (({"cancel-in-progress": "true"}, "preserve the running job"),
                              ({"cancel-in-progress": None}, "preserve the running job"),
                              ({"group": "${{ vars.LIFECYCLE_GROUP }}"}, "fixed canonical group"),
                              ({"group": "${{ github.run_id }}"}, "fixed canonical group")):
            wfs = target_fixture()
            lock = wfs[MCP]["concurrency"]
            lock.update(change)
            if lock["cancel-in-progress"] is None:
                del lock["cancel-in-progress"]
            with self.subTest(change=change):
                self.assert_rejected(wfs, claim)
        wfs = target_fixture()
        wfs[MCP]["concurrency"] = FIXTURE_GROUP
        self.assert_rejected(wfs, f"{label} lifecycle lock must be a mapping")
        wfs = target_fixture()
        wfs[MCP]["jobs"]["proveout"]["concurrency"] = dict(wfs[MCP]["concurrency"])
        self.assert_rejected(wfs, f"{label} must hold the lifecycle group exactly once (held 2 times)")
        wfs = target_fixture()
        del wfs[MCP]["jobs"]["proveout"]
        self.assert_rejected(wfs, f"{label} standalone consumer missing")

    def test_user_flow_hold_must_not_delay_production_probe(self):
        wfs = target_fixture()
        wfs[USER_FLOW]["concurrency"] = wfs[USER_FLOW]["jobs"]["staging"].pop("concurrency")
        self.assert_rejected(wfs, "production passive probe holds")
        wfs = target_fixture()
        wfs[USER_FLOW]["jobs"]["prod-passive"]["needs"] = "staging"
        self.assert_rejected(wfs, "waits on the staging probe")
        wfs = target_fixture()
        del wfs[USER_FLOW]["jobs"]["prod-passive"]
        self.assert_rejected(wfs, "production passive probe missing")

    def test_rollback_orchestrator_cannot_hold_child_lock(self):
        for node in ("workflow", "job"):
            wfs = target_fixture()
            target = wfs[ROLLBACK] if node == "workflow" else wfs[ROLLBACK]["jobs"]["proof"]
            target["concurrency"] = {"group": FIXTURE_GROUP, "cancel-in-progress": "false"}
            with self.subTest(node=node):
                self.assert_rejected(wfs, "rollback-proof orchestrator holds")
        wfs = target_fixture()
        wfs[ROLLBACK]["concurrency"]["group"] = "${{ vars.PROOF_GROUP }}"
        self.assert_rejected(wfs, "rollback-proof orchestrator holds")

    def test_split_legacy_lock_is_rejected(self):
        for legacy in LEGACY_GROUPS:
            for file, node in ((CONTAINER, None), (ROLLBACK, "drill"), (MCP, None)):
                wfs = target_fixture(group=[g for g in LEGACY_GROUPS if g != legacy][0])
                target = wfs[file] if node is None else wfs[file]["jobs"][node]
                target["concurrency"]["group"] = legacy
                with self.subTest(legacy=legacy, file=file):
                    self.assert_rejected(wfs, "split lifecycle lock")

    def test_each_voice_invocation_must_remain_available(self):
        for invocation in INVOCATIONS:
            wfs = target_fixture()
            wfs[VOICE]["on"].pop(invocation)
            with self.subTest(invocation=invocation):
                self.assert_rejected(wfs, f"voice: missing {invocation} invocation")

    def test_caller_closure_rejects_other_callers(self):
        wfs = target_fixture()
        wfs[USER_FLOW]["jobs"]["sneaky"] = {"uses": f"./.github/workflows/{VOICE}"}
        self.assert_rejected(wfs, "caller closure")
        wfs = target_fixture()
        wfs[DEPLOY]["jobs"]["voice-again"] = {"uses": f"./.github/workflows/{HELP}"}
        self.assert_rejected(wfs, "caller closure")

    def test_unclassified_staging_consumer_is_rejected(self):
        for job in ({"environment": "staging", "steps": []},
                    {"steps": [{"run": "curl \"$STAGING_WORKER_URL\"",
                                "env": {"STAGING_WORKER_URL": "${{ vars.STAGING_WORKER_URL }}"}}]}):
            wfs = target_fixture()
            wfs["new-staging-consumer.yml"] = {"on": {"workflow_dispatch": ""},
                                               "jobs": {"probe": job}}
            with self.subTest(job=job):
                self.assert_rejected(wfs, "unclassified staging consumer")
        wfs = target_fixture()
        del wfs["staging-ownership-status.yml"]
        self.assert_rejected(wfs, "stale exemption")
        # A demoted holder cannot hide in the inventory either.
        wfs = target_fixture()
        with mock.patch.dict(STANDALONE_HOLDERS, clear=False):
            del STANDALONE_HOLDERS[(MCP, "proveout")]
            self.assert_rejected(wfs, f"unclassified staging consumer {MCP} proveout")

    def test_duplicate_yaml_keys_cannot_hide_a_lock_or_trigger(self):
        with self.assertRaisesRegex(ValueError, "duplicate workflow key"):
            yaml.load("concurrency:\n  group: one\n  group: two\n", Loader=UniqueKeyLoader)


class ExistingSafetyControls(unittest.TestCase):
    def test_existing_live_entrypoint_stays_intact(self):
        wfs = load_all()
        deploy, voice = wfs[DEPLOY], wfs[VOICE]
        self.assertEqual(set(voice["on"]), set(INVOCATIONS))
        self.assertEqual(voice["on"]["schedule"], [{"cron": "41 */6 * * *"}])
        self.assertEqual(deploy["on"]["push"]["branches"], ["main"])
        job = voice["jobs"]["synthetic"]
        self.assertEqual(job["if"], "github.ref == 'refs/heads/main'")
        self.assertEqual(job["environment"], "voice-synthetic")
        self.assertEqual(job["timeout-minutes"], "10")
        self.assertEqual(job["permissions"], {"contents": "read"})
        self.assertNotIn("continue-on-error", job)
        live = [step for step in step_runs(job)
                if "scripts/staging_voice_synthetic.py" in step["run"]]
        self.assertEqual(len(live), 1)
        self.assertNotIn("if", live[0])
        self.assertNotIn("continue-on-error", live[0])
        self.assertIn('python3 scripts/staging_voice_synthetic.py --evidence', live[0]["run"])
        self.assertIn("for attempt in 1 2 3; do", live[0]["run"])
        self.assertIn('[ "$attempt" = 3 ] || sleep 60', live[0]["run"])
        self.assertEqual(live[0]["env"]["TWO_STAGING_EMPTY_GRACE_SECONDS"],
                         "${{ vars.STAGING_EMPTY_GRACE_SECONDS || '60' }}")
        self.assertNotIn("--grace", live[0]["run"])
        self.assertNotIn("--delete-slack", live[0]["run"])
        self.assertEqual(live[0]["env"]["TWO_VOICE_SYNTHETIC_BOT_TOKEN"],
                         "${{ secrets.TWO_VOICE_SYNTHETIC_BOT_TOKEN }}")

    def test_deploy_lock_scope_contains_takeover_readiness_and_rollback(self):
        job = load_all()[DEPLOY]["jobs"]["deploy"]
        self.assertEqual(job["environment"], "staging")
        self.assertEqual(job["needs"], ["schema-gate", "roles-gate"])
        steps = job["steps"]
        by_id = {step["id"]: step for step in steps if "id" in step}
        self.assertIn("cloudflare/wrangler-action@", by_id["deploy"]["uses"])
        self.assertIn("deployment-takeover", by_id["takeover"]["run"])
        self.assertTrue(any("staging_rollout.py verify" in step.get("run", "") for step in steps))
        rollback = by_id["auto_rollback"]
        self.assertIn("worker_container_rollback.py rollback", rollback["run"])
        self.assertIn("--variant staging", rollback["run"])
        self.assertEqual(rollback["if"], "failure() && steps.deploy.outcome == 'success' "
                         "&& steps.snapshot.outcome == 'success'")

    def test_certify_promotion_still_needs_every_live_gate(self):
        job = load_all()[DEPLOY]["jobs"]["certify-promotion"]
        self.assertLessEqual({"deploy", "voice-synthetic", "help-embed-synthetic",
                              "user-flow-synthetic", "pinned-history-guard"}, set(job["needs"]))
        self.assertTrue(job["if"].startswith("success() && "))
        self.assertNotIn("continue-on-error", job)

    def test_deletion_budget_and_live_failure_assertion_are_unchanged(self):
        path = ROOT / "scripts/staging_voice_synthetic.py"
        spec = importlib.util.spec_from_file_location("lifecycle_budget_control", path)
        synth = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(synth)
        with mock.patch.dict(os.environ, {}, clear=True):
            args = synth.parse_args([])
        self.assertEqual((args.grace, args.delete_slack), (60.0, 20.0))
        self.assertEqual(args.grace + args.delete_slack, 80.0)
        # Pin executable source (not comments) linking both deadlines and both
        # room IDs to the observation consumed by the real delete verdict.
        tree = ast.parse(path.read_text())
        live = next(node for node in tree.body
                    if isinstance(node, ast.FunctionDef) and node.name == "run_live")
        text = ast.unparse(live)
        self.assertIn("time.monotonic() + args.grace + args.delete_slack", text)
        self.assertIn("e['t'] == 'CHANNEL_DELETE' and e['d']['id'] in rooms", text)
        self.assertIn("obs['deleted'] = set(rooms) <= gone", text)
        for deleted, expected in ((False, "FAIL"), (True, "PASS")):
            observation = {"creator": {"id": "100", "parent_id": "200"},
                           "me": "300", "deleted": deleted}
            rows = [row for row in synth.evaluate(observation) if row["check"] == "delete"]
            self.assertEqual(len(rows), 1)
            self.assertEqual(rows[0]["verdict"], expected)


if __name__ == "__main__":
    unittest.main()
