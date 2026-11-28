"""Verify the atomic forward transaction before allowing a production takeover."""

from production_recovery_takeover import HEX, RUN_ID
from production_release_ledgers import STORE, ledger_endpoints
from staging_migrate_approval import digest_json
from staging_recovery_status import matches
from staging_rollout import mapping, require


def forward_receipt(plan_id, plan, outcome):
    """Bind both ledger readbacks and the committed 0475 stamp to this plan."""
    before, expanded, restored = ledger_endpoints(plan)
    receipt = mapping(outcome)
    fence = receipt.get("attendance_fence_sha256")
    require(matches(RUN_ID, plan_id) and matches(HEX, fence)
            and fence == plan.get("attendance_fence_sha256", fence)
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
            and type(receipt.get("applied_count")) is int and receipt["applied_count"] == len(plan["pending_before"])
            and type(receipt.get("applied_store_count")) is int and receipt["applied_store_count"] == len(STORE),
            "production_release_forward_ledgers_changed")
    return {"plan_run_id": plan_id, "source_sha": plan["source_sha"],
            "database_fingerprint": digest_json(plan["target"]),
            "expanded_ledgers_sha256": digest_json(expanded), "restored_ledgers_sha256": digest_json(restored),
            "attendance_fence_sha256": fence}
