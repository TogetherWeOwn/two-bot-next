"""Closed, bounded announcement-candidate evidence syntax; no runtime authority.

This module is trusted tooling, not a candidate application import. Shape
validation does not authenticate evidence, qualify a candidate, or permit any
operation. There is intentionally no dispatch/credential/network adapter here.
"""

from dataclasses import dataclass
from datetime import datetime, timezone
import json
import re


SCHEMA = "two.bot.announcement-candidate-staging.v1"
PURPOSE = "premerge-evidence"
RECEIPT_LIMIT = 1024 * 1024
ARCHIVE_LIMIT = 32 * 1024 * 1024
DEPTH_LIMIT = 12
TEXT_LIMIT = 4096
REFERENCE_LIMIT = 256
LEDGER_LIMIT = 512
GATE_NAMES = frozenset({
    "schema_plan", "roles", "rollout", "health", "readiness", "voice", "help", "user_flow",
})
COMMAND_GATES = frozenset({
    "feed-add", "feed-remove", "feed-pause", "feed-resume", "feed-list", "feed-inspect",
    "rsvp", "rsvp-attendance",
})
REASON_CODES = frozenset({
    "identity", "authority", "topology", "isolation", "resource", "budget", "gate",
    "readiness", "sample", "provenance", "attempt", "replay", "telemetry", "effect",
    "cleanup", "rollback", "expiry", "serialization", "unsupported",
})
PHASES = (
    "baseline", "off", "on", "replay", "negative", "final_off", "cleanup", "restoration",
)


class Refusal(ValueError):
    """A constant diagnostic: never include rejected values or parser errors."""

    def __init__(self, code):
        self.code = code
        super().__init__("announcement evidence refused: " + code)


def require(condition, code):
    if not condition:
        raise Refusal(code)


def _object_pairs(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "duplicate_key")
        result[key] = value
    return result


def _reject_constant(_value):
    raise Refusal("non_json_number")


def _integer(text):
    require(len(text) <= 20, "integer_size")
    return int(text)


def _bounds(value, depth=1):
    require(depth <= DEPTH_LIMIT, "depth")
    if isinstance(value, str):
        try:
            size = len(value.encode("utf-8"))
        except UnicodeError:
            raise Refusal("unicode") from None
        require(size <= TEXT_LIMIT, "text_size")
    elif type(value) is dict:
        require(len(value) <= REFERENCE_LIMIT, "object_size")
        for key, child in value.items():
            _bounds(key, depth + 1)
            _bounds(child, depth + 1)
    elif type(value) is list:
        require(len(value) <= LEDGER_LIMIT, "list_size")
        for child in value:
            _bounds(child, depth + 1)
    else:
        require(value is None or type(value) in (bool, int), "scalar_type")


def parse_receipt(raw):
    """Parse UTF-8 bytes without leaking rejected data or accepting extensions.

    Parsing and schema checking are deliberately separate from authenticated
    independent verification. No parsed dictionary is an admission capability.
    """
    require(type(raw) is bytes and 0 < len(raw) <= RECEIPT_LIMIT, "receipt_size")
    # Bound container depth before the recursive decoder allocates nested data.
    depth = 0
    quoted = False
    escaped = False
    for byte in raw:
        if quoted:
            if escaped:
                escaped = False
            elif byte == 92:
                escaped = True
            elif byte == 34:
                quoted = False
        elif byte == 34:
            quoted = True
        elif byte in (91, 123):
            depth += 1
            require(depth <= DEPTH_LIMIT, "depth")
        elif byte in (93, 125):
            depth -= 1
            require(depth >= 0, "json")
    try:
        value = json.loads(raw.decode("utf-8"), object_pairs_hook=_object_pairs,
                           parse_constant=_reject_constant, parse_int=_integer)
    except (UnicodeError, json.JSONDecodeError, RecursionError, OverflowError):
        raise Refusal("json") from None
    _bounds(value)
    validate_receipt_shape(value)
    return value


@dataclass(frozen=True)
class Text:
    pattern: str


@dataclass(frozen=True)
class Enum:
    values: tuple


@dataclass(frozen=True)
class Integer:
    minimum: int = 0
    maximum: int = 2**63 - 1


