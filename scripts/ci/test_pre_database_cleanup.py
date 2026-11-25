"""Rehearse PRE cleanup with an injected disposable database readback."""

from dataclasses import replace
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from ci.pre_operator_acceptance_fixture import PRE_STATES, SchemaState
    from ci.test_staging_pre_reaper_fixture import SyntheticStaging
    from pre_sweep import SweepError, sweep_target
    from pre_target import Target


class PreDatabaseCleanupTests(unittest.TestCase):
    def test_terminal_readback_requires_no_effective_database_writer(self):
        catalogs = {
            "0473/0465": SchemaState((115, 473), (20, 465), False, None),
            "0475/0465": SchemaState((116, 475), (20, 465), True, 1_800_000_001),
        }
        for shape, catalog in catalogs.items():
            target = Target("pre-fixture-database", 42, 1_800_000_000, shape)
            sibling = replace(target, run_id=43)
            cases = (
                ("no writer", set(), False),
                ("sibling role", {(target, sibling.service_name)}, True),
                ("delegated connection", {(target, "fixture-delegated-connection")}, True),
            )
            for case, terminal_writers, residual_writer in cases:
                with self.subTest(shape=shape, case=case):
                    self.check_terminal_readback(target, sibling, catalog, terminal_writers, residual_writer)

    def check_terminal_readback(self, target, sibling, catalog, terminal_writers, residual_writer):
        staging = SyntheticStaging(target)
        inventory = staging.inventory
        apply = staging.apply
        app = staging.snapshot.container
        catalog_by_database = {target: catalog, sibling: catalog}
        database_reads = []
        admitted_grants = {(target, target.service_name), (sibling, sibling.service_name)}

        def apply_scoped(requested, action):
            confirmed = apply(requested, action)
            if confirmed and action.operation == "revoke_write_access":
                admitted_grants.discard((requested, action.id))
            return confirmed

        def observe(requested):
            snapshot = inventory(requested)
            if snapshot.container is not None:
                return snapshot
            # The terminal probe combines remaining grants with independent
            # observations of effective database writers after teardown.
            database_reads.append((requested, catalog_by_database[requested]))
            writers = admitted_grants | terminal_writers
            return replace(snapshot, write_access=any(database == requested for database, _ in writers))

        status_and_read = [("status", target), ("inventory", target)]
        revoke = ("revoke_write_access", target, target.service_name, target.service_name)
        cleanup = [revoke, ("delete_worker", target, target.worker_name, target.worker_name),
                   ("stop_container", target, app.name, app.id), ("delete_container", target, app.name, app.id)]
        retry = [revoke] if residual_writer else []
        with patch.object(staging, "inventory", side_effect=observe), patch.object(staging, "apply", side_effect=apply_scoped):
            for attempt, effects in enumerate((cleanup, retry)):
                staging.events.clear()
                if residual_writer:
                    with self.assertRaisesRegex(SweepError, "^PRE cleanup readback could not be verified$"):
                        sweep_target(target, staging, now=target.expires_at + attempt)
                else:
                    self.assertEqual(sweep_target(target, staging, now=target.expires_at + attempt), "cleared")
                self.assertEqual(staging.events, status_and_read + effects + [("inventory", target)])

        self.assertEqual(admitted_grants, {(sibling, sibling.service_name)})
        self.assertEqual(database_reads, [(target, PRE_STATES[target.shape])] * 3)
