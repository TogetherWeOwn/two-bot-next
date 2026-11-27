"""Synthetic evidence for the default-off native PRE database grant guard."""

from dataclasses import FrozenInstanceError, replace
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_native_cleanup import Reservation
    from pre_native_db_grant_guard import (
        DatabaseIdentity, GrantReadback, RevocationReadback, verify_native_pre_db_grants,
    )
    from pre_registry_cleanup import RegistryReservation
    from pre_target import Target, TargetError
    from pre_worker_upload import DatabaseEndpoint


def evidence(shape="0473/0465", host="pre-db.example.invalid"):
    target = Target("pre-fixture-grants", 42, 1_800_000_000, shape)
    reference = "registry.example.invalid/test/two-bot-next-attendance-predecessor-staging@sha256:" + "a" * 64
    reservation = Reservation(RegistryReservation(target, reference), "b" * 32, None, target.started_at + 90)
    identity = DatabaseIdentity(
        DatabaseEndpoint(target, target.service_name, host, 5432, "fixture", "writer", True),
        "verify-full", True,
    )
    pre = GrantReadback(
        identity, "pre_migration", shape, complete=True,
        restricted_database_grants=0, restricted_schema_grants=0, restricted_role_grants=0,
        privileged_role_memberships=0, parameter_grants=0,
        superuser=False, bypassrls=False, createrole=False, createdb=False, replication=False, cross_schema_write=False,
    )
    cutoff = reservation.cutoff
    return dict(
        enabled=True, reservation=reservation, pinned_identity=identity,
        grant_rows=(pre, replace(pre, schema_mode="post_migration")),
        revocation=RevocationReadback(identity, cutoff, True, True, cutoff + 1, 0),
        stopped_at=cutoff + 1, now=cutoff + 2,
    )


