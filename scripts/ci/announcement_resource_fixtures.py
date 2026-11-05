"""SYNTHETIC resource consistency claims, not literal live resource evidence.

This hand-written fixture deliberately does NOT qualify stored-case, telemetry,
transitive inventory or lifecycle behavior. Example numeric IDs and key hashes
are fabricated, not allocated resources, raw keys or authentication evidence.
"""

import copy

from announcement_authority_fixtures import authority_fixture, principal
from announcement_provenance_fixtures import at, envelope
from announcement_runtime_evidence import canonical_digest


def repin_resources(receipt, records):
    """Rehash copied claims to exercise semantic refusals beyond hash equality."""
    resources = receipt["resources"]
    identity_map = records["runtime-identity-map"]["data"]
    resources["identity_map"]["sha256"] = canonical_digest(identity_map)
    plan = records["runtime-resource-plan"]["data"]
    for key in ("entries", "states", "identity_map", "inventory_manifest"):
        plan[key] = copy.deepcopy(resources[key])
    resources["plan"]["sha256"] = canonical_digest(plan)
    receipt["authority"]["resource_plan"] = copy.deepcopy(resources["plan"])
    for pin in (resources["identity_map"], resources["plan"]):
        records[pin["ref"]]["data"] = {k: v for k, v in pin.items() if k != "ref"}


def repin_authority(receipt, records):
    # This rewrites fabricated scope claims, never signs/authenticates anything.
    authority = receipt["authority"]
    fixtures = receipt["fixtures"]
    budget = {"entries": receipt["resources"]["entries"], "fixtures": {k: fixtures[k] for k in (
        "feed_ref", "item_ref", "setting_ref", "rsvp_ref", "nonce_ref",
        "source_content_sha256", "item_content_sha256", "interaction_plan_ref", "scheduler_plan_ref")}}
    authority["execution"]["budgets_sha256"] = canonical_digest(budget)
    scope = records["runtime-authority-scope"]["data"]
    scope.update(resource_plan=copy.deepcopy(receipt["resources"]["plan"]),
                 identity_map=copy.deepcopy(receipt["resources"]["identity_map"]),
                 budgets_sha256=canonical_digest(budget))
    digest = canonical_digest(scope)
    authority["scope_sha256"] = digest
    for decision in authority["decisions"]:
        decision["scope_sha256"] = digest
        records[decision["ref"]]["data"]["claim"] = copy.deepcopy(decision)
    records["execution-ref"]["data"].update(claim=copy.deepcopy(authority["execution"]),
                                           scope_sha256=digest)
    for record in records.values():
        if record["kind"] == "authority_history" and record["ref"].startswith("runtime-"):
            record["data"]["scope_sha256"] = digest
    for record in records.values():
        if record["kind"] == "authority_read" and record["ref"].startswith("runtime-"):
            data = record["data"]
            data["claim"]["scope_sha256"] = digest
            data["claim"]["history_sha256"] = canonical_digest(records[data["history_ref"]]["data"])
    authority["reads"] = [copy.deepcopy(records[x["ref"]]["data"]["claim"])
                           for x in authority["reads"]]


