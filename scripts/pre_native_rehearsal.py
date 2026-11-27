"""Dormant verifier for an explicit Durable Object Container start rehearsal."""

from collections.abc import Callable
from dataclasses import dataclass, field
import re
from typing import Protocol
from urllib.parse import urlsplit
from uuid import UUID

from pre_start_stop import Deployment, EffectWindow
from pre_target import DIGEST, Target, TargetError, require
from pre_worker_upload import DATABASE_NAME, DNS_LABEL, DatabaseEndpoint


LOOPBACK_AUTHORITY = re.compile(r"(?:127\.0\.0\.1|\[::1\]|localhost)(?::[1-9][0-9]{0,4})?")
DATABASE_HOST = re.compile(rf"{DNS_LABEL}(?:\.{DNS_LABEL})*")


@dataclass(frozen=True)
class LaunchPolicy:
    """Effective options for native start(), with a pre-start fixture observation.

    The protected adapter verifies both listeners are ready in the bot's
    network namespace and applies the two URLs as DISCORD_GATEWAY_URL and
    DISCORD_API_BASE in the process environment at the effect boundary.
    """

    scheduling_policy: str
    discord_gateway_url: str = field(repr=False)
    discord_api_base: str = field(repr=False)
    fixtures_ready_in_namespace: bool
    enable_internet: bool
    database: DatabaseEndpoint


@dataclass(frozen=True)
class RuntimeIdentity:
    """Pins or independently inspected running identity and effective writer.

    Readback must identify the running process's image and actual database
    write destination and user. Configuration or an echoed start reply alone
    cannot establish that identity.
    """

    deployment: Deployment
    image_digest: str
    database_writer: DatabaseEndpoint


@dataclass(frozen=True)
class StopReadback:
    """Direct, fresh post-stop observation of the exact rehearsal deployment."""

    deployment: Deployment
    active_instances: int
    assigned_instances: int
    startup_drained: bool
    restart_fenced: bool


class StopCustody(Protocol):
    """Independent authority that stays responsible if the runner disappears.

    Arm atomically claims the exact reservation and deployment once and
    durably schedules stop at the original cutoff before acknowledging.
    Stop closes admission, fences pending effects and requests stop for only
    the owned deployment, including after cutoff or loss of a start reply.
    Custody and the restart fence persist through independent reconciliation.
    """

    def arm(self, window: EffectWindow, deployment: Deployment) -> bool: ...
    def stop(self, deployment: Deployment) -> bool: ...


def _call(operation, *args, failure):
    try:
        return operation(*args)
    except Exception:
        pass
    raise TargetError(failure)


def _loopback(url, schemes):
    if (type(url) is not str or not 0 < len(url) <= 2048
            or any(not "!" <= char <= "~" or char in "\\#" for char in url)):
        return False
    try:
        parsed = urlsplit(url)
        return (parsed.scheme in schemes and LOOPBACK_AUTHORITY.fullmatch(parsed.netloc)
                and (parsed.port is None or parsed.port <= 65535))
    except ValueError:
        return False


def _deployment(value, target):
    return (type(value) is Deployment and type(value.target) is Target and value.target == target
            and all(type(identifier) is UUID and identifier.int for identifier in
                    (value.start_id, value.worker_version, value.container_id)))


def _database(value, target):
    return (type(value) is DatabaseEndpoint and type(value.target) is Target and value.target == target
            and type(value.service) is str and value.service == target.service_name
            and value.disposable is True and type(value.port) is int and 0 < value.port <= 65535
            and type(value.host) is str and 0 < len(value.host) <= 253
            and DATABASE_HOST.fullmatch(value.host)
            and all(type(name) is str and re.fullmatch(DATABASE_NAME, name)
                    for name in (value.database, value.user)))


def _runtime(value, target):
    return (type(value) is RuntimeIdentity and _deployment(value.deployment, target)
            and type(value.image_digest) is str and DIGEST.fullmatch(value.image_digest)
            and _database(value.database_writer, target))


def _check_time(window, clock, previous=None):
    now = _call(clock, failure="pre_native_clock_unconfirmed")
    window.require_open(now)
    require(previous is None or now >= previous, "pre_native_clock_regressed")
    return now


def rehearse_native(
    window: EffectWindow, expected: RuntimeIdentity, policy: LaunchPolicy, *,
    clock: Callable[[], int], custody: StopCustody,
    launch: Callable[[EffectWindow, Deployment, LaunchPolicy], Deployment],
    read_runtime: Callable[[Deployment], RuntimeIdentity],
    read_stop: Callable[[Deployment], StopReadback],
) -> StopReadback:
    """Launch once, verify independently, and stop under retained custody.

    The protected caller supplies separately authenticated pins, a trusted
    authority clock and independent runtime and stop observations. Launch
    must recheck the exact armed claim, cutoff, fence and effective policy at
    native start() itself. Readbacks inspect the exact deployment directly.
    Stop runs even after expiry or a failed launch. This function never
    releases the reservation or disarms the independent stop authority.
    """
    require(type(window) is EffectWindow, "invalid_pre_native_window")
    target = window.target
    require(_runtime(expected, target), "invalid_pre_native_pins")
    require(type(policy) is LaunchPolicy and type(policy.scheduling_policy) is str
            and policy.scheduling_policy == "durable-object", "pre_native_scheduling_required")
    require(policy.enable_internet is False, "pre_native_public_egress_unconfirmed")
    require(policy.fixtures_ready_in_namespace is True
            and _loopback(policy.discord_gateway_url, ("ws", "wss"))
            and _loopback(policy.discord_api_base, ("http", "https")), "pre_native_discord_fixture_unconfirmed")
    require(_database(policy.database, target) and policy.database == expected.database_writer,
            "pre_native_database_mismatch")

    deployment = expected.deployment
    now = _check_time(window, clock)
    armed = _call(lambda: custody.arm(window, deployment), failure="pre_native_stop_unarmed")
    require(armed is True, "pre_native_stop_unarmed")
    try:
        now = _check_time(window, clock, now)
        reply = _call(launch, window, deployment, policy, failure="pre_native_launch_unconfirmed")
        require(_deployment(reply, target) and reply == deployment, "pre_native_launch_unconfirmed")
        observed = _call(read_runtime, deployment, failure="pre_native_runtime_unconfirmed")
        require(_runtime(observed, target) and observed == expected, "pre_native_runtime_unconfirmed")
    finally:
        stopped = _call(lambda: custody.stop(deployment), failure="pre_native_stop_unconfirmed")
        require(stopped is True, "pre_native_stop_unconfirmed")
        stop = _call(read_stop, deployment, failure="pre_native_stop_unconfirmed")
        require(type(stop) is StopReadback and _deployment(stop.deployment, target)
                and stop.deployment == deployment and type(stop.active_instances) is int
                and type(stop.assigned_instances) is int and stop.active_instances == stop.assigned_instances == 0
                and stop.startup_drained is True and stop.restart_fenced is True, "pre_native_stop_unconfirmed")
        _check_time(window, clock, now)
    return stop
