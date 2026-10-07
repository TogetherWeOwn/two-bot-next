#![cfg(feature = "db")]

use serde_json::Value;
use two_bot_core::{ChannelClaim, ChannelClaimTicket, ChannelModerationStore, LockdownSeed};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "111111111111111111";
const CHANNEL: &str = "222222222222222222";
const OPERATOR: &str = "333333333333333333";
const TIME: &str = "2026-10-04T00:00:00.000Z";
const ORIGINAL_HASH: &str = "actor-derived-test-hash";

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("test bootstrap required");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .unwrap()
}

async fn claim(store: &ChannelModerationStore, key: &str) -> ChannelClaimTicket {
    let ChannelClaim::Claimed { ticket } = store
        .claim(GUILD, key, "moderation.lockdown", "hash", TIME)
        .await
        .unwrap()
    else {
        panic!("expected a new claim");
    };
    assert!(store.claim_channel(&ticket, CHANNEL).await.unwrap());
    ticket
}

#[tokio::test]
async fn release_audits_previous_state_preserves_seed_and_prevents_old_key_replay() {
    let db = database().await;
    let store = ChannelModerationStore::from_pool(db.pool().clone());
    let ChannelClaim::Claimed { ticket } = store
        .claim(GUILD, "wedged", "moderation.lockdown", ORIGINAL_HASH, TIME)
        .await
        .unwrap()
    else {
        panic!("expected a new claim");
    };
    assert!(store.claim_channel(&ticket, CHANNEL).await.unwrap());
    let seed = store
        .record_lockdown(
            CHANNEL,
            GUILD,
            &LockdownSeed {
                prior_allow: "3072".to_owned(),
                prior_deny: "8192".to_owned(),
                prior_exists: true,
            },
            "reconcile",
            TIME,
        )
        .await
        .unwrap();
    let before = store
        .inspect_channel_lane(GUILD, CHANNEL, "wedged")
        .await
        .unwrap()
        .unwrap();
    let report = before.report();
    assert_eq!(report["moderation_idempotency"]["state"], "in_flight");
    assert_eq!(
        report["moderation_idempotency"]["request_hash"],
        "[REDACTED]"
    );
    assert_eq!(
        report["moderation_channel_executions"]["claim_token"],
        "[REDACTED]"
    );
    assert_eq!(
        report["moderation_idempotency"]["claim_token"],
        "[REDACTED]"
    );
    let raw_token: String = sqlx::query_scalar(
        "SELECT claim_token FROM moderation_idempotency WHERE idempotency_key = 'wedged'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert!(!report.to_string().contains(&raw_token));
    let debug_inspection = format!("{before:?}");
    assert!(!debug_inspection.contains(&raw_token));
    assert!(!debug_inspection.contains(ORIGINAL_HASH));
    let audit_id = store
        .force_release_channel_lane(&before, OPERATOR, "REST settled; channel reconciled")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(store.get_lockdown(CHANNEL).await.unwrap(), Some(seed));
    let released_hash: String = sqlx::query_scalar(
        "SELECT request_hash FROM moderation_idempotency WHERE guild_id = $1 AND idempotency_key = 'wedged'",
    )
    .bind(GUILD)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(released_hash, "operator_released");
    assert!(store
        .inspect_channel_lane(GUILD, CHANNEL, "wedged")
        .await
        .unwrap()
        .is_none());
    assert!(!store
        .complete(&ticket, "locked_down", "{}", TIME)
        .await
        .unwrap());
    assert!(matches!(
        store
            .claim(GUILD, "wedged", "moderation.unlock", ORIGINAL_HASH, TIME)
            .await
            .unwrap(),
        ChannelClaim::Mismatch
    ));
    let ChannelClaim::Replayed {
        outcome,
        result_json,
    } = store
        .claim(GUILD, "wedged", "moderation.lockdown", ORIGINAL_HASH, TIME)
        .await
        .unwrap()
    else {
        panic!("released intent must replay, not execute");
    };
    assert_eq!(outcome, "operator_released");
    let replay: Value = serde_json::from_str(&result_json).unwrap();
    assert_eq!(replay["outcome"], "operator_released");
    let audit: (String, String, String, String) = sqlx::query_as(
        "SELECT actor_id, idempotency_key, action, metadata_json FROM moderation_audit WHERE request_id = $1",
    ).bind(audit_id).fetch_one(db.pool()).await.unwrap();
    assert_eq!(audit.0, OPERATOR);
    assert_eq!(audit.1, "wedged");
    assert_eq!(audit.2, "moderation.channel_lane_release");
    let metadata: Value = serde_json::from_str(&audit.3).unwrap();
    assert_eq!(metadata["previous"], report);
    assert_eq!(
        metadata["previous"]["moderation_idempotency"]["request_hash"],
        "[REDACTED]"
    );
    assert_eq!(metadata["database_login"], "agent_test");
    assert_eq!(metadata["database_role"], "agent_test");
    assert_eq!(metadata["recovery_seed"], "untouched");
    assert!(!audit.3.contains(&raw_token));
    assert!(store
        .force_release_channel_lane(&before, OPERATOR, "duplicate")
        .await
        .unwrap()
        .is_none());
    claim(&store, "new-attempt").await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn stale_generation_cannot_delete_a_recreated_lane_or_claim() {
    let db = database().await;
    let store = ChannelModerationStore::from_pool(db.pool().clone());
    let old = claim(&store, "same-key").await;
    let inspection = store
        .inspect_channel_lane(GUILD, CHANNEL, "same-key")
        .await
        .unwrap()
        .unwrap();
    assert!(store.release(&old).await.unwrap());
    claim(&store, "same-key").await;
    let current = store
        .inspect_channel_lane(GUILD, CHANNEL, "same-key")
        .await
        .unwrap()
        .unwrap();
    assert_ne!(inspection.generation(), current.generation());
    assert!(store
        .force_release_channel_lane(&inspection, OPERATOR, "stale")
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        store
            .inspect_channel_lane(GUILD, CHANNEL, "same-key")
            .await
            .unwrap()
            .unwrap()
            .report(),
        current.report()
    );
    let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_audit")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(audits, 0);
    db.close().await.unwrap();
}

#[tokio::test]
async fn completed_claim_is_refused_even_when_it_still_has_a_lane() {
    let db = database().await;
    let store = ChannelModerationStore::from_pool(db.pool().clone());
    let ticket = claim(&store, "done").await;
    let before = store
        .inspect_channel_lane(GUILD, CHANNEL, "done")
        .await
        .unwrap()
        .unwrap();
    assert!(store
        .complete(&ticket, "locked_down", "{}", TIME)
        .await
        .unwrap());
    let completed = store
        .inspect_channel_lane(GUILD, CHANNEL, "done")
        .await
        .unwrap()
        .unwrap();
    assert!(!completed.releasable());
    for inspection in [&before, &completed] {
        assert!(store
            .force_release_channel_lane(inspection, OPERATOR, "refuse done")
            .await
            .unwrap()
            .is_none());
    }
    assert_eq!(
        store
            .inspect_channel_lane(GUILD, CHANNEL, "done")
            .await
            .unwrap()
            .unwrap()
            .report(),
        completed.report()
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn wrong_scope_and_inconsistent_tokens_are_refused() {
    let db = database().await;
    let store = ChannelModerationStore::from_pool(db.pool().clone());
    claim(&store, "held").await;
    for (guild, channel, key) in [
        ("other", CHANNEL, "held"),
        (GUILD, "other", "held"),
        (GUILD, CHANNEL, "other"),
    ] {
        assert!(store
            .inspect_channel_lane(guild, channel, key)
            .await
            .unwrap()
            .is_none());
    }
    let old = store
        .inspect_channel_lane(GUILD, CHANNEL, "held")
        .await
        .unwrap()
        .unwrap();
    // Only the ledger generation changes: deletion must roll back if retirement loses.
    sqlx::query("UPDATE moderation_idempotency SET claim_token = pg_catalog.gen_random_uuid()::text WHERE idempotency_key = 'held'")
        .execute(db.pool()).await.unwrap();
    assert!(store
        .force_release_channel_lane(&old, OPERATOR, "stale ledger")
        .await
        .unwrap()
        .is_none());
    let inconsistent = store
        .inspect_channel_lane(GUILD, CHANNEL, "held")
        .await
        .unwrap()
        .unwrap();
    assert!(!inconsistent.releasable());
    assert!(store
        .force_release_channel_lane(&inconsistent, OPERATOR, "inconsistent")
        .await
        .unwrap()
        .is_none());
    let lanes: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_channel_executions")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(lanes, 1);
    db.close().await.unwrap();
}

#[tokio::test]
async fn audit_failure_rolls_back_both_lane_release_and_request_retirement() {
    let db = database().await;
    let store = ChannelModerationStore::from_pool(db.pool().clone());
    claim(&store, "audit-failure").await;
    let before = store
        .inspect_channel_lane(GUILD, CHANNEL, "audit-failure")
        .await
        .unwrap()
        .unwrap();
    sqlx::query("ALTER TABLE moderation_audit ADD CONSTRAINT refuse_operator_audit CHECK (outcome <> 'operator_released')")
        .execute(db.pool()).await.unwrap();
    assert!(store
        .force_release_channel_lane(&before, OPERATOR, "audit failure")
        .await
        .is_err());
    assert_eq!(
        store
            .inspect_channel_lane(GUILD, CHANNEL, "audit-failure")
            .await
            .unwrap()
            .unwrap()
            .report(),
        before.report()
    );
    sqlx::query("ALTER TABLE moderation_audit DROP CONSTRAINT refuse_operator_audit")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(store
        .force_release_channel_lane(&before, OPERATOR, "retry persistence")
        .await
        .unwrap()
        .is_some());
    db.close().await.unwrap();
}
