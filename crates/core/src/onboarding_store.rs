//! Onboarding persistence over the legacy funnel `events` table.
//!
//! Use [`begin_prompt`] before executing a welcome. The transaction-scoped
//! member lock serializes join/gate-clear redeliveries, then checks the durable
//! marker. Record only after a successful send; a failed send drops the guard
//! and leaves the member eligible for retry. Like the legacy send-then-record
//! path, a process crash after Discord accepted the message but before commit
//! is not an exactly-once delivery guarantee across those two systems.

use sqlx::{PgConnection, Pool, Postgres, Transaction};

use super::onboarding::{
    channel_routed_row, funnel_idempotency_key, game_selected_row, prompted_row,
    session_routed_row, FunnelRow, GameSelection, SessionPlan, EVENT_ONBOARDING_PROMPTED,
};

#[derive(Debug, thiserror::Error)]
pub enum OnboardingStoreError {
    #[error("onboarding store: {0}")]
    Sqlx(#[from] sqlx::Error),
}

async fn insert_row(connection: &mut PgConnection, row: &FunnelRow) -> Result<bool, sqlx::Error> {
    let inserted: Option<(i64,)> = sqlx::query_as(
        "INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
         VALUES ($1, $2, $3, $4::timestamptz, $5, $6, $7)
         ON CONFLICT (idempotency_key) DO NOTHING RETURNING id",
    )
    .bind(row.event_type)
    .bind(&row.member_id)
    .bind(&row.guild_id)
    .bind(&row.occurred_at)
    .bind(&row.source)
    .bind(&row.metadata)
    .bind(funnel_idempotency_key(row))
    .fetch_optional(connection)
    .await?;
    Ok(inserted.is_some())
}

async fn has_prompt(
    connection: &mut PgConnection,
    guild_id: &str,
    member_id: &str,
) -> Result<bool, sqlx::Error> {
    let row: Option<(i32,)> = sqlx::query_as(
        "SELECT 1 FROM events WHERE guild_id = $1 AND member_id = $2 AND event_type = $3 LIMIT 1",
    )
    .bind(guild_id)
    .bind(member_id)
    .bind(EVENT_ONBOARDING_PROMPTED)
    .fetch_optional(connection)
    .await?;
    Ok(row.is_some())
}

/// Read-only fast path for `decide_prompt`. This alone is not a claim: before
/// sending, use `begin_prompt` to re-check under the member lock.
pub async fn has_onboarding_prompt(
    pool: &Pool<Postgres>,
    guild_id: &str,
    member_id: &str,
) -> Result<bool, OnboardingStoreError> {
    Ok(has_prompt(&mut *pool.acquire().await?, guild_id, member_id).await?)
}

/// Permission to execute one welcome, held until its result is recorded.
/// Dropping the guard without `record_sent` rolls back and releases the lock.
/// Keep the executor's send timeout bounded; this guard holds a DB connection.
pub struct PromptGuard {
    transaction: Transaction<'static, Postgres>,
    guild_id: String,
    member_id: String,
}

impl PromptGuard {
    /// Call only when Discord has accepted the welcome. No failed sends or
    /// log-only dry runs may count as prompted. Returns the insert result.
    pub async fn record_sent(
        self,
        channel_id: &str,
        occurred_at: &str,
    ) -> Result<bool, OnboardingStoreError> {
        self.record_success(channel_id, occurred_at, false).await
    }

    /// An anchor welcome is also a successful route. Commit both rows under
    /// the prompt lock, so a failed route insert cannot consume the marker.
    pub async fn record_anchor_sent(
        self,
        channel_id: &str,
        occurred_at: &str,
    ) -> Result<bool, OnboardingStoreError> {
        self.record_success(channel_id, occurred_at, true).await
    }

