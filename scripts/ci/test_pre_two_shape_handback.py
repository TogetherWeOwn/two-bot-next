"""Replay the PRE shape transition with only in-memory evidence and effects."""

from dataclasses import replace
from pathlib import Path
import sys
import unittest
from unittest.mock import ANY, Mock, call, patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from ci.pre_operator_acceptance_fixture import (
        PRE_STATES, SAMPLE_BUILD, AcceptedRow, IngressReadback, RegistryCleanupEvidence, SweepEvidence, accept,
        sample_endpoint, verify_endpoint,
    )
    from pre_registry_cleanup import PublicationSettlement, RegistryReadback, RegistryReservation
    from pre_start_stop import Operation, ProtectedPre
    from pre_sweep import CleanupReadback, SweepError, sweep_target
    from pre_target import Action, Inventory, ProtectedRouteReadback, Resource, Target, TargetError, require
    from rollback_proof import ProofError


ORIGINAL = Target("pre-fixture-operator", 42, 1_800_000_000, "0473/0465")
RETAINED = replace(ORIGINAL, run_id=43, started_at=ORIGINAL.started_at + 60, shape="0475/0465")
REFERENCE = ("registry.example.invalid/staging/two-bot-next-attendance-predecessor-staging@"
             + SAMPLE_BUILD.image_digest)


class ShapeFixture:
    def __init__(self, target, worker_version, app_id):
        self.target = target
        self.now = target.started_at + 10
        self.endpoint = sample_endpoint(target, PRE_STATES[target.shape], worker_version)
        self.registry = RegistryReservation(target, REFERENCE)
        self.read_registry = Mock(return_value=RegistryReadback(target, REFERENCE, present=False))
        self.publication_settlement = PublicationSettlement(self.registry)

        app = Resource(target.container_name, app_id)
        self.app_id = app_id
        self.route = ProtectedRouteReadback(target, target.worker_name, app)
        self.read_ingress = Mock(side_effect=lambda _, *, request: IngressReadback(self.route, False, request))
        worker = Resource(target.worker_name, target.worker_name)
        self.before = Inventory(target, worker, app, 1, True, True, startup_settled=True, assigned_count=1)
        self.drained = replace(self.before, worker=None, write_access=False, instance_count=0, assigned_count=0)
        self.absent = replace(self.drained, container=None)
        self.finalizer = Mock(spec_set=["run_status", "inventory", "apply", "stop_container", "cleanup_inventory"])
        self.finalizer.run_status.return_value = "terminal"
        self.finalizer.inventory.side_effect = [self.before, self.absent]
        self.finalizer.cleanup_inventory.side_effect = self.cleanup_inventory
        self.finalizer.stop_container.return_value = self.drained
        self.finalizer.apply.return_value = True

        # The stop caller is separate from the sweeper and has only stop authority.
        self.stop_authority = Mock(spec_set=["fence", "close", "settle"])
        self.stop_authority.fence.return_value = True
        self.stop_authority.close.return_value = True
        self.stop_authority.settle.return_value = True
        self.controller = ProtectedPre(target, authorize=lambda pinned, op: pinned == target and op is Operation.STOP,
                                       admission=self.stop_authority, runtime=None, external_stop=self.stop_authority)
        self.stopped = False

    def cleanup_inventory(self, target, *, request):
        require(target == self.target, "wrong_pre_inventory_target")
        return CleanupReadback(self.absent, request)

    def ready_then_stop(self):
        verify_endpoint(self.target, SAMPLE_BUILD, **self.endpoint)
        self.stopped = self.controller.stop()
        return self.stopped

    def handback(self):
        require(self.stopped is True, "pre_stop_unconfirmed")
        outcome = sweep_target(self.target, self.finalizer, now=self.now)
        registry = None
        if outcome == "cleared":
            inventory = self.finalizer.inventory(self.target)
            if self.publication_settlement is None:
                raise ProofError("pre_registry_publication_unsettled")
            registry = RegistryCleanupEvidence(
                inventory, self.read_registry(self.registry), self.now,
                self.finalizer.run_status(self.target), self.publication_settlement,
            )
        return accept(self.registry, SAMPLE_BUILD, **self.endpoint,
                      sweep=SweepEvidence(self.target, outcome), registry=registry,
                      pinned_route=self.route, pinned_app_id=self.app_id,
                      read_ingress=self.read_ingress)


def start_retained_after_handback(original, reserve, start):
    accepted = original.handback()
    target = reserve()
    require(target.shape == RETAINED.shape and target.run_id != original.target.run_id,
            "pre_retained_reservation_not_independent")
    return accepted, start(target)


