"""Offline case/operation correspondence, NOT genuinely stored-data acceptance.

Composes scheduler/input and telemetry comparisons. Exact case membership and
operation order are necessary but cannot authenticate a request, prove source
selection, inspect real stored business values or replace lifecycle checks.
No new evidence root, serializer, effect adapter or runtime permission exists.
"""

from collections import Counter

from announcement_runtime_inputs import INPUT_PLAN_REF
from announcement_runtime_provenance import _index, _unique_refs
from announcement_runtime_resources import PLAN_REF
from announcement_runtime_scheduler import PHASE_VALUES, check_scheduler_graph_claims
from announcement_runtime_schema import require, utc
from announcement_runtime_telemetry import check_telemetry_claims


def check_case_effect_claims(receipt, evidence, now):
    """Reject borrowed, missing, repeated or wrong-phase case effects.

    The narrow profile supports the four feed windows plus exactly the declared
    interaction cases. Startup/synthetic/setup/cleanup operations remain in the
    full-window accounting but are NOT qualified by this scoped comparison.
    Unknown/blocked outcomes fail upstream, never become proof of suppression.
    """
    check_scheduler_graph_claims(receipt, evidence, now)
    check_telemetry_claims(receipt, evidence, now)
    observations, fixtures = receipt["observations"], receipt["fixtures"]
    plan = evidence.resolve(PLAN_REF, "resource_plan_content")
    inputs = evidence.resolve(INPUT_PLAN_REF, "input_plan_content")
    slots = plan["slots"]
    interactions = evidence.resolve(inputs["interactions"]["ref"], "interaction_plan")["interactions"]
    by_interaction = _index(interactions, "case_id", "case_inventory")
    cases = _index(observations["cases"], "case_id", "case_inventory")
    actual = _index(observations["actual_effects"], "operation_id", "operation_replay")
    expected = _index(observations["expected_effects"], "operation_id", "operation_replay")
    coverage = observations["coverage"][0]
    coverage_ref = coverage["ref"]
    platforms = {record["operation_id"]: record for record in
                 (evidence.resolve(ref, "platform_effect") for ref in coverage["platform_receipt_refs"])}
    feed_cases = [case for case in cases.values() if case["case_id"] not in by_interaction]
    phases = {phase for phase, _, _ in PHASE_VALUES}
    require(len(feed_cases) == len(phases) and {case["phase"] for case in feed_cases} == phases and
            all(case["input_ref"] == fixtures["scheduler_plan_ref"] for case in feed_cases),
            "feed_case_inventory")
    feed_resources = {slots[key] for key in ("feed_ref", "delivery_ref", "setting_ref",
                                              "feed_channel_ref", "source_ref")}
    interaction_resources = {slots[key] for key in ("actor_ref", "control_channel_ref", "event_ref")}
    used = []

    def signature(effect):
        return effect["resource_ref"], effect["method"]

    def pair(slot, method):
        return slots[slot], method

    def ordered(effects):
        # Sequence is the full-window attempted-operation order, not local case
        # list order or a same-second timestamp used to hide reversed operations.
        return sorted(effects, key=lambda effect: effect["sequence"])

    def before(left, right):
        require(left["sequence"] < right["sequence"], "case_operation_order")

    for case in cases.values():
        require(case["outcome"] == "pass" and case["coverage_ref"] == coverage_ref,
                "case_coverage")
        _unique_refs(case["resource_refs"])
        refs = case["actual_effect_refs"]
        _unique_refs(refs)
        _unique_refs(case["expected_effect_refs"])
        require(set(refs) == set(case["expected_effect_refs"]) and
                set(refs) <= actual.keys() and set(refs) <= expected.keys(), "case_effect_inventory")
        effects = ordered([actual[ref] for ref in refs])
        used.extend(refs)
        require(all(utc(case["started_at"]) <= utc(effect["observed_at"]) < utc(case["ended_at"]) and
                    effect["served_ref"] == case["served_ref"] and
                    effect["resource_ref"] in case["resource_refs"] for effect in effects),
                "case_effect_scope")
        require(all(utc(platforms[ref]["observed_at"]) < utc(case["ended_at"])
                    for ref in refs if ref in platforms), "case_platform_scope")
        counts = Counter(map(signature, effects))
        interaction = by_interaction.get(case["case_id"])
        if interaction is not None:
            positive = case["phase"] == "on"
            allowed_resources = interaction_resources | ({slots["rsvp_ref"], slots["rsvp_audit_ref"]}
                                                          if positive else set())
            require(set(case["resource_refs"]) == allowed_resources, "case_resources")
            required = Counter({pair("control_channel_ref", "interaction_ack"): 1})
            if positive:
                required.update({pair("control_channel_ref", "interaction_edit"): 1,
                    pair("actor_ref", "member_get"): 2, pair("event_ref", "event_get"): 2,
                    pair("rsvp_ref", "db_insert"): 1, pair("rsvp_audit_ref", "db_insert"): 1})
            optional = {pair(slot, "db_read") for slot in ("rsvp_ref", "rsvp_audit_ref")} if positive else set()
            require(all(key in required or key in optional for key in counts) and
                    all(counts[key] == count for key, count in required.items()), "case_interaction_effects")
            if positive:
                def one(slot, method):
                    return next(effect for effect in effects if signature(effect) == pair(slot, method))
                ack = one("control_channel_ref", "interaction_ack")
                edit = one("control_channel_ref", "interaction_edit")
                writes = [one(slot, "db_insert") for slot in ("rsvp_ref", "rsvp_audit_ref")]
                for effect in effects:
                    if effect is not ack:
                        before(ack, effect)
                    if effect is not edit:
                        before(effect, edit)
                for slot, method in (("actor_ref", "member_get"), ("event_ref", "event_get")):
                    pre, post = [effect for effect in effects if signature(effect) == pair(slot, method)]
                    for write in writes:
                        before(pre, write)
                        before(write, post)
        else:
            require(set(case["resource_refs"]) == feed_resources, "case_resources")
            allowed = {pair("source_ref", "source_get"), pair("feed_channel_ref", "history_get"),
                       pair("feed_ref", "db_read"), pair("delivery_ref", "db_read")}
            positive = case["phase"] == "on"
            required = Counter({pair("feed_channel_ref", "feed_post"): 1,
                                pair("delivery_ref", "db_insert"): 1,
                                pair("delivery_ref", "db_cas"): 1}) if positive else Counter()
            require(all(key in allowed or key in required for key in counts) and
                    all(counts[key] == count for key, count in required.items()), "case_feed_effects")
            if positive:
                require(counts[pair("source_ref", "source_get")] > 0, "case_source_read")
                claim, post, complete = (next(effect for effect in effects if signature(effect) == key)
                                         for key in (pair("delivery_ref", "db_insert"),
                                                     pair("feed_channel_ref", "feed_post"),
                                                     pair("delivery_ref", "db_cas")))
                before(claim, post)
                before(post, complete)
                for effect in effects:
                    if effect["method"] == "source_get":
                        before(effect, claim)

    _unique_refs(used)
    required_ids = {effect["operation_id"] for effect in actual.values()
        if effect["method"] in ("feed_post", "interaction_ack", "interaction_edit") or
        (effect["resource_ref"] in (slots["delivery_ref"], slots["rsvp_ref"], slots["rsvp_audit_ref"])
         and effect["method"] in ("db_insert", "db_cas")) or
        (effect["resource_ref"] in (slots["source_ref"], slots["actor_ref"], slots["event_ref"],
                                    slots["feed_channel_ref"])
         and effect["method"] in ("source_get", "member_get", "event_get", "history_get"))}
    require(required_ids <= set(used), "unassigned_case_effect")
