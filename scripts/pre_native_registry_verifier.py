"""Verify a native PRE registry closure from authenticated offline evidence."""

from dataclasses import dataclass
from hashlib import sha256
from json import dumps
from uuid import UUID

from pre_admission import AdmissionRecord, SettlementSource
from pre_registry_cleanup import RegistryDisposition, RegistryReservation
from pre_start_stop import Deployment, EffectWindow
from pre_target import Target, require


@dataclass(frozen=True)
class NativeRegistryReadback:
    """Authoritative registry disposition after cleanup and publication settlement."""

    target: Target
    reference: str
    disposition: RegistryDisposition
    inventory_complete: bool
    observed_at: int


@dataclass(frozen=True)
class NativeRegistryClosureReceipt:
    closure_digest: str


def _matches_target(candidate: Target, expected: Target) -> bool:
    return (type(candidate) is Target and type(candidate.scope) is str
            and type(candidate.shape) is str and candidate == expected)


def verify_native_pre_registry_closure(
    deployment: Deployment, *, window: EffectWindow, record: AdmissionRecord,
    reservation: RegistryReservation, readback: NativeRegistryReadback,
    promotion_disabled: bool, now: int,
) -> NativeRegistryClosureReceipt:
    """Bind a closure receipt to one deployed reservation and its readback.

    The caller authenticates the inputs, including the current durable admission
    record, promotion hold, clock and registry disposition for the reserved
    reference. The receipt attests their consistency and freshness.
    """
    require(promotion_disabled is True, "pre_native_promotion_hold_unconfirmed")
    require(type(window) is EffectWindow, "invalid_pre_native_window")
    target = window.target
    require(type(target.scope) is str and type(target.shape) is str, "invalid_pre_native_target")
    window.require_open(now)

    require(type(deployment) is Deployment and _matches_target(deployment.target, target)
            and all(type(identity) is UUID and identity.int != 0 for identity in (
                deployment.start_id, deployment.worker_version, deployment.container_id,
            )), "invalid_pre_native_deployment")
    require(type(record) is AdmissionRecord and _matches_target(record.target, target)
            and record.closed is True and type(record.settled_by) is SettlementSource
            and record.settled_by in (SettlementSource.PROVIDER, SettlementSource.RUNTIME)
            and type(record.start_id) is UUID and record.start_id == deployment.start_id,
            "pre_native_admission_unsettled")
    require(type(reservation) is RegistryReservation and _matches_target(reservation.target, target),
            "wrong_pre_native_registry_reservation")
    require(type(readback) is NativeRegistryReadback and _matches_target(readback.target, target)
            and type(readback.reference) is str and readback.reference == reservation.reference
            and readback.inventory_complete is True
            and readback.disposition is RegistryDisposition.ALREADY_ABSENT,
            "pre_native_registry_absence_unconfirmed")
    require(type(readback.observed_at) is int and target.started_at <= readback.observed_at
            and 0 <= now - readback.observed_at <= 3600, "invalid_pre_native_registry_readback_time")

    evidence = {
        "kind": "native_pre_registry_closure",
        "target": {
            "scope": target.scope, "run_id": target.run_id,
            "started_at": target.started_at, "shape": target.shape,
        },
        "cutoff": window.cutoff,
        "start_id": str(deployment.start_id),
        "worker_version": str(deployment.worker_version),
        "container_id": str(deployment.container_id),
        "settled_by": record.settled_by.value,
        "reference": reservation.reference,
        "observed_at": readback.observed_at,
        "disposition": readback.disposition.value,
    }
    canonical = dumps(evidence, sort_keys=True, separators=(",", ":"))
    return NativeRegistryClosureReceipt(sha256(canonical.encode("utf-8")).hexdigest())
