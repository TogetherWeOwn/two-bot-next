"""Synthetic binding metadata for the dormant native PRE verifier."""

from dataclasses import FrozenInstanceError, replace
from pathlib import Path
import sys
import unittest
from unittest.mock import patch
from uuid import UUID

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_native_secret_scope import (
        BindingInventory, BindingScope, BindingTeardown, PromotionReadback, SecretBinding,
        SecretScopeReservation, SecretScopeVerdict, verify_native_pre_secret_scope,
    )
    from pre_start_stop import Deployment, EffectWindow
    from pre_target import Target, TargetError


def digest(character):
    return "sha256:" + character * 64


def evidence(shape="0473/0465"):
    start = 1_800_000_000
    target = Target("pre-fixture-demo", 123, start, shape)
    scope = BindingScope(Deployment(target, UUID(int=1), UUID(int=2), UUID(int=3)), target.worker_name, "pre")
    window = EffectWindow(replace(target), start + 60)
    reservation = SecretScopeReservation(
        scope, window, frozenset({"PRE_WRITER", "PRE_FIXTURE", "PROD_SECRET", "STAGING_SECRET"}),
        frozenset({"PROD_SECRET", "STAGING_SECRET"}), frozenset({digest("a")}), frozenset({digest("b")}),
    )
    rows = (
        SecretBinding("PRE_WRITER", "first", digest("c"), True, False, False, False),
        SecretBinding("PRE_FIXTURE", "second", digest("d"), True, False, False, False),
    )
    return dict(
        enabled=True, reservation=reservation, promotion=PromotionReadback(True, start + 60),
        inventory=BindingInventory(scope, start + 20, True, rows),
        teardown=BindingTeardown(scope, start + 59, True, start + 60, start + 60, 0),
        readback_not_before=start + 60, now=start + 61,
    )


class Unreadable:
    def fail(self, *_args):
        raise AssertionError("unexpected access to untrusted metadata")

    __bool__ = __iter__ = __eq__ = __repr__ = fail


