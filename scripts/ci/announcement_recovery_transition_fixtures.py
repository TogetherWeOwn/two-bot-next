"""SYNTHETIC public transition composition, NOT full receipt or live evidence.

Retain all resource/provenance/telemetry operations and the isolated RSVP/audit
transaction. Feed setup is at 02:00, ordinary effects at 08:00, five separately
scoped DB deletions at 20:30, candidate sample at 21:00, restoration at 23:00
and both final authority checks at 24:00. Every DB endpoint is restored absent.
Setting values/cadence, complete business cases, drain and process restoration
are NOT proved. No checker is mocked and no production bound is changed.
"""

import copy

from announcement_provenance_fixtures import at, envelope
from announcement_resource_fixtures import repin_authority, repin_resources
from announcement_telemetry_fixtures import binding, repin_telemetry
from announcement_transition_fixtures import repin_ownership, transition_fixture
from announcement_runtime_evidence import canonical_digest
from announcement_runtime_recovery import RECOVERY_PLAN_REF
from announcement_runtime_schema import utc
from test_announcement_runtime_recovery_fences import repin_recovery_scope
from test_announcement_runtime_recovery_grant import GRANT, SCOPE
from test_announcement_runtime_recovery_installations import install_fixture
from test_announcement_runtime_recovery_reads import CKPT, CLEAN_CP


SETUP = "budget-feed-db_insert-0"
AUDIT_CLEANUP = "budget-rsvp-audit-db_delete_owned-0"


def repin(receipt, records):
    """Rehash derived bindings only; never repair semantic mutations in tests."""
    repin_resources(receipt, records)
    repin_recovery_scope(receipt, records)
    repin_authority(receipt, records)
    scope = records[SCOPE]["data"]
    scope["runtime_scope_sha256"] = receipt["authority"]["scope_sha256"]
    repin_recovery_scope(receipt, records)
    pin = receipt["resources"]["plan"]
    records[pin["ref"]]["data"] = {key: value for key, value in pin.items() if key != "ref"}
    repin_ownership(receipt, records)
    repin_telemetry(receipt, records)


