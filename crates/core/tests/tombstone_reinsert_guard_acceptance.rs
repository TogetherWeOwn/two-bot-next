//! Acceptance: the tombstone re-INSERT guard in the scheduled-event mirror.
//!
//! PROTECTED test-author file. The Builder builds on this file's immutable
//! head without editing it: a Builder change here voids the handoff.
//!
//! Disposable-database pins for the tombstone layer on top of the merged
//! epoch-claim gate: equal stamps still commit (idempotent writers share a
//! millisecond), so a row deleted by a same-millis tick can be resurrected
//! by a stale fetch unless the delete leaves a tombstone behind.
//!
//! Contract pinned here:
//! - every claiming `replace_events` tick writes one tombstone per mirror
//!   row its DELETE removed: `(guild_id, event_id, observed_at)` into
//!   `scheduled_events_tombstone`, refreshing `deleted_at` on re-delete;
//! - the tick's INSERTs carry a `NOT EXISTS` guard against that table, so a
//!   delete-then-stale-replay at equal stamps resurrects nothing;
//! - each claiming tick purges tombstones older than 24 h before its
//!   `observed_at` and trims each guild to the newest 1,000 tombstones,
//!   oldest-first;
//! - genuinely new ids still insert, in both guilds (the guard is
//!   guild-scoped), and the mutation path (`upsert_event`) stays ungated.
//!
//! Deliberately unpinned (Builder's choice): the exact 24 h boundary row,
//! tie-breaking among equal-`deleted_at` rows under the cap, and intra-tick
//! SQL order. The migration half lives in
//! `tombstone_migration_acceptance.rs` (offline, both chains + lockfile).
//!
//! Disposable database only (`TestDatabase` over the guarded agent-testdb
//! service). Skips without `TWO_TEST_DATABASE_URL`; never staging or
//! production. Synthetic fixtures only: no Discord, no live network.

#![cfg(feature = "db")]

use two_bot_core::{replace_events, upsert_event, EventStatus, ScheduledEvent};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "326474832151838730";
const OTHER_GUILD: &str = "999988887777666555";
const T1: &str = "2026-08-25T20:00:00.000Z";
const T2: &str = "2026-08-25T21:00:00.000Z";
const STARTS_AT: &str = "2026-09-06T17:30:00.000Z";

/// 25 h apart: unambiguously purged under the 24 h retention rule without
/// pinning the exact-boundary row.
const T_OLD: &str = "2026-08-24T20:00:00.000Z";
const T_PURGE: &str = "2026-08-25T21:00:00.000Z";
const T_PURGE_PLUS_1MS: &str = "2026-08-25T21:00:00.001Z";
const T_PURGE_PLUS_2MS: &str = "2026-08-25T21:00:00.002Z";

/// Cap-fixture stamps, all within 3 h so the 24 h purge never interferes
/// with the 1,000-row cap assertions.
const T_SEED: &str = "2026-08-25T18:00:00.000Z";
const T_D1: &str = "2026-08-25T19:00:00.000Z";
const T_D2: &str = "2026-08-25T20:00:00.000Z";
const T_D3: &str = "2026-08-25T21:00:00.000Z";

async fn connect(test: &str) -> Option<TestDatabase> {
    match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => Some(
            TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
                .await
                .expect("create migrated test database"),
        ),
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure tombstone acceptance ({test})"
            );
            eprintln!(
                "SKIP tombstone_reinsert_guard_acceptance ({test}): TWO_TEST_DATABASE_URL not set"
            );
            None
        }
    }
}

fn event(id: &str, name: &str) -> ScheduledEvent {
    ScheduledEvent {
        id: id.to_owned(),
        name: name.to_owned(),
        starts_at: STARTS_AT.to_owned(),
        channel_id: Some("voice-1".to_owned()),
        description: None,
        status: EventStatus::Scheduled,
    }
}

fn cap_event(index: usize) -> ScheduledEvent {
    event(&format!("cap-{index:04}"), "Cap event")
}

/// The guild's mirror ids, in id order.
async fn mirror_event_ids(pool: &sqlx::PgPool, guild: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT event_id FROM scheduled_events WHERE guild_id = $1 ORDER BY event_id",
    )
    .bind(guild)
    .fetch_all(pool)
    .await
    .expect("read mirror ids")
}

async fn tombstone_table_exists(pool: &sqlx::PgPool) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS (
           SELECT 1 FROM information_schema.tables
            WHERE table_name = 'scheduled_events_tombstone'
         )",
    )
    .fetch_one(pool)
    .await
    .expect("probe tombstone table")
}

