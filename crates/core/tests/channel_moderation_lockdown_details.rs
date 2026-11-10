#![cfg(feature = "db")]

//! Lockdown-setter attribution pins for `ChannelModerationStore::get_lockdown_details`.
//!
//! Store-level companion to the protected channel-inspect acceptance suite:
//! the actor query attributes a setter only when a successful (`locked_down`)
//! audit row matches the saved reason *and* the saved time. An older cycle's
//! row (different reason or time) or a refused attempt must leave the setter
//! unknown instead of naming the wrong moderator.
//!
//! Runs on the disposable test database only:
//! TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local \
//! python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db \
//!   --test channel_moderation_lockdown_details

use two_bot_core::{ChannelAuditRow, ChannelModerationStore, LockdownRecord, LockdownSeed};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "111111111111111111";
const CHANNEL: &str = "222222222222222222";
const ACTOR: &str = "333333333333333333";
const REASON: &str = "raid drill";
const LOCKED_AT: &str = "2026-10-01T00:00:00.000Z";
const OLDER_AT: &str = "2026-09-01T00:00:00.000Z";

async fn database() -> (ChannelModerationStore, TestDatabase) {
    let url =
        std::env::var("TWO_TEST_DATABASE_URL").expect("explicit isolated test service required");
    let fixture = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated disposable fixture");
    let store = ChannelModerationStore::from_pool(fixture.pool().clone());
    (store, fixture)
}

async fn seed(store: &ChannelModerationStore, reason: &str, locked_at: &str) -> LockdownRecord {
    store
        .record_lockdown(
            CHANNEL,
            GUILD,
            &LockdownSeed {
                prior_allow: "1024".to_owned(),
                prior_deny: "8192".to_owned(),
                prior_exists: true,
            },
            reason,
            locked_at,
        )
        .await
        .expect("seed lockdown record")
}

async fn audit(
    store: &ChannelModerationStore,
    request_id: &str,
    reason: &str,
    outcome: &str,
    created_at: &str,
) {
    store
        .record_audit(&ChannelAuditRow {
            request_id: request_id.to_owned(),
            guild_id: GUILD.to_owned(),
            actor_id: ACTOR.to_owned(),
            action: "moderation.lockdown".to_owned(),
            channel_id: Some(CHANNEL.to_owned()),
            reason: reason.to_owned(),
            outcome: outcome.to_owned(),
            idempotency_key: format!("details-pin-{request_id}"),
            metadata_json: "{}".to_owned(),
            created_at: created_at.to_owned(),
        })
        .await
        .expect("seed audit row");
}

#[tokio::test]
async fn mismatched_reason_audit_row_leaves_setter_unknown() {
    let (store, fixture) = database().await;
    let record = seed(&store, REASON, LOCKED_AT).await;
    // An older cycle's successful lockdown by the same actor, with a
    // different saved reason, must not attribute this record's setter.
    audit(
        &store,
        "reason-mismatch",
        "older cycle",
        "locked_down",
        LOCKED_AT,
    )
    .await;
    let details = store
        .get_lockdown_details(&record)
        .await
        .expect("read lockdown details");
    assert_eq!(details, Some((LOCKED_AT.to_owned(), None)));
    fixture.close().await.expect("verify fixture teardown");
}

#[tokio::test]
async fn mismatched_time_audit_row_leaves_setter_unknown() {
    let (store, fixture) = database().await;
    let record = seed(&store, REASON, LOCKED_AT).await;
    // Same reason, but the audit row belongs to an earlier lockdown: the
    // saved time does not match, so the setter stays unknown.
    audit(&store, "time-mismatch", REASON, "locked_down", OLDER_AT).await;
    let details = store
        .get_lockdown_details(&record)
        .await
        .expect("read lockdown details");
    assert_eq!(details, Some((LOCKED_AT.to_owned(), None)));
    fixture.close().await.expect("verify fixture teardown");
}

#[tokio::test]
async fn refused_audit_row_leaves_setter_unknown() {
    let (store, fixture) = database().await;
    let record = seed(&store, REASON, LOCKED_AT).await;
    // A refused lockdown attempt that shares reason and time is not a
    // successful attribution source.
    audit(&store, "refused-row", REASON, "refused", LOCKED_AT).await;
    let details = store
        .get_lockdown_details(&record)
        .await
        .expect("read lockdown details");
    assert_eq!(details, Some((LOCKED_AT.to_owned(), None)));
    fixture.close().await.expect("verify fixture teardown");
}

#[tokio::test]
async fn matching_lockdown_audit_row_attributes_setter() {
    let (store, fixture) = database().await;
    let record = seed(&store, REASON, LOCKED_AT).await;
    audit(&store, "matching-row", REASON, "locked_down", LOCKED_AT).await;
    let details = store
        .get_lockdown_details(&record)
        .await
        .expect("read lockdown details");
    assert_eq!(
        details,
        Some((LOCKED_AT.to_owned(), Some(ACTOR.to_owned())))
    );
    fixture.close().await.expect("verify fixture teardown");
}
