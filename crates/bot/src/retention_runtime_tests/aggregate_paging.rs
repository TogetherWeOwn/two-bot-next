#![cfg(test)]

//! Acceptance contract: aggregate-expiry passes are bounded and advance.
//!
//! The retention tick currently lists every stale guild in one unbounded
//! fetch and visits them sequentially, aborting on the first guild whose
//! delete fails. These tests pin the required behaviour at the real
//! retention job boundary (no invented store API):
//!
//! - Each pass visits a bounded page of candidate guilds, never the whole
//!   eligible list when the list exceeds the page, and consecutive passes
//!   advance past already visited guilds instead of restarting at the
//!   first page. This must hold in dry-run mode too, where nothing is
//!   deleted and only an advancing cursor can make progress. A cursor
//!   that resets every pass (the mutation this file guards) can never
//!   report `guilds_deferred=0` while visiting fewer than all guilds.
//! - Every sweep event (`game_activity_aggregate_expiry`) carries, beside
//!   the existing `stale_rows`/`horizon_days`/`mode`:
//!   `guilds_visited` (candidate guilds visited this pass),
//!   `guilds_deferred` (eligible candidate guilds still after the visited
//!   page; zero only when the pass reached the end of the eligible list).
//!   `stale_rows` counts only rows expired (execute) or previewed
//!   (dry-run) in the visited guilds this pass. These are counts only:
//!   no per-guild metric labels.
//! - Dry-run totals are a same-predicate preview of the visited page,
//!   not a promise of an exact future delete count under concurrent
//!   writes; the sums asserted here are over a quiescent fixture.
//! - One guild's bounded delete failure retains its rows for a later
//!   retry but cannot starve later guilds across passes.
//! - An oversized guild is expired in bounded per-pass row batches
//!   across passes without losing its remaining stale rows.
//! - The retention horizon, per-guild delete predicate, dry-run versus
//!   execute behaviour and fresh-row protection are unchanged.
//!
//! Successive passes invoke the same registered job action, exactly as
//! the supervisor invokes one registered job per tick, so continuation
//! state may live wherever the implementation keeps it. Disposable
//! database only; no wall-clock timing assertions.

use super::database::{database, RUNTIME_LOCK};
use super::*;
use sqlx::PgPool;

/// More candidate guilds than any single bounded page may visit: a pass
/// visiting all of them at once is the unbounded behaviour under test.
const GUILDS: u64 = 12;
const STALE_PER_GUILD: u64 = 2;

fn guild_id(index: u64) -> String {
    format!("8{index:017}")
}

/// One registered retention job, invoked pass after pass.
struct Passes {
    action: jobs::JobAction,
}

impl Passes {
    fn new(pool: &PgPool, mode: RetentionMode) -> Self {
        Self {
            action: job(pool.clone(), mode).action,
        }
    }

    async fn pass(&self) -> (Result<(), ErrorClass>, String) {
        let capture = crate::tracing_capture::global();
        let offset = capture.text().len();
        let result = tokio::time::timeout(Duration::from_secs(30), (self.action)())
            .await
            .expect("retention pass must complete within fixture deadline");
        (result, capture.text()[offset..].to_owned())
    }
}

fn expiry_line(logs: &str) -> &str {
    let events: Vec<_> = logs
        .lines()
        .filter(|line| {
            line.contains("message=game_activity_aggregate_expiry")
                || line.contains("message=\"game_activity_aggregate_expiry\"")
        })
        .collect();
    assert_eq!(
        events.len(),
        1,
        "one aggregate expiry sweep event per pass with stale rows: {logs}"
    );
    events[0]
}

fn num_field(line: &str, name: &str) -> u64 {
    let token = line
        .split_whitespace()
        .find_map(|part| part.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("sweep event must carry {name}: {line}"));
    token
        .trim_matches('"')
        .parse()
        .unwrap_or_else(|_| panic!("{name} must be a count: {line}"))
}

