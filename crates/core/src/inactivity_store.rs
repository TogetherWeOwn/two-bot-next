//! Inactivity sweep sqlx store (TOG-10092).
//!
//! Ports the `flagInactive()` write path from legacy `src/jobs/inactivity.ts`
//! onto the funnel `events`/`members` tables (0001, S6-owned): select the
//! quiet members, record one `member_inactive` event each, project
//! `inactive_flagged_at`. The pure selection stays in [`crate::inactivity`]:
//! this module only moves rows, so the SQL is a thin transliteration of the
//! legacy query.
//!
//! Behind the `db` feature so domain unit tests never need a Postgres driver.
//! The hourly scheduler calls [`run_sweep`]; until the S4 router slice lands,
//! the outcome type is the integration surface — no private dispatcher lives
//! here, and nothing here messages anybody (read-only contract, see
//! [`crate::inactivity`]).
//!
//! Legacy table/column names are kept exactly. Timestamps bind as ISO-8601
//! UTC with `::timestamptz` casts (repo convention from 0001/0002).

use std::collections::HashSet;

use sqlx::{Pool, Postgres};

use super::inactivity::{
    flag_inactive, should_flag, InactivityCandidate, InactivityOutcome, INACTIVITY_EVENT_SOURCE,
};

/// Inactivity store failure: transport, or a sweep timestamp that never parsed.
#[derive(Debug, thiserror::Error)]
pub enum InactivityStoreError {
    #[error("inactivity store database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("invalid inactivity sweep timestamp: {0:?}")]
    InvalidTimestamp(String),
    #[error("invalid inactivity sweep budget: {0:?}")]
    InvalidBudget(String),
}

/// Quiet-member rows per select: the sweep re-runs the bounded select until
/// it comes back empty, so no single read materializes an unbounded row set.
const SWEEP_PAGE_ROWS: usize = 4_096;

/// Page/time budget for one [`run_sweep_budgeted`] pass: at most `max_pages`
/// keyset selects, stopping early when `deadline` passes (checked per page).
/// Large memberships complete across passes by feeding the returned
/// [`SweepCursor`] back as `resume`.
#[derive(Debug, Clone, Copy)]
pub struct SweepBudget {
    /// Maximum keyset selects this pass reads. Zero is rejected with
    /// [`InactivityStoreError::InvalidBudget`] before any select: a zero
    /// budget that returned "more work" with an unchanged cursor would spin
    /// a resume loop forever, and silently reading a page anyway would write
    /// events after the budget was exhausted.
    pub max_pages: usize,
    /// Wall-clock budget for this pass, checked before every select.
    pub deadline: std::time::Instant,
}

/// Opaque resume position for a budgeted sweep: moved by value from one
/// [`run_sweep_budgeted`] pass to the next. Callers never inspect it — the
/// keyset cursor inside keeps pages disjoint across passes, and the pinned
/// sweep time keeps the cutoff and outcome stamps fixed, so the union across
/// passes is byte-identical to one complete sweep at the originating time no
/// matter what `now`/`days` later passes are called with.
#[derive(Debug, Clone)]
pub struct SweepCursor {
    cursor: Option<(String, String)>,
    /// Sweep time pinned by the originating pass: resume passes reuse it
    /// instead of their own `now`/`days`, so a member that goes quiet between
    /// passes is never silently skipped behind the cursor.
    now_ms: i64,
    /// Day threshold pinned beside `now_ms`.
    days: u64,
}

/// One hourly sweep (legacy `flagInactive`): select members still present,
/// not bots, quiet since before the cutoff, and either never flagged or last
/// flagged before the cutoff; record one `member_inactive` event each and
/// return their ids. `days` is the `TWO_INACTIVITY_DAYS` threshold, `now` the
/// sweep time as ISO-8601 UTC. An unparseable `now` fails the sweep with
/// [`InactivityStoreError::InvalidTimestamp`] before any query or write.
pub async fn run_sweep(
    pool: &Pool<Postgres>,
    now: &str,
    days: u64,
) -> Result<InactivityOutcome, InactivityStoreError> {
    let (outcome, _) = run_sweep_inner(pool, now, days, None, None).await?;
    Ok(outcome)
}