def resource_fixture(*, synthetic=False):
    receipt, records, now = authority_fixture()

    def add(ref, kind, data):
        records[ref] = envelope(ref, kind, copy.deepcopy(data))

    add("resource-owner", "resource_owner", {"principal": principal("controller"),
        "execution_ref": "execution-ref", "nonce_ref": "one-shot-nonce"})
    add("resource-observer", "observer", {"principal": principal("verifier"),
        "job_ref": "job-resource-observer", "provisioning_ref": "verifier-provisioning"})
    receipt["attempt"]["job_refs"].append("job-resource-observer")
    receipt["attempt"]["producer_refs"].append("producer-resource-observer")
    add("job-resource-observer", "job", {"id": "89", "producer_ref": "producer-resource-observer",
        "source": "independent_verifier", "head": receipt["control"]["version"]["commit"],
        "conclusion": "success", "started_at": at("00:00:00"), "ended_at": at("00:24:00")})
    add("producer-resource-observer", "producer", {"principal": principal("verifier"),
        "workflow": receipt["control"]["workflow"], "repository": "TogetherWeOwn/two-bot-next"})
    entries, states, bindings, budgets = [], [], [], []

    def resource(ref, kind, identity, methods, *, absent=False, restored=True):
        before = {"ref": "before-" + ref, "presence": "absent"} if absent else {
            "ref": "before-" + ref, "presence": "present", "version": "1", "value_sha256": "a" * 64}
        after = {**copy.deepcopy(before), "ref": "after-" + ref}
        if not restored:
            after.update(presence="present", version="2", value_sha256="b" * 64)
        states.extend([before, after])
        for state, clock in ((before, "00:01:00"), (after, "00:23:00")):
            add(state["ref"], "resource_state", {"claim": state, "resource_ref": ref,
                "identity_sha256": canonical_digest(identity), "owner_ref": "resource-owner",
                "observer_ref": "resource-observer", "observed_at": at(clock)})
        bindings.append({"resource_ref": ref, "identity": identity, "owner_ref": "resource-owner",
                         "isolation_ref": "isolation-" + ref})
        add("isolation-" + ref, "resource_isolation", {"resource_ref": ref,
            "identity_sha256": canonical_digest(identity), "owner_ref": "resource-owner",
            "observer_ref": "resource-observer", "observed_at": at("00:00:30"),
            "resource_epoch": "9", "purpose": "existing_isolated_test",
            "boundary": "all_paths_external", "credential_exposure": "mediated_only",
            "production_memberships": []})
        entries.append({"ref": ref, "kind": kind, "identity": identity, "owner": "resource-owner",
            "allowed_operations": list(methods), "expected_count": sum(methods.values()),
            "maximum_count": sum(methods.values()), "not_before": at("00:00:00"),
            "expires_at": at("00:40:00"), "before_state_ref": before["ref"],
            "after_state_ref": after["ref"]})
        for method, count in methods.items():
            budgets.append({"ref": "budget-" + ref + "-" + method, "resource_ref": ref,
                            "method": method, "expected_count": count, "maximum_count": count})

    def discord(ref, kind, identifier, methods):
        resource(ref, kind, {"kind": "discord", "application_id": "100", "guild_id": "200",
            "object_id": identifier, "object_type": kind,
            "permissions_sha256": "c" * 64, "membership_sha256": "d" * 64}, methods)

    discord("app", "application", "100", {"gateway_identify": 1})
    discord("actor", "actor", "300", {"member_get": 2})
    discord("control-channel", "channel", "400", {"interaction_ack": 2, "interaction_edit": 1})
    discord("feed-channel", "channel", "500", {"feed_post": 1, "history_get": 1})
    discord("event", "event", "600", {"event_get": 2})
    for ref, kind, table, digit, methods in (
            ("feed", "feed", "feed_relays", "1", {"db_insert": 1, "db_delete_owned": 1}),
            ("delivery", "delivery", "feed_deliveries", "2",
             {"db_insert": 1, "db_cas": 1, "db_delete_owned": 1}),
            ("setting", "setting", "guild_settings", "3",
             {"db_insert": 1, "db_cas": 2, "db_delete_owned": 1}),
            ("rsvp", "rsvp", "event_rsvps", "4", {"db_insert": 1, "db_delete_owned": 1}),
            ("rsvp-audit", "audit", "announcements_audit_log", "5", {"db_insert": 1})):
        resource(ref, kind, {"kind": "database", "account_ref": "test-account",
            "project_ref": "test-project", "endpoint_identity_sha256": "f" * 64,
            "database_ref": "test-database", "schema_ref": "test-schema", "table": table,
            "key_sha256": digit * 64, "role_ref": "test-role", "grants_sha256": "e" * 64,
            "trigger_inventory_sha256": "d" * 64}, methods, absent=True,
            restored=kind != "audit")
    resource("source", "source", {"kind": "source", "origin_identity_sha256": "d" * 64,
        "path_sha256": "e" * 64, "content_sha256": "a" * 64, "item_ref": "item-ref"},
        {"source_get": 3})
    if synthetic:
        discord("synthetic-actor", "actor", "700", {"member_get": 1})
        discord("synthetic-channel", "channel", "800", {"interaction_ack": 1})
        resource("synthetic", "synthetic", {"kind": "synthetic", "job_id": "89",
            "actor_ref": "synthetic-actor", "request_sha256": "c" * 64,
            "resource_refs": ["synthetic-actor", "synthetic-channel"],
            "cleanup_plan_ref": "synthetic-cleanup"}, {"synthetic_request": 1})
    receipt["resources"].update(entries=entries, states=states)
    receipt["fixtures"].update(feed_ref="feed", setting_ref="setting", rsvp_ref="rsvp",
                               source_content_sha256="a" * 64, item_content_sha256="b" * 64)
    add("item-ref", "feed_item", {"source_ref": "source", "feed_ref": "feed", "item_ref": "item-ref",
        "source_content_sha256": "a" * 64, "item_content_sha256": "b" * 64,
        "nonce_ref": receipt["fixtures"]["nonce_ref"]})
    add("runtime-identity-map", "identity_map_content", {"bindings": bindings})
    add("runtime-resource-plan", "resource_plan_content", {
        "slots": {"application_ref": "app", "actor_ref": "actor",
            "control_channel_ref": "control-channel", "feed_channel_ref": "feed-channel",
            "event_ref": "event", "feed_ref": "feed", "delivery_ref": "delivery",
            "setting_ref": "setting", "rsvp_ref": "rsvp", "rsvp_audit_ref": "rsvp-audit",
            "source_ref": "source"},
        "entries": entries, "states": states, "identity_map": receipt["resources"]["identity_map"],
        "inventory_manifest": receipt["resources"]["inventory_manifest"], "operation_budgets": budgets,
        "input_plan_sha256": "a" * 64, "serialization_plan_sha256": "b" * 64,
        "recovery_plan_sha256": "c" * 64,
        "not_before": at("00:00:00"), "expires_at": at("00:40:00")})
    repin_resources(receipt, records)
    repin_authority(receipt, records)
    # Accounting examples only: putting setup/cleanup counts at one instant is
    # intentionally NOT actual full-lifecycle/stored-case qualification evidence.
    effects = []
    for budget in budgets:
        for i in range(budget["expected_count"]):
            effects.append({"operation_id": budget["ref"] + "-" + str(i),
                "sequence": len(effects) + 1, "resource_ref": budget["resource_ref"],
                "method": budget["method"], "payload_sha256": "a" * 64,
                "nonce_ref": [], "attempted_count": 1, "accepted_count": 1,
                "platform_id_refs": [], "db_transition_refs": [],
                "observed_at": at("00:08:00"), "served_ref": "served-one", "outcome": "accepted"})
    receipt["observations"]["expected_effects"] = copy.deepcopy(effects)
    receipt["observations"]["actual_effects"] = copy.deepcopy(effects)
    return receipt, records, now
