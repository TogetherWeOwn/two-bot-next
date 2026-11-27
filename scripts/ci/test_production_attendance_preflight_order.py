"""Run protected attendance prepare against synthetic plan and provider evidence."""

from contextlib import redirect_stderr
from copy import deepcopy
from dataclasses import asdict
from datetime import datetime, timezone
from functools import partial
from hashlib import sha256
from io import BytesIO, StringIO
import json
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch
from zipfile import ZipFile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from ci.production_migrate_claim import projection_hash, store_projection_hash
import production_release as release
import production_release_preflight as preflight
from production_migrate_approval import stable_plan
from production_release_ledgers import CUTOVER, STORE
from staging_migrate_approval import digest_json
from staging_migrate_reviewed import APP_ID
from staging_rollout import GateError


REPO = {"full_name": "TogetherWeOwn/two-bot-next", "id": 1396224242}
RELEASE_WORKFLOW = ".github/workflows/deploy-production.yml"
PLAN_WORKFLOW = ".github/workflows/production-migrate.yml"
HISTORY = ("actions/workflows/deploy-production.yml/runs?event=workflow_dispatch&branch=main"
           "&created=%3E%3D2026-10-01T00%3A00%3A00Z&per_page=30")
PLAN_ID = 101
RUN_ID = 203
ARTIFACT_ID = 901
PLAN_ARTIFACT = "production-migrate-manifest"
NOW = datetime(2026, 10, 1, 0, 15, tzinfo=timezone.utc)


def synthetic_plan(pin):
    plan = {
        "runner_version": 1, "mode": "plan", "migration_target": "production",
        "role": "two_bot_migrator_ro", "role_verified_connections": 1,
        "plan_run_id": "", "plan_provenance_verified": False, "applied_count": 0, "applied_store_count": 0,
        "source_sha": pin.candidate_sha, "recovery_evidence_ref": pin.recovery_reference(), "acl_plan_ref": "fixture",
        "target": {"host": "db.example.invalid", "database": "two_bot", "branch_id": "fixture-branch"},
        "audit": {"verify_findings": [
            "missing object: public.event_attendance_windows",
            "missing object: public.event_voice_sessions",
            "missing object: public.event_voice_sessions_id_seq",
            "missing object: public.live_sequence_watermarks",
            "missing object: public.voice_heat_hours",
            "missing object: web_v1.voice_heat",
        ]},
    }
    for name, baseline, pending in (("", 474, CUTOVER), ("_store", 477, STORE)):
        rows = [{"version": version, "description": "fixture", "sha384": "a" * 96}
                for version in (baseline, *pending)]
        plan[f"pending{name}_before"] = list(pending)
        plan[f"source{name}_migrations"] = [{**row, "sha384_recomputed": row["sha384"]} for row in rows]
        plan[f"ledger{name}_before"] = [{**rows[0], "success": True}]
        plan[f"ledger{name}_after"] = deepcopy(plan[f"ledger{name}_before"])
    plan.update(plan_manifest_sha256=projection_hash(plan), store_plan_manifest_sha256=store_projection_hash(plan))
    return plan


def run_record(run_id, head, created, **overrides):
    return {
        "id": run_id, "run_attempt": 1, "path": RELEASE_WORKFLOW, "event": "workflow_dispatch",
        "head_branch": "main", "head_sha": head, "repository": REPO, "head_repository": REPO,
        "created_at": created, "status": "in_progress", "display_title": f"attendance release plan {PLAN_ID}",
        "actor": {"id": 202}, "triggering_actor": {"id": 202}, **overrides,
    }


class SyntheticSource:
    def __init__(self, pin, plan, events):
        self.events = events
        self.producer = run_record(PLAN_ID, pin.candidate_sha, "2026-10-01T00:00:00Z", path=PLAN_WORKFLOW,
                                   status="completed", conclusion="success")
        self.current = run_record(RUN_ID, pin.workflow_sha, "2026-10-01T00:10:00Z")
        self.history = {"total_count": 1, "workflow_runs": [self.current]}
        with BytesIO() as buffer:
            with ZipFile(buffer, "w") as archive:
                archive.writestr(f"{PLAN_ARTIFACT}.json", json.dumps(plan))
            self.archive = buffer.getvalue()
        self.digest = f"sha256:{sha256(self.archive).hexdigest()}"

    def api(self, path, *, binary=False):
        policy = "environments/production-migrate-apply"
        check = "production-migration-approval"
        check_path = f"commits/{self.current['head_sha']}/check-runs?check_name={check}&filter=all&per_page=30"
        if path == f"actions/artifacts/{ARTIFACT_ID}/zip" and binary:
            return self.archive
        if binary:
            raise AssertionError(f"unexpected binary fixture API route {path}")
        if path == check_path:
            self.events.append("bound approval")
        return deepcopy({
            f"actions/runs/{RUN_ID}": self.current,
            f"actions/runs/{PLAN_ID}": self.producer,
            f"actions/runs/{PLAN_ID}/attempts/1/jobs?per_page=4": {"total_count": 3, "jobs": [
                {"name": f"production-migrate ({step})", "conclusion": outcome}
                for step, outcome in (("plan", "success"), ("claim", "skipped"), ("apply", "skipped"))
            ]},
            f"actions/runs/{PLAN_ID}/artifacts?per_page=2": {"total_count": 1, "artifacts": [{
                "id": ARTIFACT_ID, "name": PLAN_ARTIFACT, "expired": False,
                "size_in_bytes": len(self.archive), "digest": self.digest,
            }]},
            policy: {"name": "production-migrate-apply", "can_admins_bypass": False,
                     "deployment_branch_policy": {"custom_branch_policies": True, "protected_branches": False},
                     "protection_rules": [{"type": "branch_policy"}]},
            f"{policy}/deployment-branch-policies?per_page=2": {"total_count": 1, "branch_policies": [
                {"type": "branch", "name": "main"},
            ]},
            check_path: {"total_count": 1, "check_runs": [{
                "id": 1001, "app": {"id": APP_ID}, "head_sha": self.current["head_sha"], "name": check,
                "external_id": f"production-apply:{PLAN_ID}:{self.digest.removeprefix('sha256:')}",
                "status": "completed", "conclusion": "success", "completed_at": "2026-10-01T00:05:00Z",
            }]},
            HISTORY: self.history,
        }[path])


