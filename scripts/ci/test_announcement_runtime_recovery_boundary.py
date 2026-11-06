"""SYNTHETIC paired authority/recovery scope, NOT full-packet or live evidence.

Unlike the earlier recovery-only fixtures, ordinary runtime scope, decisions,
reads, installation anchors and checkpoints agree with this receipt. Ordinary
setup/read/setting writes pass the real authority checker when compared alone;
adding cleanup reaches the exact unsupported cross-consumer boundary, not an
unrelated scope or timestamp error. No checker is mocked or omitted from the
production entry point. Telemetry, literal resources and process restoration are
still incomplete, and both runtime admission and lifecycle cleanup MUST refuse.

Timeline: ordinary lease 00:00-00:40, attempt 00:00-00:22, setup 00:02,
rollout 00:05, read 00:12, setting write 00:15, separately authorized cleanup
00:20:30, final reads 00:22. Recovery lease remains 00:20-00:30. The fixture
DOES NOT demonstrate recovery after ordinary execution expiry.
"""

import copy
from datetime import timedelta
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_authority_fixtures import authority_fixture
from announcement_lifecycle_fixtures import CLEANUP, INTERIOR, SETUP
from announcement_provenance_fixtures import at, evidence_index
from announcement_resource_fixtures import repin_authority
from announcement_runtime_authority import check_authority_graph_claims
from announcement_runtime_evidence import canonical_digest
from announcement_runtime_lifecycle import check_lifecycle_phase_claims
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_recovery import RECOVERY_PLAN_REF, check_recovery_ownership_claims
from announcement_runtime_schema import Refusal, utc
from test_announcement_runtime_recovery_fences import repin_recovery_scope
from test_announcement_runtime_recovery_grant import GRANT, SCOPE
from test_announcement_runtime_recovery_ownership import ownership_fixture


CLOCKS = ("00:00:10", "00:02:00", "00:05:00", "00:15:00", "00:12:00", "00:22:00")
PRIORS = ("absent", "present")


def paired_fixture(prior="absent"):
    """Align existing inert graphs without changing any production predicate."""
    receipt, records, now = ownership_fixture(prior)
    ordinary, _, _ = authority_fixture()
    # Recovery fixtures intentionally start with the minimal lifecycle receipt.
    # Use the ordinary graph's exact source/build/document identities here.
    for field in ("scope", "build", "fixtures"):
        receipt[field] = copy.deepcopy(ordinary[field])
    for field in ("identity_map", "inventory_manifest"):
        receipt["resources"][field] = copy.deepcopy(ordinary["resources"][field])
        records["runtime-resource-plan"]["data"][field] = copy.deepcopy(receipt["resources"][field])
    receipt["resources"]["plan"]["revision"] = ordinary["resources"]["plan"]["revision"]
    receipt["attempt"]["checkpoints"] = copy.deepcopy(ordinary["attempt"]["checkpoints"])
    authority = receipt["authority"]
    pairs = list(zip(receipt["attempt"]["checkpoints"], authority["installation_approval_refs"]))
    for (checkpoint, install_ref), clock in zip(pairs, CLOCKS):
        checkpoint["checked_at"] = at(clock)
        fence = records[checkpoint["mutation_fence_ref"]]["data"]
        fence.update(observed_at=at(clock), valid_until=(utc(at(clock)) + timedelta(
            seconds=30)).strftime("%Y-%m-%dT%H:%M:%SZ"))
        install = records[install_ref]["data"]
        install["checked_at"] = at(clock)
        for record in (checkpoint, install):
            for role in ("controller", "verifier"):
                data = records[record[role + "_read_ref"]]["data"]
                data["claim"]["read_at"] = at(clock)
                history = records[data["history_ref"]]["data"]
                history["ended_at"] = at(clock)
                data["claim"]["history_sha256"] = canonical_digest(history)
    pairs.sort(key=lambda pair: pair[0]["checked_at"])
    receipt["attempt"]["checkpoints"] = [pair[0] for pair in pairs]
    authority["installation_approval_refs"] = [pair[1] for pair in pairs]
    receipt["control"]["installation"]["authority_receipt_refs"] = copy.deepcopy(
        authority["installation_approval_refs"])
    records[authority["execution"]["ref"]]["data"]["installation_approval_refs"] = copy.deepcopy(
        authority["installation_approval_refs"])
    for operation, checkpoint_ref in ((SETUP, "checkpoint-1"), (INTERIOR, "checkpoint-3")):
        records["transition-" + operation]["data"]["claim"]["authority_ref"] = checkpoint_ref
    receipt["lifecycle"]["setup"][0]["authority_ref"] = "checkpoint-1"
    scope = records["runtime-authority-scope"]["data"]
    scope.update(scope=copy.deepcopy(receipt["scope"]), control=copy.deepcopy(receipt["control"]),
                 components_manifest=copy.deepcopy(receipt["build"]["components_manifest"]),
                 inventory_manifest=copy.deepcopy(receipt["resources"]["inventory_manifest"]),
                 image_digest=receipt["build"]["image_digest"])
    plan = records[RECOVERY_PLAN_REF]["data"]
    plan.update(contract=copy.deepcopy(receipt["scope"]["contract"]),
                candidate=copy.deepcopy(receipt["scope"]["candidate"]))
    # Pin the plan/resource bytes before binding the ordinary scope, then bind
    # recovery to that scope. No hash includes itself or grants authority.
    repin_recovery_scope(receipt, records)
    repin_authority(receipt, records)
    records[SCOPE]["data"]["runtime_scope_sha256"] = authority["scope_sha256"]
    repin_recovery_scope(receipt, records)
    pin = receipt["resources"]["plan"]
    records[pin["ref"]]["data"] = {key: value for key, value in pin.items() if key != "ref"}
    return receipt, records, now


