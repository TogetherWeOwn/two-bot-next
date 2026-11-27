"""Synthetic proofs for both PRE shapes with no provider transport."""

from dataclasses import replace
from pathlib import Path
import sys
import unittest
from unittest.mock import patch
from uuid import UUID

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_native_first_create import (InitialDeployment, PrivateBindingPin, PrivateBindingReadback,
                                         verify_native_first_create)
    from pre_target import Target, TargetError


TARGET = Target("pre-fixture-native", 203, 1_800_000_000, "0473/0465")
OTHER_SHAPE = replace(TARGET, shape="0475/0465")
ENVIRONMENT = "pre-staging"
VERSION = UUID("5cb771af-3421-4223-8a06-39e061f06400")
OTHER_VERSION = UUID("157558d8-f4fd-45b3-bf3a-62a1449062e0")
BINDING_ID = UUID("815cf82b-cadb-474f-80c9-9a9750f958fb")
BAD_UUIDS = (None, "not-a-uuid", str(VERSION), VERSION.int, UUID(int=0))


class EmptyTupleSubclass(tuple):
    pass


class StringSubclass(str):
    pass


def evidence(target=TARGET):
    destination = dict(target=target, worker_service=target.worker_name, environment=ENVIRONMENT, version=VERSION)
    return dict(
        reserved_environment=ENVIRONMENT,
        initial=InitialDeployment(**destination, workers_dev=False, preview_urls=False, complete=True,
                                  routes=(), custom_domains=(), history_complete=True, prior_versions=(),
                                  closed_from_first_effect=True),
        binding=PrivateBindingPin(**destination, binding_id=BINDING_ID),
        serving=PrivateBindingReadback(**destination, binding_id=BINDING_ID,
                                       via_private_binding=True, independent=True),
    )


class PreNativeFirstCreateTests(unittest.TestCase):
    def refuse(self, reason, *, target=TARGET, **changes):
        with self.assertRaisesRegex(TargetError, f"^{reason}$"):
            verify_native_first_create(target, **(evidence() | changes))

    def refuse_fields(self, part, reason, cases):
        original = evidence()[part]
        for field, values in cases.items():
            for value in values:
                with self.subTest(part=part, field=field, value=value):
                    self.refuse(reason, **{part: replace(original, **{field: value})})

    def test_both_pre_shapes_verify_offline(self):
        with patch("socket.socket", side_effect=AssertionError("network effect")), \
                patch("subprocess.Popen", side_effect=AssertionError("subprocess effect")):
            for target in (TARGET, OTHER_SHAPE):
                with self.subTest(shape=target.shape):
                    self.assertEqual(verify_native_first_create(target, **evidence(target)), VERSION)

    def test_missing_or_malformed_reservation(self):
        for target in (None, TARGET.worker_name, replace(TARGET, shape=StringSubclass(TARGET.shape)),
                       replace(TARGET, scope=StringSubclass(TARGET.scope))):
            with self.subTest(target=target):
                self.refuse("invalid_pre_native_target", target=target)
        for environment in (None, 0, "", " ", " pre-staging", "pre-staging ", "pre-staging\n", "pre\0staging",
                            StringSubclass(ENVIRONMENT)):
            with self.subTest(environment=environment):
                self.refuse("invalid_pre_native_environment", reserved_environment=environment)
        missing = {name: replace(evidence()[name], environment="") for name in ("initial", "binding", "serving")}
        self.refuse("invalid_pre_native_environment", reserved_environment="", **missing)

    def test_each_receipt_must_name_the_exact_reservation_and_destination(self):
        foreign_scope = replace(TARGET, scope="pre-fixture-other")
        self.assertEqual(foreign_scope.worker_name, TARGET.worker_name)
        self.assertEqual(OTHER_SHAPE.worker_name, TARGET.worker_name)
        cases = {
            "target": (None, TARGET.worker_name, foreign_scope, OTHER_SHAPE,
                       replace(TARGET, run_id=304), replace(TARGET, started_at=TARGET.started_at + 1)),
            "worker_service": (None, "", TARGET.service_name, "two-bot-next-staging",
                               StringSubclass(TARGET.worker_name)),
            "environment": (None, "", "production", "PRE-STAGING", StringSubclass(ENVIRONMENT)),
        }
        for part, reason in (("initial", "wrong_pre_native_create_target"),
                             ("binding", "wrong_pre_native_binding"),
                             ("serving", "pre_native_dispatch_unconfirmed")):
            self.refuse_fields(part, reason, cases)

    def test_wrong_receipt_types_are_refused(self):
        self.refuse("wrong_pre_native_create_target", initial=None)
        self.refuse("wrong_pre_native_binding", binding=None)
        self.refuse("pre_native_dispatch_unconfirmed", serving=None)
        self.refuse("pre_native_dispatch_unconfirmed", serving=evidence()["binding"])
        self.refuse("pre_native_dispatch_unconfirmed", serving=evidence()["initial"])

    def test_initial_version_requires_a_nonzero_uuid(self):
        self.refuse_fields("initial", "invalid_pre_native_initial_version", {"version": BAD_UUIDS})

    def test_current_ingress_must_be_complete_and_closed(self):
        self.refuse_fields("initial", "pre_native_ingress_unconfirmed", {
            "complete": (False, None, 1),
            "workers_dev": (True, None, 0),
            "preview_urls": (True, None, 0),
            "routes": (("public.example.invalid/*",), [], None, EmptyTupleSubclass()),
            "custom_domains": (("public.example.invalid",), [], None, EmptyTupleSubclass()),
        })

    def test_closed_snapshot_cannot_supply_first_create_proof(self):
        self.refuse_fields("initial", "pre_native_first_create_unconfirmed", {
            "history_complete": (False, None, 1),
            "prior_versions": ((OTHER_VERSION,), (VERSION,), [], None, EmptyTupleSubclass()),
            "closed_from_first_effect": (False, None, 1),
        })

    def test_pin_must_identify_one_binding_and_the_initial_version(self):
        self.refuse_fields("binding", "wrong_pre_native_binding", {"binding_id": BAD_UUIDS})
        self.refuse_fields("binding", "wrong_pre_native_binding_version", {"version": (*BAD_UUIDS, OTHER_VERSION)})
        valid = evidence()
        self.refuse("wrong_pre_native_binding_version", binding=replace(valid["binding"], version=OTHER_VERSION),
                    serving=replace(valid["serving"], version=OTHER_VERSION))

    def test_serving_readback_must_be_independent_and_over_the_pinned_binding(self):
        self.refuse_fields("serving", "pre_native_dispatch_unconfirmed", {
            "binding_id": (*BAD_UUIDS, OTHER_VERSION),
            "via_private_binding": (False, None, 1),
            "independent": (False, None, 1),
        })
        self.refuse_fields("serving", "wrong_pre_native_serving_version", {"version": (*BAD_UUIDS, OTHER_VERSION)})


if __name__ == "__main__":
    unittest.main()
