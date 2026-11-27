"""Synthetic Item 4 stop and cleanup evidence for both reserved PRE shapes."""

from dataclasses import replace
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

SCRIPTS = str(Path(__file__).resolve().parents[1])
with patch.object(sys, "path", [SCRIPTS, *sys.path]):
    from pre_native_cleanup import (AbsenceReadback, AppCounts, FenceCheck, NativeRegistryReadback,
                                    Reservation, StopEvidence, verify_cleanup)
    from pre_registry_cleanup import RegistryReservation
    from pre_target import Target, TargetError


class Readers:
    def __init__(self, shape="0473/0465", *, never_created=False, lease_cutoff=None, early_stop=False):
        run_id = 475 if shape == "0475/0465" else 473
        target = Target(f"pre-fixture-{run_id}", run_id, 1_800_000_000, shape)
        registry = RegistryReservation(
            target, "registry.example.invalid/test/two-bot-next-attendance-predecessor-staging@sha256:" + "a" * 64,
        )
        app_id = None if never_created else "5cb771af-3421-4223-8a06-39e061f06400"
        self.reservation = Reservation(registry, "b" * 32, app_id, lease_cutoff)
        cutoff = self.reservation.cutoff
        settled_at = cutoff - 10 if early_stop else cutoff + 3
        self.stop = StopEvidence(
            reservation=self.reservation, fence_id="independent-fence", armed_at=target.started_at + 1,
            armed_cutoff=cutoff, first_create_at=None if never_created else target.started_at + 2,
            independent=True, runner_loss=FenceCheck("runner_loss", "independent-fence", cutoff, cutoff, True),
            runner_fenced_at=min(cutoff, settled_at),
            delayed_create=FenceCheck("delayed_create", "independent-fence", cutoff, cutoff + 1, True),
            legacy_keepalive=FenceCheck("legacy_keepalive", "independent-fence", cutoff, cutoff + 2, True),
            settled_at=settled_at, startup_settled=True, write_access=False, worker_removed_at=settled_at + 1,
            app_deleted_at=None if never_created else settled_at + 3, app_never_created=never_created,
            publication_settled_at=cutoff + 4,
        )
        self.counts = AppCounts(self.reservation, settled_at + 2, 0, 0)
        self.absence = AbsenceReadback(self.reservation, cutoff + 7, False, False, False, False, False, False, False)
        self.registry = NativeRegistryReadback(self.reservation, cutoff + 8, False)
        self.calls = []

    def read_stop(self, reservation):
        self.calls.append(("stop", reservation))
        return self.stop

    def read_app_counts(self, reservation):
        self.calls.append(("counts", reservation))
        return self.counts

    def read_absence(self, reservation):
        self.calls.append(("absence", reservation))
        return self.absence

    def read_registry(self, reservation):
        self.calls.append(("registry", reservation))
        return self.registry

    def verify(self, *, readback_offset=7):
        verify_cleanup(self.reservation, authority=self, transport=self,
                       readback_not_before=self.reservation.cutoff + readback_offset, now=self.reservation.cutoff + 9)


