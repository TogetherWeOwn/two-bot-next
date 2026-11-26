"""Exercise the dormant app image and writer checks before injected launch."""

from dataclasses import replace
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock, call
from uuid import UUID

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from pre_registry_cleanup import RegistryDisposition, RegistryReservation
from pre_runtime_binding import RuntimeBinding, RuntimeBindingReadback, launch_with_verified_binding
from pre_start_stop import (
    Deployment, Endpoint, Operation, PrivateHttpReply, ProbeBindingIdentity, probe_private_get,
)
from staging_health_contract_probe import BODY_CAP, TIMEOUT_SECONDS
from pre_target import ProtectedRouteReadback, Resource, Target, TargetError
from test_pre_caller import ORIGINAL, RECEIPT, RETAINED, Fixture, Upload


DIGEST = "sha256:" + "a" * 64
APP = UUID(int=301)
WORKER = UUID(int=302)
START = UUID(int=303)


def binding_for(deployment):
    target = deployment.target
    repository = f"registry.example.invalid/{target.scope}/two-bot-next-attendance-predecessor-staging"
    image = RegistryReservation(target, f"{repository}@{DIGEST}")
    return RuntimeBinding(deployment, "namespace_1", image, "db_1", "writer_1")


def readback_for(binding, request):
    deployment = binding.deployment
    target = deployment.target
    app = Resource(target.container_name, str(deployment.container_id))
    route = ProtectedRouteReadback(target, target.worker_name, app)
    return RuntimeBindingReadback(request, binding, route, disposable_database=True,
                                  can_write=True, no_shared_writer=True)


