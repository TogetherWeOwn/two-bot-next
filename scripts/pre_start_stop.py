"""Dormant protected orchestration for one disposable predecessor reservation.

Cloudflare's script PUT and subdomain settings are separate operations, and a
first Durable Object class creation needs a deployment. A live adapter needs
an authenticated first-create path with public ingress already blocked, plus
an external deadline and a durable stop fence that controls pending creates
and legacy Container keepalive. The documented inactivity timeout does not
provide that fence. There is no concrete provider adapter or workflow caller.
"""

from collections.abc import Callable
from dataclasses import dataclass, field
from enum import Enum
from threading import Lock
from typing import Protocol
from uuid import UUID

from pre_target import (
    Inventory, ProtectedRouteReadback, Target, TargetError, confirm_protected_route, require, verify_cleanup,
)
from staging_health_contract_probe import BODY_CAP, TIMEOUT_SECONDS


class Operation(Enum):
    START = "start"
    PROBE = "probe"
    STOP = "stop"


class Endpoint(Enum):
    HEALTH = "/health"
    READINESS = "/readyz"


class StartupAdmission(Protocol):
    """Durable startup admission and settlement for the full Target.

    Admit once across all runners, with authority time in the half-open
    admission window at commit. Never return the ID again, including after a
    lost reply. Close permanently prevents admission and retains the start ID.
    Settle requires affirmative drain and restart-fence evidence for that ID.
    """

    def admit_start(self, target: Target) -> UUID | None: ...
    def close(self, target: Target) -> bool: ...
    def settle(self, target: Target) -> bool: ...


class WorkerUpload(Protocol):
    """The protected caller supplies the immutable, verified upload payload."""

    @property
    def target(self) -> Target: ...
    @property
    def content_type(self) -> str: ...
    @property
    def body(self) -> bytes: ...


@dataclass(frozen=True)
class Deployment:
    """Exact identities attested by the protected runtime adapter."""

    target: Target
    start_id: UUID
    worker_version: UUID
    container_id: UUID


@dataclass(frozen=True)
class ProbeReply:
    status: int
    worker_version: UUID
    body: bytes = field(repr=False)


@dataclass(frozen=True)
class ProbeBindingIdentity:
    worker_id: str
    deployment: Deployment


@dataclass(frozen=True)
class PrivateHttpReply:
    status: int
    worker_versions: tuple[str, ...] = field(repr=False)
    body: bytes = field(repr=False)


class PrivateProbeBinding(Protocol):
    """Authenticated handle to one private service, with no alternate destination.

    The adapter must check the current reservation, exact binding destination,
    authorization, deadline and stop fence at send time. The adapter must
    enforce the total timeout and cap the response read, without redirects or
    retries. Return every x-two-worker-version field value, regardless of
    header case.
    """

    @property
    def identity(self) -> ProbeBindingIdentity: ...

    def request(self, method: str, path: str, *, expected: ProbeBindingIdentity,
                follow_redirects: bool, timeout_seconds: int, max_body_bytes: int) -> PrivateHttpReply: ...


class PrivateRuntime(Protocol):
    """Enforce authorization, exact operation, deadline and stop at effect time.

    Return a deployment only after verifying the exact artifact and isolated
    app identity, private ingress from first create, current unrouted readback
    and an authenticated service binding restricted to this exact Worker.
    Resolve and compare the deployment against the authoritative identity for
    the full reservation on each bounded GET using the `probe_private_get` contract.
    Controller instances can span different runners, so retain that identity
    outside the controller. Refuse effects outside the admission window,
    after a fence, or for unverified identities. Treat a lost reply as uncertain.
    """

    def create_private(self, target: Target, start_id: UUID, upload: WorkerUpload) -> Deployment: ...
    def probe(self, deployment: Deployment, endpoint: Endpoint) -> ProbeReply: ...


