"""Disposable provider, ownership and dispatch evidence for the protected inventory."""

from copy import deepcopy
from io import BytesIO
import json
from pathlib import Path
import sys
import unittest

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


def provider_fixture():
    path = Path(__file__).with_name("fixtures") / "staging_recovery_status_provider.json"
    return json.loads(path.read_text())


class StagingRecoveryStatusTests(unittest.TestCase):
    def setUp(self):
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

        def ownership(token):
            self.assertEqual(token, CONTROL["OWNERSHIP_CONTROL_TOKEN"])
            self.assertEqual(reads, [DEPLOYMENTS, VERSION_DETAIL, APPS, ROLLOUTS, ROLLOUT_DETAIL])
            update()
            return self.state

        return status.read_status(ENV, client_factory=lambda *args, **kwargs: Provider(), read=ownership)

    def test_bracket_projects_only_provider_and_ownership_evidence(self):
        receipt = self.observe()
        self.assertEqual(set(receipt), {"schema_version", "workflow_sha", "workflow_run_id", "observed_at",
                                        "verification", "worker", "container", "ownership"})
        self.assertEqual(receipt["verification"], "provider_only")
        self.assertEqual(receipt["worker"]["version_id"], VERSION)
        self.assertEqual(receipt["container"]["image_digest"], IMAGE.split("@")[1])
        self.assertEqual(receipt["ownership"], {"phase": "active", "epoch": 7, "running": False})
        self.assertNotIn(SECRET_SENTINEL, json.dumps(receipt))

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
                    case.assertEqual(request.full_url, f"{status.ORIGIN}/internal/ownership")
                    case.assertEqual(timeout, 15)
                    return Response(raw)

            with self.subTest(code=code), self.assertRaisesRegex(status.GateError, f"^{code}$"):
                status.read_ownership(CONTROL["OWNERSHIP_CONTROL_TOKEN"], opener=Opener())

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
