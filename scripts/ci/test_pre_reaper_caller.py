"""Replay the dormant reaper against one independently held reservation."""

from contextlib import contextmanager, nullcontext
from dataclasses import replace
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock, patch
from uuid import UUID

from scripts.ci.test_staging_pre_reaper_fixture import OPERATIONS, SyntheticStaging

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_reaper_caller import CleanupClaim, ReaperReservation, reap_pre
    from pre_registry_cleanup import (
        RegistryDisposition, RegistryReadback, RegistryReservation, registry_cleanup_disposition,
    )
    from pre_sweep import SweepError, sweep_target
    from pre_target import Target


class SyntheticAuthority:
    def __init__(self, target):
        self.reservation = ReaperReservation(UUID("12345678-1234-4234-8234-123456789abc"), target)
        self.staging = SyntheticStaging(target)
        self.now = target.expires_at
        self.start_write_in_flight = False
        self.claimed = False
        self.registry_present = False
        self.registry = RegistryReservation(
            target, "registry.invalid/team/two-bot-next-attendance-predecessor-staging@sha256:" + "a" * 64,
        )

    def resolve(self, reference):
        if reference == self.reservation.reference:
            return self.reservation
        return None

    def run_status(self, target):
        return self.staging.run_status(target)

    def trusted_now(self):
        return self.now

    @contextmanager
    def cleanup_claim(self, reservation):
        if self.claimed or self.start_write_in_flight:
            yield None
            return
        self.claimed = True
        try:
            yield CleanupClaim(reservation, self.staging)
        finally:
            self.claimed = False

    def finalize(self, target, *, authorize, trusted_now, adapter):
        # Stand in for the protected finalizer. Exercise the landed exact
        # sweep and registry verdict, using only synthetic provider state.
        if not self.claimed or not authorize(target):
            raise SweepError("synthetic claim is missing")
        result = sweep_target(target, adapter, now=trusted_now())
        if result == "spared":
            return result
        return registry_cleanup_disposition(
            self.registry, adapter.inventory(target),
            RegistryReadback(target, self.registry.reference, self.registry_present),
            now=trusted_now(), run_status=adapter.run_status(target), publication_settled=True,
        )