@dataclass(frozen=True)
class Array:
    item: object
    minimum: int = 1
    maximum: int = REFERENCE_LIMIT


@dataclass(frozen=True)
class Union:
    tag: str
    variants: dict


def _validate(value, spec):
    if type(spec) is dict:
        require(type(value) is dict and value.keys() == spec.keys(), "object_fields")
        for key, child_spec in spec.items():
            _validate(value[key], child_spec)
    elif isinstance(spec, Text):
        require(type(value) is str and re.fullmatch(spec.pattern, value) is not None,
                "text_format")
        require(value.casefold() not in {
            "unbound", "unknown", "todo", "tbd", "null", "none", "all", "hold",
        }, "unresolved_symbol")
        if spec is UTC:
            try:
                datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ")
            except ValueError:
                raise Refusal("timestamp") from None
        if spec is DATE:
            try:
                datetime.strptime(value, "%Y-%m-%d")
            except ValueError:
                raise Refusal("date") from None
        if spec is TOKEN_FREE_PATH:
            require(all(part not in (".", "..") for part in value.split("/")), "path")
    elif isinstance(spec, Enum):
        require(any(type(value) is type(item) and value == item for item in spec.values),
                "enum")
    elif isinstance(spec, Integer):
        require(type(value) is int and spec.minimum <= value <= spec.maximum, "integer")
    elif isinstance(spec, Array):
        require(type(value) is list and spec.minimum <= len(value) <= spec.maximum,
                "array_size")
        for item in value:
            _validate(item, spec.item)
    elif isinstance(spec, Union):
        require(type(value) is dict and type(value.get(spec.tag)) is str, "union_tag")
        variant = spec.variants.get(value[spec.tag])
        require(variant is not None, "union_tag")
        _validate(value, variant)
    else:
        raise RuntimeError("invalid trusted announcement schema")


def enum(*values):
    return Enum(values)


def refs(minimum=1):
    return Array(REF, minimum)


