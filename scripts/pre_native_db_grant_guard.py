"""Offline grant and revocation evidence for one native PRE reservation."""

from dataclasses import dataclass
import re

from pre_native_cleanup import Reservation
from pre_target import Target, check_clock, require
from pre_worker_upload import DATABASE_NAME, DATABASE_TLS, DNS_LABEL, DatabaseEndpoint


_HOST = re.compile(rf"(?:{DNS_LABEL}\.)+[a-z](?:[a-z0-9-]{{0,61}}[a-z0-9])?")
_SCHEMA_MODES = ("pre_migration", "post_migration")


@dataclass(frozen=True, repr=False)
class DatabaseIdentity:
    """Endpoint and effective TLS policy, with no credential-bearing field."""

    endpoint: DatabaseEndpoint
    tls_mode: str
    tls_required: bool


@dataclass(frozen=True, repr=False)
class GrantReadback:
    """Complete effective privileges and catalog evidence for one schema mode.

    Derive schema_shape from the observed catalog. The three counts cover all
    production or shared grants, including effective privileges through role
    membership, ownership and PUBLIC. Cover inherited and assumable authority
    when assessing flags and cross-schema writes. Incomplete inventory cannot
    establish that these counts or flags are clear.
    """

    identity: DatabaseIdentity
    schema_mode: str
    schema_shape: str
    complete: bool
    restricted_database_grants: int
    restricted_schema_grants: int
    restricted_role_grants: int
    superuser: bool
    bypassrls: bool
    createrole: bool
    createdb: bool
    cross_schema_write: bool


@dataclass(frozen=True, repr=False)
class RevocationReadback:
    """Authoritative readback of effective access after writer shutdown."""

    identity: DatabaseIdentity
    revoked_at: int
    delegated_roles_revoked: bool
    active_sessions_terminated: bool
    observed_after_revocation: int
    remaining_write_grants: int


@dataclass(frozen=True)
class GrantVerdict:
    code: str


def _valid_identity(identity, target):
    if type(identity) is not DatabaseIdentity or type(identity.endpoint) is not DatabaseEndpoint:
        return False
    endpoint = identity.endpoint
    observed = endpoint.target
    return (type(observed) is Target and type(observed.scope) is str and type(observed.shape) is str
            and type(observed.run_id) is int and type(observed.started_at) is int and observed == target
            and type(endpoint.service) is str and endpoint.service == target.service_name
            and endpoint.disposable is True
            and type(endpoint.host) is str and len(endpoint.host) <= 253 and _HOST.fullmatch(endpoint.host)
            and type(endpoint.port) is int and 0 < endpoint.port <= 65535
            and all(type(name) is str and 0 < len(name) <= 63 and re.fullmatch(DATABASE_NAME, name)
                    for name in (endpoint.database, endpoint.user))
            and identity.tls_required is True and type(identity.tls_mode) is str
            and len(identity.tls_mode) <= len("verify-full")
            and re.fullmatch(DATABASE_TLS, "sslmode=" + identity.tls_mode) is not None)


def _zero(count):
    return type(count) is int and count == 0


def verify_native_pre_db_grants(*, enabled=False, reservation: Reservation | None = None,
                               pinned_identity: DatabaseIdentity | None = None,
                               grant_rows: tuple[GrantReadback, ...] | None = None,
                               revocation: RevocationReadback | None = None,
                               stopped_at: int | None = None, now: int | None = None) -> GrantVerdict:
    """Verify protected reservation evidence or raise TargetError with a fixed code.

    The protected caller authenticates the reservation and independently
    obtains its database pin, both catalog/grant observations, the stop time
    and the post-stop database readback. Use a common Unix-second clock.
    Success verifies supplied evidence and confers no operation authority.
    """
    if enabled is not True:
        return GrantVerdict("disabled")

    require(type(reservation) is Reservation, "invalid_pre_db_reservation")
    target = reservation.registry.target
    require(type(target.scope) is str and type(target.shape) is str, "invalid_pre_db_reservation")
    check_clock(target, now)
    check_clock(target, stopped_at)
    require(stopped_at <= now, "invalid_pre_db_stop_time")
    require(_valid_identity(pinned_identity, target), "invalid_pre_database_pin")
    require(type(grant_rows) is tuple and len(grant_rows) == len(_SCHEMA_MODES),
            "invalid_pre_db_grant_rows")

    modes = set()
    for row in grant_rows:
        require(type(row) is GrantReadback and row.complete is True, "incomplete_pre_db_grants")
        require(_valid_identity(row.identity, target) and row.identity == pinned_identity,
                "pre_database_endpoint_mismatch")
        require(type(row.schema_mode) is str and row.schema_mode in _SCHEMA_MODES
                and row.schema_mode not in modes and type(row.schema_shape) is str
                and row.schema_shape == target.shape, "pre_db_schema_state_mismatch")
        modes.add(row.schema_mode)
        require(all(_zero(count) for count in (row.restricted_database_grants, row.restricted_schema_grants,
                                               row.restricted_role_grants)), "pre_db_restricted_grants")
        require(all(flag is False for flag in (row.superuser, row.bypassrls, row.createrole, row.createdb,
                                               row.cross_schema_write)), "pre_db_privileged_grants")

    require(type(revocation) is RevocationReadback and _valid_identity(revocation.identity, target)
            and revocation.identity == pinned_identity, "wrong_pre_db_revocation_target")
    require(type(revocation.revoked_at) is int
            and target.started_at <= revocation.revoked_at <= reservation.cutoff,
            "invalid_pre_db_revocation_time")
    require(type(revocation.observed_after_revocation) is int
            and max(stopped_at, revocation.revoked_at) <= revocation.observed_after_revocation <= now,
            "stale_pre_db_revocation_readback")
    require(revocation.delegated_roles_revoked is True and revocation.active_sessions_terminated is True
            and _zero(revocation.remaining_write_grants), "pre_writer_revocation_unconfirmed")
    return GrantVerdict("verified")
