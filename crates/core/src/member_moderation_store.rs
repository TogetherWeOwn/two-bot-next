//! Postgres member-moderation ledger. Enabled only by the core `db` feature.
//!
//! Construct exactly one consumer per guild and clone it into command/sweep
//! paths: clones share the per-member queues. SQL claims are atomic across
//! connections, but ordering Discord effects requires that single consumer.
//! Every database operation is fenced to the guild bound at construction.

use std::future::Future;

use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::member_moderation::{
    AuditRow, ClaimState, MemberModerationStore, MemberQueues, StoreError, UnbanJob,
    UnbanResolution,
};

#[derive(Clone)]
pub struct PgMemberModerationStore {
    pool: PgPool,
    guild_id: String,
    queues: MemberQueues,
}

impl PgMemberModerationStore {
    #[must_use]
    pub fn new(pool: PgPool, guild_id: impl Into<String>) -> Self {
        Self {
            pool,
            guild_id: guild_id.into(),
            queues: MemberQueues::default(),
        }
    }

    fn ensure_guild(&self, guild: &str) -> Result<(), StoreError> {
        if guild != self.guild_id {
            return Err(StoreError::new("moderation store guild mismatch"));
        }
        Ok(())
    }

    // Both temporary and permanent bans create their fence before dispatch.
    // Only an explicitly rejected exact identity can reuse a request id, and
    // it receives a fresh sequence generation even if its clock moved back.
    async fn prepare(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        guild: &str,
        user: &str,
        request: &str,
        now: &str,
    ) -> Result<(), StoreError> {
        self.ensure_guild(guild)?;
        let changed = sqlx::query(
            "INSERT INTO moderation_member_bans
             (request_id, guild_id, user_id, state, created_at)
             VALUES ($1, $2, $3, 'prepared', $4::text::timestamptz)
             ON CONFLICT (request_id) DO UPDATE SET state = 'prepared',
               generation = EXCLUDED.generation, created_at = EXCLUDED.created_at,
               completed_at = NULL
             WHERE moderation_member_bans.state = 'rejected'
               AND moderation_member_bans.guild_id = EXCLUDED.guild_id
               AND moderation_member_bans.user_id = EXCLUDED.user_id",
        )
        .bind(request)
        .bind(&self.guild_id)
        .bind(user)
        .bind(now)
        .execute(&mut **tx)
        .await
        .map_err(rolled_back_error)?
        .rows_affected();
        if changed != 1 {
            // The competing non-rejected intent survives; the failed insert
            // wrote nothing for this request. Rolled back either way.
            return Err(StoreError::rolled_back(REQUEST_ID_IN_USE));
        }
        Ok(())
    }

    // Acceptance, current generation and the exact staged schedule are checked
    // by the same UPDATE. Recovery never guesses whether dispatch succeeded.
    // Supersession belongs to confirm_ban, not activation: a delayed older
    // activation cannot supersede a newer permanent or temporary ban.
    async fn activate(
        &self,
        guild: &str,
        user: &str,
        request: &str,
        recovery: bool,
    ) -> Result<(), StoreError> {
        self.ensure_guild(guild)?;
        let changed = sqlx::query(
            "UPDATE moderation_scheduled_unbans AS job SET state = 'pending'
             FROM moderation_member_bans AS intent
             WHERE job.request_id = $1 AND job.guild_id = $2 AND job.user_id = $3
               AND job.state = 'staged' AND intent.request_id = job.request_id
               AND intent.guild_id = job.guild_id AND intent.user_id = job.user_id
               AND intent.state = 'accepted'
               AND NOT EXISTS (
                 SELECT 1 FROM moderation_member_bans AS newer
                 WHERE newer.guild_id = intent.guild_id AND newer.user_id = intent.user_id
                   AND newer.generation > intent.generation AND newer.state <> 'rejected'
               )",
        )
        .bind(request)
        .bind(&self.guild_id)
        .bind(user)
        .execute(&self.pool)
        .await
        .map_err(db_error)?
        .rows_affected();
        if changed != 1 && !recovery {
            return Err(StoreError::new("lost or fenced staged unban"));
        }
        Ok(())
    }