    async fn record_success(
        mut self,
        channel_id: &str,
        occurred_at: &str,
        anchor: bool,
    ) -> Result<bool, OnboardingStoreError> {
        let row = prompted_row(&self.guild_id, &self.member_id, channel_id, occurred_at);
        let inserted = insert_row(&mut self.transaction, &row).await?;
        if inserted && anchor {
            let routed = channel_routed_row(
                &self.guild_id,
                &self.member_id,
                &[channel_id.to_owned()],
                0,
                occurred_at,
            );
            insert_row(&mut self.transaction, &routed).await?;
        }
        self.transaction.commit().await?;
        Ok(inserted)
    }
}

/// Acquire the per-guild/member prompt lock, then re-check the legacy marker.
/// `None` means another welcome already succeeded; do not post again.
pub async fn begin_prompt(
    pool: &Pool<Postgres>,
    guild_id: &str,
    member_id: &str,
) -> Result<Option<PromptGuard>, OnboardingStoreError> {
    let mut transaction = pool.begin().await?;
    // Stable across processes, independent of Rust's randomized hash seed.
    // Hash collisions only serialize unrelated members; they never skip one.
    let key = format!("{guild_id}:{member_id}:onboarding_prompted");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(key)
        .execute(&mut *transaction)
        .await?;
    if has_prompt(&mut transaction, guild_id, member_id).await? {
        transaction.rollback().await?;
        return Ok(None);
    }
    Ok(Some(PromptGuard {
        transaction,
        guild_id: guild_id.to_owned(),
        member_id: member_id.to_owned(),
    }))
}

/// Idempotently import/record a welcome that has already been sent. Runtime
/// senders should hold a `PromptGuard` instead of checking then sending.
pub async fn record_prompted(
    pool: &Pool<Postgres>,
    guild_id: &str,
    member_id: &str,
    channel_id: &str,
    occurred_at: &str,
) -> Result<bool, OnboardingStoreError> {
    match begin_prompt(pool, guild_id, member_id).await? {
        Some(guard) => guard.record_sent(channel_id, occurred_at).await,
        None => Ok(false),
    }
}

/// Record granted game roles after the executor's role writes succeed.
pub async fn record_game_selected(
    pool: &Pool<Postgres>,
    guild_id: &str,
    member_id: &str,
    keys: &[String],
    occurred_at: &str,
) -> Result<bool, OnboardingStoreError> {
    Ok(insert_row(
        &mut *pool.acquire().await?,
        &game_selected_row(guild_id, member_id, keys, occurred_at),
    )
    .await?)
}

/// Record the destinations re-resolved after successful role writes.
pub async fn record_channel_routed(
    pool: &Pool<Postgres>,
    guild_id: &str,
    member_id: &str,
    selection: &GameSelection,
    occurred_at: &str,
) -> Result<bool, OnboardingStoreError> {
    Ok(insert_row(
        &mut *pool.acquire().await?,
        &channel_routed_row(
            guild_id,
            member_id,
            &selection.channel_ids,
            selection.degraded_count,
            occurred_at,
        ),
    )
    .await?)
}

/// Stage both game success rows in the caller's role-write transaction.
/// The caller must commit only after the deferred reply succeeds. Any insert,
/// reply, or commit failure then leaves neither successful-selection row.
/// An empty destination list records the granted games but no successful route.
pub async fn record_game_selection_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    guild_id: &str,
    member_id: &str,
    keys: &[String],
    selection: &GameSelection,
    occurred_at: &str,
) -> Result<(), OnboardingStoreError> {
    insert_row(
        transaction,
        &game_selected_row(guild_id, member_id, keys, occurred_at),
    )
    .await?;
    if !selection.channel_ids.is_empty() {
        insert_row(
            transaction,
            &channel_routed_row(
                guild_id,
                member_id,
                &selection.channel_ids,
                selection.degraded_count,
                occurred_at,
            ),
        )
        .await?;
    }
    Ok(())
}

/// Stage roleless routing until the caller's deferred reply has succeeded.
pub async fn record_session_routed_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    guild_id: &str,
    member_id: &str,
    plan: &SessionPlan,
    occurred_at: &str,
) -> Result<bool, OnboardingStoreError> {
    Ok(insert_row(
        transaction,
        &session_routed_row(guild_id, member_id, plan, occurred_at),
    )
    .await?)
}

/// Record roleless session routing; repeated picks at new timestamps count.
pub async fn record_session_routed(
    pool: &Pool<Postgres>,
    guild_id: &str,
    member_id: &str,
    plan: &SessionPlan,
    occurred_at: &str,
) -> Result<bool, OnboardingStoreError> {
    Ok(insert_row(
        &mut *pool.acquire().await?,
        &session_routed_row(guild_id, member_id, plan, occurred_at),
    )
    .await?)
}
