//! Acceptance: the epoch-claim gate in the scheduled-event mirror swap.
//!
//! Disposable-database pins for the bounded-claim epoch layer: the claim
//! table exists with its contracted shape, removing it (rollback) leaves
//! every snapshot readable, a stale fetch after a newer commit rolls back
//! the whole transaction with a `StaleSnapshot` skip and zero writes, and
//! newer, equal-stamp, and genesis writes commit normally.
//!
//! The outcome below is derived from the epoch gate itself — a commit
//! advances the guild epoch to the observed stamp, so an epoch still
//! strictly newer than the stamp proves the transaction rolled back —
//! without depending on the builder's return-type shape. The tick-level log
//! line naming the skip is builder scope in the scheduler crate; the
//! `StaleSnapshot` outcome name is the contracted identity pinned here.
//!
//! Disposable database only (`TestDatabase` over the guarded agent-testdb
//! service). Skips without `TWO_TEST_DATABASE_URL`; never staging or
//! production. Synthetic fixtures only: no Discord, no live network.

#![cfg(feature = "db")]

use two_bot_core::{replace_events, write_counter, EventStatus, ScheduledEvent};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "326474832151838730";
const OTHER_GUILD: &str = "999988887777666555";
const T0: &str = "2026-08-25T19:00:00.000Z";
const T1: &str = "2026-08-25T20:00:00.000Z";
const T2: &str = "2026-08-25T21:00:00.000Z";
const STARTS_AT: &str = "2026-09-06T17:30:00.000Z";

/// The contracted swap outcome. `StaleSnapshot` is the name the design gives
/// the rolled-back stale fetch; the scheduler logs it as a skip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimOutcome {
    Committed,
    StaleSnapshot,
}

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
                "CI must configure epoch-claim acceptance ({test})"
            );
            eprintln!("SKIP epoch_claim_stale_snapshot_acceptance ({test}): TWO_TEST_DATABASE_URL not set");
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

/// `(event_id, name, status, updated_at)` for the guild's mirror, in id
/// order: the full observable row state a stale tick must leave untouched.
async fn mirror_rows(pool: &sqlx::PgPool, guild: &str) -> Vec<(String, String, String, String)> {
    sqlx::query_as(
        "SELECT event_id, name, status, updated_at
           FROM scheduled_events
          WHERE guild_id = $1
          ORDER BY event_id",
    )
    .bind(guild)
    .fetch_all(pool)
    .await
    .expect("read mirror")
}

async fn claim_table_exists(pool: &sqlx::PgPool) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS (
           SELECT 1 FROM information_schema.tables
            WHERE table_name = 'scheduled_events_claim'
         )",
    )
    .fetch_one(pool)
    .await
    .expect("probe claim table")
}

async fn claim_epoch(pool: &sqlx::PgPool, guild: &str) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT epoch FROM scheduled_events_claim WHERE guild_id = $1",
    )
    .bind(guild)
    .fetch_optional(pool)
    .await
    .expect("read claim")
    .flatten()
}

async fn meta_guild(pool: &sqlx::PgPool) -> Option<String> {
    sqlx::query_scalar("SELECT guild_id FROM web_contract_meta WHERE singleton = TRUE")
        .fetch_one(pool)
        .await
        .expect("read contract meta")
}

async fn replace_events_with_outcome(
    pool: &sqlx::PgPool,
    guild: &str,
    observed_at: &str,
    events: &[ScheduledEvent],
) -> ClaimOutcome {
    replace_events(pool, guild, observed_at, events)
        .await
        .expect("replace_events succeeds");
    if !claim_table_exists(pool).await {
        return ClaimOutcome::Committed;
    }
    match claim_epoch(pool, guild).await {
        Some(epoch) if epoch.as_str() > observed_at => ClaimOutcome::StaleSnapshot,
        _ => ClaimOutcome::Committed,
    }
}

#[tokio::test]
async fn claim_table_exists_with_guild_epoch_shape() {
    let Some(db) = connect("claim-table-shape").await else {
        return;
    };
    assert!(
        claim_table_exists(db.pool()).await,
        "scheduled_events_claim is missing: the epoch-claim migration was not applied"
    );
    let columns: Vec<(String, String)> = sqlx::query_as(
        "SELECT column_name, data_type
           FROM information_schema.columns
          WHERE table_name = 'scheduled_events_claim'
          ORDER BY ordinal_position",
    )
    .fetch_all(db.pool())
    .await
    .expect("read claim columns");
    let names: Vec<&str> = columns.iter().map(|(name, _)| name.as_str()).collect();
    assert!(
        names.contains(&"guild_id") && names.contains(&"epoch"),
        "claim table needs guild_id + epoch, found {names:?}"
    );
    for (name, data_type) in &columns {
        if name == "guild_id" || name == "epoch" {
            assert_eq!(data_type, "text", "{name} must be TEXT");
        }
    }
    let primary_key: Vec<String> = sqlx::query_scalar(
        "SELECT kcu.column_name
           FROM information_schema.table_constraints tc
           JOIN information_schema.key_column_usage kcu
             ON kcu.constraint_name = tc.constraint_name
            AND kcu.constraint_schema = tc.constraint_schema
          WHERE tc.table_name = 'scheduled_events_claim'
            AND tc.constraint_type = 'PRIMARY KEY'
          ORDER BY kcu.ordinal_position",
    )
    .fetch_all(db.pool())
    .await
    .expect("read claim primary key");
    assert_eq!(
        primary_key,
        vec!["guild_id".to_owned()],
        "claim table primary key must be exactly (guild_id)"
    );
}

