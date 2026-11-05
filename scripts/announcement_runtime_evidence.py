"""Bounded typed provenance/authority/resource claims, not authentication.

Neither a sidecar's kind, filename, digest nor successful resolution selects a
trust root or proves authority or independent observation. Remaining resource
and lifecycle kinds have no permissive fallback: unimplemented kinds refuse,
and runtime admission remains unsupported.
"""

import copy
import hashlib
import json

from announcement_runtime_schema import (
    ARCHIVE_LIMIT, Array, AUTHORITY, AUTHORITY_READ, CONTROL, DECISION, DOCUMENT,
    COUNT, COVERAGE, GATE_NAMES, IDENTITY, IMAGE, Integer, LEDGER_LIMIT, LOCK, MANIFEST, OPERATIONS,
    PRINCIPAL, REF, REFERENCE_LIMIT, RESOURCE, RUN, SCOPE, SERVICE_ID, SERVED_PAIR,
    SHA, SHA256, STATE, TOKEN_FREE_PATH, UTC, UUID, COMMAND_GATES,
    Text, TRANSITION, Union, VERSION, WORKER, _validate, enum,
    parse_bounded_json, require,
)


EVIDENCE_SCHEMA = "two.bot.announcement-evidence.v1"
# No generic record, arbitrary extensions or caller-supplied schema definition.
PROVENANCE_RECORDS = {
    "document": {"revision": UUID, "sha256": SHA256},
    "manifest": {"revision": SHA, "sha256": SHA256},
    "producer": {"principal": PRINCIPAL, "workflow": CONTROL["workflow"],
                 "repository": enum("TogetherWeOwn/two-bot-next")},
    "job": {"id": SERVICE_ID, "producer_ref": REF,
            "source": enum("candidate", "control", "independent_verifier", "external_observer"),
            "head": SHA, "conclusion": enum("success", "failure"),
            "started_at": UTC, "ended_at": UTC},
    "step": {"id": SERVICE_ID, "job_ref": REF, "result": enum("success", "failure")},
    "check": {"id": SERVICE_ID, "job_ref": REF, "head": SHA,
              "result": enum("success", "failure")},
    "build_provenance": {"build_ref": REF, "candidate": VERSION,
                         "image_digest": SERVED_PAIR["image_digest"],
                         "components_sha256": SHA256, "worker": WORKER},
    "build_isolation": {"build_ref": REF, "candidate": VERSION, "job_ref": REF,
                        "boundary_id": UUID,
                        "credential_paths": Array(enum("persisted_checkout", "host_environment",
                            "shared_cache", "docker_socket", "control_plane"), 0, 5),
                        "outcome": enum("isolated", "unknown")},
    "external_served": {"observer_ref": REF, "sample": SERVED_PAIR},
    "observer": {"principal": PRINCIPAL, "job_ref": REF, "provisioning_ref": REF},
    "gate_sample": {"gate_name": enum(*sorted(GATE_NAMES)), "job_id": SERVICE_ID,
                    "served_ref": REF, "observed_at": UTC,
                    "status": Integer(100, 599), "result": enum("observed", "unknown"),
                    "request_sha256": SHA256},
    "gate_assertion": {"gate_name": enum(*sorted(GATE_NAMES)),
                       "sample_refs": Array(REF), "outcome": enum("success", "failure"),
                       "assertions_sha256": SHA256},
    "installation_mapping": {"pr": SERVICE_ID, "head": VERSION, "squash": VERSION,
                             "control": VERSION},
    "artifact": {"id": SERVICE_ID, "name": TOKEN_FREE_PATH,
                 "size": Integer(1, ARCHIVE_LIMIT), "sha256": SHA256,
                 "producer_ref": REF, "created_at": UTC, "expires_at": UTC},
}