/// `(event_id, deleted_at)` for the guild's tombstones, in id order.
async fn tombstone_rows(pool: &sqlx::PgPool, guild: &str) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT event_id, deleted_at
           FROM scheduled_events_tombstone
          WHERE guild_id = $1
          ORDER BY event_id",
    )
    .bind(guild)
    .fetch_all(pool)
    .await
    .expect("read tombstones")
}

async fn tombstone_deleted_at(pool: &sqlx::PgPool, guild: &str, event_id: &str) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT deleted_at FROM scheduled_events_tombstone WHERE guild_id = $1 AND event_id = $2",
    )
    .bind(guild)
    .bind(event_id)
    .fetch_optional(pool)
    .await
    .expect("read tombstone")
    .flatten()
}

async fn require_tombstone_table(pool: &sqlx::PgPool) {
    assert!(
        tombstone_table_exists(pool).await,
        "scheduled_events_tombstone is missing: the tombstone migration was not applied"
    );
}

#[tokio::test]
async fn tombstone_table_exists_with_guild_event_deleted_shape() {
    let Some(db) = connect("tombstone-table-shape").await else {
        return;
    };
    require_tombstone_table(db.pool()).await;
    let columns: Vec<(String, String)> = sqlx::query_as(
        "SELECT column_name, data_type
           FROM information_schema.columns
          WHERE table_name = 'scheduled_events_tombstone'
          ORDER BY ordinal_position",
    )
    .fetch_all(db.pool())
    .await
    .expect("read tombstone columns");
    let names: Vec<&str> = columns.iter().map(|(name, _)| name.as_str()).collect();
    assert!(
        names.contains(&"guild_id") && names.contains(&"event_id") && names.contains(&"deleted_at"),
        "tombstone table needs guild_id + event_id + deleted_at, found {names:?}"
    );
    for (name, data_type) in &columns {
        if name == "guild_id" || name == "event_id" || name == "deleted_at" {
            assert_eq!(data_type, "text", "{name} must be TEXT");
        }
    }
    let primary_key: Vec<String> = sqlx::query_scalar(
        "SELECT kcu.column_name
           FROM information_schema.table_constraints tc
           JOIN information_schema.key_column_usage kcu
             ON kcu.constraint_name = tc.constraint_name
            AND kcu.constraint_schema = tc.constraint_schema
          WHERE tc.table_name = 'scheduled_events_tombstone'
            AND tc.constraint_type = 'PRIMARY KEY'
          ORDER BY kcu.ordinal_position",
    )
    .fetch_all(db.pool())
    .await
    .expect("read tombstone primary key");
    assert_eq!(
        primary_key,
        vec!["guild_id".to_owned(), "event_id".to_owned()],
        "tombstone table primary key must be exactly (guild_id, event_id)"
    );
}

#[tokio::test]
async fn delete_tick_writes_tombstone_and_redelete_refreshes_it() {
    let Some(db) = connect("tombstone-write").await else {
        return;
    };
    require_tombstone_table(db.pool()).await;
    replace_events(db.pool(), GUILD, T1, &[event("event-1", "Sunday Squad")])
        .await
        .expect("seed snapshot commits");
    replace_events(db.pool(), GUILD, T1, &[])
        .await
        .expect("delete tick commits");
    assert!(
        mirror_event_ids(db.pool(), GUILD).await.is_empty(),
        "delete tick empties the mirror"
    );
    assert_eq!(
        tombstone_deleted_at(db.pool(), GUILD, "event-1")
            .await
            .as_deref(),
        Some(T1),
        "delete tick writes the tombstone at its observed stamp"
    );
    // The ungated mutation path re-adds the row; deleting it again refreshes
    // the tombstone to the latest observed delete instead of keeping the
    // stale stamp (which would purge a still-relevant tombstone early).
    upsert_event(db.pool(), GUILD, T1, &event("event-1", "Sunday Squad"))
        .await
        .expect("mutation re-adds the row");
    replace_events(db.pool(), GUILD, T2, &[])
        .await
        .expect("re-delete tick commits");
    assert_eq!(
        tombstone_deleted_at(db.pool(), GUILD, "event-1")
            .await
            .as_deref(),
        Some(T2),
        "re-delete refreshes the tombstone to the newer observed stamp"
    );
}

