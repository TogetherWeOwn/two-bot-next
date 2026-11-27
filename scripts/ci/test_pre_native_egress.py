"""Synthetic effective launch controls, using only in-memory trace adapters."""

from dataclasses import replace
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_native_egress import (
        API, COLD, FAULTS, GATEWAY, KEEPALIVE, PATHS, Endpoint, EgressObservation,
        LaunchObservation, Listener, Policy, PolicyObservation, Process,
        Reservation, verify_effective_launch_controls,
    )
    from pre_target import Target, TargetError


DIGEST = "sha256:" + "a" * 64
FIRST = Reservation(Target("pre-fixture-first", 21, 1_800_000_000, "0473/0465"),
                    Endpoint("tcp", "192.0.2.10", 5432))
RETAINED = Reservation(Target("pre-fixture-retained", 22, 1_800_000_000, "0475/0465"),
                       Endpoint("tcp", "2001:db8::10", 5432))
DENIED = frozenset(("dns", "direct_ipv4", "direct_ipv6", "https_wss"))


class Trace:
    def __init__(self):
        self.attempts = []
        self.change_policy = lambda reading: reading
        self.change_launch = lambda reading: reading

    def fields(self, attempt):
        reservation = FIRST if attempt.target == FIRST.target else RETAINED
        replacement = attempt.path == KEEPALIVE
        namespace = f"net:{reservation.target.run_id}:{int(replacement)}"
        fault = FAULTS.index(attempt.fault) + 1 if attempt.fault else 0
        offset = 100 * (fault + 2 * replacement) if fault else 0
        event = offset + (20 if replacement else 10)
        return reservation.postgres, namespace, 2 if replacement else 1, event

    def policy(self, attempt):
        postgres, namespace, generation, event = self.fields(attempt)
        policy = None
        if attempt.fault != "missing_policy":
            policy = Policy(namespace, generation, event - 2, True, (postgres,))
        return self.change_policy(PolicyObservation(attempt, policy))

    def probe(self, attempt):
        self.attempts.append(attempt)
        postgres, namespace, generation, event = self.fields(attempt)
        listener = None
        if attempt.fault != "missing_listener":
            listener = Listener(namespace, "127.0.0.1:8081", event - 1)
        forced_from = event - 10 if attempt.path == KEEPALIVE else None
        process, egress = None, None
        if attempt.fault is None:
            process = Process(namespace, generation, DIGEST, GATEWAY, API)
            egress = EgressObservation(namespace, generation, DENIED, postgres)
        return self.change_launch(LaunchObservation(
            attempt, attempt.path, namespace, event, forced_from, listener, process,
            attempt.fault, int(process is not None), 0, egress,
        ))


