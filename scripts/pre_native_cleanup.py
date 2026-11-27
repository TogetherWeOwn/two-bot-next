"""Verify authenticated stop and cleanup receipts for one first-attempt PRE reservation."""

from dataclasses import dataclass
import re
from typing import Protocol

from pre_registry_cleanup import RegistryReservation
from pre_target import APP_ID, TargetError, check_clock, require


@dataclass(frozen=True)
class Reservation:
    registry: RegistryReservation
    account_id: str
    app_id: str | None
    lease_cutoff: int | None = None

    def __post_init__(self):
        require(type(self.registry) is RegistryReservation, "invalid_pre_registry_pin")
        require(type(self.account_id) is str and re.fullmatch(r"[0-9a-f]{32}", self.account_id),
                "invalid_pre_cleanup_account")
        require(self.app_id is None or (type(self.app_id) is str and APP_ID.fullmatch(self.app_id)),
                "invalid_pre_cleanup_app")
        if self.lease_cutoff is not None:
            check_clock(self.registry.target, self.lease_cutoff)
            require(self.lease_cutoff > self.registry.target.started_at, "invalid_pre_lease_cutoff")

    @property
    def cutoff(self) -> int:
        expiry = self.registry.target.expires_at
        return expiry if self.lease_cutoff is None else min(expiry, self.lease_cutoff)


@dataclass(frozen=True)
class FenceCheck:
    """Effect-boundary readback of the original fence during one disruption."""

    disruption: str
    fence_id: str
    cutoff: int
    observed_at: int
    blocked: bool


@dataclass(frozen=True)
class StopEvidence:
    """Authenticated lifecycle and settlement for one reservation.

    The fence ID identifies its arm event and generation. runner_fenced_at is
    when the fence stopped the runtime or prevented startup under runner loss.
    first_create_at is the first operation allowed past the fence, or None on
    proof none was allowed. The two other checks confirm effect-boundary denial.
    startup_settled confirms closed admission and settled pending creates and
    starts. write_access covers effective access, including delegated roles and
    open connections. worker_removed_at records removal or confirmed absence
    after settlement. app_never_created requires affirmative lifecycle proof.
    """

    reservation: Reservation
    fence_id: str
    armed_at: int
    armed_cutoff: int
    first_create_at: int | None
    independent: bool
    runner_loss: FenceCheck
    runner_fenced_at: int
    delayed_create: FenceCheck
    legacy_keepalive: FenceCheck
    settled_at: int
    startup_settled: bool
    write_access: bool
    worker_removed_at: int
    app_deleted_at: int | None
    app_never_created: bool
    publication_settled_at: int


@dataclass(frozen=True)
class AppCounts:
    """Direct active and assigned counts while the pinned app is readable.

    An empty Cloudflare instance listing can lag and cannot supply either count.
    """

    reservation: Reservation
    observed_at: int
    active: int
    assigned: int


@dataclass(frozen=True)
class AbsenceReadback:
    """Complete handback readback for the Worker, ingress, app and disposable writer."""

    reservation: Reservation
    observed_at: int
    worker_present: bool
    subdomain_present: bool
    preview_present: bool
    routes_present: bool
    custom_domains_present: bool
    app_present: bool
    write_access: bool


@dataclass(frozen=True)
class NativeRegistryReadback:
    """Authoritative presence read for the exact reserved digest reference."""

    reservation: Reservation
    observed_at: int
    present: bool


class Authority(Protocol):
    def read_stop(self, reservation: Reservation) -> StopEvidence: ...


class Transport(Protocol):
    def read_app_counts(self, reservation: Reservation) -> AppCounts: ...
    def read_absence(self, reservation: Reservation) -> AbsenceReadback: ...
    def read_registry(self, reservation: Reservation) -> NativeRegistryReadback: ...


def _read(reader, reservation, receipt_type, reason):
    try:
        receipt = reader(reservation)
    except Exception:
        raise TargetError(reason) from None
    require(type(receipt) is receipt_type and type(receipt.reservation) is Reservation
            and receipt.reservation == reservation, reason)
    return receipt


