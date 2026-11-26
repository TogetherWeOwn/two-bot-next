"""Compose the protected PRE calls with injected authorities and adapters."""

from collections.abc import Callable
from typing import Literal, Protocol

from build_identity import is_build_id_shape, is_build_revision_shape, is_deploy_id_shape
from pre_admission import AdmissionRecord, AdmissionStore, SettlementEvidence, StartupAdmission
from pre_registry_cleanup import (
    PublicationSettlement, RegistryDisposition, RegistryReadback, RegistryReservation, registry_cleanup_disposition,
)
from pre_start_stop import (
    Deployment, Endpoint, ExternalStop, Operation, PrivateRuntime, ProbeReply, ProtectedPre, WorkerUpload,
)
from pre_sweep import CleanupReadback, SweepError, sweep_target
from pre_target import Action, Inventory, Target, TargetError, require, verify_cleanup
from staging_rollout import GateError, decode, runtime_ready
from worker_container_rollback import gateway_state


class ProtectedPreCaller:
    """Use the full Target from the authenticated reservation on each runner.

    The host authenticates the protected run and grants only its operations.
    Pass the checked Worker upload and runtime stamps from the pinned build
    handoff. The admission store, runtime and external stop enforce any
    earlier enclosing lease cutoff. The runtime owns private provisioning
    and effect-time checks. A separately authorized
    runner can stop with only the original reservation and the same durable
    authority, including after a refusal or lost reply.
    """

    def __init__(
        self, target: Target, *, authorize: Callable[[Target, Operation], bool], store: AdmissionStore,
        observe_settlement: Callable[[AdmissionRecord], SettlementEvidence | None],
        runtime: PrivateRuntime, external_stop: ExternalStop,
    ):
        self._controller = ProtectedPre(
            target, authorize=authorize, admission=StartupAdmission(store, observe_settlement),
            runtime=runtime, external_stop=external_stop,
        )

    def start(self, upload: WorkerUpload) -> Deployment:
        return self._controller.start(upload)

    def read(self, deployment: Deployment, *, verified_receipt: dict) -> tuple[ProbeReply, ProbeReply]:
        require(type(verified_receipt) is dict, "pre_build_stamps_unconfirmed")
        revision = verified_receipt.get("BOT_BUILD_REVISION")
        build_id = verified_receipt.get("BOT_BUILD_ID")
        deploy_id = verified_receipt.get("BOT_DEPLOY_ID")
        require(is_build_revision_shape(revision)
                and revision == verified_receipt.get("source_sha") == verified_receipt.get("worker_message")
                and is_build_id_shape(build_id) and build_id.endswith("-1")
                and is_deploy_id_shape(deploy_id) and deploy_id.endswith(f".1+{revision[:7]}"),
                "pre_build_stamps_unconfirmed")
        health = self._controller.probe(deployment, Endpoint.HEALTH)
        require(health.status == 200, "pre_health_unconfirmed")
        readiness = self._controller.probe(deployment, Endpoint.READINESS)
        version = str(deployment.worker_version)
        try:
            ready = runtime_ready(readiness.status, {"x-two-worker-version": str(readiness.worker_version)}, readiness.body,
                                  version, revision, build_id, deploy_id)
            ready = ready and gateway_state(decode(readiness.body)) == "ready"
        except GateError:
            raise TargetError("pre_readiness_unconfirmed") from None
        require(ready, "pre_readiness_unconfirmed")
        return health, readiness

    def stop(self) -> bool:
        return self._controller.stop()


class FinalizerAdapter(Protocol):
    """Authenticate run status and exact-scope observations independently.

    Inventory uses durable startup settlement and preserves the exact app's
    zero-count proof or affirmative never-created evidence across reads.
    Cleanup inventory is observed afresh under the held cleanup claim.
    Registry methods resolve the original reservation and authenticate the
    publisher's settlement independently of registry presence.
    """

    def run_status(self, target: Target) -> str: ...
    def inventory(self, target: Target) -> Inventory: ...
    def cleanup_inventory(self, target: Target, *, request: object) -> CleanupReadback: ...
    def apply(self, target: Target, action: Action) -> bool: ...
    def stop_container(self, target: Target, action: Action) -> Inventory: ...
    def registry_reservation(self, target: Target) -> RegistryReservation: ...
    def publication_settlement(self, reservation: RegistryReservation) -> PublicationSettlement | None: ...
    def read_registry(self, reservation: RegistryReservation) -> RegistryReadback: ...


def finalize_pre(
    target: Target, *, authorize: Callable[[Target], bool], trusted_now: Callable[[], int],
    adapter: FinalizerAdapter,
) -> Literal["spared"] | RegistryDisposition:
    """Run independently of the qualification attempt and its stop caller.

    Only ALREADY_ABSENT confirms both the sweep and registry handback.
    """
    try:
        require(type(target) is Target and authorize(target) is True, "pre_finalizer_denied")
        now = trusted_now()
    except Exception:
        raise SweepError("PRE finalizer authorization could not be verified") from None
    if sweep_target(target, adapter, now=now) == "spared":
        return "spared"

    try:
        request = object()
        readback = adapter.cleanup_inventory(target, request=request)
        require(type(readback) is CleanupReadback and readback.request is request,
                "wrong_pre_cleanup_readback")
        inventory = readback.inventory
        verify_cleanup(target, inventory)
        reservation = adapter.registry_reservation(target)
        require(type(reservation) is RegistryReservation and reservation.target == target,
                "wrong_pre_registry_scope")
        settlement = adapter.publication_settlement(reservation)
        require(type(settlement) is PublicationSettlement, "pre_registry_publication_unsettled")
        require(type(settlement.reservation) is RegistryReservation and settlement.reservation == reservation,
                "wrong_pre_registry_publication_scope")
        status = adapter.run_status(target)
        disposition = registry_cleanup_disposition(
            reservation, inventory, adapter.read_registry(reservation),
            now=trusted_now(), run_status=status, publication_settlement=settlement,
        )
        require(disposition is not None, "pre_registry_cleanup_unconfirmed")
    except Exception:
        raise SweepError("PRE registry handback could not be verified") from None
    return disposition