# These schemas describe claims about authority, NEVER provision an authority
# root. In particular there is no endpoint, key, plugin or adapter input.
AUTHORITY_RECORDS = {
    "principal_binding": {"principal": PRINCIPAL,
        "roles": Array(enum("author", "executor", "caller", "security", "qa",
                            "controller", "verifier"), 1, 7)},
    "authority_consumer": {"principal": PRINCIPAL, "role": enum("controller", "verifier"),
                           "provisioning_ref": REF},
    "authority_source": {"repository": SCOPE["repository"], "policy": DOCUMENT,
                         "principal_mapping_sha256": SHA256,
                         "principal_refs": Array(REF, 7, 7),
                         "controller_ref": REF, "verifier_ref": REF,
                         "installation_mapping_ref": REF, "history_ref": REF,
                         "stage_bindings": Array({"purpose": enum("runtime", "installation"),
                             "role": enum("security", "qa"), "principal_ref": REF,
                             "stage_id": UUID, "participant_id": UUID}, 4, 4)},
    "authority_scope": {"scope": SCOPE, "control": CONTROL, "policy": DOCUMENT,
                        "source_sha256": SHA256, "consumers_sha256": SHA256,
                        "implementation": MANIFEST, "resource_plan": DOCUMENT,
                        "identity_map": DOCUMENT, "inventory_manifest": MANIFEST,
                        "components_manifest": MANIFEST, "image_digest": IMAGE,
                        "budgets_sha256": SHA256, "epoch": SERVICE_ID,
                        "resource_epoch": SERVICE_ID, "principal_refs": Array(REF, 7, 7)},
    "authority_decision": {"claim": DECISION, "source_ref": REF,
                           "security_round": Integer(1), "generation": SERVICE_ID},
    "authority_read": {"claim": AUTHORITY_READ, "history_ref": REF, "consumer_ref": REF},
    "authority_history": {"source_ref": REF, "scope_sha256": SHA256,
        "epoch": SERVICE_ID, "resource_epoch": SERVICE_ID, "generation": SERVICE_ID,
        "effective_decision_refs": Array(REF, 2, 2),
        "started_at": UTC, "ended_at": UTC, "first_sequence": Integer(1),
        "last_sequence": Integer(), "coverage": enum("complete", "gap", "unknown"),
        "events": Array({"sequence": Integer(1),
            "kind": enum("push", "scope_change", "policy_change", "resource_change",
                         "revoke", "dismiss"),
            "observed_at": UTC, "scope_sha256": SHA256,
            "invalidated_decision_refs": Array(REF, 0)}, 0)},
    "installation_approval": {"source_ref": REF, "mapping_ref": REF,
        "policy": DOCUMENT, "implementation": MANIFEST, "control": VERSION,
        "epoch": SERVICE_ID, "scope_sha256": SHA256, "security_round": Integer(1),
        "decisions": Array(DECISION, 2, 2),
        "history_ref": REF, "controller_read_ref": REF, "verifier_read_ref": REF,
        "checked_at": UTC},
    "execution_authority": {"claim": AUTHORITY["execution"], "source_ref": REF,
        "scope_sha256": SHA256, "epoch": SERVICE_ID, "resource_epoch": SERVICE_ID,
        "generation": SERVICE_ID, "installation_approval_refs": Array(REF)},
    "nonce_consumption": {"source_ref": REF, "execution_ref": REF, "nonce_ref": REF,
        "run": RUN, "owner_ref": REF, "consumed_at": UTC,
        "authority_epoch": SERVICE_ID, "resource_epoch": SERVICE_ID,
        "outcome": enum("consumed", "replayed", "unknown")},
    "mutation_fence": {"source_ref": REF, "execution_ref": REF, "checkpoint_ref": REF,
        "run": RUN, "phase": enum("credentials", "setup", "rollout", "transition",
                                     "effect_batch", "final"),
        "authority_epoch": SERVICE_ID, "resource_epoch": SERVICE_ID,
        "nonce_ref": REF, "consumption_ref": REF, "observed_at": UTC, "valid_until": UTC,
        "boundary": enum("external_atomic", "application_flag", "unknown")},
}
# Content hashes bind these offline resource claims to the plan/document pins.
# They are not independently obtained literal-resource or containment proofs.
RESOURCE_SLOTS = {
    key: REF for key in ("application_ref", "actor_ref", "control_channel_ref",
        "feed_channel_ref", "event_ref", "feed_ref", "delivery_ref", "setting_ref",
        "rsvp_ref", "rsvp_audit_ref", "source_ref")
}
OPERATION_BUDGET = {"ref": REF, "resource_ref": REF, "method": enum(*OPERATIONS),
                    "expected_count": COUNT, "maximum_count": COUNT}