/// One budgeted sweep pass over the same quiet-member set as [`run_sweep`]:
/// at most `budget.max_pages` selects, stopping when `budget.deadline`
/// passes. A zero `max_pages` is rejected with
/// [`InactivityStoreError::InvalidBudget`] before any select. Returns the
/// pass outcome plus `Some(cursor)` iff more work may remain — an empty or
/// short page means done and returns `None`, while an expired deadline
/// before any page returns `Some` with an empty outcome. Resume passes
/// continue strictly past the cursor and reuse the originating pass's sweep
/// time: every member event is written exactly once and nothing is silently
/// skipped within or across passes, even when later passes are called with a
/// newer `now`. The paged SELECT keeps its SQL text.
pub async fn run_sweep_budgeted(
    pool: &Pool<Postgres>,
    now: &str,
    days: u64,
    budget: SweepBudget,
    resume: Option<SweepCursor>,
) -> Result<(InactivityOutcome, Option<SweepCursor>), InactivityStoreError> {
    if budget.max_pages == 0 {
        return Err(InactivityStoreError::InvalidBudget(
            "max_pages must be at least 1".to_owned(),
        ));
    }
    run_sweep_inner(
        pool,
        now,
        days,
        Some((budget.max_pages, budget.deadline)),
        resume,
    )
    .await
}

