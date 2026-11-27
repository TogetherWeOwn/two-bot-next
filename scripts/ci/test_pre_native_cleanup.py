"""Synthetic Item 4 stop and cleanup evidence for both reserved PRE shapes."""

from dataclasses import replace
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

SCRIPTS = str(Path(__file__).resolve().parents[1])
with patch.object(sys, "path", [SCRIPTS, *sys.path]):
    from pre_native_cleanup import (AbsenceReadback, AppCounts, FenceCheck, RegistryReadback,
                                    Reservation, StopEvidence, verify_cleanup)
    from pre_registry_cleanup import RegistryReservation
    from pre_target import Target, TargetError


class Readers:
    def __init__(self, shape="0473/0465", *, never_created=False, lease_cutoff=None):
        retained = shape == "0475/0465"
        target = Target("pre-fixture-retained" if retained else "pre-fixture-original",
                        475 if retained else 473, 1_800_000_000, shape)
        registry = RegistryReservation(
            target, "registry.example.invalid/test/two-bot-next-attendance-predecessor-staging@sha256:" + "a" * 64,
        )
        app_id = None if never_created else "5cb771af-3421-4223-8a06-39e061f06400"
        self.reservation = Reservation(registry, "b" * 32, app_id, lease_cutoff)
        cutoff = self.reservation.cutoff
        self.stop = StopEvidence(
            reservation=self.reservation, fence_id="independent-fence", armed_at=target.started_at + 1,
            armed_cutoff=cutoff, first_create_at=None if never_created else target.started_at + 2,
            independent=True, runner_loss=FenceCheck("runner_loss", "independent-fence", cutoff, cutoff, True),
            delayed_create=FenceCheck("delayed_create", "independent-fence", cutoff, cutoff + 1, True),
            legacy_keepalive=FenceCheck("legacy_keepalive", "independent-fence", cutoff, cutoff + 2, True),
            settled_at=cutoff + 3, startup_settled=True, write_access=False, worker_removed_at=cutoff + 4,
            app_deleted_at=None if never_created else cutoff + 6, app_never_created=never_created,
            publication_settled_at=cutoff + 4,
        )
        self.counts = AppCounts(self.reservation, cutoff + 5, 0, 0)
        self.absence = AbsenceReadback(self.reservation, cutoff + 7, False, False, False, False, False, False)
        self.registry = RegistryReadback(self.reservation, cutoff + 8, False)
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

    def verify(self):
        verify_cleanup(self.reservation, authority=self, transport=self,
                       readback_not_before=self.reservation.cutoff + 7, now=self.reservation.cutoff + 9)


