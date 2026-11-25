"""Join synthetic PRE evidence in memory. This fixture has no release caller."""

from dataclasses import dataclass

from build_identity import is_build_revision_shape, is_deploy_id_shape
from pre_registry_cleanup import (
    PublicationSettlement, RegistryDisposition, RegistryReadback, RegistryReservation, registry_cleanup_disposition,
)
from pre_target import Inventory, Target, TargetError
from rollback_proof import DIGEST, ProofError, VERSION, require
from staging_rollout import GateError, decode, runtime_ready
from worker_container_rollback import gateway_state


@dataclass(frozen=True)
class Build:
    run_id: int
    revision: str
    deploy_id: str
    image_digest: str

    @property
    def build_id(self):
        return f"{self.run_id}-1"


@dataclass(frozen=True)
class SchemaState:
    cutover: tuple[int, int]  # Ledger row count and final migration version.
    store: tuple[int, int]
    attendance_recovered_at: bool
    installed_0475_at: int | None

    def matches(self, expected):
        return (self == expected and type(self.attendance_recovered_at) is bool
                and type(self.installed_0475_at) is type(expected.installed_0475_at)
                and all(type(value) is int for value in (*self.cutover, *self.store)))


# Stub the reviewed plan and inverse endpoint projections. The timestamp is a
# synthetic pin for the original 0475 ledger row retained by the inverse.
PRE_STATES = {
    "0473/0465": SchemaState((115, 473), (20, 465), False, None),
    "0475/0465": SchemaState((116, 475), (20, 465), True, 1_800_000_001),
}


@dataclass(frozen=True)
class SchemaEvidence:
    reservation: Target
    state: SchemaState


@dataclass(frozen=True)
class ServingEvidence:
    reservation: Target
    worker_version: str
    worker_message: str
    image_digest: str


@dataclass(frozen=True)
class Probe:
    status: int
    worker_version: str | None
    body: bytes = b""


@dataclass(frozen=True)
class WorkerEvidence:
    reservation: Target
    health: Probe | None
    readyz: Probe | None


@dataclass(frozen=True)
class SweepEvidence:
    # Record the reservation passed to #2563's sweep_target with its outcome.
    # An exception or lost response has no outcome. Only 'cleared' signifies
    # completion of the Worker and Container checks.
    reservation: Target
    outcome: str | None


@dataclass(frozen=True)
class RegistryCleanupEvidence:
    inventory: Inventory
    readback: RegistryReadback
    now: int
    run_status: str
    publication_settlement: PublicationSettlement | None


@dataclass(frozen=True)
class AcceptedRow:
    reservation: Target
    build: Build
    schema: SchemaState
    worker_version: str
    cleanup: str


def accept(registry_reservation: RegistryReservation, build, *, receipt, schema, serving, worker, sweep, registry):
    """Compose a stubbed verified build receipt and scoped observations.

    The operator pins come from outside the observations. `receipt` stands in
    for #2558's verified #2556 build.json. The protected publication plan
    pins the registry reservation before the copy. Readback follows verified
    target cleanup and independent proof of publisher settlement.
    """
    reservation = registry_reservation.target
    require(type(build.run_id) is int and 0 < build.run_id < 10**20
            and is_build_revision_shape(build.revision) and is_deploy_id_shape(build.deploy_id)
            and build.deploy_id.endswith(f".1+{build.revision[:7]}")
            and isinstance(build.image_digest, str) and DIGEST.fullmatch(build.image_digest),
            "pre_build_pin_invalid")
    require(isinstance(receipt, dict) and receipt.get("source_sha") == receipt.get("worker_message")
            == receipt.get("BOT_BUILD_REVISION") == build.revision
            and receipt.get("BOT_BUILD_ID") == build.build_id
            and receipt.get("BOT_DEPLOY_ID") == build.deploy_id
            and receipt.get("image_manifest_digest") == build.image_digest,
            "pre_receipt_mismatch")

    require(schema is not None and serving is not None and worker is not None,
            "pre_endpoint_evidence_missing")
    require(all(evidence.reservation == reservation for evidence in (schema, serving, worker)),
            "pre_endpoint_reservation_mismatch")
    require(reservation.shape in PRE_STATES and schema.state.matches(PRE_STATES[reservation.shape]),
            "pre_schema_mismatch")
    require(isinstance(serving.worker_version, str) and VERSION.fullmatch(serving.worker_version)
            and serving.worker_message == build.revision
            and serving.image_digest == build.image_digest, "pre_serving_identity_mismatch")

    health, readyz = worker.health, worker.readyz
    require(health is not None and readyz is not None, "pre_endpoint_evidence_missing")
    require(health.status == 200 and health.worker_version == serving.worker_version,
            "pre_worker_health_unproved")
    try:
        ready = runtime_ready(readyz.status, {"x-two-worker-version": readyz.worker_version}, readyz.body,
                              serving.worker_version, build.revision, build.build_id, build.deploy_id)
        ready = ready and gateway_state(decode(readyz.body)) == "ready"
    except GateError:
        raise ProofError("pre_worker_readiness_unproved") from None
    require(ready, "pre_worker_readiness_unproved")

    require(sweep is not None, "pre_cleanup_unproved")
    require(sweep.reservation == reservation, "pre_cleanup_reservation_mismatch")
    require(sweep.outcome == "cleared", "pre_cleanup_unproved")
    require(registry is not None, "pre_registry_cleanup_unproved")
    require(registry_reservation.reference.partition("@")[2] == build.image_digest,
            "pre_registry_build_mismatch")
    try:
        disposition = registry_cleanup_disposition(
            registry_reservation, registry.inventory, registry.readback, now=registry.now,
            run_status=registry.run_status, publication_settlement=registry.publication_settlement,
        )
    except TargetError as error:
        raise ProofError(str(error)) from None
    require(disposition is RegistryDisposition.ALREADY_ABSENT, "pre_registry_cleanup_unproved")
    return AcceptedRow(reservation, build, schema.state, serving.worker_version, sweep.outcome)
