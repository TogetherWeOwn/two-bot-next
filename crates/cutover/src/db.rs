//! sqlx seam for the cutover tools.
//!
//! Ports the `openDb` + `EventStore` + `LevelingService` write paths the
//! seven CLIs need. Pool max 5 and a 15s statement timeout mirror legacy
//! (`src/store/postgresDriver.ts`). Migrations apply unless skipped — the
//! rewards probe opens with `skip_migrations` so a read-only probe cannot
//! build schema on a database it was pointed at by accident.
//!
//! Event semantics ported verbatim from `src/store/eventStore.ts`:
//! idempotency-key arbitration via `ON CONFLICT DO NOTHING`, earliest-wins
//! `record_earliest`, forward-only `touch_activity`, and the members
//! projection guards.

use sqlx::postgres::PgPoolOptions;
use sqlx::{Pool, Postgres};
use two_bot_core::database_tls::{self, TlsPolicy};

/// Legacy pool default (`TWO_DB_POOL_MAX ?? 5`).
pub const DB_POOL_MAX_DEFAULT: u32 = 5;
/// Legacy statement timeout (`statementTimeoutMillis ?? 15_000`).
pub const STATEMENT_TIMEOUT_MS: u64 = 15_000;
/// Acquire bound for cutover pool opens: a hung database refuses within 10 s
/// instead of hanging on the 30 s SQLx default. Voice inherits it through the
/// shared `pool_options` constructor.
pub const ACQUIRE_TIMEOUT_SECS: u64 = 10;

/// Open the Postgres pool and apply pending migrations (unless skipped),
/// under the `TWO_DATABASE_TLS` policy: unset means `required`.
pub async fn connect(
    url: &str,
    pool_max: u32,
    skip_migrations: bool,
) -> Result<CutoverDb, sqlx::Error> {
    connect_with_tls(url, pool_max, skip_migrations, tls_policy_from_env()?).await
}

/// Read the one TLS policy setting; an unset value is `Required`.
fn tls_policy_from_env() -> Result<TlsPolicy, sqlx::Error> {
    let value = std::env::var_os(database_tls::POLICY_SETTING);
    // A non-UTF-8 value parses as "" and is refused like any unknown value.
    TlsPolicy::from_setting(value.as_ref().map(|v| v.to_str().unwrap_or("")))
        .map_err(|message| sqlx::Error::InvalidArgument(message.to_owned()))
}

fn pool_options(pool_max: u32) -> PgPoolOptions {
    PgPoolOptions::new()
        .max_connections(pool_max)
        .acquire_timeout(std::time::Duration::from_secs(ACQUIRE_TIMEOUT_SECS))
}

fn voice_pool_options(pool_max: u32) -> PgPoolOptions {
    pool_options(pool_max)
        .min_connections(1)
        // SQLx 0.9 reaps idle connections even at the minimum, then reconnects.
        // Disable that churn for voice; retain the normal max-lifetime rotation.
        .idle_timeout(None)
        // A warm socket may have died during a DB/proxy restart. Keep the
        // liveness round trip so acquire replaces it before a room transaction.
        .test_before_acquire(true)
}

/// Open the latency-sensitive voice pool with one warm connection, using the
/// same URL validation, TLS policy and migration behavior as [`connect`].
/// Other runtime and CLI pools deliberately retain their existing defaults.
pub async fn connect_voice(
    url: &str,
    pool_max: u32,
    skip_migrations: bool,
) -> Result<CutoverDb, sqlx::Error> {
    connect_with_pool_options(
        url,
        voice_pool_options(pool_max),
        skip_migrations,
        tls_policy_from_env()?,
    )
    .await
}

/// [`connect`] with an explicit TLS policy (tests pass `LocalOnly`).
pub async fn connect_with_tls(
    url: &str,
    pool_max: u32,
    skip_migrations: bool,
    tls: TlsPolicy,
) -> Result<CutoverDb, sqlx::Error> {
    connect_with_pool_options(url, pool_options(pool_max), skip_migrations, tls).await
}

