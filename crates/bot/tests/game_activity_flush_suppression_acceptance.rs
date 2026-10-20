//! Protected acceptance tests: the game-activity aggregate flush must be
//! serialized with opt-out suppression changes.
//!
//! Contract under test (from the card spec, blind to the implementation):
//!
//! - The flush's opt-out gate read and its aggregate upsert are serialized
//!   with suppression changes — either a `record_event`-style lock spanning
//!   gate-plus-upsert, or a re-check before the upsert that drops
//!   newly-suppressed members. Both mechanisms satisfy the one observable
//!   invariant these tests pin: a member whose suppression is installed
//!   between the gate read and the upsert contributes zero to the stored
//!   aggregates, and a suppression change never slips into that window
//!   unnoticed.
//! - Sequential behavior is unchanged: a suppression installed before a
//!   flush contributes zero, a flush that lands before an opt-out stays in
//!   the report after the purge (the aggregates carry no member column to
//!   purge), later flushes for the suppressed member add nothing, other
//!   members keep counting throughout, and opting back in restores the
//!   member's contribution.
//!
//! Members are attributed through the game dimension on purpose: the stored
//! rows are keyed by (guild, game) with no member column, so member A plays
//! only "Apex" and control member B plays only "Zelda". An "Apex" row can
//! therefore only be A's contribution.
//!
//! The race test drives a deterministic disposable-database interleaving
//! with test-only table locks (never a sleep-based guess about where the
//! flush is): the gate's table is locked so the spawned flush parks at its
//! gate read, the toggle and aggregate tables are locked so the flush parks
//! again mid-flush after the gate read has returned, and only then is
//! suppression installed through the production opt-out service. If the
//! opt-out completes while the flush is parked, the stored aggregates must
//! exclude the member (re-check semantics, and the failing baseline). If
//! the opt-out cannot complete until the flush finishes, the flush
//! linearized first (lock semantics) and the test instead pins that the
//! suppression is fully installed afterwards and the next flush adds zero.
//!
//! PROTECTED: the paired builder implements production code against this
//! file unchanged. Do not edit these tests to make an implementation pass;
//! if a test is wrong, that is a finding for the test author, not a builder
//! edit.

use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Postgres, Transaction};
use two_bot::voice_rooms::game_activity::{persist_guild, GameActivityCollector, PersistOutcome};
use two_bot_core::game_activity::{ActivitySnapshot, ActivityType, PresenceDelta};
use two_bot_core::game_activity_store::read_guild_aggregates;
use two_bot_cutover::db::CutoverDb;
use two_bot_cutover::privacy::{is_opted_out, opt_back_in, opt_out_member};
use two_bot_testsupport::TestDatabase;

const GUILD: u64 = 1545644954272137297;
const MEMBER_A: u64 = 123456789012345678;
const MEMBER_B: u64 = 123456789012345679;
const MIN: i64 = 60_000;
const T0: i64 = 1_750_000_000_000;
const GAP_TIMEOUT_MS: i64 = 60 * MIN;

fn guild_text() -> String {
    GUILD.to_string()
}

