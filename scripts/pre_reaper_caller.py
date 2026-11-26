"""Dormant caller contract for an independent protected PRE reaper."""

from contextlib import AbstractContextManager
from dataclasses import dataclass, field
from typing import Generic, Literal, Protocol, TypeVar
from uuid import UUID

from pre_registry_cleanup import RegistryDisposition
from pre_sweep import SweepError
from pre_target import Target, check_clock, require


Adapter = TypeVar("Adapter")


@dataclass(frozen=True)
class ReaperReservation:
    reference: UUID
    target: Target


@dataclass(frozen=True)
class CleanupClaim(Generic[Adapter]):
    reservation: ReaperReservation
    adapter: Adapter = field(repr=False)


class ReaperAuthority(Protocol[Adapter]):
    """The protected host authenticates the reaper independently of the start job."""

    def resolve(self, reference: UUID) -> ReaperReservation | None:
        """Read the original immutable record, written durably before any start effect.

        Authenticate the protected caller and its access to the record. The
        reference is a lookup key. Refuse ambiguous or unauthorized reads.
        """

    def trusted_now(self) -> int:
        """Read nondecreasing authority time or fail on uncertainty."""

    def run_status(self, target: Target) -> Literal["active", "terminal"]:
        """Read authenticated status of the original run and attempt or fail."""

    def cleanup_claim(self, reservation: ReaperReservation) -> AbstractContextManager[CleanupClaim[Adapter] | None]:
        """Recheck authorization and cutoff, then claim the exact provider scope.

        Exclude all competing cleanup and starting-runner writes, including
        delayed effects, before yielding. Serialize on the owned provider
        resources as well as the full reservation because names can collide.
        Bind adapter access to the claim at the provider effect boundary and
        revoke it on exit or loss. Preserve the independent start fence after
        exit. Startup settlement has its own evidence and can remain pending.
        Yield None or raise if any of these conditions is unconfirmed. A
        GitHub Actions queue provides scheduling only.
        """


def reap_pre(reference: str, *, authority: ReaperAuthority[Adapter], finalize) -> Literal["spared"] | RegistryDisposition:
    """Resolve one reservation and invoke the protected PRE finalizer under its claim.

    Pass `pre_caller.finalize_pre` as `finalize`. Only ALREADY_ABSENT supports
    handback. A failed or unresolved run can be replayed with the same reference
    to obtain fresh proof. There is no workflow trigger or provider I/O here.
    """
    try:
        require(type(reference) is str and len(reference) == 36, "invalid_pre_reaper_reference")
        key = UUID(reference)
        require(key.int != 0 and str(key) == reference, "invalid_pre_reaper_reference")
        reservation = authority.resolve(key)
        require(type(reservation) is ReaperReservation and type(reservation.reference) is UUID
                and reservation.reference == key and type(reservation.target) is Target,
                "unconfirmed_pre_reaper_reservation")
        target = reservation.target
        status = authority.run_status(target)
        now = authority.trusted_now()
        check_clock(target, now)
        require(type(status) is str and status in ("active", "terminal"), "unconfirmed_pre_run_status")
    except Exception:
        raise SweepError("PRE reaper reservation or eligibility could not be verified") from None
    if status == "active" and now < target.expires_at:
        return "spared"

    disposition = None
    authorized_target = None
    try:
        with authority.cleanup_claim(reservation) as claim:
            require(type(claim) is CleanupClaim and type(claim.reservation) is ReaperReservation
                    and type(claim.reservation.reference) is UUID and type(claim.reservation.target) is Target
                    and claim.reservation == reservation and claim.adapter is not None,
                    "unconfirmed_pre_cleanup_claim")
            authorized_target = target
            try:
                disposition = finalize(
                    target, authorize=lambda candidate: type(candidate) is Target and candidate == authorized_target,
                    trusted_now=authority.trusted_now, adapter=claim.adapter,
                )
            finally:
                authorized_target = None
        require(type(disposition) is RegistryDisposition, "unconfirmed_pre_reaper_disposition")
    except Exception:
        raise SweepError("PRE reaper claim or finalization could not be verified") from None
    return disposition