class EffectiveLaunchControlsTests(unittest.TestCase):
    def setUp(self):
        self.trace = Trace()

    def verify(self, first=FIRST, retained=RETAINED, digest=DIGEST):
        return verify_effective_launch_controls(
            first, retained, digest=digest, observe_policy=self.trace.policy, probe=self.trace.probe,
        )

    def test_requires_each_reservation_path_and_fault(self):
        self.assertIsNone(self.verify())
        self.assertEqual(
            [(attempt.target, attempt.path, attempt.fault) for attempt in self.trace.attempts],
            [(reservation.target, path, fault)
             for reservation in (FIRST, RETAINED) for path in PATHS for fault in (None, *FAULTS)],
        )

    def test_rejects_invalid_pins_and_reservation_collisions(self):
        colliding = replace(RETAINED, target=replace(FIRST.target, shape=RETAINED.target.shape))
        with self.assertRaisesRegex(TargetError, "^pre_egress_reservation_collision$"):
            self.verify(retained=colliding)
        with self.assertRaisesRegex(TargetError, "^pre_egress_invalid_digest_pin$"):
            self.verify(digest="latest")
        with self.assertRaisesRegex(TargetError, "^pre_egress_invalid_reservation$"):
            self.verify(retained=FIRST)

    def test_rejects_wrong_effective_settings_and_digest(self):
        cases = (
            ("gateway_url", GATEWAY + "/", "wrong_process_settings"),
            ("api_base", "http://localhost:8081", "wrong_process_settings"),
            ("api_base", None, "wrong_process_settings"),
            ("digest", "sha256:" + "b" * 64, "wrong_digest"),
        )
        for field, value, reason in cases:
            with self.subTest(field=field, value=value):
                self.trace.change_launch = lambda r: replace(
                    r, process=replace(r.process, **{field: value})
                ) if r.attempt.path == KEEPALIVE and r.process else r
                with self.assertRaisesRegex(TargetError, f"^pre_egress_{reason}$"):
                    self.verify()

    def test_correlates_namespace_readiness_and_restart(self):
        mutations = (
            (lambda r: replace(r, process=replace(r.process, namespace="net:other")), "process_policy_mismatch"),
            (lambda r: replace(r, listener=replace(r.listener, namespace="net:other")),
             "listener_not_ready"),
            (lambda r: replace(r, listener=replace(r.listener, ready_at=r.event)), "listener_not_ready"),
            (lambda r: replace(r, forced_from=None), "keepalive_trigger_unconfirmed"),
            (lambda r: replace(r, forced_from=9), "stale_keepalive_restart"),
            (lambda r: replace(r, path=COLD), "launch_unconfirmed"),
        )
        for mutate, reason in mutations:
            with self.subTest(reason=reason):
                self.trace.change_launch = lambda r: mutate(r) if r.attempt.path == KEEPALIVE and r.process else r
                with self.assertRaisesRegex(TargetError, f"^pre_egress_{reason}$"):
                    self.verify()
        self.trace.change_policy = lambda r: replace(r, policy=replace(r.policy, generation=1)) if (
            r.attempt.path == KEEPALIVE and not r.attempt.fault
        ) else r
        self.trace.change_launch = lambda r: replace(r, process=replace(r.process, generation=1)) if (
            r.attempt.path == KEEPALIVE and r.process
        ) else r
        with self.assertRaisesRegex(TargetError, "^pre_egress_stale_keepalive_restart$"):
            self.verify()

    def test_rejects_unenforced_policy_and_broad_or_cross_reservation_rules(self):
        for fields, reason in (
            ({"default_deny": False}, "policy_not_enforced"),
            ({"enforced_at": 10}, "policy_not_enforced"),
            ({"allowlist": (FIRST.postgres, RETAINED.postgres)}, "wrong_allowlist"),
            ({"allowlist": (RETAINED.postgres,)}, "wrong_allowlist"),
            ({"allowlist": (replace(FIRST.postgres, protocol="udp"),)}, "wrong_allowlist"),
            ({"allowlist": (replace(FIRST.postgres, address="0.0.0.0/0"),)}, "wrong_allowlist"),
        ):
            with self.subTest(fields=fields):
                self.trace.change_policy = lambda r: replace(r, policy=replace(r.policy, **fields))
                with self.assertRaisesRegex(TargetError, f"^pre_egress_{reason}$"):
                    self.verify()

    def test_requires_all_denials_zero_public_traffic_and_exact_postgres(self):
        for fields, reason in (
            *[({"denied": DENIED - {vector}}, "denial_unconfirmed") for vector in sorted(DENIED)],
            ({"generation": 99}, "denial_unconfirmed"),
            ({"postgres_allowed": RETAINED.postgres}, "postgres_unconfirmed"),
        ):
            with self.subTest(fields=fields):
                self.trace.change_launch = lambda r: replace(r, egress=replace(r.egress, **fields))
                with self.assertRaisesRegex(TargetError, f"^pre_egress_{reason}$"):
                    self.verify()
        self.trace.change_launch = lambda r: replace(r, public_discord_packets=1)
        with self.assertRaisesRegex(TargetError, "^pre_egress_public_traffic_unconfirmed$"):
            self.verify()

    def test_refusal_must_affirm_zero_starts_on_each_path(self):
        for path in PATHS:
            for fault in FAULTS:
                with self.subTest(path=path, fault=fault):
                    self.trace.change_launch = lambda r: replace(r, process_starts=1) if (
                        r.attempt.path, r.attempt.fault
                    ) == (path, fault) else r
                    with self.assertRaisesRegex(TargetError, "^pre_egress_refusal_unconfirmed$"):
                        self.verify()

    def test_rejects_replayed_observations_and_redacts_adapter_failures(self):
        self.trace.change_policy = lambda r: replace(r, attempt=replace(r.attempt))
        with self.assertRaisesRegex(TargetError, "^pre_egress_policy_unconfirmed$"):
            self.verify()
        self.trace.change_policy = lambda r: r
        with patch.object(self.trace, "change_launch", side_effect=RuntimeError("private-probe-detail")):
            with self.assertRaisesRegex(TargetError, "^pre_egress_launch_unconfirmed$") as failure:
                self.verify()
        self.assertIsNone(failure.exception.__context__)
