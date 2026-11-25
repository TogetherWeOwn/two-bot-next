"""Rehearse an expiry retry against shared synthetic provider listings."""

from dataclasses import replace
from pathlib import Path
import sys
import unittest
from unittest.mock import DEFAULT, call, patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_inventory import normalize_inventory
    from pre_sweep import SweepError, sweep_target
    from pre_target import Action, Inventory, Target


EXPIRED = Target("pre-fixture-retry", 42, 1_800_000_000, "0473/0465")
SIBLING = replace(EXPIRED, run_id=43, started_at=EXPIRED.started_at + 60, shape="0475/0465")
APP_ID = "12345678-1234-4234-8234-123456789abc"
SIBLING_APP_ID = "87654321-4321-4321-8321-cba987654321"


def listing(rows):
    return {"rows": rows, "total_count": len(rows), "complete": True}


class FakeProvider:
    def __init__(self, *, lose_worker_delete_reply=True):
        self.lose_worker_delete_reply = lose_worker_delete_reply
        self.app_ids = {EXPIRED: APP_ID, SIBLING: SIBLING_APP_ID}
        self.workers = {target.worker_name for target in self.app_ids}
        self.writers = {target.service_name for target in self.app_ids}
        self.apps = {
            app_id: {"id": app_id, "name": target.container_name,
                     "health": {"instances": {"active": 1, "assigned": 1}}}
            for target, app_id in self.app_ids.items()
        }
        self.stop_proofs = {}
        self.effects = []

    def run_status(self, target):
        return "active"

    def inventory(self, target):
        app_id = self.app_ids[target]
        projection = normalize_inventory(
            target, target, workers=listing([{"id": name} for name in self.workers]),
            applications=listing([{"id": app["id"], "name": app["name"]} for app in self.apps.values()]),
            pinned_app_id=app_id, app_detail=self.apps.get(app_id),
        )
        active, assigned = projection.instance_count, projection.assigned_count
        if projection.container is None:
            active, assigned = self.stop_proofs.get((target, app_id), (active, assigned))
        return Inventory(
            target, projection.worker, projection.container, instance_count=active, assigned_count=assigned,
            write_access=target.service_name in self.writers, complete=projection.complete,
            startup_settled=True,  # The fixture assumes independently closed admission and settled startup.
        )

    def stop_container(self, target, action):
        app = self.apps[action.id]
        self.effects.append(Action("stop_container", action.name, action.id))
        app["health"]["instances"] = {"active": 0, "assigned": 0}
        observed = self.inventory(target)
        self.stop_proofs[(target, action.id)] = (observed.instance_count, observed.assigned_count)
        return observed

    def apply(self, target, action):
        self.effects.append(action)
        if action.operation == "revoke_write_access":
            self.writers.remove(action.id)
        elif action.operation == "delete_worker":
            self.workers.remove(action.id)
            if self.lose_worker_delete_reply:
                # The provider commits the delete, but the caller loses the reply.
                raise OSError("synthetic lost delete reply")
        elif action.operation == "delete_container":
            del self.apps[action.id]
        else:
            raise AssertionError("unexpected cleanup operation")
        return True


class PreSweepInventoryRetryTests(unittest.TestCase):
    def test_expired_sweep_replans_after_lost_worker_delete_reply(self):
        provider = FakeProvider()
        sibling_before = provider.inventory(SIBLING)
        now = EXPIRED.expires_at
        revoke = Action("revoke_write_access", EXPIRED.service_name, EXPIRED.service_name)
        delete_worker = Action("delete_worker", EXPIRED.worker_name, EXPIRED.worker_name)
        stop = Action("stop_container", EXPIRED.container_name, APP_ID)
        delete_app = Action("delete_container", EXPIRED.container_name, APP_ID)

        with self.assertRaisesRegex(SweepError, "^PRE cleanup operation was not confirmed$"):
            sweep_target(EXPIRED, provider, now=now)
        self.assertEqual(provider.effects, [revoke, delete_worker])
        interrupted = provider.inventory(EXPIRED)
        self.assertIsNone(interrupted.worker)
        self.assertFalse(interrupted.write_access)
        self.assertEqual(interrupted.container.id, APP_ID)
        self.assertEqual((interrupted.instance_count, interrupted.assigned_count), (1, 1))

        self.assertEqual(sweep_target(EXPIRED, provider, now=now + 1), "cleared")
        cleanup = [revoke, delete_worker, stop, delete_app]
        self.assertEqual(provider.effects, cleanup)
        self.assertEqual(sweep_target(EXPIRED, provider, now=now + 2), "cleared")
        self.assertEqual(provider.effects, cleanup)
        self.assertEqual(provider.workers, {SIBLING.worker_name})
        self.assertEqual(set(provider.apps), {SIBLING_APP_ID})
        self.assertEqual(provider.writers, {SIBLING.service_name})
        self.assertEqual(provider.inventory(SIBLING), sibling_before)

        provider.stop_proofs.clear()
        with self.assertRaisesRegex(SweepError, "^PRE cleanup readback could not be verified$"):
            sweep_target(EXPIRED, provider, now=now + 3)
        self.assertEqual(provider.effects, cleanup)

    def test_timed_out_cleanup_readback_requires_fresh_absence_proof(self):
        provider = FakeProvider(lose_worker_delete_reply=False)
        sibling_before = provider.inventory(SIBLING)
        now = EXPIRED.expires_at
        cleanup = [
            Action("revoke_write_access", EXPIRED.service_name, EXPIRED.service_name),
            Action("delete_worker", EXPIRED.worker_name, EXPIRED.worker_name),
            Action("stop_container", EXPIRED.container_name, APP_ID),
            Action("delete_container", EXPIRED.container_name, APP_ID),
        ]

        # Planning and direct stop reads succeed. Only final readback times out.
        reads = [DEFAULT, DEFAULT, TimeoutError("synthetic provider read timeout"), DEFAULT, DEFAULT]
        with patch.object(provider, "inventory", wraps=provider.inventory, side_effect=reads) as inventory:
            with self.assertRaisesRegex(SweepError, "^PRE cleanup readback could not be verified$"):
                sweep_target(EXPIRED, provider, now=now)
            self.assertEqual(inventory.call_args_list, [call(EXPIRED)] * 3)
            self.assertEqual(provider.effects, cleanup)
            self.assertEqual(provider.app_ids[EXPIRED], APP_ID)
            self.assertEqual(provider.stop_proofs, {(EXPIRED, APP_ID): (0, 0)})

            # A separate sweep plans and verifies from fresh reads for the same reservation.
            self.assertEqual(sweep_target(EXPIRED, provider, now=now + 1), "cleared")
            self.assertEqual(inventory.call_args_list, [call(EXPIRED)] * 5)

        self.assertEqual(provider.effects, cleanup)
        self.assertEqual(provider.inventory(SIBLING), sibling_before)