RESOURCE_RECORDS = {
    "resource_owner": {"principal": PRINCIPAL, "execution_ref": REF, "nonce_ref": REF},
    "resource_state": {"claim": STATE, "resource_ref": REF, "identity_sha256": SHA256,
                       "owner_ref": REF, "observer_ref": REF, "observed_at": UTC},
    "resource_isolation": {"resource_ref": REF, "identity_sha256": SHA256,
        "owner_ref": REF, "observer_ref": REF, "observed_at": UTC,
        "resource_epoch": SERVICE_ID, "purpose": enum("existing_isolated_test"),
        "boundary": enum("all_paths_external", "application_only", "unknown"),
        "credential_exposure": enum("mediated_only", "raw", "unknown"),
        "production_memberships": Array(SERVICE_ID, 0)},
    "identity_map_content": {"bindings": Array({"resource_ref": REF, "identity": IDENTITY,
        "owner_ref": REF, "isolation_ref": REF})},
    "resource_plan_content": {"slots": RESOURCE_SLOTS, "entries": Array(RESOURCE),
        "states": Array(STATE), "identity_map": DOCUMENT, "inventory_manifest": MANIFEST,
        "operation_budgets": Array(OPERATION_BUDGET, 1, LEDGER_LIMIT),
        "input_plan_sha256": SHA256, "serialization_plan_sha256": SHA256,
        "not_before": UTC, "expires_at": UTC},
    "feed_item": {"source_ref": REF, "feed_ref": REF, "item_ref": REF,
        "source_content_sha256": SHA256, "item_content_sha256": SHA256, "nonce_ref": REF},
}
# Literal row keys are RESTRICTED evidence, never public artifact payloads. These
# closed claims do not authenticate the map or prove consent/resource existence.
ROW_ID = Text(r"[A-Za-z0-9][A-Za-z0-9:._-]{0,127}")
DB_KEY = Union("table", {
    "feed_relays": {"table": enum("feed_relays"), "id": ROW_ID},
    "feed_deliveries": {"table": enum("feed_deliveries"), "feed_id": ROW_ID,
                        "item_key": ROW_ID},
    "guild_settings": {"table": enum("guild_settings"), "guild_id": SERVICE_ID,
                       "key": enum("TWO_ANNOUNCEMENTS")},
    "event_rsvps": {"table": enum("event_rsvps"), "guild_id": SERVICE_ID,
                    "event_id": SERVICE_ID, "user_id": SERVICE_ID},
    "announcements_audit_log": {"table": enum("announcements_audit_log"), "id": ROW_ID},
})
CONTENT_PIN = {"ref": REF, "sha256": SHA256}
INTERACTION = {
    "case_id": REF, "command": enum(*sorted(COMMAND_GATES)),
    "phase": enum("off", "on", "replay", "negative", "final_off"),
    "interaction_id": SERVICE_ID, "application_id": SERVICE_ID,
    "actor_ref": REF, "channel_ref": REF, "event_ref": REF,
    "request_sha256": SHA256, "ack_count": Integer(1, 1),
    "edit_count": Integer(0, 1), "followup_count": enum(0), "ephemeral": enum(True),
    "maximum_wire_attempts": Integer(1, 2),
}
INPUT_RECORDS = {
    "literal_db_key": DB_KEY,
    "test_consent": {"actor_ref": REF, "actor_id": SERVICE_ID,
        "application_id": SERVICE_ID, "guild_id": SERVICE_ID,
        "execution_ref": REF, "nonce_ref": REF, "case_ids": Array(REF),
        "granted_at": UTC, "not_before": UTC, "expires_at": UTC,
        "outcome": enum("granted", "revoked", "unknown")},
    "interaction_plan": {"interactions": Array(INTERACTION), "exchanges": CONTENT_PIN},
    "input_plan_content": {"keys": Array({"resource_ref": REF, "key": CONTENT_PIN}),
        "consent": CONTENT_PIN, "interactions": CONTENT_PIN, "scheduler_plan_sha256": SHA256,
        "source_plan_sha256": SHA256, "setting_plan_sha256": SHA256,
        "feed": {"resource_ref": REF, "guild_id": SERVICE_ID, "channel_id": SERVICE_ID,
                 "source_ref": REF, "item_key": ROW_ID, "nonce_ref": REF},
        "rsvp": {"resource_ref": REF, "status": enum("going", "interested", "declined"),
                 "interaction_id": SERVICE_ID, "audit_ref": REF}},
}
# C's feed cadence is store-first with boot fallback, never an inferred constant.
# Missing/null announcements settings are deliberately not admissible off proof.
CADENCE = Union("source", {
    "stored": {"source": enum("stored"), "stored_seconds": Integer(60, 86400)},
    "boot": {"source": enum("boot"), "row_presence": enum("absent")},
})
SCHEDULER_RECORDS = {
    "scheduler_plan_content": {"candidate": VERSION, "feed_ref": REF, "item_ref": REF,
        "setting_ref": REF, "activation_fence": CONTENT_PIN,
        "boot_poll_seconds": Integer(60, 86400), "cadence": CADENCE,
        "windows": Array({"phase": enum("off", "on", "replay", "final_off"),
            "boundary_ref": REF, "reader": CONTENT_PIN, "poll": CONTENT_PIN,
            "started_at": UTC, "ended_at": UTC}, 4, 4)},
    "feed_activation_fence": {"execution_ref": REF, "nonce_ref": REF,
        "enforcement_epoch": SERVICE_ID, "observed_at": UTC, "valid_until": UTC,
        "boundary": enum("external_atomic", "application_only", "unknown"),
        "boot_announcements": enum(True), "job": enum("feeds"),
        "registration": enum("registered", "absent", "unknown")},
    "stored_setting_read": {"boundary_ref": REF, "setting_ref": REF,
        "row_presence": enum("stored_string", "absent", "null", "unknown"),
        "stored_value": enum("0", "1"), "row_version": SERVICE_ID,
        "snapshot_value": enum("0", "1"), "snapshot_generation": SERVICE_ID,
        "read_at": UTC, "served_ref": REF, "cadence": CADENCE},
    "feed_poll": {"boundary_ref": REF, "feed_ref": REF, "item_ref": REF,
        "setting_ref": REF, "snapshot_generation": SERVICE_ID, "served_ref": REF,
        "observed_at": UTC, "eligible_item": enum(True),
        "effective_poll_seconds": Integer(60, 86400),
        "decision": enum("suppressed", "admitted", "reconciled", "unknown")},
}
# Source bytes, URLs, row values and literal nonce stay RESTRICTED. This narrow
# ASCII RSS profile avoids pretending Python URL/trim/markdown behavior is C's
# general Rust parser. Unsupported source forms refuse; no URL is ever fetched.
SOURCE_TARGET = {"origin": Text(r"https://[a-z0-9.-]+"),
                 "path": Text(r"/[A-Za-z0-9/._~-]*")}