async fn run_sweep_inner(
    pool: &Pool<Postgres>,
    now: &str,
    days: u64,
    budget: Option<(usize, std::time::Instant)>,
    resume: Option<SweepCursor>,
) -> Result<(InactivityOutcome, Option<SweepCursor>), InactivityStoreError> {
    let parse = super::funnel::parse_iso_millis;
    let format = super::funnel::format_iso_millis;
    // Fail closed before any query or write: an unparseable sweep time must
    // never silently flag nobody and report success.
    let now_ms =
        parse(now).ok_or_else(|| InactivityStoreError::InvalidTimestamp(now.to_owned()))?;
    let mut cursor: Option<(String, String)> = None;
    let mut resume_time: Option<(i64, u64)> = None;
    if let Some(resume) = resume {
        cursor = resume.cursor;
        resume_time = Some((resume.now_ms, resume.days));
    }
    // Resume passes reuse the originating sweep time: re-deriving the cutoff
    // from a newer `now` would silently skip members that went quiet between
    // passes (they sit behind the cursor) and stamp the tail with a different
    // `occurred_at`, so the union would equal no complete sweep.
    let (now_ms, days) = resume_time.unwrap_or((now_ms, days));
    // The cutoff is computed in Rust (legacy `Date.now() - days * 86_400_000`)
    // and binds as one ISO timestamp: no interval arithmetic in SQL, and the
    // same millisecond feeds both the query and the outcome stamp.
    let cutoff = format(super::inactivity::inactivity_cutoff_ms(now_ms, days));
    /// One sweep row: guild, member, last-seen, flagged-at, bot flag.
    /// Timestamps decode as `OffsetDateTime` and format in Rust (the
    /// `rsvp_store` `iso_millis` pattern): no `to_char` quoting in SQL.
    type SweepRow = (
        String,
        String,
        Option<sqlx::types::time::OffsetDateTime>,
        Option<sqlx::types::time::OffsetDateTime>,
        bool,
    );
    /// `OffsetDateTime` to epoch millis for the shared ISO formatter.
    fn epoch_millis(dt: &sqlx::types::time::OffsetDateTime) -> i64 {
        dt.unix_timestamp() * 1000 + i64::from(dt.millisecond())
    }
    // Progress is structural, not observational: pages advance a composite
    // `(guild_id, member_id)` cursor along PRIMARY KEY order, so each page is
    // disjoint from every earlier page by construction. Rows that stay
    // eligible after being visited — a full page whose events already exist
    // but whose projections lag, or a page MemberJoin reopens by clearing
    // `inactive_flagged_at` — can never pin the window or hide the writable
    // tail behind them: the cursor has already moved past their keys. A page
    // therefore ends the sweep only when it comes back empty or short, never
    // because its rows were seen before.
    let mut written: Vec<(String, String)> = Vec::with_capacity(SWEEP_PAGE_ROWS);
    let mut outcome = InactivityOutcome { flagged: vec![] };
    let cutoff_ms = super::inactivity::inactivity_cutoff_ms(now_ms, days);
    let mut pages: usize = 0;
    let finished = loop {
        if let Some((max_pages, deadline)) = budget {
            // Zero budgets never reach here: `run_sweep_budgeted` rejects
            // them before the first select, so this check keeps the exact
            // at-most-max_pages contract.
            if pages >= max_pages || std::time::Instant::now() >= deadline {
                break false;
            }
        }
        // Keyset paging re-runs the bounded select with the previous page's
        // final key instead of using OFFSET (an offset would skip members the
        // earlier pages just flagged). `$3` is false on the first page, when
        // `$4`/`$5` are empty and ignored; on later pages the row comparison
        // seeks strictly past the cursor. Suppressed members are excluded
        // here, not only at the per-write gate: they never leave the eligible
        // set, so without the exclusion a suppressed prefix of
        // SWEEP_PAGE_ROWS or more would fill whole pages the sweep must still
        // walk key by key. The per-write suppression check stays as the
        // TOCTOU gate for opt-outs that land between this select and the
        // write.
        let (has_cursor, cursor_guild, cursor_member) = cursor
            .as_ref()
            .map(|(guild_id, member_id)| (true, guild_id.as_str(), member_id.as_str()))
            .unwrap_or((false, "", ""));
        let rows: Vec<SweepRow> = sqlx::query_as(
            "SELECT guild_id, member_id, COALESCE(last_active_at, joined_at),
                    inactive_flagged_at, is_bot
               FROM members
              WHERE left_at IS NULL
                AND NOT is_bot
                AND COALESCE(last_active_at, joined_at) < $1::timestamptz
                AND (inactive_flagged_at IS NULL OR inactive_flagged_at < $1::timestamptz)
                AND NOT EXISTS (
                    SELECT 1 FROM privacy_opt_outs AS opt
                    WHERE opt.guild_id = members.guild_id
                      AND opt.member_id = members.member_id
                )
                AND (NOT $3::boolean OR (guild_id, member_id) > ($4, $5))
              ORDER BY guild_id, member_id
              LIMIT $2",
        )
        .bind(&cutoff)
        .bind(SWEEP_PAGE_ROWS as i64)
        .bind(has_cursor)
        .bind(cursor_guild)
        .bind(cursor_member)
        .fetch_all(pool)
        .await?;
        pages += 1;
        if rows.is_empty() {
            break true;
        }
        let page_len = rows.len();
        if let Some((guild_id, member_id, _, _, _)) = rows.last() {
            cursor = Some((guild_id.clone(), member_id.clone()));
        }
        let candidates: Vec<InactivityCandidate> = rows
            .into_iter()
            .map(
                |(guild_id, member_id, last_seen, flagged_at, is_bot)| InactivityCandidate {
                    guild_id,
                    member_id,
                    last_seen_ms: last_seen.as_ref().map(epoch_millis),
                    flagged_at_ms: flagged_at.as_ref().map(epoch_millis),
                    is_bot,
                    has_left: false,
                },
            )
            .collect();
        let page = flag_inactive(&candidates, now_ms, days);
        for flagged in &page.flagged {
            let key = super::inactivity::member_inactive_event_key(
                &flagged.guild_id,
                &flagged.member_id,
                &flagged.occurred_at,
            );
            let mut tx = pool.begin().await?;
            // Serialize with concurrent opt-outs so a sweep that selected the
            // member before the opt-out cannot write after the purge. Lock order
            // matches `opt_out_member`'s sorted order; ROW EXCLUSIVE never blocks
            // other writers. The select already excludes suppressed members;
            // this is the TOCTOU gate for opt-outs landing between the select
            // and the write: a suppressed pair is skipped, since the flag event
            // would otherwise recreate per-user data the purge just removed.
            sqlx::query("LOCK TABLE events, members, privacy_opt_outs IN ROW EXCLUSIVE MODE")
                .execute(&mut *tx)
                .await?;
            // Revalidate the initial SELECT's eligibility atomically with the
            // write: the member may have become active, left, become a bot, or
            // been flagged by a concurrent sweep after selection. `FOR UPDATE`
            // pins the row so a concurrent winner commits first and this
            // transaction observes its flag stamp; a stale candidate rolls back
            // with no event, no flag mutation, and no outcome entry.
            type FreshRow = (
                Option<sqlx::types::time::OffsetDateTime>,
                Option<sqlx::types::time::OffsetDateTime>,
                bool,
                Option<sqlx::types::time::OffsetDateTime>,
            );
            let fresh: Option<FreshRow> = sqlx::query_as(
                "SELECT COALESCE(last_active_at, joined_at), inactive_flagged_at, is_bot, left_at
               FROM members WHERE guild_id = $1 AND member_id = $2 FOR UPDATE",
            )
            .bind(&flagged.guild_id)
            .bind(&flagged.member_id)
            .fetch_optional(&mut *tx)
            .await?;
            let Some((last_seen, flagged_at, is_bot, left_at)) = fresh else {
                tx.rollback().await?;
                continue;
            };
            let current = InactivityCandidate {
                guild_id: flagged.guild_id.clone(),
                member_id: flagged.member_id.clone(),
                last_seen_ms: last_seen.as_ref().map(epoch_millis),
                flagged_at_ms: flagged_at.as_ref().map(epoch_millis),
                is_bot,
                has_left: left_at.is_some(),
            };
            if !should_flag(&current, cutoff_ms) {
                tx.rollback().await?;
                continue;
            }
            if crate::privacy::suppressed_in(&mut tx, &flagged.guild_id, &flagged.member_id).await?
            {
                tx.rollback().await?;
                continue;
            }
            let inserted: Option<(i64,)> = sqlx::query_as(
            "INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
             VALUES ('member_inactive', $1, $2, $3::timestamptz, $4, $5, $6)
             ON CONFLICT (idempotency_key) DO NOTHING RETURNING id",
        )
        .bind(&flagged.member_id)
        .bind(&flagged.guild_id)
        .bind(&flagged.occurred_at)
        .bind(INACTIVITY_EVENT_SOURCE)
        .bind(format!("{{\"thresholdDays\":{days}}}"))
        .bind(&key)
        .fetch_optional(&mut *tx)
        .await?;
            if inserted.is_some() {
                sqlx::query(
                    "UPDATE members SET inactive_flagged_at = $1::timestamptz
                  WHERE guild_id = $2 AND member_id = $3",
                )
                .bind(&flagged.occurred_at)
                .bind(&flagged.guild_id)
                .bind(&flagged.member_id)
                .execute(&mut *tx)
                .await?;
                written.push((flagged.guild_id.clone(), flagged.member_id.clone()));
            } else if sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM events WHERE idempotency_key = $1",
            )
            .bind(&key)
            .fetch_one(&mut *tx)
            .await?
                > 0
            {
                // Already flagged in an earlier sweep: the outcome still reports
                // the member, but there is no new write to gate. The lagging
                // projection is deliberately left alone — repairing it here
                // would rewrite another sweep's outcome — and the keyset
                // cursor advances past such pages regardless of write outcome.
                written.push((flagged.guild_id.clone(), flagged.member_id.clone()));
            }
            tx.commit().await?;
        }
        outcome.flagged.extend(page.flagged);
        if page_len < SWEEP_PAGE_ROWS {
            break true;
        }
    };
    // Linear membership: one set build, one O(1) probe per outcome, and the
    // probes borrow, so the filter clones nothing per item.
    let written_set: HashSet<(&str, &str)> = written
        .iter()
        .map(|(guild_id, member_id)| (guild_id.as_str(), member_id.as_str()))
        .collect();
    outcome
        .flagged
        .retain(|f| written_set.contains(&(f.guild_id.as_str(), f.member_id.as_str())));
    let resume = if finished {
        None
    } else {
        Some(SweepCursor {
            cursor,
            now_ms,
            days,
        })
    };
    Ok((outcome, resume))
}

