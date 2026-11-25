"""Verify the atomic forward transaction before allowing a production takeover."""

from ci.production_migrate_claim import projection_hash, store_projection_hash
from production_recovery_takeover import HEX, RUN_ID
from staging_migrate_approval import digest_json
from staging_recovery_status import matches
from staging_rollout import mapping, require

CUTOVER = (475, 479, 480, 481, 482)
STORE = (478,)


def ledger_endpoints(plan):
    """Derive the three permissible ledger states from the approved source."""
    require(plan.get("pending_before") == list(CUTOVER)
            and plan.get("pending_store_before") == list(STORE)
            and plan.get("plan_manifest_sha256") == projection_hash(plan)
            and plan.get("store_plan_manifest_sha256") == store_projection_hash(plan),
            "production_release_plan_projection_invalid")
    expanded = [[dict(row, success=True) for row in plan[key]]
                for key in ("source_migrations", "source_store_migrations")]
    before = [[row for row in rows if row["version"] not in pending]
              for rows, pending in zip(expanded, (CUTOVER, STORE), strict=True)]
    restored = [[row for row in rows if row["version"] not in removed]
                for rows, removed in zip(expanded, (CUTOVER[1:], STORE), strict=True)]
    require([plan.get("ledger_before"), plan.get("ledger_store_before")] == before
            and [plan.get("ledger_after"), plan.get("ledger_store_after")] == before,
            "production_release_plan_ledgers_invalid")
    return before, expanded, restored


def forward_receipt(plan_id, plan, outcome):
    """Bind both ledger readbacks and the committed 0475 stamp to this plan."""
    before, expanded, restored = ledger_endpoints(plan)
    receipt = mapping(outcome)
    fence = receipt.get("attendance_fence_sha256")
    require(matches(RUN_ID, plan_id) and matches(HEX, fence)
            and type(receipt.get("runner_version")) is int and receipt["runner_version"] == 1
            and receipt.get("mode") == "apply" and receipt.get("migration_target") == "production"
            and receipt.get("role") == "two_bot_migrator" and receipt.get("plan_run_id") == plan_id
            and receipt.get("plan_provenance_verified") is True,
            "production_release_forward_unverified")
    require(all(receipt.get(key) == plan.get(key) for key in (
        "source_sha", "target", "plan_manifest_sha256", "store_plan_manifest_sha256",
        "recovery_evidence_ref", "acl_plan_ref", "pending_before", "pending_store_before",
    )), "production_release_forward_plan_changed")
    require([receipt.get("ledger_before"), receipt.get("ledger_store_before")] == before
            and [receipt.get("ledger_after"), receipt.get("ledger_store_after")] == expanded
            and type(receipt.get("applied_count")) is int and receipt["applied_count"] == len(CUTOVER)
            and type(receipt.get("applied_store_count")) is int and receipt["applied_store_count"] == len(STORE),
            "production_release_forward_ledgers_changed")
    return {"plan_run_id": plan_id, "source_sha": plan["source_sha"],
            "database_fingerprint": digest_json(plan["target"]),
            "expanded_ledgers_sha256": digest_json(expanded), "restored_ledgers_sha256": digest_json(restored),
            "attendance_fence_sha256": fence}
