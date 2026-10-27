#![cfg(feature = "db")]

//! Breaks caught: locking no-op rows; shrinking reads to remaining update
//! quota; and removing the decay read lock (stale XP overwrites a live award).
//! All interleavings rendezvous on PostgreSQL lock state, never sleep races.

#[path = "support/hotpath_queries.rs"]
mod hotpath_queries;

use hotpath_queries::{assert_fetch_bound, Queries};
use sqlx::PgPool;
use std::time::Duration;
use two_bot_core::leveling_store as store;
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1545644954272137297";
const SEASON: &str = "2026-Q3";
const OLD: &str = "2026-08-15T00:00:00.000Z";
const TICK: &str = "2026-09-20T00:00:00.000Z";
const AWARD_GATE: i64 = 7_348_057;

type Audit = (String, String, i32, Option<String>, bool);

fn member(index: usize) -> String {
    format!("610000000000{index:06}")
}

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("decay concurrency requires disposable TWO_TEST_DATABASE_URL");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .unwrap()
}

async fn seed(pool: &PgPool, index: usize, xp: i64, weeks: i32, at: &str) {
    sqlx::query(
        "INSERT INTO member_season_xp (guild_id, season_id, member_id, seasonal_xp, decayed_weeks, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6::text::timestamptz)",
    ).bind(GUILD).bind(SEASON).bind(member(index)).bind(xp).bind(weeks).bind(at)
    .execute(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO member_levels (guild_id, member_id, xp, imported_xp, updated_at)
         VALUES ($1, $2, 1000, 1000, $3::text::timestamptz)",
    )
    .bind(GUILD)
    .bind(member(index))
    .bind(OLD)
    .execute(pool)
    .await
    .unwrap();
}

async fn worker_pool(db: &TestDatabase, name: &str) -> (PgPool, i32) {
    let pool = db.independent_pool().await.unwrap();
    let pid: i32 = sqlx::query_scalar(
        "SELECT pg_backend_pid() FROM set_config('application_name', $1, false)",
    )
    .bind(name)
    .fetch_one(&pool)
    .await
    .unwrap();
    (pool, pid)
}

async fn blocked_by(pool: &PgPool, blocker: i32, name: &str) -> Option<String> {
    sqlx::query_scalar(
        "SELECT query FROM pg_stat_activity
         WHERE datname = current_database() AND application_name = $2
           AND $1 = ANY(pg_blocking_pids(pid))",
    )
    .bind(blocker)
    .bind(name)
    .fetch_optional(pool)
    .await
    .unwrap()
}