#[cfg(test)]
mod tests {
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    use super::*;

    /// Only an absent URL skips the test. Configured setup failures are errors.
    async fn test_pool(prefix: &str) -> Result<Option<(Pool<Postgres>, String)>, sqlx::Error> {
        let url = match std::env::var("TWO_TEST_DATABASE_URL") {
            Ok(url) => url,
            Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(_) => panic!("TWO_TEST_DATABASE_URL must be Unicode"),
        };
        test_pool_with_url(prefix, &url).await.map(Some)
    }

    async fn test_pool_with_url(
        prefix: &str,
        url: &str,
    ) -> Result<(Pool<Postgres>, String), sqlx::Error> {
        let options: PgConnectOptions = url.parse()?;
        // No DATABASE_URL or inherited credentials: tests accept only the
        // approved agent container or the ephemeral Postgres service in
        // GitHub Actions (see the `community-db` CI job).
        let host = options.get_host();
        assert!(
            host == "agent-testdb"
                || (host == "localhost"
                    && std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true")),
            "test-container-only URL"
        );
        assert_eq!(
            options.get_username(),
            "agent_test",
            "test-container-only user"
        );
        // No shared fixed-name fixtures: each invocation owns its schema,
        // including concurrent invocations of the same test in other processes.
        static NEXT_SCHEMA: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let schema = format!(
            "{prefix}_{}_{nonce}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        assert!(is_test_schema_name(&schema), "safe unique test schema");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await?;
        // SAFETY: only the guarded identifier interpolates; values bind.
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await?;
        admin.close().await;
        let pool = PgPoolOptions::new()
            .max_connections(3)
            .connect_with(options.options([("search_path", schema.as_str())]))
            .await?;
        // The sweep reads/writes the S6-owned funnel tables; this test needs
        // 0001 only (0310/0311 are unrelated). `members.is_bot` defaults
        // FALSE per 0001, matching legacy. The sweep's suppression gate
        // needs the opt-out table too (suppression-only, like other
        // lifecycle writers); no rows are seeded unless a test inserts one.
        sqlx::raw_sql(include_str!("../../cutover/migrations/0001_funnel.sql"))
            .execute(&pool)
            .await?;
        sqlx::raw_sql(
            "CREATE TABLE IF NOT EXISTS privacy_opt_outs (
               guild_id TEXT NOT NULL,
               member_id TEXT NOT NULL,
               opted_out_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
               PRIMARY KEY (guild_id, member_id)
             )",
        )
        .execute(&pool)
        .await?;
        Ok((pool, schema))
    }

    /// Test-only schema names: lowercase identifier characters, so DDL
    /// interpolation cannot break out of the identifier position.
    fn is_test_schema_name(schema: &str) -> bool {
        !schema.is_empty()
            && schema.len() <= 63
            && schema
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    }

    async fn drop_schema(pool: &Pool<Postgres>, schema: &str) {
        assert!(is_test_schema_name(schema));
        // SAFETY: guarded by `is_test_schema_name` (see `test_pool`).
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(pool)
            .await
            .expect("clean up owned test schema");
        pool.close().await;
    }

    async fn seed_member(
        pool: &Pool<Postgres>,
        member: &str,
        joined_at: &str,
        last_active_at: Option<&str>,
        is_bot: bool,
        left_at: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO members (guild_id, member_id, joined_at, last_active_at, is_bot, left_at)
             VALUES ('guild-a', $1, $2::timestamptz, $3::timestamptz, $4, $5::timestamptz)",
        )
        .bind(member)
        .bind(joined_at)
        .bind(last_active_at)
        .bind(is_bot)
        .bind(left_at)
        .execute(pool)
        .await
        .expect("seed member");
    }

