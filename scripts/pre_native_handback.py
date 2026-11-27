"""Offline handback verification for one authenticated native PRE reservation."""

from dataclasses import asdict, dataclass
from hashlib import sha256
import json
from uuid import UUID

from pre_admission import AdmissionRecord, SettlementEvidence, SettlementSource
from pre_cleanup_run import CleanupRunStatus, classify_cleanup_run
from pre_target import DIGEST, Target, require


@dataclass(frozen=True)
class PredecessorPin:
    """Serving identity pinned before the rehearsal by the protected caller."""

    worker_version: UUID
    image_digest: str


@dataclass(frozen=True)
class PredecessorReadback:
    """Fresh post-handback serving identity read independently of the pin."""

    worker_version: UUID
    image_digest: str
    independent: bool


@dataclass(frozen=True)
class HandbackEvidence:
    """Affirmative observations bound to this exact reservation.

    The adapter confirms PRODUCTION_AUTO_PROMOTE_DISABLED=true and directly
    verifies zero active bindings and containers for the exact disposable
    actor after startup settlement. An empty listing can lag Cloudflare state
    and cannot attest drain.
    """

    target: Target
    predecessor: PredecessorReadback
    promotion_disabled: bool
    exact_actor_drained: bool
    independent_readback: bool


@dataclass(frozen=True)
class HandbackReceipt:
    target: Target
    initial_version: UUID
    predecessor: PredecessorPin
    start_id: UUID | None
    settlement_source: SettlementSource
    cleanup_workflow_id: int
    cleanup_head_sha: str
    handback_digest: str


def _is_target(value):
    return type(value) is Target and type(value.scope) is str and type(value.shape) is str


def _valid_uuid(value):
    return type(value) is UUID and value.int != 0


def _valid_predecessor(value):
    return (_valid_uuid(value.worker_version) and type(value.image_digest) is str
            and DIGEST.fullmatch(value.image_digest) and value.image_digest != "sha256:" + "0" * 64)


def _closed_for(record, target):
    return (type(record) is AdmissionRecord and _is_target(record.target) and record.target == target
            and record.closed is True and (record.start_id is None or _valid_uuid(record.start_id))
            and type(record.settled_by) is SettlementSource)


def _plain_cleanup_run(run):
    # These fields reach equality checks in the shared classifier. Reject
    # Python objects that can override those checks at the evidence boundary.
    return (type(run) is dict and all(type(key) is str for key in run)
            and all(type(run.get(key)) is str for key in ("path", "head_sha", "head_branch", "event", "status"))
            and (run.get("conclusion") is None or type(run["conclusion"]) is str)
            and all(type(repo) is dict and all(type(key) is str for key in repo)
                    and type(repo.get("full_name")) is str
                    for repo in (run.get("repository"), run.get("head_repository"))))


def verify_native_pre_handback(
    target: Target, *, record: AdmissionRecord, settlement: SettlementEvidence,
    cleanup_run: object, expected_workflow_id: int, expected_head_sha: str,
    initial_version: UUID, predecessor_pin: PredecessorPin, evidence: HandbackEvidence,
) -> HandbackReceipt:
    """Verify the supplied facts or raise TargetError with a refusal code.

    The caller supplies the current durable record, independent settlement,
    strictly decoded fresh run detail and provider observations. Take workflow
    pins and predecessor identity from protected context and the disposable
    version from verified first creation. The receipt commits to those claims
    and grants no operational authority.
    """
    require(_is_target(target), "invalid_pre_native_handback_target")
    require(_closed_for(record, target), "pre_native_handback_admission_unconfirmed")
    require(type(settlement) is SettlementEvidence and _closed_for(settlement.closed, target)
            and settlement.closed == record, "pre_native_handback_settlement_mismatch")
    require(type(settlement.source) is SettlementSource and settlement.source is record.settled_by,
            "pre_native_handback_settlement_source_mismatch")
    require(settlement.startup_drained is True, "pre_native_handback_startup_not_drained")
    require(settlement.restart_fenced is True, "pre_native_handback_restart_not_fenced")

    require(type(expected_head_sha) is str and _plain_cleanup_run(cleanup_run),
            "pre_native_handback_cleanup_unresolved")
    cleanup = classify_cleanup_run(target, cleanup_run, expected_workflow_id=expected_workflow_id,
                                   expected_head_sha=expected_head_sha)
    require(cleanup is CleanupRunStatus.TERMINAL_SUCCESS, f"pre_native_handback_cleanup_{cleanup.value}")

    require(_valid_uuid(initial_version), "invalid_pre_native_handback_initial_version")
    require(type(predecessor_pin) is PredecessorPin and _valid_predecessor(predecessor_pin),
            "invalid_pre_native_handback_predecessor_pin")
    require(predecessor_pin.worker_version != initial_version, "pre_native_handback_predecessor_is_disposable")
    require(type(evidence) is HandbackEvidence and _is_target(evidence.target) and evidence.target == target,
            "wrong_pre_native_handback_evidence_target")
    readback = evidence.predecessor
    require(type(readback) is PredecessorReadback and _valid_predecessor(readback)
            and readback.independent is True, "pre_native_handback_predecessor_unconfirmed")
    require(readback.worker_version == predecessor_pin.worker_version
            and readback.image_digest == predecessor_pin.image_digest,
            "pre_native_handback_predecessor_changed")
    require(evidence.promotion_disabled is True, "pre_native_handback_promotion_not_disabled")
    require(evidence.exact_actor_drained is True, "pre_native_handback_actor_not_drained")
    require(evidence.independent_readback is True, "pre_native_handback_actor_readback_not_independent")

    # Hash claims with a versioned domain, excluding incidental API metadata.
    claims = {
        "schema": "native-pre-handback/v1",
        "target": asdict(target),
        "initial_version": str(initial_version),
        "predecessor": {
            "worker_version": str(predecessor_pin.worker_version), "image_digest": predecessor_pin.image_digest,
        },
        "start_id": None if record.start_id is None else str(record.start_id),
        "settlement_source": settlement.source.value,
        "cleanup_workflow_id": expected_workflow_id,
        "cleanup_head_sha": expected_head_sha,
    }
    canonical = json.dumps(claims, sort_keys=True, separators=(",", ":")).encode("ascii")
    return HandbackReceipt(target, initial_version, predecessor_pin, record.start_id, settlement.source,
                           expected_workflow_id, expected_head_sha, sha256(canonical).hexdigest())
