"""Project the protected release plan into the Rust runner's ledger row shape."""

from ci.production_migrate_claim import SHA256, projection_hash, store_projection_hash
from staging_rollout import mapping, require, sequence

CUTOVER = (475, 479, 480, 481, 482)
RETAINED_CUTOVER = CUTOVER[1:]
STORE = (478,)
LEDGER_FIELDS = {"version", "description", "sha384", "success"}


def _project(source, before, after, pending):
    migrations = {row["version"]: row for row in source}
    require(all(row.get("sha384_recomputed") == row["sha384"] for row in source),
            "production_release_plan_checksum_invalid")
    installed = [mapping(row) for row in sequence(before)]
    require(all(set(row) == LEDGER_FIELDS and type(row["version"]) is int
                and isinstance(row["description"], str) and row["success"] is True for row in installed)
            and [row["version"] for row in installed] == [version for version in migrations if version not in pending],
            "production_release_plan_ledgers_invalid")
    require(all(row["sha384"] == migrations[row["version"]]["sha384"] for row in installed)
            and sequence(after) == installed, "production_release_plan_ledgers_invalid")
    by_version = {row["version"]: row for row in installed}
    for version in pending:
        source_row = migrations[version]
        by_version[version] = {"version": version, "description": source_row["description"],
                               "sha384": source_row["sha384"], "success": True}
    return installed, [by_version[version] for version in migrations]


def ledger_endpoints(plan):
    """Return the reviewed baseline, expanded schema, and restored schema ledgers."""
    pending = plan.get("pending_before")
    fence = plan.get("attendance_fence_sha256")
    require((pending == list(CUTOVER) and "attendance_fence_sha256" not in plan
             or pending == list(RETAINED_CUTOVER) and isinstance(fence, str) and SHA256.fullmatch(fence))
            and plan.get("pending_store_before") == list(STORE)
            and plan.get("plan_manifest_sha256") == projection_hash(plan)
            and plan.get("store_plan_manifest_sha256") == store_projection_hash(plan),
            "production_release_plan_projection_invalid")
    cutover, expanded_cutover = _project(plan["source_migrations"], plan.get("ledger_before"),
                                         plan.get("ledger_after"), pending)
    if pending == list(RETAINED_CUTOVER):
        description = next((row["description"] for row in plan["source_migrations"] if row["version"] == 475), None)
        require(any(row["version"] == 475 and row["description"] == description for row in cutover),
                "production_release_plan_ledgers_invalid")
    store, expanded_store = _project(plan["source_store_migrations"], plan.get("ledger_store_before"),
                                     plan.get("ledger_store_after"), STORE)
    restored_cutover = [row for row in expanded_cutover if row["version"] not in RETAINED_CUTOVER]
    return [cutover, store], [expanded_cutover, expanded_store], [restored_cutover, store]
