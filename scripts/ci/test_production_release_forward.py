"""Synthetic evidence for the forward database release and inverse boundary."""

from copy import deepcopy
from dataclasses import replace
from datetime import datetime, timezone
import json
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from production_release_forward import CUTOVER, STORE, forward_receipt
from production_ownership_status import provider_sample
import production_release as release
import production_release_preflight as preflight
from ci.production_migrate_claim import projection_hash, store_projection_hash
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

    def test_prepare_rejects_agreeing_invalid_pre_plans_before_reading_provider(self):
        pin, plan = self.preflight_fixture()
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
                  patch.object(release, "check_url"), patch.object(release, "control_client"),
                  patch.object(release, "read_status") as observe,
                  patch.object(release, "RollbackClient") as provider):
                self.assertEqual(release.main(["prepare"], env), 1)
            observe.assert_not_called()
            provider.assert_not_called()

    def test_stop_refuses_a_spent_attendance_plan_before_the_provider_call(self):
        pin, plan = self.preflight_fixture()
        run = {"id": 456}
        artifact = {"archive_digest": "sha256:" + "8" * 64}
        with TemporaryDirectory() as temp:
            directory = Path(temp) / "production-release"
            directory.mkdir()
            proof = {"pin": vars(pin), "epoch": 17, "run_id": "456", "plan_run_id": "123",
                     "plan_archive_sha256": artifact["archive_digest"]}
            (directory / "preflight.json").write_text(json.dumps(proof), encoding="utf-8")
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
        run = {"id": 456, "run_attempt": 1, "head_sha": pin.workflow_sha}
        stopped = {"deploymentId": pin.rollback_worker, "running": False,
                   "owner": {"deploymentId": None, "phase": "fenced", "epoch": 18, "oldEpoch": 17,
                             "actor": f"production-schema-release:456:{pin.candidate_sha}"}}
        job = {"name": preflight.PREPARE_JOB, "run_id": 456, "run_attempt": 1, "head_sha": pin.workflow_sha,
               "status": "completed", "conclusion": "success", "steps": [
                   {"name": step, "status": "completed", "conclusion": "success"} for step in preflight.PREPARE_STEPS]}
        now = datetime(2026, 11, 25, tzinfo=timezone.utc)
        api = Mock(side_effect=lambda route: {"created_at": now.isoformat()} if route == "actions/runs/123"
                   else {"total_count": 1, "jobs": [job]})
        apply = Mock(return_value={**self.outcome, "recovery_evidence_ref": pin.recovery_reference()})
        for bad_stop in ({**stopped, "running": True},
                         {**stopped, "owner": {**stopped["owner"], "oldEpoch": 18, "epoch": 19}}):
            with self.subTest(stop=bad_stop), self.assertRaisesRegex(GateError, "production_forward_stopped_fence_unproved"):
                preflight.apply_under_prepared_fence(pin, run, "123", plan["target"], bad_stop, apply, api,
                                                     preflight_epoch=preflight_epoch)
        api.assert_not_called()
        job["conclusion"] = "cancelled"
        with self.assertRaisesRegex(GateError, "production_inverse_job_invalid"):
            preflight.apply_under_prepared_fence(pin, run, "123", plan["target"], stopped, apply, api,
                                                 preflight_epoch=preflight_epoch)
        apply.assert_not_called()
        job["conclusion"] = "success"
        artifact = {"archive_digest": "sha256:" + "f" * 64}
        with (patch.object(preflight, "authenticated_plan", return_value=(plan, artifact)),
              patch.object(preflight, "verify_environment") as environment,
              patch.object(preflight, "verify_check", return_value=(789, now)) as approval):
            result = preflight.apply_under_prepared_fence(pin, run, "123", plan["target"], stopped, apply, api,
                                                          preflight_epoch=preflight_epoch)
        environment.assert_called_once_with(api)
        self.assertEqual(approval.call_args.args[:3], (api, run, "production-apply:123:" + "f" * 64))
        apply.assert_called_once_with("123", plan)
        self.assertEqual(result["stopped_epoch"], 18)
        self.assertEqual(result["attendance_fence_sha256"], "c" * 64)

    def test_post_commit_requires_an_authentic_forward_result_for_takeover_or_recovery(self):
        pin, plan = self.preflight_fixture()
        run = {"id": 456, "run_attempt": 1, "head_sha": pin.workflow_sha}
        fixture = Path(__file__).with_name("fixtures") / "production_release_forward_stop.json"
        proof = json.loads(fixture.read_text())
        artifact = proof["plan_artifact"]
        env = {**proof["forward_artifact_env"], "RELEASE_STOPPED": json.dumps(proof["stop_ack"])}
        document = {**forward_receipt("123", plan, {**self.outcome, "recovery_evidence_ref": pin.recovery_reference()}),
                    "run_id": "456", "run_attempt": 1, "stopped_epoch": 18,
                    "plan_archive_sha256": artifact["archive_digest"], "release_reference": pin.recovery_reference()}
        api = Mock()
        with (patch.object(release.artifacts, "protected_job") as protected_job,
              patch.object(release.artifacts, "authentic_document", return_value=document) as authenticate):
            self.assertEqual(release.forward_document(env, pin, run, "123", plan, artifact, api).fence_epoch, 18)
            protected_job.assert_called_once_with(run, api, release.artifacts.FORWARD_JOB,
                                                  release.artifacts.FORWARD_STEPS)
            authenticate.assert_called_once_with(run, release.artifacts.FORWARD_ARTIFACT, "789", "a" * 64, api)
            for key in ("run_attempt", "stopped_epoch", "restored_ledgers_sha256", "attendance_fence_sha256"):
                with self.subTest(key=key), self.assertRaises(GateError):
                    authenticate.return_value = {**document, key: "foreign"}
                    release.forward_document(env, pin, run, "123", plan, artifact, api)
            protected_job.side_effect = GateError("protected_job_failed")
            authenticate.reset_mock()
            with self.assertRaisesRegex(GateError, "protected_job_failed"):
                release.forward_document(env, pin, run, "123", plan, artifact, api)
            authenticate.assert_not_called()

    def test_takeover_cli_passes_the_ownership_client(self):
        pin, plan = self.preflight_fixture()
        run = {"id": 456}
        artifact = {"archive_digest": "sha256:" + "8" * 64}
        recovery = Mock()
        worker_url = "https://prod.invalid"
        with (TemporaryDirectory() as temp,
              patch.object(release, "release_context", return_value=(pin, run, "123", plan, artifact)),
              patch.object(release, "check_url", return_value=worker_url),
              patch.object(release, "forward_document", return_value=recovery),
              patch.object(release, "control_client") as control_client,
              patch.object(release, "take_over") as take_over):
            env = {"RUNNER_TEMP": temp, "PRODUCTION_WORKER_URL": worker_url,
                   "OWNERSHIP_CONTROL_TOKEN": "fixture", "NEW_VERSION": pin.rollback_worker}
            self.assertEqual(release.main(["takeover"], env), 0)
        control_client.assert_called_once_with("production", "fixture", worker_url, "",
                                               f"production-schema-release:456:{pin.candidate_sha}")
        take_over.assert_called_once()
        release_pin, proven_recovery, version, _, control = take_over.call_args.args
        self.assertEqual((release_pin, proven_recovery, version), (pin, recovery, pin.rollback_worker))
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
        control = Mock(side_effect=GateError("lost_ack"))
        with self.assertRaisesRegex(GateError, "production_release_candidate_pair_unproved"):
            release.take_over(pin, recovery, version, Mock(return_value=foreign), control)
        control.assert_not_called()
        prior = {**foreign, "worker": {**foreign["worker"], "sha": pin.rollback_sha}}
        prior["container"] = {**state["container"], "image_digest": pin.rollback_image}
        provider[f"/containers/applications/{pin.application_id}/rollouts?limit=100"][0]["status"] = "progressing"
        progressing = {**provider_sample(client, account, allow_rollout=True), "ownership": state["ownership"]}
        observe, pause = Mock(side_effect=[prior, progressing, state]), Mock()
        with self.assertRaisesRegex(GateError, "lost_ack"):
            release.take_over(pin, recovery, version, observe, control, pause=pause)
        control.assert_called_once_with("takeover-fenced", 18, expected_deployment=version)
        self.assertEqual(pause.call_count, 2)
        owner = {**state["ownership"], "phase": "active", "epoch": 19}
        control = Mock()
        release.take_over(pin, recovery, version, Mock(side_effect=[state, {**state, "ownership": owner}]), control)
        owner["running"] = True
        with self.assertRaisesRegex(GateError, "production_release_takeover_unconfirmed"):
            release.take_over(pin, recovery, version, Mock(side_effect=[state, {**state, "ownership": owner}]), control)
        self.assertEqual(control.call_count, 2)
