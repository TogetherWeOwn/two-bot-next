"""Exercise dormant PRE orchestration with synthetic authority and runtime stubs."""

from dataclasses import dataclass, replace
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock, patch
from uuid import UUID

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_start_stop import Deployment, Endpoint, Operation, ProbeReply, ProtectedPre, StartupAdmission
    from pre_target import Target, TargetError


TARGET = Target("pre-fixture-test", 123, 1_800_000_000, "0473/0465")
START = UUID(int=1)
DEPLOYMENT = Deployment(TARGET, START, UUID(int=2), UUID(int=3))


@dataclass(frozen=True)
class Upload:
    target: Target = TARGET
    content_type: str = "multipart/form-data; boundary=fixture"
    body: bytes = b"fixture"


class StopAuthority:
    def __init__(self, calls):
        self.calls = calls
        self.now = TARGET.started_at
        self.armed = None
        self.closed = False

    def arm(self, target, start_id):
        self.calls.append(("arm", target, start_id))
        if self.closed or not target.started_at <= self.now < target.expires_at:
            return False
        self.armed = (target, start_id)
        return True

    def fence(self, target):
        self.calls.append(("fence", target))
        self.closed = True
        return True

    def allow_effect(self, target, start_id):
        if self.closed or self.armed != (target, start_id) or self.now >= target.expires_at:
            raise OSError("provider detail must stay private")


class Runtime:
    def __init__(self, calls, authority):
        self.calls = calls
        self.authority = authority
        self.deployment = None
        self.before_create = Mock()
        self.reply = ProbeReply(200, DEPLOYMENT.worker_version, b"{}")

    def create_private(self, target, start_id, upload):
        self.before_create()
        self.authority.allow_effect(target, start_id)
        self.calls.append(("create", target, start_id, upload))
        self.deployment = DEPLOYMENT
        return DEPLOYMENT

    def probe(self, deployment, endpoint):
        self.authority.allow_effect(deployment.target, deployment.start_id)
        if self.deployment != deployment:
            raise OSError("unbound deployment")
        self.calls.append(("probe", deployment, endpoint))
        return self.reply