class NativeCleanupTests(unittest.TestCase):
    def test_both_reservations_verify_created_and_never_created(self):
        for shape in ("0473/0465", "0475/0465"):
            for never_created in (False, True):
                with self.subTest(shape=shape, never_created=never_created):
                    readers = Readers(shape, never_created=never_created)
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
            ({"armed_at": readers.stop.first_create_at}, "pre_create_outside_fence"),
            ({"first_create_at": readers.reservation.cutoff}, "pre_create_outside_fence"),
        ):
            with self.subTest(updates=updates), patch.object(readers, "stop", replace(readers.stop, **updates)):
                with self.assertRaisesRegex(TargetError, reason):
                    readers.verify()

    def test_original_earlier_cutoff_survives_all_three_disruptions(self):
        readers = Readers(lease_cutoff=1_800_000_060)
        readers.verify()
        for disruption in ("runner_loss", "delayed_create", "legacy_keepalive"):
            original = getattr(readers.stop, disruption)
            for updates in (
                {"fence_id": "replacement-fence"}, {"cutoff": readers.reservation.registry.target.expires_at},
                {"blocked": False}, {"observed_at": readers.reservation.cutoff - 1}, {"disruption": "other"},
            ):
                stop = replace(readers.stop, **{disruption: replace(original, **updates)})
                with self.subTest(disruption=disruption, updates=updates), patch.object(readers, "stop", stop):
                    with self.assertRaisesRegex(TargetError, "pre_cutoff_fence_unconfirmed"):
                        readers.verify()

    def test_rejects_unsettled_start_or_effective_writer(self):
        for updates, reason in (
            ({"startup_settled": False}, "pre_startup_unsettled"),
            ({"write_access": True}, "pre_writer_revocation_unconfirmed"),
            ({"write_access": None}, "pre_writer_revocation_unconfirmed"),
        ):
            readers = Readers()
            readers.stop = replace(readers.stop, **updates)
            with self.subTest(updates=updates), self.assertRaisesRegex(TargetError, reason):
                readers.verify()
            self.assertEqual(readers.calls, [("stop", readers.reservation)])

    def test_empty_listing_cannot_prove_stop_or_never_created(self):
        for never_created in (False, True):
            readers = Readers(never_created=never_created)
            readers.counts = []
            readers.stop = replace(readers.stop, app_never_created=False)
            with self.subTest(never_created=never_created), self.assertRaisesRegex(
                TargetError, "pre_app_never_created_unconfirmed" if never_created else "pre_app_counts_unconfirmed",
            ):
                readers.verify()

    def test_counts_must_both_be_zero_and_observed_before_app_deletion(self):
        readers = Readers()
        for updates in ({"active": 1}, {"assigned": 1}, {"assigned": None}, {"active": False},
                        {"observed_at": readers.stop.app_deleted_at},
                        {"observed_at": readers.stop.worker_removed_at - 1}):
            with self.subTest(updates=updates), patch.object(readers, "counts", replace(readers.counts, **updates)):
                with self.assertRaisesRegex(TargetError, "pre_app_stop_unconfirmed"):
                    readers.verify()

    def test_all_evidence_is_exact_scope(self):
        readers = Readers()
        reservation = readers.reservation
        changed_scope = replace(reservation.registry.target, shape="0475/0465")
        for field, foreign, reason in (
            ("stop", replace(reservation, registry=replace(reservation.registry, target=changed_scope)),
             "wrong_pre_stop_evidence"),
            ("counts", replace(reservation, app_id="cccccccc-cccc-cccc-cccc-cccccccccccc"),
             "pre_app_counts_unconfirmed"),
            ("absence", replace(reservation, account_id="c" * 32), "wrong_pre_absence_readback"),
            ("registry", replace(reservation, registry=replace(reservation.registry,
                                                               reference=reservation.registry.reference[:-1] + "b")),
             "wrong_pre_registry_readback"),
        ):
            receipt = replace(getattr(readers, field), reservation=foreign)
            with self.subTest(field=field), patch.object(readers, field, receipt):
                with self.assertRaisesRegex(TargetError, reason):
                    readers.verify()

    def test_post_delete_absence_must_be_fresh_and_complete(self):
        readers = Readers()
        for observed_at in (readers.stop.app_deleted_at, readers.reservation.cutoff + 10):
            with self.subTest(observed_at=observed_at), patch.object(
                readers, "absence", replace(readers.absence, observed_at=observed_at),
            ):
                with self.assertRaisesRegex(TargetError, "stale_pre_absence_readback"):
                    readers.verify()
        with self.assertRaisesRegex(TargetError, "stale_pre_absence_readback"):
            verify_cleanup(readers.reservation, authority=readers, transport=readers,
                           readback_not_before=readers.absence.observed_at + 1, now=readers.registry.observed_at)
        for field in ("worker_present", "subdomain_present", "preview_present", "routes_present",
                      "custom_domains_present", "app_present"):
            with self.subTest(field=field), patch.object(readers, "absence", replace(readers.absence, **{field: True})):
                with self.assertRaisesRegex(TargetError, "pre_absence_unconfirmed"):
                    readers.verify()

    def test_registry_must_be_settled_before_handback(self):
        readers = Readers()
        with patch.object(readers, "stop", replace(readers.stop, publication_settled_at=None)):
            with self.assertRaisesRegex(TargetError, "pre_publication_unsettled"):
                readers.verify()
        for updates in ({"present": True}, {"present": None}, {"observed_at": readers.stop.publication_settled_at}):
            with self.subTest(updates=updates), patch.object(readers, "registry", replace(readers.registry, **updates)):
                with self.assertRaisesRegex(TargetError, "pre_registry_disposition_unsettled"):
                    readers.verify()

    def test_read_failure_returns_bounded_error(self):
        readers = Readers()
        with patch.object(readers, "read_absence", side_effect=OSError("raw provider response")):
            with self.assertRaisesRegex(TargetError, "^wrong_pre_absence_readback$"):
                readers.verify()
