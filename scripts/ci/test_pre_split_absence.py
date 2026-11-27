"""Exercise independent PRE cleanup with split exact-scope provider readbacks."""

from contextlib import contextmanager
from dataclasses import replace
from pathlib import Path
import sys
import unittest
from unittest.mock import patch
from uuid import UUID

from scripts.ci import test_pre_container_lookup as app_fixture
from scripts.ci import test_pre_inventory as inventory_fixture
from scripts.ci import test_pre_worker_lookup as worker_fixture
from scripts.ci.test_pre_caller import ORIGINAL, RETAINED, Fixture, Upload
from scripts.ci.test_staging_pre_reaper_fixture import OPERATIONS

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_caller import finalize_pre
    from pre_container_lookup import ContainerAppScope, read_reserved_container_app
    from pre_inventory import (
        APPLICATIONS_PER_PAGE, CONTAINER_APPLICATIONS, normalize_inventory, read_provider_listing,
    )
    from pre_reaper_caller import CleanupClaim, ReaperReservation, reap_pre
    from pre_registry_cleanup import RegistryDisposition
    from pre_start_stop import Operation
    from pre_sweep import SweepError
    from pre_target import TargetError
    from pre_worker_lookup import WorkerLookupResponse, read_reserved_worker


class SplitReadback(Fixture):
    def __init__(self, target, inconclusive_read):
        super().__init__(target)
        self.original = ReaperReservation(UUID(int=target.run_id), target)
        self.scope = ContainerAppScope(target, app_fixture.SCOPE.account_id, self.provisioned.container.id)
        self.neighbor = {"id": app_fixture.SIBLING_APP_ID,
                         "name": replace(target, run_id=target.run_id + 10).container_name}
        self.inconclusive_read = inconclusive_read
        self.post_delete_observations = []
        self.claims = []
        self.claimed = False
        self.stopped_counts = (None, None)

    def resolve(self, reference):
        return self.original if reference == self.original.reference else None

    def trusted_now(self):
        return self.now

    @contextmanager
    def cleanup_claim(self, reservation):
        assert reservation is self.original and not self.claimed
        assert self.fenced and self.record.closed and self.now >= reservation.target.expires_at
        self.claims.append(reservation)
        self.claimed = True
        try:
            yield CleanupClaim(reservation, self)
        finally:
            self.claimed = False

    def read_page(self, route, query):
        assert self.claimed and route == CONTAINER_APPLICATIONS
        cursor = "synthetic-second-page"
        app = self.snapshot.container
        if query == {"per_page": APPLICATIONS_PER_PAGE}:
            if app is None:
                return inventory_fixture.provider_page(self.neighbor)
            return inventory_fixture.provider_page(self.neighbor, next_page_token=cursor)
        assert query == {"per_page": APPLICATIONS_PER_PAGE, "page_token": cursor} and app is not None
        return inventory_fixture.provider_page({"id": app.id, "name": app.name}, page_token=cursor)

    def read_worker(self, path):
        assert self.claimed and path == f"/accounts/{self.scope.account_id}/workers/workers/{self.target.worker_name}"
        if self.snapshot.worker is None:
            return WorkerLookupResponse(path, 404, worker_fixture.MISSING)
        result = worker_fixture.WORKER | {"name": self.target.worker_name}
        return WorkerLookupResponse(path, 200, worker_fixture.SUCCESS | {"result": result})

    def inventory(self, target):
        assert self.claimed and target is self.original.target
        base = super().inventory(target)
        applications = read_provider_listing(
            target, self.scope.target, route=CONTAINER_APPLICATIONS, read_page=self.read_page,
        )
        worker = read_reserved_worker(
            target, self.scope.target, staging_account_id=self.scope.account_id, read=self.read_worker,
        )
        path = f"/accounts/{self.scope.account_id}/containers/applications/{self.scope.app_id}"
        after_delete = base.container is None
        if after_delete:
            # Identical status and body. Only the independent handler proof differs.
            read_number = len(self.post_delete_observations) + 1
            reply = replace(app_fixture.ABSENT, scope=self.scope, path=path,
                            application_not_found=read_number != self.inconclusive_read)
        else:
            app = {"account_id": self.scope.account_id, "id": base.container.id, "name": base.container.name,
                   "health": {"instances": {"active": base.instance_count, "assigned": base.assigned_count}}}
            reply = replace(app_fixture.PRESENT, scope=self.scope, path=path,
                            body=app_fixture.PRESENT.body | {"result": app})
        try:
            container = read_reserved_container_app(self.scope, read=lambda _: reply)
        except TargetError as error:
            if after_delete:
                self.post_delete_observations.append((worker, str(error), applications["total_count"]))
            raise
        if after_delete:
            self.post_delete_observations.append((worker, container, applications["total_count"]))

        workers = inventory_fixture.listing() if worker is None else inventory_fixture.listing({"id": worker.id})
        projection = normalize_inventory(
            target, self.scope.target, workers=workers, applications=applications,
            pinned_app_id=self.scope.app_id, app_detail=reply.body["result"],
        )
        assert (projection.worker, projection.container) == (worker, container)
        counts = self.stopped_counts if after_delete else (projection.instance_count, projection.assigned_count)
        return replace(base, worker=worker, container=container, complete=projection.complete,
                       instance_count=counts[0], assigned_count=counts[1])

    def stop_container(self, target, action):
        assert self.claimed
        super().stop_container(target, action)
        observed = self.inventory(target)
        assert observed.container is not None and observed.container.id == self.scope.app_id
        self.stopped_counts = observed.instance_count, observed.assigned_count
        return observed

    def apply(self, target, action):
        assert self.claimed and target is self.original.target
        return super().apply(target, action)

    def read_registry(self, reservation):
        assert self.claimed and reservation.target is self.original.target
        return super().read_registry(reservation)

    def reap(self):
        return reap_pre(str(self.original.reference), authority=self, finalize=finalize_pre)


