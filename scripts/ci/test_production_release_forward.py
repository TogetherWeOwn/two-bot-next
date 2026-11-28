"""Synthetic evidence for the forward database release and inverse boundary."""

from copy import deepcopy
from dataclasses import replace
from datetime import datetime, timezone
import json
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import Mock, call, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from production_release_forward import forward_receipt
from production_release_ledgers import CUTOVER, STORE
from production_ownership_status import provider_sample
import production_release as release
import production_recovery_workflow as recovery_workflow
import production_release_preflight as preflight
from ci.production_migrate_claim import projection_hash, store_projection_hash
from ci.test_production_release_ledgers import retained_plan
from staging_migrate_approval import digest_json
from staging_rollout import GateError


class ReleaseForwardTests(unittest.TestCase):
    def setUp(self):
        def source(versions):
            return [{"version": version, "description": "fixture", "sha384": "b" * 96,
                     "sha384_recomputed": "b" * 96}
                    for version in versions]

        self.plan = {"source_sha": "a" * 40,
                     "target": {"host": "prod.invalid", "database": "two_bot", "branch_id": "fixture-branch"},
                     "recovery_evidence_ref": "recovery", "acl_plan_ref": "roles",
                     "pending_before": list(CUTOVER), "pending_store_before": list(STORE),
                     "source_migrations": source((473, *CUTOVER)), "source_store_migrations": source((465, *STORE))}
        self.expanded = [[{**{key: row[key] for key in ("version", "description", "sha384")}, "success": True}
                          for row in self.plan[key]]
                         for key in ("source_migrations", "source_store_migrations")]
        self.expanded[0][0]["description"] = "historical cutover baseline"
        self.expanded[1][0]["description"] = "historical store baseline"
        self.plan.update(plan_manifest_sha256=projection_hash(self.plan),
                         store_plan_manifest_sha256=store_projection_hash(self.plan),
                         ledger_before=self.expanded[0][:1], ledger_after=self.expanded[0][:1],
                         ledger_store_before=self.expanded[1][:1], ledger_store_after=self.expanded[1][:1])
        self.outcome = {**deepcopy(self.plan), "runner_version": 1, "mode": "apply", "role": "two_bot_migrator",
                        "migration_target": "production", "plan_run_id": "123", "plan_provenance_verified": True,
                        "applied_count": 5, "applied_store_count": 1, "attendance_fence_sha256": "c" * 64,
                        "ledger_after": self.expanded[0], "ledger_store_after": self.expanded[1]}

    def test_forward_receipt_binds_exact_fence_and_both_ledger_endpoints(self):
        result = forward_receipt("123", self.plan, self.outcome)
        self.assertEqual(result, {"plan_run_id": "123", "source_sha": "a" * 40,
                                  "database_fingerprint": digest_json(self.plan["target"]),
                                  "expanded_ledgers_sha256": digest_json(self.expanded),
                                  "restored_ledgers_sha256": digest_json([self.expanded[0][:2], self.expanded[1][:1]]),
                                  "attendance_fence_sha256": "c" * 64})

    def test_forward_receipt_refuses_pre_commit_or_drifted_evidence(self):
        for key, value in {"attendance_fence_sha256": None, "plan_run_id": "124", "applied_store_count": True,
                           "plan_provenance_verified": False, "ledger_after": self.plan["ledger_before"],
                           "ledger_store_after": self.plan["ledger_store_before"], "target": {}}.items():
            with self.subTest(key=key), self.assertRaises(GateError):
                forward_receipt("123", self.plan, {**self.outcome, key: value})
        with self.assertRaisesRegex(GateError, "production_release_plan_ledgers_invalid"):
            forward_receipt("123", {**self.plan, "ledger_store_before": []}, self.outcome)
        bad_source = deepcopy(self.plan)
        bad_source["source_migrations"][0]["sha384_recomputed"] = "c" * 96
        with self.assertRaisesRegex(GateError, "production_release_plan_checksum_invalid"):
            forward_receipt("123", bad_source, self.outcome)

    def test_retained_forward_passes_the_reviewed_pending_list_and_requires_the_original_digest(self):
        plan = retained_plan(self.plan, self.outcome["attendance_fence_sha256"])
        outcome = {**self.outcome, "applied_count": 4, "ledger_before": plan["ledger_before"],
                   "pending_before": plan["pending_before"], "plan_manifest_sha256": plan["plan_manifest_sha256"]}
        self.assertEqual(forward_receipt("123", plan, outcome), forward_receipt("123", self.plan, self.outcome))
        for changed in ({"attendance_fence_sha256": "d" * 64}, {"applied_count": 5}):
            with self.assertRaises(GateError):
                forward_receipt("123", plan, {**outcome, **changed})

        with TemporaryDirectory() as scratch, patch.object(release.subprocess, "run") as process:
            process.return_value = Mock(returncode=0, stdout=json.dumps(outcome))
            env = {f"TWO_BOT_PRODUCTION_{role}_DATABASE_URL": "fixture" for role in ("PLAN", "MIGRATOR")}
            for plan_id, mode in ((None, "--plan"), ("123", "--apply")):
                release.database("fixture", Path(scratch), plan, env, plan_id=plan_id)
                args = process.call_args.args[0]
                self.assertIn(mode, args)
                self.assertEqual(args[args.index("--expected-pending") + 1], "479,480,481,482")
            self.assertEqual(json.loads((Path(scratch) / "reviewed-plan.json").read_bytes()), plan)

    def preflight_fixture(self):
        pin = preflight.ReleasePreflight("d" * 40, self.plan["source_sha"], "sha256:" + "1" * 64,
                                         "sha256:" + "2" * 64, "e" * 40,
                                         "12345678-1234-4567-8123-123456789abc", "sha256:" + "3" * 64,
                                         "12345678-1234-4567-8123-123456789def", "4" * 32, "5" * 64)
        plan = {**self.plan, "mode": "plan", "role": "two_bot_migrator_ro", "migration_target": "production",
                "role_verified_connections": 1, "audit": {"verify_findings": []},
                "recovery_evidence_ref": pin.recovery_reference()}
        return pin, plan

    def test_preflight_binds_immutable_candidate_and_live_rollback_capability(self):
        pin, plan = self.preflight_fixture()
        state = {"worker": {"version_id": pin.rollback_worker, "sha": pin.rollback_sha, "traffic_percent": 100},
                 "container": {"image_digest": pin.rollback_image, "application_id": pin.application_id,
                               "namespace_id": pin.namespace_id, "rollout_status": "completed"},
                 "ownership": {"phase": "active", "running": True, "epoch": 17}}
        observe = Mock(side_effect=lambda: deepcopy(state))
        control = Mock(return_value={"deploymentId": pin.rollback_worker, "exactTakeover": True})
        self.assertEqual(preflight.verify_preflight(pin, plan["target"], plan, deepcopy(plan), observe, control), 17)
        control.assert_called_once_with("takeover-capability", expected_deployment=pin.rollback_worker)
        control.reset_mock()
        with self.assertRaisesRegex(GateError, "production_release_preflight_plan_changed"):
            preflight.verify_preflight(replace(pin, candidate_archive="sha256:" + "6" * 64), plan["target"],
                                       plan, plan, observe, control)
        control.assert_not_called()
        control.return_value = {"deploymentId": pin.rollback_worker, "exactTakeover": False}
        with self.assertRaisesRegex(GateError, "production_release_retained_takeover_unproved"):
            preflight.verify_preflight(pin, plan["target"], plan, plan, observe, control)
        control.return_value["exactTakeover"] = True
        shifted = deepcopy(state)
        shifted["ownership"]["epoch"] += 1
        with self.assertRaisesRegex(GateError, "production_release_retained_pair_changed"):
            preflight.verify_preflight(pin, plan["target"], plan, plan, Mock(side_effect=[state, shifted]), control)
        approved = retained_plan(plan)
        observe.reset_mock()
        with self.assertRaisesRegex(GateError, "production_release_preflight_plan_changed"):
            preflight.verify_preflight(pin, approved["target"], approved,
                                       {**approved, "attendance_fence_sha256": "d" * 64}, observe, control)
        observe.assert_not_called()

    def test_known_legacy_predecessor_fences_only_the_observed_pair_at_the_preflight_epoch(self):
        pin, plan = self.preflight_fixture()
        pin = replace(pin, rollback_sha="d314f4c63ecbea263aa953bc8a649a444a97da7c")
        plan["recovery_evidence_ref"] = pin.recovery_reference()
        actor = release.release_actor("456", pin.candidate_sha)
        before = {"worker": {"version_id": pin.rollback_worker, "sha": pin.rollback_sha, "traffic_percent": 100},
                  "container": {"image_digest": pin.rollback_image, "application_id": pin.application_id,
                                "namespace_id": pin.namespace_id, "rollout_status": "completed"},
                  "ownership": {"phase": "active", "running": True, "epoch": 17}}
        observe = Mock(return_value=before)
        control = Mock()
        self.assertEqual(preflight.verify_preflight(pin, plan["target"], plan, deepcopy(plan), observe, control), 17)
        self.assertEqual(observe.call_count, 2)
        control.assert_not_called()

        for section, field, changed in (("worker", "version_id", "unknown"), ("worker", "traffic_percent", 99),
                                        ("container", "image_digest", pin.candidate_image),
                                        ("ownership", "epoch", 18)):
            with self.subTest(section=section, field=field):
                drifted = deepcopy(before)
                drifted[section][field] = changed
                with self.assertRaisesRegex(GateError, "production_release_retained_pair_changed"):
                    release.stop_prior_pair(pin, "456", 17, Mock(return_value=drifted), control)
                control.assert_not_called()

        stopped = {**before, "ownership": {"phase": "fenced", "running": False, "epoch": 18, "owner_actor": actor}}
        ack = {"deploymentId": pin.rollback_worker, "running": False,
               "owner": {"phase": "fenced", "deploymentId": None, "epoch": 18, "oldEpoch": 17, "actor": actor}}
        timeline = Mock()
        timeline.control.return_value = ack
        timeline.observe.side_effect = [before, stopped]
        self.assertEqual(release.stop_prior_pair(pin, "456", 17, timeline.observe, timeline.control), ack)
        self.assertEqual(timeline.mock_calls, [call.observe(),
                         call.control("fence-legacy", 17, expected_deployment=pin.rollback_worker), call.observe()])

        changed = {**stopped, "container": {**stopped["container"], "image_digest": pin.candidate_image}}
        control.return_value = ack
        with self.assertRaisesRegex(GateError, "production_stopped_fence_unproved"):
            release.stop_prior_pair(pin, "456", 17, Mock(side_effect=[before, changed]), control)
        control.assert_called_once_with("fence-legacy", 17, expected_deployment=pin.rollback_worker)

    def test_prepare_rejects_agreeing_invalid_pre_plans_before_reading_provider(self):
        pin, plan = self.preflight_fixture()
        # The read-only shape is valid, but the required PRE audit findings are absent.
        plan.update(runner_version=1, plan_run_id="", plan_provenance_verified=False,
                    applied_count=0, applied_store_count=0)
        run = {"id": 456}
        artifact = {"archive_digest": "sha256:" + "8" * 64}
        with TemporaryDirectory() as temp:
            directory = Path(temp) / "production-release"
            directory.mkdir()
            (directory / "fresh-plan.json").write_text(json.dumps(plan), encoding="utf-8")
            env = {"RUNNER_TEMP": temp, "PRODUCTION_WORKER_URL": "https://prod.invalid",
                   "OWNERSHIP_CONTROL_TOKEN": "fixture", "CLOUDFLARE_ACCOUNT_ID": "fixture",
                   "CLOUDFLARE_API_TOKEN": "fixture"}
            with (patch.object(release, "release_context", return_value=(pin, run, "123", plan, artifact)),
                  patch.object(release, "check_url"), patch.object(release, "control_client") as control,
                  patch.object(release, "require_unused_attendance_plan") as unused,
                  patch.object(release, "read_status") as observe,
                  patch.object(release, "RollbackClient") as provider):
                self.assertEqual(release.main(["prepare"], env), 1)
            unused.assert_not_called()
            control.assert_not_called()
            observe.assert_not_called()
            provider.assert_not_called()

    def test_prepare_checks_single_use_before_provider_access(self):
        pin, plan = self.preflight_fixture()
        fresh = {**plan, "role_verified_connections": 2}
        run = {"id": 456}
        artifact = {"run_id": 123, "artifact_id": 789, "archive_digest": "sha256:" + "8" * 64}
        provider_steps = ["control", "preflight", "provider", "provenance", "registry", "write_proof"]
        spent = GateError("production_attendance_plan_superseded_by_release_attempt")
        for refusal, expected_steps in ((spent, []), (None, provider_steps)):
            with self.subTest(approved=refusal is None), TemporaryDirectory() as temp:
                directory = Path(temp) / "production-release"
                directory.mkdir()
                (directory / "fresh-plan.json").write_text(json.dumps(fresh), encoding="utf-8")
                output_path = Path(temp) / "output"
                env = {"RUNNER_TEMP": temp, "PRODUCTION_WORKER_URL": "https://prod.invalid",
                       "OWNERSHIP_CONTROL_TOKEN": "fixture", "CLOUDFLARE_ACCOUNT_ID": "fixture",
                       "CLOUDFLARE_API_TOKEN": "fixture", "GITHUB_OUTPUT": str(output_path)}
                calls = Mock()
                calls.approval.side_effect = refusal
                calls.preflight.return_value = 17
                calls.write_proof.side_effect = release.write_json
                with (patch.object(release, "release_context", return_value=(pin, run, "123", plan, artifact)),
                      patch.object(release, "check_url"),
                      patch.multiple(release, validate_attendance_plan=calls.validate,
                                     require_unused_attendance_plan=calls.approval,
                                     control_client=calls.control, verify_preflight=calls.preflight,
                                     read_status=calls.observe, RollbackClient=calls.provider,
                                     validate_receipt_against_live=calls.provenance,
                                     image_in_registry=calls.registry, write_json=calls.write_proof)):
                    self.assertEqual(release.main(["prepare"], env), int(refusal is not None))
                self.assertEqual(calls.mock_calls[:3], [call.validate(plan), call.validate(fresh),
                                                        call.approval(release.github, artifact, run)])
                self.assertEqual([name for name, _, _ in calls.mock_calls[3:]], expected_steps)
                intent = directory / f"{preflight.STOP_INTENT_ARTIFACT}.json"
                self.assertEqual(intent.exists(), refusal is None)
                if refusal is None:
                    self.assertEqual(json.loads(intent.read_text()),
                                     preflight.stop_intent(pin, run, "123", plan, artifact, 17))
                self.assertEqual(output_path.exists(), refusal is None)

    def test_stop_refuses_a_spent_attendance_plan_before_the_provider_call(self):
        pin, plan = self.preflight_fixture()
        run = {"id": 456}
        artifact = {"archive_digest": "sha256:" + "8" * 64}
        with TemporaryDirectory() as temp:
            directory = Path(temp) / "production-release"
            directory.mkdir()
            proof = preflight.stop_intent(pin, run, "123", plan, artifact, 17)
            release.write_json(directory / "production-release-stop-intent.json", proof)
            env = {"RUNNER_TEMP": temp, "PRODUCTION_WORKER_URL": "https://prod.invalid",
                   "OWNERSHIP_CONTROL_TOKEN": "fixture", "GITHUB_OUTPUT": str(Path(temp) / "output")}
            with (patch.object(release, "release_context", return_value=(pin, run, "123", plan, artifact)),
                  patch.object(release, "check_url"), patch.object(release, "control_client") as control,
                  patch.object(release, "require_unused_attendance_plan",
                               side_effect=GateError("production_attendance_plan_superseded_by_release_attempt")) as unused,
                  patch.object(release, "stop_prior_pair", return_value={}) as stop):
                self.assertEqual(release.main(["stop"], env), 1)
            unused.assert_called_once_with(release.github, artifact, run)
            control.assert_not_called()
            stop.assert_not_called()

    def test_forward_database_boundary_refuses_wrong_fence_or_producer_before_apply(self):
        pin, plan = self.preflight_fixture()
        preflight_epoch = 17
        fingerprint = digest_json(plan["target"])
        run = {"id": 456, "run_attempt": 1, "head_sha": pin.workflow_sha}
        stopped = {"deploymentId": pin.rollback_worker, "running": False,
                   "owner": {"deploymentId": None, "phase": "fenced", "epoch": 18, "oldEpoch": 17,
                             "actor": f"production-schema-release:456:{pin.candidate_sha}"}}
        job = {"name": preflight.PREPARE_JOB, "run_id": 456, "run_attempt": 1, "head_sha": pin.workflow_sha,
               "status": "completed", "conclusion": "success", "steps": [
                   {"name": step, "status": "completed", "conclusion": "success"} for step in preflight.STOP_INTENT_STEPS]}
        now = datetime(2026, 11, 25, tzinfo=timezone.utc)
        api = Mock(side_effect=lambda route: {"created_at": now.isoformat()} if route == "actions/runs/123"
                   else {"total_count": 1, "jobs": [job]})
        apply = Mock(return_value={**self.outcome, "recovery_evidence_ref": pin.recovery_reference()})
        for bad_stop in ({**stopped, "running": True},
                         {**stopped, "owner": {**stopped["owner"], "oldEpoch": 18, "epoch": 19}}):
            with self.subTest(stop=bad_stop), self.assertRaisesRegex(GateError, "production_forward_stopped_fence_unproved"):
                preflight.apply_under_bound_fence(pin, run, "123", fingerprint, bad_stop, apply, api,
                                                  preflight_epoch=preflight_epoch)
        api.assert_not_called()
        job["conclusion"] = "cancelled"
        with self.assertRaisesRegex(GateError, "production_inverse_job_invalid"):
            preflight.apply_under_bound_fence(pin, run, "123", fingerprint, stopped, apply, api,
                                              preflight_epoch=preflight_epoch)
        apply.assert_not_called()
        job["conclusion"] = "success"
        retained = job["steps"].pop(1)
        with self.assertRaisesRegex(GateError, "production_inverse_step_invalid"):
            preflight.apply_under_bound_fence(pin, run, "123", fingerprint, stopped, apply, api,
                                              preflight_epoch=preflight_epoch)
        apply.assert_not_called()
        job["steps"].insert(1, retained)
        artifact = {"archive_digest": "sha256:" + "f" * 64}
        with (patch.object(preflight, "authenticated_plan", return_value=(plan, artifact)),
              patch.object(preflight, "verify_environment") as environment,
              patch.object(preflight, "verify_check", return_value=(789, now)) as approval):
            with self.assertRaisesRegex(GateError, "production_forward_plan_changed"):
                preflight.apply_under_bound_fence(pin, run, "123", "0" * 64, stopped, apply, api,
                                                  preflight_epoch=preflight_epoch)
            apply.assert_not_called()
            result = preflight.apply_under_bound_fence(pin, run, "123", fingerprint, stopped, apply, api,
                                                       preflight_epoch=preflight_epoch)
        environment.assert_called_once_with(api)
        self.assertEqual(approval.call_args.args[:3], (api, run, "production-apply:123:" + "f" * 64))
        apply.assert_called_once_with("123", plan)
        self.assertEqual(result["stopped_epoch"], 18)
        self.assertEqual(result["attendance_fence_sha256"], "c" * 64)

    def test_plan_only_exports_the_fingerprint_after_the_target_matches(self):
        pin, plan = self.preflight_fixture()
        target = plan["target"]
        run = {"id": 456, "run_attempt": 1, "head_sha": pin.workflow_sha}
        with (TemporaryDirectory() as temp,
              patch.object(release, "release_request", return_value=(pin, run, "123")),
              patch.object(release, "approved_plan", return_value=(plan, {})) as approve,
              patch.object(release, "database") as database,
              patch("sys.stderr")):
            output = Path(temp) / "output"
            env = {"RUNNER_TEMP": temp, "GITHUB_OUTPUT": str(output), "RELEASE_RUNNER": "fixture-runner",
                   **{f"PRODUCTION_{key.upper()}": value for key, value in target.items()}}
            database.return_value = {**plan, "target": {**target, "branch_id": "other-branch"}}
            self.assertEqual(release.main(["plan"], env), 1)
            self.assertFalse(output.exists())
            database.return_value = plan
            self.assertEqual(release.main(["plan"], env), 0)
            approve.assert_called_with(pin, run, "123", target)
            self.assertEqual(output.read_text(), f"target-fingerprint={digest_json(target)}\n")

    def test_prepare_retains_the_epoch_it_exports(self):
        pin, plan = self.preflight_fixture()
        run = {"id": 456, "run_attempt": 1, "head_sha": pin.workflow_sha}
        artifact = {"archive_digest": "sha256:" + "f" * 64}
        with (TemporaryDirectory() as temp,
              patch.object(release, "release_context", return_value=(pin, run, "123", plan, artifact)),
              patch.object(release, "check_url"),
              patch.object(release, "validate_attendance_plan"), patch.object(release, "require_unused_attendance_plan"),
              patch.object(release, "control_client"), patch.object(release, "RollbackClient"),
              patch.object(release, "validate_receipt_against_live"), patch.object(release, "image_in_registry"),
              patch.object(release, "verify_preflight", return_value=17)):
            directory = Path(temp) / "production-release"
            directory.mkdir()
            release.write_json(directory / "fresh-plan.json", plan)
            output = Path(temp) / "output"
            env = {"RUNNER_TEMP": temp, "GITHUB_OUTPUT": str(output),
                   "PRODUCTION_WORKER_URL": "https://fixture.invalid", "OWNERSHIP_CONTROL_TOKEN": "fixture",
                   "CLOUDFLARE_ACCOUNT_ID": "fixture", "CLOUDFLARE_API_TOKEN": "fixture"}
            self.assertEqual(release.main(["prepare"], env), 0)
            intent = json.loads((directory / "production-release-stop-intent.json").read_text())
            self.assertEqual(intent, preflight.stop_intent(pin, run, "123", plan, artifact, 17))
            self.assertIn(f"epoch={intent['epoch']}", output.read_text().splitlines())

    def test_stop_refuses_missing_or_changed_intent_before_fencing(self):
        pin, plan = self.preflight_fixture()
        run = {"id": 456, "run_attempt": 1, "head_sha": pin.workflow_sha}
        artifact = {"archive_digest": "sha256:" + "f" * 64}
        intent = preflight.stop_intent(pin, run, "123", plan, artifact, 17)
        with (TemporaryDirectory() as temp,
              patch.object(release, "release_context", return_value=(pin, run, "123", plan, artifact)),
              patch.object(release, "check_url"), patch.object(release, "control_client") as control,
              patch.object(release, "require_unused_attendance_plan") as unused,
              patch.object(release, "read_status") as observe, patch("sys.stderr")):
            env = {"RUNNER_TEMP": temp, "PRODUCTION_WORKER_URL": "https://fixture.invalid",
                   "OWNERSHIP_CONTROL_TOKEN": "fixture"}
            self.assertEqual(release.main(["stop"], env), 1)
            path = Path(temp) / "production-release" / "production-release-stop-intent.json"
            for key, value in (("epoch", None), ("epoch", True), ("target_fingerprint", "0" * 64)):
                with self.subTest(key=key, value=value):
                    release.write_json(path, {**intent, key: value})
                    self.assertEqual(release.main(["stop"], env), 1)
            observe.assert_not_called()
            unused.assert_not_called()
            control.assert_not_called()
            observe.return_value = {
                "worker": {"version_id": pin.rollback_worker, "sha": pin.rollback_sha, "traffic_percent": 100},
                "container": {"image_digest": pin.rollback_image, "application_id": pin.application_id,
                              "namespace_id": pin.namespace_id},
                "ownership": {"phase": "active", "running": True, "epoch": 17},
            }
            release.write_json(path, {**intent, "epoch": 18})
            self.assertEqual(release.main(["stop"], env), 1)
            unused.assert_called_once_with(release.github, artifact, run)
            observe.assert_called_once_with(env)
            control.return_value.assert_not_called()

    def test_post_commit_requires_an_authentic_forward_result_for_takeover_or_recovery(self):
        pin, plan = self.preflight_fixture()
        run = {"id": 456, "run_attempt": 1, "head_sha": pin.workflow_sha}
        fixture = Path(__file__).with_name("fixtures") / "production_release_forward_stop.json"
        proof = json.loads(fixture.read_text())
        artifact = proof["plan_artifact"]
        env = {**proof["forward_artifact_env"], "RELEASE_STOPPED": json.dumps(proof["stop_ack"]),
               "RELEASE_TARGET_FINGERPRINT": digest_json(plan["target"]), "CANDIDATE_WORKER": ""}
        document = {**forward_receipt("123", plan, {**self.outcome, "recovery_evidence_ref": pin.recovery_reference()}),
                    "run_id": "456", "run_attempt": 1, "stopped_epoch": 18,
                    "plan_archive_sha256": artifact["archive_digest"], "release_reference": pin.recovery_reference()}
        api = Mock()
        with (patch.object(release, "release_request", return_value=(pin, run, "123")),
              patch.object(preflight, "authenticated_plan", return_value=(plan, artifact)),
              patch.object(preflight, "verify_environment"),
              patch.object(preflight, "verify_check", side_effect=GateError("independent_production_approval_stale")) as check,
              patch.object(release.artifacts, "protected_job") as protected_job,
              patch.object(release.artifacts, "authentic_document", return_value=document) as authenticate):
            with self.assertRaisesRegex(GateError, "independent_production_approval_stale"):
                release.release_context(env, api)
            check.reset_mock()
            result = release.applied_release(env, api)[-1]
            check.assert_not_called()
            self.assertEqual(result.fence_epoch, 18)
            protected_job.assert_called_once_with(run, api, release.artifacts.FORWARD_JOB,
                                                  release.artifacts.FORWARD_STEPS)
            authenticate.assert_called_once_with(run, release.artifacts.FORWARD_ARTIFACT, "789", "a" * 64, api)
            recovered, _, _, approval, _ = recovery_workflow.release_admission(env)
            self.assertEqual(recovered, result)
            self.assertEqual(approval["plan_archive_sha256"], artifact["archive_digest"])
            check.assert_not_called()
            for key in ("run_attempt", "stopped_epoch", "restored_ledgers_sha256", "attendance_fence_sha256",
                        "plan_archive_sha256"):
                with self.subTest(key=key), self.assertRaises(GateError):
                    authenticate.return_value = {**document, key: "foreign"}
                    release.applied_release(env, api)
            protected_job.side_effect = GateError("protected_job_failed")
            authenticate.reset_mock()
            with self.assertRaisesRegex(GateError, "protected_job_failed"):
                release.applied_release(env, api)
            authenticate.assert_not_called()

    def test_takeover_cli_passes_the_ownership_client(self):
        pin, plan = self.preflight_fixture()
        run = {"id": 456}
        artifact = {"archive_digest": "sha256:" + "8" * 64}
        recovery = Mock()
        worker_url = "https://prod.invalid"
        uploaded_version = "12345678-1234-4567-8123-123456789aaa"
        with (TemporaryDirectory() as temp,
              patch.object(release, "applied_release", return_value=(pin, run, "123", plan, artifact, recovery)),
              patch.object(release, "check_url", return_value=worker_url),
              patch.object(release, "control_client") as control_client,
              patch.object(release, "take_over") as take_over):
            env = {"RUNNER_TEMP": temp, "PRODUCTION_WORKER_URL": worker_url,
                   "OWNERSHIP_CONTROL_TOKEN": "fixture", "NEW_VERSION": uploaded_version}
            self.assertEqual(release.main(["takeover"], env), 0)
        control_client.assert_called_once_with("production", "fixture", worker_url, "",
                                               f"production-schema-release:456:{pin.candidate_sha}")
        take_over.assert_called_once()
        release_pin, proven_recovery, version, _, control = take_over.call_args.args
        self.assertEqual((release_pin, proven_recovery, version), (pin, recovery, uploaded_version))
        self.assertIs(control, control_client.return_value)

    def test_takeover_refuses_a_provider_version_different_from_the_uploaded_worker(self):
        data = json.loads(Path(__file__).with_name("fixtures").joinpath("production_ownership_status_provider.json").read_text())
        provider, takeover = data["provider"], data["release_takeover"]
        pin = replace(self.preflight_fixture()[0], **takeover["pin"])
        account = "c" * 32
        client = Mock(api=provider.__getitem__)
        state = {**provider_sample(client, account), "ownership": takeover["fenced_owner"]}
        output = takeover["upload_output"]
        version = release.produced_version(output)
        for invalid in ("", output + output, "Current Version ID: abcd-1234\n", "Current Version ID: not-a-uuid\n"):
            with self.subTest(invalid=invalid), self.assertRaises(GateError):
                release.produced_version(invalid)
        recovery = Mock(run_id="456", fence_epoch=18)
        foreign = {**state, "worker": {**state["worker"], "version_id": pin.rollback_worker}}
        status = call("status", expected_deployment=version)
        exact_takeover = call("takeover-fenced", 18, expected_deployment=version)
        control = Mock()
        with self.assertRaisesRegex(GateError, "production_release_candidate_pair_unproved"):
            release.take_over(pin, recovery, version, Mock(return_value=foreign), control)
        self.assertEqual(control.call_args_list, [status])
        prior = {**foreign, "worker": {**foreign["worker"], "sha": pin.rollback_sha}}
        prior["container"] = {**state["container"], "image_digest": pin.rollback_image}
        provider[f"/containers/applications/{pin.application_id}/rollouts?limit=100"][0]["status"] = "progressing"
        progressing = {**provider_sample(client, account, allow_rollout=True), "ownership": state["ownership"]}
        effects, pause = Mock(), Mock()
        effects.observe.side_effect = [prior, progressing, state]
        effects.control.side_effect = [None, GateError("lost_ack")]
        with self.assertRaisesRegex(GateError, "lost_ack"):
            release.take_over(pin, recovery, version, effects.observe, effects.control, pause=pause)
        self.assertEqual(effects.mock_calls, [call.control("status", expected_deployment=version),
                         call.observe(), call.observe(), call.observe(),
                         call.control("takeover-fenced", 18, expected_deployment=version)])
        self.assertEqual(pause.call_count, 2)
        owner = {**state["ownership"], "phase": "active", "epoch": 19}
        control = Mock()
        release.take_over(pin, recovery, version, Mock(side_effect=[state, {**state, "ownership": owner}]), control)
        owner["running"] = True
        with self.assertRaisesRegex(GateError, "production_release_takeover_unconfirmed"):
            release.take_over(pin, recovery, version, Mock(side_effect=[state, {**state, "ownership": owner}]), control)
        self.assertEqual(control.call_args_list, [status, exact_takeover] * 2)
