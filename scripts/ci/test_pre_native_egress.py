from dataclasses import replace
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_native_egress import (
        COLD, FAULTS, KEEPALIVE, Endpoint, EgressObservation,
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
OTHER_POSTGRES = {FIRST.target: RETAINED.postgres, RETAINED.target: FIRST.postgres}


class Trace:
    def fields(self, attempt, *, namespace=None):
        reservation = FIRST if attempt.target == FIRST.target else RETAINED
        replacement = attempt.path == KEEPALIVE
        namespace = namespace or f"net:{reservation.target.run_id}"
        fault = attempt.fault or attempt.baseline_for
        offset = 100 * (FAULTS.index(fault) + 1) if fault else 0
        event = offset + (20 if replacement else 5 if attempt.fault else 10)
        return reservation.postgres, namespace, event - 8, event

    def policy(self, attempt):
        postgres, namespace, generation, event = self.fields(attempt)
        policy = Policy(namespace, generation, event - 2, True, (postgres,)) if attempt.fault != "missing_policy" else None
        return PolicyObservation(attempt, policy)

    def probe(self, attempt):
        postgres, namespace, generation, event = self.fields(attempt)
        listener = Listener(namespace, "127.0.0.1:8081", event - 1) if attempt.fault != "missing_listener" else None
        forced_from = event - 10 if attempt.path == KEEPALIVE else None
        process, egress = None, None
        if attempt.fault is None:
            process = Process(namespace, generation, DIGEST, "ws://127.0.0.1:8081", "http://127.0.0.1:8081")
            egress = EgressObservation(namespace, generation, DENIED, postgres)
        return LaunchObservation(attempt, attempt.path, namespace, event, forced_from, listener, process,
                                 attempt.fault, int(process is not None), 0, egress)


class EffectiveLaunchControlsTests(unittest.TestCase):
    def setUp(self):
        self.trace = Trace()

    def verify(self, retained=RETAINED, digest=DIGEST):
        return verify_effective_launch_controls(FIRST, retained, digest=digest,
                                                observe_policy=self.trace.policy, probe=self.trace.probe)

    def rejects(self, reason, *, policy=None, launch=None, path=COLD, fault=None, baseline_for=None):
        def change(mutate, reading):
            a = reading.attempt
            matches = (a.target, a.path, a.fault, a.baseline_for) == trial
            return mutate(reading) if matches and mutate else reading
        read_policy, read_launch = self.trace.policy, self.trace.probe
        for target in (FIRST.target, RETAINED.target):
            trial = (target, path, fault, baseline_for)
            with (self.subTest(target=target), patch.object(self.trace, "policy", lambda a: change(policy, read_policy(a))),
                  patch.object(self.trace, "probe", lambda a: change(launch, read_launch(a))),
                  self.assertRaisesRegex(TargetError, f"^pre_egress_{reason}$")):
                self.verify()

    def test_requires_each_reservation_path_and_fault(self):
        with patch.object(self.trace, "probe", wraps=self.trace.probe) as probe:
            self.assertIsNone(self.verify())
        scenarios = [(COLD, None, None), (KEEPALIVE, None, None)]
        for fault in FAULTS:
            scenarios.extend([(COLD, fault, None), (COLD, None, fault), (KEEPALIVE, fault, None)])
        self.assertEqual([(a.target, a.path, a.fault, a.baseline_for) for (a,), _ in probe.call_args_list],
                         [(r.target, *scenario) for r in (FIRST, RETAINED) for scenario in scenarios])

    def test_rejects_invalid_pins_and_reservation_collisions(self):
        colliding = replace(FIRST.target, shape=RETAINED.target.shape)
        for args, reason in (
            ({"retained": replace(RETAINED, target=colliding)}, "reservation_collision"),
            ({"retained": replace(RETAINED, postgres=FIRST.postgres)}, "reservation_collision"),
            ({"retained": FIRST}, "invalid_reservation"), ({"digest": "latest"}, "invalid_digest_pin"),
            *[({"retained": replace(RETAINED, postgres=Endpoint("tcp", address, port))}, "invalid_reservation")
              for address, port in (("169.254.169.254", 80), ("127.0.0.1", 8080), ("127.0.0.1", 8081),
                                    ("100.100.100.200", 80), ("fd00:ec2::254", 80), ("::102:304", 5432),
                                    ("64:ff9b::a29f:87e8", 5432), ("2002:c000:201::", 5432), ("2001::2", 5432))],
        ):
            with self.assertRaisesRegex(TargetError, f"^pre_egress_{reason}$"):
                self.verify(**args)

    def test_rejects_wrong_effective_settings_and_digest(self):
        for field, value, reason in (
            ("gateway_url", "ws://localhost:8081", "wrong_process_settings"), ("gateway_url", None, "wrong_process_settings"),
            ("api_base", "http://localhost:8081", "wrong_process_settings"), ("api_base", None, "wrong_process_settings"),
            ("digest", "sha256:" + "b" * 64, "wrong_digest"),
        ):
            self.rejects(reason, path=KEEPALIVE, launch=lambda r: replace(r, process=replace(r.process, **{field: value})))

    def test_correlates_namespace_readiness_and_restart(self):
        self.rejects("not_cold_start", launch=lambda r: replace(r, forced_from=5))
        for mutate, reason in (
            (lambda r: replace(r, process=replace(r.process, namespace="other")), "process_policy_mismatch"),
            (lambda r: replace(r, process=replace(r.process, generation=3)), "process_policy_mismatch"),
            (lambda r: replace(r, listener=replace(r.listener, namespace="other")), "listener_not_ready"),
            (lambda r: replace(r, listener=replace(r.listener, ready_at=r.event)), "listener_not_ready"),
            (lambda r: replace(r, forced_from=None), "keepalive_trigger_unconfirmed"),
            (lambda r: replace(r, forced_from=9), "keepalive_trigger_unconfirmed"),
            (lambda r: replace(r, path=COLD), "launch_unconfirmed"),
            (lambda r: replace(r, refusal="missing_policy"), "process_unconfirmed"),
        ):
            self.rejects(reason, path=KEEPALIVE, launch=mutate)
        for fields in ({"generation": 1}, {"enforced_at": 8}):
            for fault in (None, "missing_listener"):
                self.rejects("stale_keepalive_restart", path=KEEPALIVE, fault=fault,
                             policy=lambda r: replace(r, policy=replace(r.policy, **fields)))
        self.rejects("stale_namespace_policy", fault="missing_listener",
                     policy=lambda r: replace(r, policy=replace(r.policy, generation=1)))
        self.rejects("stale_namespace_policy", path=KEEPALIVE,
                     policy=lambda r: replace(r, policy=replace(r.policy, enforced_at=9)))
        self.rejects("stale_namespace_policy", baseline_for="missing_policy",
                     policy=lambda r: replace(r, policy=replace(r.policy, generation=12, enforced_at=18)),
                     launch=lambda r: replace(r, process=replace(r.process, generation=12),
                                              egress=replace(r.egress, generation=12)))
        self.rejects("listener_not_ready", baseline_for="missing_listener",
                     launch=lambda r: replace(r, listener=replace(r.listener, ready_at=119)))

    def test_rejects_unenforced_policy_and_broad_or_cross_reservation_rules(self):
        for fields, reason in (
            ({"default_deny": False}, "policy_not_enforced"),
            ({"enforced_at": 10}, "policy_not_enforced"),
            ({"namespace": "other"}, "policy_not_enforced"),
            ({"allowlist": (FIRST.postgres, RETAINED.postgres)}, "wrong_allowlist"),
            ({"allowlist": (replace(RETAINED.postgres, protocol="udp"),)}, "wrong_allowlist"),
            ({"allowlist": (replace(RETAINED.postgres, address="0.0.0.0/0"),)}, "wrong_allowlist"),
        ):
            self.rejects(reason, policy=lambda r: replace(r, policy=replace(r.policy, **fields)))
        self.rejects("wrong_allowlist", policy=lambda r: replace(
            r, policy=replace(r.policy, allowlist=(OTHER_POSTGRES[r.attempt.target],))))

    def test_requires_all_denials_zero_public_traffic_and_exact_postgres(self):
        for fields, reason in (
            *[({"denied": DENIED - {vector}}, "denial_unconfirmed") for vector in sorted(DENIED)],
            ({"generation": 99}, "denial_unconfirmed"),
            ({"namespace": "other"}, "denial_unconfirmed"),
        ):
            self.rejects(reason, launch=lambda r: replace(r, egress=replace(r.egress, **fields)))
        self.rejects("postgres_unconfirmed", launch=lambda r: replace(
            r, egress=replace(r.egress, postgres_allowed=OTHER_POSTGRES[r.attempt.target])))
        self.rejects("public_traffic_unconfirmed", launch=lambda r: replace(r, public_discord_packets=1))

    def test_refusal_must_affirm_zero_starts_on_each_path(self):
        for path in (COLD, KEEPALIVE):
            for fault in FAULTS:
                if path == KEEPALIVE:
                    self.rejects("keepalive_trigger_unconfirmed", path=path, fault=fault,
                                 launch=lambda r: replace(r, forced_from=1))
                for fields in ({"process_starts": 1}, {"refusal": "other"},
                               {"process": object()}, {"egress": object()}):
                    self.rejects("refusal_unconfirmed", path=path, fault=fault,
                                 launch=lambda r: replace(r, **fields))
            self.rejects("policy_not_missing", path=path, fault="missing_policy",
                         policy=lambda r: replace(r, policy=Policy("net:22", 1, 1, True, (RETAINED.postgres,))))
            self.rejects("listener_not_missing", path=path, fault="missing_listener",
                         launch=lambda r: replace(r, listener=Listener(r.namespace, "127.0.0.1:8081", r.event - 1)))

    def test_rejects_replayed_observations_and_redacts_adapter_failures(self):
        self.rejects("policy_unconfirmed", policy=lambda r: replace(r, attempt=replace(r.attempt)))
        fields = self.trace.fields
        for read, reason in (
            (lambda a: fields(replace(a, baseline_for=None)), "stale_launch_event"),
            (lambda a: fields(a, namespace="net:shared"), "shared_namespace"),
        ):
            with (patch.object(self.trace, "fields", side_effect=read),
                  self.assertRaisesRegex(TargetError, f"^pre_egress_{reason}$")):
                self.verify()
        with patch.object(self.trace, "probe", side_effect=RuntimeError("private-probe-detail")):
            with self.assertRaisesRegex(TargetError, "^pre_egress_launch_unconfirmed$") as failure:
                self.verify()
        self.assertIsNone(failure.exception.__context__)