async fn seed_guild(pool: &PgPool, guild: &str, stale: u64, fresh: u64) {
    for j in 0..stale {
        sqlx::query(
            "INSERT INTO game_activity_guild_aggregates \
             (guild_id, game, total_play_ms, in_voice_ms, session_count, updated_at) \
             VALUES ($1, $2, 600000, 0, 1, now() - make_interval(days => 100))",
        )
        .bind(guild)
        .bind(format!("Stale-{j}"))
        .execute(pool)
        .await
        .unwrap();
    }
    for j in 0..fresh {
        sqlx::query(
            "INSERT INTO game_activity_guild_aggregates \
             (guild_id, game, total_play_ms, in_voice_ms, session_count) \
             VALUES ($1, $2, 600000, 0, 1)",
        )
        .bind(guild)
        .bind(format!("Fresh-{j}"))
        .execute(pool)
        .await
        .unwrap();
    }
}

async fn stale_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM game_activity_guild_aggregates \
         WHERE updated_at < now() - make_interval(days => 90)",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn stale_rows_in(pool: &PgPool, guilds: &[String]) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM game_activity_guild_aggregates \
         WHERE guild_id = ANY($1) AND updated_at < now() - make_interval(days => 90)",
    )
    .bind(guilds)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn fresh_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM game_activity_guild_aggregates \
         WHERE updated_at >= now() - make_interval(days => 90)",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Control: passes on the current implementation and must keep passing.
/// A single-guild fixture previews in dry-run (deleting nothing) and
/// deletes exactly the stale row in execute, keeping the fresh row.
#[tokio::test]
async fn aggregate_expiry_small_fixture_previews_and_deletes_exactly_the_stale_row() {
    let _serial = RUNTIME_LOCK.lock().await;
    let Some(db) = database().await else { return };
    let pool = db.pool();
    seed_guild(pool, &guild_id(1), 1, 1).await;

    let dry = Passes::new(pool, RetentionMode::DryRun);
    let (result, logs) = dry.pass().await;
    result.unwrap();
    let preview = expiry_line(&logs);
    assert_eq!(num_field(preview, "stale_rows"), 1);
    assert_eq!(num_field(preview, "horizon_days"), 90);
    assert!(
        preview.contains("mode=DryRun") || preview.contains("mode=\"DryRun\""),
        "dry run must tag its preview: {preview}"
    );
    assert_eq!(stale_rows(pool).await, 1, "dry run deletes nothing");
    assert_eq!(fresh_rows(pool).await, 1);

    let execute = Passes::new(pool, RetentionMode::Execute);
    let (result, logs) = execute.pass().await;
    result.unwrap();
    assert_eq!(num_field(expiry_line(&logs), "stale_rows"), 1);
    assert_eq!(stale_rows(pool).await, 0);
    assert_eq!(fresh_rows(pool).await, 1, "fresh row survives execute");
    db.close().await.unwrap();
}