class ExternalStop(Protocol):
    """Independent of the calling workflow and the historical Worker.

    Arm before create and enforce the deadline in Target even after runner
    loss. Arm is bound to the admitted start ID and cannot reopen a fenced or
    expired reservation. Arm must atomically reserve the exact provider names
    in their account and refuse a collision with any other reservation. Two
    full Targets can have the same names. Key ownership and stop by the full
    Target. The runtime checks ownership and the fence at the effect boundary,
    including for delayed requests. Fence persists even if create never
    replied or never began. It prevents new effects and initiates stop of the
    exact runtime, including internal keepalive, only for the claim holder.
    A conflicting reservation cannot fence or release those names.
    The admission settlement observer verifies drain and no restart separately.
    """

    def arm(self, target: Target, start_id: UUID) -> bool: ...
    def fence(self, target: Target) -> bool: ...

    def confirm_probe_active(self, target: Target, start_id: UUID) -> bool:
        """Confirm acceptance against the current fence and trusted authority time.

        Use one authoritative observation without network I/O. Return True only
        for the exact armed Target and start ID while unfenced, at or after
        target.started_at and before both target.expires_at and any earlier
        enclosing cutoff. Refuse if any of that state is unknown or stale.
        """
        ...


class OfflineExternalStop:
    """In-memory claim and fence model for one fixed provider account.

    Share one instance across the synthetic callers for that account. The
    clock supplies trusted, nondecreasing time. ProtectedPre supplies the
    consumed start ID. A live adapter needs durable, account-wide state and
    must enforce the fence at the actual provider effect boundary.
    """

    def __init__(self, clock: Callable[[], int]):
        self._clock = clock
        self._lock = Lock()
        self._claims: dict[tuple[str, str], tuple[Target, UUID]] = {}
        self._fenced: set[Target] = set()

    def _in_window(self, target: Target) -> bool:
        now = self._clock()
        return type(now) is int and target.started_at <= now < target.expires_at

    def arm(self, target: Target, start_id: UUID) -> bool:
        if type(target) is not Target or type(start_id) is not UUID or not start_id.int:
            return False
        with self._lock:
            if target in self._fenced or not self._in_window(target):
                return False
            names = target.provider_names
            owner = self._claims.get(names)
            if owner is not None:
                if owner[0] != target:
                    self._fenced.add(target)
                return False
            self._claims[names] = (target, start_id)
            return True

    def fence(self, target: Target) -> bool:
        if type(target) is not Target:
            return False
        with self._lock:
            self._fenced.add(target)
            owner = self._claims.get(target.provider_names)
            return owner is not None and owner[0] == target

    def allows_effect(self, target: Target, start_id: UUID) -> bool:
        """Model the check at a synchronous synthetic create or probe."""
        if type(target) is not Target or type(start_id) is not UUID:
            return False
        with self._lock:
            return (self._claims.get(target.provider_names) == (target, start_id)
                    and target not in self._fenced and self._in_window(target))

    def release(self, target: Target, start_id: UUID, *, inventory: Inventory) -> bool:
        """Release only the fenced claim after independently verified cleanup.

        The caller supplies fresh, authenticated inventory for the claim.
        Keep the full-Target fence after releasing the account names.
        """
        if type(target) is not Target or type(start_id) is not UUID:
            return False
        with self._lock:
            names = target.provider_names
            if self._claims.get(names) != (target, start_id) or target not in self._fenced:
                return False
            verify_cleanup(target, inventory)
            del self._claims[names]
            return True


def _call(operation, *args, failure):
    try:
        return operation(*args)
    except Exception:
        pass
    raise TargetError(failure) from None


def _valid_deployment(deployment):
    return (type(deployment) is Deployment and type(deployment.target) is Target
            and all(type(value) is UUID and value.int for value in
                    (deployment.start_id, deployment.worker_version, deployment.container_id)))


