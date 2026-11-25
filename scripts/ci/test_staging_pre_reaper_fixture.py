"""Exercise the dormant sweep with disposable in-memory PRE resources."""

from dataclasses import replace
from pathlib import Path
import sys
import unittest
from unittest.mock import call, patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_sweep import SweepError, sweep_target
    from pre_target import Inventory, Resource, Target


READS = ("status", "inventory")
OPERATIONS = ("revoke_write_access", "delete_worker", "stop_container", "delete_container")


class SyntheticStaging:
    def __init__(self, target):
        self.snapshot = Inventory(
            target, Resource(target.worker_name, target.worker_name),
            Resource(target.container_name, "12345678-1234-4234-8234-123456789abc"),
            instance_count=1, assigned_count=1, write_access=True, complete=True, startup_settled=True,
        )
        self.status = "terminal"
        self.unconfirmed = None
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
        changes = {
            "revoke_write_access": {"write_access": False},
            "delete_worker": {"worker": None},
            "delete_container": {"container": None},
        }
        self.snapshot = replace(self.snapshot, **changes[action.operation])
        return True

    def stop_container(self, target, action):
        self.events.append(("stop_container", target, action.name, action.id))
        self.snapshot = replace(self.snapshot, instance_count=0, assigned_count=0)
        return self.snapshot


class PreReaperFixtureTests(unittest.TestCase):
    def setUp(self):
        self.target = Target("pre-fixture-first", 42, 1_800_000_000, "0473/0465")

    def sweep(self, staging, *, now=None):
        return sweep_target(self.target, staging, now=self.target.started_at + 1 if now is None else now)

    def refused(self, staging, reason, *, operations=(), now=None):
        with self.assertRaisesRegex(SweepError, reason):
            self.sweep(staging, now=now)
        self.assertEqual(tuple(event[0] for event in staging.events), READS + operations)

    def test_eligible_pair_revokes_then_deletes_then_reads_back(self):
        for shape, status in (("0473/0465", "terminal"), ("0475/0465", "active")):
            with self.subTest(shape=shape):
                target = self.target = replace(self.target, shape=shape)
                staging = SyntheticStaging(target)
                staging.status = status
                app = staging.snapshot.container
                now = target.expires_at if status == "active" else target.started_at + 1
                self.assertEqual(self.sweep(staging, now=now), "cleared")
                self.assertEqual(staging.events, [
                    ("status", target), ("inventory", target),
                    ("revoke_write_access", target, target.service_name, target.service_name),
                    ("delete_worker", target, target.worker_name, target.worker_name),
                    ("stop_container", target, app.name, app.id),
                    ("delete_container", target, app.name, app.id),
                    ("inventory", target),
                ])

    def test_active_run_is_spared_and_unknown_status_fails_closed(self):
        staging = SyntheticStaging(self.target)
        staging.status = "active"
        self.assertEqual(self.sweep(staging), "spared")
        self.assertEqual(tuple(event[0] for event in staging.events), READS)
        staging.status = "unknown"
        staging.events.clear()
        self.refused(staging, "^PRE cleanup authorization could not be verified$", now=self.target.expires_at)

    def test_unrelated_names_and_reservations_never_reach_the_adapter(self):
        foreign = replace(self.target, run_id=43)
        for mismatch in (
            {"target": foreign},
            {"worker": Resource("two-bot-next-staging", "two-bot-next-staging")},
            {"container": Resource(foreign.container_name, "12345678-1234-4234-8234-123456789def")},
            {"complete": False},
        ):
            with self.subTest(mismatch=mismatch):
                staging = SyntheticStaging(self.target)
                staging.snapshot = replace(staging.snapshot, **mismatch)
                self.refused(staging, "^PRE cleanup authorization could not be verified$")

    def test_unconfirmed_revocation_or_delete_stops_the_sweep(self):
        for index, operation in enumerate(OPERATIONS):
            if operation == "stop_container":
                continue
            with self.subTest(operation=operation):
                staging = SyntheticStaging(self.target)
                staging.unconfirmed = operation
                self.refused(staging, "^PRE cleanup operation was not confirmed$",
                             operations=OPERATIONS[:index + 1])

    def test_container_delete_requires_direct_zero_for_the_pinned_app(self):
        for changes in (
            {"startup_settled": False}, {"write_access": True}, {"container": None},
            {"worker": Resource(self.target.worker_name, self.target.worker_name)},
            {"container": Resource(self.target.container_name, "12345678-1234-4234-8234-123456789def")},
            {"instance_count": 1}, {"assigned_count": 1}, {"assigned_count": None},
        ):
            with self.subTest(changes=changes):
                staging = SyntheticStaging(self.target)
                stop = staging.stop_container
                with patch.object(staging, "stop_container", side_effect=lambda *args: replace(stop(*args), **changes)):
                    self.refused(staging, "^PRE container stop could not be verified$", operations=OPERATIONS[:-1])

    def test_readback_requires_fresh_complete_absence_zero_and_settlement(self):
        initial = SyntheticStaging(self.target).snapshot
        for changes in (
            {"worker": initial.worker}, {"container": initial.container},
            {"instance_count": 1}, {"assigned_count": 1}, {"assigned_count": None},
            {"write_access": True}, {"complete": False}, {"startup_settled": False},
        ):
            with self.subTest(changes=changes):
                staging = SyntheticStaging(self.target)
                cleared = replace(initial, worker=None, container=None, instance_count=0,
                                  assigned_count=0, write_access=False)
                with patch.object(staging, "inventory", side_effect=[initial, replace(cleared, **changes)]) as inventory:
                    with self.assertRaisesRegex(SweepError, "^PRE cleanup readback could not be verified$"):
                        self.sweep(staging)
                self.assertEqual(inventory.call_args_list, [call(self.target), call(self.target)])

    def test_late_start_requires_another_sweep_before_success(self):
        staging = SyntheticStaging(self.target)
        arrived = staging.snapshot
        empty = replace(arrived, worker=None, container=None, instance_count=0, assigned_count=0, write_access=False)
        unsettled = replace(empty, startup_settled=False)
        for before, after in ((unsettled, unsettled), (empty, arrived)):
            staging.events.clear()
            with patch.object(staging, "inventory", side_effect=[before, after]) as inventory, self.subTest():
                with self.assertRaisesRegex(SweepError, "^PRE cleanup readback could not be verified$"):
                    self.sweep(staging)
                self.assertEqual(inventory.call_args_list, [call(self.target), call(self.target)])
                self.assertEqual(staging.events, [("status", self.target)])
        self.assertEqual(self.sweep(staging), "cleared")

    def test_adapter_errors_do_not_expose_raw_details(self):
        for method, reason in (
            ("run_status", "cleanup authorization could not be verified"),
            ("apply", "cleanup operation was not confirmed"),
            ("stop_container", "container stop could not be verified"),
            ("inventory", "cleanup readback could not be verified"),
        ):
            with self.subTest(method=method):
                staging = SyntheticStaging(self.target)
                unavailable = OSError("unredacted synthetic provider details")
                effect = [staging.snapshot, unavailable] if method == "inventory" else unavailable
                with patch.object(staging, method, side_effect=effect):
                    with self.assertRaisesRegex(SweepError, f"^PRE {reason}$") as failure:
                        self.sweep(staging)
                self.assertTrue(failure.exception.__suppress_context__)