    /// A synthetic pool that can never touch the network: lazy, then closed.
    /// Budget-shape tests take it to prove they stop before any select — a
    /// regression that queries would fail here instead of hanging on I/O.
    async fn closed_lazy_pool() -> Pool<Postgres> {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://agent_test:@agent-testdb:5432/agent_test")
            .expect("synthetic lazy pool");
        pool.close().await;
        pool
    }

    /// An expired deadline stops the budgeted pass before any select: the
    /// pass reports more work with an empty outcome. Offline (closed pool):
    /// this also executes the real `run_sweep_budgeted` entry and budget
    /// check in-process, which the child-compiled acceptance binary cannot
    /// cover.
    #[tokio::test]
    async fn budgeted_pass_with_expired_deadline_reports_more_work_without_selects() {
        let pool = closed_lazy_pool().await;
        let budget = SweepBudget {
            max_pages: 10_000,
            deadline: std::time::Instant::now(),
        };
        let (outcome, resume) =
            run_sweep_budgeted(&pool, "2026-09-07T00:00:00.000Z", 14, budget, None)
                .await
                .expect("budgeted pass");
        assert!(outcome.flagged.is_empty());
        assert!(resume.is_some());
    }

    /// An unparseable sweep time fails the budgeted pass before any query,
    /// like the complete sweep.
    #[tokio::test]
    async fn budgeted_pass_with_unparseable_now_fails_before_any_query() {
        let pool = closed_lazy_pool().await;
        let budget = SweepBudget {
            max_pages: 10_000,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(60),
        };
        assert!(matches!(
            run_sweep_budgeted(&pool, "not-a-time", 14, budget, None).await,
            Err(InactivityStoreError::InvalidTimestamp(_))
        ));
    }

