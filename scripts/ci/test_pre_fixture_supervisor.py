"""Focused native PRE fixture lease and provider readback checks."""

from datetime import datetime, timezone
from io import BytesIO
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch
from uuid import UUID

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from ci.pre_fixture_supervisor import lease
    from pre_native_image_pin import NativeScope
    from pre_native_instance import read_terminal_instance
    from pre_start_stop import Deployment
    from pre_target import Target, TargetError


class NativeFixtureTests(unittest.TestCase):
    def test_original_fixture_lease_and_endpoints(self):
        env = {"PRE_SHAPE": "0473/0465", "PRE_CUTOFF_MS": "120000", "PRE_START_ID": str(UUID(int=1)),
               "DISCORD_GATEWAY_URL": "ws://127.0.0.1:8081", "DISCORD_API_BASE": "http://127.0.0.1:8081"}
        for shape, version in (("0473/0465", 473), ("0475/0465", 475)):
            self.assertEqual(lease(dict(env, PRE_SHAPE=shape), 110000), (version, 120000, str(UUID(int=1))))
        for field, value in (("PRE_SHAPE", "0475/0464"), ("PRE_CUTOFF_MS", "110000"),
                             ("PRE_CUTOFF_MS", "9999999999"), ("PRE_START_ID", str(UUID(int=0))),
                             ("DISCORD_GATEWAY_URL", "wss://gateway.discord.gg"),
                             ("DISCORD_API_BASE", "http://127.0.0.1:8082")):
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, "invalid PRE fixture lease"):
                lease(dict(env, **{field: value}), 110000)

    def test_provider_terminal_read_requires_pinned_actor_and_fresh_stop(self):
        target = Target("pre-fixture-test", 123, 1800000000, "0473/0465")
        deployment = Deployment(target, UUID(int=1), UUID(int=2), UUID(int=3))
        scope = NativeScope(deployment, "a" * 32, "pre-test", "b" * 32, "c" * 64)
        stopped = target.started_at + 2
        stamp = datetime.fromtimestamp(stopped, timezone.utc).isoformat()
        terminal = {"id": scope.object_id, "application_id": str(deployment.container_id),
                    "status": {"state": "stopped", "updated_at": stamp}}
        with patch("pre_native_instance.no_redirect_opener") as opener, \
                patch("pre_native_instance.time.time_ns", return_value=(stopped + 1) * 10**9):
            def read(result, *, since=stopped):
                response = BytesIO(json.dumps({"success": True, "errors": [], "messages": [],
                                              "result": result}).encode())
                response.status = 200
                opener.return_value.open.return_value = response
                return read_terminal_instance(scope, stop_requested_ms=since * 1000, token="fixture")

            observed = read(terminal)
            self.assertEqual((observed.scope, observed.stopped_at_ms, observed.observed_at_ms),
                             (scope, stopped * 1000, (stopped + 1) * 1000))
            opener.assert_called_once_with(use_proxy=False)
            request = opener.return_value.open.call_args.args[0]
            self.assertEqual(request.get_method(), "GET")
            self.assertEqual(request.full_url, f"https://api.cloudflare.com/client/v4/accounts/{scope.account_id}"
                             f"/containers/applications/{deployment.container_id}/instances/{scope.object_id}")
            for invalid in ([], dict(terminal, id="d" * 64), dict(terminal, application_id=str(UUID(int=4))),
                            dict(terminal, status={"state": "stopping", "updated_at": stamp})):
                with self.assertRaisesRegex(TargetError, "pre_terminal_instance_unconfirmed"):
                    read(invalid)
            with self.assertRaisesRegex(TargetError, "pre_terminal_instance_unconfirmed"):
                read(terminal, since=stopped + 1)
