"""Protected offline contract for staging deploy / voice lifecycle exclusion.

Run with check.yml's scripts/ci unittest discovery. Only YAML and Python source
are read; no workflow is dispatched and no Discord, database or deploy is used.
The positive controls add locks to in-memory copies, never repository files.
These tests prove the declared lock scope, not GitHub scheduler execution,
queue fairness, or that a runtime deletion failure has been repaired.
"""

import ast
from copy import deepcopy
import importlib.util
import os
from pathlib import Path
import unittest
from unittest import mock

import yaml

ROOT = Path(__file__).resolve().parents[2]
DEPLOY = "deploy-staging.yml"
VOICE = "voice-synthetic.yml"
INVOCATIONS = ("schedule", "workflow_dispatch", "workflow_call")


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


def workflows():
    return tuple(yaml.load((ROOT / ".github/workflows" / name).read_text(),
                           Loader=UniqueKeyLoader) for name in (DEPLOY, VOICE))


def job_group(job, label, errors):
    lock = job.get("concurrency")
    if not isinstance(lock, dict):
        errors.append(f"{label}: missing job-level lifecycle lock")
        return None
    group = lock.get("group")
    if not isinstance(group, str) or not group.strip() or "${{" in group:
        errors.append(f"{label}: lifecycle group must be fixed, nonempty, not expression-derived")
        return None
    if lock.get("cancel-in-progress") != "false":
        errors.append(f"{label}: lifecycle lock must explicitly preserve the running job")
    # GitHub concurrency group matching is case-insensitive, repository-scoped.
    return group.casefold()


def lock_group(node):
    lock = node.get("concurrency")
    return lock.get("group", "").casefold() if isinstance(lock, dict) else ""


def lifecycle_errors(deploy, voice, invocation):
    """Accept only a shared fixed lock on both execution jobs, not their caller.

    No expression evaluator: a run-, workflow-, event-, ref-, matrix-, input- or
    variable-derived key is not the fixed repository-wide exclusion contract.
    In particular a reusable workflow inherits caller context, so matching
    expression *text* is insufficient to establish matching lock values.
    """
    errors = []
    if invocation not in (voice.get("on") or {}):
        errors.append(f"voice: missing {invocation} invocation")
    deploy_job = deploy.get("jobs", {}).get("deploy", {})
    voice_job = voice.get("jobs", {}).get("synthetic", {})
    deploy_group = job_group(deploy_job, "deploy", errors)
    voice_group = job_group(voice_job, "synthetic", errors)
    if deploy_group and voice_group and deploy_group != voice_group:
        errors.append("deploy and synthetic: lifecycle job groups do not match")

    caller = deploy.get("jobs", {}).get("voice-synthetic", {})
    if invocation == "workflow_call":
        if caller.get("uses") != "./.github/workflows/voice-synthetic.yml":
            errors.append("caller: must invoke the local voice workflow")
        needs = caller.get("needs")
        if needs != "deploy" and needs != ["deploy"]:
            errors.append("caller: must wait for deploy completion")
        if "if" in caller or "continue-on-error" in caller:
            errors.append("caller: must retain default success-only, failure-propagating gate")

    # These nodes remain active while their children run. Holding the same
    # group here would deadlock (or cancel) the nested lifecycle job.
    for group in {deploy_group, voice_group} - {None}:
        for label, ancestor in (("deploy workflow", deploy),
                                ("voice workflow", voice),
                                ("reusable caller", caller)):
            if lock_group(ancestor) == group:
                errors.append(f"{label}: waiting ancestor holds lifecycle lock")
    return errors


def locked_fixture(group="fixture-staging-lifecycle"):
    deploy, voice = deepcopy(workflows())
    for job in (deploy["jobs"]["deploy"], voice["jobs"]["synthetic"]):
        job["concurrency"] = {"group": group, "cancel-in-progress": "false"}
    return deploy, voice


class RepositoryContractTests(unittest.TestCase):
    def test_scheduled_synthetic_excludes_deploy(self):
        self.assertEqual(lifecycle_errors(*workflows(), "schedule"), [])

    def test_manual_synthetic_excludes_deploy(self):
        self.assertEqual(lifecycle_errors(*workflows(), "workflow_dispatch"), [])

    def test_called_synthetic_excludes_deploy_without_nested_lock(self):
        self.assertEqual(lifecycle_errors(*workflows(), "workflow_call"), [])


