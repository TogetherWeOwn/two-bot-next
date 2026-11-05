"""SYNTHETIC per-case operation claims, not stored-data or lifecycle proof.

Only selected case operations move across the timeline. Setup/cleanup remain at
08:00 and transition authority/state refs remain placeholders. No real request,
response body, source parser, persistence, fence or recovery is exercised.
"""

import copy

from announcement_provenance_fixtures import at
from announcement_scheduler_fixtures import scheduler_fixture
from announcement_telemetry_fixtures import telemetry_fixture, repin_telemetry


def resequence(receipt, records):
    """Preserve caller list order within each second, then refresh derived hashes."""
    actual = receipt["observations"]["actual_effects"]
    actual.sort(key=lambda effect: effect["observed_at"])
    for sequence, effect in enumerate(actual, 1):
        effect["sequence"] = sequence
    receipt["observations"]["expected_effects"] = copy.deepcopy(actual)
    repin_telemetry(receipt, records)


def case_fixture():
    receipt, records, now = telemetry_fixture(scheduler_fixture())
    cases = {case["case_id"]: case for case in receipt["observations"]["cases"]}
    effects = receipt["observations"]["actual_effects"]
    by_method = {}
    for effect in effects:
        by_method.setdefault((effect["resource_ref"], effect["method"]), []).append(effect)
    for case in cases.values():
        case["expected_effect_refs"], case["actual_effect_refs"] = [], []
        if case["case_id"].startswith("feed-"):
            case["resource_refs"].append("source")
        elif case["phase"] == "on":
            case["resource_refs"].extend(("rsvp", "rsvp-audit"))

    def assign(case_id, effect, clock):
        effect["observed_at"] = at(clock)
        case = cases[case_id]
        for field in ("expected_effect_refs", "actual_effect_refs"):
            case[field].append(effect["operation_id"])
        if effect["method"].startswith("db_"):
            for ref in effect["db_transition_refs"]:
                records[ref]["data"]["claim"]["observed_at"] = at(clock)
        else:
            records["wire-" + effect["operation_id"]]["data"]["observed_at"] = at(clock)
            platform = records.get("platform-" + effect["operation_id"])
            if platform is not None:
                platform["data"]["observed_at"] = at(clock)

    interactions = records["interaction-plan-ref"]["data"]["interactions"]
    acks = by_method[("control-channel", "interaction_ack")]
    for interaction, ack in zip(interactions, acks):
        assign(interaction["case_id"], ack, "00:07:30" if interaction["phase"] == "on" else "00:06:00")
    assign("on-rsvp", by_method[("control-channel", "interaction_edit")][0], "00:08:30")
    for resource, method in (("actor", "member_get"), ("event", "event_get")):
        for effect, clock in zip(by_method[(resource, method)], ("00:07:40", "00:08:20")):
            assign("on-rsvp", effect, clock)
    for resource in ("rsvp", "rsvp-audit"):
        assign("on-rsvp", by_method[(resource, "db_insert")][0], "00:08:00")
    for phase, effect, clock in zip(("off", "on", "replay"), by_method[("source", "source_get")],
                                   ("00:06:00", "00:07:40", "00:09:30")):
        assign("feed-" + phase, effect, clock)
    assign("feed-replay", by_method[("feed-channel", "history_get")][0], "00:10:00")
    for resource, method, clock in (("delivery", "db_insert", "00:07:50"),
                                    ("feed-channel", "feed_post", "00:08:00"),
                                    ("delivery", "db_cas", "00:08:10")):
        assign("feed-on", by_method[(resource, method)][0], clock)
    for record in records.values():
        if record["kind"] == "db_transaction":
            members = {x["operation_id"] for x in record["data"]["effects"]}
            record["data"]["committed_at"] = next(effect["observed_at"] for effect in effects
                                                   if effect["operation_id"] in members)
    resequence(receipt, records)
    return receipt, records, now
