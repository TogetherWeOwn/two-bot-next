"""Offline acceptance and refusal tests for native PRE launch and stop."""

from dataclasses import replace
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock, call, patch
from uuid import UUID

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_native_rehearsal import LaunchPolicy, RuntimeIdentity, StopReadback, rehearse_native
    from pre_start_stop import Deployment, EffectWindow
    from pre_target import Target, TargetError
    from pre_worker_upload import DatabaseEndpoint


SHAPES = ("0473/0465", "0475/0465")
START = 1_800_000_000
DIGEST = "sha256:" + "a" * 64


class RehearsalFixture:
    def __init__(self, shape=SHAPES[0]):
        self.target = Target("pre-fixture-native", 123, START, shape)
        self.window = EffectWindow(self.target, START + 60)
        self.deployment = Deployment(self.target, UUID(int=1), UUID(int=2), UUID(int=3))
        self.database = DatabaseEndpoint(self.target, self.target.service_name, "db.invalid", 5432,
                                         "fixture", "fixture_writer", True)
        self.expected = RuntimeIdentity(self.deployment, DIGEST, self.database)
        self.policy = LaunchPolicy("durable-object", "ws://127.0.0.1:8081/gateway?encoding=json",
                                   "http://[::1]:8082", True, False, self.database)
        self.stopped = StopReadback(self.deployment, 0, 0, True, True)
        self.calls = Mock()
        self.calls.clock.side_effect = [START, START, START + 59]
        self.calls.custody.arm.return_value = True
        self.calls.custody.stop.return_value = True
        self.calls.launch.return_value = replace(self.deployment)
        self.calls.read_runtime.return_value = replace(self.expected)
        self.calls.read_stop.return_value = replace(self.stopped)

    def run(self):
        return rehearse_native(self.window, self.expected, self.policy, clock=self.calls.clock,
                               custody=self.calls.custody, launch=self.calls.launch,
                               read_runtime=self.calls.read_runtime, read_stop=self.calls.read_stop)

    def full_sequence(self):
        return [call.clock(), call.custody.arm(self.window, self.deployment), call.clock(),
                call.launch(self.window, self.deployment, self.policy), call.read_runtime(self.deployment),
                call.custody.stop(self.deployment), call.read_stop(self.deployment), call.clock()]