def ordinary_prefix(receipt):
    """Compare only the three ordinary operations; never claim full accounting.

    Keep the original exact authority/resource scope and all final checkpoints.
    This deliberately partial projection is only a positive control that the
    ordinary graph itself works. The full paired receipt below MUST refuse.
    """
    result = copy.deepcopy(receipt)
    for field in ("actual_effects", "expected_effects"):
        result["observations"][field] = [effect for effect in result["observations"][field]
                                         if effect["operation_id"] != CLEANUP]
    return result


class RecoveryBoundary(unittest.TestCase):
    def test_shared_scope_supports_ordinary_prefix_and_separate_recovery_only(self):
        for prior in PRIORS:
            with self.subTest(prior=prior):
                receipt, records, now = paired_fixture(prior)
                self.assertLessEqual(len(records), 256)
                self.assertIsNone(check_authority_graph_claims(
                    ordinary_prefix(receipt), evidence_index(records), now))
                self.assertIsNone(check_recovery_ownership_claims(
                    receipt, evidence_index(records), now))
                self.assertEqual(records[SCOPE]["data"]["runtime_scope_sha256"],
                                 receipt["authority"]["scope_sha256"])
                self.assertNotEqual(records[GRANT]["data"]["epoch"], receipt["authority"]["epoch"])
                self.assertFalse(receipt["promotable"])
                with self.assertRaisesRegex(Refusal, "unsupported$"):
                    runtime_preflight()

    def test_full_authority_graph_reaches_named_cleanup_boundary(self):
        for prior in PRIORS:
            with self.subTest(prior=prior):
                receipt, records, now = paired_fixture(prior)
                with self.assertRaisesRegex(Refusal, "transition_authority$"):
                    check_authority_graph_claims(receipt, evidence_index(records), now)
                with self.assertRaisesRegex(Refusal, "unsupported_recovery_authority$"):
                    check_lifecycle_phase_claims(receipt, evidence_index(records), now)

    def test_recovery_grant_cannot_replace_expired_ordinary_lease(self):
        for prior in PRIORS:
            with self.subTest(prior=prior):
                receipt, records, now = paired_fixture(prior)
                receipt["authority"]["execution"]["expires_at"] = at("00:22:00")
                # A coherent, current recovery graph is deliberately insufficient.
                self.assertIsNone(check_recovery_ownership_claims(receipt, evidence_index(records), now))
                with self.assertRaisesRegex(Refusal, "expiry$"):
                    check_authority_graph_claims(ordinary_prefix(receipt), evidence_index(records), now)

    def test_exactly_sixty_second_final_reads_do_not_extend_their_lifetime(self):
        for prior in PRIORS:
            with self.subTest(prior=prior):
                receipt, records, now = paired_fixture(prior)
                self.assertIsNone(check_authority_graph_claims(ordinary_prefix(receipt),
                    evidence_index(records), now + timedelta(seconds=60)))
                with self.assertRaisesRegex(Refusal, "stale_checkpoint$"):
                    check_authority_graph_claims(ordinary_prefix(receipt), evidence_index(records),
                                                now + timedelta(seconds=61))

    def test_missing_ordinary_installation_still_refuses_with_valid_recovery(self):
        for prior in PRIORS:
            with self.subTest(prior=prior):
                receipt, records, now = paired_fixture(prior)
                ref = next(ref for checkpoint, ref in zip(receipt["attempt"]["checkpoints"],
                    receipt["authority"]["installation_approval_refs"]) if checkpoint["phase"] == "final")
                del records[ref]
                with self.assertRaisesRegex(Refusal, "unresolved_reference$"):
                    check_authority_graph_claims(ordinary_prefix(receipt), evidence_index(records), now)


if __name__ == "__main__":
    unittest.main()
