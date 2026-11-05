"""Offline owned DB state-chain comparisons, NOT mutation or restoration proof.

Composes typed authority and telemetry claims. State/ownership witnesses are
unauthenticated inert data, never SQL, a credential, a CAS primitive or a trust
root. Literal values, stored cases, phase completion and actual restoration
remain mandatory separate checks. runtime_preflight() always refuses.
"""

from announcement_runtime_authority import _mutation_reads_current, check_authority_graph_claims
from announcement_runtime_evidence import canonical_digest
from announcement_runtime_provenance import _index, _unique_refs
from announcement_runtime_schema import require, utc
from announcement_runtime_telemetry import check_telemetry_claims


def check_transition_claims(receipt, evidence, now):
    """Compare every DB write's owned before/after chain in operation order.

    Read-only effects cannot hide a transition. Every write must consume a
    distinct external compare-and-match witness tied to its exact snapshots,
    operation, execution/nonce/epochs and current effect-batch checkpoint. These
    are necessary consistency predicates, not independently acquired evidence.
    """
    check_telemetry_claims(receipt, evidence, now)
    check_authority_graph_claims(receipt, evidence, now)
    resources = _index(receipt["resources"]["entries"], "ref", "resource_inventory")
    authority, attempt = receipt["authority"], receipt["attempt"]
    coverage = receipt["observations"]["coverage"][0]
    checkpoints = _index(attempt["checkpoints"], "ref", "checkpoint_inventory")
    observer_ref = coverage["observer_ref"]
    prior, state_cache = {}, {}
    witnesses, created_states = [], []

    def state(ref, resource):
        record = evidence.resolve(ref, "resource_state")
        require(record["claim"]["ref"] == ref and
                record["resource_ref"] == resource["ref"] and
                record["identity_sha256"] == canonical_digest(resource["identity"]) and
                record["owner_ref"] == resource["owner"], "transition_state_binding")
        # Endpoint observers were checked by the resource graph; intermediate
        # observations must use the independent full-window telemetry observer.
        if ref not in (resource["before_state_ref"], resource["after_state_ref"]):
            require(record["observer_ref"] == observer_ref, "transition_state_observer")
        if ref in state_cache:
            require(state_cache[ref] == record, "transition_state_alias")
        state_cache[ref] = record
        return record

    for resource in resources.values():
        if resource["identity"]["kind"] == "database":
            prior[resource["ref"]] = state(resource["before_state_ref"], resource)

    for effect in receipt["observations"]["actual_effects"]:
        if not effect["method"].startswith("db_") or effect["method"] == "db_read":
            continue
        resource = resources[effect["resource_ref"]]
        claim = evidence.resolve(effect["db_transition_refs"][0], "db_transition")["claim"]
        before = state(claim["before_state_ref"], resource)
        after = state(claim["actual_after_state_ref"], resource)
        require(before == prior[resource["ref"]], "transition_chain")
        require(after["claim"]["ref"] != before["claim"]["ref"] and
                after["claim"]["ref"] not in (resource["before_state_ref"],
                                               resource["after_state_ref"]),
                "transition_state_reuse")
        created_states.append(after["claim"]["ref"])
        observed = utc(effect["observed_at"])
        require(utc(attempt["started_at"]) <= utc(before["observed_at"]) <= observed and
                after["observed_at"] == effect["observed_at"], "transition_state_time")
        old, new = before["claim"], after["claim"]
        method = effect["method"]
        if method == "db_insert":
            require(old["presence"] == "absent" and new["presence"] == "present",
                    "transition_insert")
        elif method == "db_cas":
            require(old["presence"] == new["presence"] == "present" and
                    int(new["version"]) > int(old["version"]), "transition_cas")
        else:
            require(method == "db_delete_owned" and old["presence"] == "present" and
                    new["presence"] == "absent", "transition_delete")

        checkpoint = checkpoints.get(claim["authority_ref"])
        require(checkpoint is not None and checkpoint["phase"] == "effect_batch",
                "transition_authority")
        fence = evidence.resolve(checkpoint["mutation_fence_ref"], "mutation_fence")
        require(utc(fence["observed_at"]) <= observed < utc(fence["valid_until"]),
                "transition_authority_time")
        checkpoint_index = attempt["checkpoints"].index(checkpoint)
        installation = evidence.resolve(authority["installation_approval_refs"][checkpoint_index],
                                        "installation_approval")
        require(_mutation_reads_current(checkpoint, installation, evidence, observed),
                "stale_mutation_read")
        witness_ref = claim["ownership_match_ref"]
        witness = evidence.resolve(witness_ref, "ownership_match")
        require(witness == {"operation_id": effect["operation_id"],
            "resource_ref": resource["ref"], "identity_sha256": canonical_digest(resource["identity"]),
            "key_sha256": resource["identity"]["key_sha256"], "owner_ref": resource["owner"],
            "execution_ref": authority["execution"]["ref"],
            "nonce_ref": authority["execution"]["nonce_ref"],
            "authority_epoch": authority["epoch"], "resource_epoch": attempt["enforcement_epoch"],
            "checkpoint_ref": checkpoint["ref"], "method": method,
            "before_state_sha256": canonical_digest(before),
            "after_state_sha256": canonical_digest(after), "observer_ref": observer_ref,
            "observed_at": effect["observed_at"], "boundary": "external_compare_and_match",
            "outcome": "matched"}, "ownership_match")
        witnesses.append(witness_ref)
        prior[resource["ref"]] = after

    _unique_refs(witnesses)
    _unique_refs(created_states)
    def contents(record):
        return {key: value for key, value in record["claim"].items() if key != "ref"}

    for ref, last in prior.items():
        resource = resources[ref]
        final = state(resource["after_state_ref"], resource)
        require(utc(last["observed_at"]) <= utc(final["observed_at"]) <=
                utc(attempt["ended_at"]), "transition_final_time")
        # The separately observed final state must equal the last operation's
        # complete state, not merely a convenient equal value digest.
        require(contents(last) == contents(final), "transition_final_state")
