"""SYNTHETIC owned-state chains, not stored cases, SQL CAS or cleanup proof.

Effects deliberately remain at the telemetry fixture's SINGLE instant. These
claims compare operation-ordered state versions and ownership, but do not model
real lifecycle phases or exact setting/business values. Nothing is authenticated.
"""

import copy

from announcement_provenance_fixtures import at, envelope
from announcement_resource_fixtures import repin_resources, repin_authority
from announcement_runtime_evidence import canonical_digest
from announcement_telemetry_fixtures import telemetry_fixture


def repin_ownership(receipt, records):
    """Refresh only derived snapshot hashes, never semantic witness fields."""
    for effect in receipt["observations"]["actual_effects"]:
        if effect["method"].startswith("db_") and effect["method"] != "db_read":
            claim = records[effect["db_transition_refs"][0]]["data"]["claim"]
            match = records[claim["ownership_match_ref"]]["data"]
            match["before_state_sha256"] = canonical_digest(
                records[claim["before_state_ref"]]["data"])
            match["after_state_sha256"] = canonical_digest(
                records[claim["actual_after_state_ref"]]["data"])


def add_late_effect_checkpoint(receipt, records):
    """Clone a complete fabricated read/fence at 08:01, AFTER the 08:00 effects.

    The original 08:00 fence still covers every effect for the upstream graph.
    Naming this later, internally coherent checkpoint must not authorize an
    earlier transition. No snapshot here is acquired from a service.
    """
    authority, attempt = receipt["authority"], receipt["attempt"]
    index = next(i for i, x in enumerate(attempt["checkpoints"]) if x["phase"] == "effect_batch")
    checkpoint = attempt["checkpoints"][index]
    install_ref = authority["installation_approval_refs"][index]
    install = records[install_ref]["data"]
    reads = [record[field] for record in (checkpoint, install)
             for field in ("controller_read_ref", "verifier_read_ref")]
    refs = set(reads + [records[ref]["data"]["history_ref"] for ref in reads] +
               [install_ref, checkpoint["mutation_fence_ref"]])
    mapping = {ref: ref + "-late" for ref in refs | {checkpoint["ref"]}}

    def remap(value):
        if type(value) is dict:
            return {key: remap(child) for key, child in value.items()}
        if type(value) is list:
            return [remap(child) for child in value]
        if type(value) is str:
            return at("00:08:01") if value == checkpoint["checked_at"] else mapping.get(value, value)
        return value

    for ref in refs:
        records[mapping[ref]] = remap(records[ref])
    for ref in reads:
        data = records[mapping[ref]]["data"]
        data["claim"]["history_sha256"] = canonical_digest(records[data["history_ref"]]["data"])
        if data["claim"]["ref"].startswith("runtime-"):
            authority["reads"].append(copy.deepcopy(data["claim"]))
    attempt["checkpoints"].insert(index + 1, remap(checkpoint))
    authority["installation_approval_refs"].insert(index + 1, mapping[install_ref])
    receipt["control"]["installation"]["authority_receipt_refs"] = copy.deepcopy(
        authority["installation_approval_refs"])
    records[authority["execution"]["ref"]]["data"]["installation_approval_refs"] = copy.deepcopy(
        authority["installation_approval_refs"])
    records["runtime-authority-scope"]["data"]["control"] = copy.deepcopy(receipt["control"])
    repin_authority(receipt, records)
    return mapping[checkpoint["ref"]]


def transition_fixture():
    receipt, records, now = telemetry_fixture()
    entries = {x["ref"]: x for x in receipt["resources"]["entries"]}
    prior = {ref: entry["before_state_ref"] for ref, entry in entries.items()
             if entry["identity"]["kind"] == "database"}
    versions = {ref: 0 for ref in prior}
    checkpoint = next(x for x in receipt["attempt"]["checkpoints"] if x["phase"] == "effect_batch")
    authority = receipt["authority"]
    observer = receipt["observations"]["coverage"][0]["observer_ref"]

    def add(ref, kind, data):
        records[ref] = envelope(ref, kind, copy.deepcopy(data))

    for effect in receipt["observations"]["actual_effects"]:
        if not effect["method"].startswith("db_") or effect["method"] == "db_read":
            continue
        ref, method, operation = effect["resource_ref"], effect["method"], effect["operation_id"]
        resource = entries[ref]
        transition = records[effect["db_transition_refs"][0]]["data"]["claim"]
        after_ref = "state-" + operation
        witness_ref = "match-" + operation
        versions[ref] += 1
        if method == "db_delete_owned":
            claim = {"ref": after_ref, "presence": "absent"}
        else:
            claim = {"ref": after_ref, "presence": "present", "version": str(versions[ref]),
                     "value_sha256": str(versions[ref]) * 64}
        after = {"claim": claim, "resource_ref": ref,
            "identity_sha256": canonical_digest(resource["identity"]),
            "owner_ref": resource["owner"], "observer_ref": observer,
            "observed_at": effect["observed_at"]}
        add(after_ref, "resource_state", after)
        before = records[prior[ref]]["data"]
        transition.update(before_state_ref=prior[ref], intended_after_state_ref=after_ref,
                          actual_after_state_ref=after_ref, authority_ref=checkpoint["ref"],
                          ownership_match_ref=witness_ref)
        add(witness_ref, "ownership_match", {"operation_id": operation, "resource_ref": ref,
            "identity_sha256": canonical_digest(resource["identity"]),
            "key_sha256": resource["identity"]["key_sha256"], "owner_ref": resource["owner"],
            "execution_ref": authority["execution"]["ref"],
            "nonce_ref": authority["execution"]["nonce_ref"], "authority_epoch": authority["epoch"],
            "resource_epoch": receipt["attempt"]["enforcement_epoch"],
            "checkpoint_ref": checkpoint["ref"], "method": method,
            "before_state_sha256": canonical_digest(before),
            "after_state_sha256": canonical_digest(after), "observer_ref": observer,
            "observed_at": effect["observed_at"], "boundary": "external_compare_and_match",
            "outcome": "matched"})
        prior[ref] = after_ref
    # Endpoint observations have their own refs/times/observer, but must compare
    # the COMPLETE final version/value/presence, not just the value hash.
    for ref, last_ref in prior.items():
        final_ref = entries[ref]["after_state_ref"]
        final = records[final_ref]["data"]["claim"]
        final.clear()
        final.update(copy.deepcopy(records[last_ref]["data"]["claim"]), ref=final_ref)
        next(s for s in receipt["resources"]["states"] if s["ref"] == final_ref).update(final)
    repin_resources(receipt, records)
    repin_authority(receipt, records)
    return receipt, records, now