def _verify_fence(reservation, stop, now):
    target = reservation.registry.target
    cutoff = reservation.cutoff
    require(stop.independent is True and type(stop.fence_id) is str
            and 0 < len(stop.fence_id) <= 128 and type(stop.armed_cutoff) is int
            and stop.armed_cutoff == cutoff, "unconfirmed_pre_independent_fence")
    require(type(stop.armed_at) is int and target.started_at <= stop.armed_at < cutoff,
            "invalid_pre_fence_arm_time")
    if stop.first_create_at is not None:
        require(type(stop.first_create_at) is int and stop.armed_at < stop.first_create_at < cutoff,
                "pre_create_outside_fence")
    require(type(stop.runner_fenced_at) is int
            and (stop.first_create_at or stop.armed_at) <= stop.runner_fenced_at <= min(cutoff, stop.settled_at),
            "pre_runner_stop_unconfirmed")
    for disruption, check in (("runner_loss", stop.runner_loss), ("delayed_create", stop.delayed_create),
                              ("legacy_keepalive", stop.legacy_keepalive)):
        require(type(check) is FenceCheck and check.blocked is True
                and type(check.disruption) is str and check.disruption == disruption
                and type(check.fence_id) is str and check.fence_id == stop.fence_id
                and type(check.cutoff) is int and check.cutoff == cutoff
                and type(check.observed_at) is int and cutoff <= check.observed_at <= now,
                "pre_cutoff_fence_unconfirmed")


def verify_cleanup(reservation: Reservation, *, authority: Authority, transport: Transport,
                   readback_not_before: int, now: int) -> None:
    """Verify one reservation, including fresh absence before handback.

    The caller supplies trusted pins, time and a readback freshness floor.
    Readers derive scope from authenticated receipts and use a common clock.
    Both writer checks cover the disposable database pinned to the Target.
    A settled publication can no longer create the exact registry reference,
    including after a lost copy reply. A still-present reference is unresolved.
    """
    require(type(reservation) is Reservation, "invalid_pre_cleanup_reservation")
    target = reservation.registry.target
    check_clock(target, now)
    check_clock(target, readback_not_before)
    require(readback_not_before <= now, "invalid_pre_readback_window")

    stop = _read(authority.read_stop, reservation, StopEvidence, "wrong_pre_stop_evidence")
    require(type(stop.settled_at) is int and stop.settled_at <= now and stop.startup_settled is True,
            "pre_startup_unsettled")
    _verify_fence(reservation, stop, now)
    require(stop.write_access is False, "pre_writer_revocation_unconfirmed")
    require(type(stop.worker_removed_at) is int and stop.settled_at < stop.worker_removed_at <= now,
            "pre_worker_removal_unconfirmed")

    cleanup_at = stop.worker_removed_at
    if reservation.app_id is None:
        require(stop.app_never_created is True and stop.app_deleted_at is None,
                "pre_app_never_created_unconfirmed")
    else:
        require(stop.app_never_created is False and stop.first_create_at is not None
                and type(stop.app_deleted_at) is int and cleanup_at < stop.app_deleted_at <= now,
                "pre_app_deletion_unconfirmed")
        counts = _read(transport.read_app_counts, reservation, AppCounts, "pre_app_counts_unconfirmed")
        require(type(counts.observed_at) is int and cleanup_at < counts.observed_at < stop.app_deleted_at
                and type(counts.active) is int and type(counts.assigned) is int
                and counts.active == counts.assigned == 0, "pre_app_stop_unconfirmed")
        cleanup_at = stop.app_deleted_at

    absence = _read(transport.read_absence, reservation, AbsenceReadback, "wrong_pre_absence_readback")
    require(type(absence.observed_at) is int and cleanup_at < absence.observed_at <= now
            and readback_not_before <= absence.observed_at, "stale_pre_absence_readback")
    require(absence.write_access is False, "pre_writer_revocation_unconfirmed")
    require(all(present is False for present in (
        absence.worker_present, absence.subdomain_present, absence.preview_present,
        absence.routes_present, absence.custom_domains_present, absence.app_present,
    )), "pre_absence_unconfirmed")

    require(type(stop.publication_settled_at) is int
            and target.started_at <= stop.publication_settled_at <= now, "pre_publication_unsettled")
    registry = _read(transport.read_registry, reservation, NativeRegistryReadback, "wrong_pre_registry_readback")
    require(type(registry.observed_at) is int and stop.publication_settled_at < registry.observed_at <= now
            and absence.observed_at <= registry.observed_at and registry.present is False,
            "pre_registry_disposition_unsettled")
