//! Builder tests: the `StaleSnapshot` outcome of the epoch-claim gate.
//!
//! Companion to the `epoch_claim_*_acceptance` suites (protected, test-author
//! owned): those derive commit/rollback from epoch and row probes without
//! pinning the return shape. These pin the builder's return contract —
//! `replace_events` reports `Some(ScheduledEventsSkip::StaleSnapshot)` exactly
//! for the rolled-back stale fetch — and the skip's log identity (`Debug`
//! renders `StaleSnapshot`, the name the Events tick logs).
//!
//! The database test runs on a disposable test database only (`TestDatabase`
//! over the guarded agent-testdb service). Skips without
//! `TWO_TEST_DATABASE_URL`; never staging or production.

use two_bot_core::ScheduledEventsSkip;

#[cfg(feature = "db")]
use two_bot_core::{replace_events, write_counter, EventStatus, ScheduledEvent};
#[cfg(feature = "db")]
use two_bot_testsupport::TestDatabase;

#[cfg(feature = "db")]
const GUILD: &str = "326474832151838730";
#[cfg(feature = "db")]
const OTHER_GUILD: &str = "999988887777666555";
#[cfg(feature = "db")]
const T0: &str = "2026-08-25T19:00:00.000Z";
#[cfg(feature = "db")]
const T1: &str = "2026-08-25T20:00:00.000Z";
#[cfg(feature = "db")]
const T2: &str = "2026-08-25T21:00:00.000Z";
#[cfg(feature = "db")]
const T3: &str = "2026-08-25T22:00:00.000Z";
#[cfg(feature = "db")]
const STARTS_AT: &str = "2026-09-06T17:30:00.000Z";

#[test]
fn stale_snapshot_skip_keeps_its_log_name() {
    assert_eq!(
        format!("{:?}", ScheduledEventsSkip::StaleSnapshot),
        "StaleSnapshot"
    );
    assert_ne!(
        ScheduledEventsSkip::StaleSnapshot,
        ScheduledEventsSkip::DiscordReadFailed
    );
    assert_ne!(
        ScheduledEventsSkip::StaleSnapshot,
        ScheduledEventsSkip::InvalidResponse
    );
}

#[cfg(feature = "db")]
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
                "CI must configure epoch-claim outcome ({test})"
            );
            eprintln!("SKIP replace_events_stale_outcome ({test}): TWO_TEST_DATABASE_URL not set");
            None
        }
    }
}

#[cfg(feature = "db")]
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

#[cfg(feature = "db")]
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

#[cfg(feature = "db")]
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

#[cfg(feature = "db")]
async fn meta_guild(pool: &sqlx::PgPool) -> Option<String> {
    sqlx::query_scalar("SELECT guild_id FROM web_contract_meta WHERE singleton = TRUE")
        .fetch_one(pool)
        .await
        .expect("read contract meta")
}

#[cfg(feature = "db")]
#[tokio::test]
async fn replace_events_reports_stale_only_for_strictly_older_snapshots() {
    let Some(db) = connect("stale-outcome").await else {
        return;
    };
    // Genesis commits with no outcome: no claim row exists yet.
    assert_eq!(
        replace_events(db.pool(), GUILD, T2, &[event("event-1", "Sunday Squad")])
            .await
            .expect("genesis commits"),
        None
    );
    assert_eq!(claim_epoch(db.pool(), GUILD).await.as_deref(), Some(T2));
    // Pin the contract elsewhere so ANY write by the stale tick is observable.
    write_counter(db.pool(), OTHER_GUILD, T0, 7)
        .await
        .expect("pin contract to other guild");
    // The stale fetch carries a never-before-seen id: only the epoch gate can
    // stop the resurrection, and the rollback must cover the contract pin too.
    assert_eq!(
        replace_events(
            db.pool(),
            GUILD,
            T1,
            &[
                event("event-1", "Sunday Squad"),
                event("event-9", "Ghost Revival"),
            ],
        )
        .await
        .expect("stale call returns"),
        Some(ScheduledEventsSkip::StaleSnapshot)
    );
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
    // Newer and equal stamps commit with no outcome.
    assert_eq!(
        replace_events(
            db.pool(),
            GUILD,
            T3,
            &[
                event("event-1", "Sunday Squad"),
                event("event-2", "Friday Fights"),
            ],
        )
        .await
        .expect("newer commits"),
        None
    );
    assert_eq!(
        replace_events(
            db.pool(),
            GUILD,
            T3,
            &[
                event("event-1", "Sunday Squad"),
                event("event-2", "Friday Fights"),
            ],
        )
        .await
        .expect("equal stamp commits"),
        None
    );
    assert_eq!(mirror_rows(db.pool(), GUILD).await.len(), 2);
}
