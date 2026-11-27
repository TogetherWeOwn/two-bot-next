"""Synthetic PRE listings and direct app responses for the pure projection."""

from dataclasses import replace
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_inventory import (APPLICATIONS_PER_PAGE, CONTAINER_APPLICATIONS, MAX_APPLICATION_PAGES,
                               MAX_PAGE_TOKEN_LENGTH, MAX_ROWS, normalize_inventory, read_provider_listing)
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


def provider_page(*apps, **info):
    return {"success": True, "errors": [], "messages": [], "result": list(apps),
            "result_info": {"per_page": APPLICATIONS_PER_PAGE} | info}


class PreProviderListingTests(unittest.TestCase):
    def read(self, read_page, *, source=TARGET, route=CONTAINER_APPLICATIONS):
        return read_provider_listing(TARGET, source, route=route, read_page=read_page)

    def test_cursor_listing_exposes_app_on_later_page(self):
        cursor = "synthetic+/="
        neighbor = {"name": replace(TARGET, run_id=43).container_name, "id": OTHER_ID}
        pages = iter([provider_page(neighbor, next_page_token=cursor),
                      provider_page(APP | {"configuration": {"image": "synthetic"}}, page_token=cursor)])
        requests = []

        def read_page(route, query):
            requests.append((route, query))
            return next(pages)

        apps = self.read(read_page)
        self.assertEqual(requests, [(CONTAINER_APPLICATIONS, {"per_page": APPLICATIONS_PER_PAGE}),
                                    (CONTAINER_APPLICATIONS, {"per_page": APPLICATIONS_PER_PAGE,
                                                              "page_token": cursor})])
        self.assertEqual(apps, listing(neighbor, APP))
        result = normalize_inventory(TARGET, TARGET, workers=listing(), applications=apps,
                                     pinned_app_id=APP_ID, app_detail=detail(1, 2))
        self.assertEqual(result.container, Resource(TARGET.container_name, APP_ID))
        self.assertEqual((result.instance_count, result.assigned_count), (1, 2))

    def test_empty_terminal_page_can_show_app_absence(self):
        apps = self.read(lambda route, query: provider_page())
        self.assertEqual(apps, listing())
        result = normalize_inventory(TARGET, TARGET, workers=listing(), applications=apps)
        self.assertIsNone(result.container)
        self.assertIsNone(result.instance_count)

    def test_incomplete_and_unsupported_page_shapes_fail_closed(self):
        for reason, page in (
            ("unrecognized_pre_listing_page", None),
            ("unrecognized_pre_listing_page", provider_page() | {"success": 1}),
            ("unrecognized_pre_listing_page", provider_page() | {"errors": [{"code": 1000}]}),
            ("unrecognized_pre_listing_page", provider_page() | {"messages": [{"code": 1000}]}),
            ("unrecognized_pre_listing_page", provider_page() | {"truncated": True}),
            ("wrong_pre_listing_page", {"success": True, "result": []}),
            ("wrong_pre_listing_page", provider_page() | {"result_info": {}}),
            ("wrong_pre_listing_page", provider_page(per_page=float(APPLICATIONS_PER_PAGE))),
            ("wrong_pre_listing_page", provider_page(per_page=1)),
            ("wrong_pre_listing_page", provider_page(total_count=1)),
            ("wrong_pre_listing_page", provider_page(page_token="unexpected")),
            ("invalid_pre_listing_page_size", provider_page() | {"result": APP}),
            ("invalid_pre_listing_page_size", provider_page(*([APP] * (APPLICATIONS_PER_PAGE + 1)))),
            ("unrecognized_pre_resource", provider_page(None)),
            ("invalid_pre_app_id", provider_page(APP | {"id": "not-a-uuid"})),
            ("empty_pre_listing_page", provider_page(next_page_token="next")),
        ):
            with self.subTest(page=page), self.assertRaisesRegex(TargetError, f"^{reason}$"):
                self.read(lambda route, query: page)

    def test_consistency_and_progress_are_required_across_pages(self):
        for reason, second in (
            ("wrong_pre_listing_page", provider_page(APP)),
            ("wrong_pre_listing_page", provider_page(APP, page_token="another-page")),
            ("empty_pre_listing_page", provider_page(page_token="next")),
            ("invalid_pre_listing_cursor", provider_page(APP, page_token="next", next_page_token="next")),
            ("duplicate_pre_resource", provider_page(APP, page_token="next")),
            ("duplicate_pre_resource", provider_page(APP | {"id": OTHER_ID}, page_token="next")),
            ("duplicate_pre_resource", provider_page(APP | {"name": "other-app"}, page_token="next")),
        ):
            with self.subTest(second=second), self.assertRaisesRegex(TargetError, f"^{reason}$"):
                pages = iter([provider_page(APP, next_page_token="next"), second])
                self.read(lambda route, query: next(pages))

    def test_malformed_cursors_and_missing_continuation_fail_closed(self):
        for cursor in (None, False, "", " ", "a\n", "é", "a" * (MAX_PAGE_TOKEN_LENGTH + 1)):
            with self.subTest(cursor=cursor), self.assertRaisesRegex(TargetError, "^invalid_pre_listing_cursor$"):
                self.read(lambda route, query: provider_page(APP, next_page_token=cursor))
        pages = iter([provider_page(APP, next_page_token="next")])
        with self.assertRaisesRegex(TargetError, "^pre_listing_read_failed$"):
            self.read(lambda route, query: next(pages))

    def test_nonterminating_traversal_hits_page_bound(self):
        calls = []

        def read_page(route, query):
            calls.append(query)
            echo = {"page_token": query["page_token"]} if "page_token" in query else {}
            app = {"name": f"app-{len(calls)}", "id": f"12345678-1234-4234-8234-{len(calls):012d}"}
            return provider_page(app, next_page_token=str(len(calls)), **echo)

        with self.assertRaisesRegex(TargetError, "^pre_listing_page_limit$"):
            self.read(read_page)
        self.assertEqual(len(calls), MAX_APPLICATION_PAGES)

    def test_wrong_reservation_and_unsupported_routes_refuse_before_read(self):
        calls = []

        def read_page(route, query):
            calls.append((route, query))
            return {"success": True, "result": []}

        with self.assertRaisesRegex(TargetError, "^wrong_pre_inventory_target$"):
            self.read(read_page, source=replace(TARGET, shape="0475/0465"))
        for route in ("/workers/scripts", CONTAINER_APPLICATIONS + "?name=another-app"):
            with self.subTest(route=route), self.assertRaisesRegex(TargetError, "^unsupported_pre_listing_route$"):
                self.read(read_page, route=route)
        self.assertEqual(calls, [])
