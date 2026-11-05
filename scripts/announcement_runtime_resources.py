"""Offline resource/state/budget claim comparisons; never runtime admission.

Content-bound claims cannot prove a test resource exists, authenticate a literal
key map, establish isolation or grant any operation. This layer resolves only its
closed resource graph. Stored-case, effect/telemetry and lifecycle semantics,
complete transitive inventories and independent external enforcement remain
mandatory separate checks. runtime_preflight() remains unsupported.
"""

from collections import Counter
from datetime import datetime, timezone

from announcement_runtime_evidence import EvidenceIndex, canonical_digest
from announcement_runtime_policy import check_lease
from announcement_runtime_schema import LEDGER_LIMIT, require, utc, validate_receipt_shape


PLAN_REF = "runtime-resource-plan"
IDENTITY_MAP_REF = "runtime-identity-map"

# This is a type-to-method ceiling, not an allowlist or an execution plan.
# Every actual count still needs an exact content-bound per-method budget.
METHOD_CEILINGS = {
    "application": {"gateway_identify", "gateway_heartbeat", "gateway_read",
                    "guild_commands_replace"},
    "actor": {"member_get"},
    "channel": {"feed_post", "interaction_ack", "interaction_edit", "history_get",
                "message_delete_owned"},
    "event": {"event_get"},
    "feed": {"db_read", "db_insert", "db_cas", "db_delete_owned"},
    "delivery": {"db_read", "db_insert", "db_cas", "db_delete_owned"},
    "setting": {"db_read", "db_insert", "db_cas", "db_delete_owned"},
    "rsvp": {"db_read", "db_insert", "db_cas", "db_delete_owned"},
    "audit": {"db_read", "db_insert", "db_delete_owned"},
    "source": {"source_get"}, "synthetic": {"synthetic_request"},
}
TABLES = {"feed": "feed_relays", "delivery": "feed_deliveries",
          "setting": "guild_settings", "rsvp": "event_rsvps",
          "audit": "announcements_audit_log"}
SLOT_KINDS = {
    "application_ref": "application", "actor_ref": "actor",
    "control_channel_ref": "channel", "feed_channel_ref": "channel", "event_ref": "event",
    "feed_ref": "feed", "delivery_ref": "delivery", "setting_ref": "setting",
    "rsvp_ref": "rsvp", "rsvp_audit_ref": "audit", "source_ref": "source",
}


def _index(records, field, code):
    result = {x[field]: x for x in records}
    require(len(result) == len(records), code)
    return result


