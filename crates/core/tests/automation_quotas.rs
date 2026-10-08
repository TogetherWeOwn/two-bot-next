//! Store-level capacity proofs on an owned disposable database, never live data.
#![cfg(feature = "db")]

use sqlx::PgPool;
use two_bot_core::automation_quota::{AutomationQuota, QuotaWriteError};
use two_bot_core::feeds::{FeedKind, FeedRelay};
use two_bot_core::feeds_store::{self, FeedStoreError};
use two_bot_core::lfg::{LfgPost, LfgRole, LfgStatus};
use two_bot_core::{lfg_store, scheduled_store};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "900000000000000001";
const OTHER: &str = "900000000000000002";
const AT: &str = "2026-10-08T01:00:00.000Z";

async fn fixture() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("test bootstrap required");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("create migrated disposable fixture")
}

fn schedule(guild: &str, id: &str, enabled: bool) -> scheduled_store::ScheduledWrite {
    scheduled_store::ScheduledWrite {
        id: id.into(),
        guild_id: guild.into(),
        channel_id: "900000000000000003".into(),
        body: "fixture schedule".into(),
        next_run_at: AT.into(),
        interval_seconds: None,
        enabled,
        created_by: "900000000000000004".into(),
        created_at: AT.into(),
        updated_by: "900000000000000004".into(),
        updated_at: AT.into(),
    }
}

fn feed(guild: &str, id: &str, enabled: bool) -> FeedRelay {
    FeedRelay {
        id: id.into(),
        guild_id: guild.into(),
        channel_id: "900000000000000003".into(),
        kind: FeedKind::Rss,
        source: format!("https://example.com/{id}.xml"),
        enabled,
        last_checked_at: None,
        created_by: "900000000000000004".into(),
        created_at: 0,
        updated_at: 0,
    }
}

fn post(guild: &str, id: &str, status: LfgStatus) -> LfgPost {
    LfgPost {
        id: id.into(),
        guild_id: guild.into(),
        channel_id: "900000000000000003".into(),
        message_id: None,
        title: "fixture LFG".into(),
        starts_at: AT.into(),
        status,
        created_by: "900000000000000004".into(),
        created_at: AT.into(),
        closed_at: (status == LfgStatus::Closed).then(|| AT.into()),
    }
}

fn role(id: &str) -> LfgRole {
    LfgRole {
        lfg_id: id.into(),
        role_key: "player".into(),
        label: "Player".into(),
        slots: 2,
        position: 0,
    }
}

