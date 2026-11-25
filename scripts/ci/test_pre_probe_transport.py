"""Verify private predecessor GETs with an in-memory binding and no sockets."""

from dataclasses import replace
from functools import partial
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock, call, patch
from uuid import UUID

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_start_stop import (
        Deployment, Endpoint, Operation, PrivateHttpReply, PrivateRuntime, ProbeBindingIdentity,
        ProbeReply, ProtectedPre, probe_private_get,
    )
    from pre_target import ProtectedRouteReadback, Resource, Target, TargetError
    from staging_health_contract_probe import BODY_CAP, TIMEOUT_SECONDS


TARGET = Target("pre-fixture-probe", 123, 1_800_000_000, "0473/0465")
DEPLOYMENT = Deployment(TARGET, UUID(int=1), UUID(int=2), UUID(int=3))
ROUTE = ProtectedRouteReadback(TARGET, TARGET.worker_name, Resource(TARGET.container_name, str(UUID(int=3))))
IDENTITY = ProbeBindingIdentity(ROUTE.worker_id, DEPLOYMENT)
HEALTH = PrivateHttpReply(200, (str(DEPLOYMENT.worker_version),), b'{"status":"ok"}')


class PrivateProbeTransportTests(unittest.TestCase):
    def setUp(self):
        self.binding = Mock(identity=IDENTITY)
        self.binding.request.return_value = HEALTH
        self.probe = partial(probe_private_get, pinned_deployment=DEPLOYMENT, pinned_route=ROUTE,
                             binding=self.binding)

    def assert_refuses(self, reason, operation, *args, **kwargs):
        with self.assertRaisesRegex(TargetError, rf"^{reason}$") as caught:
            operation(*args, **kwargs)
        self.assertIsNone(caught.exception.__context__)

    def assert_sent_once(self, endpoint):
        self.binding.request.assert_called_once_with("GET", endpoint.value, expected=IDENTITY,
                                                     follow_redirects=False, timeout_seconds=TIMEOUT_SECONDS,
                                                     max_body_bytes=BODY_CAP + 1)

    def test_refuses_sibling_binding_even_if_it_would_return_pinned_version(self):
        self.binding.identity = replace(IDENTITY, worker_id="sibling-service")
        self.assert_refuses("pre_probe_binding_unconfirmed", self.probe, DEPLOYMENT, Endpoint.HEALTH)
        self.binding.request.assert_not_called()

        self.binding.identity = IDENTITY
        self.assertEqual(self.probe(DEPLOYMENT, Endpoint.HEALTH), ProbeReply(200, DEPLOYMENT.worker_version, HEALTH.body))
        self.assert_sent_once(Endpoint.HEALTH)

    def test_refuses_redirect_before_fallback(self):
        fallback = Mock(return_value=HEALTH)
        for status in (300, 301, 302, 303, 304, 307, 308, 399):
            with self.subTest(status=status):
                self.binding.request.reset_mock()
                redirect = replace(HEALTH, status=status)

                def request(method, path, *, follow_redirects, **_):
                    return fallback(method, path) if follow_redirects else redirect

                self.binding.request.side_effect = request
                self.assert_refuses("pre_probe_redirect", self.probe, DEPLOYMENT, Endpoint.HEALTH)
                self.assert_sent_once(Endpoint.HEALTH)
                fallback.assert_not_called()

    def test_rejects_changed_pins_and_paths_before_send(self):
        foreign = replace(TARGET, shape="0475/0465")  # Same Worker and app names.
        for deployment in (replace(DEPLOYMENT, target=foreign), replace(DEPLOYMENT, start_id=UUID(int=4)),
                           replace(DEPLOYMENT, worker_version=UUID(int=4)), replace(DEPLOYMENT, container_id=UUID(int=4))):
            with self.subTest(deployment=deployment):
                self.assert_refuses("pre_deployment_unconfirmed", self.probe, deployment, Endpoint.READINESS)
                self.binding.identity = replace(IDENTITY, deployment=deployment)
                self.assert_refuses("pre_probe_binding_unconfirmed", self.probe, DEPLOYMENT, Endpoint.READINESS)
        self.binding.identity = IDENTITY
        self.assert_refuses("wrong_pre_route_target", self.probe, DEPLOYMENT, Endpoint.READINESS,
                            pinned_route=replace(ROUTE, target=foreign))
        self.assert_refuses("wrong_pre_route_app", self.probe, DEPLOYMENT, Endpoint.READINESS,
                            pinned_route=replace(ROUTE, app=replace(ROUTE.app, id=str(UUID(int=4)))))
        self.assert_refuses("invalid_pre_probe", self.probe, DEPLOYMENT, "/readyz?other=1")
        self.binding.request.assert_not_called()

    def test_refuses_invalid_http_status(self):
        for status in (199, 600, True, "200"):
            with self.subTest(status=status):
                self.binding.request.reset_mock()
                self.binding.request.return_value = replace(HEALTH, status=status)
                self.assert_refuses("pre_probe_unconfirmed", self.probe, DEPLOYMENT, Endpoint.HEALTH)
                self.assert_sent_once(Endpoint.HEALTH)

    def test_checks_version_and_body_bounds_without_retry(self):
        for stamps in ((), HEALTH.worker_versions * 2, (str(UUID(int=4)),)):
            with self.subTest(stamps=stamps):
                self.binding.request.reset_mock()
                self.binding.request.return_value = replace(HEALTH, worker_versions=stamps)
                self.assert_refuses("pre_probe_version_unconfirmed", self.probe, DEPLOYMENT, Endpoint.READINESS)
                self.assert_sent_once(Endpoint.READINESS)

        self.binding.request.reset_mock()
        self.binding.request.return_value = replace(HEALTH, body=b"x" * (BODY_CAP + 1))
        self.assert_refuses("pre_probe_unconfirmed", self.probe, DEPLOYMENT, Endpoint.READINESS)
        self.assert_sent_once(Endpoint.READINESS)

        self.binding.request.reset_mock()
        self.binding.request.side_effect = TimeoutError("synthetic transport detail")
        self.assert_refuses("pre_probe_unconfirmed", self.probe, DEPLOYMENT, Endpoint.READINESS)
        self.assert_sent_once(Endpoint.READINESS)

    def test_positive_health_and_bounded_readiness_through_protected_pre(self):
        runtime = Mock(spec_set=PrivateRuntime)
        runtime.probe.side_effect = self.probe
        authorize = Mock(return_value=True)
        controller = ProtectedPre(TARGET, authorize=authorize, admission=Mock(), runtime=runtime, external_stop=Mock())
        for endpoint, reply in ((Endpoint.HEALTH, HEALTH),
                                (Endpoint.READINESS, replace(HEALTH, body=b"x" * BODY_CAP)),
                                (Endpoint.READINESS, replace(HEALTH, status=503, body=b"{}"))):
            with self.subTest(endpoint=endpoint, status=reply.status):
                self.binding.request.reset_mock()
                self.binding.request.return_value = reply
                self.assertEqual(controller.probe(DEPLOYMENT, endpoint),
                                 ProbeReply(reply.status, DEPLOYMENT.worker_version, reply.body))
                self.assert_sent_once(endpoint)
        self.assertEqual(authorize.call_args_list, [call(TARGET, Operation.PROBE)] * 3)