def probe_private_get(deployment: Deployment, endpoint: Endpoint, *, pinned_deployment: Deployment,
                      pinned_route: ProtectedRouteReadback, binding: PrivateProbeBinding) -> ProbeReply:
    """Verify the outbound probe against independent protected pins.

    The caller supplies the authoritative deployment and pinned route from the
    reservation independently of the binding. This helper has no provider client.
    """
    require(type(endpoint) is Endpoint, "invalid_pre_probe")
    require(_valid_deployment(pinned_deployment) and _valid_deployment(deployment)
            and deployment == pinned_deployment, "pre_deployment_unconfirmed")
    confirm_protected_route(deployment.target, pinned_app_id=str(pinned_deployment.container_id),
                            readback=pinned_route)
    expected = ProbeBindingIdentity(pinned_route.worker_id, pinned_deployment)
    identity = _call(lambda: binding.identity, failure="pre_probe_binding_unconfirmed")
    require(type(identity) is ProbeBindingIdentity and type(identity.worker_id) is str
            and _valid_deployment(identity.deployment) and identity == expected, "pre_probe_binding_unconfirmed")

    reply = _call(lambda: binding.request("GET", endpoint.value, expected=expected, follow_redirects=False,
                                          timeout_seconds=TIMEOUT_SECONDS, max_body_bytes=BODY_CAP + 1),
                  failure="pre_probe_unconfirmed")
    require(type(reply) is PrivateHttpReply and type(reply.status) is int
            and 200 <= reply.status <= 599, "pre_probe_unconfirmed")
    require(not 300 <= reply.status <= 399, "pre_probe_redirect")
    stamp = reply.worker_versions
    require(type(stamp) is tuple and len(stamp) == 1 and type(stamp[0]) is str
            and stamp[0] == str(deployment.worker_version), "pre_probe_version_unconfirmed")
    require(type(reply.body) is bytes and len(reply.body) <= BODY_CAP, "pre_probe_unconfirmed")
    return ProbeReply(reply.status, deployment.worker_version, reply.body)


class ProtectedPre:
    def __init__(self, target: Target, *, authorize: Callable[[Target, Operation], bool],
                 admission: StartupAdmission, runtime: PrivateRuntime, external_stop: ExternalStop):
        require(type(target) is Target, "invalid_pre_target")
        self._target = target
        self._authorize = authorize
        self._admission = admission
        self._runtime = runtime
        self._external_stop = external_stop

    def _allow(self, operation):
        allowed = _call(self._authorize, self._target, operation, failure="pre_operation_denied")
        require(allowed is True, "pre_operation_denied")

    def _require_deployment(self, deployment):
        require(_valid_deployment(deployment) and deployment.target == self._target,
                "pre_deployment_unconfirmed")

    def start(self, upload: WorkerUpload) -> Deployment:
        """Start once and reconcile an uncertain outcome through the stop path.

        The protected operator invokes stop after a failure or abandonment
        past admission. An independent, authorized stop caller owns close and
        settlement if the start runner disappears or loses authorization.
        The already armed deadline applies even if that caller is unavailable.
        """
        self._allow(Operation.START)
        target = self._target
        upload_target = getattr(upload, "target", None)
        require(type(upload_target) is Target and upload_target == target, "wrong_pre_upload_target")
        start_id = _call(self._admission.admit_start, target, failure="pre_start_unconfirmed")
        require(type(start_id) is UUID and start_id.int, "pre_start_not_admitted")
        armed = _call(self._external_stop.arm, target, start_id, failure="pre_external_stop_unarmed")
        require(armed is True, "pre_external_stop_unarmed")
        deployment = _call(self._runtime.create_private, target, start_id, upload, failure="pre_create_unconfirmed")
        self._require_deployment(deployment)
        require(deployment.start_id == start_id, "pre_deployment_unconfirmed")
        return deployment

    def probe(self, deployment: Deployment, endpoint: Endpoint) -> ProbeReply:
        self._allow(Operation.PROBE)
        self._require_deployment(deployment)
        require(type(endpoint) is Endpoint, "invalid_pre_probe")
        reply = _call(self._runtime.probe, deployment, endpoint, failure="pre_probe_unconfirmed")
        require(type(reply) is ProbeReply and type(reply.status) is int and 200 <= reply.status <= 599
                and type(reply.worker_version) is UUID and reply.worker_version == deployment.worker_version
                and type(reply.body) is bytes and len(reply.body) <= BODY_CAP, "pre_probe_unconfirmed")
        active = _call(lambda: self._external_stop.confirm_probe_active(self._target, deployment.start_id),
                       failure="pre_probe_unconfirmed")
        require(active is True, "pre_probe_unconfirmed")
        return reply

    def stop(self) -> bool:
        """Fence effects, close admission and persist independent settlement."""
        self._allow(Operation.STOP)
        target = self._target
        fence_confirmed = False
        try:
            fence_confirmed = _call(self._external_stop.fence, target, failure="pre_external_stop_unconfirmed") is True
        finally:
            close_failure = ("pre_admission_close_unconfirmed" if fence_confirmed
                             else "pre_external_stop_and_close_unconfirmed")
            closed = _call(self._admission.close, target, failure=close_failure)
        if not fence_confirmed or closed is not True:
            return False
        return _call(self._admission.settle, target, failure="pre_stop_unsettled") is True