class NativeSecretScopeTests(unittest.TestCase):
    def assert_rejected(self, expected, current, **changes):
        with self.assertRaisesRegex(TargetError, f"^{expected}$"):
            verify_native_pre_secret_scope(**(current | changes))

    def test_disabled_ignores_evidence_and_requires_literal_true(self):
        poison = Unreadable()
        self.assertEqual(verify_native_pre_secret_scope(), SecretScopeVerdict(False, False))
        for enabled in (False, None, 1, "true", poison):
            verdict = verify_native_pre_secret_scope(
                enabled=enabled, reservation=poison, promotion=poison, inventory=poison,
                teardown=poison, readback_not_before=poison, now=poison,
            )
            self.assertEqual(verdict, SecretScopeVerdict(False, False))
            with self.assertRaises(FrozenInstanceError):
                verdict.verified = True

    def test_exact_scope_passes_for_both_shapes_at_cutoff(self):
        for shape in ("0473/0465", "0475/0465"):
            with self.subTest(shape=shape):
                self.assertEqual(verify_native_pre_secret_scope(**evidence(shape)), SecretScopeVerdict(True, True))

    def test_empty_inventory_requires_complete_readback_and_teardown(self):
        current = evidence()
        current["inventory"] = replace(current["inventory"], bindings=())
        self.assertEqual(verify_native_pre_secret_scope(**current), SecretScopeVerdict(True, True))
        self.assert_rejected("unconfirmed_pre_secret_inventory", current,
                             inventory=replace(current["inventory"], complete=False))
        self.assert_rejected("wrong_pre_secret_teardown_scope", current, teardown=None)

    def test_scope_mismatches_fail_for_inventory_and_teardown(self):
        current = evidence()
        scope = current["reservation"].scope
        deployment = scope.deployment
        other_target = replace(deployment.target, scope="pre-fixture-other")
        for wrong in (
            replace(scope, worker_service="two-bot-next-production"), replace(scope, environment="staging"),
            replace(scope, deployment=replace(deployment, target=other_target)),
            replace(scope, deployment=replace(deployment, start_id=UUID(int=4))),
            replace(scope, deployment=replace(deployment, worker_version=UUID(int=4))),
            replace(scope, deployment=replace(deployment, container_id=UUID(int=4))),
            replace(scope, deployment=replace(deployment, worker_version=Unreadable())),
        ):
            for field in ("inventory", "teardown"):
                with self.subTest(field=field):
                    self.assert_rejected(f"wrong_pre_secret_{field}_scope", current,
                                         **{field: replace(current[field], scope=wrong)})
        window = EffectWindow(other_target, current["reservation"].window.cutoff)
        self.assert_rejected("invalid_pre_secret_reservation", current,
                             reservation=replace(current["reservation"], window=window))

    def test_promotion_readback_must_be_true_and_fresh(self):
        current = evidence()
        floor = current["readback_not_before"]
        for observed in (None, PromotionReadback(False, floor), PromotionReadback(1, floor),
                         PromotionReadback(True, floor - 1), PromotionReadback(True, current["now"] + 1)):
            self.assert_rejected("pre_auto_promote_disable_unconfirmed", current, promotion=observed)
        self.assert_rejected("invalid_pre_clock", current, now=True)
        self.assert_rejected("invalid_pre_readback_window", current, readback_not_before=current["now"] + 1)

    def test_inventory_and_protected_catalogs_must_be_well_formed(self):
        current = evidence()
        inventory = current["inventory"]
        for change in (dict(complete=False), dict(complete=1), dict(bindings=Unreadable()),
                       dict(observed_at=current["reservation"].window.cutoff)):
            self.assert_rejected("unconfirmed_pre_secret_inventory", current, inventory=replace(inventory, **change))
        for field in ("allowed_secret_names", "protected_secret_names", "production_binding_digests",
                      "live_staging_binding_digests"):
            for malformed in (None, frozenset({"invalid value"})):
                self.assert_rejected("invalid_pre_secret_policy", current,
                                     reservation=replace(current["reservation"], **{field: malformed}))

    def test_forbidden_or_aliased_bindings_fail_closed(self):
        current = evidence()
        inventory = current["inventory"]
        first, second = inventory.bindings
        for code, changes in (
            ("forbidden_pre_secret_name", dict(name="UNKNOWN_SECRET")),
            ("forbidden_pre_secret_name", dict(name="PROD_SECRET")),
            ("forbidden_pre_secret_name", dict(name="STAGING_SECRET")),
            ("pre_secret_binding_not_isolated", dict(disposable=False)),
            ("pre_secret_binding_not_isolated", dict(plaintext_exposed=True)),
            ("pre_secret_binding_not_isolated", dict(plaintext_exposed=0)),
            ("pre_secret_binding_not_isolated", dict(live_discord_bot_token=True)),
            ("pre_secret_binding_not_isolated", dict(shared_production_resource=True)),
            ("invalid_pre_secret_identity", dict(secret_version_id="")),
            ("invalid_pre_secret_identity", dict(binding_digest="invalid")),
            ("shared_pre_secret_digest", dict(binding_digest=digest("a"))),
            ("shared_pre_secret_digest", dict(binding_digest=digest("b"))),
            ("duplicate_pre_secret_binding", dict(name=first.name)),
            ("duplicate_pre_secret_binding", dict(secret_version_id=first.secret_version_id)),
            ("duplicate_pre_secret_binding", dict(binding_digest=first.binding_digest)),
        ):
            with self.subTest(changes=changes):
                self.assert_rejected(code, current, inventory=replace(inventory, bindings=(
                    first, replace(second, **changes),
                )))
        self.assert_rejected("invalid_pre_secret_binding", current, inventory=replace(inventory, bindings=(None,)))

    def test_teardown_must_follow_stop_and_confirm_absence_by_cutoff(self):
        current = evidence()
        teardown = current["teardown"]
        for changes in (
            dict(bindings_removed=False), dict(bindings_removed=1), dict(remaining_secret_count=1),
            dict(remaining_secret_count=False), dict(removed_at=teardown.stopped_at - 1),
            dict(stopped_at=current["inventory"].observed_at - 1),
            dict(removed_at=teardown.removed_at + 1, observed_at=teardown.observed_at + 1),
            dict(observed_at=teardown.removed_at - 1), dict(observed_at=current["now"] + 1),
        ):
            with self.subTest(changes=changes):
                self.assert_rejected("pre_secret_teardown_unconfirmed", current, teardown=replace(teardown, **changes))

    def test_formatted_metadata_and_errors_redact_malformed_credentials(self):
        current = evidence()
        markers = ("synthetic-token-canary", "synthetic-password-canary", "postgresql://fixture@sample.invalid/db")
        token, password, dsn = markers
        inventory = current["inventory"]
        binding = replace(inventory.bindings[0], secret_version_id=token, binding_digest=dsn)
        scope = replace(inventory.scope, environment=password)
        values = (scope, binding, replace(current["reservation"], allowed_secret_names=frozenset(markers)),
                  replace(current["promotion"], observed_at=token), replace(current["teardown"], removed_at=dsn),
                  replace(inventory, bindings=(binding,)), verify_native_pre_secret_scope(**current))
        with self.assertRaisesRegex(TargetError, "^invalid_pre_secret_identity$") as failure:
            verify_native_pre_secret_scope(**(current | dict(inventory=values[-2])))
        for value in (*values, failure.exception):
            for marker in markers:
                self.assertNotIn(marker, repr(value))
                self.assertNotIn(marker, str(value))
