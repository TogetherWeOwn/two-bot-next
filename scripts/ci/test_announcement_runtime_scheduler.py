"""Offline cadence/reader/poll adversaries, not staging hold/allow observations."""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_provenance_fixtures import at, evidence_index
from announcement_scheduler_fixtures import scheduler_fixture, repin_scheduler
from announcement_runtime_authority import check_authority_graph_claims
from announcement_runtime_evidence import SCHEDULER_RECORDS
from announcement_runtime_provenance import check_provenance_claims
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_scheduler import check_scheduler_graph_claims
from announcement_runtime_schema import Refusal


def data(records, ref="scheduler-plan-ref"):
    return records[ref]["data"]


class SchedulerGraph(unittest.TestCase):
    def check(self, receipt, records, now):
        return check_scheduler_graph_claims(receipt, evidence_index(records), now)

    def reject(self, mutate, code=None, *, repin=True):
        receipt, records, now = scheduler_fixture()
        mutate(receipt, records)
        if repin:
            repin_scheduler(receipt, records)
        with self.assertRaisesRegex(Refusal, code or "announcement evidence refused"):
            self.check(receipt, records, now)

    def test_consistent_claims_and_same_replay_snapshot_never_admit_runtime(self):
        receipt, records, now = scheduler_fixture()
        index = evidence_index(records)
        self.assertIsNone(check_scheduler_graph_claims(receipt, index, now))
        self.assertIsNone(check_authority_graph_claims(receipt, index, now))
        self.assertIsNone(check_provenance_claims(receipt, index, now))
        index.require_all_used()
        with self.assertRaisesRegex(Refusal, "unsupported"):
            runtime_preflight()

    def test_every_scheduler_kind_has_closed_nonempty_shape(self):
        _, records, _ = scheduler_fixture()
        examples = {r["kind"]: ref for ref, r in records.items() if r["kind"] in SCHEDULER_RECORDS}
        self.assertEqual(set(examples), set(SCHEDULER_RECORDS))
        for kind, ref in examples.items():
            for value in (None, {}, {**data(records, ref), "extra": True}):
                with self.subTest(kind=kind, value=type(value)):
                    self.reject(lambda r, s: s[ref].update(data=value), repin=False)

    def test_missing_wrong_kind_or_other_attempt_evidence_refuses(self):
        for ref in ("scheduler-plan-ref", "activation-fence-ref", "reader-off", "poll-on"):
            with self.subTest(ref=ref):
                self.reject(lambda r, s: s.pop(ref), "unresolved_reference", repin=False)
                self.reject(lambda r, s: s[ref].update(kind="document"), repin=False)
                self.reject(lambda r, s: s[ref]["run"].update(attempt=2), "evidence_attempt", repin=False)

    def test_plan_and_inner_records_are_content_bound(self):
        for ref, changes in (("scheduler-plan-ref", {"boot_poll_seconds": 301}),
                             ("reader-off", {"row_version": "90"}),
                             ("poll-on", {"effective_poll_seconds": 61}),
                             ("activation-fence-ref", {"registration": "absent"})):
            with self.subTest(ref=ref):
                self.reject(lambda r, s: data(s, ref).update(changes), "scheduler_content", repin=False)

    def test_exact_candidate_and_fixture_identity_required(self):
        for changes in ({"feed_ref": "delivery"}, {"item_ref": "other-item"},
                        {"setting_ref": "rsvp"}, {"candidate": {"commit": "a" * 40,
                                                                "tree": "b" * 40}}):
            self.reject(lambda r, s: data(s).update(changes), "scheduler_fixture")

    def test_activation_is_not_an_application_flag_or_missing_job(self):
        for changes in ({"boundary": "application_only"}, {"boundary": "unknown"},
                        {"registration": "absent"}, {"registration": "unknown"},
                        {"execution_ref": "other-execution"}, {"nonce_ref": "old-nonce"},
                        {"enforcement_epoch": "999"}, {"boot_announcements": False}):
            self.reject(lambda r, s: data(s, "activation-fence-ref").update(changes))

    def test_activation_covers_startup_through_attempt_without_extending_lease(self):
        for changes in ({"observed_at": at("00:03:01")}, {"observed_at": "2025-12-31T23:59:59Z"},
                        {"valid_until": at("00:23:59")}, {"valid_until": at("00:40:01")}):
            self.reject(lambda r, s: data(s, "activation-fence-ref").update(changes),
                        "scheduler_activation_window")

    def test_four_exact_unique_ordered_windows_and_boundaries(self):
        self.reject(lambda r, s: data(s)["windows"].pop())
        self.reject(lambda r, s: data(s)["windows"].reverse(), "scheduler_window")
        self.reject(lambda r, s: data(s)["windows"][1].update(boundary_ref="boundary-off"),
                    "scheduler_inventory")
        self.reject(lambda r, s: r["fixtures"]["setting_boundaries"].append(copy.deepcopy(
            r["fixtures"]["setting_boundaries"][0])), "scheduler_inventory")

    def test_gaps_overlaps_short_windows_and_uncovered_tail_refuse(self):
        def change_window(receipt, records, index, changes):
            window = data(records)["windows"][index]
            window.update(changes)
            # Keep cases contained so continuity/tail, not case containment,
            # decides this adversary. The separate case test covers overruns.
            if "ended_at" in changes:
                for case in receipt["observations"]["cases"]:
                    if case["phase"] == window["phase"]:
                        case["ended_at"] = changes["ended_at"]
        for changes in ({"started_at": at("00:05:01")}, {"ended_at": at("00:05:59")},
                        {"ended_at": at("00:07:01")}, {"ended_at": at("00:06:59")}):
            self.reject(lambda r, s: change_window(r, s, 0, changes), "scheduler_window")
        self.reject(lambda r, s: change_window(r, s, -1, {"ended_at": at("00:14:59")}),
                    "scheduler_window")

    def test_observation_must_follow_rollout_and_stay_within_twenty_minutes(self):
        for changes in ({"observation_started_at": at("00:03:59")},
                        {"observation_ended_at": at("00:25:01")},
                        {"observation_ended_at": at("00:04:59")}):
            self.reject(lambda r, s: r["attempt"].update(changes), "scheduler_window")

    def test_store_cadence_is_used_not_rounded_to_boot_multiple(self):
        self.reject(lambda r, s: data(s, "poll-on").update(effective_poll_seconds=300),
                    "scheduler_poll")
        self.reject(lambda r, s: data(s, "reader-off")["cadence"].update(stored_seconds=300),
                    "scheduler_reader")
        self.reject(lambda r, s: data(s)["cadence"].update(stored_seconds=86400), "scheduler_window")

    def test_absent_cadence_can_use_unchanged_boot_but_not_null_or_changed_interval(self):
        receipt, records, now = scheduler_fixture()
        data(records).update(cadence={"source": "boot", "row_presence": "absent"}, boot_poll_seconds=60)
        for ref, record in records.items():
            if record["kind"] == "stored_setting_read":
                data(records, ref)["cadence"] = copy.deepcopy(data(records)["cadence"])
        repin_scheduler(receipt, records)
        self.assertIsNone(self.check(receipt, records, now))
        for changes in ({"source": "boot", "row_presence": "null"},
                        {"source": "stored", "stored_seconds": 59},
                        {"source": "stored", "stored_seconds": 86401},
                        {"source": "stored", "stored_seconds": True},
                        {"source": "stored", "stored_seconds": 60.0}):
            self.reject(lambda r, s: data(s).update(cadence=changes))

    def test_absent_null_or_unknown_announcement_row_is_never_off_proof(self):
        for presence in ("absent", "null", "unknown"):
            self.reject(lambda r, s: data(s, "reader-off").update(row_presence=presence),
                        "scheduler_reader")

    def test_setting_boundary_exact_phase_key_value_item_and_read_time(self):
        for changes in ({"phase": "on"}, {"resource_ref": "feed"}, {"stored_value": "1"},
                        {"eligible_item_ref": "other-item"}, {"read_at": at("00:04:59")}):
            self.reject(lambda r, s: r["fixtures"]["setting_boundaries"][0].update(changes),
                        "scheduler_boundary")

    def test_changed_value_needs_new_row_version_and_snapshot_generation(self):
        for changes in ({"row_version": "1"}, {"snapshot_generation": "10"}):
            self.reject(lambda r, s: r["fixtures"]["setting_boundaries"][1].update(changes),
                        "scheduler_generation")
        self.reject(lambda r, s: r["fixtures"]["setting_boundaries"][2].update(row_version="3"),
                    "scheduler_generation")

    def test_reader_matches_stored_row_snapshot_cadence_and_boundary(self):
        for changes in ({"boundary_ref": "boundary-on"}, {"setting_ref": "rsvp"},
                        {"stored_value": "1"}, {"snapshot_value": "1"}, {"row_version": "2"},
                        {"snapshot_generation": "9"}, {"read_at": at("00:05:01")}):
            self.reject(lambda r, s: data(s, "reader-off").update(changes), "scheduler_reader")

    def test_poll_requires_eligible_item_exact_snapshot_and_full_normal_cadence(self):
        for changes in ({"boundary_ref": "boundary-off"}, {"feed_ref": "delivery"},
                        {"item_ref": "other-item"}, {"setting_ref": "rsvp"},
                        {"snapshot_generation": "10"}, {"observed_at": at("00:07:59")},
                        {"observed_at": at("00:09:01")}, {"eligible_item": False},
                        {"decision": "suppressed"}, {"decision": "unknown"}):
            self.reject(lambda r, s: data(s, "poll-on").update(changes))

    def test_off_and_final_off_cannot_borrow_on_decisions_or_poll_receipts(self):
        self.reject(lambda r, s: data(s, "poll-off").update(decision="admitted"), "scheduler_poll")
        self.reject(lambda r, s: data(s, "poll-final_off").update(decision="admitted"), "scheduler_poll")
        self.reject(lambda r, s: data(s)["windows"][1].update(
            reader=copy.deepcopy(data(s)["windows"][0]["reader"])), "scheduler_inventory")

    def test_served_pair_matches_candidate_image_worker_and_precedes_read(self):
        for changes in ({"served_ref": "served-before"}, {"served_ref": "not-listed"}):
            self.reject(lambda r, s: data(s, "poll-on").update(changes), "scheduler_served")
        for changes in ({"external_revision": "a" * 40}, {"reported_revision": "b" * 40},
                        {"image_digest": "sha256:" + "0" * 64},
                        {"worker_version_id": "00000000-0000-4000-8000-000000000000"},
                        {"observed_at": at("00:05:01")}):
            self.reject(lambda r, s: r["served"]["samples"][0].update(changes), "scheduler_served")

    def test_each_case_uses_its_phase_boundary_served_pair_and_window(self):
        for changes in ({"gate_ref": "boundary-on"}, {"served_ref": "served-two"},
                        {"started_at": at("00:04:59")}, {"ended_at": at("00:07:01")}):
            self.reject(lambda r, s: r["observations"]["cases"][0].update(changes), "scheduler_case")

    def test_replay_and_final_off_cases_cannot_be_omitted(self):
        def mutate(receipt, records):
            receipt["observations"]["cases"] = [case for case in receipt["observations"]["cases"]
                                               if case["phase"] != "replay"]
            receipt["fixtures"]["case_ids"] = [case["case_id"] for case in
                                               receipt["observations"]["cases"]]
        self.reject(mutate, "scheduler_cases")

    def test_diagnostics_do_not_echo_private_scheduler_reference(self):
        receipt, records, now = scheduler_fixture()
        data(records, "poll-on")["item_ref"] = "private-fixture-sentinel"
        repin_scheduler(receipt, records)
        with self.assertRaises(Refusal) as raised:
            self.check(receipt, records, now)
        self.assertNotIn("private-fixture-sentinel", str(raised.exception))


if __name__ == "__main__":
    unittest.main()
