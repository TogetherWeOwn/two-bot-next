"""Offline finite recovery-plan comparisons, NOT recovery authority or execution.

The resource-plan digest binds a closed plan, not a caller allowlist. Separate
scope/grant/decision/nonce claims are compared, never authenticated or consumed.
Recovery reads/history/checkpoints are separate offline claims; installation
revalidation and mutation fences remain unimplemented. The mandatory lifecycle-
phase checker still refuses cleanup, regardless of these comparisons.
"""

from datetime import timedelta

from announcement_runtime_authority import _authority_source_claims, _decisions
from announcement_runtime_evidence import canonical_digest
from announcement_runtime_lifecycle import check_lifecycle_ledger_claims
from announcement_runtime_provenance import _index, _unique_refs
from announcement_runtime_schema import LEDGER_LIMIT, require, utc


RECOVERY_PLAN_REF = "runtime-recovery-plan"
RECOVERY_CHECKPOINTS_REF = "runtime-recovery-checkpoints"


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


def check_recovery_grant_claims(receipt, evidence, now):
    """Compare a separate grant and nonce claim; NOT a recovery authority graph.

    This composes plan consistency with declared source/consumer/principal and
    ordered decision bindings. Runtime authority/provenance checks remain
    mandatory upstream. No current recovery reads, complete history, installation
    revalidation or boundary fence is proved, so lifecycle cleanup still refuses.
    In particular successful comparison cannot make an expired grant usable.
    """
    check_recovery_plan_claims(receipt, evidence, now)
    authority, lifecycle, attempt = receipt["authority"], receipt["lifecycle"], receipt["attempt"]
    source, principals, consumers, stages = _authority_source_claims(receipt, evidence)
    plan = evidence.resolve(RECOVERY_PLAN_REF, "recovery_plan_content")
    grant_ref = lifecycle["restoration_authority_ref"]
    grant = evidence.resolve(grant_ref, "recovery_authority")
    require(grant["ref"] == grant_ref and grant["source_ref"] == authority["source_ref"] and
            grant["run"] == attempt["run"] and
            grant["resource_epoch"] == attempt["enforcement_epoch"] and
            grant["owner"] == plan["owner"] == principals["controller"], "recovery_grant")
    # The separate recovery epoch may differ from runtime's epoch. Exact recovery
    # decisions and the claimed nonce use must all agree on it; no epoch is minted.
    scope = evidence.resolve(grant["scope_ref"], "recovery_scope")
    require(scope == {
        "purpose": "owned_recovery_only", "runtime_scope_sha256": authority["scope_sha256"],
        "source_sha256": canonical_digest(source), "consumers_sha256": canonical_digest(consumers),
        "plan_ref": RECOVERY_PLAN_REF, "plan_sha256": canonical_digest(plan),
        "resource_plan": receipt["resources"]["plan"], "owner": plan["owner"],
        "run": attempt["run"], "epoch": grant["epoch"],
        "resource_epoch": attempt["enforcement_epoch"],
    } and canonical_digest(scope) == grant["scope_sha256"], "recovery_grant_scope")
    require(grant["not_before"] == plan["not_before"] and
            grant["expires_at"] == plan["expires_at"], "recovery_grant_lease")
    execution = authority["execution"]
    refs = [grant_ref, grant["scope_ref"], grant["nonce_ref"], grant["consumption_ref"]]
    require(len(refs) == len(set(refs)) and not set(refs).intersection(
        (execution["ref"], execution["nonce_ref"], execution["consumption_ref"],
         RECOVERY_PLAN_REF, authority["source_ref"])), "recovery_grant_separation")
    decisions = grant["decisions"]
    _decisions(decisions, grant["scope_sha256"], grant["epoch"], principals,
               utc(grant["not_before"]), stages, "recovery")
    prior = list(authority["decisions"])
    for ref in authority["installation_approval_refs"]:
        prior.extend(evidence.resolve(ref, "installation_approval")["decisions"])
    for field in ("ref", "decision_id", "stage_id", "participant_id"):
        require(not {d[field] for d in decisions}.intersection(d[field] for d in prior),
                "recovery_decision_separation")
    for decision in decisions:
        record = evidence.resolve(decision["ref"], "authority_decision")
        require(record == {"claim": decision, "source_ref": authority["source_ref"],
            "security_round": decisions[0]["round"], "generation": grant["generation"]},
            "recovery_decision_binding")
    consumed = evidence.resolve(grant["consumption_ref"], "recovery_nonce_consumption")
    require(consumed == {
        "source_ref": authority["source_ref"], "recovery_ref": grant_ref,
        "scope_sha256": grant["scope_sha256"], "nonce_ref": grant["nonce_ref"],
        "run": attempt["run"], "owner_ref": plan["owner"]["ref"],
        "consumed_at": consumed["consumed_at"], "authority_epoch": grant["epoch"],
        "resource_epoch": attempt["enforcement_epoch"], "outcome": "consumed",
    }, "recovery_nonce_binding")
    first_cleanup = min(utc(claim["observed_at"]) for claim in lifecycle["cleanup"])
    require(utc(grant["not_before"]) <= utc(consumed["consumed_at"]) <= first_cleanup <
            utc(grant["expires_at"]), "recovery_nonce_time")


