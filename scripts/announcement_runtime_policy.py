"""Offline refusal predicates for the announcement candidate lane.

These checks reject inconsistent claims. They cannot authenticate those claims
or supply the missing external enforcement. In particular, successful offline
checks never return a runtime capability; runtime_preflight always refuses.
"""

from collections import Counter
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone

from announcement_runtime_schema import (
    AUTHORITY, CHECKPOINT, COVERAGE, EFFECT, LEDGER_LIMIT, RESOURCE, Refusal,
    _validate, require, utc,
)


@dataclass(frozen=True)
class CapabilityGap:
    capability: str
    owner: str


# This is an implementation gap ledger, not a configurable enablement list.
# Installing an adapter requires separately reviewed code and authorization.
CAPABILITY_GAPS = (
    CapabilityGap("authenticated_controller_authority", "engineering_lead"),
    CapabilityGap("independently_provisioned_verifier_root", "engineering_lead"),
    CapabilityGap("authority_and_resource_epoch_mutation_fence", "engineering_lead"),
    CapabilityGap("one_use_nonce_in_trusted_state", "engineering_lead"),
    CapabilityGap("credential_and_all_path_egress_containment", "engineering_lead"),
    CapabilityGap("below_candidate_database_key_and_trigger_boundary", "engineering_lead"),
    CapabilityGap("complete_startup_retry_queue_inventory", "engineering_lead"),
    CapabilityGap("independent_full_window_effect_observation", "engineering_lead"),
    CapabilityGap("literal_test_resources_and_synthetic_ledger", "engineering_lead"),
    CapabilityGap("all_writer_full_window_lifecycle_fence", "engineering_lead"),
    CapabilityGap("trusted_installation_and_bootstrap", "existing_installer_owner"),
    CapabilityGap("candidate_compatible_readiness_clearance", "readiness_owner"),
)


def runtime_preflight():
    """Refuse before credentials, setup, build, rollout, effects or cleanup.

    No caller-supplied adapter, root, boolean, allowlist, token or resource plan
    can enable this entry point. The installed capability set is empty.
    """
    raise Refusal("unsupported")


def check_lease(not_before, expires_at, now):
    """Pure time predicate; caller timestamps do not establish authority."""
    require(type(now) is datetime and now.tzinfo is timezone.utc, "clock")
    start, end = utc(not_before), utc(expires_at)
    require(timedelta(0) < end - start <= timedelta(minutes=60), "lease")
    require(start <= now < end, "expiry")
    return start, end


def check_authority_claims(authority, checkpoint, now):
    """Reject malformed/stale/non-independent authority claims, not authenticate.

    The future installed controller and independent verifier must each acquire
    current records from their independently provisioned fixed root. Comparing
    copies here cannot stand in for those reads or an atomic mutation fence.
    """
    _validate(authority, AUTHORITY)
    _validate(checkpoint, CHECKPOINT)
    start, end = check_lease(authority["execution"]["not_before"],
                             authority["execution"]["expires_at"], now)
    checked_at = utc(checkpoint["checked_at"])
    require(start <= checked_at <= now < end, "checkpoint_time")
    require(now - checked_at <= timedelta(seconds=60), "stale_checkpoint")
    decisions = authority["decisions"]
    require([x["role"] for x in decisions] == ["security", "qa"], "decision_order")
    security, qa = decisions
    require(security["ref"] != qa["ref"] and
            security["decision_id"] != qa["decision_id"], "decision_identity")
    require(security["stage_id"] != qa["stage_id"] and
            security["participant_id"] != qa["participant_id"], "decision_identity")
    require(security["decision_id"] == security["security_decision_id"] ==
            qa["security_decision_id"], "security_round_binding")
    require(utc(security["decided_at"]) < utc(qa["decided_at"]) <= checked_at,
            "decision_order")
    reviewers = [x["principal"]["subject_id"] for x in decisions]
    excluded = {authority[x]["subject_id"] for x in ("author", "executor", "caller")}
    require(len(excluded) == 3 and len(set(reviewers)) == 2 and
            not excluded.intersection(reviewers), "reviewer_independence")
    require(all(x["scope_sha256"] == authority["scope_sha256"] and
                x["epoch"] == authority["epoch"] for x in decisions), "decision_scope")
    reads = authority["reads"]
    require(len({x["ref"] for x in reads}) == len(reads), "read_identity")
    by_ref = {x["ref"]: x for x in reads}
    chosen = []
    for consumer in ("controller", "verifier"):
        read = by_ref.get(checkpoint[consumer + "_read_ref"])
        require(read is not None and read["consumer"] == consumer, "consumer_read")
        require(read["source_ref"] == authority["source_ref"] and
                read["scope_sha256"] == authority["scope_sha256"] and
                read["epoch"] == checkpoint["authority_epoch"] == authority["epoch"] and
                read["resource_epoch"] == checkpoint["resource_epoch"], "read_scope")
        require(read["effective_decision_refs"] == [x["ref"] for x in decisions],
                "effective_decisions")
        read_at = utc(read["read_at"])
        require(start <= read_at <= checked_at <= now and
                now - read_at <= timedelta(seconds=60), "stale_read")
        chosen.append(read)
    require(chosen[0]["history_sha256"] == chosen[1]["history_sha256"],
            "authority_disagreement")


