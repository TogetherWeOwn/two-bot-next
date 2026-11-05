"""Offline scheduler/reader/poll claim comparisons, never stored-data proof.

No timer, setting mutation, poll execution, live observation or activation
adapter exists here. A coherent fabricated graph is not evidence that C read a
stored row, registered a job or suppressed/sent anything. External containment,
source/transitive inventories, effect semantics and lifecycle remain mandatory.
"""

from datetime import timedelta

from announcement_runtime_evidence import canonical_digest
from announcement_runtime_inputs import INPUT_PLAN_REF, check_input_graph_claims
from announcement_runtime_schema import require, utc


PHASE_VALUES = (("off", "0", "suppressed"), ("on", "1", "admitted"),
                ("replay", "1", "reconciled"), ("final_off", "0", "suppressed"))


def check_scheduler_graph_claims(receipt, evidence, now):
    """Resolve content-bound normal cadence, explicit stored bits and poll windows.

    The fixed four-window profile does not permit a new interval-setting write,
    grace, delayed readiness sample or lease extension. It refuses an observation
    budget too short for the unchanged declared cadence. This is not a complete
    verifier, and no reference, hash or application gate grants a capability.
    """
    check_input_graph_claims(receipt, evidence, now)
    fixtures, attempt = receipt["fixtures"], receipt["attempt"]
    plan = evidence.resolve(fixtures["scheduler_plan_ref"], "scheduler_plan_content")
    inputs = evidence.resolve(INPUT_PLAN_REF, "input_plan_content")
    require(canonical_digest(plan) == inputs["scheduler_plan_sha256"], "scheduler_content")
    require(plan["candidate"] == receipt["scope"]["candidate"] and
            all(plan[field] == fixtures[field] for field in
                ("feed_ref", "item_ref", "setting_ref")), "scheduler_fixture")

    def content(pin, kind):
        value = evidence.resolve(pin["ref"], kind)
        require(canonical_digest(value) == pin["sha256"], "scheduler_content")
        return value

    activation = content(plan["activation_fence"], "feed_activation_fence")
    execution = receipt["authority"]["execution"]
    require(plan["activation_fence"]["ref"] == fixtures["activation_fence_ref"] and
            activation["execution_ref"] == execution["ref"] and
            activation["nonce_ref"] == execution["nonce_ref"] and
            activation["enforcement_epoch"] == attempt["enforcement_epoch"] and
            activation["boundary"] == "external_atomic" and
            activation["registration"] == "registered", "scheduler_activation")
    require(utc(attempt["started_at"]) <= utc(activation["observed_at"]) <=
            utc(receipt["rollout"]["started_at"]) and
            utc(attempt["ended_at"]) <= utc(activation["valid_until"]) <=
            utc(execution["expires_at"]), "scheduler_activation_window")

    cadence = plan["cadence"]
    seconds = (cadence["stored_seconds"] if cadence["source"] == "stored" else
               plan["boot_poll_seconds"])
    observation_start, observation_end = (utc(attempt[field]) for field in
        ("observation_started_at", "observation_ended_at"))
    require(utc(receipt["rollout"]["ended_at"]) <= observation_start < observation_end <=
            utc(attempt["ended_at"]) <= now and
            observation_end - observation_start <= timedelta(minutes=20), "scheduler_window")
    boundaries = {x["ref"]: x for x in fixtures["setting_boundaries"]}
    windows = plan["windows"]
    require(len(boundaries) == len(fixtures["setting_boundaries"]) == len(windows) and
            len({x["boundary_ref"] for x in windows}) == len(windows) and
            {x["boundary_ref"] for x in windows} == boundaries.keys(), "scheduler_inventory")
    served = {x["ref"]: x for x in receipt["served"]["samples"]}
    require(len(served) == len(receipt["served"]["samples"]), "scheduler_served")
    require({phase for phase, _, _ in PHASE_VALUES} <=
            {case["phase"] for case in receipt["observations"]["cases"]}, "scheduler_cases")
    used_readers, used_polls = set(), set()
    previous_end = observation_start
    previous_version = previous_generation = 0
    previous_value = None
    for window, (phase, value, decision) in zip(windows, PHASE_VALUES):
        start, end = utc(window["started_at"]), utc(window["ended_at"])
        require(window["phase"] == phase and start == previous_end and
                start + timedelta(seconds=seconds) <= end <= observation_end, "scheduler_window")
        previous_end = end
        boundary = boundaries[window["boundary_ref"]]
        require(boundary["phase"] == phase and boundary["resource_ref"] == fixtures["setting_ref"] and
                boundary["eligible_item_ref"] == fixtures["item_ref"] and
                boundary["stored_value"] == value and utc(boundary["read_at"]) == start,
                "scheduler_boundary")
        version, generation = int(boundary["row_version"]), int(boundary["snapshot_generation"])
        require((generation > previous_generation and version > previous_version
                 if value != previous_value else
                 generation >= previous_generation and version == previous_version),
                "scheduler_generation")
        previous_version, previous_generation, previous_value = version, generation, value
        reader_pin, poll_pin = window["reader"], window["poll"]
        require(reader_pin["ref"] == boundary["reader_receipt_ref"] and
                poll_pin["ref"] == boundary["poll_receipt_ref"] and
                reader_pin["ref"] not in used_readers and poll_pin["ref"] not in used_polls,
                "scheduler_inventory")
        used_readers.add(reader_pin["ref"])
        used_polls.add(poll_pin["ref"])
        reader = content(reader_pin, "stored_setting_read")
        poll = content(poll_pin, "feed_poll")
        require(reader["boundary_ref"] == boundary["ref"] and
                reader["setting_ref"] == fixtures["setting_ref"] and
                reader["row_presence"] == "stored_string" and
                reader["stored_value"] == reader["snapshot_value"] == value and
                all(reader[field] == boundary[field] for field in
                    ("row_version", "snapshot_generation", "read_at")) and
                reader["cadence"] == cadence, "scheduler_reader")
        require(poll["boundary_ref"] == boundary["ref"] and
                all(poll[field] == fixtures[field] for field in
                    ("feed_ref", "item_ref", "setting_ref")) and
                poll["snapshot_generation"] == boundary["snapshot_generation"] and
                poll["effective_poll_seconds"] == seconds and poll["decision"] == decision and
                start + timedelta(seconds=seconds) <= utc(poll["observed_at"]) <= end,
                "scheduler_poll")
        require(reader["served_ref"] == poll["served_ref"] and reader["served_ref"] in served,
                "scheduler_served")
        sample = served[reader["served_ref"]]
        require(sample["external_revision"] == sample["reported_revision"] ==
                plan["candidate"]["commit"] and
                sample["image_digest"] == receipt["build"]["image_digest"] and
                sample["worker_version_id"] == receipt["rollout"]["worker_version_id"] and
                utc(sample["observed_at"]) <= start, "scheduler_served")
        # Interactions cannot borrow an on snapshot while an off window is active.
        for case in receipt["observations"]["cases"]:
            if case["phase"] == phase:
                require(case["gate_ref"] == boundary["ref"] and
                        case["served_ref"] == reader["served_ref"] and
                        start <= utc(case["started_at"]) <= utc(case["ended_at"]) <= end,
                        "scheduler_case")
    require(previous_end == observation_end, "scheduler_window")