def check_resource_graph_claims(receipt, evidence, now):
    """Reject inconsistent content-bound identities, ownership, states and caps.

    All inputs are untrusted claims. Even coherent fabricated plan, isolation and
    observer records return no capability. No record selects a root or adapter.
    This function is not a complete receipt verifier; in particular a matching
    identity digest does not reveal or authenticate restricted literal DB keys.
    """
    validate_receipt_shape(receipt)
    require(type(evidence) is EvidenceIndex, "evidence_index")
    require(type(now) is datetime and now.tzinfo is timezone.utc, "clock")
    resources, fixtures, authority, attempt = (receipt[x] for x in (
        "resources", "fixtures", "authority", "attempt"))
    evidence.require_run(attempt["run"])
    lease_start, lease_end = check_lease(authority["execution"]["not_before"],
                                        authority["execution"]["expires_at"], now)
    start, end = utc(attempt["started_at"]), utc(attempt["ended_at"])
    rollout = utc(receipt["rollout"]["started_at"])
    require(lease_start <= start <= rollout < end <= now < lease_end, "resource_window")
    for pin in (resources["plan"], resources["identity_map"]):
        evidence.bind(pin, "document")
    evidence.bind(resources["inventory_manifest"], "manifest")
    require(resources["plan"] == authority["resource_plan"], "resource_plan")
    plan = evidence.resolve(PLAN_REF, "resource_plan_content")
    identity_map = evidence.resolve(IDENTITY_MAP_REF, "identity_map_content")
    require(canonical_digest(plan) == resources["plan"]["sha256"] and
            canonical_digest(identity_map) == resources["identity_map"]["sha256"],
            "resource_content")
    for key in ("entries", "states", "identity_map", "inventory_manifest"):
        require(plan[key] == resources[key], "resource_plan")
    require(lease_start <= utc(plan["not_before"]) <= start and
            end < utc(plan["expires_at"]) <= lease_end, "resource_window")
    entries = _index(resources["entries"], "ref", "resource_inventory")
    states = _index(resources["states"], "ref", "state_inventory")
    bindings = _index(identity_map["bindings"], "resource_ref", "identity_map")
    require(bindings.keys() == entries.keys(), "identity_map")
    slots = plan["slots"]
    require(len(set(slots.values())) == len(slots), "resource_slots")
    for slot, kind in SLOT_KINDS.items():
        require(slots[slot] in entries and entries[slots[slot]]["kind"] == kind, "resource_slots")
    for field in ("feed_ref", "setting_ref", "rsvp_ref"):
        require(fixtures[field] == slots[field], "fixture_binding")
    require(len({canonical_digest(x["identity"]) for x in entries.values()}) == len(entries),
            "resource_alias")

    source = evidence.resolve(authority["source_ref"], "authority_source")
    controller = evidence.resolve(source["controller_ref"], "authority_consumer")
    verifier = evidence.resolve(source["verifier_ref"], "authority_consumer")
    require(controller["role"] == "controller" and verifier["role"] == "verifier" and
            controller["principal"]["subject_id"] != verifier["principal"]["subject_id"] and
            controller["provisioning_ref"] != verifier["provisioning_ref"], "resource_observer")
    excluded = {authority[x]["subject_id"] for x in ("author", "executor", "caller")}
    require(verifier["principal"]["subject_id"] not in excluded and
            controller["principal"]["subject_id"] not in excluded, "resource_observer")

    def observer(ref, observed):
        record = evidence.resolve(ref, "observer")
        require(record["principal"] == verifier["principal"] and
                record["provisioning_ref"] == verifier["provisioning_ref"] and
                record["job_ref"] in attempt["job_refs"], "resource_observer")
        job = evidence.resolve(record["job_ref"], "job")
        require(job["source"] in ("independent_verifier", "external_observer") and
                job["head"] == receipt["control"]["version"]["commit"] and
                job["conclusion"] == "success" and
                start <= utc(job["started_at"]) <= observed <= utc(job["ended_at"]) <= end and
                job["producer_ref"] in attempt["producer_refs"], "resource_observer")
        producer = evidence.resolve(job["producer_ref"], "producer")
        require(producer["principal"] == record["principal"] and
                producer["workflow"] == receipt["control"]["workflow"], "resource_observer")

    used_states = set()
    used_isolation = set()
    db_boundary = None
    app = entries[slots["application_ref"]]["identity"]
    require(app["kind"] == "discord" and app["object_type"] == "application" and
            app["application_id"] == app["object_id"], "discord_identity")
    discord_ids = set()
    for ref, resource in entries.items():
        identity, kind, owner_ref = resource["identity"], resource["kind"], resource["owner"]
        methods = resource["allowed_operations"]
        require(len(methods) == len(set(methods)) and set(methods) <= METHOD_CEILINGS[kind],
                "resource_methods")
        require(utc(resource["not_before"]) == utc(plan["not_before"]) and
                utc(resource["expires_at"]) == utc(plan["expires_at"]), "resource_window")
        binding = bindings[ref]
        require(binding["identity"] == identity and binding["owner_ref"] == owner_ref,
                "identity_map")
        owner = evidence.resolve(owner_ref, "resource_owner")
        require(owner == {"principal": controller["principal"],
                "execution_ref": authority["execution"]["ref"],
                "nonce_ref": authority["execution"]["nonce_ref"]}, "resource_owner")
        isolation_ref = binding["isolation_ref"]
        require(isolation_ref not in used_isolation, "resource_isolation")
        used_isolation.add(isolation_ref)
        isolation = evidence.resolve(isolation_ref, "resource_isolation")
        require(isolation["resource_ref"] == ref and isolation["owner_ref"] == owner_ref and
                isolation["identity_sha256"] == canonical_digest(identity) and
                isolation["resource_epoch"] == attempt["enforcement_epoch"] and
                isolation["boundary"] == "all_paths_external" and
                isolation["credential_exposure"] == "mediated_only" and
                not isolation["production_memberships"], "resource_isolation")
        require(start <= utc(isolation["observed_at"]) <= rollout, "resource_baseline")
        observer(isolation["observer_ref"], utc(isolation["observed_at"]))
        for field in ("before_state_ref", "after_state_ref"):
            state_ref = resource[field]
            require(state_ref in states and state_ref not in used_states, "state_inventory")
            used_states.add(state_ref)
            state = evidence.resolve(state_ref, "resource_state")
            require(state["claim"] == states[state_ref] and
                    state["claim"]["ref"] == state_ref and
                    state["resource_ref"] == ref and state["owner_ref"] == owner_ref and
                    state["identity_sha256"] == canonical_digest(identity), "state_binding")
            observed = utc(state["observed_at"])
            require((start <= observed <= rollout if field == "before_state_ref" else
                     utc(receipt["rollout"]["ended_at"]) <= observed <= end), "state_time")
            observer(state["observer_ref"], observed)
            if kind in ("application", "actor", "channel", "event", "source"):
                require(state["claim"]["presence"] == "present", "fixture_conflict")
        if kind in TABLES:
            require(identity["kind"] == "database" and identity["table"] == TABLES[kind],
                    "database_identity")
            boundary = {key: value for key, value in identity.items()
                        if key not in ("table", "key_sha256")}
            require(db_boundary is None or boundary == db_boundary, "database_boundary")
            db_boundary = boundary
        elif kind in ("application", "actor", "channel", "event"):
            require(identity["kind"] == "discord" and identity["object_type"] == kind and
                    identity["application_id"] == app["application_id"] and
                    identity["guild_id"] == app["guild_id"] and
                    identity["object_id"] not in discord_ids, "discord_identity")
            discord_ids.add(identity["object_id"])
        else:
            require(identity["kind"] == kind, "resource_identity")
    require(used_states == states.keys(), "state_inventory")
    for slot in ("delivery_ref", "rsvp_ref", "rsvp_audit_ref"):
        require(states[entries[slots[slot]]["before_state_ref"]]["presence"] == "absent",
                "fixture_conflict")

    item = evidence.resolve(fixtures["item_ref"], "feed_item")
    require(item == {"source_ref": slots["source_ref"], "feed_ref": fixtures["feed_ref"],
        "item_ref": fixtures["item_ref"], "source_content_sha256": fixtures["source_content_sha256"],
        "item_content_sha256": fixtures["item_content_sha256"], "nonce_ref": fixtures["nonce_ref"]},
        "fixture_binding")
    source_identity = entries[slots["source_ref"]]["identity"]
    require(source_identity["item_ref"] == fixtures["item_ref"] and
            source_identity["content_sha256"] == fixtures["source_content_sha256"], "fixture_binding")

    # Synthetic dependencies must be explicit and disjoint from announcement
    # fixture objects; this does not verify the synthetic's requests or cleanup.
    fixture_refs = set(slots.values()) - {slots["application_ref"]}
    supporting = set()
    for resource in entries.values():
        if resource["kind"] != "synthetic":
            continue
        identity = resource["identity"]
        refs = identity["resource_refs"]
        require(len(refs) == len(set(refs)) and set(refs) <= entries.keys() and
                not set(refs) & fixture_refs and resource["ref"] not in refs, "synthetic_resources")
        actor = entries.get(identity["actor_ref"])
        require(actor is not None and actor["kind"] == "actor" and
                identity["actor_ref"] in refs, "synthetic_resources")
        require(all(entries[ref]["kind"] in ("application", "actor", "channel", "event")
                    for ref in refs), "synthetic_resources")
        supporting.update(refs)
    require(all(ref in slots.values() or resource["kind"] == "synthetic" or
                (ref in supporting and resource["kind"] in ("actor", "channel", "event"))
                for ref, resource in entries.items()), "extra_resource")

    budgets = plan["operation_budgets"]
    _index(budgets, "ref", "operation_budget")
    by_method = {(x["resource_ref"], x["method"]): x for x in budgets}
    require(len(by_method) == len(budgets) and by_method.keys() == {
        (ref, method) for ref, resource in entries.items()
        for method in resource["allowed_operations"]}, "operation_budget")
    require(all(x["expected_count"] <= x["maximum_count"] <= LEDGER_LIMIT for x in budgets) and
            sum(x["maximum_count"] for x in budgets) <= LEDGER_LIMIT, "budget")
    for ref, resource in entries.items():
        for field in ("expected_count", "maximum_count"):
            require(resource[field] == sum(x[field] for x in budgets if x["resource_ref"] == ref),
                    "operation_budget")

    def count(slot, method):
        budget = by_method.get((slots[slot], method))
        return 0 if budget is None else budget["maximum_count"]

    def exact(slot, method, total):
        budget = by_method.get((slots[slot], method))
        require(budget is not None and budget["expected_count"] == budget["maximum_count"] == total,
                "profile_budget")

    exact("feed_channel_ref", "feed_post", 1)
    exact("rsvp_ref", "db_insert", 1)
    exact("rsvp_audit_ref", "db_insert", 1)
    exact("delivery_ref", "db_insert", 1)
    exact("delivery_ref", "db_cas", 1)
    require(count("rsvp_ref", "db_cas") == 0 and count("setting_ref", "db_cas") >= 2 and
            sum(count("setting_ref", method) for method in
                ("db_insert", "db_cas", "db_delete_owned")) == 4, "profile_budget")
    require(all(x["expected_count"] == x["maximum_count"] for x in budgets
                if x["resource_ref"] == slots["setting_ref"] and x["method"] != "db_read"),
            "profile_budget")
    require(all(x["maximum_count"] == 0 or x["method"] != "feed_post" or
                x["resource_ref"] == slots["feed_channel_ref"] for x in budgets), "profile_budget")
    # No followups/DM method exists in the closed vocabulary. Command replacement
    # has at most a startup and a restoration attempt, not unbounded publication.
    require(count("application_ref", "guild_commands_replace") <= 2, "profile_budget")
    for ledger in (receipt["observations"]["expected_effects"],
                   receipt["observations"]["actual_effects"]):
        actual = Counter((x["resource_ref"], x["method"]) for x in ledger)
        require(actual.keys() <= by_method.keys(), "unplanned_effect")
        for key, budget in by_method.items():
            require(actual[key] == budget["expected_count"] <= budget["maximum_count"], "budget")
