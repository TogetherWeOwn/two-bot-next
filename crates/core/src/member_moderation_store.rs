//! Postgres member-moderation ledger. Enabled only by the core `db` feature.
//!
//! Construct once per guild consumer and clone it into command/sweep paths:
//! clones share the legacy per-member queues. SQL claims are atomic across
//! connections, but ordering Discord effects requires a single guild consumer.

use std::future::Future;

use sqlx::{PgPool, Row};

use crate::member_moderation::{
    AuditRow, ClaimState, MemberModerationStore, MemberQueues, StoreError, UnbanJob,
};

#[derive(Clone)]
pub struct PgMemberModerationStore {
    pool: PgPool,
    queues: MemberQueues,
}

impl PgMemberModerationStore {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            queues: MemberQueues::default(),
        }
    }

    // Recovery rechecks the staged state under the same transaction as
    // supersession. A stale sweep snapshot cannot supersede a newer tempban.
    async fn activate(
        &self,
        guild: &str,
        user: &str,
        request: &str,
        now: &str,
        recovery: bool,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(db_error)?;
        let state: Option<String> = sqlx::query_scalar(
            "SELECT state FROM moderation_scheduled_unbans
             WHERE request_id = $1 AND guild_id = $2 AND user_id = $3 FOR UPDATE",
        )
        .bind(request)
        .bind(guild)
        .bind(user)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_error)?;
        if state.as_deref() != Some("staged") {
            if recovery {
                return Ok(());
            }
            return Err(StoreError::new("lost staged unban"));
        }
        sqlx::query(
            "UPDATE moderation_scheduled_unbans SET state = 'superseded',
             completed_at = $1::text::timestamptz, claim_token = NULL
             WHERE guild_id = $2 AND user_id = $3 AND request_id <> $4
               AND state IN ('staged', 'pending', 'running')",
        )
        .bind(now)
        .bind(guild)
        .bind(user)
        .bind(request)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
        sqlx::query(
            "UPDATE moderation_scheduled_unbans SET state = 'pending' WHERE request_id = $1",
        )
        .bind(request)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
        tx.commit().await.map_err(db_error)
    }
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
        let won = sqlx::query(
            "INSERT INTO moderation_idempotency
             (guild_id, idempotency_key, action, request_hash, state, claimed_at)
             VALUES ($1, $2, $3, $4, 'in_flight', $5::text::timestamptz)
             ON CONFLICT (guild_id, idempotency_key) DO NOTHING",
        )
        .bind(guild_id)
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
        .bind(guild_id)
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
        let changed = sqlx::query(
            "UPDATE moderation_idempotency SET state = 'done', outcome = $3,
             result_json = $4, completed_at = $5::text::timestamptz
             WHERE guild_id = $1 AND idempotency_key = $2 AND state = 'in_flight'",
        )
        .bind(guild_id)
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
        sqlx::query(
            "DELETE FROM moderation_idempotency WHERE guild_id = $1
                     AND idempotency_key = $2 AND state = 'in_flight'",
        )
        .bind(guild_id)
        .bind(key)
        .execute(&self.pool)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn record_audit(&self, row: &AuditRow) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO moderation_audit
             (request_id, guild_id, actor_id, action, target_id, channel_id, reason,
              outcome, idempotency_key, metadata_json, created_at)
             VALUES ($1, $2, $3, $4, $5, NULL, $6, $7, $8, $9, NOW())
             ON CONFLICT (request_id) DO NOTHING",
        )
        .bind(&row.request_id)
        .bind(&row.guild_id)
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
        sqlx::query(
            "INSERT INTO moderation_warnings (id, guild_id, user_id, actor_id, reason, request_id, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7::text::timestamptz)
             ON CONFLICT (request_id) DO NOTHING"
        ).bind(id).bind(guild).bind(user).bind(actor).bind(reason).bind(request).bind(now)
            .execute(&self.pool).await.map_err(db_error)?;
        Ok(())
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
        // A definite ban rejection cancelled this exact row and released its
        // idempotency key; that same request may legitimately be retried.
        let changed = sqlx::query(
            "INSERT INTO moderation_scheduled_unbans
             (request_id, guild_id, user_id, execute_at, reason, state, created_at)
             VALUES ($1, $2, $3, $4::text::timestamptz, $5, 'staged', $6::text::timestamptz)
             ON CONFLICT (request_id) DO UPDATE SET state = 'staged',
             execute_at = EXCLUDED.execute_at, reason = EXCLUDED.reason,
             created_at = EXCLUDED.created_at, completed_at = NULL, claimed_at = NULL, claim_token = NULL
             WHERE moderation_scheduled_unbans.state = 'cancelled'
               AND moderation_scheduled_unbans.guild_id = EXCLUDED.guild_id
               AND moderation_scheduled_unbans.user_id = EXCLUDED.user_id"
        ).bind(request).bind(guild).bind(user).bind(execute_at).bind(reason).bind(now)
            .execute(&self.pool).await.map_err(db_error)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::new("unban request id is already in use"));
        }
        Ok(())
    }

    async fn activate_staged_unban(
        &self,
        guild: &str,
        user: &str,
        request: &str,
        now: &str,
    ) -> Result<(), StoreError> {
        self.activate(guild, user, request, now, false).await
    }

    async fn cancel_staged_unban(&self, request: &str, now: &str) -> Result<(), StoreError> {
        sqlx::query("UPDATE moderation_scheduled_unbans SET state = 'cancelled',
                     completed_at = $2::text::timestamptz WHERE request_id = $1 AND state = 'staged'")
            .bind(request).bind(now).execute(&self.pool).await.map_err(db_error)?;
        Ok(())
    }

    async fn claim_due_unbans(&self, now: &str, limit: i64) -> Result<Vec<UnbanJob>, StoreError> {
        let staged = sqlx::query(
            "SELECT request_id, guild_id, user_id FROM moderation_scheduled_unbans
             WHERE state = 'staged' ORDER BY created_at DESC, request_id DESC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_error)?;
        for row in staged {
            let request: String = row.try_get("request_id").map_err(db_error)?;
            let guild: String = row.try_get("guild_id").map_err(db_error)?;
            let user: String = row.try_get("user_id").map_err(db_error)?;
            self.serialize_member(&guild, &user, || {
                self.activate(&guild, &user, &request, now, true)
            })
            .await?;
        }
        let token = format!("{:032x}", rand::random::<u128>());
        // A single UPDATE owns the rows. SKIP LOCKED lets overlapping sweeps
        // claim other jobs without ever reclaiming an uncertain running row.
        let rows = sqlx::query(
            "WITH due AS (
               SELECT request_id FROM moderation_scheduled_unbans
               WHERE state = 'pending' AND execute_at <= $1::text::timestamptz
               ORDER BY execute_at, request_id LIMIT $2 FOR UPDATE SKIP LOCKED
             ) UPDATE moderation_scheduled_unbans AS job SET state = 'running',
               claimed_at = $1::text::timestamptz, claim_token = $3
             FROM due WHERE job.request_id = due.request_id AND job.state = 'pending'
             RETURNING job.request_id, job.guild_id, job.user_id, job.reason, job.claim_token",
        )
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
            "SELECT EXISTS (SELECT 1 FROM moderation_scheduled_unbans
                           WHERE request_id = $1 AND state = 'running' AND claim_token = $2)",
        )
        .bind(request)
        .bind(token)
        .fetch_one(&self.pool)
        .await
        .map_err(db_error)
    }

    async fn complete_unban(&self, request: &str, token: &str) -> Result<(), StoreError> {
        let changed = sqlx::query(
            "UPDATE moderation_scheduled_unbans SET state = 'done',
                                  completed_at = NOW(), claim_token = NULL
                                  WHERE request_id = $1 AND state = 'running' AND claim_token = $2",
        )
        .bind(request)
        .bind(token)
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
                     WHERE request_id = $1 AND state = 'running' AND claim_token = $2",
        )
        .bind(request)
        .bind(token)
        .execute(&self.pool)
        .await
        .map_err(db_error)?;
        Ok(())
    }
}