class PreReaperCallerTests(unittest.TestCase):
    def setUp(self):
        self.target = Target("pre-fixture-original", 42, 1_800_000_000, "0473/0465")
        self.authority = SyntheticAuthority(self.target)
        self.reference = str(self.authority.reservation.reference)
        self.finalizer = Mock(wraps=self.authority.finalize)
        self.staging = self.authority.staging

    def reap(self):
        return reap_pre(self.reference, authority=self.authority, finalize=self.finalizer)

    def mutations(self):
        return [event for event in self.staging.events if event[0] in OPERATIONS]

    def test_bad_reference_or_unconfirmed_resolution_never_reaches_claim(self):
        for malformed in (None, b"0" * 36, "0" * 37, "０" * 36, self.reference.upper(),
                          str(UUID(int=0)), "12345678-1234-4234-8234-000000000000"):
            with self.subTest(reference=malformed):
                with self.assertRaisesRegex(SweepError, "reservation or eligibility"):
                    reap_pre(malformed, authority=self.authority, finalize=self.finalizer)
        wrong_record = replace(self.authority.reservation, reference=UUID(int=1))
        with patch.object(self.authority, "resolve", return_value=wrong_record):
            with self.assertRaisesRegex(SweepError, "reservation or eligibility"):
                self.reap()
        self.finalizer.assert_not_called()
        self.assertEqual(self.staging.events, [])

    def test_active_run_before_cutoff_is_spared_without_a_claim(self):
        self.authority.now = self.target.expires_at - 1
        self.staging.status = "active"
        with patch.object(self.authority, "cleanup_claim") as claim:
            self.assertEqual(self.reap(), "spared")
            claim.assert_not_called()
        self.finalizer.assert_not_called()
        self.assertEqual(self.staging.events, [("status", self.target)])

    def test_terminal_run_before_cutoff_reaches_the_finalizer(self):
        self.authority.now = self.target.expires_at - 1
        self.staging.status = "terminal"
        self.assertIs(self.reap(), RegistryDisposition.ALREADY_ABSENT)
        self.assertIs(self.finalizer.call_args.args[0], self.target)
        self.assertEqual([event[0] for event in self.mutations()], list(OPERATIONS))

    def test_unknown_status_or_clock_refuses_even_at_cutoff(self):
        for status, now in (("unknown", self.target.expires_at), ("terminal", True),
                            ("active", self.target.started_at - 1), ("terminal", 10**10)):
            with self.subTest(status=status, now=now), patch.object(self.authority, "cleanup_claim") as claim:
                self.staging.status, self.authority.now = status, now
                with self.assertRaisesRegex(SweepError, "reservation or eligibility"):
                    self.reap()
                claim.assert_not_called()
        self.finalizer.assert_not_called()
        self.assertEqual(self.mutations(), [])

    def test_claim_refuses_a_different_reference_for_the_same_target(self):
        wrong_reservation = replace(self.authority.reservation, reference=UUID(int=1))
        claim = CleanupClaim(wrong_reservation, self.staging)
        with patch.object(self.authority, "cleanup_claim", return_value=nullcontext(claim)):
            with self.assertRaisesRegex(SweepError, "claim or finalization"):
                self.reap()
        self.finalizer.assert_not_called()
        self.assertEqual(self.mutations(), [])

    def test_claim_refuses_a_live_writer_and_crossed_reservation(self):
        self.authority.start_write_in_flight = True
        with self.assertRaisesRegex(SweepError, "claim or finalization"):
            self.reap()
        crossed = replace(self.target, shape="0475/0465")
        self.assertEqual(crossed.worker_name, self.target.worker_name)
        self.assertEqual(crossed.container_name, self.target.container_name)
        claim = CleanupClaim(replace(self.authority.reservation, target=crossed), self.staging)
        with patch.object(self.authority, "cleanup_claim", return_value=nullcontext(claim)):
            with self.assertRaisesRegex(SweepError, "claim or finalization"):
                self.reap()
        self.finalizer.assert_not_called()
        self.assertEqual(self.mutations(), [])

    def test_runner_loss_replays_original_reservation_and_waits_for_settlement(self):
        self.staging.status = "active"  # The lost workflow can still appear active at cutoff.
        self.staging.snapshot = replace(self.staging.snapshot, startup_settled=False)
        app = self.staging.snapshot.container
        with self.assertRaisesRegex(SweepError, "claim or finalization"):
            self.reap()
        self.assertIs(self.finalizer.call_args.args[0], self.target)
        self.assertEqual([event[0] for event in self.mutations()], list(OPERATIONS[:-1]))
        self.assertEqual(self.staging.snapshot.container, app)

        # On a separate invocation, affirmative settlement permits finalization.
        self.staging.events.clear()
        self.staging.snapshot = replace(self.staging.snapshot, startup_settled=True)
        self.assertIs(self.reap(), RegistryDisposition.ALREADY_ABSENT)
        self.assertEqual(self.mutations(), [
            ("stop_container", self.target, app.name, app.id),
            ("delete_container", self.target, app.name, app.id),
        ])
        self.assertIsNone(self.staging.snapshot.container)

    def test_claim_excludes_another_reaper_and_scopes_finalizer_authorization(self):
        def finalize(target, *, authorize, trusted_now, adapter):
            self.assertTrue(authorize(target))
            self.assertFalse(authorize(replace(target, scope="pre-fixture-sibling")))
            with self.assertRaisesRegex(SweepError, "claim or finalization"):
                self.reap()
            return self.authority.finalize(target, authorize=authorize, trusted_now=trusted_now, adapter=adapter)

        self.finalizer.side_effect = finalize
        self.assertIs(self.reap(), RegistryDisposition.ALREADY_ABSENT)
        self.finalizer.assert_called_once()
        self.assertFalse(self.finalizer.call_args.kwargs["authorize"](self.target))
        self.assertEqual([event[0] for event in self.mutations()], list(OPERATIONS))

    def test_registry_disposition_passes_through_and_bare_success_is_refused(self):
        self.authority.registry_present = True
        self.assertIs(self.reap(), RegistryDisposition.UNRESOLVED_DIGEST_REFERENCE)
        for verdict in ("already_absent", "cleared", "spared", None, True):
            with self.subTest(verdict=verdict):
                self.finalizer.side_effect = lambda *args, **kwargs: verdict
                with self.assertRaisesRegex(SweepError, "claim or finalization"):
                    self.reap()

    def test_unconfirmed_claim_exit_or_suppressed_finalizer_error_never_clears(self):
        @contextmanager
        def uncertain_exit(_):
            yield CleanupClaim(self.authority.reservation, self.staging)
            raise OSError("raw synthetic provider error")

        @contextmanager
        def suppressing_claim(_):
            try:
                yield CleanupClaim(self.authority.reservation, self.staging)
            except OSError:
                pass

        for claim, effect in ((uncertain_exit, None), (suppressing_claim, OSError("synthetic finalizer error"))):
            with self.subTest(claim=claim), patch.object(self.authority, "cleanup_claim", side_effect=claim):
                self.finalizer.side_effect = effect
                self.finalizer.return_value = RegistryDisposition.ALREADY_ABSENT
                with self.assertRaisesRegex(SweepError, "^PRE reaper claim or finalization could not be verified$") as error:
                    self.reap()
                self.assertTrue(error.exception.__suppress_context__)