class RuntimeBindingTests(unittest.TestCase):
    def test_each_launch_requires_exact_image_and_effective_writer(self):
        for shape in ("0473/0465", "0475/0465"):
            target = Target("pre-fixture-runtime", 123, 1_800_000_000, shape)
            deployment = Deployment(target, START, WORKER, APP)
            binding = binding_for(deployment)
            previous = readback_for(binding, object())

            def observe(actual, *, request):
                self.assertEqual(actual, deployment)
                return readback_for(binding, request)

            effects = Mock()
            effects.observe.side_effect = observe
            effects.recheck.return_value = True
            effects.launch.return_value = deployment
            self.assertIs(launch_with_verified_binding(
                binding, observe=effects.observe, recheck=effects.recheck, launch=effects.launch), deployment)
            request = effects.observe.call_args.kwargs["request"]
            self.assertEqual(effects.mock_calls,
                             [call.observe(deployment, request=request), call.recheck(deployment), call.launch(binding)])

            repository, digest = binding.image.reference.rsplit("@", 1)
            foreign_image = replace(binding.image, reference=f"{repository.replace('registry.', 'foreign.', 1)}@{digest}")
            wrong_image = replace(binding.image, reference=f"{repository}@sha256:" + "f" * 64)
            other_start = replace(deployment, start_id=UUID(int=304))
            other_worker = replace(deployment, worker_version=UUID(int=304))
            other_app = replace(previous.route.app, id=str(UUID(int=304)))
            cases = (
                ("absent", None, "pre_runtime_binding_unconfirmed"),
                ("stale", previous, "pre_runtime_binding_unconfirmed"),
                ("start", replace(previous, binding=replace(binding, deployment=other_start)),
                 "wrong_pre_runtime_deployment"),
                ("version", replace(previous, binding=replace(binding, deployment=other_worker)),
                 "wrong_pre_runtime_deployment"),
                ("namespace", replace(previous, binding=replace(binding, namespace_id="namespace_2")),
                 "wrong_pre_runtime_deployment"),
                ("app", replace(previous, route=replace(previous.route, app=other_app)), "wrong_pre_route_app"),
                ("repository", replace(previous, binding=replace(binding, image=foreign_image)),
                 "wrong_pre_runtime_image"),
                ("digest", replace(previous, binding=replace(binding, image=wrong_image)), "wrong_pre_runtime_image"),
                ("database", replace(previous, binding=replace(binding, database_id="db_2")),
                 "pre_runtime_writer_unconfirmed"),
                ("writer", replace(previous, binding=replace(binding, writer_id="writer_2")),
                 "pre_runtime_writer_unconfirmed"),
                ("read-only", replace(previous, can_write=False), "pre_runtime_writer_unconfirmed"),
                ("shared", replace(previous, no_shared_writer=False), "pre_runtime_writer_unconfirmed"),
                ("unknown", replace(previous, no_shared_writer=None), "pre_runtime_writer_unconfirmed"),
                ("not disposable", replace(previous, disposable_database=False), "pre_runtime_writer_unconfirmed"),
            )
            for case, result, error in cases:
                with self.subTest(shape=shape, case=case):
                    effects.reset_mock()

                    def read(_, *, request):
                        if case in ("absent", "stale"):
                            return result
                        return replace(result, request=request)

                    with self.assertRaisesRegex(TargetError, f"^{error}$"):
                        launch_with_verified_binding(
                            binding, observe=read, recheck=effects.recheck, launch=effects.launch)
                    effects.recheck.assert_not_called()
                    effects.launch.assert_not_called()

    def test_refusal_after_arm_preserves_independent_stop(self):
        fixture = Fixture()
        launch = Mock()

        def create(target, start_id, upload):
            self.assertEqual(fixture.arm_pin, (target, start_id))
            app = UUID(fixture.provisioned.container.id)
            expected = binding_for(Deployment(target, start_id, UUID(int=target.run_id), app))

            def read(_, *, request):
                return replace(readback_for(expected, request), can_write=False)

            return launch_with_verified_binding(expected, observe=read, recheck=lambda _: True, launch=launch)

        fixture.create_private = create
        with self.assertRaisesRegex(TargetError, "^pre_create_unconfirmed$"):
            fixture.caller(Operation.START).start(Upload(fixture.target))
        launch.assert_not_called()
        with self.assertRaisesRegex(TargetError, "^pre_start_not_admitted$"):
            fixture.caller(Operation.START).start(Upload(fixture.target))
        self.assertEqual(fixture.steps(), ["admit", "arm"])
        self.assertTrue(fixture.caller(Operation.STOP).stop())

    def test_post_read_recheck_refuses_stop_or_cutoff_and_preserves_cleanup(self):
        for condition in ("stop", "cutoff", "stop after recheck"):
            with self.subTest(condition=condition):
                fixture = Fixture()
                create_process = fixture.create_private
                rechecked = Mock()
                launch = Mock()
                timeline = []

                def create(target, start_id, upload):
                    # The isolated app and writer can exist before process launch.
                    fixture.snapshot = replace(fixture.provisioned, startup_settled=False,
                                               instance_count=0, assigned_count=0)
                    app = UUID(fixture.provisioned.container.id)
                    deployment = Deployment(target, start_id, UUID(int=target.run_id), app)
                    binding = binding_for(deployment)

                    def read(actual, *, request):
                        self.assertEqual(actual, deployment)
                        evidence = readback_for(binding, request)
                        if condition == "cutoff":
                            fixture.now = target.expires_at
                        timeline.append("read")
                        return evidence

                    def stop():
                        self.assertTrue(fixture.caller(Operation.STOP).stop())
                        timeline.append("stop")

                    def recheck(actual):
                        if condition == "stop":
                            stop()
                        timeline.append("recheck")
                        rechecked(actual)
                        return (actual == deployment and fixture.arm_pin == (target, start_id)
                                and not fixture.fenced and fixture.now < target.expires_at)

                    def effect(checked):
                        timeline.append("effect")
                        stop()
                        pinned = checked.deployment
                        return create_process(pinned.target, pinned.start_id, upload)

                    launch.side_effect = effect
                    return launch_with_verified_binding(binding, observe=read, recheck=recheck, launch=launch)

                fixture.create_private = create
                with self.assertRaisesRegex(TargetError, "^pre_create_unconfirmed$"):
                    fixture.caller(Operation.START).start(Upload(fixture.target))
                rechecked.assert_called_once()
                self.assertEqual(rechecked.call_args.args[0].start_id, fixture.record.start_id)
                if condition == "stop after recheck":
                    launch.assert_called_once()
                    self.assertEqual(timeline, ["read", "recheck", "effect", "stop"])
                else:
                    launch.assert_not_called()
                    expected = ["read", "stop", "recheck"] if condition == "stop" else ["read", "recheck"]
                    self.assertEqual(timeline, expected)
                self.assertNotIn("create", fixture.steps())
                self.assertTrue(fixture.caller(Operation.STOP).stop())
                fixture.status = "terminal"
                self.assertIs(fixture.finalize(), RegistryDisposition.ALREADY_ABSENT)
                self.assertEqual(fixture.steps().count("delete_worker"), 1)
                self.assertEqual(fixture.steps().count("delete_container"), 1)

    def test_unconfirmed_recheck_never_launches(self):
        target = Target("pre-fixture-runtime", 123, 1_800_000_000, "0473/0465")
        binding = binding_for(Deployment(target, START, WORKER, APP))
        launch = Mock()

        def read(_, *, request):
            return readback_for(binding, request)

        for response in (False, None, 1, RuntimeError("synthetic unavailable authority")):
            with self.subTest(response=response):
                recheck = Mock(side_effect=response) if isinstance(response, Exception) else Mock(return_value=response)
                with self.assertRaisesRegex(TargetError, "^pre_runtime_launch_not_authorized$"):
                    launch_with_verified_binding(binding, observe=read, recheck=recheck, launch=launch)
                recheck.assert_called_once_with(binding.deployment)
                launch.assert_not_called()

    def test_launch_reply_must_confirm_the_pinned_deployment(self):
        for identity in ("worker_version", "container_id"):
            with self.subTest(identity=identity):
                fixture = Fixture()
                launch_process = fixture.create_private

                def create(target, start_id, upload):
                    app = UUID(fixture.provisioned.container.id)
                    binding = binding_for(Deployment(target, start_id, UUID(int=target.run_id), app))

                    def read(_, *, request):
                        return readback_for(binding, request)

                    def launch(checked):
                        deployment = checked.deployment
                        actual = launch_process(deployment.target, deployment.start_id, upload)
                        return replace(actual, **{identity: UUID(int=999)})

                    return launch_with_verified_binding(binding, observe=read, recheck=lambda _: True, launch=launch)

                fixture.create_private = create
                with self.assertRaisesRegex(TargetError, "^pre_create_unconfirmed$"):
                    fixture.caller(Operation.START).start(Upload(fixture.target))
                self.assertEqual(fixture.steps().count("create"), 1)
                self.assertEqual(fixture.record.start_id, fixture.created.start_id)
                with self.assertRaisesRegex(TargetError, "^pre_start_not_admitted$"):
                    fixture.caller(Operation.START).start(Upload(fixture.target))
                self.assertTrue(fixture.caller(Operation.STOP).stop())


    def test_caller_reads_only_through_the_pinned_private_get_binding(self):
        for target in (ORIGINAL, RETAINED):
            with self.subTest(shape=target.shape):
                fixture = Fixture(target)
                pinned = fixture.caller(Operation.START).start(Upload(target))
                app = fixture.provisioned.container
                route = ProtectedRouteReadback(target, target.worker_name, app)
                expected = ProbeBindingIdentity(route.worker_id, pinned)
                binding = Mock(identity=expected)
                probe_reply = fixture.probe

                def response(method, path, **_options):
                    reply = probe_reply(pinned, Endpoint(path))
                    return PrivateHttpReply(reply.status, (str(reply.worker_version),), reply.body)

                def probe(deployment, endpoint):
                    if fixture.fenced or fixture.now >= target.expires_at:
                        raise TargetError("synthetic private probe refused")
                    return probe_private_get(deployment, endpoint, pinned_deployment=pinned,
                                             pinned_route=route, binding=binding)

                binding.request.side_effect = response
                fixture.probe = probe
                read = fixture.caller(Operation.PROBE).read
                for supplied, identity, route_pin in (
                    (replace(pinned, start_id=UUID(int=999)), expected, route),
                    (pinned, replace(expected, worker_id="sibling-service"), route),
                    (pinned, expected, replace(route, target=replace(target, scope="pre-fixture-other"))),
                ):
                    binding.identity = identity
                    route = route_pin
                    with self.assertRaisesRegex(TargetError, "^pre_probe_unconfirmed$"):
                        read(supplied, verified_receipt=RECEIPT)
                    binding.request.assert_not_called()
                route = ProtectedRouteReadback(target, target.worker_name, app)
                health, readiness = read(pinned, verified_receipt=RECEIPT)
                self.assertEqual((health.status, readiness.status), (200, 200))
                self.assertEqual(binding.request.call_args_list, [
                    call("GET", endpoint.value, expected=expected, follow_redirects=False,
                         timeout_seconds=TIMEOUT_SECONDS, max_body_bytes=BODY_CAP + 1)
                    for endpoint in (Endpoint.HEALTH, Endpoint.READINESS)
                ])
                binding.request.reset_mock()
                self.assertTrue(fixture.caller(Operation.STOP).stop())
                with self.assertRaisesRegex(TargetError, "^pre_probe_unconfirmed$"):
                    read(pinned, verified_receipt=RECEIPT)
                binding.request.assert_not_called()
                fixture.status = "terminal"
                self.assertIs(fixture.finalize(), RegistryDisposition.ALREADY_ABSENT)
                binding.request.assert_not_called()