    #[tokio::test]
    async fn sweep_flags_quiet_members_once_and_never_messages() {
        let Some((pool, schema)) = test_pool("tog_10092_inactivity")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping inactivity_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        seed_member(
            &pool,
            "quiet",
            "2026-07-01T00:00:00.000Z",
            None,
            false,
            None,
        )
        .await;
        seed_member(
            &pool,
            "active",
            "2026-07-01T00:00:00.000Z",
            Some("2026-09-06T00:00:00.000Z"),
            false,
            None,
        )
        .await;
        seed_member(&pool, "bot", "2026-07-01T00:00:00.000Z", None, true, None).await;
        seed_member(
            &pool,
            "left",
            "2026-07-01T00:00:00.000Z",
            None,
            false,
            Some("2026-08-01T00:00:00.000Z"),
        )
        .await;
        let first = run_sweep(&pool, "2026-09-07T00:00:00.000Z", 14)
            .await
            .expect("first sweep");
        assert_eq!(
            first
                .flagged
                .iter()
                .map(|f| f.member_id.as_str())
                .collect::<Vec<_>>(),
            ["quiet"]
        );
        // The event row exists with the legacy repeatable key shape; the
        // projection stamps `inactive_flagged_at`.
        let events: Vec<(String,)> = sqlx::query_as(
            "SELECT idempotency_key FROM events WHERE event_type = 'member_inactive'",
        )
        .fetch_all(&pool)
        .await
        .expect("event rows");
        assert_eq!(events.len(), 1);
        let flagged_at: (Option<String>,) = sqlx::query_as(
            "SELECT to_char(inactive_flagged_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')
               FROM members WHERE guild_id = 'guild-a' AND member_id = 'quiet'",
        )
        .fetch_one(&pool)
        .await
        .expect("flag timestamp");
        assert_eq!(flagged_at.0.as_deref(), Some("2026-09-07T00:00:00.000Z"));
        // Second sweep: no double-flag, no new rows. Read-only apart from the
        // first flag — and no message, DM, or ping is ever emitted (the
        // outcome type carries no notification surface; assert the events
        // table holds exactly the one flag row).
        let second = run_sweep(&pool, "2026-09-07T01:00:00.000Z", 14)
            .await
            .expect("second sweep");
        assert!(second.flagged.is_empty());
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
            .fetch_one(&pool)
            .await
            .expect("event count");
        assert_eq!(count.0, 1);
        drop_schema(&pool, &schema).await;
    }

    #[tokio::test]
    async fn sweep_with_empty_window_writes_no_events() {
        let Some((pool, schema)) = test_pool("tog_15759_inactivity_empty")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping inactivity_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        // Nobody is quiet: one recently active member and one bot. The legacy
        // `flagInactive` still selects zero rows and writes zero events — an
        // empty window is a successful no-op, not an error.
        seed_member(
            &pool,
            "active",
            "2026-07-01T00:00:00.000Z",
            Some("2026-09-06T00:00:00.000Z"),
            false,
            None,
        )
        .await;
        seed_member(&pool, "bot", "2026-07-01T00:00:00.000Z", None, true, None).await;
        let outcome = run_sweep(&pool, "2026-09-07T00:00:00.000Z", 14)
            .await
            .expect("empty sweep");
        assert!(outcome.flagged.is_empty());
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
            .fetch_one(&pool)
            .await
            .expect("event count");
        assert_eq!(count.0, 0);
        drop_schema(&pool, &schema).await;
    }