/// Dry-run passes advance past visited guilds: no pass visits the whole
/// eligible list, yet a complete cycle of passes visits every eligible
/// guild exactly once and ends with `guilds_deferred=0`. Nothing is
/// deleted, so only an advancing cursor can satisfy this; restarting at
/// the first page repeats the first guilds forever and never ends a
/// cycle.
#[tokio::test]
async fn aggregate_expiry_dry_run_pages_advance_past_visited_guilds() {
    let _serial = RUNTIME_LOCK.lock().await;
    let Some(db) = database().await else { return };
    let pool = db.pool();
    for i in 1..=GUILDS {
        seed_guild(pool, &guild_id(i), STALE_PER_GUILD, 1).await;
    }
    seed_guild(pool, &guild_id(99), 0, 1).await; // fresh-only control guild

    let dry = Passes::new(pool, RetentionMode::DryRun);
    // A first partial cycle is allowed (continuation state may start
    // mid-list); after the first cycle boundary the accounting is exact.
    let mut known_cycle = false;
    let mut cycle_visited = 0u64;
    let mut cycle_stale = 0u64;
    let mut completed = false;
    for _ in 0..36 {
        let (result, logs) = dry.pass().await;
        result.unwrap();
        let line = expiry_line(&logs);
        let visited = num_field(line, "guilds_visited");
        let deferred = num_field(line, "guilds_deferred");
        let stale = num_field(line, "stale_rows");
        assert!(
            (1..GUILDS).contains(&visited),
            "each pass visits a bounded page, never the whole eligible list: {line}"
        );
        assert!(
            (1..=STALE_PER_GUILD * visited).contains(&stale),
            "preview counts only the visited page's stale rows: {line}"
        );
        cycle_visited += visited;
        cycle_stale += stale;
        assert!(
            cycle_visited <= GUILDS,
            "a cycle never visits more guilds than are eligible: {line}"
        );
        if known_cycle && deferred > 0 {
            assert_eq!(
                deferred,
                GUILDS - cycle_visited,
                "deferred counts eligible guilds after the visited page: {line}"
            );
        }
        if deferred == 0 {
            if known_cycle {
                assert_eq!(
                    cycle_visited, GUILDS,
                    "a complete cycle visits every eligible guild exactly once"
                );
                assert_eq!(
                    cycle_stale,
                    GUILDS * STALE_PER_GUILD,
                    "a complete dry-run cycle previews every stale row exactly once"
                );
                completed = true;
                break;
            }
            known_cycle = true;
            cycle_visited = 0;
            cycle_stale = 0;
        }
    }
    assert!(
        completed,
        "dry-run passes must finish a full cycle in a finite number of passes"
    );
    assert_eq!(
        stale_rows(pool).await,
        i64::try_from(GUILDS * STALE_PER_GUILD).unwrap(),
        "dry run deletes nothing across all passes"
    );
    assert_eq!(
        fresh_rows(pool).await,
        i64::try_from(GUILDS + 1).unwrap(),
        "fresh rows, including the fresh-only guild, are never counted or touched"
    );
    db.close().await.unwrap();
}

/// Execute passes are bounded per pass, report deferred work while it
/// remains, and finish the over-cap fixture in a finite number of passes
/// without touching fresh rows.
#[tokio::test]
async fn aggregate_expiry_execute_pages_bound_each_pass_and_finish_in_finite_passes() {
    let _serial = RUNTIME_LOCK.lock().await;
    let Some(db) = database().await else { return };
    let pool = db.pool();
    for i in 1..=GUILDS {
        seed_guild(pool, &guild_id(i), STALE_PER_GUILD, 1).await;
    }

    let execute = Passes::new(pool, RetentionMode::Execute);
    let mut passes = 0u64;
    let mut remaining_before = stale_rows(pool).await;
    loop {
        passes += 1;
        assert!(
            passes <= 24,
            "execute must finish the fixture in a finite number of passes"
        );
        let (result, logs) = execute.pass().await;
        result.unwrap();
        let line = expiry_line(&logs);
        let visited = num_field(line, "guilds_visited");
        let deferred = num_field(line, "guilds_deferred");
        let stale = num_field(line, "stale_rows");
        if passes == 1 {
            assert!(
                visited < GUILDS,
                "the first pass is bounded, not the whole eligible list: {line}"
            );
        }
        assert!(visited >= 1, "each pass visits at least one guild: {line}");
        assert!(stale >= 1, "each pass expires at least one row: {line}");
        let remaining = stale_rows(pool).await;
        assert_eq!(
            remaining_before - remaining,
            i64::try_from(stale).unwrap(),
            "stale_rows reports exactly the rows this pass deleted: {line}"
        );
        remaining_before = remaining;
        if remaining == 0 {
            assert_eq!(
                deferred, 0,
                "no work is deferred once no stale rows remain: {line}"
            );
            break;
        }
        assert!(
            deferred >= 1,
            "a partial pass reports deferred guilds while stale rows remain: {line}"
        );
    }
    assert!(
        passes >= 2,
        "an over-cap fixture takes more than one bounded pass"
    );
    assert_eq!(
        fresh_rows(pool).await,
        i64::try_from(GUILDS).unwrap(),
        "every guild's fresh row survives all passes"
    );
    db.close().await.unwrap();
}

