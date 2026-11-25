"""Exercise the dormant sweep with disposable in-memory PRE resources."""

from dataclasses import replace
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_sweep import SweepError, sweep_target
    from pre_target import Inventory, Resource, Target


class SyntheticStaging:
    def __init__(self, target):
        self.snapshot = Inventory(
            target=target,
            worker=Resource(target.worker_name, target.worker_name),
            container=Resource(target.container_name, "12345678-1234-4234-8234-123456789abc"),
            instance_count=1,
            assigned_count=1,
            write_access=True,
            complete=True,
            startup_settled=True,
        )
        self.status = "terminal"
        self.unconfirmed = None
        self.stop_overrides = {}
        self.events = []

    def run_status(self, target):
        self.events.append(("status", target))
        return self.status

    def inventory(self, target):
        self.events.append(("inventory", target))
        return self.snapshot

    def apply(self, target, action):
        self.events.append((action.operation, target, action.name, action.id))
        if action.operation == self.unconfirmed:
            return False
        if action.operation == "revoke_write_access":
            self.snapshot = replace(self.snapshot, write_access=False)
        elif action.operation == "delete_worker":
            self.snapshot = replace(self.snapshot, worker=None)
        elif action.operation == "delete_container":
            self.snapshot = replace(self.snapshot, container=None)
        else:
            raise AssertionError("Unexpected cleanup operation")
        return True

    def stop_container(self, target, action):
        self.events.append(("stop_container", target, action.name, action.id))
        stopped_counts = {"instance_count": 0, "assigned_count": 0}
        stopped_counts.update(self.stop_overrides)
        self.snapshot = replace(self.snapshot, **stopped_counts)
        return self.snapshot