SOURCE_RECORDS = {
    "source_plan_content": {"candidate": VERSION, "source_ref": REF, "feed_ref": REF,
        "item_ref": REF, "target": SOURCE_TARGET, "stored_feed": CONTENT_PIN,
        "body": CONTENT_PIN, "selection": CONTENT_PIN, "post_exchange": CONTENT_PIN,
        "delivery_trace": CONTENT_PIN,
        "maximum_response_bytes": Integer(1, 4096),
        "maximum_wire_attempts": Integer(1, LEDGER_LIMIT),
        "redirects": enum("deny"), "content_encoding": enum("identity")},
    "stored_feed_source": {"feed_ref": REF, "key_ref": REF, "guild_id": SERVICE_ID,
        "channel_id": SERVICE_ID, "source_url": Text(r"https://[A-Za-z0-9/._~-]+"),
        "kind": enum("rss", "youtube", "twitch"), "enabled": enum(True),
        "observed_at": UTC, "observer_ref": REF, "served_ref": REF},
    "source_body": {"source_ref": REF, "target": SOURCE_TARGET,
        "body_utf8": Text(r"[\s\S]+"), "byte_count": Integer(1, 4096), "sha256": SHA256},
    "source_selection": {"source_ref": REF, "body_sha256": SHA256, "item_ref": REF,
        "item": {"key": Text(r"[A-Za-z0-9][A-Za-z0-9:._-]{0,255}"),
                 "title": Text(r"[A-Za-z0-9][A-Za-z0-9 -]{0,127}"),
                 "url": Text(r"https://[a-z0-9./_~-]+"), "published_at": UTC},
        "nonce_ref": REF, "nonce": Text(r"[0-9a-f]{24}"), "post_content_sha256": SHA256},
}
# These are restricted observation CLAIMS. Complete correspondence is necessary,
# but cannot establish independent all-path observation or actual transactions.
EFFECT_BINDING = {"operation_id": REF, "effect_sha256": SHA256,
                  "resource_identity_sha256": SHA256}
