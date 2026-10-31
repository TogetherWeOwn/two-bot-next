//! Onboarding persistence over the legacy funnel `events` table.
//! The prompt marker is a per-stay guard: [`clear_onboarding_prompt`] releases
//! it when a member's goodbye is processed.
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
    session_routed_row, FunnelRow, GameSelection, SessionPlan, EVENT_CHANNEL_ROUTED,
    EVENT_GAME_ROLES_SELECTED, EVENT_ONBOARDING_PROMPTED,
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
        // Re-gate under the guard's table lock: an opt-out that committed
        // while the welcome was in flight drops the row instead of landing
        // after the purge.
        if crate::privacy::suppressed_in(&mut self.transaction, &self.guild_id, &self.member_id)
            .await?
        {
            self.transaction.rollback().await?;
            return Ok(false);
        }
        let row = prompted_row(&self.guild_id, &self.member_id, channel_id, occurred_at);
        let inserted = insert_row(&mut self.transaction, &row).await?;
        if inserted && anchor {
            let routed = channel_routed_row(
                &self.guild_id,
                &self.member_id,
                &[channel_id.to_owned()],
                0,
                &[],
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
/// A suppressed (opted-out) pair also yields `None`: no welcome is sent and
/// no per-user row is written for a member who opted out. The suppression
/// gate runs inside the guard transaction under the shared table lock (held
/// until `record_sent` commits or the guard drops), so a concurrent opt-out
/// purge cannot slip between gate and write; `record_success` re-gates
/// before the insert for opt-outs that land while the welcome is in flight.
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
    // Serialize with concurrent opt-outs (SHARE ROW EXCLUSIVE over these
    // tables): whoever locks first wins. Lock order matches `opt_out_member`.
    sqlx::query("LOCK TABLE events, privacy_opt_outs IN ROW EXCLUSIVE MODE")
        .execute(&mut *transaction)
        .await?;
    if crate::privacy::suppressed_in(&mut transaction, guild_id, member_id).await? {
        transaction.rollback().await?;
        return Ok(None);
    }
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

/// Release one member's prompt guard after their leave was processed, so a
/// rejoin is welcomed again. Returns whether a marker was removed. A single
/// atomic DELETE: job order comes from the worker queue, and a suppressed
/// pair never holds a row, so a concurrent opt-out can only turn this into a
/// no-op. The next welcome re-runs the full [`begin_prompt`] gate
/// (suppression, then the marker), so clearing never welcomes someone who
/// opted out.
pub async fn clear_onboarding_prompt(
    pool: &Pool<Postgres>,
    guild_id: &str,
    member_id: &str,
) -> Result<bool, OnboardingStoreError> {
    let removed = sqlx::query(
        "DELETE FROM events WHERE guild_id = $1 AND member_id = $2 AND event_type = $3",
    )
    .bind(guild_id)
    .bind(member_id)
    .bind(EVENT_ONBOARDING_PROMPTED)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(removed > 0)
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
/// A suppressed (opted-out) pair records nothing and reports `false`: the
/// Discord role effect itself is outside this store's scope, but no
/// per-user row is persisted for it. The gate runs under the shared table
/// lock so a concurrent opt-out cannot slip between gate and write.
pub async fn record_game_selected(
    pool: &Pool<Postgres>,
    guild_id: &str,
    member_id: &str,
    keys: &[String],
    occurred_at: &str,
) -> Result<bool, OnboardingStoreError> {
    let mut tx = pool.begin().await?;
    sqlx::query("LOCK TABLE events, privacy_opt_outs IN ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    if crate::privacy::suppressed_in(&mut tx, guild_id, member_id).await? {
        tx.rollback().await?;
        return Ok(false);
    }
    let inserted = insert_row(
        &mut tx,
        &game_selected_row(guild_id, member_id, keys, occurred_at),
    )
    .await?;
    tx.commit().await?;
    Ok(inserted)
}

/// Record the destinations re-resolved after successful role writes.
/// A suppressed (opted-out) pair records nothing and reports `false` (see
/// [`record_game_selected`]).
pub async fn record_channel_routed(
    pool: &Pool<Postgres>,
    guild_id: &str,
    member_id: &str,
    selection: &GameSelection,
    occurred_at: &str,
) -> Result<bool, OnboardingStoreError> {
    let mut tx = pool.begin().await?;
    sqlx::query("LOCK TABLE events, privacy_opt_outs IN ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    if crate::privacy::suppressed_in(&mut tx, guild_id, member_id).await? {
        tx.rollback().await?;
        return Ok(false);
    }
    let inserted = insert_row(
        &mut tx,
        &channel_routed_row(
            guild_id,
            member_id,
            &selection.channel_ids,
            selection.degraded_count,
            &selection.unavailable_keys(),
            occurred_at,
        ),
    )
    .await?;
    tx.commit().await?;
    Ok(inserted)
}

/// Stage both game success rows in the caller's role-write transaction.
/// The caller commits before replying, so the reply always describes committed
/// state: an insert or commit failure yields the failure reply path with
/// neither row. A reply failure after commit retracts the rows through
/// [`retract_picker_rows`] before the terminal error edit.
/// An empty destination list records the granted games but no successful route.
pub async fn record_game_selection_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    guild_id: &str,
    member_id: &str,
    keys: &[String],
    selection: &GameSelection,
    occurred_at: &str,
) -> Result<(), OnboardingStoreError> {
    // Suppressed pairs persist nothing, even inside the caller's role-write
    // transaction (see [`record_game_selected`]). The shared table lock
    // serializes with concurrent opt-outs inside the caller's transaction.
    sqlx::query("LOCK TABLE events, privacy_opt_outs IN ROW EXCLUSIVE MODE")
        .execute(&mut **transaction)
        .await?;
    if crate::privacy::suppressed_in(transaction, guild_id, member_id).await? {
        return Ok(());
    }
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
                &selection.unavailable_keys(),
                occurred_at,
            ),
        )
        .await?;
    }
    Ok(())
}

