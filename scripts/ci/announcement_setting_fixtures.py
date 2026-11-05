"""SYNTHETIC setting projections; no SQL, real values, CAS or restoration proof.

Only the setting operations are distributed across the fabricated timeline.
All other telemetry remains at 08:00, including cleanup-shaped writes; this is
NOT a coherent full-lifecycle/business-case fixture. The baseline names exact
setup authority and in-window setting writes use transition authority. Restoration
retains old generic batch claims without cleanup membership; full lifecycle must
still refuse. The baseline and rollout share a timestamp; triggers/audit and real CAS token
semantics remain absent.
"""

import copy
from datetime import timedelta

from announcement_provenance_fixtures import at, envelope
from announcement_resource_fixtures import repin_authority, repin_resources
from announcement_scheduler_fixtures import repin_scheduler, scheduler_fixture
from announcement_telemetry_fixtures import repin_telemetry, telemetry_fixture
from announcement_transition_fixtures import repin_ownership, transition_fixture
from announcement_runtime_evidence import canonical_digest


SETTING_OPS = ("budget-setting-db_insert-0", "budget-setting-db_cas-0",
               "budget-setting-db_cas-1", "budget-setting-db_delete_owned-0")


def add_checkpoint(receipt, records, clock, phase="effect_batch"):
    """Clone complete fabricated reads/fence for a phase, never acquire authority."""
    from announcement_runtime_schema import utc

    authority, attempt = receipt["authority"], receipt["attempt"]
    index = next(i for i, x in enumerate(attempt["checkpoints"]) if x["phase"] == "effect_batch")
    checkpoint = attempt["checkpoints"][index]
    install_ref = authority["installation_approval_refs"][index]
    install = records[install_ref]["data"]
    reads = [record[field] for record in (checkpoint, install)
             for field in ("controller_read_ref", "verifier_read_ref")]
    refs = set(reads + [records[ref]["data"]["history_ref"] for ref in reads] +
               [install_ref, checkpoint["mutation_fence_ref"]])
    suffix = "-at-" + clock.replace(":", "")
    mapping = {ref: ref + suffix for ref in refs | {checkpoint["ref"]}}
    old_time = checkpoint["checked_at"]
    old_end = records[checkpoint["mutation_fence_ref"]]["data"]["valid_until"]
    new_end = (utc(at(clock)) + timedelta(seconds=30)).strftime("%Y-%m-%dT%H:%M:%SZ")

    def remap(value):
        if type(value) is dict:
            return {key: remap(child) for key, child in value.items()}
        if type(value) is list:
            return [remap(child) for child in value]
        if type(value) is str:
            return at(clock) if value == old_time else new_end if value == old_end else mapping.get(value, value)
        return value

    for ref in refs:
        records[mapping[ref]] = remap(records[ref])
    for ref in reads:
        data = records[mapping[ref]]["data"]
        data["claim"]["history_sha256"] = canonical_digest(records[data["history_ref"]]["data"])
        if data["claim"]["ref"].startswith("runtime-"):
            authority["reads"].append(copy.deepcopy(data["claim"]))
    cloned = remap(checkpoint)
    cloned["phase"] = phase
    records[cloned["mutation_fence_ref"]]["data"]["phase"] = phase
    pairs = list(zip(attempt["checkpoints"], authority["installation_approval_refs"]))
    pairs.append((cloned, mapping[install_ref]))
    pairs.sort(key=lambda pair: pair[0]["checked_at"])
    attempt["checkpoints"] = [pair[0] for pair in pairs]
    authority["installation_approval_refs"] = [pair[1] for pair in pairs]
    receipt["control"]["installation"]["authority_receipt_refs"] = copy.deepcopy(
        authority["installation_approval_refs"])
    records[authority["execution"]["ref"]]["data"]["installation_approval_refs"] = copy.deepcopy(
        authority["installation_approval_refs"])
    records["runtime-authority-scope"]["data"]["control"] = copy.deepcopy(receipt["control"])
    repin_authority(receipt, records)
    return mapping[checkpoint["ref"]]