#[tokio::test]
async fn equal_stamp_stale_replay_resurrects_nothing() {
    // The core F3 pin, observable through the mirror alone: all three ticks
    // share one millisecond, so the epoch gate commits every one of them and
    // only the NOT EXISTS guard can stop the resurrection.
    let Some(db) = connect("equal-stamp-replay").await else {
        return;
    };
    replace_events(db.pool(), GUILD, T2, &[event("event-1", "Sunday Squad")])
        .await
        .expect("seed snapshot commits");
    replace_events(db.pool(), GUILD, T2, &[])
        .await
        .expect("same-millis delete commits");
    assert!(
        mirror_event_ids(db.pool(), GUILD).await.is_empty(),
        "delete tick empties the mirror"
    );
    replace_events(
        db.pool(),
        GUILD,
        T2,
        &[
            event("event-1", "Sunday Squad"),
            event("event-9", "Ghost Revival"),
        ],
    )
    .await
    .expect("stale replay commits at the equal stamp");
    assert!(
        mirror_event_ids(db.pool(), GUILD).await.is_empty(),
        "equal-stamp replay after a delete resurrects nothing: \
         neither the deleted id nor a never-before-seen id may reappear"
    );
}

#[tokio::test]
async fn purge_removes_tombstones_older_than_24h_and_frees_the_id() {
    let Some(db) = connect("purge-24h").await else {
        return;
    };
    require_tombstone_table(db.pool()).await;
    replace_events(db.pool(), GUILD, T_OLD, &[event("event-1", "Sunday Squad")])
        .await
        .expect("seed snapshot commits");
    replace_events(db.pool(), GUILD, T_OLD, &[])
        .await
        .expect("delete tick commits");
    assert_eq!(
        tombstone_deleted_at(db.pool(), GUILD, "event-1")
            .await
            .as_deref(),
        Some(T_OLD),
        "delete tick writes the tombstone"
    );
    replace_events(db.pool(), GUILD, T_PURGE, &[])
        .await
        .expect("claiming tick 25 h later commits");
    assert_eq!(
        tombstone_deleted_at(db.pool(), GUILD, "event-1").await,
        None,
        "each claiming tick purges tombstones older than 24 h"
    );
    // A purged id is reusable: the newest observation wins, so the same id
    // arriving 25 h after its delete inserts as a genuinely new event.
    replace_events(
        db.pool(),
        GUILD,
        T_PURGE_PLUS_1MS,
        &[
            event("event-1", "Sunday Squad"),
            event("event-2", "Friday Fights"),
        ],
    )
    .await
    .expect("previously deleted id inserts again after its tombstone purged");
    assert_eq!(
        mirror_event_ids(db.pool(), GUILD).await,
        vec!["event-1".to_owned(), "event-2".to_owned()]
    );
    replace_events(
        db.pool(),
        GUILD,
        T_PURGE_PLUS_2MS,
        &[event("event-1", "Sunday Squad")],
    )
    .await
    .expect("fresh delete commits");
    assert_eq!(
        tombstone_rows(db.pool(), GUILD).await,
        vec![("event-2".to_owned(), T_PURGE_PLUS_2MS.to_owned())],
        "only the fresh delete leaves a tombstone; the purged id stays reusable"
    );
}

#[tokio::test]
async fn purge_keeps_newest_1000_per_guild_oldest_first() {
    let Some(db) = connect("purge-cap").await else {
        return;
    };
    require_tombstone_table(db.pool()).await;
    let seed: Vec<ScheduledEvent> = (0..1005).map(cap_event).collect();
    replace_events(db.pool(), GUILD, T_SEED, &seed)
        .await
        .expect("seed snapshot commits");
    assert_eq!(mirror_event_ids(db.pool(), GUILD).await.len(), 1005);
    // Three delete waves 1 h apart: 400 tombstones at T_D1, 400 at T_D2, and
    // 205 at T_D3, for 1,005 total — 5 over the cap.
    let wave1: Vec<ScheduledEvent> = (400..1005).map(cap_event).collect();
    replace_events(db.pool(), GUILD, T_D1, &wave1)
        .await
        .expect("first delete wave commits");
    let wave2: Vec<ScheduledEvent> = (800..1005).map(cap_event).collect();
    replace_events(db.pool(), GUILD, T_D2, &wave2)
        .await
        .expect("second delete wave commits");
    // The cap is per guild: the other guild's tombstone must survive this
    // guild's trim.
    replace_events(
        db.pool(),
        OTHER_GUILD,
        T_D2,
        &[event("other-1", "Other Squad")],
    )
    .await
    .expect("other guild seed commits");
    replace_events(db.pool(), OTHER_GUILD, T_D2, &[])
        .await
        .expect("other guild delete commits");
    replace_events(db.pool(), GUILD, T_D3, &[])
        .await
        .expect("final delete wave commits");
    assert!(
        mirror_event_ids(db.pool(), GUILD).await.is_empty(),
        "delete waves empty the mirror"
    );
    let rows = tombstone_rows(db.pool(), GUILD).await;
    assert_eq!(
        rows.len(),
        1000,
        "each claiming tick trims the guild to at most 1,000 tombstones"
    );
    let oldest = rows.iter().filter(|(_, at)| at == T_D1).count();
    let middle = rows.iter().filter(|(_, at)| at == T_D2).count();
    let newest = rows.iter().filter(|(_, at)| at == T_D3).count();
    assert_eq!(
        newest, 205,
        "the trim evicts oldest-first: every newest tombstone survives"
    );
    assert_eq!(
        middle, 400,
        "the trim evicts oldest-first: the middle wave is untouched"
    );
    assert_eq!(
        oldest, 395,
        "the trim evicts 5 oldest tombstones to reach the cap"
    );
    assert_eq!(
        tombstone_deleted_at(db.pool(), OTHER_GUILD, "other-1")
            .await
            .as_deref(),
        Some(T_D2),
        "one guild's trim never touches another guild's tombstones"
    );
}