class ProviderEffects:
    def __init__(self, pin, events):
        self.pin = pin
        self.events = events
        self.calls = []
        self.state = {
            "worker": {"version_id": pin.rollback_worker, "sha": pin.rollback_sha, "traffic_percent": 100},
            "container": {"image_digest": pin.rollback_image, "application_id": pin.application_id,
                          "namespace_id": pin.namespace_id, "rollout_status": "completed"},
            "ownership": {"phase": "active", "running": True, "epoch": 17},
        }

    def record(self, name):
        self.calls.append(name)
        self.events.append(name)

    def effect(self, name, *, result=None, forbidden=False):
        def fake(*args, **kwargs):
            self.record(name)
            if forbidden:
                raise AssertionError(f"unexpected prepare effect {name}")
            return deepcopy(result)
        return fake

    def control_client(self, *args):
        self.record("control client")
        return self.control

    def control(self, action, epoch=None, expected_deployment=None):
        self.record(f"control {action}")
        if action != "takeover-capability" or epoch is not None or expected_deployment != self.pin.rollback_worker:
            raise AssertionError(f"unexpected prepare control {action}")
        return {"deploymentId": expected_deployment, "exactTakeover": True}


class AttendancePreflightOrderTests(unittest.TestCase):
    def setUp(self):
        self.pin = preflight.ReleasePreflight(
            workflow_sha="a" * 40, candidate_sha="b" * 40, candidate_archive="sha256:" + "1" * 64,
            candidate_image="sha256:" + "2" * 64, rollback_sha="c" * 40,
            rollback_worker="12345678-1234-4567-8123-123456789abc", rollback_image="sha256:" + "3" * 64,
            application_id="12345678-1234-4567-8123-123456789def", namespace_id="4" * 32,
            restore_proof_sha256="5" * 64,
        )
        self.plan = synthetic_plan(self.pin)
        self.fresh = deepcopy(self.plan)
        self.fresh["role_verified_connections"] = 2
        self.events = []
        self.refusals = []
        self.provider = ProviderEffects(self.pin, self.events)
        self.temp = Path(self.enterContext(TemporaryDirectory()))
        self.directory = self.temp / "production-release"
        self.directory.mkdir()
        self.output = self.temp / "output"

    def prepare(self, source):
        fresh_path = self.directory / "fresh-plan.json"
        original = json.dumps(self.fresh)
        fresh_path.write_text(original, encoding="utf-8")
        pins = asdict(self.pin)
        del pins["workflow_sha"], pins["candidate_sha"]
        env = {
            "RUNNER_TEMP": str(self.temp), "GITHUB_OUTPUT": str(self.output),
            "GITHUB_RUN_ID": str(RUN_ID), "GITHUB_RUN_ATTEMPT": "1", "GITHUB_ACTOR_ID": "202",
            "GITHUB_SHA": self.pin.workflow_sha, "GITHUB_WORKFLOW_SHA": self.pin.workflow_sha,
            "GITHUB_REPOSITORY": REPO["full_name"], "GITHUB_REPOSITORY_ID": str(REPO["id"]),
            "GITHUB_WORKFLOW_REF": f"{REPO['full_name']}/{RELEASE_WORKFLOW}@refs/heads/main",
            "GITHUB_EVENT_NAME": "workflow_dispatch", "GITHUB_REF": "refs/heads/main", "GITHUB_REF_PROTECTED": "true",
            "RELEASE_SHA": self.pin.candidate_sha, "RELEASE_PLAN_ID": str(PLAN_ID), "RELEASE_PINS": json.dumps(pins),
            "RELEASE_TARGET_FINGERPRINT": digest_json(self.plan["target"]),
            "PRODUCTION_WORKER_URL": "https://worker.example.invalid", "CLOUDFLARE_ACCOUNT_ID": "synthetic-account",
            "CLOUDFLARE_API_TOKEN": "synthetic", "OWNERSHIP_CONTROL_TOKEN": "synthetic",
        }
        effect = self.provider.effect
        validate = release.validate_attendance_plan
        require_unused = release.require_unused_attendance_plan

        def record_gate(label, gate, *args):
            try:
                result = gate(*args)
            except GateError as error:
                self.refusals.append((label, str(error)))
                raise
            self.events.append(label)
            return result

        def record_validation(plan):
            label = {1: "producer plan", 2: "fresh plan"}[plan["role_verified_connections"]]
            return record_gate(label, validate, plan)

        with (
            patch.object(release, "release_context", partial(release.release_context, api=source.api)),
            patch.object(release, "github", source.api),
            patch.object(release, "validate_attendance_plan", record_validation),
            patch.object(release, "require_unused_attendance_plan", partial(record_gate, "approval history", require_unused)),
            patch.object(preflight, "datetime", wraps=datetime) as clock,
            patch.multiple(release, RollbackClient=effect("provider client"),
                           validate_receipt_against_live=effect("provenance"),
                           image_in_registry=effect("registry"), read_status=effect("observe", result=self.provider.state),
                           control_client=self.provider.control_client, stop_prior_pair=effect("stop", forbidden=True),
                           database=effect("database", forbidden=True),
                           apply_under_bound_fence=effect("apply", forbidden=True)),
            patch("subprocess.Popen", side_effect=effect("process", forbidden=True)),
            patch("socket.socket", side_effect=effect("socket", forbidden=True)),
            patch("socket.getaddrinfo", side_effect=effect("DNS", forbidden=True)),
            redirect_stderr(StringIO()),
        ):
            clock.now.return_value = NOW
            status = release.main(["prepare"], env)
        self.assertEqual(fresh_path.read_text(encoding="utf-8"), original)
        return status

    def assert_refused_without_effects(self, source, gate, code):
        status = self.prepare(source)
        self.assertEqual(self.provider.calls, [])
        self.assertEqual(status, 1)
        self.assertEqual(self.refusals, [(gate, code)])
        self.assertFalse(self.output.exists())
        self.assertEqual([path.name for path in self.directory.iterdir()], ["fresh-plan.json"])

    def test_bad_producer_plan_refuses_before_provider_access(self):
        # Projection equality alone cannot distinguish False from zero.
        self.plan["applied_count"] = False
        self.assertEqual(stable_plan(self.plan), stable_plan(self.fresh))
        self.assert_refused_without_effects(SyntheticSource(self.pin, self.plan, self.events),
                                            "producer plan", "production_attendance_plan_invalid")

    def test_bad_fresh_plan_refuses_before_provider_access(self):
        self.fresh["applied_store_count"] = False
        self.assertEqual(stable_plan(self.plan), stable_plan(self.fresh))
        self.assert_refused_without_effects(SyntheticSource(self.pin, self.plan, self.events),
                                            "fresh plan", "production_attendance_plan_invalid")

    def test_reused_approval_refuses_before_provider_access(self):
        source = SyntheticSource(self.pin, self.plan, self.events)
        prior = run_record(204, self.pin.workflow_sha, "2026-10-01T00:07:00Z", status="completed", conclusion="failure")
        source.history = {"total_count": 2, "workflow_runs": [source.current, prior]}
        self.assert_refused_without_effects(source, "approval history",
                                            "production_attendance_plan_superseded_by_release_attempt")

    def test_ambiguous_approval_refuses_before_provider_access(self):
        source = SyntheticSource(self.pin, self.plan, self.events)
        source.history["total_count"] = 2
        self.assert_refused_without_effects(source, "approval history", "production_attendance_history_incomplete")

    def test_valid_prepare_checks_approval_before_reading_the_retained_pair(self):
        source = SyntheticSource(self.pin, self.plan, self.events)
        self.assertEqual(self.prepare(source), 0, self.events)
        first_provider = self.events.index(self.provider.calls[0])
        self.assertCountEqual(self.events[:first_provider],
                              ["bound approval", "producer plan", "fresh plan", "approval history"])
        self.assertLess(self.events.index("bound approval"), self.events.index("approval history"))
        self.assertCountEqual(self.provider.calls, ["control client", "provider client", "provenance", "registry",
                                                    "observe", "control takeover-capability", "observe"])
        ownership_reads = [event for event in self.provider.calls if event in ("observe", "control takeover-capability")]
        self.assertEqual(ownership_reads, ["observe", "control takeover-capability", "observe"])
        intent_name = f"{preflight.STOP_INTENT_ARTIFACT}.json"
        proof = json.loads((self.directory / intent_name).read_text(encoding="utf-8"))
        self.assertEqual(proof["epoch"], 17)
        self.assertEqual(proof["plan_archive_sha256"], source.digest)
        self.assertEqual(self.output.read_text(encoding="utf-8").splitlines(),
                         ["epoch=17", f"archive={self.pin.candidate_archive}", f"image={self.pin.candidate_image}"])
        self.assertCountEqual((path.name for path in self.directory.iterdir()), ["fresh-plan.json", intent_name])
