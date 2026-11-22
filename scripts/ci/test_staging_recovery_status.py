"""Disposable provider, ownership and dispatch evidence for the protected inventory."""

from copy import deepcopy
from hashlib import sha256
from io import BytesIO
import json
from pathlib import Path
import sys
import unittest
from zipfile import ZIP_DEFLATED, ZipFile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import staging_recovery_status as status

SOURCE = "a" * 40
ACCOUNT = "b" * 32
CLOUDFLARE = {"CLOUDFLARE_ACCOUNT_ID": ACCOUNT, "CLOUDFLARE_API_TOKEN": "provider-fixture"}
CONTROL = {"STAGING_WORKER_URL": status.ORIGIN, "OWNERSHIP_CONTROL_TOKEN": "synthetic-ownership-token-of-valid-length"}
ENV = {**CLOUDFLARE, **CONTROL, "GITHUB_SHA": SOURCE, "GITHUB_RUN_ID": "19"}
VERSION = "11111111-1111-1111-1111-111111111111"
DEPLOYMENT = "22222222-2222-2222-2222-222222222222"
APP = "33333333-3333-3333-3333-333333333333"
ROLLOUT = "44444444-4444-4444-4444-444444444444"
IMAGE = f"registry.cloudflare.com/{ACCOUNT}/{status.APPLICATION}@sha256:" + "d" * 64
DEPLOYMENTS = f"/workers/scripts/{status.WORKER}/deployments"
VERSION_DETAIL = f"/workers/scripts/{status.WORKER}/versions/{VERSION}"
APPS = "/containers/applications"
ROLLOUTS = f"{APPS}/{APP}/rollouts?limit=100"
ROLLOUT_DETAIL = f"{APPS}/{APP}/rollouts/{ROLLOUT}"
SECRET_SENTINEL = "must-not-reach-any-receipt"


