"""Dormant verifier for an explicit Durable Object Container start rehearsal."""

from collections.abc import Callable
from dataclasses import dataclass, field
import re
from typing import Protocol
from uuid import UUID

from pre_start_stop import Deployment, EffectWindow
from pre_target import DIGEST, Target, TargetError, require
from pre_worker_upload import DATABASE_NAME, DNS_LABEL, DatabaseEndpoint


LOOPBACK_IP = r"127\.0\.0\.1|\[::1\]"
PORT = r"(?P<port>[1-9][0-9]{0,4})"
GATEWAY_URL = re.compile(rf"ws://(?P<host>{LOOPBACK_IP}):{PORT}")
REST_URL = re.compile(rf"http://(?P<host>{LOOPBACK_IP}|localhost)(?::{PORT})?/?")
DATABASE_HOST = re.compile(rf"{DNS_LABEL}(?:\.{DNS_LABEL})*")


@dataclass(frozen=True)
class LaunchPolicy:
    """Native start() options and listener readiness in the bot namespace.

    Apply DISCORD_GATEWAY_URL and DISCORD_API_BASE at start(). Supply all
    addresses resolved in that namespace when the REST origin uses localhost.
    """

    scheduling_policy: str
    discord_gateway_url: str = field(repr=False)
    discord_api_base: str = field(repr=False)
    fixtures_ready_in_namespace: bool
    enable_internet: bool
    database: DatabaseEndpoint
    localhost_addresses: frozenset[str] = frozenset()


@dataclass(frozen=True)
class RuntimeIdentity:
    """Pins for the deployment, its image, and the actual database writer."""

    deployment: Deployment
    image_digest: str
    database_writer: DatabaseEndpoint


@dataclass(frozen=True)
class RuntimeReadback:
    """Independent observation of actual image, writer and namespace policy."""

    identity: RuntimeIdentity
    policy: LaunchPolicy


@dataclass(frozen=True)
class StopReadback:
    """Direct, fresh post-stop observation of the exact rehearsal deployment."""

    deployment: Deployment
    stop_id: UUID
    active_instances: int
    assigned_instances: int
    startup_drained: bool
    restart_fenced: bool


class StopCustody(Protocol):
    """Independent authority that stays responsible if the runner disappears.

    Arm atomically claims the exact deployment once and durably schedules stop
    at the original cutoff before acknowledging.
    Stop closes admission, requests shutdown and records a unique receipt ID on
    the durable fence for independent readback, even after cutoff or lost start.
    """

    def arm(self, window: EffectWindow, deployment: Deployment) -> bool: ...
    def stop(self, deployment: Deployment) -> UUID: ...


def _call(operation, *args, failure):
    try:
        return operation(*args)
    except Exception:
        pass
    raise TargetError(failure)


def _loopback(url, pattern, addresses=frozenset()):
    match = pattern.fullmatch(url) if type(url) is str else None
    return (match is not None and (match["port"] is None or int(match["port"]) <= 65535)
            and type(addresses) is frozenset and len(addresses) <= 2
            and bool(addresses) == (match["host"] == "localhost")
            and all(type(address) is str and address in ("127.0.0.1", "::1") for address in addresses))


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


def _check_policy(policy, target, database):
    require(type(policy) is LaunchPolicy and type(policy.scheduling_policy) is str
            and policy.scheduling_policy == "durable-object", "pre_native_scheduling_required")
    require(policy.enable_internet is False, "pre_native_public_egress_unconfirmed")
    require(policy.fixtures_ready_in_namespace is True
            and _loopback(policy.discord_gateway_url, GATEWAY_URL)
            and _loopback(policy.discord_api_base, REST_URL, policy.localhost_addresses), "pre_native_discord_fixture_unconfirmed")
    require(_database(policy.database, target) and policy.database == database, "pre_native_database_mismatch")


def rehearse_native(
    window: EffectWindow, expected: RuntimeIdentity, policy: LaunchPolicy, *,
    clock: Callable[[], int], custody: StopCustody,
    launch: Callable[[EffectWindow, Deployment, LaunchPolicy], Deployment],
    read_runtime: Callable[[Deployment], RuntimeReadback],
    read_stop: Callable[[Deployment], StopReadback],
) -> StopReadback:
    """With authenticated pins and trusted time, launch once and retain custody.
    Enforce the armed claim, cutoff, fence and policy at native start() itself.
    """
    require(type(window) is EffectWindow, "invalid_pre_native_window")
    target = window.target
    require(_runtime(expected, target), "invalid_pre_native_pins")
    _check_policy(policy, target, expected.database_writer)

    deployment = expected.deployment
    now = _check_time(window, clock)
    armed = _call(lambda: custody.arm(window, deployment), failure="pre_native_stop_unarmed")
    require(armed is True, "pre_native_stop_unarmed")
    try:
        now = _check_time(window, clock, now)
        reply = _call(launch, window, deployment, policy, failure="pre_native_launch_unconfirmed")
        require(_deployment(reply, target) and reply == deployment, "pre_native_launch_unconfirmed")
        observed = _call(read_runtime, deployment, failure="pre_native_runtime_unconfirmed")
        require(type(observed) is RuntimeReadback and _runtime(observed.identity, target)
                and observed.identity == expected, "pre_native_runtime_unconfirmed")
        _check_policy(observed.policy, target, expected.database_writer)
        require(observed.policy == policy, "pre_native_runtime_policy_mismatch")
    finally:
        stop_id = _call(lambda: custody.stop(deployment), failure="pre_native_stop_unconfirmed")
        require(type(stop_id) is UUID and stop_id.int, "pre_native_stop_unconfirmed")
        stop = _call(read_stop, deployment, failure="pre_native_stop_unconfirmed")
        require(type(stop) is StopReadback and _deployment(stop.deployment, target)
                and stop.deployment == deployment and type(stop.stop_id) is UUID and stop.stop_id == stop_id
                and type(stop.active_instances) is int
                and type(stop.assigned_instances) is int and stop.active_instances == stop.assigned_instances == 0
                and stop.startup_drained is True and stop.restart_fenced is True, "pre_native_stop_unconfirmed")
        _check_time(window, clock, now)
    return stop
