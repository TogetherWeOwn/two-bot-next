"""Fabricated scheduler claims; not real stored rows, poll observations or fences."""

import copy

from announcement_input_fixtures import input_fixture, repin_inputs
from announcement_provenance_fixtures import at, envelope
from announcement_runtime_evidence import canonical_digest
from announcement_runtime_scheduler import PHASE_VALUES


def repin_scheduler(receipt, records):
    plan = records[receipt["fixtures"]["scheduler_plan_ref"]]["data"]
    pins = [plan["activation_fence"]]
    for window in plan["windows"]:
        pins.extend((window["reader"], window["poll"]))
    for pin in pins:
        if pin["ref"] in records:
            pin["sha256"] = canonical_digest(records[pin["ref"]]["data"])
    records["runtime-input-plan"]["data"]["scheduler_plan_sha256"] = canonical_digest(plan)
    repin_inputs(receipt, records)


def scheduler_fixture():
    receipt, records, now = input_fixture()

    def add(ref, kind, data):
        records[ref] = envelope(ref, kind, copy.deepcopy(data))
        return {"ref": ref, "sha256": canonical_digest(data)}

    # The extended final-off case needs an independently claimed trailing sample,
    # matching the existing provenance check's before/after bracketing rule.
    for sample in receipt["served"]["samples"]:
        if sample["ref"] == "served-two":
            sample["observed_at"] = at("00:15:00")
    for record in records.values():
        if record["kind"] == "external_served" and record["data"]["sample"]["ref"] == "served-two":
            record["data"]["sample"]["observed_at"] = at("00:15:00")
    fixtures, attempt = receipt["fixtures"], receipt["attempt"]
    plan = {"candidate": copy.deepcopy(receipt["scope"]["candidate"]),
        "feed_ref": "feed", "item_ref": fixtures["item_ref"], "setting_ref": "setting",
        "boot_poll_seconds": 300, "cadence": {"source": "stored", "stored_seconds": 60},
        "activation_fence": add(fixtures["activation_fence_ref"], "feed_activation_fence", {
            "execution_ref": "execution-ref", "nonce_ref": "one-shot-nonce",
            "enforcement_epoch": attempt["enforcement_epoch"],
            "observed_at": at("00:02:50"), "valid_until": at("00:40:00"),
            "boundary": "external_atomic", "boot_announcements": True,
            "job": "feeds", "registration": "registered"}), "windows": []}
    fixtures["setting_boundaries"] = []
    times = [("00:05:00", "00:07:00", "00:06:00"),
             ("00:07:00", "00:09:00", "00:08:00"),
             ("00:09:00", "00:11:00", "00:10:00"),
             ("00:11:00", "00:15:00", "00:12:00")]
    cases = receipt["observations"]["cases"]
    example = copy.deepcopy(cases[0])
    for number, ((phase, value, decision), (start, end, poll_time)) in enumerate(
            zip(PHASE_VALUES, times), 1):
        boundary_ref, reader_ref, poll_ref = (prefix + phase for prefix in
                                             ("boundary-", "reader-", "poll-"))
        version = str((1, 2, 2, 3)[number - 1])
        generation = str((10, 11, 11, 12)[number - 1])
        fixtures["setting_boundaries"].append({"ref": boundary_ref, "phase": phase,
            "resource_ref": "setting", "stored_value": value, "row_version": version,
            "snapshot_generation": generation, "read_at": at(start),
            "reader_receipt_ref": reader_ref, "poll_receipt_ref": poll_ref,
            "eligible_item_ref": fixtures["item_ref"]})
        reader = add(reader_ref, "stored_setting_read", {"boundary_ref": boundary_ref,
            "setting_ref": "setting", "row_presence": "stored_string", "stored_value": value,
            "row_version": version, "snapshot_value": value, "snapshot_generation": generation,
            "read_at": at(start), "served_ref": "served-one", "cadence": plan["cadence"]})
        poll = add(poll_ref, "feed_poll", {"boundary_ref": boundary_ref, "feed_ref": "feed",
            "item_ref": fixtures["item_ref"], "setting_ref": "setting",
            "snapshot_generation": generation, "served_ref": "served-one",
            "observed_at": at(poll_time), "eligible_item": True,
            "effective_poll_seconds": 60, "decision": decision})
        plan["windows"].append({"phase": phase, "boundary_ref": boundary_ref,
            "reader": reader, "poll": poll, "started_at": at(start), "ended_at": at(end)})
        case = copy.deepcopy(example)
        case.update(case_id="feed-" + phase, phase=phase, input_ref=fixtures["scheduler_plan_ref"],
                    resource_refs=["feed", "delivery", "setting", "feed-channel"],
                    gate_ref=boundary_ref, served_ref="served-one",
                    started_at=at(start), ended_at=at(end))
        cases.append(case)
        for interaction in cases:
            if interaction["phase"] == phase:
                interaction.update(gate_ref=boundary_ref, served_ref="served-one",
                                   started_at=at(start), ended_at=at(end))
    fixtures["case_ids"] = [case["case_id"] for case in cases]
    add(fixtures["scheduler_plan_ref"], "scheduler_plan_content", plan)
    repin_scheduler(receipt, records)
    return receipt, records, now