class NativeRehearsalTests(unittest.TestCase):
    def test_both_layouts_launch_read_and_stop_in_order(self):
        for shape in SHAPES:
            with self.subTest(shape=shape):
                fixture = RehearsalFixture(shape)
                if shape == SHAPES[1]:
                    fixture.policy = replace(fixture.policy, discord_gateway_url="wss://[::1]:8443/gateway",
                                             discord_api_base="https://localhost:8443/api")
                self.assertEqual(fixture.run(), fixture.stopped)
                self.assertEqual(fixture.calls.mock_calls, fixture.full_sequence())

    def test_invalid_launch_policy_never_arms_or_launches(self):
        fixture = RehearsalFixture()
        wrong_target = replace(fixture.target, shape=SHAPES[1])
        invalid = [("scheduling_policy", value, "scheduling_required")
                   for value in (None, "default", "legacy", "auto-start", "keepalive")]
        invalid += [("enable_internet", value, "public_egress_unconfirmed") for value in (True, None, 0)]
        invalid += [("fixtures_ready_in_namespace", value, "discord_fixture_unconfirmed")
                    for value in (False, None, 1)]
        for field, public_url in (("discord_gateway_url", "wss://gateway.discord.gg"),
                                  ("discord_api_base", "https://discord.com/api")):
            scheme = public_url.split(":", 1)[0]
            invalid += [(field, value, "discord_fixture_unconfirmed") for value in (
                None, "", public_url, "file://localhost", f"{scheme}://127.0.0.2",
                f"{scheme}://localhost.", f"{scheme}://2130706433", f"{scheme}://[::ffff:127.0.0.1]",
                f"{scheme}://localhost.evil.invalid", f"{scheme}://localhost@evil.invalid",
                f"{scheme}://evil.invalid@localhost", f"{scheme}://localhost\\@evil.invalid",
                f"{scheme}://localhost:0", f"{scheme}://localhost:65536", f"{scheme}://localhost:",
                f"{scheme}://[::1", f"\n{scheme}://localhost", f"{scheme}://local\thost",
                f"{scheme}://localhｏst", f"{scheme}://%6cocalhost", f"{scheme}://localhost/#fragment",
                f"{scheme}://localhost/" + "a" * 2048,
            )]
        invalid += [("database", value, "database_mismatch") for value in (
            None, replace(fixture.database, target=wrong_target), replace(fixture.database, service="other"),
            replace(fixture.database, disposable=False), replace(fixture.database, disposable=1),
            replace(fixture.database, user="other"), replace(fixture.database, database="other"),
            replace(fixture.database, host="other.invalid"), replace(fixture.database, port=5433),
        )]
        for field, value, error in invalid:
            with self.subTest(field=field, value=value):
                trial = RehearsalFixture()
                trial.policy = replace(trial.policy, **{field: value})
                with self.assertRaisesRegex(TargetError, f"^pre_native_{error}$"):
                    trial.run()
                self.assertEqual(trial.calls.mock_calls, [])

    def test_invalid_pins_and_window_refuse_before_custody(self):
        fixture = RehearsalFixture()
        expected = fixture.expected
        for pins in (None, replace(expected, image_digest="latest"), replace(expected, database_writer=None),
                     replace(expected, database_writer=replace(fixture.database, port=True)),
                     replace(expected, database_writer=replace(fixture.database, host="user@db.invalid")),
                     replace(expected, database_writer=replace(fixture.database, user="")),
                     replace(expected, deployment=replace(fixture.deployment, worker_version=UUID(int=0))),
                     replace(expected, deployment=replace(fixture.deployment, start_id="not-a-uuid"))):
            with self.subTest(pins=pins):
                fixture.expected = pins
                with self.assertRaisesRegex(TargetError, "^invalid_pre_native_pins$"):
                    fixture.run()
        fixture.expected = expected
        fixture.window = None
        with self.assertRaisesRegex(TargetError, "^invalid_pre_native_window$"):
            fixture.run()
        self.assertEqual(fixture.calls.mock_calls, [])

    def test_unconfirmed_arm_never_launches_or_stops_another_claim(self):
        for reply in (None, False, 1, OSError("private arm reply")):
            with self.subTest(reply=reply):
                fixture = RehearsalFixture()
                fixture.calls.custody.arm.side_effect = [reply]
                with self.assertRaisesRegex(TargetError, "^pre_native_stop_unarmed$"):
                    fixture.run()
                self.assertEqual(fixture.calls.mock_calls, fixture.full_sequence()[:2])

    def test_lost_arm_acknowledgment_keeps_custody_and_denies_replay(self):
        fixture = RehearsalFixture()
        claims = []

        def arm(window, deployment):
            if claims:
                return False
            claims.append((window, deployment))
            raise OSError("private arm reply")

        fixture.calls.custody.arm.side_effect = arm
        fixture.calls.clock.side_effect = [START, START]
        for _ in range(2):
            with self.assertRaisesRegex(TargetError, "^pre_native_stop_unarmed$"):
                fixture.run()
        self.assertEqual(claims, [(fixture.window, fixture.deployment)])
        self.assertEqual(fixture.calls.mock_calls, fixture.full_sequence()[:2] * 2)

    def test_lost_or_mismatched_launch_reply_still_stops_exact_pin(self):
        for reply in (None, OSError("private start reply"), UUID(int=4)):
            with self.subTest(reply=reply):
                fixture = RehearsalFixture()
                if type(reply) is UUID:
                    reply = replace(fixture.deployment, start_id=reply)
                fixture.calls.launch.side_effect = [reply]
                with self.assertRaisesRegex(TargetError, "^pre_native_launch_unconfirmed$"):
                    fixture.run()
                sequence = fixture.full_sequence()
                self.assertEqual(fixture.calls.mock_calls, sequence[:4] + sequence[5:])

    def test_runtime_requires_exact_independently_observed_writer_and_build(self):
        fixture = RehearsalFixture(SHAPES[1])
        expected = fixture.expected
        inconsistent = [None, OSError("private runtime reply"), replace(expected, image_digest="sha256:" + "b" * 64)]
        for field, value in (("start_id", UUID(int=4)), ("worker_version", UUID(int=4)),
                             ("container_id", UUID(int=4)), ("target", replace(fixture.target, shape=SHAPES[0]))):
            inconsistent.append(replace(expected, deployment=replace(fixture.deployment, **{field: value})))
        for field, value in (("host", "wrong.invalid"), ("port", 5433), ("database", "other"),
                             ("user", "other"), ("disposable", False)):
            inconsistent.append(replace(expected, database_writer=replace(fixture.database, **{field: value})))
        for reply in inconsistent:
            with self.subTest(reply=reply):
                trial = RehearsalFixture(SHAPES[1])
                trial.calls.read_runtime.side_effect = [reply]
                with self.assertRaisesRegex(TargetError, "^pre_native_runtime_unconfirmed$"):
                    trial.run()
                self.assertEqual(trial.calls.mock_calls, trial.full_sequence())

    def test_stop_command_and_observation_are_both_required(self):
        for reply in (None, False, 1, OSError("private stop reply")):
            with self.subTest(reply=reply):
                fixture = RehearsalFixture()
                fixture.calls.custody.stop.side_effect = [reply]
                with self.assertRaisesRegex(TargetError, "^pre_native_stop_unconfirmed$"):
                    fixture.run()
                self.assertEqual(fixture.calls.mock_calls, fixture.full_sequence()[:6])

    def test_stop_readback_requires_exact_identity_zero_counts_drain_and_fence(self):
        fixture = RehearsalFixture()
        failures = [None, OSError("private drain reply")]
        for field, value in (("deployment", replace(fixture.deployment, container_id=UUID(int=9))),
                             ("active_instances", 1), ("active_instances", False), ("active_instances", None),
                             ("assigned_instances", 1), ("assigned_instances", False), ("assigned_instances", -1),
                             ("startup_drained", False), ("startup_drained", 1), ("restart_fenced", False),
                             ("restart_fenced", 1)):
            failures.append(replace(fixture.stopped, **{field: value}))
        for reply in failures:
            with self.subTest(reply=reply):
                trial = RehearsalFixture()
                trial.calls.read_stop.side_effect = [reply]
                with self.assertRaisesRegex(TargetError, "^pre_native_stop_unconfirmed$"):
                    trial.run()
                self.assertEqual(trial.calls.mock_calls, trial.full_sequence()[:7])

    def test_cutoff_clock_failure_and_regression_preserve_armed_stop(self):
        cases = [([START - 1], "invalid_pre_clock", 1),
                 ([True], "invalid_pre_clock", 1),
                 ([OSError("private authority reply")], "pre_native_clock_unconfirmed", 1),
                 ([START + 60], "pre_effect_window_expired", 1),
                 ([START, START + 60, START + 61], "pre_effect_window_expired", 6),
                 ([START + 1, START, START + 2], "pre_native_clock_regressed", 6),
                 ([START, START, START + 60], "pre_effect_window_expired", 8),
                 ([START, START + 1, START], "pre_native_clock_regressed", 8),
                 ([START, START, OSError("private authority reply")], "pre_native_clock_unconfirmed", 8)]
        for readings, error, count in cases:
            with self.subTest(readings=readings, error=error):
                fixture = RehearsalFixture()
                fixture.calls.clock.side_effect = readings
                with self.assertRaisesRegex(TargetError, f"^{error}$"):
                    fixture.run()
                sequence = fixture.full_sequence()
                self.assertEqual(fixture.calls.mock_calls, sequence[:3] + sequence[5:] if count == 6
                                 else sequence[:count])


if __name__ == "__main__":
    unittest.main()
