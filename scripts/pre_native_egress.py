"""Check archived PRE launch evidence through read-only synthetic trace adapters.

Adapters bind observations to the attempt after authenticating the reservation,
scenario and observed path. Correlated events share a strictly ordered clock.
"""

from dataclasses import dataclass
from ipaddress import ip_address
import re

from pre_target import DIGEST, Target, require

COLD = "cold_start"
KEEPALIVE = "forced_keepalive_restart"
FAULTS = ("missing_policy", "missing_listener")
VECTORS = frozenset(("dns", "direct_ipv4", "direct_ipv6", "https_wss"))
GATEWAY = "ws://127.0.0.1:8081"
API = "http://127.0.0.1:8081"
NAMESPACE = re.compile(r"[a-zA-Z0-9:_-]{1,128}")


@dataclass(frozen=True, repr=False)
class Endpoint:
    protocol: str
    address: str
    port: int


@dataclass(frozen=True, repr=False)
class Reservation:
    """The independent authority authenticates PostgreSQL socket ownership and network scope."""
    target: Target
    postgres: Endpoint


@dataclass(frozen=True)
class Attempt:
    target: Target
    path: str
    fault: str | None
    baseline_for: str | None = None  # Fault scenario primed by this successful cold start.


@dataclass(frozen=True, repr=False)
class Policy:
    namespace: str
    generation: int
    enforced_at: int
    default_deny: bool
    allowlist: tuple[Endpoint, ...]  # Complete effective external exceptions across all protocols.


@dataclass(frozen=True, repr=False)
class PolicyObservation:
    attempt: Attempt
    policy: Policy | None  # None requires affirmative absence at the boundary.


@dataclass(frozen=True, repr=False)
class Listener:
    namespace: str
    address: str
    ready_at: int  # Both gateway and API are ready on this socket.


@dataclass(frozen=True, repr=False)
class Process:
    namespace: str
    generation: int
    digest: str
    gateway_url: str  # Effective DISCORD_GATEWAY_URL.
    api_base: str  # Effective DISCORD_API_BASE.


@dataclass(frozen=True, repr=False)
class EgressObservation:
    namespace: str
    generation: int
    denied: frozenset[str]  # Affirmative synthetic denials. https_wss requires both protocols.
    postgres_allowed: Endpoint


@dataclass(frozen=True, repr=False)
class LaunchObservation:
    attempt: Attempt
    path: str  # Observed entry point, including the actual keepalive trigger.
    namespace: str  # Network namespace of the launch boundary, even on refusal.
    event: int  # Effective start or refusal at the launch boundary.
    forced_from: int | None  # Adapter-authenticated prior start stopped to trigger this trial.
    listener: Listener | None
    process: Process | None
    refusal: str | None
    process_starts: int  # Complete count, including on refusal.
    public_discord_packets: int  # Complete count, including on refusal.
    egress: EgressObservation | None = None


def _endpoint(endpoint):
    if (type(endpoint) is not Endpoint or type(endpoint.protocol) is not str
            or endpoint.protocol != "tcp" or type(endpoint.port) is not int
            or not 0 < endpoint.port <= 65535 or type(endpoint.address) is not str
            or len(endpoint.address) > 45 or "%" in endpoint.address):
        return False
    try:
        address = ip_address(endpoint.address)
    except ValueError:
        return False
    return (str(address) == endpoint.address and not address.is_unspecified and not address.is_link_local
            and not address.is_multicast and getattr(address, "ipv4_mapped", None) is None
            and endpoint.address != "255.255.255.255" and (endpoint.address, endpoint.port) != ("127.0.0.1", 8081))


def _positive(value):
    return type(value) is int and 0 < value < 2**64


def _namespace(value):
    return type(value) is str and NAMESPACE.fullmatch(value)


def _observe(adapter, attempt):
    try:
        return adapter(attempt)
    except Exception:
        return None