def recovery_transition_fixture():
    receipt, records, _ = transition_fixture()
    now = utc(receipt["attempt"]["ended_at"])
    resources = {entry["ref"]: entry for entry in receipt["resources"]["entries"]}
    effects = receipt["observations"]["actual_effects"]
    observer = receipt["observations"]["coverage"][0]["observer_ref"]
    life, authority = receipt["lifecycle"], receipt["authority"]

    # An audit insertion cannot simply disappear from lifecycle accounting.
    # Add its owned deletion to the exact method/resource budget and state chain.
    audit = resources["rsvp-audit"]
    audit["allowed_operations"].append("db_delete_owned")
    audit["expected_count"] += 1
    audit["maximum_count"] += 1
    records["runtime-resource-plan"]["data"]["operation_budgets"].append({
        "ref": "budget-rsvp-audit-db_delete_owned", "resource_ref": "rsvp-audit",
        "method": "db_delete_owned", "expected_count": 1, "maximum_count": 1})
    insertion = next(effect for effect in effects if effect["resource_ref"] == "rsvp-audit")
    effect = copy.deepcopy(insertion)
    effect.update(operation_id=AUDIT_CLEANUP, sequence=len(effects) + 1,
                  method="db_delete_owned", db_transition_refs=["transition-" + AUDIT_CLEANUP])
    effects.append(effect)
    previous = records[insertion["db_transition_refs"][0]]["data"]["claim"]
    claim = copy.deepcopy(previous)
    after_ref, match_ref = "state-" + AUDIT_CLEANUP, "match-" + AUDIT_CLEANUP
    claim.update(operation_id=AUDIT_CLEANUP, before_state_ref=previous["actual_after_state_ref"],
                 intended_after_state_ref=after_ref, actual_after_state_ref=after_ref,
                 ownership_match_ref=match_ref)
    state = copy.deepcopy(records[previous["actual_after_state_ref"]]["data"])
    state["claim"] = {"ref": after_ref, "presence": "absent"}
    records[after_ref] = envelope(after_ref, "resource_state", state)
    final = records[audit["after_state_ref"]]["data"]["claim"]
    final.clear()
    final.update(ref=audit["after_state_ref"], presence="absent")
    # Remove the obsolete present-state columns from the receipt endpoint too.
    endpoint = next(state for state in receipt["resources"]["states"] if state["ref"] == final["ref"])
    endpoint.clear()
    endpoint.update(final)
    transaction_ref = "transaction-" + AUDIT_CLEANUP
    records[effect["db_transition_refs"][0]] = envelope(effect["db_transition_refs"][0], "db_transition", {
        "transaction_ref": transaction_ref, "method": "db_delete_owned", "claim": claim})
    transaction = copy.deepcopy(records["transaction-rsvp-audit"]["data"])
    transaction.update(transaction_id="abcdef01-0000-4000-8000-000000000000",
                       effects=[binding(receipt, effect)], transition_refs=effect["db_transition_refs"][:])
    records[transaction_ref] = envelope(transaction_ref, "db_transaction", transaction)
    receipt["observations"]["coverage"][0]["transaction_receipt_refs"].append(transaction_ref)
    records[match_ref] = envelope(match_ref, "ownership_match", {})

    for sample in receipt["served"]["samples"]:
        if sample["ref"] == "served-two":
            sample["observed_at"] = at("00:21:00")
    records["external-served-two"]["data"]["sample"]["observed_at"] = at("00:21:00")
    life["served_after"]["observed_at"] = at("00:23:00")
    records["external-served-after"]["data"]["sample"]["observed_at"] = at("00:23:00")

    cleanup = []
    for effect in effects:
        if effect["operation_id"] != SETUP and effect["method"] != "db_delete_owned":
            continue
        is_setup = effect["operation_id"] == SETUP
        clock = at("00:02:00" if is_setup else "00:20:30")
        effect.update(observed_at=clock, served_ref=life["served_before"]["ref"] if is_setup else "served-one")
        transition = records[effect["db_transition_refs"][0]]["data"]
        claim = transition["claim"]
        claim.update(observed_at=clock, authority_ref="checkpoint-1" if is_setup else CLEAN_CP)
        records[claim["actual_after_state_ref"]]["data"]["observed_at"] = clock
        records[transition["transaction_ref"]]["data"]["committed_at"] = clock
        if is_setup:
            records[claim["ownership_match_ref"]]["data"].update(
                observed_at=clock, checkpoint_ref=claim["authority_ref"])
            life["setup"] = [copy.deepcopy(claim)]
        else:
            cleanup.append(claim)
    effects.sort(key=lambda effect: effect["observed_at"])
    for sequence, effect in enumerate(effects, 1):
        effect["sequence"] = sequence
    receipt["observations"]["expected_effects"] = copy.deepcopy(effects)
    receipt["observations"]["coverage"][0]["last_sequence"] = len(effects)
    life["cleanup"] = copy.deepcopy(cleanup)
    life["restoration_authority_ref"] = GRANT

    # Reuse the existing literal recovery schema fixtures, NOT an execution
    # adapter. Copy only separate recovery records and their installation reads.
    _, template, _ = install_fixture()
    def retime(value):
        if type(value) is dict:
            return {key: retime(child) for key, child in value.items()}
        if type(value) is list:
            return [retime(child) for child in value]
        if type(value) is str:
            return {at("00:22:00"): at("00:24:00"),
                    at("00:22:30"): at("00:24:30")}.get(value, value)
        return value

    for ref, record in template.items():
        if ref.startswith(("recovery-", "separate-recovery", "runtime-recovery")):
            records[ref] = retime(record)
    for ref, record in records.items():
        if ref.startswith("recovery-install-read-"):
            data = record["data"]
            data["claim"]["history_sha256"] = canonical_digest(records[data["history_ref"]]["data"])
    plan = records[RECOVERY_PLAN_REF]["data"]
    plan.update(contract=copy.deepcopy(receipt["scope"]["contract"]),
                candidate=copy.deepcopy(receipt["scope"]["candidate"]),
                served_before_sha256=canonical_digest(life["served_before"]),
                worker_before_sha256=canonical_digest(life["worker_before"]))
    plan["operations"] = [{"operation_id": claim["operation_id"], "resource_ref": claim["resource_ref"],
        "method": "db_delete_owned", "identity_sha256": canonical_digest(resources[claim["resource_ref"]]["identity"]),
        "key_sha256": claim["key_sha256"], "owner_ref": claim["owner"],
        "restore_state_ref": resources[claim["resource_ref"]]["before_state_ref"],
        "expected_count": 1, "maximum_count": 1} for claim in cleanup]
    source = records[authority["source_ref"]]["data"]
    records[SCOPE]["data"].update(source_sha256=canonical_digest(source),
        consumers_sha256=canonical_digest({role: records[source[role + "_ref"]]["data"]
                                          for role in ("controller", "verifier")}))
    records[CKPT]["data"]["checkpoints"][0]["operation_id_refs"] = [claim["operation_id"] for claim in cleanup]
    grant = records[GRANT]["data"]
    for claim in cleanup:
        resource = resources[claim["resource_ref"]]
        records[claim["ownership_match_ref"]] = envelope(claim["ownership_match_ref"], "ownership_match", {
            "operation_id": claim["operation_id"], "resource_ref": resource["ref"],
            "identity_sha256": canonical_digest(resource["identity"]), "key_sha256": claim["key_sha256"],
            "owner_ref": claim["owner"], "execution_ref": GRANT, "nonce_ref": grant["nonce_ref"],
            "authority_epoch": grant["epoch"], "resource_epoch": receipt["attempt"]["enforcement_epoch"],
            "checkpoint_ref": CLEAN_CP, "method": "db_delete_owned", "before_state_sha256": "0" * 64,
            "after_state_sha256": "0" * 64, "observer_ref": observer,
            "observed_at": claim["observed_at"], "boundary": "external_compare_and_match", "outcome": "matched"})
    repin(receipt, records)
    return receipt, records, now