#[tokio::test]
async fn schedules_count_disabled_rows_allow_replacement_and_preserve_guild_fence() {
    let db = fixture().await;
    for n in 0..25 {
        scheduled_store::put_scheduled(db.pool(), &schedule(GUILD, &format!("s-{n}"), false))
            .await
            .unwrap();
    }
    assert!(matches!(
        scheduled_store::put_scheduled(db.pool(), &schedule(GUILD, "overflow", true)).await,
        Err(QuotaWriteError::Capacity(AutomationQuota::Schedules))
    ));
    assert!(
        scheduled_store::put_scheduled(db.pool(), &schedule(GUILD, "s-0", true))
            .await
            .unwrap()
    );
    assert!(
        !scheduled_store::put_scheduled(db.pool(), &schedule(OTHER, "s-0", true))
            .await
            .unwrap()
    );
    scheduled_store::put_scheduled(db.pool(), &schedule(OTHER, "other", true))
        .await
        .unwrap();
    assert_eq!(
        scheduled_store::list_scheduled(db.pool(), GUILD)
            .await
            .unwrap()
            .len(),
        25
    );
    scheduled_store::delete_scheduled(db.pool(), GUILD, "s-1")
        .await
        .unwrap();
    scheduled_store::put_scheduled(db.pool(), &schedule(GUILD, "replacement", true))
        .await
        .unwrap();

    // Historical restores are not pruned or rejected by these CRUD limits.
    // A grandfathered guild may update existing definitions, but cannot grow.
    sqlx::query("INSERT INTO scheduled_messages (id, guild_id, channel_id, body, next_run_at, interval_seconds, enabled, last_run_at, last_message_id, created_by, created_at, updated_by, updated_at, claim_token, claimed_at, occurrence_nonce) SELECT 'historical', guild_id, channel_id, body, next_run_at, interval_seconds, enabled, last_run_at, last_message_id, created_by, created_at, updated_by, updated_at, claim_token, claimed_at, occurrence_nonce FROM scheduled_messages WHERE id = 's-0'")
        .execute(db.pool()).await.unwrap();
    scheduled_store::put_scheduled(db.pool(), &schedule(GUILD, "s-0", false))
        .await
        .unwrap();
    assert!(matches!(
        scheduled_store::put_scheduled(db.pool(), &schedule(GUILD, "still-overflow", true)).await,
        Err(QuotaWriteError::Capacity(AutomationQuota::Schedules))
    ));
    assert_eq!(
        scheduled_store::list_scheduled(db.pool(), GUILD)
            .await
            .unwrap()
            .len(),
        26
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn feeds_count_disabled_rows_remain_insert_only_and_release_capacity_on_removal() {
    let db = fixture().await;
    feeds_store::add_feed(db.pool(), &feed(GUILD, "f-0", false))
        .await
        .unwrap();
    let mut collision = feed(OTHER, "f-0", true);
    collision.source = "https://example.com/other.xml".into();
    assert!(matches!(
        feeds_store::add_feed(db.pool(), &collision).await,
        Err(FeedStoreError::Db(_))
    ));
    for n in 1..25 {
        feeds_store::add_feed(db.pool(), &feed(GUILD, &format!("f-{n}"), false))
            .await
            .unwrap();
    }
    assert!(matches!(
        feeds_store::add_feed(db.pool(), &feed(GUILD, "overflow", true)).await,
        Err(FeedStoreError::Capacity(AutomationQuota::Feeds))
    ));
    let stored = feeds_store::list_feeds(db.pool(), GUILD, false)
        .await
        .unwrap();
    assert_eq!(stored.len(), 25);
    assert_eq!(stored[0].source, "https://example.com/f-0.xml");
    assert!(stored.iter().all(|row| !row.enabled));
    feeds_store::add_feed(db.pool(), &feed(OTHER, "other", true))
        .await
        .unwrap();
    feeds_store::remove_feed(db.pool(), GUILD, "f-0")
        .await
        .unwrap();
    feeds_store::add_feed(db.pool(), &feed(GUILD, "replacement", true))
        .await
        .unwrap();
    assert_eq!(
        feeds_store::list_feeds(db.pool(), GUILD, false)
            .await
            .unwrap()
            .len(),
        25
    );
    db.close().await.unwrap();
}

#[tokio::test]
async fn lfg_charges_new_open_and_reopened_posts_but_not_updates_or_closed_posts() {
    let db = fixture().await;
    for n in 0..20 {
        let id = format!("p-{n}");
        lfg_store::put_lfg(
            db.pool(),
            &post(GUILD, &id, LfgStatus::Open),
            &[role(&id)],
            true,
        )
        .await
        .unwrap();
    }
    assert!(matches!(
        lfg_store::put_lfg(
            db.pool(),
            &post(GUILD, "overflow", LfgStatus::Open),
            &[role("overflow")],
            true
        )
        .await,
        Err(QuotaWriteError::Capacity(AutomationQuota::OpenLfgPosts))
    ));
    assert!(lfg_store::get_lfg(db.pool(), GUILD, "overflow")
        .await
        .unwrap()
        .is_none());
    assert!(lfg_store::list_lfg_roles(db.pool(), "overflow")
        .await
        .unwrap()
        .is_empty());
    lfg_store::put_lfg(
        db.pool(),
        &post(GUILD, "closed", LfgStatus::Closed),
        &[role("closed")],
        true,
    )
    .await
    .unwrap();
    let mut changed_role = role("closed");
    changed_role.label = "must not persist".into();
    assert!(matches!(
        lfg_store::put_lfg(
            db.pool(),
            &post(GUILD, "closed", LfgStatus::Open),
            &[changed_role],
            true
        )
        .await,
        Err(QuotaWriteError::Capacity(AutomationQuota::OpenLfgPosts))
    ));
    assert_eq!(
        lfg_store::get_lfg(db.pool(), GUILD, "closed")
            .await
            .unwrap()
            .unwrap()
            .status,
        LfgStatus::Closed
    );
    assert_eq!(
        lfg_store::list_lfg_roles(db.pool(), "closed")
            .await
            .unwrap(),
        vec![role("closed")]
    );
    lfg_store::put_lfg(db.pool(), &post(GUILD, "p-0", LfgStatus::Open), &[], false)
        .await
        .unwrap();
    lfg_store::save_lfg_message_id(db.pool(), GUILD, "p-0", "900000000000000005")
        .await
        .unwrap();
    assert!(matches!(
        lfg_store::put_lfg(db.pool(), &post(OTHER, "p-0", LfgStatus::Open), &[], false).await,
        Err(QuotaWriteError::Database(sqlx::Error::RowNotFound))
    ));
    lfg_store::put_lfg(db.pool(), &post(OTHER, "other", LfgStatus::Open), &[], true)
        .await
        .unwrap();
    lfg_store::close_lfg(db.pool(), GUILD, "p-1", AT)
        .await
        .unwrap();
    lfg_store::put_lfg(
        db.pool(),
        &post(GUILD, "closed", LfgStatus::Open),
        &[],
        false,
    )
    .await
    .unwrap();
    lfg_store::close_lfg(db.pool(), GUILD, "p-2", AT)
        .await
        .unwrap();
    // An error after the upsert rolls back both the row and its capacity lock.
    assert!(lfg_store::put_lfg(
        db.pool(),
        &post(GUILD, "rollback", LfgStatus::Open),
        &[role("wrong-post")],
        true
    )
    .await
    .is_err());
    assert!(lfg_store::get_lfg(db.pool(), GUILD, "rollback")
        .await
        .unwrap()
        .is_none());
    lfg_store::put_lfg(
        db.pool(),
        &post(GUILD, "rollback", LfgStatus::Open),
        &[role("rollback")],
        true,
    )
    .await
    .unwrap();
    db.close().await.unwrap();
}

async fn write(pool: &PgPool, quota: AutomationQuota, id: &str) -> Result<(), QuotaWriteError> {
    match quota {
        AutomationQuota::Schedules => {
            scheduled_store::put_scheduled(pool, &schedule(GUILD, id, true))
                .await
                .map(|_| ())
        }
        AutomationQuota::Feeds => match feeds_store::add_feed(pool, &feed(GUILD, id, true)).await {
            Ok(()) => Ok(()),
            Err(FeedStoreError::Capacity(quota)) => Err(QuotaWriteError::Capacity(quota)),
            Err(FeedStoreError::Db(error)) => Err(error.into()),
            Err(error) => panic!("valid fixture feed rejected: {error}"),
        },
        AutomationQuota::OpenLfgPosts => {
            lfg_store::put_lfg(pool, &post(GUILD, id, LfgStatus::Open), &[], true).await
        }
    }
}

#[tokio::test]
async fn independent_clients_serialize_the_last_slot_including_lfg_reopening() {
    let db = fixture().await;
    let a = db.independent_pool().await.unwrap();
    let b = db.independent_pool().await.unwrap();
    // Warm distinct clients so connection setup does not hide a count/insert race.
    drop(a.acquire().await.unwrap());
    drop(b.acquire().await.unwrap());
    for (quota, key, table, predicate) in [
        (
            AutomationQuota::Schedules,
            "schedules",
            "scheduled_messages",
            "TRUE",
        ),
        (AutomationQuota::Feeds, "feeds", "feed_relays", "TRUE"),
        (
            AutomationQuota::OpenLfgPosts,
            "open-lfg",
            "lfg_posts",
            "status = 'open'",
        ),
    ] {
        for n in 0..quota.limit() - 1 {
            write(db.pool(), quota, &format!("{key}-{n}"))
                .await
                .unwrap();
        }
        let first = format!("{key}-last-a");
        let second = format!("{key}-last-b");
        if quota == AutomationQuota::OpenLfgPosts {
            lfg_store::put_lfg(
                db.pool(),
                &post(GUILD, &first, LfgStatus::Closed),
                &[],
                true,
            )
            .await
            .unwrap();
        }
        let mut blocker = db.pool().begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("automation-quota:{key}:{GUILD}"))
            .execute(&mut *blocker)
            .await
            .unwrap();
        let ap = a.clone();
        let bp = b.clone();
        let one = tokio::spawn(async move { write(&ap, quota, &first).await });
        let two = tokio::spawn(async move { write(&bp, quota, &second).await });
        // Prove both stores wait on the guild lock before either can count.
        tokio::time::timeout(std::time::Duration::from_secs(4), async {
            loop {
                let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted AND database = (SELECT oid FROM pg_database WHERE datname = current_database())")
                    .fetch_one(db.pool()).await.unwrap();
                if waiting >= 2 { break; }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.expect("both independent clients must wait on capacity admission");
        blocker.commit().await.unwrap();
        let results = [one.await.unwrap(), two.await.unwrap()];
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, Err(QuotaWriteError::Capacity(q)) if *q == quota))
                .count(),
            1
        );
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {table} WHERE guild_id = $1 AND {predicate}"
        )))
        .bind(GUILD)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(count, quota.limit());
    }
    a.close().await;
    b.close().await;
    db.close().await.unwrap();
}