def compact_claims(receipt, records):
    """Keep this synthetic graph within 256 records WITHOUT relaxing any bound.

    The three initial checkpoints share one instant (not real phase proof).
    Transition and effect checkpoints share 08:00 but retain distinct fences.
    Identical history snapshots and installation reads at the same instant may
    reference one record. Runtime reads and mutation fences stay distinct. All
    non-setting, non-RSVP writes at 08:00 share one fabricated transaction, never
    the isolated RSVP/audit transaction. No operation or evidence is dropped.
    """
    for checkpoint, install_ref in zip(receipt["attempt"]["checkpoints"],
                                       receipt["authority"]["installation_approval_refs"]):
        if checkpoint["ref"] not in ("checkpoint-0", "checkpoint-1", "checkpoint-2"):
            continue
        checkpoint["checked_at"] = at("00:03:00")
        records[checkpoint["mutation_fence_ref"]]["data"].update(
            observed_at=at("00:03:00"), valid_until=at("00:03:30"))
        install = records[install_ref]["data"]
        install["checked_at"] = at("00:03:00")
        for record in (checkpoint, install):
            for field in ("controller_read_ref", "verifier_read_ref"):
                read = records[record[field]]["data"]
                read["claim"]["read_at"] = at("00:03:00")
                records[read["history_ref"]]["data"]["ended_at"] = at("00:03:00")
                read["claim"]["history_sha256"] = canonical_digest(
                    records[read["history_ref"]]["data"])
    repin_authority(receipt, records)

    def remap(value, mapping):
        if type(value) is dict:
            return {key: remap(child, mapping) for key, child in value.items()}
        if type(value) is list:
            return [remap(child, mapping) for child in value]
        return mapping.get(value, value) if type(value) is str else value

    for kind in ("authority_history", "authority_read"):
        seen, mapping = {}, {}
        for ref, record in records.items():
            if record["kind"] != kind or (kind == "authority_read" and not ref.startswith("install-")):
                continue
            data = copy.deepcopy(record["data"])
            if kind == "authority_read":
                data["claim"].pop("ref")
            digest = canonical_digest(data)
            if digest in seen:
                mapping[ref] = seen[digest]
            else:
                seen[digest] = ref
        for ref in mapping:
            del records[ref]
        receipt.update(remap(receipt, mapping))
        for ref in list(records):
            records[ref] = remap(records[ref], mapping)

    setting_ops = set(SETTING_OPS)
    groups = [ref for ref, record in records.items() if record["kind"] == "db_transaction" and
              ref != "transaction-rsvp-audit" and
              all(x["operation_id"] not in setting_ops for x in record["data"]["effects"])]
    combined = records[groups[0]]["data"]
    for ref in groups[1:]:
        group = records.pop(ref)["data"]
        combined["effects"].extend(group["effects"])
        combined["transition_refs"].extend(group["transition_refs"])
        for transition_ref in group["transition_refs"]:
            records[transition_ref]["data"]["transaction_ref"] = groups[0]
    coverage = receipt["observations"]["coverage"][0]
    coverage["transaction_receipt_refs"] = [ref for ref in coverage["transaction_receipt_refs"]
                                             if ref not in groups[1:]]
    repin_authority(receipt, records)


def repin_settings(receipt, records):
    """Refresh derived content hashes, NEVER repair values, versions or times."""
    plan = records["runtime-setting-plan"]["data"]
    for pin in plan["values"]:
        if pin["ref"] in records:
            pin["sha256"] = canonical_digest(records[pin["ref"]]["data"])
    records["runtime-input-plan"]["data"]["setting_plan_sha256"] = canonical_digest(plan)
    repin_scheduler(receipt, records)
    repin_ownership(receipt, records)
    repin_telemetry(receipt, records)