def _check_attempt(reservation, attempt, digest, observe_policy, probe, previous=None):
    policy_readback = _observe(observe_policy, attempt)
    require(type(policy_readback) is PolicyObservation and policy_readback.attempt is attempt,
            "pre_egress_policy_unconfirmed")
    launch = _observe(probe, attempt)
    require(type(launch) is LaunchObservation and launch.attempt is attempt
            and type(launch.path) is str and launch.path == attempt.path
            and _namespace(launch.namespace) and _positive(launch.event),
            "pre_egress_launch_unconfirmed")
    if previous is not None:
        event, generation, enforced_at = previous
        require(_positive(launch.forced_from) and launch.forced_from == event < launch.event,
                "pre_egress_keepalive_trigger_unconfirmed")
    else:
        require(launch.forced_from is None, "pre_egress_not_cold_start")
    require(type(launch.public_discord_packets) is int and launch.public_discord_packets == 0,
            "pre_egress_public_traffic_unconfirmed")
    policy, listener = policy_readback.policy, launch.listener
    if attempt.fault == "missing_policy":
        require(policy is None, "pre_egress_policy_not_missing")
    else:
        require(type(policy) is Policy and type(policy.namespace) is str and policy.namespace == launch.namespace
                and _positive(policy.generation) and _positive(policy.enforced_at)
                and policy.enforced_at < launch.event and policy.default_deny is True,
                "pre_egress_policy_not_enforced")
        require(type(policy.allowlist) is tuple and len(policy.allowlist) == 1
                and _endpoint(policy.allowlist[0]) and policy.allowlist[0] == reservation.postgres,
                "pre_egress_wrong_allowlist")
        if previous is not None:
            require(policy.generation > generation and policy.enforced_at > enforced_at,
                    "pre_egress_stale_keepalive_restart")
    if attempt.fault == "missing_listener":
        require(listener is None, "pre_egress_listener_not_missing")
    else:
        require(type(listener) is Listener and type(listener.namespace) is str and listener.namespace == launch.namespace
                and type(listener.address) is str and listener.address == "127.0.0.1:8081"
                and _positive(listener.ready_at) and listener.ready_at < launch.event,
                "pre_egress_listener_not_ready")

    if attempt.fault is not None:
        require(type(launch.refusal) is str and launch.refusal == attempt.fault
                and type(launch.process_starts) is int and launch.process_starts == 0
                and launch.process is None and launch.egress is None, "pre_egress_refusal_unconfirmed")
        return

    process, egress = launch.process, launch.egress
    require(type(process) is Process and launch.refusal is None
            and type(launch.process_starts) is int and launch.process_starts == 1,
            "pre_egress_process_unconfirmed")
    require(_namespace(process.namespace) and process.namespace == launch.namespace
            and _positive(process.generation) and process.generation == policy.generation,
            "pre_egress_process_policy_mismatch")
    require(type(process.gateway_url) is str and process.gateway_url == GATEWAY
            and type(process.api_base) is str and process.api_base == API,
            "pre_egress_wrong_process_settings")
    require(type(process.digest) is str and process.digest == digest, "pre_egress_wrong_digest")
    require(type(egress) is EgressObservation and _namespace(egress.namespace)
            and egress.namespace == process.namespace and _positive(egress.generation)
            and egress.generation == process.generation
            and type(egress.denied) is frozenset and len(egress.denied) == len(VECTORS)
            and all(type(vector) is str for vector in egress.denied) and egress.denied == VECTORS,
            "pre_egress_denial_unconfirmed")
    require(_endpoint(egress.postgres_allowed) and egress.postgres_allowed == reservation.postgres,
            "pre_egress_postgres_unconfirmed")
    return launch.event, policy.generation, policy.enforced_at


def verify_effective_launch_controls(first, retained, *, digest, observe_policy, probe):
    """Require success and two single-control fault trials on each launch path.

    Pass the independently authenticated reservations and verified image digest.
    The probe reads the complete trace and affirmative denials from the bot's
    effective policy, excluding outcomes inferred from errors or timeouts.
    """
    require(type(digest) is str and DIGEST.fullmatch(digest), "pre_egress_invalid_digest_pin")
    for reservation, shape in ((first, "0473/0465"), (retained, "0475/0465")):
        require(type(reservation) is Reservation and type(reservation.target) is Target
                and reservation.target.shape == shape and _endpoint(reservation.postgres),
                "pre_egress_invalid_reservation")
    require(first.target.provider_names != retained.target.provider_names and first.postgres != retained.postgres,
            "pre_egress_reservation_collision")
    for reservation in (first, retained):
        for fault in (None, *FAULTS):
            cold = Attempt(reservation.target, COLD, None, baseline_for=fault)
            previous = _check_attempt(reservation, cold, digest, observe_policy, probe)
            if fault is not None:
                _check_attempt(reservation, Attempt(reservation.target, COLD, fault), digest, observe_policy, probe)
            restart = Attempt(reservation.target, KEEPALIVE, fault)
            _check_attempt(reservation, restart, digest, observe_policy, probe, previous)
