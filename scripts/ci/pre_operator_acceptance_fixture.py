"""Join synthetic PRE evidence in memory. This fixture has no release caller."""

from dataclasses import dataclass

from rollback_proof import ProofError, VERSION, require
from staging_rollout import GateError, decode, runtime_ready
from worker_container_rollback import gateway_state


@dataclass(frozen=True)
class Reservation:
    # The immutable Target fields published in #2564. The cutoff is derived
    # from started_at, and this run ID belongs to staging, not to the build.
    scope: str
    run_id: int
    started_at: int
    shape: str


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


# Stub the reviewed plan and inverse endpoint projections. The timestamp is a
# synthetic pin for the original 0475 ledger row retained by the inverse.
PRE_STATES = {
    "0473/0465": SchemaState((115, 473), (20, 465), False, None),
    "0475/0465": SchemaState((116, 475), (20, 465), True, 1_800_000_001),
}


@dataclass(frozen=True)
class SchemaEvidence:
    reservation: Reservation
    state: SchemaState


@dataclass(frozen=True)
class ServingEvidence:
    reservation: Reservation
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
    reservation: Reservation
    health: Probe | None
    readyz: Probe | None


@dataclass(frozen=True)
class SweepEvidence:
    # Record the reservation passed to #2563's sweep_target with its outcome.
    # An exception or lost response has no outcome. Only 'cleared' signifies
    # completion of the independent stop and final readback checks.
    reservation: Reservation
    outcome: str | None


@dataclass(frozen=True)
class AcceptedRow:
    reservation: Reservation
    build: Build
    schema: SchemaState
    worker_version: str
    cleanup: str


def accept(reservation, build, *, receipt, schema, serving, worker, sweep):
    """Compose a stubbed verified build receipt and scoped observations.

    The operator pins come from outside the observations. `receipt` stands in
    for #2558's verified #2556 build.json. Schema, serving, probes and sweep
    stand in for evidence captured against each independently observed target.
    """
    require(receipt.get("source_sha") == receipt.get("worker_message")
            == receipt.get("BOT_BUILD_REVISION") == build.revision
            and receipt.get("BOT_BUILD_ID") == build.build_id
            and receipt.get("BOT_DEPLOY_ID") == build.deploy_id
            and receipt.get("image_manifest_digest") == build.image_digest,
            "pre_receipt_mismatch")

    require(schema is not None and serving is not None and worker is not None,
            "pre_endpoint_evidence_missing")
    require(all(evidence.reservation == reservation for evidence in (schema, serving, worker)),
            "pre_endpoint_reservation_mismatch")
    require(reservation.shape in PRE_STATES and schema.state == PRE_STATES[reservation.shape],
            "pre_schema_mismatch")
    require(bool(VERSION.fullmatch(serving.worker_version)) and serving.worker_message == build.revision
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
    return AcceptedRow(reservation, build, schema.state, serving.worker_version, sweep.outcome)
