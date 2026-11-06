"""SYNTHETIC snapshot reuse; no authority acquisition or lifecycle qualification."""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_provenance_fixtures import evidence_index
from announcement_recovery_transition_fixtures import recovery_transition_fixture, repin
from announcement_setting_fixtures import deduplicate_authority_snapshots
from announcement_runtime_lifecycle import (
    check_lifecycle_ledger_claims, check_lifecycle_phase_claims,
)
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_schema import Refusal
from announcement_runtime_transitions import check_transition_claims
from test_announcement_runtime_recovery_reads import CKPT


class TemporalCompaction(unittest.TestCase):
    def fixture(self):
        receipt, records, now = recovery_transition_fixture()
        deduplicate_authority_snapshots(receipt, records)
        repin(receipt, records)
        return receipt, records, now

    def compare(self, receipt, records, now):
        index = evidence_index(records)
        self.assertIsNone(check_transition_claims(receipt, index, now))
        self.assertIsNone(check_lifecycle_ledger_claims(receipt, index, now))
        self.assertIsNone(check_lifecycle_phase_claims(receipt, index, now))
        index.require_all_used()

    def test_public_transition_and_lifecycle_layers_still_compose_without_admission(self):
        receipt, records, now = self.fixture()
        self.assertLessEqual(len(records), 256)
        self.assertIsNone(self.compare(receipt, records, now))
        with self.assertRaisesRegex(Refusal, "unsupported$"):
            runtime_preflight()

    def test_extra_well_formed_sidecar_is_not_hidden_by_compaction(self):
        receipt, records, now = self.fixture()
        records["unreferenced-owner"] = copy.deepcopy(records["resource-owner"])
        records["unreferenced-owner"]["ref"] = "unreferenced-owner"
        self.assertLessEqual(len(records), 256)
        with self.assertRaisesRegex(Refusal, "unreferenced_evidence$"):
            self.compare(receipt, records, now)

    def test_no_effect_transaction_checkpoint_fence_or_lifecycle_time_changes(self):
        receipt, records, _ = recovery_transition_fixture()
        before_receipt, before_records = copy.deepcopy((receipt, records))
        deduplicate_authority_snapshots(receipt, records)
        repin(receipt, records)
        for field in ("observations", "lifecycle", "attempt"):
            self.assertEqual(receipt[field], before_receipt[field])
        retained_kinds = ("db_transaction", "db_transition", "mutation_fence",
                          "recovery_mutation_fence", "recovery_checkpoints", "ownership_match")
        before = {ref: record for ref, record in before_records.items()
                  if record["kind"] in retained_kinds}
        after = {ref: record for ref, record in records.items()
                 if record["kind"] in retained_kinds}
        self.assertEqual(after, before)
        self.assertEqual(receipt["lifecycle"]["setup"][0]["observed_at"],
                         "2026-01-01T00:02:00Z")
        self.assertEqual({claim["observed_at"] for claim in receipt["lifecycle"]["cleanup"]},
                         {"2026-01-01T00:20:30Z"})

    def test_only_identical_histories_and_ordinary_installation_reads_are_removed(self):
        receipt, records, _ = recovery_transition_fixture()
        before = copy.deepcopy(records)
        deduplicate_authority_snapshots(receipt, records)
        removed = set(before) - set(records)
        self.assertTrue(removed)
        for ref in removed:
            record = before[ref]
            self.assertIn(record["kind"], ("authority_history", "authority_read"))
            data = copy.deepcopy(record["data"])
            if record["kind"] == "authority_read":
                self.assertTrue(ref.startswith("install-"))
                data["claim"].pop("ref")
                data["history_ref"] = before[data["history_ref"]]["data"]
            candidates = []
            for survivor_ref, survivor in records.items():
                if survivor["kind"] != record["kind"]:
                    continue
                candidate = copy.deepcopy(before[survivor_ref]["data"])
                if survivor["kind"] == "authority_read":
                    if not survivor_ref.startswith("install-"):
                        continue
                    candidate["claim"].pop("ref")
                    candidate["history_ref"] = before[candidate["history_ref"]]["data"]
                candidates.append(candidate)
            self.assertIn(data, candidates)
        self.assertEqual(set(records) - set(before), set())

    def test_distinct_clocks_consumers_runtime_reads_and_recovery_reads_survive(self):
        receipt, records, _ = recovery_transition_fixture()
        reads = {ref: copy.deepcopy(record["data"]["claim"])
                 for ref, record in records.items()
                 if record["kind"] in ("authority_read", "recovery_read")}
        deduplicate_authority_snapshots(receipt, records)
        for ref, claim in reads.items():
            if not ref.startswith("install-"):
                self.assertIn(ref, records)
                self.assertEqual(records[ref]["data"]["claim"]["read_at"], claim["read_at"])
            else:
                candidates = [record["data"]["claim"] for record in records.values()
                              if record["kind"] == "authority_read" and
                              record["data"]["claim"]["ref"].startswith("install-")]
                self.assertTrue(any(all(candidate[field] == claim[field] for field in
                    ("consumer", "read_at", "epoch", "scope_sha256")) for candidate in candidates))
        self.assertEqual(len(receipt["authority"]["reads"]), 12)
        self.assertEqual(len({read["ref"] for read in receipt["authority"]["reads"]}), 12)

    def test_recovery_installation_reads_cannot_be_reused_as_ordinary_installation_reads(self):
        receipt, records, now = self.fixture()
        del records["recovery-install-read-cleanup-controller"]
        with self.assertRaisesRegex(Refusal, "unresolved_reference$"):
            check_transition_claims(receipt, evidence_index(records), now)

    def test_recovery_runtime_reads_remain_distinct_from_ordinary_reads(self):
        receipt, records, now = self.fixture()
        checkpoint = records[CKPT]["data"]["checkpoints"][0]
        checkpoint["controller_read_ref"] = receipt["attempt"]["checkpoints"][0]["controller_read_ref"]
        with self.assertRaisesRegex(Refusal, "reference_kind$"):
            check_transition_claims(receipt, evidence_index(records), now)

    def test_deduplication_is_idempotent_after_complete_repinning(self):
        receipt, records, now = self.fixture()
        before = copy.deepcopy((receipt, records))
        deduplicate_authority_snapshots(receipt, records)
        repin(receipt, records)
        self.assertEqual((receipt, records), before)
        self.assertIsNone(check_transition_claims(receipt, evidence_index(records), now))


if __name__ == "__main__":
    unittest.main()