class PreReaperFixtureTests(unittest.TestCase):
    def setUp(self):
        self.target = Target("pre-fixture-first", 42, 1_800_000_000, "0473/0465")
        self.staging = SyntheticStaging(self.target)

    def sweep(self, *, now=None):
        if now is None:
            now = self.target.started_at + 1
        return sweep_target(self.target, self.staging, now=now)

    def test_eligible_pair_revokes_then_deletes_then_reads_back(self):
        for shape, status, at_expiry in (
            ("0473/0465", "terminal", False), ("0475/0465", "active", True),
        ):
            with self.subTest(shape=shape):
                self.target = replace(self.target, shape=shape)
                self.staging = SyntheticStaging(self.target)
                self.staging.status = status
                snapshot = self.staging.snapshot
                now = self.target.expires_at if at_expiry else self.target.started_at + 1
                self.assertEqual(self.sweep(now=now), "cleared")
                self.assertEqual(self.staging.events, [
                    ("status", self.target),
                    ("inventory", self.target),
                    ("revoke_write_access", self.target, self.target.service_name, self.target.service_name),
                    ("delete_worker", self.target, snapshot.worker.name, snapshot.worker.id),
                    ("stop_container", self.target, snapshot.container.name, snapshot.container.id),
                    ("delete_container", self.target, snapshot.container.name, snapshot.container.id),
                    ("inventory", self.target),
                ])

    def test_active_run_is_spared_and_unknown_status_fails_closed(self):
        self.staging.status = "active"
        self.assertEqual(self.sweep(), "spared")
        self.assertEqual(self.staging.events, [("status", self.target), ("inventory", self.target)])

        self.staging.status = "unknown"
        self.staging.events.clear()
        with self.assertRaisesRegex(SweepError, "^PRE cleanup authorization could not be verified$"):
            self.sweep(now=self.target.expires_at)
        self.assertEqual(self.staging.events, [("status", self.target), ("inventory", self.target)])

    def test_unrelated_names_and_reservations_never_reach_the_adapter(self):
        foreign = replace(self.target, run_id=self.target.run_id + 1)
        for mismatch in (
            {"target": foreign},
            {"worker": Resource("two-bot-next-staging", "two-bot-next-staging")},
            {"container": Resource(foreign.container_name, self.staging.snapshot.container.id)},
            {"complete": False},
        ):
            with self.subTest(mismatch=mismatch):
                self.staging = SyntheticStaging(self.target)
                self.staging.snapshot = replace(self.staging.snapshot, **mismatch)
                with self.assertRaisesRegex(SweepError, "^PRE cleanup authorization could not be verified$"):
                    self.sweep()
                self.assertEqual(self.staging.events, [("status", self.target), ("inventory", self.target)])

    def test_unconfirmed_revocation_or_delete_stops_the_sweep(self):
        operations = ("revoke_write_access", "delete_worker", "delete_container")
        for index, operation in enumerate(operations):
            with self.subTest(operation=operation):
                self.staging = SyntheticStaging(self.target)
                self.staging.unconfirmed = operation
                with self.assertRaisesRegex(SweepError, "^PRE cleanup operation was not confirmed$"):
                    self.sweep()
                expected = list(operations[:index + 1])
                if operation == "delete_container":
                    expected.insert(-1, "stop_container")
                self.assertEqual(
                    [event[0] for event in self.staging.events],
                    ["status", "inventory", *expected],
                )

    def test_container_delete_requires_direct_zero_for_the_pinned_app(self):
        for bad_stop in (
            {"startup_settled": False},
            {"write_access": True},
            {"worker": self.staging.snapshot.worker},
            {"container": None},
            {"container": Resource(self.target.container_name, "12345678-1234-4234-8234-123456789def")},
            {"instance_count": 1},
            {"assigned_count": 1},
            {"assigned_count": None},
        ):
            with self.subTest(bad_stop=bad_stop):
                self.staging = SyntheticStaging(self.target)
                self.staging.stop_overrides = bad_stop
                with self.assertRaisesRegex(SweepError, "^PRE container stop could not be verified$"):
                    self.sweep()
                self.assertEqual([event[0] for event in self.staging.events], [
                    "status", "inventory", "revoke_write_access", "delete_worker", "stop_container",
                ])

    def test_readback_requires_fresh_complete_absence_zero_and_settlement(self):
        for leftover in (
            {"worker": self.staging.snapshot.worker},
            {"container": self.staging.snapshot.container},
            {"instance_count": 1},
            {"assigned_count": 1},
            {"assigned_count": None},
            {"write_access": True},
            {"complete": False},
            {"startup_settled": False},
        ):
            with self.subTest(leftover=leftover):
                self.staging = SyntheticStaging(self.target)
                inventory = self.staging.inventory

                def read(target):
                    snapshot = inventory(target)
                    return replace(snapshot, **leftover) if snapshot.container is None else snapshot

                self.staging.inventory = read
                with self.assertRaisesRegex(SweepError, "^PRE cleanup readback could not be verified$"):
                    self.sweep()

    def test_late_start_requires_another_sweep_before_success(self):
        arrived = self.staging.snapshot
        self.staging.snapshot = replace(
            self.staging.snapshot, worker=None, container=None, instance_count=0,
            assigned_count=0, write_access=False, startup_settled=False,
        )
        inventory = self.staging.inventory

        def read_then_land(target):
            observed = inventory(target)
            self.staging.snapshot = arrived
            return observed

        with patch.object(self.staging, "inventory", side_effect=read_then_land):
            with self.assertRaisesRegex(SweepError, "^PRE cleanup readback could not be verified$"):
                self.sweep()
        self.assertEqual([event[0] for event in self.staging.events], ["status", "inventory", "inventory"])
        self.assertEqual(self.staging.snapshot, arrived)
        self.assertEqual(self.sweep(), "cleared")

    def test_adapter_errors_do_not_expose_raw_details(self):
        def unavailable(*_args):
            raise OSError("unredacted synthetic provider details")

        for method, message in (
            ("run_status", "authorization could not be verified"),
            ("apply", "operation was not confirmed"),
            ("stop_container", "stop could not be verified"),
            ("inventory", "readback could not be verified"),
        ):
            with self.subTest(method=method):
                self.staging = SyntheticStaging(self.target)
                if method == "inventory":
                    original = self.staging.inventory

                    def after_delete(target):
                        if self.staging.snapshot.container is None:
                            unavailable()
                        return original(target)

                    self.staging.inventory = after_delete
                else:
                    setattr(self.staging, method, unavailable)
                with self.assertRaisesRegex(SweepError, f"^PRE (cleanup|container) {message}$") as failure:
                    self.sweep()
                self.assertTrue(failure.exception.__suppress_context__)


if __name__ == "__main__":
    unittest.main()