/// Stage the roles a partial game-picker failure already flipped, in the
/// caller's role-write transaction. Only granted picks are recorded — the
/// pending picks were never applied, and no route was delivered, so no
/// `channel_routed` row is staged. The member's retry is a fresh whole-answer
/// submission that converges to full success and records the complete
/// selection alongside this partial row. An empty grant list stages nothing:
/// revokes are never funnel events, matching the success path, where a clear
/// records nothing. Suppressed pairs persist nothing
/// (see [`record_game_selected`]).
pub async fn record_game_partial_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    guild_id: &str,
    member_id: &str,
    applied_keys: &[String],
    occurred_at: &str,
) -> Result<(), OnboardingStoreError> {
    if applied_keys.is_empty() {
        return Ok(());
    }
    sqlx::query("LOCK TABLE events, privacy_opt_outs IN ROW EXCLUSIVE MODE")
        .execute(&mut **transaction)
        .await?;
    if crate::privacy::suppressed_in(transaction, guild_id, member_id).await? {
        return Ok(());
    }
    insert_row(
        transaction,
        &game_selected_row(guild_id, member_id, applied_keys, occurred_at),
    )
    .await?;
    Ok(())
}

/// Retract the picker success rows committed for one submission when its
/// reply never reached the member, so the funnel does not count a selection
/// or route the member was told had failed. Keys are the submission's own
/// `guild/member/type/occurred_at` idempotency keys, so only that
/// submission's rows can match; a submission that staged nothing (a
/// suppressed pair, or a failure before commit) makes this a no-op. Returns
/// the number of rows removed.
pub async fn retract_picker_rows(
    pool: &Pool<Postgres>,
    guild_id: &str,
    member_id: &str,
    occurred_at: &str,
) -> Result<u64, OnboardingStoreError> {
    let keys: Vec<String> = [EVENT_GAME_ROLES_SELECTED, EVENT_CHANNEL_ROUTED]
        .into_iter()
        .map(|event_type| {
            funnel_idempotency_key(&FunnelRow {
                guild_id: guild_id.to_owned(),
                member_id: member_id.to_owned(),
                event_type,
                occurred_at: occurred_at.to_owned(),
                source: String::new(),
                metadata: None,
            })
        })
        .collect();
    let removed = sqlx::query("DELETE FROM events WHERE idempotency_key = ANY($1)")
        .bind(&keys)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(removed)
}

/// Stage roleless routing in the caller's transaction. The caller commits
/// before replying (see [`record_game_selection_in_transaction`]).
pub async fn record_session_routed_in_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    guild_id: &str,
    member_id: &str,
    plan: &SessionPlan,
    occurred_at: &str,
) -> Result<bool, OnboardingStoreError> {
    sqlx::query("LOCK TABLE events, privacy_opt_outs IN ROW EXCLUSIVE MODE")
        .execute(&mut **transaction)
        .await?;
    if crate::privacy::suppressed_in(transaction, guild_id, member_id).await? {
        return Ok(false);
    }
    Ok(insert_row(
        transaction,
        &session_routed_row(guild_id, member_id, plan, occurred_at),
    )
    .await?)
}

/// Record roleless session routing; repeated picks at new timestamps count.
/// A suppressed (opted-out) pair records nothing and reports `false` (see
/// [`record_game_selected`]).
pub async fn record_session_routed(
    pool: &Pool<Postgres>,
    guild_id: &str,
    member_id: &str,
    plan: &SessionPlan,
    occurred_at: &str,
) -> Result<bool, OnboardingStoreError> {
    let mut tx = pool.begin().await?;
    sqlx::query("LOCK TABLE events, privacy_opt_outs IN ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    if crate::privacy::suppressed_in(&mut tx, guild_id, member_id).await? {
        tx.rollback().await?;
        return Ok(false);
    }
    let inserted = insert_row(
        &mut tx,
        &session_routed_row(guild_id, member_id, plan, occurred_at),
    )
    .await?;
    tx.commit().await?;
    Ok(inserted)
}