/// Only an absent URL skips. A configured URL must name the disposable test
/// container (or the ephemeral CI service); anything else fails setup
/// loudly instead of touching a real database.
async fn database() -> Option<(TestDatabase, PgPool)> {
    let base_url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the flush suppression acceptance database"
            );
            eprintln!("SKIP flush suppression acceptance: TWO_TEST_DATABASE_URL not set");
            return None;
        }
    };
    let fixture = TestDatabase::create(&base_url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("disposable migrated database");
    // A dedicated pool with a statement timeout far above the interleaving
    // windows: the fixture pool's 5-second cap would cancel the parked
    // flush statements the race test deliberately blocks.
    let options: PgConnectOptions = base_url.parse().expect("test database URL parses");
    let host = options.get_host();
    assert!(
        host == "agent-testdb"
            || (host == "localhost" && std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true")),
        "test-container-only URL"
    );
    assert_eq!(
        options.get_username(),
        "agent_test",
        "test-container-only user"
    );
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(
            options
                .database(fixture.name())
                .options([("statement_timeout", "30000ms")]),
        )
        .await
        .expect("connect to owned disposable database");
    Some((fixture, pool))
}

async fn teardown(fixture: TestDatabase, pool: PgPool) {
    pool.close().await;
    fixture.close().await.expect("drop disposable database");
}

fn playing_delta(member: u64, at_ms: i64, game: &str) -> PresenceDelta {
    PresenceDelta {
        user_key: member.to_string(),
        observed_at_ms: at_ms,
        activities: vec![ActivitySnapshot {
            activity_type: ActivityType::Playing,
            name: game.to_owned(),
            application_id: None,
            details: None,
            state: None,
            party_size: None,
            assets: false,
            button_labels: Vec::new(),
            button_urls: Vec::new(),
            stream_url: None,
        }],
    }
}

/// Buffer one play session for `member`: two sightings `minutes` apart.
/// Every caller flushes at `start_ms + minutes`, and at that horizon the
/// session stores exactly `minutes` of play (a still-open session is
/// closed at the flush time, so a shorter seed flushed later would store
/// the longer span instead).
fn seed_play(
    collector: &GameActivityCollector,
    member: u64,
    game: &str,
    start_ms: i64,
    minutes: i64,
) {
    collector.observe_presence(GUILD, playing_delta(member, start_ms, game));
    collector.observe_presence(GUILD, playing_delta(member, start_ms + minutes * MIN, game));
}

fn seed_voice(collector: &GameActivityCollector, member: u64, start_ms: i64, minutes: i64) {
    collector.observe_join(GUILD, member, start_ms);
    collector.observe_leave(GUILD, member, start_ms + minutes * MIN);
}

/// The stored row for one game, when the flush persisted one.
async fn stored_game(pool: &PgPool, game: &str) -> Option<(i64, i64, u64)> {
    read_guild_aggregates(pool, &guild_text())
        .await
        .expect("read stored aggregates")
        .into_iter()
        .find(|row| row.game.as_deref() == Some(game))
        .map(|row| (row.total_play_ms, row.in_voice_ms, row.session_count))
}

/// Poll `pg_locks` until some session is blocked acquiring a lock on one of
/// `relations`. This is the deterministic "the flush is parked here"
/// signal; the deadline only turns a broken interleaving into a failure
/// instead of a hang.
async fn wait_for_blocked_relation(pool: &PgPool, relations: &[&str]) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        for relation in relations {
            let blocked: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_locks locks
                   JOIN pg_class class ON class.oid = locks.relation
                  WHERE NOT locks.granted AND locks.locktype = 'relation'
                    AND class.relname = $1",
            )
            .bind(relation)
            .fetch_one(pool)
            .await
            .expect("poll pg_locks");
            if blocked > 0 {
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no session parked on any of {relations:?} within 10s; the interleaving did not park the flush"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn release(tx: Option<Transaction<'static, Postgres>>) {
    if let Some(tx) = tx {
        tx.rollback().await.expect("release test lock transaction");
    }
}

/// The race: suppression installed between the flush's gate read and its
/// aggregate upsert must contribute zero (or the suppression change must be
/// serialized strictly after the flush — the lock-based reading of the same
/// contract, observed as the opt-out being unable to complete mid-flush).
#[tokio::test]
async fn suppression_installed_mid_flush_contributes_zero() {
    let Some((fixture, pool)) = database().await else {
        return;
    };
    let collector = Arc::new(GameActivityCollector::new());
    // Member A: 30 minutes of Apex with 1 minute in voice. Control member B:
    // 30 minutes of Zelda. After a correct mid-flush suppression of A, only
    // B's Zelda row may exist.
    seed_play(&collector, MEMBER_A, "Apex", T0, 30);
    seed_voice(&collector, MEMBER_A, T0, 1);
    seed_play(&collector, MEMBER_B, "Zelda", T0, 30);

    // Park the flush at its gate read: an ACCESS EXCLUSIVE lock on the
    // suppression table blocks both a plain gate SELECT and a lock-first
    // gate transaction, whichever the implementation uses.
    let mut gate_lock = pool.begin().await.expect("begin gate lock transaction");
    sqlx::query("LOCK TABLE privacy_opt_outs IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *gate_lock)
        .await
        .expect("lock suppression table");
    let persist_pool = pool.clone();
    let persist_collector = Arc::clone(&collector);
    let persist = tokio::spawn(async move {
        persist_guild(
            &persist_pool,
            &persist_collector,
            GUILD,
            GAP_TIMEOUT_MS,
            T0 + 30 * MIN,
        )
        .await
    });
    wait_for_blocked_relation(&pool, &["privacy_opt_outs"]).await;

    // Park the flush again after the gate read returns: the toggle table
    // lock stops an autocommit flush at its post-gate toggle read (before
    // any re-check-before-upsert the builder may add), and the aggregate
    // table lock stops every flush shape at its upsert. The toggle lock is
    // attempted with a timeout because a lock-first implementation may
    // legitimately hold a share lock on that table inside its gate
    // transaction; failure to take it is fine, the aggregate lock still
    // parks the flush.
    let mut toggle_tx = pool.begin().await.expect("begin toggle lock transaction");
    sqlx::query("SET LOCAL lock_timeout = '2000ms'")
        .execute(&mut *toggle_tx)
        .await
        .expect("bound toggle lock wait");
    let toggle_lock =
        match sqlx::query("LOCK TABLE guild_analytics_settings IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *toggle_tx)
            .await
        {
            Ok(_) => Some(toggle_tx),
            Err(_) => {
                toggle_tx
                    .rollback()
                    .await
                    .expect("roll back failed toggle lock transaction");
                None
            }
        };
    let mut aggregate_tx = pool
        .begin()
        .await
        .expect("begin aggregate lock transaction");
    sqlx::query("LOCK TABLE game_activity_guild_aggregates IN EXCLUSIVE MODE")
        .execute(&mut *aggregate_tx)
        .await
        .expect("lock aggregate table");
    let aggregate_lock = Some(aggregate_tx);

    // Let the gate read return (empty: no suppression exists yet), then wait
    // until the flush is parked at a post-gate step. Program order inside
    // the flush makes "parked on the toggle or aggregate table" proof the
    // gate read already happened.
    gate_lock
        .rollback()
        .await
        .expect("release gate lock transaction");
    wait_for_blocked_relation(
        &pool,
        &["guild_analytics_settings", "game_activity_guild_aggregates"],
    )
    .await;
    assert!(
        !persist.is_finished(),
        "flush must still be in flight while parked mid-flush"
    );

    // Install suppression for A through the production opt-out service,
    // between the gate read and the upsert.
    let opt_pool = pool.clone();
    let mut opt_out = tokio::spawn(async move {
        let db = CutoverDb::from_pool(opt_pool);
        opt_out_member(
            &db,
            &GUILD.to_string(),
            &MEMBER_A.to_string(),
            "acceptance-test",
        )
        .await
    });
    let opt_out_completed_mid_flush =
        tokio::time::timeout(Duration::from_secs(2), &mut opt_out).await;

    match opt_out_completed_mid_flush {
        Ok(joined) => {
            // The suppression committed while the flush was parked between
            // its gate read and its upsert. The flush must now observe it:
            // A's Apex contributes zero, B's Zelda persists untouched.
            joined
                .expect("opt-out task panicked")
                .expect("opt-out installed suppression mid-flush");
            assert!(
                !persist.is_finished(),
                "flush must still be parked when the suppression commits"
            );
            release(toggle_lock).await;
            release(aggregate_lock).await;
            let outcome = tokio::time::timeout(Duration::from_secs(10), persist)
                .await
                .expect("flush finishes after locks release")
                .expect("flush task panicked")
                .expect("flush succeeds after locks release");
            assert_eq!(
                outcome,
                PersistOutcome::Persisted {
                    rows: 1,
                    sessions: 1
                },
                "only the control member's session persists"
            );
            assert_eq!(
                stored_game(&pool, "Apex").await,
                None,
                "suppression installed between gate read and upsert contributes zero play, voice and sessions"
            );
            assert_eq!(
                stored_game(&pool, "Zelda").await,
                Some((30 * MIN, 0, 1)),
                "the un-suppressed member's session persists unchanged"
            );
        }
        Err(_) => {
            // The opt-out could not complete while the flush was in flight:
            // the implementation serialized the suppression change strictly
            // after the flush (lock semantics). Release the flush, let both
            // finish in that order, then pin that the suppression is
            // installed and the member's next flush adds zero.
            assert!(
                !persist.is_finished(),
                "a blocked opt-out only proves serialization if the flush is still in flight"
            );
            release(toggle_lock).await;
            release(aggregate_lock).await;
            tokio::time::timeout(Duration::from_secs(10), persist)
                .await
                .expect("flush finishes after locks release")
                .expect("flush task panicked")
                .expect("flush succeeds after locks release");
            tokio::time::timeout(Duration::from_secs(10), opt_out)
                .await
                .expect("opt-out finishes once the flush commits")
                .expect("opt-out task panicked")
                .expect("opt-out succeeds after the flush");
            let db = CutoverDb::from_pool(pool.clone());
            assert!(
                is_opted_out(&db, &guild_text(), &MEMBER_A.to_string())
                    .await
                    .expect("read suppression state"),
                "suppression is installed after the serialized opt-out"
            );
            // The control member counted in the flushed batch, and A's row
            // (whatever the pre-suppression flush legitimately stored) does
            // not grow from post-suppression play.
            assert_eq!(
                stored_game(&pool, "Zelda").await,
                Some((30 * MIN, 0, 1)),
                "the un-suppressed member's session persists unchanged"
            );
            let apex_before = stored_game(&pool, "Apex").await;
            let later = T0 + 180 * MIN;
            seed_play(&collector, MEMBER_A, "Apex", later, 30);
            seed_voice(&collector, MEMBER_A, later, 1);
            persist_guild(&pool, &collector, GUILD, GAP_TIMEOUT_MS, later + 30 * MIN)
                .await
                .expect("post-suppression flush succeeds");
            assert_eq!(
                stored_game(&pool, "Apex").await,
                apex_before,
                "post-suppression play contributes zero after the serialized opt-out"
            );
        }
    }
    teardown(fixture, pool).await;
}

/// Sequential control: a suppression installed before any flush starts
/// contributes zero, and the un-suppressed member in the same buffers
/// persists exactly as today.
#[tokio::test]
async fn suppression_before_flush_contributes_zero_and_control_persists() {
    let Some((fixture, pool)) = database().await else {
        return;
    };
    let db = CutoverDb::from_pool(pool.clone());
    opt_out_member(&db, &guild_text(), &MEMBER_A.to_string(), "acceptance-test")
        .await
        .expect("install suppression before flush");

    let collector = GameActivityCollector::new();
    seed_play(&collector, MEMBER_A, "Apex", T0, 30);
    seed_voice(&collector, MEMBER_A, T0, 1);
    seed_play(&collector, MEMBER_B, "Zelda", T0, 30);
    let outcome = persist_guild(&pool, &collector, GUILD, GAP_TIMEOUT_MS, T0 + 30 * MIN)
        .await
        .expect("sequential flush succeeds");
    assert_eq!(
        outcome,
        PersistOutcome::Persisted {
            rows: 1,
            sessions: 1
        }
    );
    assert_eq!(stored_game(&pool, "Apex").await, None);
    assert_eq!(stored_game(&pool, "Zelda").await, Some((30 * MIN, 0, 1)));
    teardown(fixture, pool).await;
}

/// Sequential control: a flush that lands before the opt-out stays in the
/// stored aggregates after the purge (there is no per-member row to purge
/// from the guild aggregates), the report read is unchanged, and the
/// suppressed member's later flushes add nothing on top of it.
#[tokio::test]
async fn flush_then_purge_leaves_report_unchanged_and_later_flushes_add_zero() {
    let Some((fixture, pool)) = database().await else {
        return;
    };
    let collector = GameActivityCollector::new();
    seed_play(&collector, MEMBER_A, "Apex", T0, 30);
    seed_voice(&collector, MEMBER_A, T0, 1);
    seed_play(&collector, MEMBER_B, "Zelda", T0, 30);
    persist_guild(&pool, &collector, GUILD, GAP_TIMEOUT_MS, T0 + 30 * MIN)
        .await
        .expect("pre-opt-out flush succeeds");
    let before_purge = read_guild_aggregates(&pool, &guild_text())
        .await
        .expect("read aggregates before purge");
    assert_eq!(stored_game(&pool, "Apex").await, Some((30 * MIN, MIN, 1)));
    assert_eq!(stored_game(&pool, "Zelda").await, Some((30 * MIN, 0, 1)));

    let db = CutoverDb::from_pool(pool.clone());
    opt_out_member(&db, &guild_text(), &MEMBER_A.to_string(), "acceptance-test")
        .await
        .expect("purge plus suppression succeeds");
    assert_eq!(
        read_guild_aggregates(&pool, &guild_text())
            .await
            .expect("read aggregates after purge"),
        before_purge,
        "the purge leaves the anonymous guild aggregates and their report read unchanged"
    );

    let later = T0 + 180 * MIN;
    seed_play(&collector, MEMBER_A, "Apex", later, 30);
    seed_voice(&collector, MEMBER_A, later, 1);
    let outcome = persist_guild(&pool, &collector, GUILD, GAP_TIMEOUT_MS, later + 30 * MIN)
        .await
        .expect("post-suppression flush succeeds");
    assert_eq!(
        outcome,
        PersistOutcome::Empty,
        "a flush holding only the suppressed member writes nothing"
    );
    assert_eq!(
        stored_game(&pool, "Apex").await,
        Some((30 * MIN, MIN, 1)),
        "post-suppression play adds zero on top of the pre-suppression totals"
    );
    teardown(fixture, pool).await;
}

/// Sequential control: opting back in restores the member's contribution to
/// later flushes exactly as before the opt-out.
#[tokio::test]
async fn opt_back_in_restores_contribution_to_later_flushes() {
    let Some((fixture, pool)) = database().await else {
        return;
    };
    let db = CutoverDb::from_pool(pool.clone());
    opt_out_member(&db, &guild_text(), &MEMBER_A.to_string(), "acceptance-test")
        .await
        .expect("install suppression");

    let collector = GameActivityCollector::new();
    seed_play(&collector, MEMBER_A, "Apex", T0, 30);
    persist_guild(&pool, &collector, GUILD, GAP_TIMEOUT_MS, T0 + 30 * MIN)
        .await
        .expect("suppressed flush succeeds");
    assert_eq!(
        stored_game(&pool, "Apex").await,
        None,
        "suppressed play persists nothing"
    );

    assert!(
        opt_back_in(&db, &guild_text(), &MEMBER_A.to_string())
            .await
            .expect("opt back in"),
        "opt-back-in removes the suppression row"
    );
    let later = T0 + 180 * MIN;
    seed_play(&collector, MEMBER_A, "Apex", later, 20);
    let outcome = persist_guild(&pool, &collector, GUILD, GAP_TIMEOUT_MS, later + 20 * MIN)
        .await
        .expect("post-opt-back-in flush succeeds");
    assert_eq!(
        outcome,
        PersistOutcome::Persisted {
            rows: 1,
            sessions: 1
        }
    );
    assert_eq!(
        stored_game(&pool, "Apex").await,
        Some((20 * MIN, 0, 1)),
        "only the post-opt-back-in session counts; suppressed play stays uncounted"
    );
    teardown(fixture, pool).await;
}