#[tokio::test]
async fn claim_rollback_removal_leaves_snapshots_readable() {
    // The repo has no down-migration path, so rollback here is table removal:
    // dropping the gate must leave the mirror and the contract pin readable
    // and byte-identical. Pre-existing snapshots never depend on the claim.
    let Some(db) = connect("claim-rollback").await else {
        return;
    };
    let seeded = vec![
        event("event-1", "Sunday Squad"),
        event("event-2", "Friday Fights"),
    ];
    replace_events(db.pool(), GUILD, T2, &seeded)
        .await
        .expect("seed snapshot commits");
    let before = mirror_rows(db.pool(), GUILD).await;
    assert_eq!(before.len(), 2);
    sqlx::query("DROP TABLE scheduled_events_claim")
        .execute(db.pool())
        .await
        .expect("claim table drops cleanly");
    assert_eq!(
        mirror_rows(db.pool(), GUILD).await,
        before,
        "dropping the claim table must not touch the mirror"
    );
    assert_eq!(
        meta_guild(db.pool()).await.as_deref(),
        Some(GUILD),
        "dropping the claim table must not touch the contract pin"
    );
}

#[tokio::test]
async fn stale_after_newer_returns_stale_snapshot_skip() {
    let Some(db) = connect("stale-skip").await else {
        return;
    };
    let newer = vec![event("event-1", "Sunday Squad")];
    assert_eq!(
        replace_events_with_outcome(db.pool(), GUILD, T2, &newer).await,
        ClaimOutcome::Committed,
        "newer snapshot commits"
    );
    let stale = vec![
        event("event-1", "Sunday Squad"),
        event("event-9", "Ghost Revival"),
    ];
    assert_eq!(
        replace_events_with_outcome(db.pool(), GUILD, T1, &stale).await,
        ClaimOutcome::StaleSnapshot,
        "stale-after-newer must roll back with a StaleSnapshot skip"
    );
}

#[tokio::test]
async fn stale_after_newer_writes_nothing() {
    let Some(db) = connect("stale-zero-writes").await else {
        return;
    };
    let newer = vec![event("event-1", "Sunday Squad")];
    replace_events(db.pool(), GUILD, T2, &newer)
        .await
        .expect("newer snapshot commits");
    // Pin the contract to another guild AFTER the newer commit (which re-pins
    // to its own guild), so ANY write by the stale tick — rows, epoch, or
    // contract pin — is observable.
    write_counter(db.pool(), OTHER_GUILD, T0, 7)
        .await
        .expect("pin contract to other guild");
    // The stale fetch carries a never-before-seen id: the resurrection case
    // the row-only guards cannot stop.
    let stale = vec![
        event("event-1", "Sunday Squad"),
        event("event-9", "Ghost Revival"),
    ];
    replace_events(db.pool(), GUILD, T1, &stale)
        .await
        .expect("stale call returns");
    assert_eq!(
        mirror_rows(db.pool(), GUILD).await,
        vec![(
            "event-1".to_owned(),
            "Sunday Squad".to_owned(),
            "scheduled".to_owned(),
            T2.to_owned()
        )],
        "stale tick must write nothing, not even the new id"
    );
    assert_eq!(
        claim_epoch(db.pool(), GUILD).await.as_deref(),
        Some(T2),
        "stale tick must not move the epoch"
    );
    assert_eq!(
        meta_guild(db.pool()).await.as_deref(),
        Some(OTHER_GUILD),
        "stale tick must not re-pin the contract"
    );
}

#[tokio::test]
async fn newer_after_stale_commits() {
    let Some(db) = connect("newer-commits").await else {
        return;
    };
    let first = vec![event("event-1", "Sunday Squad")];
    assert_eq!(
        replace_events_with_outcome(db.pool(), GUILD, T1, &first).await,
        ClaimOutcome::Committed,
        "first write commits"
    );
    let second = vec![
        event("event-1", "Sunday Squad"),
        event("event-2", "Friday Fights"),
    ];
    assert_eq!(
        replace_events_with_outcome(db.pool(), GUILD, T2, &second).await,
        ClaimOutcome::Committed,
        "newer-after-stale commits"
    );
    assert_eq!(
        mirror_rows(db.pool(), GUILD).await,
        vec![
            (
                "event-1".to_owned(),
                "Sunday Squad".to_owned(),
                "scheduled".to_owned(),
                T2.to_owned()
            ),
            (
                "event-2".to_owned(),
                "Friday Fights".to_owned(),
                "scheduled".to_owned(),
                T2.to_owned()
            ),
        ]
    );
}

#[tokio::test]
async fn equal_stamp_commits() {
    let Some(db) = connect("equal-stamp").await else {
        return;
    };
    let first = vec![event("event-1", "Sunday Squad")];
    assert_eq!(
        replace_events_with_outcome(db.pool(), GUILD, T2, &first).await,
        ClaimOutcome::Committed,
        "first write commits"
    );
    let second = vec![
        event("event-1", "Sunday Squad"),
        event("event-2", "Friday Fights"),
    ];
    assert_eq!(
        replace_events_with_outcome(db.pool(), GUILD, T2, &second).await,
        ClaimOutcome::Committed,
        "equal-stamp ticks still commit"
    );
    assert_eq!(mirror_rows(db.pool(), GUILD).await.len(), 2);
}

#[tokio::test]
async fn first_write_without_claim_row_commits() {
    // Genesis: no claim row exists yet, so the gate must insert rather than
    // treat zero matched rows as stale. Guards the builder's first-write
    // path against the naive zero-rows-means-stale reading.
    let Some(db) = connect("genesis").await else {
        return;
    };
    let first = vec![event("event-1", "Sunday Squad")];
    assert_eq!(
        replace_events_with_outcome(db.pool(), GUILD, T2, &first).await,
        ClaimOutcome::Committed,
        "genesis write commits"
    );
    assert_eq!(mirror_rows(db.pool(), GUILD).await.len(), 1);
}
