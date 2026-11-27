"""Synthetic exact-scope checks for the dormant native image pin verifier."""

from dataclasses import FrozenInstanceError, replace
from pathlib import Path
import sys
import unittest
from unittest.mock import ANY, Mock, call, patch
from uuid import UUID

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_native_image_pin import (
        ImageIdentity, ImageMapReadback, LoopbackFixtures, NativeImageAdmission, NativeImagePinResult,
        NativeInspect, NativeScope, NativeStart, verify_native_image_pin,
    )
    from pre_start_stop import Deployment, EffectWindow
    from pre_target import Target, TargetError


BEGIN = 1800000000
MANIFEST = "sha256:" + "a" * 64
CONFIG = "sha256:" + "b" * 64
FOREIGN = "sha256:" + "c" * 64
REPOSITORY = "registry.example.invalid/pre/replacement"
REFERENCE = f"{REPOSITORY}@{MANIFEST}"
ALIAS = "pre-replacement"
INSTANCE = "fixture-instance"
SUBCLASS_SCOPE = type("ScopeText", (str,), {})("pre-fixture-test")


class Fixture:
    def __init__(self, *, ipv6=False):
        shape = "0475/0465" if ipv6 else "0473/0465"
        target = Target("pre-fixture-test", 123, BEGIN, shape)
        deployment = Deployment(target, UUID(int=1), UUID(int=2), UUID(int=3))
        self.scope = NativeScope(deployment, "a" * 32, "pre-test", "b" * 32, "c" * 64)
        self.image = ImageIdentity(REFERENCE, MANIFEST, CONFIG, "linux/amd64")
        host = "::1" if ipv6 else "127.0.0.1"
        authority = f"[{host}]" if ipv6 else host
        self.fixtures = LoopbackFixtures(f"ws://{authority}:8081", f"http://{authority}/", host, 5432,
                                         postgres_disposable=True, postgres_http_intercept=False,
                                         in_container=True, enable_internet=False)
        self.admission = NativeImageAdmission(self.scope, EffectWindow(target, BEGIN + 10),
                                              ALIAS, self.image, self.fixtures)
        self.receipt = NativeStart(self.scope, INSTANCE, BEGIN + 2)
        self.observed = NativeInspect(self.scope, INSTANCE, True, MANIFEST, True)
        self.adapter = Mock()
        self.adapter.clock.side_effect = [BEGIN, BEGIN + 1, BEGIN + 3, BEGIN + 4]
        self.adapter.read_images.return_value = ImageMapReadback(self.scope, {ALIAS: REFERENCE})
        self.adapter.resolve_image.return_value = self.image
        self.adapter.start.return_value = self.receipt
        self.adapter.inspect.return_value = self.observed

    def verify(self):
        return verify_native_image_pin(enabled=True, admission=self.admission,
                                       read_images=self.adapter.read_images, resolve_image=self.adapter.resolve_image,
                                       clock=self.adapter.clock, start=self.adapter.start, inspect=self.adapter.inspect)