TELEMETRY_RECORDS = {
    "telemetry_window": {"claim": COVERAGE, "operation_ids": Array(REF, 0)},
    "wire_attempt": {**EFFECT_BINDING, "observer_ref": REF, "observed_at": UTC,
                     "outcome": enum("accepted", "blocked", "failed", "unknown")},
    "platform_effect": {"operation_id": REF, "resource_identity_sha256": SHA256,
        "method": enum("feed_post", "interaction_ack", "interaction_edit",
                       "guild_commands_replace"),
        "payload_sha256": SHA256, "nonce_refs": Array(REF, 0),
        "platform_ids": Array(SERVICE_ID), "observer_ref": REF, "observed_at": UTC,
        "outcome": enum("accepted", "unknown")},
    "db_transaction": {"transaction_id": UUID, "observer_ref": REF,
        "effects": Array(EFFECT_BINDING, 1, LEDGER_LIMIT),
        "transition_refs": Array(REF, 0), "committed_at": UTC,
        "outcome": enum("committed", "rolled_back", "unknown")},
    "db_transition": {"claim": TRANSITION, "transaction_ref": REF,
                      "method": enum("db_insert", "db_cas", "db_delete_owned")},
}
# A restricted post/response projection, not authenticated transport or readback.
FEED_EXCHANGE_RECORDS = {
    "feed_post_exchange": {"operation_id": REF, "origin": enum("https://discord.com"),
        "http_method": enum("POST"), "path": Text(r"/channels/[1-9][0-9]*/messages"),
        "body_utf8": Text(r"[\s\S]+"), "response": {
            "http_status": Integer(100, 599), "received_at": UTC, "observer_ref": REF,
            "message": {"id": SERVICE_ID, "channel_id": SERVICE_ID,
                "author_id": SERVICE_ID, "content": Text(r"[\s\S]+"),
                "nonce": Text(r"[0-9a-f]{24}"), "flags": Integer(),
                "mentions": Array(SERVICE_ID, 0), "mention_roles": Array(SERVICE_ID, 0),
                "mention_everyone": enum(False), "attachments": enum([])}}},
}
# Token-free request/route and original-message projections stay RESTRICTED.
# They are claims, not raw authenticated Discord envelopes or transport proof.
INTERACTION_EXCHANGES = {
    "interaction_exchanges": {"exchanges": Array({
        "case_id": REF, "observer_ref": REF, "received_at": UTC,
        "request": {"interaction_id": SERVICE_ID, "application_id": SERVICE_ID,
            "guild_id": SERVICE_ID, "channel_id": SERVICE_ID, "actor_id": SERVICE_ID,
            "command": enum(*sorted(COMMAND_GATES)), "options": Array(Union("name", {
                "event-id": {"name": enum("event-id"), "value": SERVICE_ID},
                "status": {"name": enum("status"),
                           "value": enum("going", "interested", "declined")},
            }), 0, 2)},
        "eligibility_reads": Array({"operation_id": REF, "observer_ref": REF,
            "origin": enum("https://discord.com"), "http_method": enum("GET"),
            "path": Text(r"/guilds/[1-9][0-9]*/(?:members|scheduled-events)/[1-9][0-9]*"),
            "received_at": UTC, "http_status": Integer(100, 599),
            "body_utf8": Text(r"[\s\S]+")}, 0, 4),
        "responses": Array({"operation_id": REF, "origin": enum("https://discord.com"),
            "route": enum("interaction_callback", "original_response"),
            "http_method": enum("POST", "PATCH"),
            "interaction_id": SERVICE_ID, "application_id": SERVICE_ID,
            "body_utf8": Text(r"[\s\S]+"), "readback": {
                "message_id": SERVICE_ID, "application_id": SERVICE_ID,
                "channel_id": SERVICE_ID, "flags": Integer(),
                "content": Text(r"[\s\S]*")}}, 1, 2)})},
}
# Lock graph observations are claims, NOT an acquired distributed fence. A plan
# digest in the resource scope binds every participant and its acquisition path.
LOCK_NAME = enum("deploy-staging", "staging-lifecycle")
SERIALIZATION_RECORDS = {
    "serialization_plan": {"inventory_manifest": MANIFEST,
        "nodes": Array({"ref": REF, "job_ref": REF, "parent_refs": Array(REF, 0, 1),
            "role": enum("orchestrator", "writer", "observer", "credential_free_build"),
            "acquires": Array(LOCK_NAME, 0, 2), "witness_ref": REF}),
        "writer_refs": Array(REF)},
    "serialization_participant": {"node_ref": REF, "job_ref": REF,
        "observer_ref": REF, "started_at": UTC, "ended_at": UTC,
        "held_lock_refs": Array(REF, 0, 2), "other_writer_refs": Array(REF, 0),
        "outcome": enum("exclusive", "interference", "unknown")},
    "serialization_lock": {"claim": LOCK,
        "node_ref": REF, "observer_ref": REF, "boundary": enum("all_writers_external",
            "workflow_only", "application_only", "unknown")},
}
# Compare-and-match witnesses bind restricted, exact state snapshots and keys.
# A witness is still a claim, not an external CAS operation or authority root.
TRANSITION_RECORDS = {
    "ownership_match": {"operation_id": REF, "resource_ref": REF,
        "identity_sha256": SHA256, "key_sha256": SHA256, "owner_ref": REF,
        "execution_ref": REF, "nonce_ref": REF, "authority_epoch": SERVICE_ID,
        "resource_epoch": SERVICE_ID, "checkpoint_ref": REF,
        "method": enum("db_insert", "db_cas", "db_delete_owned"),
        "before_state_sha256": SHA256, "after_state_sha256": SHA256,
        "observer_ref": REF, "observed_at": UTC,
        "boundary": enum("external_compare_and_match", "application_only", "unknown"),
        "outcome": enum("matched", "conflict", "unknown")},
}
# Restricted setting VALUE projections, not full SQL rows or CAS tokens. The
# narrow profile supports absent or explicit string-bit baselines only. Other
# prior JSON values need a separately implemented closed projection; no fallback.
SETTING_RECORDS = {
    "setting_value": {"state_ref": REF, "key_ref": REF, "value": Union("presence", {
        "absent": {"presence": enum("absent")},
        "present": {"presence": enum("present"), "stored_value": enum("0", "1")},
    })},
    "setting_plan_content": {"setting_ref": REF, "key": CONTENT_PIN,
        "values": Array(CONTENT_PIN, 6, 6),
        "operations": {phase: REF for phase in ("baseline", "enable", "disable", "restore")}},
}
# Finite work-surface/drain claims. These do NOT establish that an inventory is
# complete, that an external fence exists, or that an observer is authenticated.
WORK_RECORDS = {
    "work_inventory_content": {"candidate": VERSION, "control": VERSION,
        "source_manifest": MANIFEST,
        "surfaces": Array({"ref": REF,
            "kind": enum("startup", "job", "queue", "retry", "trigger", "gateway",
                         "internal_action", "transport", "synthetic"),
            "source": enum("candidate", "control"), "path": TOKEN_FREE_PATH,
            "blob": SHA, "sha256": SHA256, "disposition": enum("selected", "denied")})},
    "work_snapshot": {"inventory_sha256": SHA256, "observer_ref": REF,
        "enforcement_epoch": SERVICE_ID, "observed_at": UTC,
        "surfaces": Array({"surface_ref": REF, "queued": COUNT, "in_flight": COUNT,
                           "claimed": COUNT, "delayed": COUNT}),
        "outcome": enum("observed", "unknown")},
    "work_fence": {"source_ref": REF, "execution_ref": REF, "owner_ref": REF,
        "nonce_ref": REF, "authority_epoch": SERVICE_ID, "resource_epoch": SERVICE_ID,
        "inventory_sha256": SHA256, "closed_at": UTC, "held_until": UTC,
        "boundary": enum("external_all_paths", "application_only", "unknown"),
        "outcome": enum("closed", "lost", "unknown")},
    "work_drain": {"inventory_ref": REF, "coverage_ref": REF, "fence_ref": REF,
        "drained_snapshot_ref": REF, "started_at": UTC, "ended_at": UTC,
        "unknown_operation_refs": Array(REF, 0), "outcome": enum("resolved", "unknown")},
}
# Partial delivery-row projections are RESTRICTED claims, not complete SQL rows,
# claim-token proof, actual read results or permission to recover an unknown send.
DELIVERY_NONCE = Text(r"[0-9a-f]{24}")
DELIVERY_ROW = Union("state", {
    "pending": {"state": enum("pending"), "nonce": DELIVERY_NONCE},
    "delivered": {"state": enum("delivered"), "nonce": DELIVERY_NONCE,
                  "message_id": SERVICE_ID},
})
DELIVERY_EVENT = {"operation_id": REF, "observed_at": UTC, "observer_ref": REF}
# Full column projections use a digest instead of the raw claim token. SQL NULL
# is an explicit typed marker, not absent evidence or a nullable success record.
SQL_NULL = enum({"kind": "sql_null"})
DELIVERY_COLUMNS = {"feed_id": ROW_ID, "item_key": ROW_ID,
                    "nonce": DELIVERY_NONCE, "first_seen_at": UTC}