class SplitReadbackTests(unittest.TestCase):
    def lost_start(self, target, inconclusive_read=None):
        host = SplitReadback(target, inconclusive_read)
        host.lose_create_reply = True
        with self.assertRaisesRegex(TargetError, "^pre_create_unconfirmed$"):
            host.caller(Operation.START).start(Upload(target))
        self.assertEqual(host.steps(), ["admit", "arm", "create"])
        start_id = host.record.start_id
        host.now = target.expires_at
        self.assertTrue(host.caller(Operation.STOP).stop())
        self.assertEqual(host.record.start_id, start_id)
        self.assertEqual(host.status, "active")
        return host

    def assert_handback(self, host, *, claims):
        self.assertEqual([step for step in host.steps() if step in OPERATIONS], list(OPERATIONS))
        for step in ("registry_reservation", "publication_settled", "registry"):
            self.assertEqual(host.steps().count(step), 1)
        self.assertEqual(host.stopped_counts, (0, 0))
        self.assertEqual(host.claims, [host.original] * claims)
        self.assertTrue(all(event[1] == host.original.target for event in host.events))
        self.assertFalse(host.claimed)

    def test_inconclusive_app_at_either_readback_withholds_handback_until_same_reservation_retry(self):
        for target in (ORIGINAL, RETAINED):
            for read_number, stage in ((1, "cleanup readback"), (2, "registry handback")):
                with self.subTest(shape=target.shape, stage=stage):
                    host = self.lost_start(target, read_number)
                    with self.assertRaisesRegex(SweepError, "^PRE reaper claim or finalization") as failure:
                        host.reap()
                    self.assertEqual(str(failure.exception.__context__), f"PRE {stage} could not be verified")
                    first_attempt = [(None, None, 1)] * (read_number - 1)
                    first_attempt.append((None, "pre_container_lookup_unconfirmed", 1))
                    self.assertEqual(host.post_delete_observations, first_attempt)
                    for step in ("registry_reservation", "publication_settled", "registry"):
                        self.assertNotIn(step, host.steps())
                    self.assertFalse(host.claimed)

                    self.assertIs(host.reap(), RegistryDisposition.ALREADY_ABSENT)
                    self.assertEqual(host.post_delete_observations[read_number:], [(None, None, 1)] * 3)
                    self.assert_handback(host, claims=2)

    def test_both_exact_absences_allow_handback_on_first_attempt(self):
        for target in (ORIGINAL, RETAINED):
            with self.subTest(shape=target.shape):
                host = self.lost_start(target)
                self.assertIs(host.reap(), RegistryDisposition.ALREADY_ABSENT)
                self.assertEqual(host.post_delete_observations, [(None, None, 1)] * 2)
                self.assert_handback(host, claims=1)
