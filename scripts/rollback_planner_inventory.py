"""Offline acceptance contract for the two attendance rollback inventories.

The protected caller supplies independent pins and authenticates workflow,
attempt, job and artifact lineage before calling. The planner catalog SHA is
separate from the protected producer SHA and the serving pair's build SHA.
"""

from dataclasses import asdict, dataclass
import re

from ci.staging_migrate_claim import Refused, projection_hash, store_projection_hash
from production_migrate_approval import ARTIFACT as PRODUCTION_PLAN_ARTIFACT
from production_recovery_takeover import verify_inverse
from staging_migrate_approval import digest_json
from staging_recovery_status import DIGEST, HEX64, PLAN_ARTIFACT, RUN_ID, SHA, matches, plan_manifest
from staging_rollout import GateError, mapping, number, require, sequence


@dataclass(frozen=True)
class Catalog:
    source_sha: str
    cutover_version: int
    cutover_count: int
    cutover_sha256: str
    store_sha256: str


# Existing plan projections over each historical source SHA and its complete
# ordered SQLx catalog, with an empty pending list for each ledger.
CATALOGS = {
    "original-pre": Catalog(
        "d8b6c1503fd72a7533ea96979f8c2ddc794e6737", 473, 115,
        "425e796ccaa49001386c03e87fbab434ab2355eeb90bfb08c951c2de45a18b43",
        "80c213cd48e6da604a0f530430591e6ddcd73cf3878c03c1aaa229a4eba85254",
    ),
    "retained-pre": Catalog(
        "058141c29cf135304b060d8fd89aa6e21e44a0c6", 475, 116,
        "13e6dd45635b089790300733647a6fdb2eea587be3564b027c78f2106f1d44f9",
        "b72cbabfbe78bb568556acc1a5c938874d12ddace08a54d1ef300aab36c24dce",
    ),
}
WINDOW = re.compile(r"[A-Za-z0-9_-]{1,64}")
STORE_VERSION, STORE_COUNT = 465, 20
PLAN_ARTIFACTS = {"staging": PLAN_ARTIFACT, "production": PRODUCTION_PLAN_ARTIFACT}


@dataclass(frozen=True)
class PlanArtifact:
    workflow_sha: str
    run_id: str
    run_attempt: int
    artifact_id: str
    archive_digest: str


def _installed_rows(plan, ledger, source, count, version):
    catalog = plan[source]
    require(len(catalog) == count and catalog[-1]["version"] == version
            and all(row.get("sha384_recomputed") == row["sha384"] for row in catalog),
            "rollback_catalog_mismatch")
    keys = ("version", "description", "sha384")
    expected = [{**{key: row[key] for key in keys}, "success": True} for row in catalog]
    for when in ("before", "after"):
        rows = sequence(plan.get(f"{ledger}_{when}"))
        require(rows == expected and all(type(row["version"]) is int and row["success"] is True for row in rows),
                "rollback_ledger_unqualified")
    return expected


def verify_inventory(endpoint, archive, artifact, *, expected_artifact, window_id,
                     migration_target, target_fingerprint, inverse_pin=None, inverse=None):
    """Verify one exact plan ZIP against independently pinned run and target evidence.

    The v1 plan's recovery_evidence_ref carries the qualification window ID.
    For retained PRE, the caller supplies the window's protected RecoveryPin
    (including the original 0475 timestamp digest) and authenticated inverse.
    """
    require(isinstance(endpoint, str) and endpoint in CATALOGS, "rollback_endpoint_invalid")
    catalog = CATALOGS[endpoint]
    for pin in (artifact, expected_artifact):
        require(type(pin) is PlanArtifact and matches(SHA, pin.workflow_sha) and matches(RUN_ID, pin.run_id)
                and type(pin.run_attempt) is int and pin.run_attempt == 1 and matches(RUN_ID, pin.artifact_id)
                and matches(DIGEST, pin.archive_digest), "rollback_artifact_invalid")
    require(artifact == expected_artifact, "rollback_artifact_mismatch")
    require(isinstance(archive, bytes), "invalid_plan_archive")
    require(isinstance(migration_target, str) and migration_target in PLAN_ARTIFACTS,
            "rollback_migration_target_invalid")
    plan = plan_manifest(archive, expected_artifact.archive_digest, PLAN_ARTIFACTS[migration_target])
    require(matches(WINDOW, window_id) and plan.get("recovery_evidence_ref") == window_id,
            "rollback_window_mismatch")
    require(plan.get("source_sha") == catalog.source_sha, "rollback_catalog_source_mismatch")
    require(plan.get("migration_target") == migration_target
            and type(plan.get("runner_version")) is int and plan["runner_version"] == 1
            and plan.get("mode") == "plan" and plan.get("role") == "two_bot_migrator_ro"
            and number(plan.get("role_verified_connections")) > 0
            and number(plan.get("applied_count")) == number(plan.get("applied_store_count")) == 0
            and plan.get("plan_provenance_verified") is False and plan.get("plan_run_id") == ""
            and mapping(plan.get("audit")).get("verify_findings") == [], "invalid_read_only_plan")
    target = mapping(plan.get("target"))
    require(set(target) == {"host", "database", "branch_id"}
            and all(isinstance(value, str) for value in target.values())
            and matches(HEX64, target_fingerprint) and digest_json(target) == target_fingerprint,
            "rollback_target_mismatch")
    require(plan.get("pending_before") == plan.get("pending_store_before") == [], "rollback_pending_migrations")
    try:
        cutover_hash, store_hash = projection_hash(plan), store_projection_hash(plan)
    except (Refused, UnicodeError):
        raise GateError("rollback_catalog_invalid") from None
    require(cutover_hash == plan.get("plan_manifest_sha256") == catalog.cutover_sha256
            and store_hash == plan.get("store_plan_manifest_sha256") == catalog.store_sha256,
            "rollback_catalog_mismatch")
    cutover = _installed_rows(plan, "ledger", "source_migrations", catalog.cutover_count, catalog.cutover_version)
    store = _installed_rows(plan, "ledger_store", "source_store_migrations", STORE_COUNT, STORE_VERSION)
    ledgers_sha256 = digest_json([cutover, store])
    proof = {**asdict(artifact), "endpoint": endpoint, "window_id": window_id,
             "migration_target": migration_target, "target_fingerprint": target_fingerprint,
             "ledgers_sha256": ledgers_sha256, "store_version": STORE_VERSION, "store_count": STORE_COUNT,
             **asdict(catalog)}
    if endpoint == "retained-pre":
        verify_inverse(inverse_pin, inverse)
        require(inverse_pin.database_fingerprint == target_fingerprint
                and inverse_pin.restored_ledgers_sha256 == ledgers_sha256, "rollback_inverse_inventory_mismatch")
        proof.update(inverse_run_id=inverse_pin.run_id, attendance_fence_sha256=inverse_pin.attendance_fence_sha256)
    else:
        require(inverse_pin is None and inverse is None, "rollback_unexpected_inverse")
    return proof
