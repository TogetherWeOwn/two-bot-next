"""Synthetic PRE listings and direct app responses for the pure projection."""

from dataclasses import replace
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_inventory import MAX_ROWS, normalize_inventory
    from pre_target import Resource, Target, TargetError


TARGET = Target("pre-fixture-inventory", 42, 1_800_000_000, "0473/0465")
APP_ID = "12345678-1234-4234-8234-123456789abc"
OTHER_ID = "87654321-4321-4321-8321-cba987654321"
WORKER = {"id": TARGET.worker_name}
APP = {"name": TARGET.container_name, "id": APP_ID}


def listing(*rows):
    return {"rows": list(rows), "total_count": len(rows), "complete": True}


def detail(active, assigned):
    return APP | {"health": {"instances": {"active": active, "assigned": assigned}}}


class PreInventoryTests(unittest.TestCase):
    def project(self, **overrides):
        args = {"source_target": TARGET, "workers": listing(), "applications": listing()}
        return normalize_inventory(TARGET, **(args | overrides))

    def test_confirmed_absence_never_supplies_zero_counts(self):
        for pin in (None, APP_ID):
            with self.subTest(pin=pin):
                result = self.project(pinned_app_id=pin)
                self.assertIs(result.complete, True)
                self.assertEqual(result.target, TARGET)
                self.assertIsNone(result.worker)
                self.assertIsNone(result.container)
                self.assertIsNone(result.instance_count)
                self.assertIsNone(result.assigned_count)

    def test_exact_names_and_direct_counts(self):
        neighbor = replace(TARGET, run_id=43)
        workers = listing(WORKER, {"id": neighbor.worker_name}, {"id": TARGET.worker_name + "-shadow"})
        applications = listing(APP, {"name": neighbor.container_name, "id": OTHER_ID})
        for active, assigned in ((0, 0), (0, 1), (1, 0), (1, 2)):
            with self.subTest(active=active, assigned=assigned):
                result = self.project(workers=workers, applications=applications, pinned_app_id=APP_ID,
                                      app_detail=detail(active, assigned))
                self.assertIs(result.complete, True)
                self.assertEqual(result.worker, Resource(TARGET.worker_name, TARGET.worker_name))
                self.assertEqual(result.container, Resource(TARGET.container_name, APP_ID))
                self.assertEqual((result.instance_count, result.assigned_count), (active, assigned))

    def test_complete_listings_can_show_only_one_reserved_resource(self):
        worker_only = self.project(workers=listing(WORKER),
                                   applications=listing(APP | {"name": TARGET.container_name + "-shadow"}))
        self.assertEqual(worker_only.worker, Resource(TARGET.worker_name, TARGET.worker_name))
        self.assertIsNone(worker_only.container)
        app_only = self.project(applications=listing(APP), pinned_app_id=APP_ID, app_detail=detail(0, 0))
        self.assertIsNone(app_only.worker)
        self.assertEqual(app_only.container, Resource(TARGET.container_name, APP_ID))

    def test_partial_truncated_and_unrecognized_listings_fail_closed(self):
        empty = listing()
        for invalid in (None, [], {"rows": []}, empty | {"complete": False}, empty | {"complete": 1},
                        empty | {"total_count": 1}, empty | {"total_count": False}, empty | {"rows": ()},
                        empty | {"next_page_token": "synthetic-cursor"},
                        listing(*({"id": f"test-{i}"} for i in range(MAX_ROWS + 1)))):
            for key in ("workers", "applications"):
                with self.subTest(listing=key), self.assertRaises(TargetError):
                    self.project(**{key: invalid})

    def test_duplicate_and_malformed_rows_fail_closed(self):
        for key, rows in (
            ("workers", [WORKER, WORKER]),
            ("workers", [{"name": TARGET.worker_name}]),
            ("workers", [WORKER, {"id": "bad/script"}]),
            ("applications", [APP, APP | {"id": OTHER_ID}]),
            ("applications", [APP, APP | {"name": "other-app"}]),
            ("applications", [{"id": APP_ID}]),
        ):
            with self.subTest(listing=key, rows=rows), self.assertRaises(TargetError):
                self.project(**{key: listing(*rows)})

    def test_invalid_listing_uuid_and_pin_fail_independently(self):
        invalid_id = "not-an-app-uuid"
        unrelated = {"name": "unrelated-app", "id": invalid_id}
        with self.assertRaisesRegex(TargetError, "invalid_pre_app_id"):
            self.project(applications=listing(unrelated))
        with self.assertRaisesRegex(TargetError, "invalid_pre_app_pin"):
            self.project(pinned_app_id=invalid_id)

    def test_cross_target_snapshots_and_app_identity_drift_fail_closed(self):
        for foreign in (replace(TARGET, scope="pre-fixture-foreign"), replace(TARGET, shape="0475/0465"),
                        replace(TARGET, run_id=43)):
            with self.subTest(source=foreign), self.assertRaisesRegex(TargetError, "wrong_pre_inventory_target"):
                self.project(source_target=foreign)
        for pin in (None, OTHER_ID):
            with self.subTest(pin=pin), self.assertRaises(TargetError):
                self.project(applications=listing(APP), pinned_app_id=pin, app_detail=detail(0, 0))
        for mismatch in ({"id": OTHER_ID}, {"name": TARGET.container_name + "-shadow"}):
            with self.subTest(mismatch=mismatch), self.assertRaisesRegex(TargetError, "wrong_pre_app_detail"):
                self.project(applications=listing(APP), pinned_app_id=APP_ID, app_detail=detail(0, 0) | mismatch)
        with self.assertRaisesRegex(TargetError, "inconsistent_pre_app_absence"):
            self.project(applications=listing(APP | {"name": "renamed-app"}), pinned_app_id=APP_ID)
        with self.assertRaisesRegex(TargetError, "inconsistent_pre_app_absence"):
            self.project(pinned_app_id=APP_ID, app_detail=detail(0, 0))

    def test_missing_or_invalid_direct_counts_fail_closed(self):
        for invalid in (None, APP, APP | {"health": {"instances": []}},
                        APP | {"health": {"instances": {"active": 0, "healthy": 0}}},
                        detail(0, None), detail(-1, 0), detail(0, False), detail(0.0, 0), detail(0, "0")):
            with self.subTest(detail=invalid), self.assertRaises(TargetError):
                self.project(applications=listing(APP), pinned_app_id=APP_ID, app_detail=invalid)