    // F1: a member with a `running` schedule holds a dispatched unban whose
    // remote DELETE may still land. Staging a fresh ban while that
    // uncertainty is unresolved would let the late DELETE remove the new
    // ban, so every staging path refuses until authoritative
    // `resolve_uncertain_unban` evidence clears the fence. SQLSTATE-only
    // error: no row contents reach the message.
    async fn refuse_running_unban_fence(
        executor: impl sqlx::Executor<'_, Database = Postgres>,
        guild: &str,
        user: &str,
    ) -> Result<(), StoreError> {
        let running: bool = sqlx::query_scalar(
            "SELECT EXISTS (
               SELECT 1 FROM moderation_scheduled_unbans AS job
               WHERE job.guild_id = $1 AND job.user_id = $2 AND job.state = 'running'
             )",
        )
        .bind(guild)
        .bind(user)
        .fetch_one(executor)
        .await
        .map_err(db_error)?;
        if running {
            return Err(StoreError::rolled_back(
                "member has an uncertain dispatched unban; resolve it before banning",
            ));
        }
        Ok(())
    }
}

// The request id is already owned by a durable non-rejected intent: the
// request itself is fenced, never safe to retry under the same key.
const REQUEST_ID_IN_USE: &str = "ban request id is already in use";
const UNBAN_REQUEST_ID_IN_USE: &str = "unban request id is already in use";

// Never log SQL parameter values or database DETAIL (which can contain a
// moderator's reason). The SQLSTATE is enough to diagnose a ledger failure.
fn rolled_back_error(error: sqlx::Error) -> StoreError {
    let classification = match &error {
        sqlx::Error::Database(db) => db
            .code()
            .map(|s| s.into_owned())
            .unwrap_or_else(|| "database".into()),
        sqlx::Error::PoolTimedOut => "pool_timeout".into(),
        sqlx::Error::PoolClosed => "pool_closed".into(),
        sqlx::Error::Io(_) | sqlx::Error::Tls(_) => "connection".into(),
        _ => "query".into(),
    };
    StoreError::rolled_back(format!("postgres moderation ledger: {classification}"))
}

// Never log SQL parameter values or database DETAIL (which can contain a
// moderator's reason). The SQLSTATE is enough to diagnose a ledger failure.
fn db_error(error: sqlx::Error) -> StoreError {
    let classification = match &error {
        sqlx::Error::Database(db) => db
            .code()
            .map(|s| s.into_owned())
            .unwrap_or_else(|| "database".into()),
        sqlx::Error::PoolTimedOut => "pool_timeout".into(),
        sqlx::Error::PoolClosed => "pool_closed".into(),
        sqlx::Error::Io(_) | sqlx::Error::Tls(_) => "connection".into(),
        _ => "query".into(),
    };
    StoreError::new(format!("postgres moderation ledger: {classification}"))
}

impl MemberModerationStore for PgMemberModerationStore {
    async fn serialize_member<T, F, Fut>(&self, guild_id: &str, user_id: &str, run: F) -> T
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = T> + Send,
    {
        // The generic result cannot report a guild error here. Each operation
        // inside the closure independently fences its guild before any write.
        self.queues.run(guild_id, user_id, run).await
    }