async fn connect_with_pool_options(
    url: &str,
    pool_options: PgPoolOptions,
    skip_migrations: bool,
    tls: TlsPolicy,
) -> Result<CutoverDb, sqlx::Error> {
    if url.trim().is_empty() {
        return Err(sqlx::Error::InvalidArgument(
            "database URL is required".to_owned(),
        ));
    }
    if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
        return Err(sqlx::Error::InvalidArgument(
            "only Postgres is supported: database URL must use postgres:// or postgresql://"
                .to_owned(),
        ));
    }
    two_bot_core::database_url::validate(url)
        .map_err(|message| sqlx::Error::InvalidArgument(message.to_owned()))?;
    // Threat-model F6: refuse plaintext/unverified modes and the wrong host
    // class before SQLx parses the URL (see `docs/database-tls.md`).
    database_tls::enforce(url, tls)
        .map_err(|message| sqlx::Error::InvalidArgument(message.to_owned()))?;
    // Passfile diagnostics stay suppressed during the synchronous parse, but a
    // well-formed entry still supplies the password (see `database_url`).
    let mut options = database_tls::apply(two_bot_core::database_url::connect_options(url)?, tls);
    // Statement timeout rides the connection options (server-side setting
    // per connection), so no per-connection SET is needed.
    options = options.options([("statement_timeout", format!("{}ms", STATEMENT_TIMEOUT_MS))]);
    let pool = pool_options
        .connect_with(options)
        .await
        .map_err(|_| sqlx::Error::InvalidArgument("database connection failed".to_owned()))?;
    let db = CutoverDb { pool };
    if !skip_migrations {
        if let Err(e) = db.migrate().await {
            db.close().await;
            return Err(e);
        }
    }
    Ok(db)
}

#[cfg(test)]
mod pool_options_tests {
    use super::*;

    #[test]
    fn voice_pool_keeps_a_warm_connection_and_checks_liveness() {
        let options = voice_pool_options(DB_POOL_MAX_DEFAULT);
        assert_eq!(options.get_min_connections(), 1);
        assert_eq!(options.get_max_connections(), DB_POOL_MAX_DEFAULT);
        assert_eq!(options.get_idle_timeout(), None);
        assert!(options.get_test_before_acquire());
        assert_eq!(
            options.get_max_lifetime(),
            PgPoolOptions::new().get_max_lifetime()
        );
        assert_eq!(
            options.get_acquire_timeout(),
            std::time::Duration::from_secs(ACQUIRE_TIMEOUT_SECS)
        );
    }

    #[tokio::test]
    async fn voice_pool_recovers_a_terminated_idle_connection(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let url = match std::env::var("TWO_TEST_DATABASE_URL") {
            Ok(url) => url,
            Err(std::env::VarError::NotPresent) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let fixture =
            two_bot_testsupport::TestDatabase::create(&url, &sqlx::migrate!("./migrations"))
                .await?;
        // Use the real voice profile but retain the fixture's guarded target,
        // empty password, disabled passfile fallback and 5s statement timeout.
        let pool = voice_pool_options(1)
            .connect_with(fixture.pool().connect_options().as_ref().clone())
            .await;
        let pool = match pool {
            Ok(pool) => pool,
            Err(error) => {
                fixture.close().await?;
                return Err(error.into());
            }
        };
        let warmed = pool.size();
        let result = async {
            let mut connection = pool.acquire().await?;
            let original: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *connection)
                .await?;
            drop(connection);
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while pool.num_idle() != 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            // Kill only the identified connection inside this fixture's owned
            // database, waiting for exit before testing the very next acquire.
            let terminated: bool = sqlx::query_scalar(
                "SELECT pg_terminate_backend(pid, 5000) FROM pg_stat_activity
                 WHERE pid = $1 AND datname = current_database()",
            )
            .bind(original)
            .fetch_one(fixture.pool())
            .await?;
            let (replacement, value): (i32, i32) = sqlx::query_as("SELECT pg_backend_pid(), 42")
                .fetch_one(&pool)
                .await?;
            Ok::<_, Box<dyn std::error::Error>>((terminated, original, replacement, value))
        }
        .await;
        pool.close().await;
        let cleanup = fixture.close().await;
        let (terminated, original, replacement, value) = result?;
        cleanup?;
        assert_eq!(warmed, 1, "voice pool is warm before its first query");
        assert!(terminated);
        assert_ne!(original, replacement);
        assert_eq!(
            value, 42,
            "first query succeeds without an application retry"
        );
        Ok(())
    }

    #[test]
    fn nonvoice_pool_overrides_only_the_documented_acquire_bound() {
        let options = pool_options(DB_POOL_MAX_DEFAULT);
        let defaults = PgPoolOptions::new();
        assert_eq!(options.get_max_connections(), DB_POOL_MAX_DEFAULT);
        assert_eq!(
            options.get_min_connections(),
            defaults.get_min_connections()
        );
        assert_eq!(options.get_idle_timeout(), defaults.get_idle_timeout());
        assert_eq!(options.get_max_lifetime(), defaults.get_max_lifetime());
        assert_eq!(
            options.get_acquire_timeout(),
            std::time::Duration::from_secs(ACQUIRE_TIMEOUT_SECS)
        );
        assert_eq!(
            options.get_test_before_acquire(),
            defaults.get_test_before_acquire()
        );
    }
}

