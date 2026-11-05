"""Offline finite recovery-plan comparisons, NOT recovery authority or execution.

The resource-plan digest binds this closed plan, not a caller allowlist. No
recovery grant, authority read/history, nonce consumption or mutation fence is
implemented here. The mandatory lifecycle-phase checker still refuses cleanup;
a copied execution grant or this plan cannot replace separate recovery authority.
"""

from datetime import timedelta

from announcement_runtime_evidence import canonical_digest
from announcement_runtime_lifecycle import check_lifecycle_ledger_claims
from announcement_runtime_provenance import _index, _unique_refs
from announcement_runtime_schema import LEDGER_LIMIT, require, utc


RECOVERY_PLAN_REF = "runtime-recovery-plan"


def check_recovery_plan_claims(receipt, evidence, now):
    """Compare a separately bounded owned-DB cleanup plan with exact ledgers.

    Compose with, never instead of, authority/telemetry/state/phase/drain checks.
    Only prior absent/present DB projections are supported, one cleanup mutation
    per resource. Message cleanup, extra recovery reads, process restoration and
    full-row/CAS-token semantics remain unsupported. Nothing is authenticated.
    """
    check_lifecycle_ledger_claims(receipt, evidence, now)
    authority, lifecycle, attempt = receipt["authority"], receipt["lifecycle"], receipt["attempt"]
    resource_plan = evidence.resolve("runtime-resource-plan", "resource_plan_content")
    require(canonical_digest(resource_plan) == receipt["resources"]["plan"]["sha256"] and
            authority["resource_plan"] == receipt["resources"]["plan"], "recovery_content")
    plan = evidence.resolve(RECOVERY_PLAN_REF, "recovery_plan_content")
    require(canonical_digest(plan) == resource_plan["recovery_plan_sha256"], "recovery_content")
    require(plan["run"] == attempt["run"] and
            plan["contract"] == receipt["scope"]["contract"] and
            plan["policy"] == authority["policy"] and
            plan["implementation"] == authority["implementation"] and
            plan["candidate"] == receipt["scope"]["candidate"] and
            plan["control"] == receipt["control"]["version"] and
            plan["execution_ref"] == authority["execution"]["ref"] and
            plan["execution_nonce_ref"] == authority["execution"]["nonce_ref"] and
            plan["authority_epoch"] == authority["epoch"] and
            plan["resource_epoch"] == attempt["enforcement_epoch"], "recovery_scope")
    require(plan["served_before_sha256"] == canonical_digest(lifecycle["served_before"]) and
            plan["worker_before_sha256"] == canonical_digest(lifecycle["worker_before"]),
            "recovery_snapshot")
    # A plan cannot be offered as either kind of execution authority. The opaque
    # restoration reference deliberately remains unresolved by this comparison.
    require(lifecycle["restoration_authority_ref"] not in (
        RECOVERY_PLAN_REF, authority["execution"]["ref"]), "recovery_authority_separation")
    start, expiry = utc(plan["not_before"]), utc(plan["expires_at"])
    require(timedelta(0) < expiry - start <= timedelta(minutes=60), "recovery_lease")
    restored = utc(lifecycle["served_after"]["observed_at"])
    require(utc(attempt["started_at"]) <= start <= restored < expiry and
            restored <= utc(attempt["ended_at"]) <= now, "recovery_window")
    operations = plan["operations"]
    _unique_refs([entry["operation_id"] for entry in operations])
    _unique_refs([entry["resource_ref"] for entry in operations])
    require(sum(entry["maximum_count"] for entry in operations) <= LEDGER_LIMIT,
            "recovery_budget")
    cleanup = lifecycle["cleanup"]
    require([entry["operation_id"] for entry in operations] ==
            [claim["operation_id"] for claim in cleanup], "recovery_operations")
    resources = _index(receipt["resources"]["entries"], "ref", "resource_inventory")
    effects = _index(receipt["observations"]["actual_effects"], "operation_id", "operation_replay")
    # The recovery owner is the declared resource controller, never a candidate,
    # executor, caller or reviewer merely adopting a convenient owner string.
    owner = plan["owner"]
    require(all(owner["subject_id"] != principal["subject_id"] for principal in (
        authority["author"], authority["executor"], authority["caller"],
        *(decision["principal"] for decision in authority["decisions"]))), "recovery_owner")
    for entry, claim in zip(operations, cleanup):
        resource = resources.get(entry["resource_ref"])
        require(resource is not None and resource["identity"]["kind"] == "database",
                "recovery_resource")
        identity = resource["identity"]
        require(entry["identity_sha256"] == canonical_digest(identity) and
                entry["key_sha256"] == identity["key_sha256"] and
                entry["owner_ref"] == resource["owner"] and
                entry["restore_state_ref"] == resource["before_state_ref"], "recovery_resource")
        ownership = evidence.resolve(resource["owner"], "resource_owner")
        require(ownership == {"principal": owner,
            "execution_ref": plan["execution_ref"], "nonce_ref": plan["execution_nonce_ref"]},
            "recovery_owner")
        effect = effects[claim["operation_id"]]
        require(claim["resource_ref"] == entry["resource_ref"] and
                effect["method"] == entry["method"] and
                claim["expected_count"] == claim["actual_count"] ==
                entry["expected_count"] == entry["maximum_count"] == 1, "recovery_budget")
        observed = utc(claim["observed_at"])
        require(start <= observed < expiry and observed <= restored, "recovery_operation_time")
        before = evidence.resolve(resource["before_state_ref"], "resource_state")["claim"]
        require(entry["method"] == ("db_delete_owned" if before["presence"] == "absent"
                                    else "db_cas"), "recovery_method")
        final = evidence.resolve(resource["after_state_ref"], "resource_state")
        require(utc(final["observed_at"]) < expiry, "recovery_final_time")
