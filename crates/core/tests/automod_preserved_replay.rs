#![cfg(feature = "db")]

//! End-to-end proof for the TOG-10089 P2 preserved-decision fix.
//!
//! Mirrors the reviewer's `pre_count_release_preserves_matched_retry`
//! scenario delivery-for-delivery (creates at 0/10s, matched edit at message
//! 20s, unrelated create at 60s sweeping the mutable in-memory repeat
//! history, same-key/stamp retry), but exercises the prescribed new protocol:
//! `preserve_match` immediately after the inspect-match, then the retry
//! replays `ClaimResult::Preserved` instead of re-running the swept tracker.
//! The replayed decision still reconciles through target resolution,
//! counting and planning with its rotated claim.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use two_bot_core::automod_runtime::*;
use two_bot_core::automod_store::{
    AutomodStore, ClaimResult, CompletionKind, DeliveryClaim, StoredOutcome,
};
use two_bot_core::commands::PERM_MODERATE_MEMBERS;
use two_bot_core::{
    AutomodConfig, AutomodFilter, AutomodMessage, ModerationPolicy, ModerationTarget,
};

// No DATABASE_URL fallback. Tests can reach only the named disposable container,
// or the explicitly opted-in CI service container (same user/empty password).
fn test_options() -> PgConnectOptions {
    let (host, database) = if std::env::var("AUTOMOD_CI_TESTDB").as_deref() == Ok("1") {
        assert_eq!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"));
        ("127.0.0.1", "postgres")
    } else {
        ("agent-testdb", "agent_test")
    };
    PgConnectOptions::from_str(&format!("postgres://agent_test@{host}:5432/{database}")).unwrap()
}

fn runtime() -> AutomodRuntime {
    AutomodRuntime::new(
        AutomodConfig::from_map(&HashMap::from([
            ("TWO_AUTOMOD".into(), "1".into()),
            ("TWO_AUTOMOD_ENFORCE".into(), "1".into()),
        ]))
        .unwrap(),
        AutomodScope {
            guild_id: STAGING_GUILD_ID.into(),
            live_approved: false,
        },
    )
}

fn delivery(
    kind: MessageDeliveryKind,
    id: &str,
    author: &str,
    content: &str,
    message_ms: u64,
    receipt_ms: u64,
) -> MessageDelivery {
    MessageDelivery {
        kind,
        guild_id: Some(STAGING_GUILD_ID.into()),
        channel_id: "222222222222222222".into(),
        message_id: id.into(),
        edited_timestamp_ms: (kind == MessageDeliveryKind::Update).then_some(20_000),
        observed_timestamp_ms: receipt_ms,
        snapshot: Some(AutomodMessage {
            guild_id: STAGING_GUILD_ID.into(),
            channel_id: "222222222222222222".into(),
            message_id: id.into(),
            author_id: author.into(),
            author_is_bot: false,
            role_ids: vec![],
            content: content.into(),
            mentioned_user_ids: vec![],
            attachment_names: vec![],
            observed_timestamp_ms: message_ms,
        }),
    }
}

fn facts() -> TargetFacts {
    TargetFacts {
        target: ModerationTarget {
            user_id: "444444444444444444".into(),
            role_ids: vec![],
            highest_role_position: 1,
            is_bot: false,
            is_guild_owner: false,
        },
        policy: ModerationPolicy {
            owen_user_id: "555555555555555555".into(),
            protected_role_ids: HashSet::from(["666666666666666666".into()]),
            bot_user_id: None,
        },
        bot_highest_role_position: 10,
        bot_permissions: PERM_MODERATE_MEMBERS,
    }
}

async fn acquire(store: &AutomodStore, key: &DeliveryKey) -> DeliveryClaim {
    let ClaimResult::Acquired(claim) = store.claim(key).await.unwrap() else {
        panic!("expected claim")
    };
    claim
}