class PreTwoShapeHandbackTests(unittest.TestCase):
    def setUp(self):
        self.first = ShapeFixture(ORIGINAL, "12345678-1234-4234-8234-123456789abc",
                                  "87654321-4321-4321-8321-cba987654321")
        self.second = ShapeFixture(RETAINED, "12345678-1234-4234-8234-123456789def",
                                   "87654321-4321-4321-8321-cba987654322")
        self.assertIs(self.first.ready_then_stop(), True)
        self.assertEqual(self.first.stop_authority.mock_calls,
                         [call.fence(ORIGINAL), call.close(ORIGINAL), call.settle(ORIGINAL)])
        self.effects = Mock()
        self.effects.reserve.return_value = RETAINED

    def advance(self):
        return start_retained_after_handback(self.first, self.effects.reserve, self.effects.start)

    def test_positive_stop_does_not_hand_back_an_active_run(self):
        self.first.finalizer.run_status.return_value = "active"
        with self.assertRaisesRegex(ProofError, "^pre_cleanup_unproved$"):
            self.advance()
        self.assertEqual(self.effects.mock_calls, [])
        self.assertEqual(self.first.finalizer.mock_calls, [call.run_status(ORIGINAL), call.inventory(ORIGINAL)])
        self.first.read_registry.assert_not_called()

    def test_registry_uncertainty_blocks_second_start_after_terminal_sweep(self):
        absent = self.first.read_registry.return_value
        for readback, settled, reason in (
            (replace(absent, present=True), True, "pre_registry_cleanup_unproved"),
            (replace(absent, present=None), True, "unconfirmed_pre_registry_readback"),
            (absent, False, "pre_registry_publication_unsettled"),
            (self.second.read_registry.return_value, True, "wrong_pre_registry_scope"),
        ):
            with self.subTest(reason=reason):
                self.first.read_registry.reset_mock()
                self.first.read_registry.return_value = readback
                self.first.publication_settlement = (
                    PublicationSettlement(self.first.registry) if settled else None
                )
                self.first.finalizer.inventory.side_effect = [self.first.before, self.first.absent]
                with self.assertRaisesRegex(ProofError, f"^{reason}$"):
                    self.advance()
                self.assertEqual(self.effects.mock_calls, [])
                if settled:
                    self.first.read_registry.assert_called_once_with(self.first.registry)
                else:
                    self.first.read_registry.assert_not_called()

    def test_fresh_exact_handback_after_lost_final_read_allows_second_start(self):
        first = self.first
        first.finalizer.cleanup_inventory.side_effect = TimeoutError("synthetic final read timeout")
        app = Action("delete_container", ORIGINAL.container_name, first.drained.container.id)
        with self.assertRaisesRegex(SweepError, "^PRE cleanup readback could not be verified$"):
            self.advance()
        self.assertEqual(self.effects.mock_calls, [])
        first.read_registry.assert_not_called()
        self.assertEqual(first.finalizer.mock_calls, [
            call.run_status(ORIGINAL), call.inventory(ORIGINAL),
            call.apply(ORIGINAL, Action("revoke_write_access", ORIGINAL.service_name, ORIGINAL.service_name)),
            call.apply(ORIGINAL, Action("delete_worker", ORIGINAL.worker_name, ORIGINAL.worker_name)),
            call.stop_container(ORIGINAL, app), call.apply(ORIGINAL, app),
            call.cleanup_inventory(ORIGINAL, request=ANY),
        ])
        lost_request = first.finalizer.cleanup_inventory.call_args.kwargs["request"]

        # Reconcile the same reservation using fresh absence with the exact app's saved zero counts.
        first.finalizer.reset_mock()
        first.now += 1
        first.finalizer.inventory.side_effect = [first.absent, first.absent]
        first.finalizer.cleanup_inventory.side_effect = first.cleanup_inventory

        second = self.second

        def rehearse_second(target):
            self.assertEqual(target, second.target)
            self.assertNotEqual(target.worker_name, first.target.worker_name)
            self.assertGreater(target.started_at, first.now)
            with self.assertRaisesRegex(ProofError, "^pre_endpoint_reservation_mismatch$"):
                verify_endpoint(second.target, SAMPLE_BUILD, **first.endpoint)
            self.assertIs(second.ready_then_stop(), True)
            return second.handback()

        self.effects.start.side_effect = rehearse_second
        original, retained = self.advance()
        self.assertEqual(original, AcceptedRow(first.target, SAMPLE_BUILD, PRE_STATES[ORIGINAL.shape],
                                               "12345678-1234-4234-8234-123456789abc", "cleared"))
        self.assertEqual(retained, AcceptedRow(second.target, SAMPLE_BUILD, PRE_STATES[RETAINED.shape],
                                               "12345678-1234-4234-8234-123456789def", "cleared"))
        self.assertEqual(first.finalizer.mock_calls, [
            call.run_status(ORIGINAL), call.inventory(ORIGINAL), call.cleanup_inventory(ORIGINAL, request=ANY),
            call.inventory(ORIGINAL), call.run_status(ORIGINAL),
        ])
        first_request = first.finalizer.cleanup_inventory.call_args.kwargs["request"]
        self.assertIsNot(lost_request, first_request)
        second.finalizer.cleanup_inventory.assert_called_once_with(RETAINED, request=ANY)
        self.assertIsNot(first_request, second.finalizer.cleanup_inventory.call_args.kwargs["request"])
        first.read_registry.assert_called_once_with(first.registry)
        self.assertEqual(self.effects.mock_calls, [call.reserve(), call.start(RETAINED)])
        second.read_registry.assert_called_once_with(second.registry)

    def test_relabeling_first_reservation_does_not_start_second_shape(self):
        relabeled = replace(ORIGINAL, shape=RETAINED.shape)
        self.effects.reserve.return_value = relabeled
        with self.assertRaisesRegex(TargetError, "^pre_retained_reservation_not_independent$"):
            self.advance()
        self.assertEqual(self.effects.mock_calls, [call.reserve()])

    def test_route_must_match_independently_pinned_app(self):
        first = self.first
        first.route = replace(first.route, app=replace(first.route.app,
                                                       id="12345678-1234-4234-8234-123456789def"))
        with self.assertRaisesRegex(ProofError, "^pre_ingress_unproved$"):
            self.advance()
        self.assertEqual(self.effects.mock_calls, [])