class NativeDatabaseGrantGuardTests(unittest.TestCase):
    def assert_refused(self, reason, inputs, **changes):
        with self.assertRaises(TargetError) as raised:
            verify_native_pre_db_grants(**(inputs | changes))
        self.assertEqual(raised.exception.args, (reason,))

    def test_disabled_short_circuits_all_input(self):
        class Unreadable:
            def __getattribute__(self, name):
                raise AssertionError("evidence was accessed")

            def __iter__(self):
                raise AssertionError("evidence was iterated")

            def __bool__(self):
                raise AssertionError("flag was coerced")

        self.assertEqual(verify_native_pre_db_grants().code, "disabled")
        unreadable = Unreadable()
        for enabled in (False, None, 1, "true", unreadable):
            verdict = verify_native_pre_db_grants(
                enabled=enabled, reservation=unreadable, pinned_identity=unreadable,
                grant_rows=unreadable, revocation=unreadable, stopped_at=unreadable, now=unreadable,
            )
            self.assertEqual(verdict.code, "disabled")
            with self.assertRaises(FrozenInstanceError):
                verdict.code = "verified"

    def test_accepts_both_shapes_and_revocation_time_boundaries(self):
        for shape in ("0473/0465", "0475/0465"):
            with self.subTest(shape=shape):
                inputs = evidence(shape)
                self.assertEqual(verify_native_pre_db_grants(**inputs).code, "verified")
                cutoff = inputs["reservation"].cutoff
                inputs.update(stopped_at=cutoff, now=cutoff, grant_rows=inputs["grant_rows"][::-1],
                              revocation=replace(inputs["revocation"], observed_after_revocation=cutoff))
                self.assertEqual(verify_native_pre_db_grants(**inputs).code, "verified")

    def test_pins_endpoint_and_tls_for_grants_and_revocation(self):
        inputs = evidence()
        pin = inputs["pinned_identity"]
        pre, post = inputs["grant_rows"]
        endpoint = pin.endpoint
        for field, value in (
            ("host", "sibling.example.invalid"), ("port", 5433), ("database", "sibling"), ("user", "sibling"),
            ("target", replace(endpoint.target, run_id=43)), ("service", "sibling"), ("disposable", 1),
        ):
            with self.subTest(field=field):
                wrong = replace(pin, endpoint=replace(endpoint, **{field: value}))
                self.assert_refused("pre_database_endpoint_mismatch", inputs,
                                    grant_rows=(pre, replace(post, identity=wrong)))
                self.assert_refused("wrong_pre_db_revocation_target", inputs,
                                    revocation=replace(inputs["revocation"], identity=wrong))
        for wrong in (replace(pin, tls_mode="require"), replace(pin, tls_required=False),
                      replace(pin, tls_required=1)):
            self.assert_refused("pre_database_endpoint_mismatch", inputs,
                                grant_rows=(replace(pre, identity=wrong), post))
            self.assert_refused("wrong_pre_db_revocation_target", inputs,
                                revocation=replace(inputs["revocation"], identity=wrong))
        self.assert_refused("invalid_pre_database_pin", inputs, pinned_identity=replace(pin, tls_mode="disable"))
        self.assert_refused("invalid_pre_database_pin", inputs, pinned_identity=replace(pin, tls_required=False))

    def test_accepts_native_loopback_only_when_host_matches_pin(self):
        for host, alias in (("localhost", "127.0.0.1"), ("127.0.0.1", "localhost")):
            with self.subTest(host=host):
                inputs = evidence(host=host)
                self.assertEqual(verify_native_pre_db_grants(**inputs).code, "verified")
                pre, post = inputs["grant_rows"]
                wrong = replace(post.identity, endpoint=replace(post.identity.endpoint, host=alias))
                self.assert_refused("pre_database_endpoint_mismatch", inputs,
                                    grant_rows=(pre, replace(post, identity=wrong)))

    def test_requires_complete_inventory_and_both_schema_modes(self):
        inputs = evidence()
        pre, post = inputs["grant_rows"]
        for rows in (None, (), (pre,), (pre, post, post), [pre, post]):
            self.assert_refused("invalid_pre_db_grant_rows", inputs, grant_rows=rows)
        for incomplete in (None, replace(pre, complete=False), replace(pre, complete=1)):
            self.assert_refused("incomplete_pre_db_grants", inputs, grant_rows=(incomplete, post))
        for rows in ((pre, pre), (post, post), (pre, replace(post, schema_mode="unknown")),
                     (replace(pre, schema_shape="0475/0465"), post),
                     (pre, replace(post, schema_shape="0475/0465"))):
            self.assert_refused("pre_db_schema_state_mismatch", inputs, grant_rows=rows)

    def test_refuses_each_restricted_grant_and_privilege_in_either_mode(self):
        inputs = evidence()
        for index, row in enumerate(inputs["grant_rows"]):
            for field in ("restricted_database_grants", "restricted_schema_grants", "restricted_role_grants"):
                for count in (1, -1, False, 0.0, None):
                    with self.subTest(mode=row.schema_mode, field=field, count=count):
                        rows = list(inputs["grant_rows"])
                        rows[index] = replace(row, **{field: count})
                        self.assert_refused("pre_db_restricted_grants", inputs, grant_rows=tuple(rows))
            for field in ("privileged_role_memberships", "parameter_grants"):
                for count in (1, -1, False, 0.0, None):
                    with self.subTest(mode=row.schema_mode, field=field, count=count):
                        rows = list(inputs["grant_rows"])
                        rows[index] = replace(row, **{field: count})
                        self.assert_refused("pre_db_privileged_grants", inputs, grant_rows=tuple(rows))
            for field in ("superuser", "bypassrls", "createrole", "createdb", "replication", "cross_schema_write"):
                for flag in (True, 0, None):
                    with self.subTest(mode=row.schema_mode, field=field, flag=flag):
                        rows = list(inputs["grant_rows"])
                        rows[index] = replace(row, **{field: flag})
                        self.assert_refused("pre_db_privileged_grants", inputs, grant_rows=tuple(rows))

    def test_revocation_requires_cutoff_post_stop_readback_and_no_effective_writer(self):
        inputs = evidence()
        cutoff = inputs["reservation"].cutoff
        started_at = inputs["reservation"].registry.target.started_at
        revocation = inputs["revocation"]
        for timestamp in (cutoff + 1, started_at - 1, float(cutoff), True):
            self.assert_refused("invalid_pre_db_revocation_time", inputs,
                                revocation=replace(revocation, revoked_at=timestamp))
        for timestamp in (cutoff, inputs["now"] + 1, float(cutoff + 1), True):
            self.assert_refused("stale_pre_db_revocation_readback", inputs,
                                revocation=replace(revocation, observed_after_revocation=timestamp))
        self.assert_refused("stale_pre_db_revocation_readback", inputs, stopped_at=cutoff - 1,
                            revocation=replace(revocation, observed_after_revocation=cutoff - 1))
        for field, values in (("delegated_roles_revoked", (False, 1, None)),
                              ("active_sessions_terminated", (False, 1, None)),
                              ("remaining_write_grants", (1, -1, False, 0.0, None))):
            for value in values:
                with self.subTest(field=field, value=value):
                    self.assert_refused("pre_writer_revocation_unconfirmed", inputs,
                                        revocation=replace(revocation, **{field: value}))
        self.assert_refused("wrong_pre_db_revocation_target", inputs, revocation=None)
        self.assert_refused("invalid_pre_clock", inputs, now=True)
        self.assert_refused("invalid_pre_db_stop_time", inputs, stopped_at=inputs["now"] + 1)

    def test_malformed_fields_cannot_supply_equality_or_leak_in_diagnostics(self):
        class PretendsToMatch(str):
            def __eq__(self, other):
                raise AssertionError("untrusted equality was invoked")

        inputs = evidence()
        pin = inputs["pinned_identity"]
        pre, post = inputs["grant_rows"]
        sentinel = "synthetic-private-value"
        dsn = f"postgresql://fixture:{sentinel}@pre-db.example.invalid/fixture"
        invalid = replace(pin, endpoint=replace(pin.endpoint, host=dsn), tls_mode=dsn)
        bad_row = replace(post, identity=invalid, schema_shape=dsn)
        bad_revocation = replace(inputs["revocation"], identity=invalid, revoked_at=dsn)
        for value in (invalid, bad_row, bad_revocation, verify_native_pre_db_grants(**inputs),
                      verify_native_pre_db_grants(pinned_identity=invalid)):
            self.assertNotIn(sentinel, repr(value))
            self.assertNotIn(sentinel, str(value))
        self.assert_refused("invalid_pre_database_pin", inputs, pinned_identity=invalid)
        self.assert_refused("pre_database_endpoint_mismatch", inputs, grant_rows=(pre, bad_row))
        self.assert_refused("wrong_pre_db_revocation_target", inputs, revocation=bad_revocation)
        fake = replace(pin, endpoint=replace(pin.endpoint, user=PretendsToMatch("writer")))
        self.assert_refused("pre_database_endpoint_mismatch", inputs,
                            grant_rows=(pre, replace(post, identity=fake)))
        self.assert_refused("pre_db_schema_state_mismatch", inputs,
                            grant_rows=(pre, replace(post, schema_shape=PretendsToMatch(post.schema_shape))))