class ProtectedPreTests(unittest.TestCase):
    def setUp(self):
        self.calls = []
        self.admission = Mock(spec_set=StartupAdmission)
        self.admission.admit_start.side_effect = [START, None]
        self.admission.close.return_value = True
        self.admission.settle.return_value = True
        self.authorize = Mock(return_value=True)
        self.external = StopAuthority(self.calls)
        self.runtime = Runtime(self.calls, self.external)
        self.controller = ProtectedPre(TARGET, authorize=self.authorize, admission=self.admission,
                                       runtime=self.runtime, external_stop=self.external)

    def test_arms_before_private_create_and_pins_the_probe(self):
        upload = Upload()
        deployment = self.controller.start(upload)
        self.assertEqual(deployment, DEPLOYMENT)
        self.assertEqual(self.controller.probe(deployment, Endpoint.HEALTH), self.runtime.reply)
        self.assertEqual(self.controller.probe(deployment, Endpoint.READINESS), self.runtime.reply)
        self.assertEqual(self.calls, [("arm", TARGET, START), ("create", TARGET, START, upload),
                                      ("probe", DEPLOYMENT, Endpoint.HEALTH),
                                      ("probe", DEPLOYMENT, Endpoint.READINESS)])
        self.assertEqual([call.args for call in self.authorize.call_args_list],
                         [(TARGET, Operation.START), (TARGET, Operation.PROBE), (TARGET, Operation.PROBE)])

    def test_refuses_other_reservations_and_unauthorized_operations_before_effects(self):
        self.authorize.return_value = False
        for action, args in ((self.controller.start, (Upload(),)),
                             (self.controller.probe, (DEPLOYMENT, Endpoint.HEALTH)), (self.controller.stop, ())):
            with self.assertRaisesRegex(TargetError, "^pre_operation_denied$"):
                action(*args)
        self.authorize.return_value = True
        foreign = replace(TARGET, shape="0475/0465")  # Same Worker name and deadline.
        with self.assertRaisesRegex(TargetError, "^wrong_pre_upload_target$"):
            self.controller.start(Upload(foreign))
        with self.assertRaisesRegex(TargetError, "^pre_deployment_unconfirmed$"):
            self.controller.probe(replace(DEPLOYMENT, target=foreign), Endpoint.HEALTH)
        with self.assertRaisesRegex(TargetError, "^invalid_pre_probe$"):
            self.controller.probe(DEPLOYMENT, "/health")
        self.assertEqual(self.admission.mock_calls, [])
        self.assertEqual(self.calls, [])

    def test_unarmed_deadline_refuses_create(self):
        self.external.now = TARGET.expires_at
        with self.assertRaisesRegex(TargetError, "^pre_external_stop_unarmed$"):
            self.controller.start(Upload())
        self.assertEqual(self.calls, [("arm", TARGET, START)])

    def test_delayed_create_refused_when_stop_races_after_arm(self):
        self.runtime.before_create.side_effect = self.controller.stop
        with self.assertRaisesRegex(TargetError, "^pre_create_unconfirmed$"):
            self.controller.start(Upload())
        self.assertEqual(self.calls, [("arm", TARGET, START), ("fence", TARGET)])
        self.admission.close.assert_called_once_with(TARGET)
        self.admission.settle.assert_called_once_with(TARGET)
        self.assertIsNone(self.runtime.deployment)

    def test_expiry_refuses_probe_even_without_runner_cleanup(self):
        deployment = self.controller.start(Upload())
        self.external.now = TARGET.expires_at
        with self.assertRaisesRegex(TargetError, "^pre_probe_unconfirmed$"):
            self.controller.probe(deployment, Endpoint.READINESS)
        self.assertEqual([entry[0] for entry in self.calls], ["arm", "create"])

    def test_lost_create_reply_keeps_stop_available_and_cannot_retry_start(self):
        create = self.runtime.create_private

        def lose_reply(*args):
            create(*args)
            raise OSError("provider detail must stay private")

        self.runtime.create_private = lose_reply
        with self.assertRaisesRegex(TargetError, "^pre_create_unconfirmed$"):
            self.controller.start(Upload())
        self.assertEqual(self.runtime.deployment, DEPLOYMENT)
        with self.assertRaisesRegex(TargetError, "^pre_start_not_admitted$"):
            self.controller.start(Upload())
        self.admission.settle.side_effect = [False, True]
        self.assertFalse(self.controller.stop())
        with self.assertRaisesRegex(TargetError, "^pre_probe_unconfirmed$"):
            self.controller.probe(DEPLOYMENT, Endpoint.HEALTH)
        self.assertTrue(self.controller.stop())
        self.assertEqual([entry[0] for entry in self.calls], ["arm", "create", "fence", "fence"])
        self.assertEqual(self.authorize.call_args.args, (TARGET, Operation.STOP))

    def test_unconfirmed_external_fence_still_closes_admission(self):
        self.external.fence = Mock(side_effect=OSError("provider detail must stay private"))
        with self.assertRaisesRegex(TargetError, "^pre_external_stop_unconfirmed$"):
            self.controller.stop()
        self.admission.close.assert_called_once_with(TARGET)
        self.admission.settle.assert_not_called()

    def test_reports_both_uncertain_stop_operations_without_provider_detail(self):
        self.external.fence = Mock(side_effect=OSError("provider detail must stay private"))
        self.admission.close.side_effect = OSError("store detail must stay private")
        with self.assertRaisesRegex(TargetError, "^pre_external_stop_and_close_unconfirmed$") as caught:
            self.controller.stop()
        self.assertIsInstance(caught.exception.__context__, TargetError)
        self.assertIsNone(caught.exception.__context__.__context__)
        self.admission.settle.assert_not_called()

    def test_rejects_wrong_create_operation(self):
        with patch.object(self.runtime, "create_private", return_value=replace(DEPLOYMENT, start_id=UUID(int=4))):
            with self.assertRaisesRegex(TargetError, "^pre_deployment_unconfirmed$"):
                self.controller.start(Upload())

    def test_rejects_forged_deployment_or_wrong_probe_version(self):
        deployment = self.controller.start(Upload())
        forged = replace(deployment, container_id=UUID(int=5))
        with self.assertRaisesRegex(TargetError, "^pre_probe_unconfirmed$"):
            self.controller.probe(forged, Endpoint.READINESS)
        self.assertEqual([entry[0] for entry in self.calls], ["arm", "create"])
        self.runtime.reply = replace(self.runtime.reply, worker_version=UUID(int=5))
        with self.assertRaisesRegex(TargetError, "^pre_probe_unconfirmed$"):
            self.controller.probe(deployment, Endpoint.READINESS)