def check_effect_reconciliation(expected, actual, coverage, resources, *,
                                started_at, ended_at, now):
    """Reconcile independently supplied operation multisets and coverage claims.

    This is accounting, NOT an egress/DB enforcement boundary or proof of
    independent observation. Exact resource plans and observers still need
    authenticated external binding. Unknown or blocked attempts stay adverse.
    """
    require(type(now) is datetime and now.tzinfo is timezone.utc, "clock")
    require(type(expected) is list and type(actual) is list and
            len(expected) <= LEDGER_LIMIT and len(actual) <= LEDGER_LIMIT, "ledger_size")
    _validate(coverage, COVERAGE)
    require(type(resources) is list and 0 < len(resources) <= 256, "resource_inventory")
    for resource in resources:
        _validate(resource, RESOURCE)
        require(resource["expected_count"] <= resource["maximum_count"], "budget")
        require(len(set(resource["allowed_operations"])) ==
                len(resource["allowed_operations"]), "resource_operations")
    require(len({x["ref"] for x in resources}) == len(resources), "resource_inventory")
    by_ref = {x["ref"]: x for x in resources}
    start, end = utc(started_at), utc(ended_at)
    require(start <= end <= now, "effect_window")
    require(coverage["result"] == "complete" and utc(coverage["started_at"]) <= start and
            end <= utc(coverage["ended_at"]) <= now, "telemetry")
    require(coverage["first_sequence"] == 1 and coverage["last_sequence"] == len(actual),
            "watermark")
    for ledger in (expected, actual):
        for effect in ledger:
            _validate(effect, EFFECT)
            require(effect["outcome"] == "accepted" and effect["accepted_count"] == 1,
                    "uncertain_effect")
            resource = by_ref.get(effect["resource_ref"])
            require(resource is not None and effect["method"] in resource["allowed_operations"],
                    "unplanned_effect")
            observed = utc(effect["observed_at"])
            require(start <= observed <= end and
                    utc(resource["not_before"]) <= observed < utc(resource["expires_at"]),
                    "effect_time")
        require(len({x["operation_id"] for x in ledger}) == len(ledger), "operation_replay")
    require([x["sequence"] for x in actual] == list(range(1, len(actual) + 1)),
            "sequence_gap")
    require([x["observed_at"] for x in actual] == sorted(x["observed_at"] for x in actual),
            "sequence_time")

    def signature(effect):
        # Times and sequence order are checked independently. All other exact
        # receipt fields, including platform/DB/served references, must agree.
        return tuple((key, tuple(value) if type(value) is list else value)
                     for key, value in sorted(effect.items())
                     if key not in ("sequence", "observed_at"))

    require(Counter(map(signature, expected)) == Counter(map(signature, actual)),
            "effect_multiset")
    counts = Counter(x["resource_ref"] for x in actual)
    for resource in resources:
        require(counts[resource["ref"]] == resource["expected_count"] <=
                resource["maximum_count"], "budget")
    for effect in actual:
        if effect["method"].startswith("db_"):
            require(bool(coverage["transaction_receipt_refs"]), "missing_transaction_receipts")
            if effect["method"] != "db_read":
                require(bool(effect["db_transition_refs"]), "missing_transition_receipts")
        else:
            require(bool(coverage["wire_receipt_refs"]), "missing_wire_receipts")
        if effect["method"] in ("feed_post", "interaction_edit"):
            require(bool(coverage["platform_receipt_refs"]) and
                    bool(effect["platform_id_refs"]), "missing_platform_receipts")


LIFECYCLE_PHASES = (
    "snapshot", "setup", "rollout", "readiness", "synthetics", "off", "on", "replay",
    "final_off", "drain", "cleanup", "restoration", "verified",
)


class LifecycleAccounting:
    """Offline state-machine model; not a distributed staging lock.

    A missing real full-window lock or leaf-lock graph cannot be repaired with
    this object. Abort is terminal for qualification; it never resets claims or
    reopens admission to finish a test. Recovery needs separate authority.
    """

    def __init__(self):
        self._position = -1
        self._aborted = False

    def observe(self, phase):
        require(not self._aborted, "aborted")
        require(self._position + 1 < len(LIFECYCLE_PHASES) and
                phase == LIFECYCLE_PHASES[self._position + 1], "lifecycle_order")
        self._position += 1

    def abort(self):
        self._aborted = True

    def require_complete(self):
        require(not self._aborted and self._position == len(LIFECYCLE_PHASES) - 1,
                "incomplete_lifecycle")