/// An oversized guild continues across passes: no single pass deletes
/// all of its stale rows, remaining rows are never lost, and repeated
/// passes eventually expire all of them while the fresh row survives.
#[tokio::test]
async fn aggregate_expiry_oversized_guild_continues_across_passes_without_losing_rows() {
    let _serial = RUNTIME_LOCK.lock().await;
    let Some(db) = database().await else { return };
    let pool = db.pool();
    const ROWS: u64 = 40;
    seed_guild(pool, &guild_id(1), ROWS, 1).await;

    let execute = Passes::new(pool, RetentionMode::Execute);
    let mut passes = 0u64;
    let mut remaining_before = stale_rows(pool).await;
    loop {
        passes += 1;
        assert!(
            passes <= ROWS,
            "the oversized guild must finish in a finite number of passes"
        );
        let (result, logs) = execute.pass().await;
        result.unwrap();
        let stale = num_field(expiry_line(&logs), "stale_rows");
        if passes == 1 {
            assert!(
                stale < ROWS,
                "one pass deletes a bounded batch, not the whole guild: {stale}"
            );
        }
        let remaining = stale_rows(pool).await;
        assert_eq!(
            remaining_before - remaining,
            i64::try_from(stale).unwrap(),
            "remaining stale rows are retained, never silently lost"
        );
        remaining_before = remaining;
        if remaining == 0 {
            break;
        }
    }
    assert!(passes >= 2, "the oversized guild spans multiple passes");
    assert_eq!(fresh_rows(pool).await, 1, "the fresh row survives");
    db.close().await.unwrap();
}

/// A persistently failing early guild cannot starve later guilds: across
/// repeated execute passes the later guilds are expired, the failing
/// guild's stale row is retained (never reported as expired), and once
/// the failure clears a later pass expires it too.
#[tokio::test]
async fn aggregate_expiry_failing_early_guild_cannot_starve_later_guilds() {
    let _serial = RUNTIME_LOCK.lock().await;
    let Some(db) = database().await else { return };
    let pool = db.pool();
    let failing = guild_id(1);
    let later: Vec<String> = (2..=6).map(guild_id).collect();
    seed_guild(pool, &failing, 1, 1).await;
    for guild in &later {
        seed_guild(pool, guild, 1, 0).await;
    }
    assert!(
        failing.bytes().all(|b| b.is_ascii_digit()),
        "fixture guild id is interpolated into DDL below"
    );
    // SAFETY: only the digit-guarded fixture guild id interpolates.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE FUNCTION aggregate_paging_refuse() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RAISE EXCEPTION 'synthetic aggregate delete refusal'; END $$; \
         CREATE TRIGGER aggregate_paging_refuse BEFORE DELETE \
         ON game_activity_guild_aggregates FOR EACH ROW \
         WHEN (OLD.guild_id = '{failing}') EXECUTE FUNCTION aggregate_paging_refuse();"
    )))
    .execute(pool)
    .await
    .unwrap();

    let execute = Passes::new(pool, RetentionMode::Execute);
    let mut later_cleared = false;
    for _ in 0..10 {
        // A pass may surface the failure or report deferred work; what it
        // may not do is leave later guilds unvisited forever.
        let _ = execute.pass().await;
        if stale_rows_in(pool, &later).await == 0 {
            later_cleared = true;
            break;
        }
    }
    assert!(
        later_cleared,
        "later guilds must be visited despite the early guild's persistent failure"
    );
    assert_eq!(
        stale_rows_in(pool, std::slice::from_ref(&failing)).await,
        1,
        "failed work is retained for a later retry, not marked expired"
    );
    assert_eq!(fresh_rows(pool).await, 1, "fresh rows are never deleted");

    sqlx::raw_sql(
        "DROP TRIGGER aggregate_paging_refuse ON game_activity_guild_aggregates; \
         DROP FUNCTION aggregate_paging_refuse();",
    )
    .execute(pool)
    .await
    .unwrap();
    let mut cleared = false;
    for _ in 0..10 {
        let (result, _) = execute.pass().await;
        if stale_rows_in(pool, std::slice::from_ref(&failing)).await == 0 {
            result.unwrap();
            cleared = true;
            break;
        }
    }
    assert!(
        cleared,
        "the previously failing guild expires once its failure clears"
    );
    db.close().await.unwrap();
}
