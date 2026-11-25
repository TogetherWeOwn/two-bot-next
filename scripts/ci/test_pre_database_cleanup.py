"""Rehearse PRE cleanup with an injected disposable database readback."""

from dataclasses import dataclass, replace
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock, call, patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from ci.pre_operator_acceptance_fixture import PRE_STATES, SchemaState
    from ci.test_staging_pre_reaper_fixture import SyntheticStaging
    from pre_sweep import SweepError, sweep_target
    from pre_target import Target


@dataclass(frozen=True)
class DatabaseReadback:
    schema: SchemaState
    effective_writer_roles: tuple[str, ...] = ()
    delegated_writers: tuple[str, ...] = ()


class PreDatabaseCleanupTests(unittest.TestCase):
    def test_terminal_readback_requires_no_effective_database_writer(self):
        for shape, schema in PRE_STATES.items():
            target = Target("pre-fixture-database", 42, 1_800_000_000, shape)
            sibling = replace(target, run_id=43)
            clean = DatabaseReadback(schema)
            cases = (
                ("no writer", clean, False),
                ("sibling role", replace(clean, effective_writer_roles=(sibling.service_name,)), True),
                ("delegated connection", replace(clean, delegated_writers=("fixture-session",)), True),
            )
            for case, readback, residual_writer in cases:
                with self.subTest(shape=shape, case=case):
                    self.check_terminal_readback(target, sibling, readback, residual_writer)

    def check_terminal_readback(self, target, sibling, readback, residual_writer):
        staging = SyntheticStaging(target)
        inventory = staging.inventory
        app = staging.snapshot.container
        # An unrelated writer on the sibling database is allowed in every case.
        sibling_readback = DatabaseReadback(readback.schema, (sibling.service_name,))
        databases = {target: readback, sibling: sibling_readback}
        read_database = Mock(side_effect=databases.__getitem__)

        def observe(requested):
            snapshot = inventory(requested)
            if snapshot.container is not None:
                return snapshot
            # The independent terminal probe can observe access after the stop and
            # scoped revocation have both confirmed. The admitted role is gone.
            database = read_database(requested)
            return replace(snapshot, write_access=bool(database.effective_writer_roles or database.delegated_writers))

        status_and_read = [("status", target), ("inventory", target)]
        revoke = ("revoke_write_access", target, target.service_name, target.service_name)
        cleanup = [revoke, ("delete_worker", target, target.worker_name, target.worker_name),
                   ("stop_container", target, app.name, app.id), ("delete_container", target, app.name, app.id)]
        retry = [revoke] if residual_writer else []
        with patch.object(staging, "inventory", side_effect=observe):
            for attempt, effects in enumerate((cleanup, retry)):
                staging.events.clear()
                if residual_writer:
                    with self.assertRaisesRegex(SweepError, "^PRE cleanup readback could not be verified$"):
                        sweep_target(target, staging, now=target.expires_at + attempt)
                else:
                    self.assertEqual(sweep_target(target, staging, now=target.expires_at + attempt), "cleared")
                self.assertEqual(staging.events, status_and_read + effects + [("inventory", target)])

        self.assertFalse(staging.snapshot.write_access)
        self.assertNotIn(target.service_name, readback.effective_writer_roles)
        self.assertEqual(read_database.call_args_list, [call(target), call(target), call(target)])
        self.assertEqual(databases, {target: readback, sibling: sibling_readback})