async fn rendezvous(pool: &PgPool, blocker: i32, name: &str) -> String {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(query) = blocked_by(pool, blocker, name).await {
                return query;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("worker never reached the intended database lock barrier")
}

async fn no_op_prefix_award(target: usize) {
    let db = database().await;
    seed(db.pool(), 0, 1000, 0, TICK).await; // active

    // Both old targets independently pass a watermark-0 age cutoff (36d).
    // Only the watermark excludes member 1; only zero XP excludes member 2.
    seed(db.pool(), 1, 900, 1, OLD).await; // already decayed at this tick
    seed(db.pool(), 2, 0, 0, OLD).await; // no XP to decay
    seed(db.pool(), 3, 1000, 0, OLD).await; // eligible suffix / rendezvous
    let (decay_pool, decay_pid) = worker_pool(&db, "review-decay-prefix").await;
    let (award_pool, _) = worker_pool(&db, "review-award-prefix").await;
    let mut held = db.pool().begin().await.unwrap();
    let held_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *held)
        .await
        .unwrap();
    sqlx::query("SELECT member_id FROM member_season_xp WHERE guild_id = $1 AND season_id = $2 AND member_id = $3 FOR UPDATE")
        .bind(GUILD).bind(SEASON).bind(member(3)).fetch_one(&mut *held).await.unwrap();
    let decay_worker = {
        let pool = decay_pool.clone();
        tokio::spawn(async move { store::apply_seasonal_decay(&pool, GUILD, SEASON, TICK).await })
    };
    rendezvous(db.pool(), held_pid, "review-decay-prefix").await;
    let mut award_worker = {
        let pool = award_pool.clone();
        tokio::spawn(async move {
            store::award_message(&pool, GUILD, &member(target), TICK, None, None, false).await
        })
    };
    // Either the real award completes while decay is parked, or PostgreSQL
    // proves it is blocked on decay's unchanged prefix. Time only bounds a
    // failed rendezvous, not a performance threshold used as the oracle.
    let early = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            tokio::select! {
                result = &mut award_worker => break Ok(result),
                query = blocked_by(db.pool(), decay_pid, "review-award-prefix") => {
                    if let Some(query) = query { break Err(query); }
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    // Release even on the blocked mutant or a bounded rendezvous failure.
    // Join both workers and explicitly close the fixture before asserting.
    held.rollback().await.unwrap();
    let (completed_early, blocked_query, award) = match early {
        Ok(Ok(result)) => (true, None, result),
        Ok(Err(query)) => (false, Some(query), award_worker.await),
        Err(_) => (false, None, award_worker.await),
    };
    let decayed = decay_worker.await;
    decay_pool.close().await;
    award_pool.close().await;
    let prefix: Vec<(i64, i32)> = sqlx::query_as("SELECT seasonal_xp, decayed_weeks FROM member_season_xp WHERE member_id < $1 ORDER BY member_id")
        .bind(member(3)).fetch_all(db.pool()).await.unwrap();
    let timestamps: Vec<bool> = sqlx::query_scalar(
        "SELECT updated_at = (CASE WHEN member_id = $1 OR member_id = $2
                                  THEN $3 ELSE $4 END)::text::timestamptz
         FROM member_season_xp ORDER BY member_id",
    )
    .bind(member(target))
    .bind(member(0))
    .bind(TICK)
    .bind(OLD)
    .fetch_all(db.pool())
    .await
    .unwrap();
    let suffix: (i64, i32) = sqlx::query_as(
        "SELECT seasonal_xp, decayed_weeks FROM member_season_xp WHERE member_id = $1",
    )
    .bind(member(3))
    .fetch_one(db.pool())
    .await
    .unwrap();
    let lifetime: Vec<(i64, i64, i64, i64, bool)> = sqlx::query_as(
        "SELECT xp, message_xp, voice_xp, imported_xp,
                updated_at = (CASE WHEN member_id = $1 THEN $2 ELSE $3 END)::text::timestamptz
         FROM member_levels ORDER BY member_id",
    )
    .bind(member(target))
    .bind(TICK)
    .bind(OLD)
    .fetch_all(db.pool())
    .await
    .unwrap();
    let drain = store::apply_seasonal_decay(db.pool(), GUILD, SEASON, TICK).await;
    let audits: Vec<Audit> = sqlx::query_as(
        "SELECT member_id, source, xp, decision, occurred_at = $1::text::timestamptz
         FROM xp_awards ORDER BY member_id, source",
    )
    .bind(TICK)
    .fetch_all(db.pool())
    .await
    .unwrap();
    db.close().await.unwrap();
    assert!(
        completed_early,
        "no-op decay read held a row lock or award failed to rendezvous; target {target}, blocked SQL: {blocked_query:?}"
    );
    let award = award.unwrap().unwrap();
    assert_eq!(award.awarded, 15);
    assert_eq!(award.total_xp, 1015);
    assert_eq!(decayed.unwrap().unwrap(), 1);
    let mut expected_prefix = vec![(1000, 0), (900, 1), (0, 0)];
    expected_prefix[target].0 += 15;
    expected_prefix[target].1 = 0; // A real award resets the inactivity watermark.
    assert_eq!(
        prefix, expected_prefix,
        "no stale decay overwrites a live award"
    );
    assert_eq!(timestamps, vec![true; 4]);
    assert_eq!(suffix, (900, 1));
    let mut expected_lifetime = vec![(1000, 0, 0, 1000, true); 4];
    expected_lifetime[target] = (1015, 15, 0, 1000, true);
    assert_eq!(lifetime, expected_lifetime);
    assert_eq!(
        drain.unwrap(),
        0,
        "zero is a truthful drain marker after the overlap"
    );
    assert_eq!(
        audits,
        vec![
            (member(target), "message".into(), 15, None, true),
            (
                member(3),
                "decay".into(),
                100,
                Some("seasonal_decay".into()),
                true
            ),
        ],
        "one real award and one suffix decay only; no no-op or duplicate drain audit"
    );
}

#[tokio::test]
async fn no_op_prefix_does_not_block_a_fresh_member_award() {
    no_op_prefix_award(0).await;
}

/// Removing only SQL `AND seasonal_xp > 0` must block this aged zero-XP award.
#[tokio::test]
async fn no_op_prefix_does_not_block_an_old_zero_xp_member_award() {
    no_op_prefix_award(2).await;
}

/// Replacing only the SQL watermark with zero must block this positive-XP award.
#[tokio::test]
async fn no_op_prefix_does_not_block_an_old_already_decayed_member_award() {
    no_op_prefix_award(1).await;
}

#[tokio::test]
async fn active_suffix_after_24_decays_does_not_degenerate_into_row_at_a_time_reads() {
    let db = database().await;
    for index in 0..24 {
        seed(db.pool(), index, 1000, 0, OLD).await;
    }
    for index in 24..124 {
        seed(
            db.pool(),
            index,
            if index % 2 == 0 { 1000 } else { 0 },
            0,
            TICK,
        )
        .await;
    }
    seed(db.pool(), 124, 1000, 0, OLD).await;
    let (result, receipts) =
        Queries::capture(store::apply_seasonal_decay(db.pool(), GUILD, SEASON, TICK)).await;
    assert_eq!(result.unwrap(), 25, "actual update quota stays 25");
    let reads: Vec<_> = receipts
        .iter()
        .filter(|q| {
            let q = q.normalized();
            q.contains("select") && q.contains("from member_season_xp") && q.contains("member_id")
        })
        .collect();
    assert_fetch_bound(
        &receipts,
        |q| q.contains("select") && q.contains("from member_season_xp") && q.contains("member_id"),
        25,
    );
    let read_count = reads.len();
    let rows: Vec<(String, i64, i32)> = sqlx::query_as(
        "SELECT member_id, seasonal_xp, decayed_weeks FROM member_season_xp ORDER BY member_id",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    let audit_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM xp_awards WHERE source = 'decay'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    let drained = store::apply_seasonal_decay(db.pool(), GUILD, SEASON, TICK)
        .await
        .unwrap();
    db.close().await.unwrap();
    assert!(read_count <= 6, "125 rows need at most five fixed pages plus an empty drain read; observed {read_count} reads (remaining update quota must not shrink fetch pages)");
    assert_eq!(drained, 0, "zero is a truthful drain marker");
    assert_eq!(audit_count, 25);
    for (index, (_, xp, weeks)) in rows.iter().enumerate() {
        let want = if index < 24 || index == 124 {
            (900, 1)
        } else if index % 2 == 0 {
            (1000, 0)
        } else {
            (0, 0)
        };
        assert_eq!((*xp, *weeks), want, "member index {index}");
    }
}

async fn award_versus_decay(voice: bool) {
    let db = database().await;
    seed(db.pool(), 0, 1000, 0, OLD).await;
    // Award owns the seasonal row lock, then parks before commit. A correct
    // decay must read under that lock and recheck the fresh watermark after
    // award commits. Without the read lock it computes 900 from stale 1000.
    sqlx::query(
        "CREATE FUNCTION pause_live_award() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           IF NEW.seasonal_xp > OLD.seasonal_xp THEN
             PERFORM pg_advisory_xact_lock(7348057);
           END IF;
           RETURN NEW;
         END $$",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query("CREATE TRIGGER pause_live_award BEFORE UPDATE ON member_season_xp FOR EACH ROW EXECUTE FUNCTION pause_live_award()")
        .execute(db.pool()).await.unwrap();
    let (award_pool, award_pid) = worker_pool(&db, "review-live-award").await;
    let (decay_pool, _) = worker_pool(&db, "review-racing-decay").await;
    let mut gate = db.pool().begin().await.unwrap();
    let gate_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *gate)
        .await
        .unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(AWARD_GATE)
        .execute(&mut *gate)
        .await
        .unwrap();
    let award_worker = {
        let pool = award_pool.clone();
        tokio::spawn(async move {
            if voice {
                store::award_voice(&pool, GUILD, &member(0), 120, TICK, None)
                    .await
                    .map(|a| a.awarded)
            } else {
                store::award_message(&pool, GUILD, &member(0), TICK, None, None, false)
                    .await
                    .map(|a| a.awarded)
            }
        })
    };
    rendezvous(db.pool(), gate_pid, "review-live-award").await;
    let decay_worker = {
        let pool = decay_pool.clone();
        tokio::spawn(async move { store::apply_seasonal_decay(&pool, GUILD, SEASON, TICK).await })
    };
    let blocked_query = rendezvous(db.pool(), award_pid, "review-racing-decay").await;
    gate.rollback().await.unwrap();
    let awarded = award_worker.await.unwrap().unwrap();
    let decayed = decay_worker.await.unwrap().unwrap();
    award_pool.close().await;
    decay_pool.close().await;
    let state: (i64, i32, String) = sqlx::query_as(
        "SELECT seasonal_xp, decayed_weeks,
                to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
         FROM member_season_xp WHERE guild_id = $1 AND season_id = $2 AND member_id = $3",
    )
    .bind(GUILD)
    .bind(SEASON)
    .bind(member(0))
    .fetch_one(db.pool())
    .await
    .unwrap();
    let lifetime: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT xp, message_xp, voice_xp, imported_xp FROM member_levels WHERE guild_id = $1 AND member_id = $2",
    ).bind(GUILD).bind(member(0)).fetch_one(db.pool()).await.unwrap();
    let audits: Vec<(String, i32)> = sqlx::query_as("SELECT source, xp FROM xp_awards ORDER BY id")
        .fetch_all(db.pool())
        .await
        .unwrap();
    let drain = store::apply_seasonal_decay(db.pool(), GUILD, SEASON, TICK)
        .await
        .unwrap();
    db.close().await.unwrap();
    let expected_award = if voice { 10 } else { 15 };
    assert_eq!(awarded, expected_award);
    assert_eq!(
        decayed, 0,
        "award refreshed inactivity while decay waited; blocked SQL: {blocked_query}"
    );
    assert_eq!(
        state,
        (1000 + expected_award as i64, 0, TICK.into()),
        "a stale decay must not overwrite awarded XP or its updated_at/watermark"
    );
    assert_eq!(
        lifetime,
        (
            1000 + expected_award as i64,
            if voice { 0 } else { 15 },
            if voice { 10 } else { 0 },
            1000
        )
    );
    assert_eq!(
        audits,
        vec![(
            if voice { "voice" } else { "message" }.into(),
            expected_award as i32
        )],
        "no stale decay audit may commit"
    );
    assert_eq!(drain, 0);
}

#[tokio::test]
async fn message_award_committing_during_decay_read_cannot_be_overwritten() {
    award_versus_decay(false).await;
}

#[tokio::test]
async fn voice_award_committing_during_decay_read_cannot_be_overwritten() {
    award_versus_decay(true).await;
}