REF = Text(r"[a-z][a-z0-9_-]{0,95}")
SERVICE_ID = Text(r"[1-9][0-9]{0,19}")
UUID = Text(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
SHA = Text(r"[0-9a-f]{40}")
SHA256 = Text(r"[0-9a-f]{64}")
IMAGE = Text(r"sha256:[0-9a-f]{64}")
UTC = Text(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z")
DATE = Text(r"[0-9]{4}-[0-9]{2}-[0-9]{2}")
COUNT = Integer()
TOKEN_FREE_PATH = Text(r"[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)*")
VERSION = {"commit": SHA, "tree": SHA}
DOCUMENT = {"ref": REF, "revision": UUID, "sha256": SHA256}
MANIFEST = {"ref": REF, "revision": SHA, "sha256": SHA256}
PRINCIPAL = {"ref": REF, "subject_id": UUID, "identity_root_ref": REF}
RUN = {"run_id": SERVICE_ID, "attempt": Integer(1), "control_sha": SHA}
STATE = Union("presence", {
    "absent": {"presence": enum("absent"), "ref": REF},
    "present": {"presence": enum("present"), "ref": REF,
                "version": SERVICE_ID, "value_sha256": SHA256},
})

SCOPE = {
    "company_ref": REF, "task_ref": REF,
    "repository": {"id": enum("1396224242"),
                   "full_name": enum("TogetherWeOwn/two-bot-next"),
                   "head_repo_id": enum("1396224242"), "base_repo_id": enum("1396224242")},
    "pr": enum("1639"), "branch": enum("feat/announcement-command-live-dispatch"),
    "base_ref": enum("main"), "candidate": VERSION, "merge_base": VERSION,
    "recorded_base": VERSION, "contract": DOCUMENT, "source_manifest": MANIFEST,
}
CONTROL = {
    "version": VERSION,
    "workflow": {"id": SERVICE_ID, "path": TOKEN_FREE_PATH, "ref": REF, "sha": SHA},
    "installation": {"pr": SERVICE_ID, "head": VERSION, "squash": VERSION,
                     "mapping_ref": REF, "authority_receipt_refs": refs()},
    "tooling_manifest": MANIFEST, "actions_manifest": MANIFEST,
    "dependencies_manifest": MANIFEST, "modules_manifest": MANIFEST,
    "configuration_manifest": MANIFEST,
}
DECISION = {
    "ref": REF, "role": enum("security", "qa"), "decision_id": UUID,
    "stage_id": UUID, "participant_id": UUID, "round": Integer(1),
    "principal": PRINCIPAL, "scope_sha256": SHA256, "epoch": SERVICE_ID,
    "security_decision_id": UUID, "outcome": enum("approved"), "decided_at": UTC,
}
AUTHORITY_READ = {
    "ref": REF, "consumer": enum("controller", "verifier"), "source_ref": REF,
    "scope_sha256": SHA256, "epoch": SERVICE_ID, "resource_epoch": SERVICE_ID,
    "history_sha256": SHA256, "read_at": UTC, "effective_decision_refs": refs(),
}
AUTHORITY = {
    "source_ref": REF, "policy": DOCUMENT, "implementation": MANIFEST,
    "scope_sha256": SHA256, "resource_plan": DOCUMENT, "epoch": SERVICE_ID,
    "installation_approval_refs": refs(), "executor": PRINCIPAL, "author": PRINCIPAL,
    "caller": PRINCIPAL, "decisions": Array(DECISION, 2, 2),
    "reads": Array(AUTHORITY_READ, 2),
    "execution": {"ref": REF, "run": RUN, "nonce_ref": REF, "consumption_ref": REF,
                  "not_before": UTC, "expires_at": UTC, "budgets_sha256": SHA256},
}
CHECKPOINT = {
    "ref": REF, "phase": enum("credentials", "setup", "rollout", "transition",
                              "effect_batch", "final"),
    "controller_read_ref": REF, "verifier_read_ref": REF, "checked_at": UTC,
    "authority_epoch": SERVICE_ID, "resource_epoch": SERVICE_ID,
    "mutation_fence_ref": REF,
}
LOCK = {
    "ref": REF, "name": enum("deploy-staging", "staging-lifecycle"),
    "owner_ref": REF, "run": RUN, "fence": SERVICE_ID,
    "acquired_at": UTC, "released_at": UTC, "interference_refs": refs(0),
}
ATTEMPT = {
    "run": RUN, "event": enum("workflow_dispatch"), "actor": PRINCIPAL,
    "started_at": UTC, "ended_at": UTC,
    "observation_started_at": UTC, "observation_ended_at": UTC,
    "job_refs": refs(), "step_refs": refs(), "check_refs": refs(), "producer_refs": refs(),
    "checkpoints": Array(CHECKPOINT), "locks": Array(LOCK, 2, 2),
    "enforcement_epoch": SERVICE_ID,
}
MODULE = {"path": TOKEN_FREE_PATH, "size": Integer(1, ARCHIVE_LIMIT), "sha256": SHA256}
WORKER = {
    "version_id": UUID, "entrypoint": TOKEN_FREE_PATH,
    "compatibility_date": DATE,
    "compatibility_flags": Array(REF, 0), "modules": Array(MODULE),
}
BUILD = {
    "ref": REF, "id": REF, "run": RUN, "candidate": VERSION,
    "image_digest": IMAGE, "provenance_ref": REF, "isolation_receipt_ref": REF,
    "components_manifest": MANIFEST, "worker": WORKER,
}
ROLLOUT = {
    "ref": REF, "id": UUID, "run": RUN, "application_id": UUID,
    "target_version": SERVICE_ID, "deployment_id": REF, "build_ref": REF,
    "image_digest": IMAGE, "worker_version_id": UUID, "started_at": UTC, "ended_at": UTC,
}
SERVED_PAIR = {
    "ref": REF, "run": RUN, "external_source_ref": REF, "observed_at": UTC,
    "external_revision": SHA, "reported_revision": SHA,
    "image_digest": IMAGE, "worker_version_id": UUID, "modules_sha256": SHA256,
    "application_id": UUID, "target_version": SERVICE_ID,
    "build_ref": REF, "rollout_ref": REF,
}
SERVED = {"samples": Array(SERVED_PAIR)}

# Identity variants are fixed by trusted code, never caller-defined objects.
IDENTITY = Union("kind", {
    "discord": {"kind": enum("discord"), "application_id": SERVICE_ID,
                "guild_id": SERVICE_ID, "object_id": SERVICE_ID,
                "object_type": enum("application", "actor", "channel", "event", "message",
                                    "interaction", "command_set"),
                "permissions_sha256": SHA256, "membership_sha256": SHA256},
    "database": {"kind": enum("database"), "account_ref": REF, "project_ref": REF,
                 "endpoint_identity_sha256": SHA256, "database_ref": REF,
                 "schema_ref": REF, "table": enum("feed_relays", "feed_deliveries",
                    "event_rsvps", "announcements_audit_log", "guild_settings"),
                 "key_sha256": SHA256, "role_ref": REF, "grants_sha256": SHA256,
                 "trigger_inventory_sha256": SHA256},
    "source": {"kind": enum("source"), "origin_identity_sha256": SHA256,
               "path_sha256": SHA256, "content_sha256": SHA256, "item_ref": REF},
    "synthetic": {"kind": enum("synthetic"), "job_id": SERVICE_ID,
                  "actor_ref": REF, "request_sha256": SHA256,
                  "resource_refs": refs(), "cleanup_plan_ref": REF},
})
OPERATIONS = (
    "feed_post", "interaction_ack", "interaction_edit", "gateway_identify",
    "gateway_heartbeat", "gateway_read", "guild_commands_replace", "source_get",
    "member_get", "event_get", "history_get", "db_read", "db_insert", "db_cas",
    "db_delete_owned", "message_delete_owned", "synthetic_request",
)
RESOURCE = {
    "ref": REF, "kind": enum("application", "actor", "channel", "event", "feed",
                            "setting", "rsvp", "audit", "delivery", "source", "synthetic"),
    "identity": IDENTITY, "owner": REF, "allowed_operations": Array(enum(*OPERATIONS)),
    "expected_count": COUNT, "maximum_count": COUNT,
    "not_before": UTC, "expires_at": UTC, "before_state_ref": REF, "after_state_ref": REF,
}
RESOURCES = {"identity_map": DOCUMENT, "plan": DOCUMENT, "entries": Array(RESOURCE),
             "states": Array(STATE), "inventory_manifest": MANIFEST}
SETTING_BOUNDARY = {
    "ref": REF, "phase": enum(*PHASES), "resource_ref": REF,
    "stored_value": enum("0", "1"), "row_version": SERVICE_ID,
    "snapshot_generation": SERVICE_ID, "read_at": UTC,
    "reader_receipt_ref": REF, "poll_receipt_ref": REF, "eligible_item_ref": REF,
}
FIXTURES = {
    "feed_ref": REF, "item_ref": REF, "setting_ref": REF, "rsvp_ref": REF,
    "source_content_sha256": SHA256, "item_content_sha256": SHA256,
    "nonce_ref": REF, "consent_ref": REF, "baseline_ref": REF,
    "boot_announcements": enum(True), "activation_fence_ref": REF,
    "setting_boundaries": Array(SETTING_BOUNDARY), "case_ids": refs(),
    "interaction_plan_ref": REF, "scheduler_plan_ref": REF,
}
GATE = {
    "name": enum(*sorted(GATE_NAMES)), "job_id": SERVICE_ID, "run": RUN,
    "served_ref": REF, "sample_refs": refs(), "assertion_refs": refs(),
    "observed_at": UTC, "result": enum("success", "failure"),
    "status": Integer(100, 599),
}
GATES = {"readiness_clearance": DOCUMENT, "records": Array(GATE, 8, 8)}
OBSERVATION = {
    "case_id": REF, "phase": enum(*PHASES), "input_ref": REF, "resource_refs": refs(),
    "gate_ref": REF, "served_ref": REF, "started_at": UTC, "ended_at": UTC,
    "expected_effect_refs": refs(0), "actual_effect_refs": refs(0),
    "coverage_ref": REF, "outcome": enum("pass", "fail", "unknown"),
}
EFFECT = {
    "operation_id": REF, "sequence": Integer(1), "resource_ref": REF,
    "method": enum(*OPERATIONS), "payload_sha256": SHA256, "nonce_ref": refs(0),
    "attempted_count": Integer(1, 1), "accepted_count": Integer(0, 1),
    "platform_id_refs": Array(SERVICE_ID, 0), "db_transition_refs": refs(0),
    "observed_at": UTC, "served_ref": REF,
    "outcome": enum("accepted", "blocked", "failed", "unknown"),
}
COVERAGE = {
    "ref": REF, "observer_ref": REF, "run": RUN, "started_at": UTC, "ended_at": UTC,
    "first_sequence": Integer(1), "last_sequence": COUNT,
    "enforcement_epoch": SERVICE_ID, "source_inventory_sha256": SHA256,
    "wire_receipt_refs": refs(0), "platform_receipt_refs": refs(0),
    "transaction_receipt_refs": refs(0),
    "result": enum("complete", "gap", "truncated", "sampled", "unknown"),
}
OBSERVATIONS = {
    "cases": Array(OBSERVATION), "expected_effects": Array(EFFECT, 0, LEDGER_LIMIT),
    "actual_effects": Array(EFFECT, 0, LEDGER_LIMIT), "coverage": Array(COVERAGE),
    "command_gates": Array(enum(*sorted(COMMAND_GATES)), 8, 8),
    "reconciliation_ref": REF,
}
TRANSITION = {
    "operation_id": REF, "owner": REF, "resource_ref": REF, "key_sha256": SHA256,
    "before_state_ref": REF, "intended_after_state_ref": REF, "actual_after_state_ref": REF,
    "expected_count": COUNT, "actual_count": COUNT, "observed_at": UTC,
    "authority_ref": REF, "ownership_match_ref": REF,
    "outcome": enum("matched", "conflict", "unknown", "failed"),
}
LIFECYCLE = {
    "served_before": SERVED_PAIR, "served_after": SERVED_PAIR,
    "worker_before": WORKER, "worker_after": WORKER,
    "snapshot_ref": REF, "compatibility_ref": REF, "restorable_artifact_refs": refs(),
    "setup": Array(TRANSITION, 1, LEDGER_LIMIT),
    "cleanup": Array(TRANSITION, 1, LEDGER_LIMIT),
    "restoration_authority_ref": REF, "restoration_verification_ref": REF,
    "abort": enum("not_triggered", "fenced", "unresolved"),
    "drain_ref": REF, "queue_baseline_ref": REF, "queue_final_ref": REF,
    "in_flight": enum("resolved", "unknown"),
    "postcondition_refs": refs(), "readiness_after_ref": REF,
}
RESULT = {"status": enum("pass", "fail", "hold"),
          "reason_codes": Array(enum(*sorted(REASON_CODES)), 0, len(REASON_CODES)),
          "discrepancy_refs": refs(0)}
RECEIPT = {
    "schema": enum(SCHEMA), "purpose": enum(PURPOSE), "promotable": enum(False),
    "scope": SCOPE, "control": CONTROL, "authority": AUTHORITY, "attempt": ATTEMPT,
    "build": BUILD, "rollout": ROLLOUT, "served": SERVED, "resources": RESOURCES,
    "fixtures": FIXTURES, "gates": GATES, "observations": OBSERVATIONS,
    "lifecycle": LIFECYCLE, "result": RESULT,
}


def utc(value):
    """Only canonical second-precision UTC, never a local/offset timestamp."""
    _validate(value, UTC)
    try:
        return datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)
    except ValueError:
        raise Refusal("timestamp") from None


def validate_receipt_shape(value):
    """Check the fixed schema only; this is NOT independent acceptance."""
    _bounds(value)
    _validate(value, RECEIPT)
    require({x["name"] for x in value["gates"]["records"]} == GATE_NAMES, "gate_inventory")
    require(set(value["observations"]["command_gates"]) == COMMAND_GATES, "command_inventory")
    result = value["result"]
    if result["status"] == "pass":
        require(not result["reason_codes"] and not result["discrepancy_refs"], "result")
    else:
        require(bool(result["reason_codes"]) and bool(result["discrepancy_refs"]), "result")
    require(len(result["reason_codes"]) == len(set(result["reason_codes"])), "result")