def setting_fixture():
    receipt, records, now = transition_fixture(telemetry_fixture(scheduler_fixture()))
    # A pre-rollout state brackets setup; a 19:00 candidate observation brackets
    # the 18:00 setting restoration without inventing an extra sample record.
    for sample in receipt["served"]["samples"]:
        if sample["ref"] == "served-two":
            sample["observed_at"] = at("00:19:00")
    records["external-served-two"]["data"]["sample"]["observed_at"] = at("00:19:00")
    schedule = records[receipt["fixtures"]["scheduler_plan_ref"]]["data"]
    # Expand the first off window so enable can reuse the original 08:00 fence.
    clocks = (("00:05:00", "00:08:00", "00:06:00"),
              ("00:08:00", "00:10:00", "00:09:00"),
              ("00:10:00", "00:12:00", "00:11:00"),
              ("00:12:00", "00:15:00", "00:13:00"))
    boundaries = {x["ref"]: x for x in receipt["fixtures"]["setting_boundaries"]}
    for window, (start, end, poll) in zip(schedule["windows"], clocks):
        window.update(started_at=at(start), ended_at=at(end))
        boundaries[window["boundary_ref"]]["read_at"] = at(start)
        records[window["reader"]["ref"]]["data"]["read_at"] = at(start)
        records[window["poll"]["ref"]]["data"]["observed_at"] = at(poll)
        for case in receipt["observations"]["cases"]:
            if case["phase"] == window["phase"]:
                case.update(started_at=at(start), ended_at=at(end))
    records[schedule["activation_fence"]["ref"]]["data"]["observed_at"] = at("00:03:00")
    checkpoints = {clock: add_checkpoint(receipt, records, clock,
                   phase="transition" if clock == "00:12:00" else "effect_batch")
                   for clock in ("00:12:00", "00:18:00")}
    checkpoints["00:03:00"] = "checkpoint-1"
    checkpoints["00:08:00"] = "checkpoint-3"
    effects = {effect["operation_id"]: effect for effect in receipt["observations"]["actual_effects"]}
    for operation, clock, value in zip(SETTING_OPS,
            ("00:03:00", "00:08:00", "00:12:00", "00:18:00"), ("0", "1", "0", None)):
        effect = effects[operation]
        effect["observed_at"] = at(clock)
        if operation == SETTING_OPS[0]:
            effect["served_ref"] = receipt["lifecycle"]["served_before"]["ref"]
        claim = records[effect["db_transition_refs"][0]]["data"]["claim"]
        claim.update(observed_at=at(clock), authority_ref=checkpoints[clock])
        state = records[claim["actual_after_state_ref"]]["data"]
        state["observed_at"] = at(clock)
        if value is not None:
            state["claim"]["value_sha256"] = canonical_digest(value)
        records[claim["ownership_match_ref"]]["data"].update(
            checkpoint_ref=checkpoints[clock], observed_at=at(clock))
        transaction = records[records[effect["db_transition_refs"][0]]["data"]["transaction_ref"]]["data"]
        transaction["committed_at"] = at(clock)
    baseline = effects[SETTING_OPS[0]]
    receipt["lifecycle"]["setup"] = [copy.deepcopy(
        records[baseline["db_transition_refs"][0]]["data"]["claim"])]
    actual = receipt["observations"]["actual_effects"]
    actual.sort(key=lambda effect: effect["observed_at"])
    for sequence, effect in enumerate(actual, 1):
        effect["sequence"] = sequence
    receipt["observations"]["expected_effects"] = copy.deepcopy(actual)
    states = ["before-setting", *("state-" + op for op in SETTING_OPS), "after-setting"]
    values = []
    for number, (ref, value) in enumerate(zip(states, (None, "0", "1", "0", None, None))):
        value_ref = "setting-value-" + str(number)
        data = {"state_ref": ref, "key_ref": "key-setting", "value":
                {"presence": "absent"} if value is None else
                {"presence": "present", "stored_value": value}}
        records[value_ref] = envelope(value_ref, "setting_value", data)
        values.append({"ref": value_ref, "sha256": canonical_digest(data)})
    key = next(x["key"] for x in records["runtime-input-plan"]["data"]["keys"]
               if x["resource_ref"] == "setting")
    records["runtime-setting-plan"] = envelope("runtime-setting-plan", "setting_plan_content", {
        "setting_ref": "setting", "key": copy.deepcopy(key), "values": values,
        "operations": dict(zip(("baseline", "enable", "disable", "restore"), SETTING_OPS))})
    repin_resources(receipt, records)
    compact_claims(receipt, records)
    repin_settings(receipt, records)
    return receipt, records, now