def _recovery_read(ref, role, checkpoint, grant, source, evidence, freshness_at):
    record = evidence.resolve(ref, "recovery_read")
    read = record["claim"]
    require(record["recovery_ref"] == grant["ref"] and read["ref"] == ref and
            read["consumer"] == role and record["consumer_ref"] == source[role + "_ref"],
            "recovery_read_binding")
    require(read["source_ref"] == grant["source_ref"] and
            read["scope_sha256"] == grant["scope_sha256"] and
            read["epoch"] == grant["epoch"] and
            read["resource_epoch"] == grant["resource_epoch"] and
            read["effective_decision_refs"] == [d["ref"] for d in grant["decisions"]],
            "recovery_read_scope")
    require(utc(grant["not_before"]) <= utc(read["read_at"]) <=
            utc(checkpoint["checked_at"]) <= freshness_at and
            freshness_at - utc(read["read_at"]) <= timedelta(seconds=60), "stale_recovery_read")
    history = evidence.resolve(record["history_ref"], "recovery_history")
    require(history["recovery_ref"] == grant["ref"] and
            canonical_digest(history) == read["history_sha256"] and
            history["source_ref"] == grant["source_ref"] and
            history["scope_sha256"] == grant["scope_sha256"] and
            history["epoch"] == grant["epoch"] and
            history["resource_epoch"] == grant["resource_epoch"] and
            history["generation"] == grant["generation"] and
            history["effective_decision_refs"] == read["effective_decision_refs"] and
            history["ended_at"] == read["read_at"] and history["coverage"] == "complete",
            "recovery_history")
    events = history["events"]
    require(history["first_sequence"] == 1 and history["last_sequence"] == len(events) and
            [e["sequence"] for e in events] == list(range(1, len(events) + 1)),
            "recovery_history_gap")
    lower, upper = utc(history["started_at"]), utc(history["ended_at"])
    decisions = grant["decisions"]
    require(lower < utc(decisions[0]["decided_at"]) and
            all(utc(d["decided_at"]) <= upper for d in decisions) and
            [e["observed_at"] for e in events] == sorted(e["observed_at"] for e in events),
            "recovery_history_time")
    for event in events:
        require(lower <= utc(event["observed_at"]) < utc(decisions[0]["decided_at"]),
                "invalidated_recovery")
        _unique_refs(event["invalidated_decision_refs"])
        require(not set(event["invalidated_decision_refs"]).intersection(
            d["ref"] for d in decisions), "invalidated_recovery")
    return history