class NativeCleanupTests(unittest.TestCase):
    def assert_rejected(self, readers, field, reason, **updates):
        receipt = replace(getattr(readers, field), **updates)
        with self.subTest(field=field, updates=updates), patch.object(readers, field, receipt):
            with self.assertRaisesRegex(TargetError, f"^{reason}$"):
                readers.verify()

    def test_both_reservations_verify_created_and_never_created(self):
        for shape in ("0473/0465", "0475/0465"):
            for never_created in (False, True):
                with self.subTest(shape=shape, never_created=never_created):
                    readers = Readers(shape, never_created=never_created, early_stop=shape == "0475/0465")
                    readers.verify()
                    stages = ("stop", "absence", "registry") if never_created else (
                        "stop", "counts", "absence", "registry",
                    )
                    self.assertEqual(readers.calls, [(stage, readers.reservation) for stage in stages])

    def test_fence_must_be_independent_and_armed_before_create(self):
        readers = Readers()
        for updates, reason in (
            ({"independent": False}, "unconfirmed_pre_independent_fence"),
            ({"armed_cutoff": readers.reservation.cutoff + 1}, "unconfirmed_pre_independent_fence"),
            ({"armed_at": readers.reservation.registry.target.started_at - 1}, "invalid_pre_fence_arm_time"),
            ({"armed_at": readers.stop.first_create_at}, "pre_create_outside_fence"),
            ({"first_create_at": readers.reservation.cutoff}, "pre_create_outside_fence"),
        ):
            self.assert_rejected(readers, "stop", reason, **updates)
        with self.assertRaisesRegex(TargetError, "invalid_pre_lease_cutoff"):
            Readers(lease_cutoff=readers.reservation.registry.target.started_at)

    def test_original_earlier_cutoff_survives_all_three_disruptions(self):
        readers = Readers(lease_cutoff=1_800_000_060)
        readers.verify()
        for disruption in ("runner_loss", "delayed_create", "legacy_keepalive"):
            original = getattr(readers.stop, disruption)
            for updates in (
                {"fence_id": "replacement-fence"}, {"cutoff": readers.reservation.registry.target.expires_at},
                {"blocked": False}, {"disruption": "other"},
                {"observed_at": readers.reservation.cutoff - 1}, {"observed_at": readers.reservation.cutoff + 10},
            ):
                self.assert_rejected(readers, "stop", "pre_cutoff_fence_unconfirmed",
                                     **{disruption: replace(original, **updates)})

    def test_stop_requires_ordered_settlement_revocation_and_deletion(self):
        readers = Readers()
        for updates, reason in (
            ({"startup_settled": False}, "pre_startup_unsettled"),
            ({"settled_at": readers.reservation.cutoff - 1}, "pre_runner_stop_unconfirmed"),
            ({"runner_fenced_at": readers.stop.armed_at}, "pre_runner_stop_unconfirmed"),
            ({"runner_fenced_at": readers.reservation.cutoff + 1}, "pre_runner_stop_unconfirmed"),
            ({"write_access": True}, "pre_writer_revocation_unconfirmed"),
            ({"write_access": None}, "pre_writer_revocation_unconfirmed"),
            ({"worker_removed_at": readers.stop.settled_at - 1}, "pre_worker_removal_unconfirmed"),
            ({"app_deleted_at": readers.stop.worker_removed_at - 1}, "pre_app_deletion_unconfirmed"),
            ({"app_never_created": True}, "pre_app_deletion_unconfirmed"),
            ({"first_create_at": None}, "pre_app_deletion_unconfirmed"),
        ):
            self.assert_rejected(readers, "stop", reason, **updates)
            self.assertEqual(readers.calls, [("stop", readers.reservation)])
            readers.calls.clear()
        self.assert_rejected(Readers(never_created=True), "stop", "pre_app_never_created_unconfirmed",
                             app_deleted_at=readers.stop.app_deleted_at)

    def test_empty_listing_cannot_prove_stop_or_never_created(self):
        for never_created in (False, True):
            readers = Readers(never_created=never_created)
            readers.counts = []
            readers.stop = replace(readers.stop, app_never_created=False)
            reason = "pre_app_never_created_unconfirmed" if never_created else "pre_app_counts_unconfirmed"
            with self.subTest(never_created=never_created), self.assertRaisesRegex(TargetError, reason):
                readers.verify()

    def test_counts_must_both_be_zero_and_observed_before_app_deletion(self):
        readers = Readers()
        for updates in ({"active": 1}, {"assigned": 1}, {"assigned": None}, {"active": False},
                        {"observed_at": readers.stop.app_deleted_at},
                        {"observed_at": readers.stop.worker_removed_at - 1}):
            self.assert_rejected(readers, "counts", "pre_app_stop_unconfirmed", **updates)

    def test_all_evidence_is_exact_scope(self):
        readers = Readers()
        reservation = readers.reservation
        changed_scope = replace(reservation.registry.target, shape="0475/0465")
        wrong_ref = reservation.registry.reference[:-1] + "b"
        for field, foreign, reason in (
            ("stop", replace(reservation, registry=replace(reservation.registry, target=changed_scope)),
             "wrong_pre_stop_evidence"),
            ("counts", replace(reservation, app_id="cccccccc-cccc-cccc-cccc-cccccccccccc"),
             "pre_app_counts_unconfirmed"),
            ("absence", replace(reservation, account_id="c" * 32), "wrong_pre_absence_readback"),
            ("registry", replace(reservation, registry=replace(reservation.registry, reference=wrong_ref)),
             "wrong_pre_registry_readback"),
        ):
            self.assert_rejected(readers, field, reason, reservation=foreign)

    def test_post_delete_absence_must_be_fresh_and_complete(self):
        readers = Readers()
        for observed_at in (readers.stop.app_deleted_at, readers.reservation.cutoff + 10):
            self.assert_rejected(readers, "absence", "stale_pre_absence_readback", observed_at=observed_at)
        with self.assertRaisesRegex(TargetError, "stale_pre_absence_readback"):
            readers.verify(readback_offset=8)
        with self.assertRaisesRegex(TargetError, "invalid_pre_readback_window"):
            readers.verify(readback_offset=10)
        for field in ("worker_present", "subdomain_present", "preview_present", "routes_present",
                      "custom_domains_present", "app_present"):
            self.assert_rejected(readers, "absence", "pre_absence_unconfirmed", **{field: True})
        for write_access in (True, None):
            self.assert_rejected(readers, "absence", "pre_writer_revocation_unconfirmed", write_access=write_access)

    def test_registry_must_be_settled_before_handback(self):
        readers = Readers()
        for settled_at in (None, readers.reservation.registry.target.started_at - 1, readers.reservation.cutoff + 10):
            self.assert_rejected(readers, "stop", "pre_publication_unsettled", publication_settled_at=settled_at)
        for updates in ({"present": True}, {"present": None}, {"observed_at": readers.stop.publication_settled_at},
                        {"observed_at": readers.absence.observed_at - 1}):
            self.assert_rejected(readers, "registry", "pre_registry_disposition_unsettled", **updates)

    def test_read_failure_returns_bounded_error(self):
        readers = Readers()
        with patch.object(readers, "read_absence", side_effect=OSError("raw provider response")):
            with self.assertRaisesRegex(TargetError, "^wrong_pre_absence_readback$"):
                readers.verify()
