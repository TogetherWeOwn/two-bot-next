"""SYNTHETIC minimal lifecycle ledger, not live evidence or restoration proof.

One database resource, one setup write, one interior (in-window) write and
read, and one cleanup write. Only the sidecars the narrow lifecycle checker
resolves exist: two endpoint states, the cleanup final state and one
db_transition claim per write (the interior write needs one, but is NOT in a
ledger). Intermediate setup/interior states, ownership witnesses,
drain, queue, recovery and postcondition references deliberately have NO record:
the checker must not consume them. Nothing here is authenticated.

Timeline (all 2026-01-01 UTC): attempt 00:00-00:30, before-state 00:00:30,
snapshot 00:01, setup 00:02, rollout 00:05-00:10, observation 00:11-00:20,
interior read 00:12 and write 00:15, cleanup 00:20:30, restored 00:21, after
state 00:22, now 00:40.
"""

import copy
from datetime import datetime, timezone

import announcement_runtime_fixtures as base
from announcement_provenance_fixtures import at, envelope, run
from announcement_runtime_evidence import canonical_digest

NOW = datetime(2026, 1, 1, 0, 40, 0, tzinfo=timezone.utc)
RESOURCE = "db-resource"
OWNER = "owner-ref"
KEY = "e" * 64
VALUE = "a" * 64
OTHER_VALUE = "b" * 64
SETUP, READ, INTERIOR, CLEANUP = "op-setup", "op-read", "op-interior", "op-cleanup"


def identity():
    return {"kind": "database", "account_ref": "account-ref", "project_ref": "project-ref",
            "endpoint_identity_sha256": "1" * 64, "database_ref": "database-ref",
            "schema_ref": "schema-ref", "table": "guild_settings", "key_sha256": KEY,
            "role_ref": "role-ref", "grants_sha256": "2" * 64,
            "trigger_inventory_sha256": "3" * 64}


def present(ref, version, value=VALUE):
    return {"presence": "present", "ref": ref, "version": version, "value_sha256": value}


def absent(ref):
    return {"presence": "absent", "ref": ref}


def state_record(claim, observed):
    return envelope(claim["ref"], "resource_state", {
        "claim": copy.deepcopy(claim), "resource_ref": RESOURCE,
        "identity_sha256": canonical_digest(identity()), "owner_ref": OWNER,
        "observer_ref": "observer-ref", "observed_at": at(observed)})


def effect(operation, sequence, method, clock, transition=True):
    result = base.effect()
    result.update(operation_id=operation, sequence=sequence, resource_ref=RESOURCE,
                  method=method, platform_id_refs=[], observed_at=at(clock),
                  db_transition_refs=["transition-" + operation] if transition else [])
    return result


def claim(operation, clock, before, after):
    return {"operation_id": operation, "owner": OWNER, "resource_ref": RESOURCE,
            "key_sha256": KEY, "before_state_ref": before,
            "intended_after_state_ref": after, "actual_after_state_ref": after,
            "expected_count": 1, "actual_count": 1, "observed_at": at(clock),
            "authority_ref": "authority-ref", "ownership_match_ref": "match-" + operation,
            "outcome": "matched"}


def lifecycle_fixture(prior="absent"):
    """Return (receipt, {ref: sidecar}, now); prior is absent or present."""
    assert prior in ("absent", "present")
    receipt, records = base.receipt(), {}
    attempt = receipt["attempt"]
    attempt.update(run=run(), started_at=at("00:00:00"), ended_at=at("00:30:00"),
                   observation_started_at=at("00:11:00"), observation_ended_at=at("00:20:00"))
    receipt["rollout"].update(started_at=at("00:05:00"), ended_at=at("00:10:00"))
    life = receipt["lifecycle"]
    life["served_before"]["observed_at"] = at("00:01:00")
    life["served_after"]["observed_at"] = at("00:21:00")

    if prior == "absent":
        before, after = absent("state-before"), absent("state-after")
        setup_method, cleanup_method = "db_insert", "db_delete_owned"
        setup_after, cleanup_final = present("state-setup", "1", OTHER_VALUE), absent("state-final")
    else:
        # Restoring a present value mints a NEW version with the SAME value digest.
        before, after = present("state-before", "1"), present("state-after", "3")
        setup_method, cleanup_method = "db_cas", "db_cas"
        setup_after, cleanup_final = present("state-setup", "2", OTHER_VALUE), present(
            "state-final", "3")
    resource = {
        "ref": RESOURCE, "kind": "setting", "identity": identity(), "owner": OWNER,
        "allowed_operations": ["db_read", "db_insert", "db_cas", "db_delete_owned"],
        "expected_count": 4, "maximum_count": 4, "not_before": at("00:00:00"),
        "expires_at": at("00:30:00"), "before_state_ref": "state-before",
        "after_state_ref": "state-after"}
    receipt["resources"]["entries"] = [resource]
    receipt["resources"]["states"] = [copy.deepcopy(before), copy.deepcopy(after)]
    records["state-before"] = state_record(before, "00:00:30")
    records["state-after"] = state_record(after, "00:22:00")
    records["state-final"] = state_record(cleanup_final, "00:20:30")

    setup = claim(SETUP, "00:02:00", "state-before", setup_after["ref"])
    cleanup = claim(CLEANUP, "00:20:30", "state-setup", "state-final")
    life["setup"], life["cleanup"] = [setup], [cleanup]
    records["transition-" + SETUP] = envelope("transition-" + SETUP, "db_transition", {
        "claim": copy.deepcopy(setup), "transaction_ref": "transaction-setup",
        "method": setup_method})
    interior = claim(INTERIOR, "00:15:00", "state-setup", "state-interior")
    records["transition-" + INTERIOR] = envelope("transition-" + INTERIOR, "db_transition", {
        "claim": interior, "transaction_ref": "transaction-interior", "method": "db_cas"})
    records["transition-" + CLEANUP] = envelope("transition-" + CLEANUP, "db_transition", {
        "claim": copy.deepcopy(cleanup), "transaction_ref": "transaction-cleanup",
        "method": cleanup_method})

    effects = receipt["observations"]["actual_effects"] = [
        effect(SETUP, 1, setup_method, "00:02:00"),
        effect(READ, 2, "db_read", "00:12:00", transition=False),
        effect(INTERIOR, 3, "db_cas", "00:15:00"),
        effect(CLEANUP, 4, cleanup_method, "00:20:30"),
    ]
    assert len(effects) == 4
    receipt["observations"]["expected_effects"] = copy.deepcopy(effects)
    return receipt, records, copy.copy(NOW)