TARGET = {"host": "fixture.pg.psdb.cloud", "database": "postgres", "branch_id": "fixture_branch"}
FINGERPRINT = sha256(json.dumps(TARGET, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def plan_fixture():
    path = Path(__file__).with_name("fixtures") / "staging_recovery_status_plan.json"
    return json.loads(path.read_text())


def archive_fixture(content):
    info = BytesIO()
    with ZipFile(info, "w", compression=ZIP_DEFLATED) as archive:
        archive.writestr("staging-migrate-manifest.json", content)
    return info.getvalue()


def github_fixture():
    route = "actions/runs/13"
    repo = {"full_name": status.REPO}
    raw = archive_fixture(json.dumps(plan_fixture()))
    return {
        route: {"id": 13, "path": status.PLAN_WORKFLOW, "event": "workflow_dispatch", "head_branch": "main",
                "head_sha": SOURCE, "status": "completed", "conclusion": "success", "run_attempt": 1,
                "repository": repo, "head_repository": repo},
        f"{route}/attempts/1/jobs?per_page=4": {"total_count": 3, "jobs": [
            {"name": f"staging-migrate ({name})", "conclusion": verdict}
            for name, verdict in (("plan", "success"), ("claim", "skipped"), ("apply", "skipped"))
        ]},
        f"{route}/artifacts?per_page=2": {"total_count": 1, "artifacts": [
            {"name": status.PLAN_ARTIFACT, "id": 17, "expired": False, "size_in_bytes": len(raw),
             "digest": f"sha256:{sha256(raw).hexdigest()}"}
        ]},
        "actions/artifacts/17/zip": raw,
    }


def provider_fixture():
    path = Path(__file__).with_name("fixtures") / "staging_recovery_status_provider.json"
    return json.loads(path.read_text())


class StagingRecoveryStatusTests(unittest.TestCase):
    def setUp(self):
        self.plan = status.plan_projection(plan_fixture(), SOURCE)
        self.routes = provider_fixture()
        self.state = {
            "deploymentId": VERSION, "running": False,
            "owner": {"phase": "active", "deploymentId": VERSION, "epoch": 7, "actor": SECRET_SENTINEL},
        }

    def observe(self, update=lambda: None):
        reads = []
        routes = self.routes

        class Provider:
            def api(self, path):
                reads.append(path)
                return deepcopy(routes[path])

        def ownership(token, fingerprint):
            expected = FINGERPRINT if self.plan is not None else None
            self.assertEqual((token, fingerprint), (CONTROL["OWNERSHIP_CONTROL_TOKEN"], expected))
            self.assertEqual(reads, [DEPLOYMENTS, VERSION_DETAIL, APPS, ROLLOUTS, ROLLOUT_DETAIL])
            update()
            return self.state

        return status.read_status(ENV, self.plan, client_factory=lambda *args, **kwargs: Provider(), read=ownership)

    def test_bracket_projects_only_provider_and_ownership_evidence(self):
        self.plan = None
        receipt = self.observe()
        self.assertEqual(set(receipt), {"schema_version", "workflow_sha", "workflow_run_id", "observed_at",
                                        "verification", "worker", "container", "ownership", "runtime_database", "plan"})
        self.assertEqual(receipt["verification"], "provider_only")
        self.assertIsNone(receipt["plan"])
        self.assertEqual(receipt["runtime_database"], "plan_not_supplied")
        self.assertEqual(receipt["worker"]["version_id"], VERSION)
        self.assertEqual(receipt["container"]["image_digest"], IMAGE.split("@")[1])
        self.assertEqual(receipt["ownership"], {"phase": "active", "epoch": 7, "running": False})
        self.assertNotIn(SECRET_SENTINEL, json.dumps(receipt))

    def test_supported_worker_full_tuple_binds_the_exact_plan_fingerprint(self):
        self.state["runtimeDatabaseFingerprint"] = FINGERPRINT
        receipt = self.observe()
        self.assertEqual((receipt["verification"], receipt["runtime_database"]), ("full_tuple", "verified"))
        self.assertEqual(receipt["plan"]["target_fingerprint"], FINGERPRINT)

    def test_unsupported_old_worker_can_only_report_provider_evidence(self):
        receipt = self.observe()
        self.assertEqual(receipt["verification"], "provider_only")
        self.assertEqual(receipt["runtime_database"], "worker_unsupported")

    def test_wrong_runtime_target_and_invalid_fingerprints_refuse(self):
        for fingerprint in (None, "f" * 64, "", [], SECRET_SENTINEL):
            with self.subTest(fingerprint=fingerprint):
                self.state["runtimeDatabaseFingerprint"] = fingerprint
                with self.assertRaisesRegex(status.GateError, "^runtime_database_target_unproved$"):
                    self.observe()

    def test_provider_and_ownership_disagreement_refuse(self):
        def drift():
            self.routes[DEPLOYMENTS]["deployments"][0]["id"] = "55555555-5555-5555-5555-555555555555"

        with self.assertRaisesRegex(status.GateError, "^provider_state_drift$"):
            self.observe(drift)
        self.state["owner"]["deploymentId"] = DEPLOYMENT
        with self.assertRaisesRegex(status.GateError, "^ownership_target_mismatch$"):
            self.observe()

    def test_split_wrong_namespace_missing_progressing_and_unconverged_rollouts_refuse(self):
        mutations = {
            "split_traffic_deployment": lambda: self.routes[DEPLOYMENTS]["deployments"][0]["versions"].append({}),
            "staging_policy_unproved": lambda: self.routes[VERSION_DETAIL]["resources"]["bindings"][0].update(text="other"),
            "application_namespace_mismatch": lambda: self.routes[APPS][0]["durable_objects"].update(
                namespace_id="e" * 32),
            "newest_rollout_unproved": lambda: self.routes[ROLLOUTS].clear(),
            "newest_rollout_not_completed": lambda: self.routes[ROLLOUTS].append({
                **self.routes[ROLLOUTS][0], "created_at": "2026-11-22T20:00:01Z",
                "id": "55555555-5555-5555-5555-555555555555", "status": "progressing",
            }),
            "newest_rollout_not_converged": lambda: self.routes[ROLLOUT_DETAIL]["steps"][0].update(status="pending"),
            "application_image_mismatch": lambda: self.routes[APPS][0]["configuration"].update(image=IMAGE[:-1] + "e"),
        }
        for code, mutate in mutations.items():
            with self.subTest(code=code):
                self.routes = provider_fixture()
                mutate()
                with self.assertRaisesRegex(status.GateError, f"^{code}$"):
                    self.observe()

    def test_ownership_get_uses_fixed_origin_and_refuses_oversize_or_duplicate_fields(self):
        case = self

        class Response(BytesIO):
            status = 200

        for raw, code in ((b" " * (status.MAX_OWNER + 1), "ownership_response_too_large"),
                          (b'{"running":true,"running":false}', "invalid_json")):
            class Opener:
                def open(self, request, timeout):
                    case.assertEqual(request.get_method(), "GET")
                    case.assertEqual(request.get_header("User-agent"), status.USER_AGENT)
                    case.assertEqual(request.full_url, f"{status.ORIGIN}/internal/ownership?runtime_db_fingerprint={FINGERPRINT}")
                    case.assertEqual(timeout, 15)
                    return Response(raw)

            with self.subTest(code=code), self.assertRaisesRegex(status.GateError, f"^{code}$"):
                status.read_ownership(CONTROL["OWNERSHIP_CONTROL_TOKEN"], FINGERPRINT, opener=Opener())

    def test_plan_authenticates_attempt_jobs_zip_and_exact_ledger_projections(self):
        paths = github_fixture()
        receipt = status.verified_plan("13", lambda path, **kwargs: paths[path])
        self.assertEqual((receipt["run_id"], receipt["artifact_id"]), (13, 17))
        self.assertEqual(receipt["target_fingerprint"], FINGERPRINT)
        self.assertEqual((receipt["cutover_version"], receipt["store_version"]), (481, 478))
        self.assertNotIn("fixture.pg.psdb.cloud", json.dumps(receipt))
        self.assertNotIn(SECRET_SENTINEL, json.dumps(receipt))
        paths["actions/runs/13/attempts/1/jobs?per_page=4"]["jobs"][2]["conclusion"] = "success"
        with self.assertRaisesRegex(status.GateError, "^invalid_plan_jobs$"):
            status.verified_plan("13", lambda path, **kwargs: paths[path])
        manifest = plan_fixture()
        manifest["store_plan_manifest_sha256"] = "f" * 64
        with self.assertRaisesRegex(status.GateError, "^plan_projection_mismatch$"):
            status.plan_projection(manifest, SOURCE)

    def test_plan_archive_refuses_wrong_digest_malformed_and_oversized_members(self):
        for raw, code in ((archive_fixture(b"{}"), "plan_archive_digest_mismatch"),
                          (archive_fixture(b'{"role":0,"role":1}'), "invalid_json"),
                          (archive_fixture(b" " * (status.MAX_PLAN + 1)), "invalid_plan_archive")):
            digest = "f" * 64 if code == "plan_archive_digest_mismatch" else sha256(raw).hexdigest()
            with self.subTest(code=code), self.assertRaisesRegex(status.GateError, f"^{code}$"):
                status.plan_manifest(raw, f"sha256:{digest}")

    def test_dispatch_must_be_attempt_one_from_the_current_protected_workflow_head(self):
        context = {**ENV, "GITHUB_REPOSITORY": status.REPO, "GITHUB_REPOSITORY_ID": "1396224242",
                   "GITHUB_EVENT_NAME": "workflow_dispatch", "GITHUB_RUN_ATTEMPT": "1",
                   "GITHUB_REF": "refs/heads/main", "GITHUB_REF_PROTECTED": "true", "GITHUB_WORKFLOW_SHA": SOURCE,
                   "GITHUB_WORKFLOW_REF": f"{status.REPO}/{status.WORKFLOW}@refs/heads/main"}
        status.verify_main(context, lambda path: {"protected": True, "commit": {"sha": SOURCE}})
        with self.assertRaisesRegex(status.GateError, "^dispatch_is_not_current_main$"):
            status.verify_main(context, lambda path: {"protected": True, "commit": {"sha": "b" * 40}})
        with self.assertRaisesRegex(status.GateError, "^untrusted_status_dispatch$"):
            status.verify_main(dict(context, GITHUB_RUN_ATTEMPT="2"), lambda path: self.fail("unexpected API call"))
