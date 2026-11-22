"""Offline fixtures for the production ownership read."""
from copy import deepcopy
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import production_ownership_status as status

FIXTURE = Path(__file__).parent / "fixtures/production_ownership_status_provider.json"
DEPLOYMENTS = f"/workers/scripts/{status.WORKER}/deployments"


class ProductionStatusTests(unittest.TestCase):
    def test_wrong_dispatch_and_environment_refuse(self):
        fixture = json.loads(FIXTURE.read_text())
        env = fixture["github_environment"]
        api = Mock(return_value=fixture["github_head"])
        status.verify_dispatch(env, {"ref": "main"}, api)
        with self.assertRaisesRegex(status.GateError, "invalid_dispatch_context"):
            status.verify_dispatch(env | {"GITHUB_REF": "refs/tags/main"}, {"ref": "main"}, self.fail)
        with self.assertRaisesRegex(status.GateError, "invalid_dispatch_payload"):
            status.verify_dispatch(env, {"ref": "main", "inputs": {"target": "wrong"}}, self.fail)
        api.return_value["object"]["sha"] = "b" * 40
        with self.assertRaisesRegex(status.GateError, "dispatch_is_not_current_main"):
            status.verify_dispatch(env, {"ref": "main"}, api)
        settings, branches = (fixture[key] for key in ("github_production_environment", "github_branches"))
        status.environment_policy("true", Mock(side_effect=[settings, branches]))
        branches["branch_policies"][0]["type"] = "tag"
        with self.assertRaisesRegex(status.GateError, "production_environment_not_main_only"):
            status.environment_policy("true", Mock(side_effect=[settings, branches]))

    def test_bracket_and_fail_closed_projection(self):
        fixture = json.loads(FIXTURE.read_text())
        account = "c" * 32
        env = {"PRODUCTION_WORKER_URL": status.PRODUCTION_URL,
               "OWNERSHIP_CONTROL_TOKEN": "t" * 32, "CLOUDFLARE_ACCOUNT_ID": account}
        before = fixture["provider"]
        after = deepcopy(before)
        state = fixture["ownership"]
        client, read = Mock(), Mock(return_value=state)
        client.api.side_effect = lambda path: (after if read.called else before)[path]

        def observe():
            read.reset_mock()
            return status.read_status(env, client_factory=Mock(return_value=client), read=read)

        report = observe()
        self.assertEqual([call.args[0] for call in client.api.call_args_list], list(before) * 2)
        read.assert_called_once()
        self.assertEqual(report["ownership"], {"phase": "active", "epoch": 49, "running": True, "owner_actor": "redacted"})
        for private in ("private-marker", account):
            self.assertNotIn(private, json.dumps(report))
        with self.assertRaisesRegex(status.GateError, "worker_origin_mismatch"):
            status.read_status(env | {"PRODUCTION_WORKER_URL": "https://wrong.example"}, client_factory=self.fail)
        state["deploymentId"] = "wrong-worker"
        with self.assertRaisesRegex(status.GateError, "ownership_deployment_mismatch"):
            observe()
        state["deploymentId"] = state["owner"]["deploymentId"]
        after[DEPLOYMENTS]["deployments"][0]["id"] = state["deploymentId"]
        with self.assertRaisesRegex(status.GateError, "provider_state_drift"):
            observe()
        before[DEPLOYMENTS]["deployments"][0]["versions"] = None
        with self.assertRaisesRegex(status.GateError, "invalid_api_schema"):
            observe()
        read.assert_not_called()