    async fn claim(
        &self,
        guild_id: &str,
        key: &str,
        action: &str,
        hash: &str,
        now: &str,
    ) -> Result<ClaimState, StoreError> {
        self.ensure_guild(guild_id)?;
        let won = sqlx::query(
            "INSERT INTO moderation_idempotency
             (guild_id, idempotency_key, action, request_hash, state, claimed_at)
             VALUES ($1, $2, $3, $4, 'in_flight', $5::text::timestamptz)
             ON CONFLICT (guild_id, idempotency_key) DO NOTHING",
        )
        .bind(&self.guild_id)
        .bind(key)
        .bind(action)
        .bind(hash)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_error)?
        .rows_affected();
        if won == 1 {
            return Ok(ClaimState::Claimed);
        }
        let row = sqlx::query(
            "SELECT request_hash, state, outcome FROM moderation_idempotency
             WHERE guild_id = $1 AND idempotency_key = $2",
        )
        .bind(&self.guild_id)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_error)?;
        let Some(row) = row else {
            return Ok(ClaimState::InFlight);
        };
        if row.try_get::<String, _>("request_hash").map_err(db_error)? != hash {
            return Ok(ClaimState::Mismatch);
        }
        if row.try_get::<String, _>("state").map_err(db_error)? == "done" {
            return Ok(ClaimState::Replayed {
                outcome: row
                    .try_get::<Option<String>, _>("outcome")
                    .map_err(db_error)?
                    .ok_or_else(|| StoreError::new("completed claim has no outcome"))?,
            });
        }
        Ok(ClaimState::InFlight)
    }

    async fn complete(
        &self,
        guild_id: &str,
        key: &str,
        outcome: &str,
        result_json: &str,
        now: &str,
    ) -> Result<(), StoreError> {
        self.ensure_guild(guild_id)?;
        let changed = sqlx::query(
            "UPDATE moderation_idempotency SET state = 'done', outcome = $3,
             result_json = $4, completed_at = $5::text::timestamptz
             WHERE guild_id = $1 AND idempotency_key = $2 AND state = 'in_flight'",
        )
        .bind(&self.guild_id)
        .bind(key)
        .bind(outcome)
        .bind(result_json)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_error)?
        .rows_affected();
        if changed != 1 {
            return Err(StoreError::new("lost moderation claim"));
        }
        Ok(())
    }

    async fn release(&self, guild_id: &str, key: &str) -> Result<(), StoreError> {
        self.ensure_guild(guild_id)?;
        sqlx::query(
            "DELETE FROM moderation_idempotency WHERE guild_id = $1
                     AND idempotency_key = $2 AND state = 'in_flight'",
        )
        .bind(&self.guild_id)
        .bind(key)
        .execute(&self.pool)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn record_audit(&self, row: &AuditRow) -> Result<(), StoreError> {
        self.ensure_guild(&row.guild_id)?;
        sqlx::query(
            "INSERT INTO moderation_audit
             (request_id, guild_id, actor_id, action, target_id, channel_id, reason,
              outcome, idempotency_key, metadata_json, created_at)
             VALUES ($1, $2, $3, $4, $5, NULL, $6, $7, $8, $9, NOW())
             ON CONFLICT (request_id) DO NOTHING",
        )
        .bind(&row.request_id)
        .bind(&self.guild_id)
        .bind(&row.actor_id)
        .bind(row.action)
        .bind(&row.target_id)
        .bind(&row.reason)
        .bind(row.outcome)
        .bind(&row.idempotency_key)
        .bind(&row.metadata_json)
        .execute(&self.pool)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn add_warning(
        &self,
        id: &str,
        guild: &str,
        user: &str,
        actor: &str,
        reason: &str,
        request: &str,
        now: &str,
    ) -> Result<(), StoreError> {
        self.ensure_guild(guild)?;
        sqlx::query(
            "INSERT INTO moderation_warnings (id, guild_id, user_id, actor_id, reason, request_id, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7::text::timestamptz)
             ON CONFLICT (request_id) DO NOTHING"
        ).bind(id).bind(&self.guild_id).bind(user).bind(actor).bind(reason).bind(request).bind(now)
            .execute(&self.pool).await.map_err(db_error)?;
        Ok(())
    }

    async fn stage_ban(
        &self,
        guild: &str,
        user: &str,
        request: &str,
        now: &str,
    ) -> Result<(), StoreError> {
        self.ensure_guild(guild)?;
        let mut tx = self.pool.begin().await.map_err(db_error)?;
        Self::refuse_running_unban_fence(&mut *tx, &self.guild_id, user).await?;
        // A fresh intent that survives the fence must itself fence; every
        // staging path owns the member queue, so the check-then-insert is
        // atomic against this store's consumers. Every `prepare` failure
        // happens before any write for this request (the fence-only
        // refusal writes nothing; the insert either conflicts with a live
        // row or fails the statement), so the whole transaction rolls back
        // mutation-free and the error already carries that provenance.
        self.prepare(&mut tx, guild, user, request, now).await?;
        tx.commit().await.map_err(db_error)
    }

    async fn confirm_ban(
        &self,
        guild: &str,
        user: &str,
        request: &str,
        now: &str,
    ) -> Result<(), StoreError> {
        self.ensure_guild(guild)?;
        let mut tx = self.pool.begin().await.map_err(db_error)?;
        let generation: Option<i64> = sqlx::query_scalar(
            "UPDATE moderation_member_bans AS intent SET state = 'accepted',
               completed_at = $4::text::timestamptz
             WHERE intent.request_id = $1 AND intent.guild_id = $2 AND intent.user_id = $3
               AND intent.state = 'prepared'
               AND NOT EXISTS (
                 SELECT 1 FROM moderation_member_bans AS newer
                 WHERE newer.guild_id = intent.guild_id AND newer.user_id = intent.user_id
                   AND newer.generation > intent.generation AND newer.state <> 'rejected'
               )
             RETURNING intent.generation",
        )
        .bind(request)
        .bind(&self.guild_id)
        .bind(user)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_error)?;
        let generation = generation.ok_or_else(|| StoreError::new("lost prepared ban intent"))?;
        // A permanent ban has no schedule, but supersedes old expiries too.
        // Strict generation comparison prevents a late older confirmation
        // from superseding any newer schedule. Failure rolls acceptance back
        // to prepared, retaining its conservative fence against older jobs.
        // Only never-dispatched schedules (`staged`/`pending`) supersede
        // here: a `running` row holds a dispatched DELETE whose remote effect
        // may still land, so only authoritative `resolve_uncertain_unban`
        // evidence may close it — clearing its token here would let the late
        // DELETE remove this newly confirmed ban.
        sqlx::query(
            "UPDATE moderation_scheduled_unbans AS job SET state = 'superseded',
               completed_at = $1::text::timestamptz, claim_token = NULL
             FROM moderation_member_bans AS older
             WHERE job.guild_id = $2 AND job.user_id = $3
               AND job.request_id = older.request_id
               AND older.guild_id = job.guild_id AND older.user_id = job.user_id
               AND older.generation < $4 AND job.state IN ('staged', 'pending')",
        )
        .bind(now)
        .bind(&self.guild_id)
        .bind(user)
        .bind(generation)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
        tx.commit().await.map_err(db_error)
    }

    async fn stage_unban(
        &self,
        guild: &str,
        user: &str,
        execute_at: &str,
        reason: &str,
        request: &str,
        now: &str,
    ) -> Result<(), StoreError> {
        self.ensure_guild(guild)?;
        let mut tx = self.pool.begin().await.map_err(db_error)?;
        Self::refuse_running_unban_fence(&mut *tx, &self.guild_id, user).await?;
        // As in `stage_ban`: `prepare` failures precede any write for this
        // request, so the rolled-back transaction stays mutation-free.
        self.prepare(&mut tx, guild, user, request, now).await?;
        let changed = sqlx::query(
            "INSERT INTO moderation_scheduled_unbans
             (request_id, guild_id, user_id, execute_at, reason, state, created_at)
             VALUES ($1, $2, $3, $4::text::timestamptz, $5, 'staged', $6::text::timestamptz)
             ON CONFLICT (request_id) DO UPDATE SET state = 'staged',
               execute_at = EXCLUDED.execute_at, reason = EXCLUDED.reason,
               created_at = EXCLUDED.created_at, completed_at = NULL, claimed_at = NULL, claim_token = NULL
             WHERE moderation_scheduled_unbans.state = 'cancelled'
               AND moderation_scheduled_unbans.guild_id = EXCLUDED.guild_id
               AND moderation_scheduled_unbans.user_id = EXCLUDED.user_id",
        )
        .bind(request)
        .bind(&self.guild_id)
        .bind(user)
        .bind(execute_at)
        .bind(reason)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(rolled_back_error)?
        .rows_affected();
        if changed != 1 {
            return Err(StoreError::rolled_back(UNBAN_REQUEST_ID_IN_USE));
        }
        tx.commit().await.map_err(db_error)
    }

    async fn activate_staged_unban(
        &self,
        guild: &str,
        user: &str,
        request: &str,
        _now: &str,
    ) -> Result<(), StoreError> {
        self.activate(guild, user, request, false).await
    }

    async fn reject_ban(
        &self,
        guild: &str,
        user: &str,
        request: &str,
        now: &str,
    ) -> Result<(), StoreError> {
        self.ensure_guild(guild)?;
        let mut tx = self.pool.begin().await.map_err(db_error)?;
        let changed = sqlx::query(
            "UPDATE moderation_member_bans SET state = 'rejected', completed_at = $4::text::timestamptz
             WHERE request_id = $1 AND guild_id = $2 AND user_id = $3 AND state = 'prepared'",
        )
        .bind(request)
        .bind(&self.guild_id)
        .bind(user)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?
        .rows_affected();
        if changed != 1 {
            return Err(StoreError::new("lost prepared ban intent"));
        }
        // If this write fails, prepared -> rejected also rolls back. Otherwise
        // a rejected tempban could leave an automatically recoverable expiry
        // capable of removing an existing permanent ban.
        sqlx::query(
            "UPDATE moderation_scheduled_unbans SET state = 'cancelled',
               completed_at = $4::text::timestamptz
             WHERE request_id = $1 AND guild_id = $2 AND user_id = $3 AND state = 'staged'",
        )
        .bind(request)
        .bind(&self.guild_id)
        .bind(user)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
        tx.commit().await.map_err(db_error)
    }

    async fn claim_due_unbans(
        &self,
        guild: &str,
        now: &str,
        limit: i64,
    ) -> Result<Vec<UnbanJob>, StoreError> {
        self.ensure_guild(guild)?;
        let staged = sqlx::query(
            "SELECT job.request_id, job.user_id FROM moderation_scheduled_unbans AS job
             JOIN moderation_member_bans AS intent ON intent.request_id = job.request_id
               AND intent.guild_id = job.guild_id AND intent.user_id = job.user_id
             WHERE job.guild_id = $1 AND job.state = 'staged' AND intent.state = 'accepted'
               AND NOT EXISTS (
                 SELECT 1 FROM moderation_member_bans AS newer
                 WHERE newer.guild_id = intent.guild_id AND newer.user_id = intent.user_id
                   AND newer.generation > intent.generation AND newer.state <> 'rejected'
               )
             ORDER BY intent.generation DESC",
        )
        .bind(&self.guild_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_error)?;
        for row in staged {
            let request: String = row.try_get("request_id").map_err(db_error)?;
            let user: String = row.try_get("user_id").map_err(db_error)?;
            self.serialize_member(&self.guild_id, &user, || {
                self.activate(&self.guild_id, &user, &request, true)
            })
            .await?;
        }
        let token = format!("{:032x}", rand::random::<u128>());
        // A single UPDATE owns the rows. SKIP LOCKED permits overlapping
        // sweeps without ever reclaiming an uncertain running row by age.
        // Both recovery and claiming are scoped to this consumer's guild.
        let rows = sqlx::query(
            "WITH due AS (
               SELECT job.request_id FROM moderation_scheduled_unbans AS job
               JOIN moderation_member_bans AS intent ON intent.request_id = job.request_id
                 AND intent.guild_id = job.guild_id AND intent.user_id = job.user_id
               WHERE job.guild_id = $1 AND job.state = 'pending' AND intent.state = 'accepted'
                 AND job.execute_at <= $2::text::timestamptz
                 AND NOT EXISTS (
                   SELECT 1 FROM moderation_member_bans AS newer
                   WHERE newer.guild_id = intent.guild_id AND newer.user_id = intent.user_id
                     AND newer.generation > intent.generation AND newer.state <> 'rejected'
                 )
               ORDER BY job.execute_at, intent.generation LIMIT $3 FOR UPDATE OF job SKIP LOCKED
             ) UPDATE moderation_scheduled_unbans AS job SET state = 'running',
               claimed_at = $2::text::timestamptz, claim_token = $4
             FROM due WHERE job.request_id = due.request_id AND job.guild_id = $1 AND job.state = 'pending'
             RETURNING job.request_id, job.guild_id, job.user_id, job.reason, job.claim_token",
        )
        .bind(&self.guild_id)
        .bind(now)
        .bind(limit.clamp(0, 25))
        .bind(token)
        .fetch_all(&self.pool)
        .await
        .map_err(db_error)?;
        rows.into_iter()
            .map(|row| {
                Ok(UnbanJob {
                    request_id: row.try_get("request_id").map_err(db_error)?,
                    claim_token: row.try_get("claim_token").map_err(db_error)?,
                    guild_id: row.try_get("guild_id").map_err(db_error)?,
                    user_id: row.try_get("user_id").map_err(db_error)?,
                    reason: row.try_get("reason").map_err(db_error)?,
                })
            })
            .collect()
    }

    async fn owns_unban_claim(&self, request: &str, token: &str) -> Result<bool, StoreError> {
        sqlx::query_scalar(
            "SELECT EXISTS (
               SELECT 1 FROM moderation_scheduled_unbans AS job
               JOIN moderation_member_bans AS intent ON intent.request_id = job.request_id
                 AND intent.guild_id = job.guild_id AND intent.user_id = job.user_id
               WHERE job.request_id = $1 AND job.claim_token = $2 AND job.guild_id = $3
                 AND job.state = 'running' AND intent.state = 'accepted'
                 AND NOT EXISTS (
                   SELECT 1 FROM moderation_member_bans AS newer
                   WHERE newer.guild_id = intent.guild_id AND newer.user_id = intent.user_id
                     AND newer.generation > intent.generation AND newer.state <> 'rejected'
                 )
             )",
        )
        .bind(request)
        .bind(token)
        .bind(&self.guild_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db_error)
    }

    async fn complete_unban(&self, request: &str, token: &str) -> Result<(), StoreError> {
        let changed = sqlx::query(
            "UPDATE moderation_scheduled_unbans SET state = 'done',
               completed_at = NOW(), claim_token = NULL
             WHERE request_id = $1 AND state = 'running' AND claim_token = $2 AND guild_id = $3",
        )
        .bind(request)
        .bind(token)
        .bind(&self.guild_id)
        .execute(&self.pool)
        .await
        .map_err(db_error)?
        .rows_affected();
        if changed != 1 {
            return Err(StoreError::new("lost scheduled-unban claim"));
        }
        Ok(())
    }

    async fn requeue_unban(&self, request: &str, token: &str) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE moderation_scheduled_unbans SET state = 'pending',
               claimed_at = NULL, claim_token = NULL
             WHERE request_id = $1 AND state = 'running' AND claim_token = $2 AND guild_id = $3",
        )
        .bind(request)
        .bind(token)
        .bind(&self.guild_id)
        .execute(&self.pool)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn resolve_uncertain_unban(
        &self,
        request: &str,
        token: &str,
        resolution: UnbanResolution,
    ) -> Result<(), StoreError> {
        // Resolve only the exact dispatched operation. Voiding a DELETE
        // preserves a trusted expiry unless a newer ACCEPTED ban replaced it;
        // a prepared newer intent is uncertainty, not a cancelled obligation.
        let state = resolution.as_str();
        let changed = sqlx::query(
            "WITH resolved AS (
               SELECT job.request_id,
                 CASE WHEN $3 = 'completed' THEN 'done'
                   WHEN EXISTS (
                     SELECT 1 FROM moderation_member_bans AS intent
                     WHERE intent.request_id = job.request_id AND intent.guild_id = job.guild_id
                       AND intent.user_id = job.user_id AND intent.state = 'accepted'
                       AND NOT EXISTS (
                         SELECT 1 FROM moderation_member_bans AS newer
                         WHERE newer.guild_id = intent.guild_id AND newer.user_id = intent.user_id
                           AND newer.generation > intent.generation AND newer.state = 'accepted'
                       )
                   ) THEN 'pending' ELSE 'superseded' END AS state
               FROM moderation_scheduled_unbans AS job
               WHERE job.request_id = $1 AND job.state = 'running'
                 AND job.claim_token = $2 AND job.guild_id = $4
             ) UPDATE moderation_scheduled_unbans AS job SET state = resolved.state,
               completed_at = CASE WHEN resolved.state = 'pending' THEN NULL ELSE NOW() END,
               claimed_at = NULL, claim_token = NULL
             FROM resolved WHERE job.request_id = resolved.request_id AND job.guild_id = $4
               AND job.state = 'running' AND job.claim_token = $2",
        )
        .bind(request)
        .bind(token)
        .bind(state)
        .bind(&self.guild_id)
        .execute(&self.pool)
        .await
        .map_err(db_error)?
        .rows_affected();
        if changed != 1 {
            return Err(StoreError::new("lost uncertain scheduled-unban claim"));
        }
        Ok(())
    }
}
