"""Offline native PRE admission over authenticated phase receipts.

The caller authenticates the receipts and production setting readback before
submission. Obtain the target and predecessor pins from the reservation and
the start UUID from its admission record, independently of the packet. A
cleanup observation must come from direct provider or runtime evidence of the
exact actor. Empty inventory cannot supply that evidence.

Times are Unix seconds. Observations must fall within the past hour, no earlier
than reservation start and no later than the supplied authority clock.
"""

from hashlib import sha256
import json
from typing import TypedDict
from uuid import UUID

from pre_admission import SettlementSource
from pre_target import ADMISSION_SECONDS, DIGEST, LAST_UNIX_SECOND, SHA, Target
from script_intake import decode_json


PHASES = ("first_create", "rehearsal", "takeover", "readiness", "cleanup", "egress")
MAX_PACKET_BYTES = 16 * 1024
PROMOTION_SETTING = "PRODUCTION_AUTO_PROMOTE_DISABLED"
TARGET_FIELDS = {"scope", "run_id", "started_at", "shape"}
RECEIPT_FIELDS = {"target", "predecessor_digest", "predecessor_commit", "verdict", "observed_at"}


class AdmissionVerdict(TypedDict):
    admitted: bool
    reasons: list[str]
    packet_digest: str | None


def _require_fields(value, fields, reason):
    if type(value) is not dict or value.keys() != fields:
        raise ValueError(reason)


def _target(value):
    _require_fields(value, TARGET_FIELDS, "invalid_target")
    return Target(**value)


def _timestamp(value):
    return type(value) is int and 0 < value <= LAST_UNIX_SECOND


def _predecessor(digest, commit):
    return (type(digest) is str and DIGEST.fullmatch(digest)
            and type(commit) is str and SHA.fullmatch(commit))


def _validate_cleanup(observation):
    _require_fields(observation, {"start_id", "source", "terminal", "observed_at"},
                    "invalid_cleanup_observation")
    start = observation["start_id"]
    try:
        actor = UUID(start) if type(start) is str else None
    except ValueError:
        actor = None
    if (actor is None or not actor.int or str(actor) != start
            or observation["source"] not in (SettlementSource.PROVIDER.value, SettlementSource.RUNTIME.value)
            or type(observation["terminal"]) is not bool or not _timestamp(observation["observed_at"])):
        raise ValueError("invalid_cleanup_observation")


def _validate_packet(packet):
    _require_fields(packet, {"target", "receipts", "production_auto_promote_disabled"}, "invalid_packet_fields")
    _target(packet["target"])
    receipts = packet["receipts"]
    _require_fields(receipts, set(PHASES), "invalid_phases")
    for phase in PHASES:
        receipt = receipts[phase]
        fields = RECEIPT_FIELDS | {"terminal_observation"} if phase == "cleanup" else RECEIPT_FIELDS
        reason = f"invalid_{phase}_receipt"
        _require_fields(receipt, fields, reason)
        _target(receipt["target"])
        if (not _predecessor(receipt["predecessor_digest"], receipt["predecessor_commit"])
                or receipt["verdict"] not in ("pass", "fail") or not _timestamp(receipt["observed_at"])):
            raise ValueError(reason)
        if phase == "cleanup":
            _validate_cleanup(receipt["terminal_observation"])
    setting = packet["production_auto_promote_disabled"]
    _require_fields(setting, {"name", "value", "observed_at"}, "invalid_promotion_readback")
    if (setting["name"] != PROMOTION_SETTING or setting["value"] not in ("true", "false")
            or not _timestamp(setting["observed_at"])):
        raise ValueError("invalid_promotion_readback")


def _admission_reasons(packet, *, target, predecessor_digest, predecessor_commit, start_id, now):
    if (type(target) is not Target or not _predecessor(predecessor_digest, predecessor_commit)
            or type(start_id) is not UUID or not start_id.int):
        return ["invalid_admission_pins"]
    if not _timestamp(now) or now < target.started_at:
        return ["invalid_admission_clock"]

    earliest = max(target.started_at, now - ADMISSION_SECONDS)
    reasons = []
    if _target(packet["target"]) != target:
        reasons.append("target_mismatch")
    for phase in PHASES:
        receipt = packet["receipts"][phase]
        if _target(receipt["target"]) != target:
            reasons.append(f"{phase}_target_mismatch")
        if (receipt["predecessor_digest"], receipt["predecessor_commit"]) != (predecessor_digest, predecessor_commit):
            reasons.append(f"{phase}_predecessor_mismatch")
        if receipt["verdict"] != "pass":
            reasons.append(f"{phase}_not_passed")
        if not earliest <= receipt["observed_at"] <= now:
            reasons.append(f"{phase}_outside_freshness_window")

    cleanup = packet["receipts"]["cleanup"]
    observation = cleanup["terminal_observation"]
    if observation["start_id"] != str(start_id) or not observation["terminal"]:
        reasons.append("cleanup_actor_not_confirmed_terminal")
    if not earliest <= observation["observed_at"] <= cleanup["observed_at"]:
        reasons.append("cleanup_observation_outside_freshness_window")

    setting = packet["production_auto_promote_disabled"]
    if setting["value"] != "true":
        reasons.append("production_auto_promotion_not_disabled")
    if not earliest <= setting["observed_at"] <= now:
        reasons.append("production_readback_outside_freshness_window")
    return reasons


def evaluate_packet(raw: bytes, *, target: Target, predecessor_digest: str, predecessor_commit: str,
                    start_id: UUID, now: int) -> AdmissionVerdict:
    """Grade a UTF-8 JSON packet at one authority time without effects.

    Hash schema-valid packets as sorted, compact JSON, including on a denied
    verdict. Malformed packets have no canonical digest. Reasons use stable
    codes and never echo packet data.
    """
    if type(raw) is not bytes or len(raw) > MAX_PACKET_BYTES:
        return {"admitted": False, "reasons": ["invalid_packet_size_or_type"], "packet_digest": None}
    try:
        packet = decode_json(raw.decode("utf-8"), max_depth=8)
    except (ValueError, RecursionError):
        return {"admitted": False, "reasons": ["invalid_packet_json"], "packet_digest": None}
    try:
        _validate_packet(packet)
    except ValueError as error:
        return {"admitted": False, "reasons": [str(error)], "packet_digest": None}

    canonical = json.dumps(packet, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode("ascii")
    reasons = _admission_reasons(packet, target=target, predecessor_digest=predecessor_digest,
                                 predecessor_commit=predecessor_commit, start_id=start_id, now=now)
    return {"admitted": not reasons, "reasons": reasons, "packet_digest": "sha256:" + sha256(canonical).hexdigest()}