def check_recovery_read_claims(receipt, evidence, now):
    """Compare named cleanup/final reads and histories, NOT a boundary fence.

    Fixed-reference inventory is inert same-attempt data, never a trust root or
    acquisition adapter. Installation revalidation and all three mutation-phase
    consumers remain mandatory and unfinished; cleanup still refuses there.
    """
    check_recovery_grant_claims(receipt, evidence, now)
    authority, lifecycle, attempt = receipt["authority"], receipt["lifecycle"], receipt["attempt"]
    grant = evidence.resolve(lifecycle["restoration_authority_ref"], "recovery_authority")
    source = evidence.resolve(authority["source_ref"], "authority_source")
    inventory = evidence.resolve(RECOVERY_CHECKPOINTS_REF, "recovery_checkpoints")
    require(inventory["recovery_ref"] == grant["ref"] and inventory["run"] == attempt["run"],
            "recovery_checkpoint_scope")
    checkpoints = inventory["checkpoints"]
    _unique_refs([c["ref"] for c in checkpoints])
    require(not {c["ref"] for c in checkpoints}.intersection(
        c["ref"] for c in attempt["checkpoints"]), "recovery_checkpoint_separation")
    require([c["phase"] for c in checkpoints] == ["cleanup"] * (len(checkpoints) - 1) + ["final"] and
            [c["checked_at"] for c in checkpoints] == sorted(c["checked_at"] for c in checkpoints) and
            checkpoints[-1]["checked_at"] == attempt["ended_at"] and
            not checkpoints[-1]["operation_id_refs"], "recovery_checkpoint_inventory")
    claims = _index(lifecycle["cleanup"], "operation_id", "lifecycle_operation")
    operations = [op for c in checkpoints[:-1] for op in c["operation_id_refs"]]
    _unique_refs(operations)
    require(set(operations) == claims.keys() and
            all(c["operation_id_refs"] for c in checkpoints[:-1]), "recovery_checkpoint_operations")
    consumed = evidence.resolve(grant["consumption_ref"], "recovery_nonce_consumption")
    used_reads, histories = set(), []
    require(now < utc(grant["expires_at"]), "recovery_expiry")
    for checkpoint in checkpoints:
        checked = utc(checkpoint["checked_at"])
        require(checkpoint["authority_epoch"] == grant["epoch"] and
                checkpoint["resource_epoch"] == grant["resource_epoch"], "recovery_checkpoint_scope")
        require(utc(consumed["consumed_at"]) <= checked < utc(grant["expires_at"]),
                "recovery_checkpoint_time")
        freshness_at = now if checkpoint is checkpoints[-1] else checked
        for op in checkpoint["operation_id_refs"]:
            claim = claims[op]
            require(claim["authority_ref"] == checkpoint["ref"], "recovery_named_checkpoint")
            observed = utc(claim["observed_at"])
            require(checked <= observed < utc(checkpoints[-1]["checked_at"]),
                    "recovery_checkpoint_time")
            freshness_at = max(freshness_at, observed)
        pair = []
        for role in ("controller", "verifier"):
            ref = checkpoint[role + "_read_ref"]
            require(ref not in used_reads, "recovery_read_replay")
            used_reads.add(ref)
            pair.append(_recovery_read(ref, role, checkpoint, grant, source, evidence, freshness_at))
        require(pair[0] == pair[1], "recovery_read_disagreement")
        histories.extend(pair)
    histories.sort(key=lambda h: h["ended_at"])
    for previous, current in zip(histories, histories[1:]):
        require(previous["started_at"] == current["started_at"] and
                previous["events"] == current["events"][:len(previous["events"])] and
                len(previous["events"]) <= len(current["events"]), "recovery_history_rewrite")
        require(all(utc(e["observed_at"]) > utc(previous["ended_at"])
                    for e in current["events"][len(previous["events"]):]), "recovery_history_backfill")