STORED_DELIVERY_ROW = Union("state", {
    "pending": {**DELIVERY_COLUMNS, "state": enum("pending"), "message_id": SQL_NULL,
        "delivered_at": SQL_NULL, "claim_token_sha256": SHA256, "claimed_at": UTC},
    "delivered": {**DELIVERY_COLUMNS, "state": enum("delivered"), "message_id": SERVICE_ID,
        "delivered_at": UTC, "claim_token_sha256": SQL_NULL, "claimed_at": SQL_NULL},
})
DELIVERY_RECORDS = {
    "feed_delivery_trace": {"key_ref": REF, "nonce_ref": REF, "claim_ref": REF,
        "storage": {"claim_token_sha256": SHA256, "claimed": STORED_DELIVERY_ROW,
                    "completed": STORED_DELIVERY_ROW, "replay": STORED_DELIVERY_ROW},
        "events": Array(Union("kind", {
            "claim": {"kind": enum("claim"), **DELIVERY_EVENT, "row": DELIVERY_ROW},
            "wire_attempt": {"kind": enum("wire_attempt"), **DELIVERY_EVENT},
            "accepted": {"kind": enum("accepted"), **DELIVERY_EVENT, "message_id": SERVICE_ID},
            "committed": {"kind": enum("committed"), **DELIVERY_EVENT, "row": DELIVERY_ROW},
            "replay": {"kind": enum("replay"), **DELIVERY_EVENT, "row": DELIVERY_ROW},
        }), 1, 16)},
}
EVIDENCE_RECORDS = {**PROVENANCE_RECORDS, **AUTHORITY_RECORDS, **RESOURCE_RECORDS,
                    **INPUT_RECORDS, **SCHEDULER_RECORDS, **SOURCE_RECORDS,
                    **TELEMETRY_RECORDS, **SERIALIZATION_RECORDS, **TRANSITION_RECORDS,
                    **SETTING_RECORDS, **WORK_RECORDS, **INTERACTION_EXCHANGES,
                    **FEED_EXCHANGE_RECORDS, **DELIVERY_RECORDS}