#[tokio::test]
#[ignore = "requires agent-testdb, or the opt-in CI Postgres service container"]
async fn pre_count_release_preserves_matched_retry() {
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(test_options())
        .await
        .expect("agent-testdb must be available; never substitute credentials or another database");
    let schema = format!("automod_preserved_{}", std::process::id());
    sqlx::QueryBuilder::<sqlx::Postgres>::new("CREATE SCHEMA ")
        .push(&schema)
        .build()
        .execute(&admin)
        .await
        .unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(test_options().options([("search_path", schema.as_str())]))
        .await
        .unwrap();
    for sql in [
        include_str!("../../cutover/migrations/0220_automod.sql"),
        include_str!("../../cutover/migrations/0221_automod_delivery_claims.sql"),
        include_str!("../../cutover/migrations/0222_automod_counted_claim.sql"),
        include_str!("../../cutover/migrations/0223_automod_preserved_match.sql"),
    ] {
        sqlx::raw_sql(sql).execute(&pool).await.unwrap();
    }
    let store = AutomodStore::new(pool.clone());
    let mut runtime = runtime();

    // Two clean creates establish the repeat window the edit will match.
    for (id, time) in [("1", 0), ("2", 10_000)] {
        let msg = delivery(
            MessageDeliveryKind::Create,
            id,
            "444444444444444444",
            "same",
            time,
            time,
        );
        let key = DeliveryKey::from_delivery(&msg, false).unwrap();
        let claim = acquire(&store, &key).await;
        assert_eq!(
            runtime.inspect(&msg),
            Inspection::Accepted(FunnelDisposition::Accept)
        );
        assert!(store
            .complete(
                &claim,
                &StoredOutcome {
                    matched: false,
                    deleted: false,
                    outcome: CompletionKind::Accepted
                }
            )
            .await
            .unwrap());
    }

    // The edit matches on the third repeat; the gateway preserves the
    // decision immediately, before target resolution. The target lookup is
    // unavailable, so the pre-count claim is safely released.
    let mut edit = delivery(
        MessageDeliveryKind::Update,
        "3",
        "444444444444444444",
        "same",
        0,
        20_000,
    );
    let key = DeliveryKey::from_delivery(&edit, false).unwrap();
    let ClaimResult::Acquired(claim) = store.claim(&key).await.unwrap() else {
        panic!("claim")
    };
    let first = runtime.inspect(&edit);
    assert!(matches!(&first, Inspection::Matched(m) if m.filter == AutomodFilter::RepeatedMessage));
    let Inspection::Matched(ref matched) = first else {
        unreachable!()
    };
    assert!(store.preserve_match(&claim, matched).await.unwrap());
    assert_eq!(runtime.target_gate(matched, None), TargetGate::Unavailable);
    assert!(store.release_unmutated(&claim).await.unwrap());

    // The gateway continues while the target lookup is unavailable. A clean
    // create from another author expires this edit's previous repeat history.
    let unrelated = delivery(
        MessageDeliveryKind::Create,
        "99",
        "555555555555555555",
        "unrelated",
        60_000,
        60_000,
    );
    let unrelated_key = DeliveryKey::from_delivery(&unrelated, false).unwrap();
    let unrelated_claim = acquire(&store, &unrelated_key).await;
    assert_eq!(
        runtime.inspect(&unrelated),
        Inspection::Accepted(FunnelDisposition::Accept)
    );
    assert!(store
        .complete(
            &unrelated_claim,
            &StoredOutcome {
                matched: false,
                deleted: false,
                outcome: CompletionKind::Accepted
            }
        )
        .await
        .unwrap());

    // Same revision, same key — but the repeat history is swept, so a fresh
    // re-inspection would silently accept. The retry must replay the
    // preserved IDs/reason code instead of acquiring a fresh claim.
    edit.observed_timestamp_ms = 60_000;
    assert_eq!(DeliveryKey::from_delivery(&edit, false).unwrap(), key);
    let ClaimResult::Preserved(retry_claim, replayed) = store.claim(&key).await.unwrap() else {
        panic!("released decision must replay Preserved, not a fresh Acquire")
    };
    assert_eq!(replayed.filter, AutomodFilter::RepeatedMessage);
    assert_eq!(replayed.subject.message_id, "3");
    assert_eq!(replayed.subject.author_id, "444444444444444444");
    assert_eq!(replayed.funnel, FunnelDisposition::None);
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM automod_processed_messages")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "retry replays pre-count: nothing counted yet");

    // The replayed decision still reconciles: resolve, count, plan, fence,
    // and settle with the rotated claim.
    let facts = facts();
    assert_eq!(
        runtime.target_gate(&replayed, Some(&facts)),
        TargetGate::Allowed
    );
    let record = store
        .record_violation(
            &retry_claim,
            &replayed.subject,
            replayed.filter,
            "2026-09-30T00:00:00.000Z",
        )
        .await
        .unwrap();
    assert!(record.inserted);
    assert_eq!(record.count, 1);
    let plan = runtime.plan(&replayed, record, Some(&facts));
    assert_eq!(plan.outcome, PlanOutcome::Ready);
    assert!(
        plan.effects
            .iter()
            .any(|e| matches!(e, AutomodEffect::DeleteMessage)),
        "first sanction deletes the exact message"
    );
    assert!(store.mark_mutation_started(&retry_claim).await.unwrap());
    let receipt = StoredOutcome {
        matched: true,
        deleted: true,
        outcome: CompletionKind::Deleted,
    };
    assert!(store.complete(&retry_claim, &receipt).await.unwrap());
    assert!(matches!(store.claim(&key).await.unwrap(), ClaimResult::Replayed(r) if r == receipt));

    pool.close().await;
    sqlx::QueryBuilder::<sqlx::Postgres>::new("DROP SCHEMA ")
        .push(&schema)
        .push(" CASCADE")
        .build()
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