/// Apply the same embedded migrations to an already-authorized pool.
/// Scratch drills supply explicit test-only options without credential fallback.
pub async fn migrate_pool(pool: &Pool<Postgres>) -> Result<(), sqlx::Error> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .map_err(|_| sqlx::Error::InvalidArgument("database migration failed".to_owned()))
}

/// Cutover database handle.
#[derive(Debug, Clone)]
pub struct CutoverDb {
    pool: Pool<Postgres>,
}

impl CutoverDb {
    /// Wrap an already-open pool (gateway runtimes own their pool; the
    /// privacy service borrows it for one call, no new connections).
    #[must_use]
    pub fn from_pool(pool: Pool<Postgres>) -> Self {
        Self { pool }
    }

    #[must_use]
    pub fn pool(&self) -> &Pool<Postgres> {
        &self.pool
    }

    /// Apply the crate's embedded migrations.
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        migrate_pool(&self.pool).await
    }

    /// Close idle connections (drains the pool).
    pub async fn close(self) {
        self.pool.close().await;
    }
}

/// One funnel event write (subset of legacy `FunnelEvent` the tools emit).
#[derive(Debug, Clone)]
pub struct FunnelWrite {
    pub member_id: Option<String>,
    pub guild_id: String,
    pub event_type: String,
    pub occurred_at: String,
    pub source: String,
    pub metadata: Option<String>,
}

/// Legacy idempotency keys (`idempotencyKey()` in `src/core/events.ts`):
/// repeatable event types key on member+time; once-per-member milestones key
/// on member alone.
fn idempotency_key(e: &FunnelWrite) -> String {
    const REPEATABLE: [&str; 8] = [
        "invite_click",
        "member_join",
        "member_inactive",
        "member_leave",
        "game_roles_selected",
        "channel_routed",
        "voice_session_start",
        "voice_session_end",
    ];
    if REPEATABLE.contains(&e.event_type.as_str()) {
        format!(
            "{}:{}:{}:{}",
            e.guild_id,
            e.member_id.as_deref().unwrap_or("anon"),
            e.event_type,
            e.occurred_at
        )
    } else {
        format!(
            "{}:{}:{}",
            e.guild_id,
            e.member_id.as_deref().unwrap_or("anon"),
            e.event_type
        )
    }
}

/// Insert-first arbitration: the unique index decides the winner atomically,
/// then the members projection is applied for the winner only.
///
/// The privacy gate runs first, inside the same transaction: a suppressed
/// `(guild_id, member_id)` pair or a guild-disabled analytics family drops
/// the write as `(false, None)` with no event row and no members projection.
/// Anonymous writes (`member_id = None`) are never gated.
pub async fn record_event(
    db: &CutoverDb,
    e: &FunnelWrite,
) -> Result<(bool, Option<i64>), sqlx::Error> {
    let key = idempotency_key(e);
    let mut tx = db.pool.begin().await?;
    // Serialize with concurrent opt-outs (which take SHARE ROW EXCLUSIVE
    // over these tables): whoever locks first wins, so a write that passes
    // the gate can never land after the purge. Lock order matches
    // `opt_out_member`'s sorted order, and ROW EXCLUSIVE never blocks other
    // writers.
    sqlx::query("LOCK TABLE events, members, privacy_opt_outs IN ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    if two_bot_core::privacy::ingestion_blocked_in(
        &mut tx,
        &e.guild_id,
        e.member_id.as_deref(),
        &e.event_type,
    )
    .await?
    {
        tx.rollback().await?;
        return Ok((false, None));
    }
    // A suppressed inviter's attribution never re-enters through a join they
    // did not perform: strip `inviterId` from `member_join` metadata when the
    // inviter is suppressed, keeping the joiner's own row (and every other
    // metadata key) intact.
    let mut metadata = e.metadata.clone();
    if e.event_type == "member_join" {
        if let Some(inviter) = two_bot_core::privacy::inviter_in_metadata(metadata.as_deref()) {
            if two_bot_core::privacy::suppressed_in(&mut tx, &e.guild_id, &inviter).await? {
                metadata = two_bot_core::privacy::scrubbed_member_join_metadata(
                    metadata.as_deref(),
                    &inviter,
                );
            }
        }
    }
    let inserted: Option<(i64,)> = sqlx::query_as(
        "INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
         VALUES ($1, $2, $3, $4::timestamptz, $5, $6, $7)
         ON CONFLICT (idempotency_key) DO NOTHING RETURNING id",
    )
    .bind(&e.event_type)
    .bind(&e.member_id)
    .bind(&e.guild_id)
    .bind(&e.occurred_at)
    .bind(&e.source)
    .bind(&metadata)
    .bind(&key)
    .fetch_optional(&mut *tx)
    .await?;
    match inserted {
        None => {
            let existing: Option<(i64,)> =
                sqlx::query_as("SELECT id FROM events WHERE idempotency_key = $1")
                    .bind(&key)
                    .fetch_optional(&mut *tx)
                    .await?;
            tx.commit().await?;
            Ok((false, existing.map(|(id,)| id)))
        }
        Some((id,)) => {
            project_event(&mut tx, e).await?;
            tx.commit().await?;
            Ok((true, Some(id)))
        }
    }
}