class LockOracleControls(unittest.TestCase):
    def assert_rejected(self, deploy, voice, claim):
        for invocation in INVOCATIONS:
            with self.subTest(invocation=invocation):
                self.assertTrue(any(claim in e for e in lifecycle_errors(
                    deploy, voice, invocation)), (claim, invocation))

    def test_fixed_job_locks_accept_all_entrypoints_without_pinning_spelling(self):
        for group in ("fixture-staging-lifecycle", "another-valid-lifecycle-key"):
            deploy, voice = locked_fixture(group)
            for invocation in INVOCATIONS:
                with self.subTest(group=group, invocation=invocation):
                    self.assertEqual(lifecycle_errors(deploy, voice, invocation), [])

    def test_group_comparison_is_case_insensitive(self):
        deploy, voice = locked_fixture()
        voice["jobs"]["synthetic"]["concurrency"]["group"] = "FIXTURE-STAGING-LIFECYCLE"
        for invocation in INVOCATIONS:
            self.assertEqual(lifecycle_errors(deploy, voice, invocation), [])

    def test_missing_lock_on_either_execution_job_is_rejected(self):
        for target in ("deploy", "synthetic"):
            deploy, voice = locked_fixture()
            (deploy if target == "deploy" else voice)["jobs"][target].pop("concurrency")
            with self.subTest(target=target):
                self.assert_rejected(deploy, voice, "missing job-level")

    def test_distinct_job_groups_are_rejected(self):
        deploy, voice = locked_fixture()
        voice["jobs"]["synthetic"]["concurrency"]["group"] = "different-lifecycle"
        self.assert_rejected(deploy, voice, "groups do not match")

    def test_empty_missing_and_expression_derived_keys_are_rejected(self):
        keys = (None, "", " ", "${{ github.run_id }}", "${{ github.workflow }}-staging",
                "staging-${{ github.event_name }}", "staging-${{ github.ref }}",
                "${{ inputs.lock }}", "${{ vars.LIFECYCLE_GROUP }}",
                "staging-${{ matrix.target }}")
        for key in keys:
            for target in ("deploy", "synthetic", "both"):
                deploy, voice = locked_fixture()
                jobs = {"deploy": deploy["jobs"]["deploy"],
                        "synthetic": voice["jobs"]["synthetic"]}
                for name, job in jobs.items():
                    if target in (name, "both"):
                        job["concurrency"]["group"] = key
                with self.subTest(key=key, target=target):
                    self.assert_rejected(deploy, voice, "must be fixed")

    def test_cancelling_or_implicit_cancel_policy_is_rejected(self):
        for target in ("deploy", "synthetic"):
            for value in ("true", "${{ github.event_name == 'push' }}", None):
                deploy, voice = locked_fixture()
                lock = (deploy if target == "deploy" else voice)["jobs"][target]["concurrency"]
                if value is None:
                    lock.pop("cancel-in-progress")
                else:
                    lock["cancel-in-progress"] = value
                with self.subTest(target=target, value=value):
                    self.assert_rejected(deploy, voice, "preserve the running job")

    def test_workflow_level_only_lock_is_not_execution_job_exclusion(self):
        deploy, voice = locked_fixture()
        for workflow, job in ((deploy, "deploy"), (voice, "synthetic")):
            workflow["concurrency"] = workflow["jobs"][job].pop("concurrency")
        self.assert_rejected(deploy, voice, "missing job-level")

    def test_waiting_workflow_or_reusable_caller_cannot_hold_shared_lock(self):
        for target in ("deploy workflow", "voice workflow", "caller"):
            deploy, voice = locked_fixture()
            ancestor = {"deploy workflow": deploy, "voice workflow": voice,
                        "caller": deploy["jobs"]["voice-synthetic"]}[target]
            ancestor["concurrency"] = {"group": "FIXTURE-STAGING-LIFECYCLE",
                                       "cancel-in-progress": "false"}
            self.assert_rejected(deploy, voice, "waiting ancestor holds")

    def test_lock_on_preflight_instead_of_deploy_is_rejected(self):
        deploy, voice = locked_fixture()
        deploy["jobs"]["schema-gate"]["concurrency"] = deploy["jobs"]["deploy"].pop("concurrency")
        self.assert_rejected(deploy, voice, "deploy: missing job-level")

    def test_each_invocation_must_remain_available(self):
        for invocation in INVOCATIONS:
            deploy, voice = locked_fixture()
            voice["on"].pop(invocation)
            with self.subTest(invocation=invocation):
                self.assertIn(f"voice: missing {invocation} invocation",
                              lifecycle_errors(deploy, voice, invocation))

    def test_called_synthetic_requires_successful_deploy_and_local_call(self):
        changes = ({"needs": "schema-gate"}, {"uses": "./wrong.yml"},
                   {"if": "always()"}, {"continue-on-error": "true"})
        for change in changes:
            deploy, voice = locked_fixture()
            deploy["jobs"]["voice-synthetic"].update(change)
            with self.subTest(change=change):
                self.assertTrue(any(e.startswith("caller:") for e in
                                    lifecycle_errors(deploy, voice, "workflow_call")))

    def test_duplicate_yaml_keys_cannot_hide_a_lock_or_trigger(self):
        with self.assertRaisesRegex(ValueError, "duplicate workflow key"):
            yaml.load("concurrency:\n  group: one\n  group: two\n", Loader=UniqueKeyLoader)


class ExistingSafetyControls(unittest.TestCase):
    def test_existing_workflow_serialization_and_live_entrypoint_stay_intact(self):
        deploy, voice = workflows()
        for workflow, group in ((deploy, "deploy-staging"), (voice, "voice-synthetic")):
            self.assertEqual(workflow["concurrency"],
                             {"group": group, "cancel-in-progress": "false"})
        self.assertEqual(set(voice["on"]), set(INVOCATIONS))
        self.assertEqual(voice["on"]["schedule"], [{"cron": "41 */6 * * *"}])
        self.assertEqual(deploy["on"]["push"]["branches"], ["main"])
        job = voice["jobs"]["synthetic"]
        self.assertEqual(job["if"], "github.ref == 'refs/heads/main'")
        self.assertEqual(job["environment"], "voice-synthetic")
        self.assertEqual(job["timeout-minutes"], "10")
        self.assertEqual(job["permissions"], {"contents": "read"})
        self.assertNotIn("continue-on-error", job)
        live = [step for step in job["steps"] if "run" in step]
        self.assertEqual(len(live), 1)
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
        deploy, _ = workflows()
        job = deploy["jobs"]["deploy"]
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
