"""Exercise recovery routing after an interrupted protected schema apply."""

from contextlib import redirect_stderr
from copy import deepcopy
from hashlib import sha256
from io import BytesIO, StringIO
import json
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import Mock, patch
from zipfile import ZipFile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import production_recovery_workflow as recovery
import production_release_interrupted as interrupted
import production_release_preflight as preflight
from ci.production_migrate_claim import projection_hash, store_projection_hash
from staging_rollout import GateError


class InterruptedReleaseTests(unittest.TestCase):
    def setUp(self):
        self.pin = preflight.ReleasePreflight(
            workflow_sha="a" * 40,
            candidate_sha="b" * 40,
            candidate_archive="sha256:" + "1" * 64,
            candidate_image="sha256:" + "2" * 64,
            rollback_sha="c" * 40,
            rollback_worker="33333333-3333-4333-8333-333333333333",
            rollback_image="sha256:" + "4" * 64,
            application_id="55555555-5555-4555-8555-555555555555",
            namespace_id="6" * 32,
            restore_proof_sha256="7" * 64,
        )
        self.origin = "123"
        self.epoch = 21
        self.actor = f"production-schema-release:{self.origin}:{self.pin.candidate_sha}"
        self.foreign_actor = f"production-schema-release:124:{self.pin.candidate_sha}"
        self.stopped = {
            "worker": {
                "version_id": self.pin.rollback_worker,
                "sha": self.pin.rollback_sha,
                "traffic_percent": 100,
            },
            "container": {
                "application_id": self.pin.application_id,
                "namespace_id": self.pin.namespace_id,
                "image_digest": self.pin.rollback_image,
                "rollout_status": "completed",
            },
            "ownership": {
                "epoch": self.epoch,
                "running": False,
                "phase": "fenced",
                "owner_actor": self.actor,
            },
        }
        self.live = deepcopy(self.stopped)
        self.observe = Mock(side_effect=lambda: deepcopy(self.live))
        self.control = Mock()
        self.recover_post = Mock()

    def reconcile(self, classification):
        return recovery.reconcile_interrupted(
            self.pin, self.origin, self.epoch, classification,
            self.observe, self.control, self.recover_post,
        )

    def recovery_admission(self):
        repo = {"id": 1396224242, "full_name": "TogetherWeOwn/two-bot-next"}
        origin = {
            "id": int(self.origin),
            "run_attempt": 1,
            "head_sha": self.pin.workflow_sha,
            "head_branch": "main",
            "repository": repo,
            "head_repository": repo,
            "actor": {"id": 42},
            "triggering_actor": {"id": 42},
            "path": ".github/workflows/deploy-production.yml",
            "event": "workflow_dispatch",
            "status": "completed",
            "conclusion": "cancelled",
        }
        current = {**origin, "id": 456, "head_sha": "f" * 40, "status": "in_progress", "conclusion": None}
        env = {
            "RELEASE_RECOVERY_RUN_ID": self.origin,
            "GITHUB_RUN_ID": str(current["id"]),
            "GITHUB_SHA": current["head_sha"],
            "GITHUB_WORKFLOW_SHA": current["head_sha"],
            "GITHUB_REPOSITORY": repo["full_name"],
            "GITHUB_REPOSITORY_ID": str(repo["id"]),
            "GITHUB_EVENT_NAME": "workflow_dispatch",
            "GITHUB_RUN_ATTEMPT": "1",
            "GITHUB_ACTOR_ID": "42",
            "GITHUB_REF": "refs/heads/main",
            "GITHUB_REF_PROTECTED": "true",
            "GITHUB_WORKFLOW_REF": f"{repo['full_name']}/{origin['path']}@refs/heads/main",
        }
        steps = ("Verify forward release preflight", "Retain the exact recovery stop intent",
                 "Confirm the exact stopped writer")
        job = {"run_id": origin["id"], "run_attempt": 1, "head_sha": origin["head_sha"], "status": "completed"}
        prepare = {**job, "name": "Production release / Prepare and stop the prior pair", "conclusion": "success",
                   "steps": [{"name": name, "status": "completed", "conclusion": "success"} for name in steps]}
        apply = {**job, "name": "Production release / Apply the approved schema", "conclusion": "cancelled",
                 "steps": [{"name": "Apply the protected forward transaction", "status": "completed",
                            "conclusion": "cancelled"},
                           {"name": "Upload the verified forward result", "status": "completed",
                            "conclusion": "skipped"}]}
        plan_id = "789"
        plan = self.reviewed_plan()
        plan_artifact = {"archive_digest": "sha256:" + "9" * 64}
        intent = preflight.stop_intent(self.pin, origin, plan_id, plan, plan_artifact, self.epoch - 1)
        root = f"actions/runs/{self.origin}"
        routes = {
            root: origin,
            f"actions/runs/{current['id']}": current,
            f"compare/{origin['head_sha']}...{current['head_sha']}": {
                "status": "ahead", "merge_base_commit": {"sha": origin["head_sha"]},
            },
            f"{root}/attempts/1/jobs?per_page=30": {"total_count": 2, "jobs": [prepare, apply]},
        }
        self.publish(routes, origin, 654, "production-release-stop-intent", json.dumps(intent).encode())
        return env, routes, plan, plan_artifact

    @staticmethod
    def publish(routes, run, artifact_id, name, content):
        archive = BytesIO()
        with ZipFile(archive, "w") as bundle:
            bundle.writestr(f"{name}.json", content)
        raw = archive.getvalue()
        digest = sha256(raw).hexdigest()
        entry = {"id": artifact_id, "name": f"{name}-{run['id']}-1", "expired": False,
                 "digest": f"sha256:{digest}", "size_in_bytes": len(raw)}
        lineage = {"id": run["id"], "head_sha": run["head_sha"], "head_branch": "main",
                   "repository_id": run["repository"]["id"], "head_repository_id": run["head_repository"]["id"]}
        listing = routes.setdefault(f"actions/runs/{run['id']}/artifacts?per_page=30", {"artifacts": []})
        listing["artifacts"].append(entry)
        listing["total_count"] = len(listing["artifacts"])
        routes[f"actions/artifacts/{artifact_id}"] = {**entry, "workflow_run": lineage}
        routes[f"actions/artifacts/{artifact_id}/zip"] = raw
        return digest

    def reviewed_plan(self):
        def inventory(versions):
            return [{"version": version, "description": "fixture", "sha384": "b" * 96,
                     "sha384_recomputed": "b" * 96} for version in versions]

        plan = {
            "target": {"host": "db.invalid", "database": "fixture", "branch_id": "fixture"},
            "source_sha": self.pin.candidate_sha,
            "recovery_evidence_ref": self.pin.recovery_reference(),
            "acl_plan_ref": "fixture",
            "pending_before": [475, 479, 480, 481, 482],
            "pending_store_before": [478],
            "source_migrations": inventory((473, 475, 479, 480, 481, 482)),
            "source_store_migrations": inventory((465, 478)),
        }
        cutover, store = self.db_ledgers(plan)
        return {**plan, "plan_manifest_sha256": projection_hash(plan), "store_plan_manifest_sha256": store_projection_hash(plan),
                "ledger_before": cutover[:1], "ledger_after": cutover[:1],
                "ledger_store_before": store[:1], "ledger_store_after": store[:1]}

    @staticmethod
    def db_ledgers(plan):
        return [[{key: row[key] for key in ("version", "description", "sha384")} | {"success": True}
                 for row in plan[chain]] for chain in ("source_migrations", "source_store_migrations")]

    def test_cancelled_apply_routes_locked_db_outcomes_to_the_pinned_predecessor(self):
        for state in ("pre", "post", "uncertain"):
            with self.subTest(state=state), TemporaryDirectory() as temp:
                self.check_cancelled_apply(state, Path(temp))

    def check_cancelled_apply(self, state, temp):
        env, routes, plan, plan_artifact = self.recovery_admission()
        current = routes["actions/runs/456"]
        jobs = []
        routes["actions/runs/456/attempts/1/jobs?per_page=30"] = {"jobs": jobs}
        directory = temp / "interrupted-production-release"
        output = temp / "outputs"
        env.update(RUNNER_TEMP=str(temp), GITHUB_OUTPUT=str(output), PRODUCTION_WORKER_URL="https://bot.fixture.invalid",
                   CLOUDFLARE_ACCOUNT_ID="fixture", CLOUDFLARE_API_TOKEN="synthetic",
                   OWNERSHIP_CONTROL_TOKEN="synthetic", TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL="synthetic")

        def producer(name, steps, conclusion="success"):
            jobs.append({"name": name, "run_id": 456, "run_attempt": 1, "head_sha": current["head_sha"],
                         "status": "completed", "conclusion": conclusion,
                         "steps": [{"name": step, "status": "completed", "conclusion": "success"} for step in steps]})
            routes["actions/runs/456/attempts/1/jobs?per_page=30"]["total_count"] = len(jobs)

        def upload(name, artifact_id, prefix):
            digest = self.publish(routes, current, artifact_id, name, (directory / f"{name}.json").read_bytes())
            env[f"{prefix}_ARTIFACT_ID"] = str(artifact_id)
            env[f"{prefix}_ARCHIVE_SHA256"] = digest

        def takeover_client(_variant, _token, _url, _staging_url, actor):
            def takeover(*args, **kwargs):
                self.control(*args, **kwargs)
                self.live["ownership"].update(epoch=self.epoch + 1, phase="active", owner_actor=actor, running=True)
            return takeover

        stamp = "d" * 64
        classification = {"state": "post", "attendance_fence_sha256": stamp} if state == "post" else {"state": "pre"}
        cutover, store = self.db_ledgers(plan)
        inverse = {"state": "restored", "attendance_fence_sha256": stamp, "expanded": [cutover, store],
                   "restored": [cutover[:2], store[:1]]}
        process = Mock(side_effect=[Mock(returncode=int(state == "uncertain"), stdout=json.dumps(classification)),
                                    Mock(returncode=0, stdout=json.dumps(inverse))])
        api = Mock(side_effect=lambda route, **_kwargs: routes[route])
        rollback = Mock(evidence={"stable_polls": 1})
        self.live = deepcopy(self.stopped)
        self.control.reset_mock()
        with (patch.object(preflight, "bound_plan", return_value=(plan, plan_artifact)),
              patch.object(interrupted, "environment_policy"),
              patch.object(interrupted, "read_status", side_effect=lambda _env: deepcopy(self.live)),
              patch.object(interrupted, "control_client", side_effect=takeover_client),
              patch.object(interrupted, "RollbackClient"), patch.object(interrupted, "Rollback", return_value=rollback),
              patch.object(recovery.subprocess, "run", process), redirect_stderr(StringIO())):
            self.assertEqual(interrupted.main(["admit"], env, api), 0)
            upload(interrupted.ADMISSION, 701, "ADMISSION")
            producer(interrupted.ADMISSION_JOB, interrupted.ADMISSION_STEPS)
            self.assertEqual(interrupted.main(["classify", "fixture-migrator"], env, api), int(state == "uncertain"))
            self.assertEqual(process.call_args.args[0][-1], "--classify-interrupted-release")
            self.assertEqual(process.call_args.kwargs["env"], {
                "TWO_BOT_PRODUCTION_MIGRATOR_DATABASE_URL": "synthetic", "TWO_DATABASE_TLS": "required",
            })
            if state == "uncertain":
                self.assertFalse(output.exists())
                self.assertFalse((directory / f"{interrupted.DECISION}.json").exists())
                producer(interrupted.DATABASE_JOB, (), "failure")
                self.assertEqual(interrupted.main(["restart"], env, api), 1)
            else:
                self.assertEqual(output.read_text(), f"state={state}\n")
                upload(interrupted.DECISION, 702, "DECISION")
                steps = (interrupted.CLASSIFY_STEP, interrupted.DECISION_STEP)
                producer(interrupted.DATABASE_JOB, steps)
                if state == "post":
                    self.assertEqual(interrupted.main(["restart"], env, api), 1)
                    self.control.assert_not_called()
                    self.assertEqual(interrupted.main(["inverse", "fixture-migrator"], env, api), 0)
                    self.assertEqual(process.call_args.args[0][-2:], ["--inverse-attendance-fence-sha256", stamp])
                    upload(interrupted.INVERSE, 703, "INVERSE")
                    jobs.pop()
                    producer(interrupted.DATABASE_JOB, (*steps, *interrupted.INVERSE_STEPS))
                self.control.assert_not_called()
                self.assertEqual(interrupted.main(["restart"], env, api), 0)

        self.assertEqual(process.call_count, 2 if state == "post" else 1)
        if state == "uncertain":
            self.control.assert_not_called()
            rollback.wait_stable.assert_not_called()
            self.assertEqual(self.live, self.stopped)
            return
        self.control.assert_called_once_with("takeover-fenced", self.epoch, expected_deployment=self.pin.rollback_worker)
        rollback.wait_stable.assert_called_once_with(self.pin.rollback_worker, rollback.now.return_value)
        rollback.post_worker_restore.assert_not_called()
        rollback.restore_image.assert_not_called()
        receipt = json.loads((directory / "production-release-interrupted-restored.json").read_bytes())
        self.assertEqual(receipt["outcome"], {"state": state, "worker": self.pin.rollback_worker, "epoch": self.epoch + 1})

    def test_cancelled_apply_refuses_unfinished_origin_or_unproved_intent(self):
        env, routes, _, _ = self.recovery_admission()
        root = f"actions/runs/{self.origin}"
        jobs = f"{root}/attempts/1/jobs?per_page=30"
        compare = f"compare/{self.pin.workflow_sha}...{env['GITHUB_SHA']}"
        cases = {
            "in progress": lambda routes: routes[root].update(status="in_progress", conclusion=None),
            "rerun": lambda routes: routes[root].update(run_attempt=2),
            "successful origin": lambda routes: routes[root].update(conclusion="success"),
            "different workflow lineage": lambda routes: routes[compare].update(merge_base_commit={"sha": "e" * 40}),
            "cancelled intent upload": lambda routes: routes[jobs]["jobs"][0]["steps"][1].update(
                conclusion="cancelled",
            ),
            "different producer": lambda routes: routes["actions/artifacts/654"]["workflow_run"].update(id=456),
            "wrong archive": lambda routes: routes.update({"actions/artifacts/654/zip": b"synthetic replacement"}),
        }
        for name, mutate in cases.items():
            with self.subTest(name=name):
                changed = deepcopy(routes)
                mutate(changed)
                api = Mock(side_effect=lambda route, **_kwargs: changed[route])
                with patch.object(preflight, "bound_plan") as bound, self.assertRaises(GateError):
                    preflight.interrupted_release_context(env, api)
                bound.assert_not_called()

    def test_pre_lost_reply_confirms_exact_predecessor_without_a_second_takeover(self):
        def commit_then_lose_reply(*_args, **_kwargs):
            self.live["ownership"].update(epoch=self.epoch + 1, running=True, phase="active")
            raise GateError("fixture_takeover_reply_lost")

        self.control.side_effect = commit_then_lose_reply
        expected = {"state": "pre", "worker": self.pin.rollback_worker, "epoch": self.epoch + 1}
        self.assertEqual(self.reconcile({"state": "pre"}), expected)
        self.assertEqual(self.reconcile({"state": "pre"}), expected)
        self.control.assert_called_once_with(
            "takeover-fenced", self.epoch, expected_deployment=self.pin.rollback_worker,
        )
        self.recover_post.assert_not_called()

    def test_pre_control_reply_needs_confirmed_ownership(self):
        self.control.return_value = {"deploymentId": self.pin.rollback_worker}

        with self.assertRaises(GateError):
            self.reconcile({"state": "pre"})
        self.control.assert_called_once_with(
            "takeover-fenced", self.epoch, expected_deployment=self.pin.rollback_worker,
        )
        self.recover_post.assert_not_called()

    def test_unproved_database_result_leaves_the_stop_in_place(self):
        for classification in (
            {"state": "ambiguous"},
            {"state": "post"},
            {"state": "post", "attendance_fence_sha256": "D" * 64},
            {"state": "pre", "attendance_fence_sha256": "d" * 64},
        ):
            with self.subTest(classification=classification), self.assertRaises(GateError):
                self.reconcile(classification)
        self.control.assert_not_called()
        self.recover_post.assert_not_called()
        self.observe.assert_not_called()

    def test_verified_database_rejects_changed_worker_image_or_fence(self):
        for section, field, value in (
            ("worker", "version_id", "88888888-8888-4888-8888-888888888888"),
            ("container", "image_digest", self.pin.candidate_image),
            ("ownership", "owner_actor", self.foreign_actor),
            ("ownership", "epoch", self.epoch + 1),
            ("ownership", "running", True),
        ):
            for classification in ({"state": "pre"}, {"state": "post", "attendance_fence_sha256": "d" * 64}):
                with self.subTest(section=section, field=field, classification=classification):
                    self.live = deepcopy(self.stopped)
                    self.live[section][field] = value
                    with self.assertRaises(GateError):
                        self.reconcile(classification)
        self.control.assert_not_called()
        self.recover_post.assert_not_called()

    def test_lost_pre_reply_does_not_retry_at_pending_or_foreign_active_epoch(self):
        for phase, actor, running in (("fenced", self.actor, False), ("active", self.foreign_actor, True)):
            with self.subTest(phase=phase):
                after = {"epoch": self.epoch + 1, "phase": phase, "owner_actor": actor, "running": running}

                def lose_reply(*_args, **_kwargs):
                    self.live["ownership"].update(after)
                    raise GateError("fixture_takeover_reply_lost")

                self.live = deepcopy(self.stopped)
                self.control.reset_mock()
                self.control.side_effect = lose_reply
                with self.assertRaises(GateError):
                    self.reconcile({"state": "pre"})
                with self.assertRaises(GateError):
                    self.reconcile({"state": "pre"})
                self.control.assert_called_once_with(
                    "takeover-fenced", self.epoch, expected_deployment=self.pin.rollback_worker,
                )
                self.assertEqual(self.live["ownership"], after)
        self.recover_post.assert_not_called()

    def test_post_cannot_enter_inverse_after_any_writer_has_resumed(self):
        for actor in (self.actor, self.foreign_actor):
            with self.subTest(actor=actor), self.assertRaises(GateError):
                self.live["ownership"].update(epoch=self.epoch + 1, phase="active", owner_actor=actor, running=True)
                self.reconcile({"state": "post", "attendance_fence_sha256": "d" * 64})
        self.control.assert_not_called()
        self.recover_post.assert_not_called()


if __name__ == "__main__":
    unittest.main()