#[tokio::test]
async fn stale_tick_writes_nothing_including_no_tombstones() {
    // The epoch gate still rolls the whole transaction back: a stale fetch
    // that observes "no events" must leave the mirror row in place and write
    // no tombstone for the row it would have deleted.
    let Some(db) = connect("stale-no-tombstone").await else {
        return;
    };
    require_tombstone_table(db.pool()).await;
    replace_events(db.pool(), GUILD, T2, &[event("event-1", "Sunday Squad")])
        .await
        .expect("newer snapshot commits");
    replace_events(db.pool(), GUILD, T1, &[])
        .await
        .expect("stale call returns");
    assert_eq!(
        mirror_event_ids(db.pool(), GUILD).await,
        vec!["event-1".to_owned()],
        "stale tick leaves the mirror untouched"
    );
    assert_eq!(
        tombstone_deleted_at(db.pool(), GUILD, "event-1").await,
        None,
        "stale tick writes no tombstone for the row it would have deleted"
    );
}

#[tokio::test]
async fn genuinely_new_events_still_insert() {
    // Control: the guard blocks only tombstoned ids. New ids keep inserting
    // through the snapshot path, and the guard is guild-scoped — the same id
    // in another guild is a different event.
    let Some(db) = connect("new-inserts").await else {
        return;
    };
    replace_events(db.pool(), GUILD, T1, &[event("event-1", "Sunday Squad")])
        .await
        .expect("first write commits");
    replace_events(
        db.pool(),
        GUILD,
        T2,
        &[
            event("event-1", "Sunday Squad"),
            event("event-2", "Friday Fights"),
        ],
    )
    .await
    .expect("newer snapshot commits");
    assert_eq!(
        mirror_event_ids(db.pool(), GUILD).await,
        vec!["event-1".to_owned(), "event-2".to_owned()],
        "genuinely new events still insert"
    );
    replace_events(
        db.pool(),
        OTHER_GUILD,
        T2,
        &[event("event-1", "Sunday Squad")],
    )
    .await
    .expect("same id in another guild commits");
    assert_eq!(
        mirror_event_ids(db.pool(), OTHER_GUILD).await,
        vec!["event-1".to_owned()],
        "the guard is guild-scoped: the same id inserts in another guild"
    );
}

#[tokio::test]
async fn upsert_event_stays_ungated_after_delete() {
    // Control: the mutation path bypasses the snapshot guard. A delete
    // tombstones the id for stale snapshots, but an internal action
    // completing afterwards still lands its row — including at the equal
    // stamp, where no conflicting mirror row exists to guard against.
    let Some(db) = connect("upsert-ungated").await else {
        return;
    };
    replace_events(db.pool(), GUILD, T2, &[event("event-1", "Sunday Squad")])
        .await
        .expect("seed snapshot commits");
    replace_events(db.pool(), GUILD, T2, &[])
        .await
        .expect("delete tick commits");
    assert!(mirror_event_ids(db.pool(), GUILD).await.is_empty());
    upsert_event(db.pool(), GUILD, T2, &event("event-1", "Renamed By Action"))
        .await
        .expect("mutation upsert applies");
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT event_id, name, updated_at
           FROM scheduled_events
          WHERE guild_id = $1",
    )
    .bind(GUILD)
    .fetch_all(db.pool())
    .await
    .expect("read mirror row");
    assert_eq!(
        rows,
        vec![(
            "event-1".to_owned(),
            "Renamed By Action".to_owned(),
            T2.to_owned()
        )],
        "the mutation path stays ungated: its row lands with its own content and stamp"
    );
}
