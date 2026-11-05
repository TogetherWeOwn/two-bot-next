"""Restricted offline fixture/input comparisons; never admission or consent proof.

Literal keys must stay in restricted evidence. No serializer, public packet
producer, root, credential or effect adapter is provided here. Scheduler,
transitive inventories, stored-case/effect semantics and lifecycle verification
remain separate mandatory work. Consistent fabricated claims grant nothing.
"""

from collections import Counter
from datetime import datetime, timezone

from announcement_runtime_evidence import EvidenceIndex, canonical_digest
from announcement_runtime_resources import PLAN_REF, TABLES, check_resource_graph_claims
from announcement_runtime_schema import COMMAND_GATES, require, utc


INPUT_PLAN_REF = "runtime-input-plan"


def check_input_graph_claims(receipt, evidence, now):
    """Resolve content-bound literal keys, consent and finite interaction plans.

    This scoped consistency check returns no capability. In particular consent
    is not authenticated, a source/content digest is not an immutable feed, and
    listed interactions are not performed observations. Live replay interactions
    remain explicitly unsupported rather than acquiring a fresh ACK/write budget.
    """
    require(type(evidence) is EvidenceIndex, "evidence_index")
    require(type(now) is datetime and now.tzinfo is timezone.utc, "clock")
    check_resource_graph_claims(receipt, evidence, now)
    resource_plan = evidence.resolve(PLAN_REF, "resource_plan_content")
    plan = evidence.resolve(INPUT_PLAN_REF, "input_plan_content")
    require(canonical_digest(plan) == resource_plan["input_plan_sha256"], "input_content")
    fixtures, attempt = receipt["fixtures"], receipt["attempt"]
    slots = resource_plan["slots"]
    entries = {x["ref"]: x for x in receipt["resources"]["entries"]}

    def content(pin, kind):
        data = evidence.resolve(pin["ref"], kind)
        require(canonical_digest(data) == pin["sha256"], "input_content")
        return data

    # Compare full typed PK contents, not author-chosen equal key digests.
    bindings = plan["keys"]
    require(len({x["resource_ref"] for x in bindings}) == len(bindings) and
            len({x["key"]["ref"] for x in bindings}) == len(bindings) and
            {x["resource_ref"] for x in bindings} == {
                ref for ref, entry in entries.items() if entry["kind"] in TABLES}, "key_inventory")
    keys = {}
    for binding in bindings:
        ref = binding["resource_ref"]
        key = content(binding["key"], "literal_db_key")
        require(key["table"] == entries[ref]["identity"]["table"] and
                canonical_digest(key) == entries[ref]["identity"]["key_sha256"], "literal_key")
        keys[ref] = key
    require(len({canonical_digest(x) for x in keys.values()}) == len(keys), "literal_key_alias")
    app = entries[slots["application_ref"]]["identity"]
    actor = entries[slots["actor_ref"]]["identity"]
    channel = entries[slots["feed_channel_ref"]]["identity"]
    event = entries[slots["event_ref"]]["identity"]
    guild = app["guild_id"]
    feed = plan["feed"]
    require(feed["resource_ref"] == slots["feed_ref"] and feed["guild_id"] == guild and
            feed["channel_id"] == channel["object_id"] and
            feed["source_ref"] == slots["source_ref"] and
            feed["nonce_ref"] == fixtures["nonce_ref"], "feed_input")
    require(keys[slots["delivery_ref"]] == {"table": "feed_deliveries",
                "feed_id": keys[slots["feed_ref"]]["id"], "item_key": feed["item_key"]},
            "delivery_key")
    require(keys[slots["setting_ref"]] == {"table": "guild_settings", "guild_id": guild,
                                          "key": "TWO_ANNOUNCEMENTS"}, "setting_key")
    require(keys[slots["rsvp_ref"]] == {"table": "event_rsvps", "guild_id": guild,
                "event_id": event["object_id"], "user_id": actor["object_id"]}, "rsvp_key")

    require(plan["consent"]["ref"] == fixtures["consent_ref"] and
            plan["interactions"]["ref"] == fixtures["interaction_plan_ref"], "input_reference")
    consent = content(plan["consent"], "test_consent")
    execution = receipt["authority"]["execution"]
    require(consent["actor_ref"] == slots["actor_ref"] and
            consent["actor_id"] == actor["object_id"] and
            consent["application_id"] == app["application_id"] and
            consent["guild_id"] == guild and consent["execution_ref"] == execution["ref"] and
            consent["nonce_ref"] == execution["nonce_ref"] and
            consent["outcome"] == "granted", "test_consent")
    start, end = utc(attempt["started_at"]), utc(attempt["ended_at"])
    require(utc(consent["granted_at"]) <= start and
            utc(consent["not_before"]) <= start < end < utc(consent["expires_at"]), "consent_window")

    interactions = content(plan["interactions"], "interaction_plan")["interactions"]
    cases = {x["case_id"]: x for x in receipt["observations"]["cases"]}
    require(len(cases) == len(receipt["observations"]["cases"]) and
            len(set(fixtures["case_ids"])) == len(fixtures["case_ids"]) and
            set(fixtures["case_ids"]) == cases.keys(), "case_inventory")
    require(len({x["case_id"] for x in interactions}) == len(interactions) and
            len({x["interaction_id"] for x in interactions}) == len(interactions), "interaction_replay")
    consent_cases = consent["case_ids"]
    require(len(set(consent_cases)) == len(consent_cases) and
            set(consent_cases) == {x["case_id"] for x in interactions}, "consent_cases")
    require({x["command"] for x in interactions if x["phase"] == "off"} == COMMAND_GATES,
            "interaction_gates")
    totals = Counter()
    positive = []
    for interaction in interactions:
        case = cases.get(interaction["case_id"])
        require(case is not None and interaction["phase"] == case["phase"] and
                case["input_ref"] == plan["interactions"]["ref"], "interaction_case")
        require(interaction["application_id"] == app["application_id"] and
                interaction["actor_ref"] == slots["actor_ref"] and
                interaction["channel_ref"] == slots["control_channel_ref"] and
                interaction["event_ref"] == slots["event_ref"], "interaction_resource")
        require(set((slots["actor_ref"], slots["control_channel_ref"], slots["event_ref"])) <=
                set(case["resource_refs"]), "interaction_resource")
        require(interaction["phase"] != "replay", "unsupported_interaction_replay")
        if interaction["phase"] == "on":
            require(interaction["command"] == "rsvp", "unsupported_positive_interaction")
            positive.append(interaction)
            require(interaction["edit_count"] == 1, "interaction_budget")
        else:
            require(interaction["edit_count"] == 0, "interaction_budget")
        # Exactly one wire ACK, at most one original-response edit. Retries are
        # attempts, not extra budget hidden behind a logical-success counter.
        require(interaction["maximum_wire_attempts"] ==
                interaction["ack_count"] + interaction["edit_count"], "interaction_budget")
        totals["interaction_ack"] += interaction["ack_count"]
        totals["interaction_edit"] += interaction["edit_count"]
    require(len(positive) == 1, "rsvp_input")
    rsvp = plan["rsvp"]
    require(rsvp["resource_ref"] == slots["rsvp_ref"] and
            rsvp["audit_ref"] == slots["rsvp_audit_ref"] and
            rsvp["interaction_id"] == positive[0]["interaction_id"] and
            keys[slots["rsvp_audit_ref"]] == {"table": "announcements_audit_log",
                "id": "rsvp:" + rsvp["interaction_id"]}, "rsvp_input")
    budgets = {(x["resource_ref"], x["method"]): x for x in resource_plan["operation_budgets"]}
    for method, total in totals.items():
        budget = budgets.get((slots["control_channel_ref"], method))
        require(budget is not None and budget["expected_count"] ==
                budget["maximum_count"] == total, "interaction_budget")
    require(all(x["maximum_count"] == 0 or x["resource_ref"] == slots["control_channel_ref"]
                for x in resource_plan["operation_budgets"]
                if x["method"] in ("interaction_ack", "interaction_edit")), "interaction_budget")
