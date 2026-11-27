"""Correlate saved, independently attributed production watch readbacks offline.

The release pin comes from the admitted release record. Each receipt projects
the identity and capture window from its own authenticated evidence. The caller
verifies provenance before supplying these projections. A verified bundle is
evidence for review and carries no authority to deploy, roll back or write.
"""

from dataclasses import dataclass
import re
from typing import Literal

from build_identity import SHA
from t0_acceptance_report import UUID_RE, utc_instant

IMAGE_DIGEST = re.compile(r"sha256:[0-9a-f]{64}")
MAX_OWNER_EPOCH = 2**53 - 1  # The Worker represents epochs with safe JS integers.
PIN_FIELDS = {
    "environment", "candidate_sha", "worker_version", "image_digest", "owner_epoch",
    "started_at", "completed_at",
}
RELEASE_FLAGS = {"production_lane_non_overlapping", "production_auto_promote_disabled"}
# For each domain, the timestamp fields, true attestations, and zero counts.
RECEIPT_FIELDS = {
    "naming": (("empty_room_rename_at", "occupied_room_return_at"), (), ()),
    "help": (("observed_at",), ("branding_confirmed", "cleanup_confirmed"), ()),
    "gateway": (("endpoint_policy_refused_at",), (), ("unexpected_allows",)),
    "event_write_fence": (
        ("discord_permissions_readback_at",), ("current_and_retained_discord_fenced",), ("cross_rollback_writes",),
    ),
}


@dataclass(frozen=True)
class ProductionWatchResult:
    status: Literal["disabled", "refused", "verified"]
    reason: str | None = None


DISABLED = ProductionWatchResult("disabled")


def _has_fields(value, fields):
    return type(value) is dict and value.keys() == fields


def _matches(pattern, value):
    return type(value) is str and pattern.fullmatch(value) is not None


def _instant(value):
    # Bound work on malformed input while retaining the full observed fraction.
    if type(value) is not str or len(value) > 64:
        raise ValueError
    return utc_instant(value)


def _pin(value):
    environment = value["environment"]
    sha, worker, digest, epoch = (
        value["candidate_sha"], value["worker_version"], value["image_digest"], value["owner_epoch"],
    )
    start, end = _instant(value["started_at"]), _instant(value["completed_at"])
    if (type(environment) is not str or environment != "production"
            or not _matches(SHA, sha) or not _matches(UUID_RE, worker) or not _matches(IMAGE_DIGEST, digest)
            or type(epoch) is not int or not 0 <= epoch <= MAX_OWNER_EPOCH
            or start >= end):
        raise ValueError
    return (sha, worker, digest, epoch), (start, end)


def _receipt_matches(domain, receipt, pin):
    timestamps, required_true, required_zero = RECEIPT_FIELDS[domain]
    if not _has_fields(receipt, PIN_FIELDS.union(timestamps, required_true, required_zero)):
        return False
    try:
        if _pin(receipt) != pin:
            return False
        observed = tuple(_instant(receipt[field]) for field in timestamps)
    except ValueError:
        return False
    start, end = pin[1]
    if any(not start <= instant <= end for instant in observed):
        return False
    if domain == "naming" and observed[0] >= observed[1]:
        return False
    return (all(receipt[field] is True for field in required_true)
            and all(type(receipt[field]) is int and receipt[field] == 0 for field in required_zero))


def verify_production_watch_bundle(release=None, bundle=None, *, enabled=False) -> ProductionWatchResult:
    """Verify four receipts against an independent release pin when explicitly enabled.

    The release has PIN_FIELDS and RELEASE_FLAGS. The bundle has one receipt per
    domain in RECEIPT_FIELDS. Each receipt repeats PIN_FIELDS and carries only
    that domain's timestamp and outcome fields. Watch bounds are inclusive and
    must span a positive interval. Receipt bounds must match the whole watch.

    Naming timestamps attest both successful transitions of the same occupied
    room. Help's observation attests branding and completed cleanup on the
    pinned serving release. Gateway requires an observed endpoint policy refusal
    and zero unexpected allows. The event fence attests effective Discord denial
    for both current and retained runtime identities, with zero cross-rollback
    writes over the declared capture window.
    """
    if enabled is not True:
        return DISABLED
    if not _has_fields(release, PIN_FIELDS | RELEASE_FLAGS):
        return ProductionWatchResult("refused", "invalid_release_pin")
    if release["production_lane_non_overlapping"] is not True:
        return ProductionWatchResult("refused", "production_lane_overlap_or_unverified")
    if release["production_auto_promote_disabled"] is not True:
        return ProductionWatchResult("refused", "production_auto_promote_not_disabled")
    try:
        pin = _pin(release)
    except ValueError:
        return ProductionWatchResult("refused", "invalid_release_pin")
    if not _has_fields(bundle, RECEIPT_FIELDS.keys()):
        return ProductionWatchResult("refused", "invalid_bundle_inventory")
    for domain in RECEIPT_FIELDS:
        if not _receipt_matches(domain, bundle[domain], pin):
            return ProductionWatchResult("refused", f"invalid_{domain}_receipt")
    return ProductionWatchResult("verified")
