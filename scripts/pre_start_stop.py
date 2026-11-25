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
from typing import Protocol
from uuid import UUID

from pre_target import Target, TargetError, require
from staging_health_contract_probe import BODY_CAP


class Operation(Enum):
    START = "start"
    PROBE = "probe"
    STOP = "stop"


class Endpoint(Enum):
    HEALTH = "/health"
    READINESS = "/readyz"


class StartupAdmission(Protocol):
    """The durable admission and settlement methods used by this controller."""

    def admit_start(self, target: Target) -> UUID | None: ...
    def close(self, target: Target) -> bool: ...
    def settle(self, target: Target) -> bool: ...


class WorkerUpload(Protocol):
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


class PrivateRuntime(Protocol):
    """Enforce authorization, exact operation, deadline and stop at effect time.

    Return a deployment only after verifying the exact artifact and isolated
    app identity, private ingress from first create, current unrouted readback
    and an authenticated service binding restricted to this exact Worker.
    Revalidate the binding and exact deployment on each bounded GET without
    following redirects. Refuse effects outside the admission window, after a
    fence, or for unverified identities. Treat a lost reply as uncertain.
    """

    def create_private(self, target: Target, start_id: UUID, upload: WorkerUpload) -> Deployment: ...
    def probe(self, deployment: Deployment, endpoint: Endpoint) -> ProbeReply: ...


class ExternalStop(Protocol):
    """Independent of the calling workflow and the historical Worker.

    Arm before create and enforce the deadline in Target even after runner
    loss. Arm is bound to the admitted start ID and cannot reopen a fenced or
    expired reservation. The runtime must consult the same authority before
    any effect, including delayed requests. Fence persists even if create
    never replied or never began. It prevents new effects and initiates stop
    of the exact runtime, including internal keepalive. Repeating it is safe.
    The admission settlement observer verifies drain and no restart separately.
    """

    def arm(self, target: Target, start_id: UUID) -> bool: ...
    def fence(self, target: Target) -> bool: ...


def _call(operation, *args, failure):
    try:
        return operation(*args)
    except Exception:
        raise TargetError(failure) from None


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
        require(type(deployment) is Deployment and type(deployment.target) is Target
                and deployment.target == self._target
                and all(type(value) is UUID and value.int for value in
                        (deployment.start_id, deployment.worker_version, deployment.container_id)),
                "pre_deployment_unconfirmed")

    def start(self, upload: WorkerUpload) -> Deployment:
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
        return reply

    def stop(self) -> bool:
        """Fence effects, close admission and persist independent settlement."""
        self._allow(Operation.STOP)
        target = self._target
        try:
            fenced = _call(self._external_stop.fence, target, failure="pre_external_stop_unconfirmed")
        finally:
            closed = _call(self._admission.close, target, failure="pre_admission_close_unconfirmed")
        if fenced is not True or closed is not True:
            return False
        return _call(self._admission.settle, target, failure="pre_stop_unsettled") is True