/// Members projection guards (legacy `EventStore.project`): joins overwrite
/// the arrival columns but clear `left_at`; milestone columns are
/// earliest/first-wins; recency only ever moves forward.
pub(crate) async fn project_event(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    e: &FunnelWrite,
) -> Result<(), sqlx::Error> {
    let Some(member_id) = e.member_id.as_deref() else {
        return Ok(());
    };
    sqlx::query("INSERT INTO members (guild_id, member_id) VALUES ($1, $2) ON CONFLICT (guild_id, member_id) DO NOTHING")
        .bind(&e.guild_id)
        .bind(member_id)
        .execute(&mut **tx)
        .await?;

    match e.event_type.as_str() {
        "member_join" => {
            sqlx::query("UPDATE members SET joined_at = $1::timestamptz WHERE guild_id = $2 AND member_id = $3")
                .bind(&e.occurred_at)
                .bind(&e.guild_id)
                .bind(member_id)
                .execute(&mut **tx)
                .await?;
            sqlx::query(
                "UPDATE members SET join_source = $1 WHERE guild_id = $2 AND member_id = $3",
            )
            .bind(&e.source)
            .bind(&e.guild_id)
            .bind(member_id)
            .execute(&mut **tx)
            .await?;
            sqlx::query("UPDATE members SET left_at = NULL WHERE guild_id = $1 AND member_id = $2")
                .bind(&e.guild_id)
                .bind(member_id)
                .execute(&mut **tx)
                .await?;
            sqlx::query("UPDATE members SET inactive_flagged_at = NULL WHERE guild_id = $1 AND member_id = $2")
                .bind(&e.guild_id).bind(member_id).execute(&mut **tx).await?;
        }
        // Earliest wins: a rejoin re-screens, but conversion counts the first.
        "gate_cleared" => {
            sqlx::query("UPDATE members SET gate_cleared_at = $1::timestamptz WHERE guild_id = $2 AND member_id = $3 AND gate_cleared_at IS NULL")
                .bind(&e.occurred_at).bind(&e.guild_id).bind(member_id).execute(&mut **tx).await?;
        }
        "first_message" => {
            sqlx::query("UPDATE members SET first_message_at = $1::timestamptz WHERE guild_id = $2 AND member_id = $3 AND first_message_at IS NULL")
                .bind(&e.occurred_at).bind(&e.guild_id).bind(member_id).execute(&mut **tx).await?;
            advance_activity(tx, &e.guild_id, member_id, &e.occurred_at).await?;
        }
        // `second_message` is a rung marker in the log only: no column, but
        // recency still advances.
        "second_message" | "third_message" | "first_voice_session" => {
            if e.event_type == "third_message" {
                sqlx::query("UPDATE members SET third_message_at = $1::timestamptz WHERE guild_id = $2 AND member_id = $3 AND third_message_at IS NULL")
                    .bind(&e.occurred_at).bind(&e.guild_id).bind(member_id).execute(&mut **tx).await?;
            }
            if e.event_type == "first_voice_session" {
                sqlx::query("UPDATE members SET first_voice_at = $1::timestamptz WHERE guild_id = $2 AND member_id = $3 AND first_voice_at IS NULL")
                    .bind(&e.occurred_at).bind(&e.guild_id).bind(member_id).execute(&mut **tx).await?;
            }
            advance_activity(tx, &e.guild_id, member_id, &e.occurred_at).await?;
        }
        "member_inactive" => {
            sqlx::query("UPDATE members SET inactive_flagged_at = $1::timestamptz WHERE guild_id = $2 AND member_id = $3")
                .bind(&e.occurred_at).bind(&e.guild_id).bind(member_id).execute(&mut **tx).await?;
        }
        "member_leave" => {
            sqlx::query("UPDATE members SET left_at = $1::timestamptz WHERE guild_id = $2 AND member_id = $3")
                .bind(&e.occurred_at)
                .bind(&e.guild_id)
                .bind(member_id)
                .execute(&mut **tx)
                .await?;
        }
        _ => {}
    }
    Ok(())
}

