"""Report the registry disposition for an isolated predecessor reservation."""

from dataclasses import dataclass
from enum import StrEnum
import re

from pre_target import DIGEST, Inventory, Target, plan_cleanup, require, verify_cleanup


# Match the dedicated repository format used by the #2568 publication plan.
_DNS_LABEL = r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?"
_STAGING_REPOSITORY = re.compile(
    rf"(?:{_DNS_LABEL}\.)+{_DNS_LABEL}/[a-z0-9]+(?:[._-][a-z0-9]+)*/"
    r"two-bot-next-attendance-predecessor-staging"
)


class RegistryDisposition(StrEnum):
    ALREADY_ABSENT = "already_absent"
    UNRESOLVED_DIGEST_REFERENCE = "unresolved_digest_reference"


@dataclass(frozen=True)
class RegistryReservation:
    """The protected caller records the planned immutable reference before copy."""

    target: Target
    reference: str

    def __post_init__(self):
        require(type(self.target) is Target, "invalid_pre_registry_target")
        require(type(self.reference) is str and len(self.reference) <= 255 + len("@sha256:") + 64,
                "invalid_pre_registry_reference")
        repository, _, digest = self.reference.partition("@")
        require(len(repository) <= 255 and _STAGING_REPOSITORY.fullmatch(repository)
                and DIGEST.fullmatch(digest), "invalid_pre_registry_reference")


@dataclass(frozen=True)
class PublicationSettlement:
    """Affirmative, independently verified settlement of the reserved publication.

    The caller binds this receipt to the original first-attempt reservation
    after proving that its publisher can no longer create the reference,
    including after a lost copy result.
    """

    reservation: RegistryReservation


@dataclass(frozen=True)
class RegistryReadback:
    """Fresh, authoritative presence for the exact reference, without redirects.

    Use False only when the authenticated lookup confirms absence in the
    reserved repository. Use None for an ambiguous result or failed lookup.
    A present result requires the verified OCI digest readback from #2568.
    """

    target: Target
    reference: str
    present: bool | None


def registry_cleanup_disposition(
    reservation: RegistryReservation, inventory: Inventory, readback: RegistryReadback, *,
    now: int, run_status: str, publication_settlement: PublicationSettlement | None,
) -> RegistryDisposition | None:
    """Report a disposition after protected cleanup and publication settlement.

    The caller authenticates the reservation, run status, settlement receipt
    and exact-scope readbacks. It observes the registry after verifying target
    cleanup and publication settlement. None for the receipt means settlement
    is unconfirmed.

    None spares an active reservation before its cutoff. The returned verdict
    conveys no mutation authority.
    """
    require(type(reservation) is RegistryReservation, "invalid_pre_registry_reservation")
    target = reservation.target
    require(type(readback) is RegistryReadback and readback.target == target
            and readback.reference == reservation.reference, "wrong_pre_registry_scope")
    if plan_cleanup(target, inventory, now=now, run_status=run_status) is None:
        return None
    verify_cleanup(target, inventory)
    require(type(publication_settlement) is PublicationSettlement, "pre_registry_publication_unsettled")
    require(type(publication_settlement.reservation) is RegistryReservation
            and publication_settlement.reservation == reservation, "wrong_pre_registry_publication_scope")
    require(type(readback.present) is bool, "unconfirmed_pre_registry_readback")
    if readback.present:
        return RegistryDisposition.UNRESOLVED_DIGEST_REFERENCE
    return RegistryDisposition.ALREADY_ABSENT