    #[tokio::test]
    async fn sweep_pages_past_a_suppressed_prefix_without_dropping_the_tail() {
        let Some((pool, schema)) = test_pool("inactivity_suppressed_prefix")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping inactivity_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        // A full page of suppressed-and-quiet members ahead of a writable tail.
        // Suppressed rows never leave the eligible set, so without the
        // select-time exclusion they would pin the LIMIT window and the sweep
        // would end without ever visiting the tail. `generate_series` keeps
        // the seed to two statements.
        sqlx::query(
            "INSERT INTO members (guild_id, member_id, joined_at)
             SELECT 'guild-a', 'member-' || g, '2026-07-01T00:00:00.000Z'::timestamptz
             FROM generate_series(1, 4196) g",
        )
        .execute(&pool)
        .await
        .expect("seed members");
        sqlx::query(
            "INSERT INTO privacy_opt_outs (guild_id, member_id)
             SELECT 'guild-a', 'member-' || g FROM generate_series(1, 4096) g",
        )
        .execute(&pool)
        .await
        .expect("seed opt-outs");
        let outcome = run_sweep(&pool, "2026-09-07T00:00:00.000Z", 14)
            .await
            .expect("sweep past suppressed prefix");
        assert_eq!(outcome.flagged.len(), 100);
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
            .fetch_one(&pool)
            .await
            .expect("event count");
        assert_eq!(count.0, 100);
        drop_schema(&pool, &schema).await;
    }

    #[tokio::test]
    async fn sweep_pages_past_a_lagging_projection_prefix_without_dropping_the_tail() {
        let Some((pool, schema)) = test_pool("inactivity_lagging_prefix")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping inactivity_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        // A full page whose events already exist but whose projections lag,
        // ahead of a 100-member writable tail. The existing-event arm reports
        // without touching projections, so without keyset progress the prefix
        // would reselect on every page and the sweep would end without ever
        // visiting the tail. `generate_series` keeps the seed to two
        // statements; the seeded keys match this sweep's idempotency shape.
        sqlx::query(
            "INSERT INTO members (guild_id, member_id, joined_at)
             SELECT 'guild-a', 'member-' || g, '2026-07-01T00:00:00.000Z'::timestamptz
             FROM generate_series(1, 4196) g",
        )
        .execute(&pool)
        .await
        .expect("seed members");
        sqlx::query(
            "INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
             SELECT 'member_inactive', 'member-' || g, 'guild-a',
                    '2026-09-07T00:00:00.000Z'::timestamptz, 'job:inactivity',
                    '{\"thresholdDays\":14}',
                    'guild-a:member-' || g || ':member_inactive:2026-09-07T00:00:00.000Z'
             FROM generate_series(1, 4096) g",
        )
        .execute(&pool)
        .await
        .expect("seed existing events");
        let outcome = run_sweep(&pool, "2026-09-07T00:00:00.000Z", 14)
            .await
            .expect("sweep past lagging prefix");
        assert_eq!(outcome.flagged.len(), 4196);
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
            .fetch_one(&pool)
            .await
            .expect("event count");
        assert_eq!(count.0, 4196);
        // Existing-event rows are reported but their projections stay
        // untouched; only the writable tail advances.
        let untouched: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM members WHERE inactive_flagged_at IS NULL")
                .fetch_one(&pool)
                .await
                .expect("untouched projection count");
        assert_eq!(untouched.0, 4096);
        drop_schema(&pool, &schema).await;
    }

    /// A zero page budget is rejected before any select: the pass reads
    /// nothing and writes nothing, so "at most max_pages selects" holds
    /// exactly. Offline (closed pool): any attempted I/O would surface as a
    /// `Db` error instead of `InvalidBudget`.
    #[tokio::test]
    async fn budgeted_pass_with_zero_max_pages_is_rejected_without_selects() {
        let pool = closed_lazy_pool().await;
        let budget = SweepBudget {
            max_pages: 0,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(60),
        };
        assert!(matches!(
            run_sweep_budgeted(&pool, "2026-09-07T00:00:00.000Z", 14, budget, None).await,
            Err(InactivityStoreError::InvalidBudget(_))
        ));
    }

