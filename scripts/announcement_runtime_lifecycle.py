"""Offline lifecycle DB-ledger correspondence, NOT cleanup or recovery authority.

This narrow check resolves existing setup/cleanup claims against telemetry and
state sidecars. It does not replace transition/authority/resource/provenance
checks, acquire locks, prove drain or authenticate prior state. Unsupported
message cleanup refuses; runtime_preflight() still always refuses.
"""

from datetime import datetime, timezone

from announcement_runtime_evidence import EvidenceIndex, canonical_digest
from announcement_runtime_provenance import _index, _unique_refs
from announcement_runtime_schema import require, utc, validate_receipt_shape


def check_lifecycle_ledger_claims(receipt, evidence, now):
    """Compare setup/cleanup DB writes and exact restoration value projections.

    Run alongside, never instead of, the other claim checkers. A coherent ledger
    returns no capability. In particular the opaque drain/recovery/postcondition
    references are NOT consumed here: a complete packet must still refuse them
    until their own closed semantics and independent observations are implemented.
    """
    validate_receipt_shape(receipt)
    require(type(evidence) is EvidenceIndex, "evidence_index")
    require(type(now) is datetime and now.tzinfo is timezone.utc, "clock")
    attempt, lifecycle = receipt["attempt"], receipt["lifecycle"]
    evidence.require_run(attempt["run"])
    require(lifecycle["abort"] == "not_triggered" and lifecycle["in_flight"] == "resolved",
            "lifecycle_adverse")
    start, end = utc(attempt["started_at"]), utc(attempt["ended_at"])
    snapshot = utc(lifecycle["served_before"]["observed_at"])
    restored = utc(lifecycle["served_after"]["observed_at"])
    rollout_start, rollout_end = utc(receipt["rollout"]["started_at"]), utc(receipt["rollout"]["ended_at"])
    observation_start = utc(attempt["observation_started_at"])
    observation_end = utc(attempt["observation_ended_at"])
    require(start <= snapshot <= rollout_start <= rollout_end <= observation_start <
            observation_end <= restored <= end <= now, "lifecycle_window")
    resources = _index(receipt["resources"]["entries"], "ref", "resource_inventory")
    effects = receipt["observations"]["actual_effects"]
    by_operation = _index(effects, "operation_id", "operation_replay")
    require([effect["sequence"] for effect in effects] == list(range(1, len(effects) + 1)),
            "sequence_gap")
    require([effect["observed_at"] for effect in effects] ==
            sorted(effect["observed_at"] for effect in effects), "sequence_time")
    # Message ownership/content/version comparison is not a DB transition. Do
    # not silently admit message deletion through this narrower ledger checker.
    require(not any(effect["method"] == "message_delete_owned" for effect in effects),
            "unsupported_message_cleanup")
    require(all(effect["outcome"] == "accepted" and effect["accepted_count"] == 1 and
                start <= utc(effect["observed_at"]) <= end for effect in effects),
            "lifecycle_effect")
    transitions = {}
    for effect in effects:
        if not effect["method"].startswith("db_") or effect["method"] == "db_read":
            continue
        require(len(effect["db_transition_refs"]) == 1, "lifecycle_operation")
        transition = evidence.resolve(effect["db_transition_refs"][0], "db_transition")
        claim = transition["claim"]
        resource = resources.get(effect["resource_ref"])
        require(resource is not None and resource["identity"]["kind"] == "database" and
                transition["method"] == effect["method"] and
                claim["operation_id"] == effect["operation_id"] and
                claim["resource_ref"] == effect["resource_ref"] and
                claim["key_sha256"] == resource["identity"]["key_sha256"] and
                claim["owner"] == resource["owner"] and
                claim["observed_at"] == effect["observed_at"] and
                claim["intended_after_state_ref"] == claim["actual_after_state_ref"] and
                claim["expected_count"] == claim["actual_count"] == 1 and
                claim["outcome"] == "matched", "lifecycle_transition")
        transitions[effect["operation_id"]] = claim
    assigned = []
    for field, lower, upper, methods in (
            ("setup", snapshot, rollout_start, ("db_insert", "db_cas")),
            ("cleanup", observation_end, restored, ("db_cas", "db_delete_owned"))):
        ledger = lifecycle[field]
        _unique_refs([claim["operation_id"] for claim in ledger])
        sequences = []
        for claim in ledger:
            effect = by_operation.get(claim["operation_id"])
            require(effect is not None and effect["method"] in methods and
                    len(effect["db_transition_refs"]) == 1, "lifecycle_operation")
            require(transitions[effect["operation_id"]] == claim, "lifecycle_transition")
            require(lower <= utc(claim["observed_at"]) <= upper, "lifecycle_operation_time")
            sequences.append(effect["sequence"])
            assigned.append(effect["operation_id"])
        require(sequences == sorted(sequences), "lifecycle_ledger_order")
    _unique_refs(assigned)
    # Every DB write outside the test window belongs to its exact lifecycle
    # ledger. Writes inside the window require case/setting semantics elsewhere.
    # Explicit ledger membership wins at a shared endpoint; an unlisted write at
    # observation_end is cleanup, not a new candidate effect batch.
    for effect in effects:
        if effect["method"].startswith("db_") and effect["method"] != "db_read":
            observed = utc(effect["observed_at"])
            if not observation_start <= observed < observation_end:
                require(effect["operation_id"] in assigned, "unassigned_lifecycle_write")

    def state(ref, resource):
        record = evidence.resolve(ref, "resource_state")
        require(record["claim"]["ref"] == ref and
                record["resource_ref"] == resource["ref"] and
                record["identity_sha256"] == canonical_digest(resource["identity"]) and
                record["owner_ref"] == resource["owner"], "lifecycle_state")
        return record

    for resource in resources.values():
        if resource["identity"]["kind"] != "database":
            continue
        before = state(resource["before_state_ref"], resource)
        after = state(resource["after_state_ref"], resource)
        require(start <= utc(before["observed_at"]) <= snapshot and
                restored <= utc(after["observed_at"]) <= end, "lifecycle_state_time")
        # Restoring a present prior value creates a new version. Do not mistake
        # an equal digest on an absent/present mismatch for prior-state recovery,
        # nor require the old version token to be reused after an owned CAS.
        old, new = before["claim"], after["claim"]
        require(old["presence"] == new["presence"] and
                (old["presence"] == "absent" or old["value_sha256"] == new["value_sha256"]),
                "lifecycle_prior_state")
        writes = [effect for effect in effects if effect["resource_ref"] == resource["ref"] and
                  effect["method"].startswith("db_") and effect["method"] != "db_read"]
        if writes:
            last = transitions[writes[-1]["operation_id"]]
            final_record = state(last["actual_after_state_ref"], resource)
            require(final_record["observed_at"] == last["observed_at"], "lifecycle_final_time")
            final = final_record["claim"]
            require({k: v for k, v in final.items() if k != "ref"} ==
                    {k: v for k, v in new.items() if k != "ref"}, "lifecycle_final_state")
        else:
            require({k: v for k, v in old.items() if k != "ref"} ==
                    {k: v for k, v in new.items() if k != "ref"}, "lifecycle_unobserved_change")
