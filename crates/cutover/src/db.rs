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

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Pool, Postgres};
use std::str::FromStr;

/// Legacy pool default (`TWO_DB_POOL_MAX ?? 5`).
pub const DB_POOL_MAX_DEFAULT: u32 = 5;
/// Legacy statement timeout (`statementTimeoutMillis ?? 15_000`).
pub const STATEMENT_TIMEOUT_MS: u64 = 15_000;

/// Open the Postgres pool and apply pending migrations (unless skipped).
pub async fn connect(
    url: &str,
    pool_max: u32,
    skip_migrations: bool,
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
    let mut options = PgConnectOptions::from_str(url)
        .map_err(|e| sqlx::Error::InvalidArgument(format!("invalid database URL: {e}")))?;
    // Statement timeout rides the connection options (server-side setting
    // per connection), so no per-connection SET is needed.
    options = options.options([("statement_timeout", format!("{}ms", STATEMENT_TIMEOUT_MS))]);
    let pool = PgPoolOptions::new()
        .max_connections(pool_max)
        .connect_with(options)
        .await?;
    let db = CutoverDb { pool };
    if !skip_migrations {
        if let Err(e) = db.migrate().await {
            db.close().await;
            return Err(e);
        }
    }
    Ok(db)
}

/// Cutover database handle.
#[derive(Debug, Clone)]
pub struct CutoverDb {
    pool: Pool<Postgres>,
}

impl CutoverDb {
    #[must_use]
    pub fn pool(&self) -> &Pool<Postgres> {
        &self.pool
    }

    /// Apply the crate's embedded migrations.
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .map_err(|e| sqlx::Error::InvalidArgument(format!("migration failed: {e}")))
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
pub async fn record_event(
    db: &CutoverDb,
    e: &FunnelWrite,
) -> Result<(bool, Option<i64>), sqlx::Error> {
    let key = idempotency_key(e);
    let mut tx = db.pool.begin().await?;
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
    .bind(&e.metadata)
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
async fn project_event(
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
async fn advance_activity(
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
/// `touchActivity`).
pub async fn touch_activity(
    db: &CutoverDb,
    guild_id: &str,
    member_id: &str,
    at: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO members (guild_id, member_id, last_active_at) VALUES ($1, $2, $3::timestamptz)
         ON CONFLICT (guild_id, member_id) DO UPDATE SET last_active_at = excluded.last_active_at
           WHERE members.last_active_at IS NULL OR members.last_active_at < excluded.last_active_at",
    )
    .bind(guild_id)
    .bind(member_id)
    .bind(at)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// Mark a member row as a bot (legacy `markBot`).
pub async fn mark_bot(db: &CutoverDb, guild_id: &str, member_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO members (guild_id, member_id, is_bot) VALUES ($1, $2, TRUE)
         ON CONFLICT (guild_id, member_id) DO UPDATE SET is_bot = TRUE",
    )
    .bind(guild_id)
    .bind(member_id)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// Once-per-member milestone keeping the EARLIEST known time: backfill can
/// discover a message older than one already on file, and the older
/// timestamp is the truth (legacy `recordEarliest`).
pub async fn record_earliest(
    db: &CutoverDb,
    e: &FunnelWrite,
) -> Result<(bool, Option<i64>), sqlx::Error> {
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
    if let Some(member_id) = e.member_id.as_deref() {
        let pull_back = match e.event_type.as_str() {
            "first_message" => Some(
                "UPDATE members SET first_message_at = $1::timestamptz WHERE guild_id = $2 AND member_id = $3 AND (first_message_at IS NULL OR first_message_at > $4)",
            ),
            "third_message" => Some(
                "UPDATE members SET third_message_at = $1::timestamptz WHERE guild_id = $2 AND member_id = $3 AND (third_message_at IS NULL OR third_message_at > $4)",
            ),
            "first_voice_session" => Some(
                "UPDATE members SET first_voice_at = $1::timestamptz WHERE guild_id = $2 AND member_id = $3 AND (first_voice_at IS NULL OR first_voice_at > $4)",
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
    use std::collections::BTreeMap;
    let mut normalized: BTreeMap<u64, &str> = BTreeMap::new();
    for r in rewards {
        if r.level == 0 {
            return Err(ReplaceRewardsError::Invalid(
                "reward level must be a positive integer".to_owned(),
            ));
        }
        if !crate::is_snowflake(&r.role_id) {
            return Err(ReplaceRewardsError::Invalid(format!(
                "invalid Discord role id: {}",
                r.role_id
            )));
        }
        normalized.insert(r.level, r.role_id.as_str());
    }
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
        .bind(level as i64)
        .bind(role_id)
        .execute(&mut *tx)
        .await
        .map_err(ReplaceRewardsError::Db)?;
    }
    tx.commit().await.map_err(ReplaceRewardsError::Db)?;
    Ok(())
}

/// Reward-replacement failure: invalid input vs database error.
#[derive(Debug, thiserror::Error)]
pub enum ReplaceRewardsError {
    #[error("{0}")]
    Invalid(String),
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}
