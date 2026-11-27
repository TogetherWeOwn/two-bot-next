"""Synthetic checks for the production recovery handoffs and failure ordering."""

from contextlib import redirect_stderr
from copy import deepcopy
from dataclasses import asdict, replace
from io import StringIO
import json
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import Mock, call, patch

ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / "scripts/ci/fixtures/production_inverse_admission.json"
sys.path.insert(0, str(ROOT / "scripts"))
import production_recovery_artifact as artifacts
import production_recovery_workflow as recovery
import production_release as release_cli
import production_release_interrupted as interrupted
import production_release_preflight as preflight
import staging_recovery_status
from production_recovery_takeover import RecoveryPin
from staging_migrate_approval import digest_json
from staging_rollout import GateError
from test_production_recovery_artifact import artifact_archive, producer, release_run_fixture
from worker_container_rollback import require_ordinary_production_rollback, source_has_attendance_schema


class RecoveryWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.case = json.loads(FIXTURE.read_text(encoding="utf-8"))
        admission = self.case["admission"]
        self.pin = RecoveryPin(**admission["pin"])
        self.candidate = (admission["candidate_worker"], self.pin.candidate_sha, admission["candidate_image"])
        self.result = {"state": "restored", **asdict(self.pin)}
        self.state = {
            "worker": {"version_id": self.candidate[0], "sha": self.candidate[1], "traffic_percent": 100},
            "container": {"application_id": self.pin.application_id, "namespace_id": self.pin.namespace_id,
                          "image_digest": self.candidate[2], "rollout_status": "completed", "rollout_id": "rollout"},
            "ownership": {"epoch": 21, "running": False, "phase": "fenced",
                          "owner_actor": recovery.fence_actor(self.pin)},
        }
        self.ack = {"deploymentId": self.candidate[0], "running": False,
                    "owner": {"deploymentId": None, "phase": "fenced", "actor": recovery.fence_actor(self.pin),
                              "epoch": 21, "oldEpoch": 20}}
        self.observe = Mock(side_effect=lambda: deepcopy(self.state))
        self.control = Mock()
        self.rollback = Mock()

    def test_interrupted_restore_planning_requires_current_exact_inverse(self):
        pin, plan = self.pin, self.case["plan"]
        release = Mock(candidate_sha=pin.candidate_sha, rollback_sha=pin.rollback_sha, rollback_worker=pin.worker_version,
                       rollback_image=pin.image_digest, application_id=pin.application_id, namespace_id=pin.namespace_id)
        intent = {"epoch": pin.fence_epoch - 1}
        admission = {"binding": {"run_id": "456", "workflow_sha": "d" * 40, "stop_intent": intent}, "phase": "stopped"}
        decision = {"admission": admission, "classification": {
            "state": "post", "attendance_fence_sha256": pin.attendance_fence_sha256}}
        exact = interrupted.recovery_pin(release, pin.run_id, pin.fence_epoch, plan, pin.attendance_fence_sha256)
        result = {"state": "restored", **asdict(exact)}
        proof = {"decision_sha256": digest_json(decision), "result": result}
        for inverse, refusal, may_plan in (
            (GateError("unavailable"), "unavailable", False),
            ({**proof, "result": {**result, "worker_version": self.candidate[0]}}, "production_inverse_readback_mismatch", False),
            ({**proof, "decision_sha256": "0" * 64}, "production_interrupted_inverse_decision_changed", False),
            (proof, "fixture reached provider planning", True),
        ):
            planner = Mock(side_effect=GateError("fixture reached provider planning"))
            context = (release, {"id": int(pin.run_id)}, "121", plan, {}, pin.fence_epoch)
            with (self.subTest(refusal=refusal), TemporaryDirectory() as temp,
                  patch.multiple(interrupted, interrupted_release_context=Mock(return_value=context),
                                 stop_intent=Mock(return_value=intent), packet=Mock(side_effect=[admission, decision, inverse]),
                                 environment_policy=planner)):
                env = dict(GITHUB_RUN_ID="456", GITHUB_SHA="d" * 40, RUNNER_TEMP=temp)
                with self.assertRaisesRegex(GateError, f"^{refusal}$"):
                    interrupted.execute(["restart"], env, Mock())
                self.assertEqual(planner.call_count, int(may_plan))
                self.assertFalse((Path(temp) / "interrupted-production-release/production-release-interrupted-restored.json").exists())
                self.assertEqual(interrupted.packet.call_args.args[3:6], (
                    (interrupted.CLASSIFY_STEP, interrupted.DECISION_STEP, *interrupted.INVERSE_STEPS),
                    interrupted.INVERSE, "INVERSE"))

    def test_inverse_requires_exact_durable_stop_and_protected_job_success(self):
        run = self.case["run"]
        job = self.case["fence_job"]
        api = Mock(return_value={"total_count": 1, "jobs": [job]})
        inverse = Mock(return_value=self.result)
        for ack in ({**self.ack, "running": True}, {**self.ack, "deploymentId": "other"},
                    {**self.ack, "owner": {**self.ack["owner"], "epoch": 22}},
                    {**self.ack, "owner": {**self.ack["owner"], "actor": "wrong"}}):
            with self.subTest(ack=ack), self.assertRaises(GateError):
                recovery.inverse_under_fence(self.pin, self.candidate, ack, run, api, inverse)
        job["conclusion"] = "cancelled"
        with self.assertRaises(GateError):
            recovery.inverse_under_fence(self.pin, self.candidate, self.ack, run, api, inverse)
        inverse.assert_not_called()
        job["conclusion"] = "success"
        self.assertEqual(recovery.inverse_under_fence(self.pin, self.candidate, self.ack, run, api, inverse), self.result)
        inverse.assert_called_once_with()

    def test_stop_requires_matching_provider_state_after_the_ack(self):
        # The first read is a running active writer at the previous epoch.
        before = deepcopy(self.state)
        before["ownership"].update(epoch=20, running=True, phase="active")
        after = deepcopy(self.state)
        after["worker"]["deployment_id"] = "changed"
        self.control.return_value = self.ack
        with self.assertRaisesRegex(GateError, "production_inverse_serving_pair_changed"):
            recovery.establish_fence(replace(self.pin, fence_epoch=19), self.candidate,
                                     Mock(side_effect=[before, after]), self.control)
        self.control.assert_called_once_with("fence-exact", 20, expected_deployment=self.candidate[0])

    def forward_fixture(self, run):
        case = self.case
        plan, artifact, plan_id = case["plan"], case["artifact"], case["admission"]["plan_run_id"]
        release = Mock(**asdict(self.pin), workflow_sha=run["head_sha"], rollback_worker=self.pin.worker_version,
                       rollback_image=self.pin.image_digest, candidate_image=self.candidate[2])
        release.recovery_reference.return_value = plan["recovery_evidence_ref"]
        fields = ("database_fingerprint", "expanded_ledgers_sha256", "restored_ledgers_sha256", "attendance_fence_sha256")
        document = {**{field: getattr(self.pin, field) for field in fields}, "plan_run_id": plan_id,
                    "source_sha": self.pin.candidate_sha, "run_id": self.pin.run_id, "run_attempt": 1,
                    "stopped_epoch": self.pin.fence_epoch, "release_reference": plan["recovery_evidence_ref"],
                    "plan_archive_sha256": artifact["archive_digest"]}
        stopped = {**self.ack, "deploymentId": self.pin.worker_version}
        env = {"RELEASE_STOPPED": json.dumps(stopped), "RELEASE_PREFLIGHT_EPOCH": "20",
               "FORWARD_ARTIFACT_ID": "789", "FORWARD_ARCHIVE_SHA256": "8" * 64, "CANDIDATE_WORKER": self.candidate[0],
               "PRODUCTION_WORKER_URL": "https://fixture.invalid", "RELEASE_TARGET_FINGERPRINT": self.pin.database_fingerprint}
        return (release, run, plan_id, plan, artifact), document, env

    def test_admission_binds_the_rust_inverse_ledger_rows(self):
        case = self.case
        context, document, env = self.forward_fixture({**case["run"], "run_attempt": 1})
        release, run, plan_id, plan, artifact = context
        with (patch.object(release_cli, "release_request", return_value=(release, run, plan_id)),
              patch.multiple(preflight, authenticated_plan=Mock(return_value=(plan, artifact)), verify_environment=Mock()),
              patch.object(preflight, "verify_check", side_effect=GateError("unapproved")) as check,
              patch.object(artifacts, "protected_job") as producer,
              patch.object(artifacts, "authentic_document", side_effect=GateError("unproved")) as authenticate,
              patch.object(recovery, "check_url") as provider_url):
            with self.assertRaisesRegex(GateError, "unapproved"):
                release_cli.release_context(env)
            check.assert_called_once()
            self.assertEqual(check.call_args.args[2], "production-apply:321:" + "8" * 64)
            check.reset_mock()

            self.assertEqual(recovery.main(["fence"], {**env, "RELEASE_PREFLIGHT_EPOCH": "19"}), 1)
            producer.assert_not_called()
            authenticate.assert_not_called()
            self.assertEqual(recovery.main(["fence"], env), 1)
            producer.assert_called_once_with(run, recovery.github, artifacts.FORWARD_JOB, artifacts.FORWARD_STEPS)
            authenticate.assert_called_once_with(run, artifacts.FORWARD_ARTIFACT, "789", "8" * 64, recovery.github)
            provider_url.assert_not_called()
            authenticate.side_effect, authenticate.return_value = None, document
            pin, candidate, reviewed, approval, _ = recovery.release_admission(env)
            self.assertEqual((pin, candidate, reviewed, approval["plan_archive_sha256"]),
                             (self.pin, self.candidate, plan, artifact["archive_digest"]))
            for field, digest in case["source_ledger_digests"].items():
                authenticate.return_value = {**document, field: digest}
                with self.subTest(field=field), self.assertRaisesRegex(GateError, "production_forward_artifact_mismatch"):
                    recovery.release_admission(env)
            check.assert_not_called()

    def test_restore_refuses_receipts_with_wrong_or_missing_ownership_epoch(self):
        run_env, run = release_run_fixture()
        context, forward, env = self.forward_fixture(run)
        release, _, plan_id, plan, plan_artifact = context
        jobs = [producer(run, artifacts.FORWARD_JOB, artifacts.FORWARD_STEPS),
                producer(run, artifacts.JOB, (artifacts.WRITE_STEP, artifacts.UPLOAD_STEP))]
        bad_epoch = self.pin.fence_epoch + 2
        cases = (
            ("matching epochs", forward, self.result, True, True),
            ("wrong forward epoch", {**forward, "stopped_epoch": bad_epoch}, self.result, False, False),
            ("missing forward epoch", {k: v for k, v in forward.items() if k != "stopped_epoch"},
             self.result, False, False),
            ("wrong inverse epoch", forward, {**self.result, "fence_epoch": bad_epoch}, True, False),
            ("missing inverse epoch", forward, {k: v for k, v in self.result.items() if k != "fence_epoch"},
             True, False),
        )
        for name, forward_doc, inverse_result, fetch_inverse, admitted in cases:
            with self.subTest(case=name):
                # Repack each changed receipt and bind its actual digest in the
                # workflow inputs and inverse approval. Only the epoch differs.
                forward_digest, forward_entry, forward_routes = artifact_archive(
                    run, artifacts.FORWARD_ARTIFACT, 789, forward_doc)
                approval = {"plan_run_id": plan_id, "plan_archive_sha256": plan_artifact["archive_digest"],
                            "workflow_sha": run["head_sha"], "recovery_ref": plan["recovery_evidence_ref"],
                            "candidate_worker": self.candidate[0], "candidate_image": self.candidate[2],
                            "forward_artifact_id": "789", "forward_archive_sha256": forward_digest}
                inverse_digest, inverse_entry, inverse_routes = artifact_archive(
                    run, artifacts.ARTIFACT, 987,
                    {"operation": "undo-0480", "approval": approval, "result": inverse_result})
                root = f"actions/runs/{run['id']}"
                routes = {root: run, f"{root}/attempts/1/jobs?per_page=30": {"total_count": len(jobs), "jobs": jobs},
                          f"{root}/artifacts?per_page=30": {"total_count": 2, "artifacts": [forward_entry, inverse_entry]},
                          **forward_routes, **inverse_routes}
                api = Mock(side_effect=lambda path, **_: routes[path.removeprefix(f"repos/{artifacts.REPO}/")])
                restore_env = {**run_env, **env, "FORWARD_ARCHIVE_SHA256": forward_digest,
                               "STOPPED_FENCE_ACK": env["RELEASE_STOPPED"], "ARTIFACT_ID": "987",
                               "ARCHIVE_SHA256": inverse_digest}
                boundary = Mock(side_effect=AssertionError("provider boundary reached"))
                blocked = Mock(side_effect=AssertionError("unexpected provider, database or receipt operation"))
                with (patch.object(staging_recovery_status, "gh", api),
                      patch.object(release_cli, "release_request", return_value=(release, run, plan_id)),
                      patch.multiple(preflight, authenticated_plan=Mock(return_value=(plan, plan_artifact)),
                                     verify_environment=Mock()),
                      patch.multiple(recovery, check_url=boundary, RollbackClient=blocked, read_status=blocked,
                                     control_client=blocked, run_inverse=blocked, write_json=blocked),
                      redirect_stderr(StringIO())):
                    if admitted:
                        with self.assertRaisesRegex(AssertionError, "^provider boundary reached$"):
                            recovery.main(["restore"], restore_env)
                        boundary.assert_called_once_with("production", env["PRODUCTION_WORKER_URL"])
                    else:
                        self.assertEqual(recovery.main(["restore"], restore_env), 1)
                        boundary.assert_not_called()
                downloads = [request for request in api.call_args_list if request.kwargs.get("binary")]
                expected = [call(f"repos/{artifacts.REPO}/actions/artifacts/789/zip", binary=True)]
                if fetch_inverse:
                    expected.append(call(f"repos/{artifacts.REPO}/actions/artifacts/987/zip", binary=True))
                self.assertEqual(downloads, expected)
                blocked.assert_not_called()

    def test_inverse_refuses_incomplete_database_readback(self):
        plan = dict(target=dict(host="db", database="fixture", branch_id="branch"),
                    acl_plan_ref="acl", plan_manifest_sha256="plan", store_plan_manifest_sha256="store")
        approval = {"recovery_ref": "ref", "plan_run_id": "321"}
        env = {"TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL": "fixture"}
        with TemporaryDirectory() as scratch, patch.object(recovery.subprocess, "run") as run:
            run.return_value = Mock(returncode=0, stdout=b'{"state":"restored"}')
            with self.assertRaisesRegex(GateError, "production_inverse_database_readback_invalid"):
                recovery.run_inverse("fixture", Path(scratch), self.pin, plan, approval, env)

    def restore(self):
        recovery.restore_under_fence(self.pin, self.result, self.observe, self.control, self.rollback,
                                     "test-image", self.candidate)

    def test_post_commit_provider_failures_leave_the_writer_stopped(self):
        self.rollback.post_worker_restore.side_effect = GateError("provider_write_refused")
        with self.assertRaisesRegex(GateError, "provider_write_refused"):
            self.restore()
        self.control.assert_not_called()
        self.rollback.restore_image.assert_not_called()
        self.rollback.wait_stable.assert_not_called()

        self.rollback.reset_mock(side_effect=True)
        self.state["worker"].update(version_id=self.pin.worker_version, sha=self.pin.rollback_sha)
        self.rollback.restore_image.side_effect = GateError("provider_rollout_refused")
        with self.assertRaisesRegex(GateError, "provider_rollout_refused"):
            self.restore()
        self.rollback.post_worker_restore.assert_not_called()
        self.control.assert_not_called()
        self.rollback.wait_stable.assert_not_called()

    def test_pending_prior_rollout_keeps_the_writer_stopped(self):
        self.state["worker"].update(version_id=self.pin.worker_version, sha=self.pin.rollback_sha)
        self.state["container"].update(image_digest=self.pin.image_digest, rollout_status="progressing")
        with self.assertRaisesRegex(GateError, "production_recovery_rollout_pending"):
            self.restore()
        self.assertEqual(self.rollback.sleep.call_count, recovery.WORKER_RESTORE_READS - 1)
        self.rollback.restore_image.assert_not_called()
        self.control.assert_not_called()
        self.rollback.wait_stable.assert_not_called()

    def test_recovery_waits_for_an_exact_rollout_before_image_write_or_takeover(self):
        for digest in (self.candidate[2], self.pin.image_digest):
            with self.subTest(digest=digest):
                self.rollback.reset_mock(side_effect=True)
                self.control.reset_mock()
                self.state["worker"].update(version_id=self.pin.worker_version, sha=self.pin.rollback_sha)
                self.state["container"].update(image_digest=digest, rollout_status="progressing")
                self.state["ownership"].update(phase="fenced", epoch=21, owner_actor=recovery.fence_actor(self.pin))

                def finish_rollout(_delay):
                    self.rollback.restore_image.assert_not_called()
                    self.control.assert_not_called()
                    self.state["container"]["rollout_status"] = "completed"

                self.rollback.sleep.side_effect = finish_rollout
                self.rollback.restore_image.side_effect = lambda _image: self.state["container"].update(
                    image_digest=self.pin.image_digest)
                self.control.side_effect = lambda *_args, **_kwargs: self.state["ownership"].update(
                    epoch=22, phase="active", owner_actor=self.pin.actor)
                self.restore()
                self.rollback.sleep.assert_called_once()
                self.assertEqual(self.rollback.restore_image.call_count, int(digest == self.candidate[2]))
                self.control.assert_called_once_with("takeover-fenced", 21, expected_deployment=self.pin.worker_version)

    def test_recovery_refuses_changed_rollout_or_stop_while_waiting(self):
        self.state["worker"].update(version_id=self.pin.worker_version, sha=self.pin.rollback_sha)
        self.state["container"]["rollout_status"] = "progressing"
        for group, key, value in (("container", "rollout_id", "new-rollout"), ("ownership", "owner_actor", "other")):
            with self.subTest(key=key), patch.dict(self.state[group]):
                self.rollback.sleep.side_effect = lambda _delay: self.state[group].update({key: value})
                with self.assertRaisesRegex(GateError, "production_(inverse_serving_pair_changed|stopped_fence_unproved)"):
                    self.restore()
                self.rollback.restore_image.assert_not_called()
                self.control.assert_not_called()

    def test_post_commit_recovery_reuses_the_forward_stop_and_refuses_an_unknown_worker(self):
        self.assertEqual(recovery.establish_fence(self.pin, self.candidate, self.observe, self.control),
                         (self.pin, None))
        with self.assertRaisesRegex(GateError, "production_inverse_serving_pair_changed"):
            recovery.establish_fence(self.pin, ("", *self.candidate[1:]), self.observe, self.control)
        self.state["worker"].update(version_id=self.pin.worker_version, sha=self.pin.rollback_sha)
        self.assertEqual(recovery.establish_fence(self.pin, ("", *self.candidate[1:]), self.observe, self.control),
                         (self.pin, None))
        self.control.assert_not_called()

    def test_watch_breach_refences_only_the_exact_active_candidate_at_the_next_epoch(self):
        initial = replace(self.pin, fence_epoch=19)
        self.state["ownership"].update(epoch=20, phase="active", running=True)

        def fence(*_args, **_kwargs):
            self.state["ownership"].update(epoch=21, phase="fenced", running=False)
            return self.ack

        self.control.side_effect = fence
        self.assertEqual(recovery.establish_fence(initial, self.candidate, self.observe, self.control),
                         (self.pin, self.ack))
        self.control.assert_called_once_with("fence-exact", 20, expected_deployment=self.candidate[0])
        self.control.reset_mock()
        self.state["ownership"].update(epoch=20, phase="fenced")
        with self.assertRaisesRegex(GateError, "production_inverse_fence_epoch_changed"):
            recovery.establish_fence(initial, self.candidate, self.observe, self.control)
        self.control.assert_not_called()

    def test_one_time_takeover_reconciles_lost_reply_before_readiness(self):
        self.pin = replace(self.pin, rollback_sha="d314f4c63ecbea263aa953bc8a649a444a97da7c")
        self.result = {"state": "restored", **asdict(self.pin)}

        def worker(version):
            self.state["worker"].update(version_id=version, sha=self.pin.rollback_sha)

        def image(_):
            self.state["container"]["image_digest"] = self.pin.image_digest

        def lost_reply(*_args, **_kwargs):
            self.state["ownership"].update(epoch=22, phase="active", owner_actor=self.pin.actor)
            raise GateError("ownership_control_unavailable")

        self.rollback.post_worker_restore.side_effect = worker
        self.rollback.restore_image.side_effect = image
        self.control.side_effect = lost_reply
        self.restore()
        self.restore()
        self.rollback.post_worker_restore.assert_called_once_with(self.pin.worker_version)
        self.rollback.restore_image.assert_called_once_with("test-image")
        self.control.assert_called_once_with("takeover-legacy-fenced", 21, expected_deployment=self.pin.worker_version)
        self.assertEqual(self.rollback.wait_stable.call_count, 2)

    def test_restore_after_second_fence_refuses_the_old_handoff(self):
        old_pin = replace(self.pin, fence_epoch=19)
        old_ack = {**self.ack, "deploymentId": self.pin.worker_version,
                   "owner": {**self.ack["owner"], "epoch": 19, "oldEpoch": 18}}
        self.state["ownership"].update(epoch=20, phase="active", running=True)

        def fence(*_args, **_kwargs):
            self.state["ownership"].update(epoch=21, phase="fenced", running=False)
            return self.ack

        self.control.side_effect = fence
        pin, ack = recovery.establish_fence(old_pin, self.candidate, self.observe, self.control)
        self.assertEqual((pin, ack), (self.pin, self.ack))
        self.control.assert_called_once_with("fence-exact", 20, expected_deployment=self.candidate[0])
        self.control.reset_mock()
        self.observe.reset_mock()

        # Both handoffs have a matching inverse receipt. The live fence must
        # still rule out the old handoff before any restore or takeover writes.
        old_result = {**self.result, "fence_epoch": old_pin.fence_epoch}
        inverse = Mock(side_effect=[old_result, self.result])
        write_receipt = Mock()
        approval = {"plan_run_id": self.case["admission"]["plan_run_id"]}
        admission = (old_pin, self.candidate, self.case["plan"], approval, self.case["run"])
        env = {"PRODUCTION_WORKER_URL": "https://fixture.invalid", "CLOUDFLARE_ACCOUNT_ID": "fixture",
               "CLOUDFLARE_API_TOKEN": "fixture", "OWNERSHIP_CONTROL_TOKEN": "fixture", "RUNNER_TEMP": "fixture",
               "ARTIFACT_ID": "987", "ARCHIVE_SHA256": "8" * 64}
        application = recovery.APPLICATIONS["production"]
        retained_image = f"registry.cloudflare.com/fixture/{application}@{pin.image_digest}"

        def take_over(*_args, **_kwargs):
            self.rollback.wait_stable.assert_not_called()
            self.state["ownership"].update(epoch=22, phase="active", owner_actor=pin.actor)

        def record_stable_polls(_version, _started_at):
            self.rollback.evidence["stable_polls"] = 3

        self.control.side_effect = take_over
        self.rollback.post_worker_restore.side_effect = lambda version: self.state["worker"].update(
            version_id=version, sha=pin.rollback_sha)
        self.rollback.restore_image.side_effect = lambda image: self.state["container"].update(
            image_digest=image.rsplit("@", 1)[-1])
        self.rollback.evidence = {}
        self.rollback.wait_stable.side_effect = record_stable_polls
        with (patch.multiple(recovery, release_admission=Mock(return_value=admission),
                             authenticated_inverse=inverse, write_json=write_receipt,
                             read_status=Mock(side_effect=lambda *_args, **_kwargs: self.observe()),
                             check_url=Mock(return_value=env["PRODUCTION_WORKER_URL"]), RollbackClient=Mock(),
                             validate_receipt_against_live=Mock(), control_client=Mock(return_value=self.control),
                             Rollback=Mock(return_value=self.rollback)), patch("sys.stderr")):
            stale = {**env, "STOPPED_FENCE_ACK": json.dumps(old_ack), "ARTIFACT_ID": "986",
                     "ARCHIVE_SHA256": "7" * 64}
            self.assertEqual(recovery.main(["restore"], stale), 1)
            inverse.assert_called_once_with(old_pin, approval, "986", "7" * 64, stale)
            self.observe.assert_called_once_with()
            self.assertEqual(self.rollback.mock_calls, [])
            self.control.assert_not_called()
            write_receipt.assert_not_called()

            current = {**env, "STOPPED_FENCE_ACK": json.dumps(ack)}
            self.assertEqual(recovery.main(["restore"], current), 0)
            self.assertEqual(inverse.call_count, 2)
            inverse.assert_called_with(pin, approval, "987", "8" * 64, current)

        self.rollback.post_worker_restore.assert_called_once_with(pin.worker_version)
        self.rollback.restore_image.assert_called_once_with(retained_image)
        self.control.assert_called_once_with("takeover-fenced", 21, expected_deployment=pin.worker_version)
        self.rollback.wait_stable.assert_called_once_with(pin.worker_version, self.rollback.now.return_value)
        write_receipt.assert_called_once_with(Path(env["RUNNER_TEMP"]) / "production-release-restored.json",
                                              {"approval": approval, "inverse": self.result, "provider": self.state,
                                               "stable_polls": 3})

    def test_normal_paths_refuse_cross_schema_and_release_owned_fences(self):
        owner = {"owner": {"phase": "active", "actor": "fixture"}}
        has_schema = lambda sha: sha == self.pin.candidate_sha
        with self.assertRaisesRegex(GateError, "production_rollback_requires_protected_inverse"):
            require_ordinary_production_rollback(self.pin.candidate_sha, self.pin.rollback_sha, owner, has_schema=has_schema)
        owner["owner"].update(phase="fenced", actor=recovery.fence_actor(self.pin))
        with self.assertRaisesRegex(GateError, "production_rollback_fence_reserved"):
            require_ordinary_production_rollback(self.pin.rollback_sha, self.pin.rollback_sha, owner, has_schema=has_schema)
        owner["owner"].update(phase="active")
        with self.assertRaisesRegex(GateError, "production_rollback_requires_protected_inverse"):
            require_ordinary_production_rollback(self.pin.candidate_sha, self.pin.rollback_sha, owner, has_schema=has_schema)
        require_ordinary_production_rollback(self.pin.candidate_sha, self.pin.candidate_sha, owner, has_schema=has_schema)
        with self.assertRaisesRegex(GateError, "production_rollback_source_unproved"):
            source_has_attendance_schema(None)
