"""Offline stored setting/value/reader comparisons, NOT persistence or CAS proof.

Only absent or explicit JSON string-bit prior values have a closed projection
here. Unknown/null/other prior values refuse, never fall back to boot or become
an inferred restoration target. SQL rows, signed CAS tokens, settings audit and
revision triggers, actual readers and cleanup authority still require independent
external evidence. runtime_preflight() remains unconditionally unsupported.
"""

from announcement_runtime_evidence import canonical_digest
from announcement_runtime_inputs import INPUT_PLAN_REF
from announcement_runtime_provenance import _index, _unique_refs
from announcement_runtime_scheduler import check_scheduler_graph_claims
from announcement_runtime_schema import require, utc
from announcement_runtime_transitions import check_transition_claims


SETTING_PLAN_REF = "runtime-setting-plan"
SETTING_PHASES = ("baseline", "enable", "disable", "restore")


def check_setting_claims(receipt, evidence, now):
    """Link four owned setting mutations to value projections and actual readers.

    This composes necessary offline claims, not a complete verifier. No supplied
    projection, witness, hash, authority record or coherent fabricated graph can
    grant runtime admission. No database or network is accessed here.
    """
    check_transition_claims(receipt, evidence, now)
    check_scheduler_graph_claims(receipt, evidence, now)
    fixtures, attempt = receipt["fixtures"], receipt["attempt"]
    inputs = evidence.resolve(INPUT_PLAN_REF, "input_plan_content")
    plan = evidence.resolve(SETTING_PLAN_REF, "setting_plan_content")
    require(canonical_digest(plan) == inputs["setting_plan_sha256"], "setting_content")
    require(plan["setting_ref"] == fixtures["setting_ref"], "setting_resource")
    resources = _index(receipt["resources"]["entries"], "ref", "resource_inventory")
    resource = resources[fixtures["setting_ref"]]
    key = next(x["key"] for x in inputs["keys"] if x["resource_ref"] == resource["ref"])
    require(plan["key"] == key, "setting_key")
    evidence.resolve(key["ref"], "literal_db_key")  # Literal content was checked by inputs.
    writes = [effect for effect in receipt["observations"]["actual_effects"]
              if effect["resource_ref"] == resource["ref"] and effect["method"] != "db_read"]
    require([effect["operation_id"] for effect in writes] ==
            [plan["operations"][phase] for phase in SETTING_PHASES], "setting_operations")
    _unique_refs(list(plan["operations"].values()))
    transitions = [evidence.resolve(effect["db_transition_refs"][0], "db_transition")["claim"]
                   for effect in writes]
    state_refs = [resource["before_state_ref"],
                  *(claim["actual_after_state_ref"] for claim in transitions),
                  resource["after_state_ref"]]
    _unique_refs(state_refs)
    projections, states = {}, {}
    _unique_refs([pin["ref"] for pin in plan["values"]])
    for pin in plan["values"]:
        record = evidence.resolve(pin["ref"], "setting_value")
        require(canonical_digest(record) == pin["sha256"], "setting_content")
        ref = record["state_ref"]
        require(ref in state_refs and ref not in projections and
                record["key_ref"] == key["ref"], "setting_value_inventory")
        state = evidence.resolve(ref, "resource_state")["claim"]
        value = record["value"]
        require(value["presence"] == state["presence"], "setting_value_presence")
        if value["presence"] == "present":
            require(canonical_digest(value["stored_value"]) == state["value_sha256"],
                    "setting_value_digest")
        projections[ref], states[ref] = value, state
    require(projections.keys() == set(state_refs), "setting_value_inventory")
    before, baseline, enabled, disabled, restored, final = (projections[ref] for ref in state_refs)
    for actual, value in ((baseline, "0"), (enabled, "1"), (disabled, "0")):
        require(actual == {"presence": "present", "stored_value": value}, "setting_value")
    require(restored == final == before, "setting_restore_value")
    require([effect["method"] for effect in writes] == [
        "db_insert" if before["presence"] == "absent" else "db_cas",
        "db_cas", "db_cas",
        "db_delete_owned" if before["presence"] == "absent" else "db_cas"], "setting_methods")

    scheduler = evidence.resolve(fixtures["scheduler_plan_ref"], "scheduler_plan_content")
    activation = evidence.resolve(scheduler["activation_fence"]["ref"], "feed_activation_fence")
    times = [utc(effect["observed_at"]) for effect in writes]
    require(times[0] <= utc(activation["observed_at"]) and
            utc(attempt["observation_ended_at"]) < times[3], "setting_mutation_window")
    boundaries = _index(fixtures["setting_boundaries"], "ref", "scheduler_inventory")
    # Every stored reader must name the latest committed state, not simply a
    # plausible increasing version. No value change may precede its poll sample.
    for window in scheduler["windows"]:
        boundary = boundaries[window["boundary_ref"]]
        read_at = utc(boundary["read_at"])
        preceding = [i for i, timestamp in enumerate(times) if timestamp <= read_at]
        require(bool(preceding), "setting_reader_state")
        position = preceding[-1]
        expected_position = {"off": 0, "on": 1, "replay": 1, "final_off": 2}[window["phase"]]
        require(position == expected_position, "setting_reader_state")
        ref = state_refs[position + 1]
        require(projections[ref]["stored_value"] == boundary["stored_value"] and
                states[ref]["version"] == boundary["row_version"], "setting_reader_state")
        poll = evidence.resolve(window["poll"]["ref"], "feed_poll")
        require(utc(poll["observed_at"]) < times[position + 1], "setting_poll_state")