class NativeImagePinTests(unittest.TestCase):
    def test_disabled_is_frozen_and_never_calls_adapters(self):
        trap = Mock(side_effect=AssertionError("disabled adapter called"))
        result = verify_native_image_pin()
        self.assertEqual(result, NativeImagePinResult(False))
        with self.assertRaises(FrozenInstanceError):
            result.verified = True
        for enabled in (False, None, 1, "true", object()):
            with self.subTest(enabled=enabled):
                self.assertIs(verify_native_image_pin(enabled=enabled, admission=object(), read_images=trap,
                                                       resolve_image=trap, clock=trap, start=trap, inspect=trap), result)
        trap.assert_not_called()

    def test_verifies_exact_start_and_inspect_with_ipv4_or_ipv6_fixtures(self):
        for ipv6 in (False, True):
            with self.subTest(ipv6=ipv6):
                f = Fixture(ipv6=ipv6)
                self.assertEqual(f.verify(), NativeImagePinResult(True, INSTANCE, MANIFEST))
                self.assertEqual(f.adapter.mock_calls, [
                    call.clock(), call.read_images(f.scope), call.resolve_image(REFERENCE), call.clock(),
                    call.start(f.admission, REFERENCE), call.clock(), call.inspect(f.scope, INSTANCE), call.clock(),
                ])

    def test_refuses_mutable_or_unexpected_image_map_before_resolution(self):
        for images in ({}, {"unexpected": REFERENCE}, {ALIAS: REFERENCE, "extra": REFERENCE},
                       {ALIAS: REPOSITORY}, {ALIAS: f"{REPOSITORY}:latest"},
                       {ALIAS: f"{REPOSITORY}@{FOREIGN}"}):
            with self.subTest(images=images):
                f = Fixture()
                f.adapter.read_images.return_value = ImageMapReadback(f.scope, images)
                with self.assertRaisesRegex(TargetError, "map_unconfirmed$"):
                    f.verify()
                f.adapter.resolve_image.assert_not_called()
                f.adapter.start.assert_not_called()

    def test_refuses_invalid_build_pin_before_adapters(self):
        for reference in (f"{REPOSITORY}:latest", f"{REPOSITORY}:latest@{MANIFEST}", f"repo@{MANIFEST}"):
            with self.subTest(reference=reference):
                f = Fixture()
                f.admission = replace(f.admission, image=replace(f.image, reference=reference))
                with self.assertRaisesRegex(TargetError, "admission_invalid$"):
                    f.verify()
                self.assertEqual(f.adapter.mock_calls, [])

    def test_refuses_images_from_another_object_environment_or_deployment(self):
        scope = Fixture().scope
        deployment = scope.deployment
        for change in (dict(environment="other"), dict(object_id="d" * 64), dict(account_id="e" * 32),
                       dict(namespace_id="f" * 32), dict(deployment=replace(deployment, worker_version=UUID(int=4))),
                       dict(deployment=replace(deployment, target=replace(deployment.target, run_id=124))),
                       dict(deployment=replace(deployment, target=replace(deployment.target, scope=SUBCLASS_SCOPE))),
                       dict(deployment=replace(deployment, target=replace(deployment.target, shape=ANY)))):
            with self.subTest(change=change):
                f = Fixture()
                f.adapter.read_images.return_value = ImageMapReadback(replace(scope, **change), {ALIAS: REFERENCE})
                with self.assertRaisesRegex(TargetError, "map_unconfirmed$"):
                    f.verify()
                f.adapter.resolve_image.assert_not_called()

    def test_refuses_manifest_config_platform_or_reference_mismatch(self):
        for change in (dict(manifest_digest=FOREIGN, reference=f"{REPOSITORY}@{FOREIGN}"),
                       dict(config_digest=FOREIGN), dict(platform="linux/arm64"),
                       dict(reference=f"other.example.invalid/pre/replacement@{MANIFEST}")):
            with self.subTest(change=change):
                f = Fixture()
                f.adapter.resolve_image.return_value = replace(f.image, **change)
                with self.assertRaisesRegex(TargetError, "resolution_unconfirmed$"):
                    f.verify()
                f.adapter.start.assert_not_called()

    def test_refuses_internet_or_nonlocal_fixture_configuration(self):
        for change in (dict(enable_internet=True), dict(enable_internet=0), dict(in_container=False),
                       dict(discord_gateway_url="ws://example.invalid:8081"),
                       dict(discord_gateway_url="ws://127.0.0.1:65536"),
                       dict(discord_api_base="http://127.0.0.1@example.invalid"),
                       dict(discord_api_base="http://[::ffff:127.0.0.1]"),
                       dict(postgres_host="db.example.invalid"), dict(postgres_host="::"),
                       dict(postgres_disposable=False), dict(postgres_http_intercept=True)):
            with self.subTest(change=change):
                f = Fixture()
                f.admission = replace(f.admission, fixtures=replace(f.fixtures, **change))
                with self.assertRaises(TargetError):
                    f.verify()
                self.assertEqual(f.adapter.mock_calls, [])

    def test_checks_window_before_start_and_through_inspect(self):
        cutoff, expired = BEGIN + 10, "effect_window_expired$"
        for ticks, starts, inspections, reason in (
            ([cutoff], 0, 0, expired), ([BEGIN, cutoff], 0, 0, expired),
            ([BEGIN, BEGIN + 1, cutoff], 1, 0, expired), ([BEGIN, BEGIN + 1, BEGIN + 3, cutoff], 1, 1, expired),
            ([BEGIN + 1, BEGIN], 0, 0, "clock_regressed$"), ([OSError()], 0, 0, "clock_unconfirmed$"),
        ):
            with self.subTest(ticks=ticks):
                f = Fixture()
                f.adapter.clock.side_effect = ticks
                with self.assertRaisesRegex(TargetError, reason):
                    f.verify()
                self.assertEqual(f.adapter.start.call_count, starts)
                self.assertEqual(f.adapter.inspect.call_count, inspections)

    def test_refuses_window_from_another_reservation(self):
        for change in (dict(run_id=124), dict(scope=SUBCLASS_SCOPE), dict(shape=ANY)):
            with self.subTest(change=change):
                f = Fixture()
                window = replace(f.admission.window, target=replace(f.scope.deployment.target, **change))
                f.admission = replace(f.admission, window=window)
                with self.assertRaisesRegex(TargetError, "admission_invalid$"):
                    f.verify()
                self.assertEqual(f.adapter.mock_calls, [])

    def test_refuses_stale_future_or_unbound_start_receipt(self):
        for change in (dict(started_at=BEGIN), dict(started_at=BEGIN + 10),
                       dict(scope=replace(Fixture().scope, environment="other")), dict(container_id="")):
            with self.subTest(change=change):
                f = Fixture()
                f.adapter.start.return_value = replace(f.receipt, **change)
                with self.assertRaisesRegex(TargetError, "start_unconfirmed$"):
                    f.verify()
                f.adapter.inspect.assert_not_called()

    def test_inspect_requires_running_instance_image_and_fixtures(self):
        for change in (dict(image_digest=FOREIGN), dict(container_id="other-instance"), dict(running=False),
                       dict(fixtures_ready_in_container=False), dict(scope=replace(Fixture().scope, object_id="d" * 64))):
            with self.subTest(change=change):
                f = Fixture()
                f.adapter.inspect.return_value = replace(f.observed, **change)
                with self.assertRaisesRegex(TargetError, "inspect_unconfirmed$"):
                    f.verify()
                f.adapter.start.assert_called_once()

    def test_lost_start_does_not_retry_or_expose_adapter_error(self):
        f = Fixture()
        f.adapter.start.side_effect = OSError("synthetic private payload")
        with self.assertRaisesRegex(TargetError, "^pre_native_image_start_unconfirmed$") as error:
            f.verify()
        self.assertIsNone(error.exception.__context__)
        f.adapter.start.assert_called_once()
        f.adapter.inspect.assert_not_called()