    #[tokio::test]
    async fn budgeted_resume_reuses_the_originating_sweep_time() {
        let Some((pool, schema)) = test_pool("inactivity_resume_time")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping inactivity_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        let Some((fresh, fresh_schema)) = test_pool("inactivity_resume_full")
            .await
            .expect("test database setup")
        else {
            drop_schema(&pool, &schema).await;
            eprintln!("skipping inactivity_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        // 5,000 quiet members in both schemas: the budgeted sweep completes
        // across two passes while its resume call passes a *later* now. The
        // cursor pins the originating sweep time, so the union is
        // byte-identical to one complete sweep at that time — a resume that
        // re-derived its cutoff and stamps from the later now would drift.
        for target in [&pool, &fresh] {
            sqlx::query(
                "INSERT INTO members (guild_id, member_id, joined_at)
                 SELECT 'guild-a', 'member-' || g, '2026-07-01T00:00:00.000Z'::timestamptz
                 FROM generate_series(1, 5000) g",
            )
            .execute(target)
            .await
            .expect("seed members");
        }
        let one_page = SweepBudget {
            max_pages: 1,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(300),
        };
        let (first, cursor) =
            run_sweep_budgeted(&pool, "2026-09-07T00:00:00.000Z", 14, one_page, None)
                .await
                .expect("first budgeted pass");
        assert_eq!(first.flagged.len(), 4_096);
        let (rest, done) = run_sweep_budgeted(
            &pool,
            "2026-09-08T00:00:00.000Z",
            14,
            SweepBudget {
                max_pages: 10_000,
                deadline: std::time::Instant::now() + std::time::Duration::from_secs(300),
            },
            Some(cursor.expect("more work remains after page one")),
        )
        .await
        .expect("resume pass under a later now");
        assert!(done.is_none());
        let mut union = first.flagged;
        union.extend(rest.flagged);
        let full = run_sweep(&fresh, "2026-09-07T00:00:00.000Z", 14)
            .await
            .expect("complete sweep at the originating time");
        assert_eq!(union, full.flagged);
        assert_eq!(union.len(), 5_000);
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
            .fetch_one(&pool)
            .await
            .expect("event count");
        assert_eq!(count.0, 5_000);
        drop_schema(&pool, &schema).await;
        drop_schema(&fresh, &fresh_schema).await;
    }

    #[tokio::test]
    async fn budgeted_resume_pages_past_a_lagging_prefix_to_the_tail() {
        let Some((pool, schema)) = test_pool("inactivity_budget_lagging")
            .await
            .expect("test database setup")
        else {
            eprintln!("skipping inactivity_store test: TWO_TEST_DATABASE_URL not set");
            return;
        };
        // Same shape as the complete-sweep lagging-prefix regression: a full
        // page whose events already exist but whose projections lag, ahead of
        // a 100-member writable tail. The prefix rows stay eligible forever,
        // so only the threaded resume cursor moves the window forward — a
        // pass that restarted from the head would reselect the prefix and
        // never reach the tail.
        sqlx::query(
            "INSERT INTO members (guild_id, member_id, joined_at)
             SELECT 'guild-a', 'member-' || g, '2026-07-01T00:00:00.000Z'::timestamptz
             FROM generate_series(1, 4196) g",
        )
        .execute(&pool)
        .await
        .expect("seed members");
        sqlx::query(
            "INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
             SELECT 'member_inactive', 'member-' || g, 'guild-a',
                    '2026-09-07T00:00:00.000Z'::timestamptz, 'job:inactivity',
                    '{\"thresholdDays\":14}',
                    'guild-a:member-' || g || ':member_inactive:2026-09-07T00:00:00.000Z'
             FROM generate_series(1, 4096) g",
        )
        .execute(&pool)
        .await
        .expect("seed existing events");
        let budget = SweepBudget {
            max_pages: 1,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(300),
        };
        let mut union = Vec::new();
        let mut resume = None;
        let mut passes = 0;
        loop {
            passes += 1;
            assert!(passes <= 4, "budgeted resume did not reach the tail");
            let (out, next) =
                run_sweep_budgeted(&pool, "2026-09-07T00:00:00.000Z", 14, budget, resume)
                    .await
                    .expect("budgeted pass");
            union.extend(out.flagged);
            match next {
                Some(cursor) => resume = Some(cursor),
                None => break,
            }
        }
        assert_eq!(passes, 2, "one page per pass over prefix plus tail");
        assert_eq!(union.len(), 4_196);
        assert!(
            union.iter().any(|f| f.member_id == "member-4196"),
            "resume reached the writable tail"
        );
        // Prefix rows are reported without new writes; only the tail inserts.
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
            .fetch_one(&pool)
            .await
            .expect("event count");
        assert_eq!(count.0, 4_196);
        drop_schema(&pool, &schema).await;
    }
}
