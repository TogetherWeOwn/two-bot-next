"""Exercise ledger projections using the producer and readback row shapes."""

from copy import deepcopy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from ci.production_migrate_claim import projection_hash, store_projection_hash
from production_release_ledgers import ledger_endpoints
from staging_rollout import GateError


def ledger_row(version, description):
    return {"version": version, "description": description, "sha384": f"{version:096x}", "success": True}


def source_row(version):
    checksum = f"{version:096x}"
    return {"version": version, "description": f"migration {version}", "sha384": checksum,
            "sha384_recomputed": checksum}


class ProductionReleaseLedgerTests(unittest.TestCase):
    def setUp(self):
        self.cutover = ledger_row(474, "original cutover description")
        self.store = ledger_row(477, "original store description")
        self.plan = {"source_sha": "a" * 40, "pending_before": [475, 479, 480, 481, 482],
                     "pending_store_before": [478],
                     "source_migrations": [source_row(v) for v in (474, 475, 479, 480, 481, 482)],
                     "source_store_migrations": [source_row(v) for v in (477, 478)],
                     "ledger_before": [self.cutover], "ledger_after": [deepcopy(self.cutover)],
                     "ledger_store_before": [self.store], "ledger_store_after": [deepcopy(self.store)]}
        self.plan["plan_manifest_sha256"] = projection_hash(self.plan)
        self.plan["store_plan_manifest_sha256"] = store_projection_hash(self.plan)

    def test_manifest_projects_to_the_rust_inverse_readback_shape(self):
        attendance = ledger_row(475, "migration 475")
        expected_cutover = [self.cutover, attendance, *(ledger_row(v, f"migration {v}") for v in range(479, 483))]
        expected_store = [self.store, ledger_row(478, "migration 478")]
        self.assertEqual(ledger_endpoints(self.plan), (
            [[self.cutover], [self.store]], [expected_cutover, expected_store],
            [[self.cutover, attendance], [self.store]],
        ))

    def test_refuses_source_checksum_or_installed_ledger_drift(self):
        for field, change in (
            ("source_migrations", {"sha384_recomputed": "1" * 96}),
            ("ledger_store_before", {"sha384": "1" * 96}),
            ("ledger_store_before", {"success": False}),
            ("ledger_after", {"description": "changed during plan"}),
        ):
            with self.subTest(field=field, change=change), self.assertRaises(GateError):
                plan = deepcopy(self.plan)
                plan[field][0].update(change)
                ledger_endpoints(plan)