/// Recency never moves backwards (legacy `advance`).
pub(crate) async fn advance_activity(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    guild_id: &str,
    member_id: &str,
    at: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE members SET last_active_at = $1::timestamptz WHERE guild_id = $2 AND member_id = $3
          AND (last_active_at IS NULL OR last_active_at < $4::timestamptz)",
    )
    .bind(at)
    .bind(guild_id)
    .bind(member_id)
    .bind(at)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Bump activity without emitting an event; forward-only (legacy
/// `touchActivity`). A suppressed pair is a no-op: the bump would create or
/// refresh a per-user `members` row for a member who opted out. The gate
/// runs inside the write transaction under the shared table lock, so a bump
/// that passes the gate can never land after a concurrent opt-out purge.
pub async fn touch_activity(
    db: &CutoverDb,
    guild_id: &str,
    member_id: &str,
    at: &str,
) -> Result<(), sqlx::Error> {
    let mut tx = db.pool().begin().await?;
    // Lock order matches `opt_out_member`'s sorted order; ROW EXCLUSIVE
    // never blocks other writers.
    sqlx::query("LOCK TABLE members, privacy_opt_outs IN ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    if two_bot_core::privacy::suppressed_in(&mut tx, guild_id, member_id).await? {
        tx.rollback().await?;
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO members (guild_id, member_id, last_active_at) VALUES ($1, $2, $3::timestamptz)
         ON CONFLICT (guild_id, member_id) DO UPDATE SET last_active_at = excluded.last_active_at
           WHERE members.last_active_at IS NULL OR members.last_active_at < excluded.last_active_at",
    )
    .bind(guild_id)
    .bind(member_id)
    .bind(at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Mark a member row as a bot (legacy `markBot`). A suppressed pair is a
/// no-op: the flag write would create a per-user `members` row for a member
/// who opted out. Like [`touch_activity`], the gate runs under the shared
/// table lock so a concurrent opt-out purge cannot slip between gate and
/// write.
pub async fn mark_bot(db: &CutoverDb, guild_id: &str, member_id: &str) -> Result<(), sqlx::Error> {
    let mut tx = db.pool().begin().await?;
    sqlx::query("LOCK TABLE members, privacy_opt_outs IN ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    if two_bot_core::privacy::suppressed_in(&mut tx, guild_id, member_id).await? {
        tx.rollback().await?;
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO members (guild_id, member_id, is_bot) VALUES ($1, $2, TRUE)
         ON CONFLICT (guild_id, member_id) DO UPDATE SET is_bot = TRUE",
    )
    .bind(guild_id)
    .bind(member_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Once-per-member milestone keeping the EARLIEST known time: backfill can
/// discover a message older than one already on file, and the older
/// timestamp is the truth (legacy `recordEarliest`).
pub async fn record_earliest(
    db: &CutoverDb,
    e: &FunnelWrite,
) -> Result<(bool, Option<i64>), sqlx::Error> {
    // Gate before the insert AND before the pull-back below: for a
    // suppressed pair or a disabled family even reconfirming an existing row
    // would refresh per-user state, so the whole call is a no-op.
    if two_bot_core::privacy::ingestion_blocked(
        db.pool(),
        &e.guild_id,
        e.member_id.as_deref(),
        &e.event_type,
    )
    .await?
    {
        return Ok((false, None));
    }
    let (inserted, id) = record_event(db, e).await?;
    if inserted {
        return Ok((true, id));
    }
    let key = idempotency_key(e);
    let mut tx = db.pool.begin().await?;
    // Rendered as UTC ISO text (legacy 0009 contract): the Rust side
    // compares and stores ISO-8601 strings throughout.
    let existing: Option<(i64, String)> =
        sqlx::query_as("SELECT id, to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') FROM events WHERE idempotency_key = $1 FOR UPDATE")
            .bind(&key)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((id, current_at)) = existing else {
        tx.commit().await?;
        return Ok((false, None));
    };
    // ISO-8601 UTC compares lexicographically (legacy string compare).
    if e.occurred_at >= current_at {
        tx.commit().await?;
        return Ok((false, Some(id)));
    }
    sqlx::query(
        "UPDATE events SET occurred_at = $1::timestamptz, source = $2, metadata = $3 WHERE id = $4",
    )
    .bind(&e.occurred_at)
    .bind(&e.source)
    .bind(&e.metadata)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    // The column name comes from a fixed 3-way match over event types, so
    // each branch is a static string (sqlx 0.9 audits dynamic SQL).
    // `$4` binds ISO-8601 text and must cast: without it Postgres refuses
    // `timestamptz > text`, so the pull-back errors instead of moving the
    // milestone earlier.
    if let Some(member_id) = e.member_id.as_deref() {
        let pull_back = match e.event_type.as_str() {
            "first_message" => Some(
                "UPDATE members SET first_message_at = $1::timestamptz WHERE guild_id = $2 AND member_id = $3 AND (first_message_at IS NULL OR first_message_at > $4::timestamptz)",
            ),
            "third_message" => Some(
                "UPDATE members SET third_message_at = $1::timestamptz WHERE guild_id = $2 AND member_id = $3 AND (third_message_at IS NULL OR third_message_at > $4::timestamptz)",
            ),
            "first_voice_session" => Some(
                "UPDATE members SET first_voice_at = $1::timestamptz WHERE guild_id = $2 AND member_id = $3 AND (first_voice_at IS NULL OR first_voice_at > $4::timestamptz)",
            ),
            _ => None,
        };
        if let Some(q) = pull_back {
            sqlx::query(q)
                .bind(&e.occurred_at)
                .bind(&e.guild_id)
                .bind(member_id)
                .bind(&e.occurred_at)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    Ok((false, Some(id)))
}

/// Current stored rewards, ascending by level (legacy `roleRewards`).
pub async fn role_rewards(
    db: &CutoverDb,
    guild_id: &str,
) -> Result<Vec<crate::LevelRoleReward>, sqlx::Error> {
    // `level` is INTEGER (INT4) per the legacy DDL; decode as i32.
    let rows: Vec<(i32, String)> = sqlx::query_as(
        "SELECT level, role_id FROM level_role_rewards WHERE guild_id = $1 ORDER BY level ASC",
    )
    .bind(guild_id)
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|(level, role_id)| crate::LevelRoleReward {
            level: level as u64,
            role_id,
        })
        .collect())
}

/// Replace the full reward configuration atomically: last row per level wins
/// in the input, then delete-all + insert in one transaction (legacy
/// `replaceRoleRewards`).
///
/// Takes the same per-guild advisory lock as the runtime writer
/// (`two-bot-core::leveling_store::replace_role_rewards`) with the same key:
/// with an empty ladder two concurrent replacements otherwise both finish
/// `DELETE` before either `INSERT`s and commit the union of two independent
/// configurations (TOG-10359). Keep both key strings in sync.
pub async fn replace_role_rewards(
    db: &CutoverDb,
    guild_id: &str,
    rewards: &[crate::LevelRoleReward],
) -> Result<(), ReplaceRewardsError> {
    let normalized = normalize_role_rewards(rewards)?;
    let mut tx = db.pool.begin().await.map_err(ReplaceRewardsError::Db)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("{guild_id}:level_role_rewards"))
        .execute(&mut *tx)
        .await
        .map_err(ReplaceRewardsError::Db)?;
    sqlx::query("DELETE FROM level_role_rewards WHERE guild_id = $1")
        .bind(guild_id)
        .execute(&mut *tx)
        .await
        .map_err(ReplaceRewardsError::Db)?;
    for (level, role_id) in normalized {
        sqlx::query(
            "INSERT INTO level_role_rewards (guild_id, level, role_id) VALUES ($1, $2, $3)",
        )
        .bind(guild_id)
        .bind(level)
        .bind(role_id)
        .execute(&mut *tx)
        .await
        .map_err(ReplaceRewardsError::Db)?;
    }
    tx.commit().await.map_err(ReplaceRewardsError::Db)?;
    Ok(())
}

/// Validate every input row against the INT4 storage domain, then keep the
/// last role per level and require unique roles in that final ladder. Pure:
/// callers can reject invalid replacements before opening a database.
pub fn normalize_role_rewards(
    rewards: &[crate::LevelRoleReward],
) -> Result<std::collections::BTreeMap<i32, &str>, ReplaceRewardsError> {
    use std::collections::{BTreeMap, HashSet};
    let mut normalized = BTreeMap::new();
    for r in rewards {
        let level = i32::try_from(r.level)
            .ok()
            .filter(|level| *level > 0)
            .ok_or_else(|| {
                ReplaceRewardsError::Invalid(format!(
                    "reward level must be between 1 and {}",
                    i32::MAX
                ))
            })?;
        if !crate::is_snowflake(&r.role_id) {
            return Err(ReplaceRewardsError::Invalid(format!(
                "invalid Discord role id: {}",
                r.role_id
            )));
        }
        normalized.insert(level, r.role_id.as_str());
    }
    let mut roles = HashSet::new();
    for role_id in normalized.values() {
        if !roles.insert(role_id.trim_start_matches('0')) {
            return Err(ReplaceRewardsError::Invalid(format!(
                "duplicate Discord role id in reward ladder: {role_id}"
            )));
        }
    }
    Ok(normalized)
}

/// Reward-replacement failure: invalid input vs database error.
#[derive(Debug, thiserror::Error)]
pub enum ReplaceRewardsError {
    #[error("{0}")]
    Invalid(String),
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// Message-milestone write idempotency (TOG-15759): a second write pass over
/// the same ladder inserts 0 events. Runs in the existing `check.yml`
/// "cutover reward ladder lib regression" step (same target, same guard).
#[cfg(test)]
mod message_scan_write_tests {
    use super::*;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    const MESSAGE_RUNGS: [&str; 3] = ["first_message", "second_message", "third_message"];

    fn ladder_write(member: &str, rung: usize, at: &str) -> FunnelWrite {
        FunnelWrite {
            member_id: Some(member.to_owned()),
            guild_id: "100000000000000010".to_owned(),
            event_type: MESSAGE_RUNGS[rung].to_owned(),
            occurred_at: at.to_owned(),
            source: "channel:999000000000000001".to_owned(),
            metadata: Some(r#"{"backfill":"message_scan"}"#.to_owned()),
        }
    }

    #[tokio::test]
    #[ignore = "requires agent-testdb or the CI Postgres service"]
    async fn second_write_pass_over_same_ladder_inserts_zero_events(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let host = if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") {
            "127.0.0.1"
        } else {
            "agent-testdb"
        };
        let options = PgConnectOptions::new()
            .host(host)
            .port(5432)
            .username("agent_test")
            .password("")
            .database("postgres")
            .ssl_mode(PgSslMode::Disable);
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options.clone())
            .await?;
        let schema = format!(
            "msgladder_test_{}_{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        );
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await?;
        let path = schema.clone();
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(5))
            .after_connect(move |conn, _| {
                let path = path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(path)
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect_with(options)
            .await?;
        let db = CutoverDb { pool };
        let result = async {
            sqlx::raw_sql(include_str!("../migrations/0001_funnel.sql"))
                .execute(db.pool())
                .await?;
            // The ladder writer consults the opt-out suppression table.
            sqlx::raw_sql(include_str!("../migrations/0433_privacy_opt_out.sql"))
                .execute(db.pool())
                .await?;
            let member = "100000000000000001";
            let times = [
                "2026-03-01T00:00:00.000Z",
                "2026-03-02T00:00:00.000Z",
                "2026-03-03T00:00:00.000Z",
            ];
            // First pass over a full three-rung ladder writes three events.
            let mut first_written = 0;
            for (i, at) in times.iter().enumerate() {
                let (is_new, _) = record_earliest(&db, &ladder_write(member, i, at)).await?;
                if is_new {
                    first_written += 1;
                }
            }
            assert_eq!(first_written, 3);
            // A pure repeat of the same ladder is a no-op: 0 new events, and
            // the stored rung times are untouched (record_earliest only moves
            // earlier, never later).
            let mut second_written = 0;
            for (i, at) in times.iter().enumerate() {
                let (is_new, _) = record_earliest(&db, &ladder_write(member, i, at)).await?;
                if is_new {
                    second_written += 1;
                }
            }
            assert_eq!(second_written, 0, "re-run writes 0 and is a no-op");
            let count: (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM events WHERE member_id = $1")
                    .bind(member)
                    .fetch_one(db.pool())
                    .await?;
            assert_eq!(count.0, 3);
            // An older re-scan of the same rung pulls the milestone back
            // without inserting (record_earliest semantics, not record).
            let (is_new, _) = record_earliest(
                &db,
                &ladder_write(member, 0, "2026-02-28T00:00:00.000Z"),
            )
            .await?;
            assert!(!is_new);
            let stored: (String,) = sqlx::query_as(
                "SELECT to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') FROM events WHERE idempotency_key = $1",
            )
            .bind("100000000000000010:100000000000000001:first_message")
            .fetch_one(db.pool())
            .await?;
            assert_eq!(stored.0, "2026-02-28T00:00:00.000Z");
            let count: (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM events WHERE member_id = $1")
                    .bind(member)
                    .fetch_one(db.pool())
                    .await?;
            assert_eq!(count.0, 3, "earlier re-scan pulls back, never inserts");
            Ok::<_, Box<dyn std::error::Error>>(())
        }
        .await;
        db.close().await;
        // Only this generated schema is disposable; preserve all shared data.
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await?;
        admin.close().await;
        result
    }
}

#[cfg(test)]
mod reward_tests {
    use super::*;
    use crate::LevelRoleReward;
    use sqlx::postgres::{PgConnectOptions, PgSslMode};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    const ROLE_A: &str = "90000000000000002";
    const ROLE_B: &str = "90000000000000003";

    fn reward(level: u64, role_id: &str) -> LevelRoleReward {
        LevelRoleReward {
            level,
            role_id: role_id.to_owned(),
        }
    }

    fn invalid(rewards: &[LevelRoleReward]) {
        assert!(matches!(
            normalize_role_rewards(rewards),
            Err(ReplaceRewardsError::Invalid(_))
        ));
    }

    #[test]
    fn reward_levels_match_positive_int4_storage() {
        let rewards = [reward(1, ROLE_A), reward(i32::MAX as u64, ROLE_B)];
        assert_eq!(
            normalize_role_rewards(&rewards)
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            vec![(1, ROLE_A), (i32::MAX, ROLE_B)]
        );
        for level in [0, i32::MAX as u64 + 1, i64::MAX as u64, u64::MAX] {
            invalid(&[reward(level, ROLE_A)]);
        }
        invalid(&[reward(1, "bad"), reward(1, ROLE_A)]);
        invalid(&[reward(u64::MAX, ROLE_A), reward(1, ROLE_B)]);
    }

    #[test]
    fn reward_role_uniqueness_ignores_leading_zero_aliases() {
        invalid(&[reward(1, ROLE_A), reward(2, "090000000000000002")]);
    }

    #[test]
    fn reward_role_uniqueness_is_checked_after_last_level_wins() {
        let rewards = [reward(2, ROLE_A), reward(1, ROLE_A), reward(1, ROLE_B)];
        assert_eq!(
            normalize_role_rewards(&rewards)
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            vec![(1, ROLE_B), (2, ROLE_A)]
        );
        assert!(normalize_role_rewards(&[]).unwrap().is_empty());
        assert_eq!(
            normalize_role_rewards(&[reward(1, ROLE_A), reward(1, ROLE_A)])
                .unwrap()
                .len(),
            1
        );
        invalid(&[reward(1, ROLE_A), reward(2, ROLE_B), reward(2, ROLE_A)]);
    }

    #[tokio::test]
    async fn invalid_rewards_do_not_acquire_a_connection() {
        let pool = PgPoolOptions::new().connect_lazy_with(
            PgConnectOptions::new()
                .host("agent-testdb")
                .username("agent_test")
                .password(""),
        );
        pool.close().await;
        let db = CutoverDb { pool };
        for rewards in [
            vec![reward(u64::MAX, ROLE_A)],
            vec![reward(1, ROLE_A), reward(2, ROLE_A)],
        ] {
            assert!(matches!(
                replace_role_rewards(&db, "g1", &rewards).await,
                Err(ReplaceRewardsError::Invalid(_))
            ));
        }
        assert!(matches!(
            replace_role_rewards(&db, "g1", &[]).await,
            Err(ReplaceRewardsError::Db(sqlx::Error::PoolClosed))
        ));
    }

    #[tokio::test]
    #[ignore = "requires agent-testdb or the CI Postgres service"]
    async fn reward_replacement_preserves_ladder_on_invalid_input(
    ) -> Result<(), Box<dyn std::error::Error>> {
        // No inherited app URL or credentials; only the disposable test service.
        let host = if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") {
            "127.0.0.1"
        } else {
            "agent-testdb"
        };
        let options = PgConnectOptions::new()
            .host(host)
            .port(5432)
            .username("agent_test")
            .password("")
            .database("postgres")
            .ssl_mode(PgSslMode::Disable);
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options.clone())
            .await?;
        let schema = format!(
            "reward_test_{}_{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        );
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await?;
        let path = schema.clone();
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(5))
            .after_connect(move |conn, _| {
                let path = path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(path)
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect_with(options)
            .await?;
        let db = CutoverDb { pool };
        let result = async {
            sqlx::raw_sql(include_str!("../migrations/0002_leveling.sql"))
                .execute(db.pool())
                .await?;
            let original = vec![reward(5, ROLE_A)];
            replace_role_rewards(&db, "g1", &original).await?;
            replace_role_rewards(&db, "g2", &original).await?;
            for rewards in [
                vec![reward(i32::MAX as u64 + 1, ROLE_B)],
                vec![reward(1, ROLE_A), reward(2, ROLE_A)],
            ] {
                assert!(matches!(
                    replace_role_rewards(&db, "g1", &rewards).await,
                    Err(ReplaceRewardsError::Invalid(_))
                ));
                assert_eq!(role_rewards(&db, "g1").await?, original);
            }
            let replacement = vec![
                reward(1, ROLE_A),
                reward(i32::MAX as u64, ROLE_A),
                reward(1, ROLE_B),
            ];
            replace_role_rewards(&db, "g1", &replacement).await?;
            assert_eq!(
                role_rewards(&db, "g1").await?,
                vec![reward(1, ROLE_B), reward(i32::MAX as u64, ROLE_A)]
            );
            assert_eq!(role_rewards(&db, "g2").await?, original);
            replace_role_rewards(&db, "g1", &[]).await?;
            assert!(role_rewards(&db, "g1").await?.is_empty());
            Ok::<_, Box<dyn std::error::Error>>(())
        }
        .await;
        db.close().await;
        // Only this generated schema is disposable; preserve all shared data.
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await?;
        admin.close().await;
        result
    }
}

#[cfg(test)]
#[path = "db_pool_timeout_acceptance_tests.rs"]
mod pool_timeout_acceptance_tests;