def canonical_digest(value):
    """SHA-256 of canonical inert JSON, not a signature or authority decision."""
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":"),
                                     ensure_ascii=True).encode("ascii")).hexdigest()


class EvidenceIndex:
    """An isolated snapshot of closed same-attempt sidecars, with typed lookup.

    Construct only from inert member bytes after bounded archive inspection.
    This class does not fetch URLs, import modules, open files or authenticate
    any record. Unsupported authority/control evidence must not be relabeled as
    one of these provenance types to obtain acceptance.
    """

    def __init__(self, files, run):
        _validate(run, RUN)
        require(type(files) is dict and 0 < len(files) <= REFERENCE_LIMIT,
                "evidence_inventory")
        require(all(type(data) is bytes for data in files.values()), "evidence_bytes")
        require(sum(map(len, files.values())) <= ARCHIVE_LIMIT, "evidence_size")
        self._run = copy.deepcopy(run)
        self._records = {}
        self._used = set()
        for path, raw in files.items():
            record = parse_bounded_json(raw)
            require(type(record) is dict, "evidence_record")
            kind = record.get("kind")
            require(type(kind) is str and kind in EVIDENCE_RECORDS, "evidence_kind")
            _validate(record, {"schema": enum(EVIDENCE_SCHEMA), "ref": REF,
                               "kind": enum(kind), "run": RUN,
                               "data": EVIDENCE_RECORDS[kind]})
            ref = record["ref"]
            require(path == "evidence/" + ref + ".json", "evidence_path")
            require(record["run"] == self._run, "evidence_attempt")
            require(ref not in self._records, "evidence_duplicate")
            self._records[ref] = record

    def resolve(self, ref, kind):
        _validate(ref, REF)
        require(kind in EVIDENCE_RECORDS, "evidence_kind")
        record = self._records.get(ref)
        require(record is not None, "unresolved_reference")
        require(record["kind"] == kind, "reference_kind")
        self._used.add(ref)
        return copy.deepcopy(record["data"])

    def bind(self, claim, kind):
        """Compare a complete document/manifest pin, without authenticating it."""
        require(kind in ("document", "manifest"), "evidence_kind")
        _validate(claim, DOCUMENT if kind == "document" else MANIFEST)
        data = self.resolve(claim["ref"], kind)
        require(data == {key: value for key, value in claim.items() if key != "ref"},
                "reference_binding")

    def require_run(self, run):
        require(run == self._run, "evidence_attempt")

    def require_all_used(self):
        """Final packet accounting must reject even well-formed extra evidence."""
        require(self._used == self._records.keys(), "unreferenced_evidence")
