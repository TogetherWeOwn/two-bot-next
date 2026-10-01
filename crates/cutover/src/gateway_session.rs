//! Postgres gateway checkpoint + funnel batch transaction. Uses the existing
//! cutover migrations and members projection rather than a second event store.
//!
//! sqlx 0.9 transaction executor: <https://docs.rs/sqlx/0.9.0/sqlx/struct.Transaction.html>

use sqlx::{PgConnection, PgPool};
use two_bot_core::gateway_funnel::{FunnelBatch, SnapshotWrite};
use two_bot_core::gateway_session::{dispatch_action, DispatchAction, GatewaySession};
use two_bot_core::{format_iso_millis, idempotency_key, FunnelEvent};

use crate::db::{project_event, FunnelWrite};

/// A captured feature job, not a second owner of membership funnel facts.
/// Payloads must contain data only, never credentials or callback tokens.
pub struct GatewayJob {
    pub payload: String,
    pub occurred_at_ms: i64,
}

pub struct ClaimedGatewayJob {
    pub id: i64,
    pub payload: String,
    pub occurred_at_ms: i64,
}

#[derive(Clone)]
pub struct GatewaySessionStore {
    pool: PgPool,
    guild_id: String,
    shard_id: i32,
}

impl GatewaySessionStore {
    pub fn new(pool: PgPool, guild_id: String, shard_id: i32) -> Self {
        Self {
            pool,
            guild_id,
            shard_id,
        }
    }

    pub async fn load(&self) -> Result<Option<GatewaySession>, sqlx::Error> {
        self.read(&mut *self.pool.acquire().await?).await
    }

    async fn read(
        &self,
        connection: &mut PgConnection,
    ) -> Result<Option<GatewaySession>, sqlx::Error> {
        let row: Option<(String, i64, String, i64)> = sqlx::query_as(
            "SELECT session_id, seq, resume_url, floor(extract(epoch FROM updated_at) * 1000)::bigint
             FROM gateway_sessions WHERE guild_id = $1 AND shard_id = $2",
        ).bind(&self.guild_id).bind(self.shard_id).fetch_optional(connection).await?;
        row.map(|(session_id, sequence, resume_url, updated_at_ms)| {
            Ok(GatewaySession {
                session_id,
                sequence: u64::try_from(sequence).map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
                resume_url,
                updated_at_ms,
            })
        })
        .transpose()
    }

    pub async fn clear(&self) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM gateway_sessions WHERE guild_id = $1 AND shard_id = $2")
            .bind(&self.guild_id)
            .bind(self.shard_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Never advances the checkpoint ahead of effects. A crash before commit
    /// rolls both back; a crash after commit resumes beyond this dispatch.
    pub async fn commit_dispatch(
        &self,
        session: &GatewaySession,
        batch: FunnelBatch,
    ) -> Result<DispatchAction, sqlx::Error> {
        self.commit_dispatch_with_job(session, batch, None)
            .await
            .map(|(action, _)| action)
    }

    /// The captured pre-pipeline state becomes durable in the same transaction
    /// as the S3 facts and checkpoint. Restart recovery does not depend on cache.
    pub async fn commit_dispatch_with_job(
        &self,
        session: &GatewaySession,
        batch: FunnelBatch,
        job: Option<GatewayJob>,
    ) -> Result<(DispatchAction, Option<i64>), sqlx::Error> {
        let sequence =
            i64::try_from(session.sequence).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
        let mut tx = self.pool.begin().await?;
        // Serialize even the first checkpoint (there may be no row to lock).
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("gateway:{}:{}", self.guild_id, self.shard_id))
            .execute(&mut *tx)
            .await?;
        let saved = self.read(&mut tx).await?;
        if dispatch_action(saved.as_ref(), &session.session_id, session.sequence)
            == DispatchAction::Duplicate
        {
            tx.rollback().await?;
            return Ok((DispatchAction::Duplicate, None));
        }
        let job_id = if let Some(job) = job {
            let queued: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM gateway_onboarding_jobs
                 WHERE guild_id = $1 AND shard_id = $2
                 AND state IN ('pending', 'running', 'failed')",
            )
            .bind(&self.guild_id)
            .bind(self.shard_id)
            .fetch_one(&mut *tx)
            .await?;
            if queued >= 32 {
                return Err(sqlx::Error::InvalidArgument(
                    "onboarding durable queue capacity exceeded; checkpoint unchanged".into(),
                ));
            }
            Some(
                sqlx::query_scalar::<_, i64>(
                    "INSERT INTO gateway_onboarding_jobs
                 (guild_id, shard_id, session_id, seq, occurred_at_ms, payload)
                 VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
                )
                .bind(&self.guild_id)
                .bind(self.shard_id)
                .bind(&session.session_id)
                .bind(sequence)
                .bind(job.occurred_at_ms)
                .bind(job.payload)
                .fetch_one(&mut *tx)
                .await?,
            )
        } else {
            None
        };
        for (guild_id, member_id) in batch.bots {
            if guild_id.to_string() != self.guild_id {
                return Err(sqlx::Error::InvalidArgument(
                    "gateway bot belongs to another guild".into(),
                ));
            }
            sqlx::query(
                "INSERT INTO members (guild_id, member_id, is_bot) VALUES ($1, $2, TRUE)
                 ON CONFLICT (guild_id, member_id) DO UPDATE SET is_bot = TRUE",
            )
            .bind(&self.guild_id)
            .bind(member_id.to_string())
            .execute(&mut *tx)
            .await?;
        }
        for snapshot in batch.snapshots {
            let guild_id = match &snapshot {
                SnapshotWrite::StoreAll(guild_id, _)
                | SnapshotWrite::DeleteMissing(guild_id, _) => *guild_id,
            };
            if guild_id.to_string() != self.guild_id {
                return Err(sqlx::Error::InvalidArgument(
                    "gateway snapshot belongs to another guild".into(),
                ));
            }
            match snapshot {
                SnapshotWrite::StoreAll(_, states) => {
                    for state in states {
                        sqlx::query(
                            "INSERT INTO invite_snapshots (guild_id, code, uses, inviter_id, channel_id, updated_at)
                             VALUES ($1, $2, $3, $4, $5, to_timestamp($6::double precision / 1000))
                             ON CONFLICT (guild_id, code) DO UPDATE SET uses = EXCLUDED.uses,
                             inviter_id = EXCLUDED.inviter_id, channel_id = EXCLUDED.channel_id, updated_at = EXCLUDED.updated_at",
                        ).bind(&self.guild_id).bind(state.code)
                            .bind(i32::try_from(state.uses).map_err(|e| sqlx::Error::Decode(Box::new(e)))?)
                            .bind(state.inviter_id.map(|id| id.to_string()))
                            .bind(state.channel_id.map(|id| id.to_string()))
                            .bind(session.updated_at_ms).execute(&mut *tx).await?;
                    }
                }
                SnapshotWrite::DeleteMissing(_, live) => {
                    sqlx::query(
                        "DELETE FROM invite_snapshots WHERE guild_id = $1 AND code <> ALL($2)",
                    )
                    .bind(&self.guild_id)
                    .bind(live.into_iter().collect::<Vec<_>>())
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
        for event in batch.events {
            if event.guild_id.to_string() != self.guild_id {
                return Err(sqlx::Error::InvalidArgument(
                    "gateway batch belongs to another guild".into(),
                ));
            }
            let metadata = event.metadata.as_ref().map(serde_json::Value::to_string);
            let write = FunnelWrite {
                guild_id: event.guild_id.to_string(),
                member_id: event.member_id.map(|id| id.to_string()),
                event_type: event.event_type.as_str().to_owned(),
                occurred_at: event.occurred_at.clone(),
                source: event.source.clone(),
                metadata,
            };
            let inserted = sqlx::query(
                "INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
                 VALUES ($1, $2, $3, $4::timestamptz, $5, $6, $7)
                 ON CONFLICT (idempotency_key) DO NOTHING",
            ).bind(&write.event_type).bind(&write.member_id).bind(&write.guild_id)
                .bind(&write.occurred_at).bind(&write.source).bind(&write.metadata)
                .bind(idempotency_key(&event)).execute(&mut *tx).await?;
            if inserted.rows_affected() == 1 {
                project_event(&mut tx, &write).await?;
            }
        }
        for (guild_id, member_id, at) in batch.activity {
            if guild_id.to_string() != self.guild_id {
                return Err(sqlx::Error::InvalidArgument(
                    "gateway activity belongs to another guild".into(),
                ));
            }
            sqlx::query(
                "INSERT INTO members (guild_id, member_id, last_active_at) VALUES ($1, $2, $3::timestamptz)
                 ON CONFLICT (guild_id, member_id) DO UPDATE SET last_active_at = EXCLUDED.last_active_at
                 WHERE members.last_active_at IS NULL OR members.last_active_at < EXCLUDED.last_active_at",
            ).bind(&self.guild_id).bind(member_id.to_string()).bind(&at).execute(&mut *tx).await?;
        }
        for snapshot in batch.invite_snapshots {
            if snapshot.guild_id.to_string() != self.guild_id {
                return Err(sqlx::Error::InvalidArgument(
                    "gateway invite snapshot belongs to another guild".into(),
                ));
            }
            let mut live_codes = Vec::with_capacity(snapshot.states.len());
            for state in snapshot.states {
                let uses =
                    i32::try_from(state.uses).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
                sqlx::query(
                    "INSERT INTO invite_snapshots (guild_id, code, uses, inviter_id, channel_id, updated_at)
                     VALUES ($1, $2, $3, $4, $5, $6::timestamptz)
                     ON CONFLICT (guild_id, code) DO UPDATE SET
                     uses = EXCLUDED.uses, inviter_id = EXCLUDED.inviter_id,
                     channel_id = EXCLUDED.channel_id, updated_at = EXCLUDED.updated_at",
                )
                .bind(&self.guild_id)
                .bind(&state.code)
                .bind(uses)
                .bind(state.inviter_id.map(|id| id.to_string()))
                .bind(state.channel_id.map(|id| id.to_string()))
                .bind(&snapshot.observed_at)
                .execute(&mut *tx)
                .await?;
                live_codes.push(state.code);
            }
            if snapshot.replace_all {
                sqlx::query(
                    "DELETE FROM invite_snapshots WHERE guild_id = $1 AND NOT (code = ANY($2))",
                )
                .bind(&self.guild_id)
                .bind(&live_codes)
                .execute(&mut *tx)
                .await?;
            }
        }
        sqlx::query(
            "INSERT INTO gateway_sessions (guild_id, shard_id, session_id, seq, resume_url, updated_at)
             VALUES ($1, $2, $3, $4, $5, to_timestamp($6::double precision / 1000))
             ON CONFLICT (guild_id, shard_id) DO UPDATE SET
             session_id = EXCLUDED.session_id, seq = EXCLUDED.seq,
             resume_url = EXCLUDED.resume_url, updated_at = EXCLUDED.updated_at",
        ).bind(&self.guild_id).bind(self.shard_id).bind(&session.session_id)
            .bind(sequence).bind(&session.resume_url).bind(session.updated_at_ms)
            .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok((DispatchAction::Apply, job_id))
    }

    /// A single shard owner calls this once at boot, never while its workers run.
    /// Running rows survived a process interruption; retry at most three times.
    pub async fn recover_onboarding_jobs(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE gateway_onboarding_jobs SET
             state = CASE WHEN attempts >= 3 THEN 'failed' ELSE 'pending' END
             WHERE guild_id = $1 AND shard_id = $2 AND state = 'running'",
        )
        .bind(&self.guild_id)
        .bind(self.shard_id)
        .execute(&self.pool)
        .await?;
        let failed: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM gateway_onboarding_jobs
             WHERE guild_id = $1 AND shard_id = $2 AND state = 'failed')",
        )
        .bind(&self.guild_id)
        .bind(self.shard_id)
        .fetch_one(&self.pool)
        .await?;
        if failed {
            return Err(sqlx::Error::InvalidArgument(
                "onboarding delivery attempt limit reached; recovery requires corrected cause"
                    .into(),
            ));
        }
        Ok(())
    }

    pub async fn claim_onboarding_job(&self) -> Result<Option<ClaimedGatewayJob>, sqlx::Error> {
        let row: Option<(i64, String, i64)> = sqlx::query_as(
            "UPDATE gateway_onboarding_jobs SET state = 'running', attempts = attempts + 1
             WHERE id = (SELECT id FROM gateway_onboarding_jobs
                 WHERE guild_id = $1 AND shard_id = $2 AND state = 'pending' AND attempts < 3
                 ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED)
             RETURNING id, payload, occurred_at_ms",
        )
        .bind(&self.guild_id)
        .bind(self.shard_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id, payload, occurred_at_ms)| ClaimedGatewayJob {
            id,
            payload,
            occurred_at_ms,
        }))
    }

    /// Retain only a delivery receipt on terminal outcomes. In particular, a
    /// token-free interrupted-interaction receipt is not a successful reply.
    pub async fn finish_onboarding_job(
        &self,
        id: i64,
        interrupted: bool,
    ) -> Result<(), sqlx::Error> {
        let result = sqlx::query(
            "UPDATE gateway_onboarding_jobs SET state = $4, payload = NULL
             WHERE id = $1 AND guild_id = $2 AND shard_id = $3 AND state = 'running'",
        )
        .bind(id)
        .bind(&self.guild_id)
        .bind(self.shard_id)
        .bind(if interrupted {
            "interrupted"
        } else {
            "completed"
        })
        .execute(&self.pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(sqlx::Error::InvalidArgument(
                "onboarding delivery receipt missing".into(),
            ));
        }
        Ok(())
    }

    pub async fn invite_snapshots(&self) -> Result<Vec<two_bot_core::InviteState>, sqlx::Error> {
        let rows: Vec<(String, i32, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT code, uses, inviter_id, channel_id FROM invite_snapshots WHERE guild_id = $1",
        )
        .bind(&self.guild_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(code, uses, inviter_id, channel_id)| {
                Ok(two_bot_core::InviteState {
                    code,
                    uses: u64::try_from(uses).map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
                    inviter_id: inviter_id
                        .map(|id| id.parse())
                        .transpose()
                        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
                    channel_id: channel_id
                        .map(|id| id.parse())
                        .transpose()
                        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
                })
            })
            .collect()
    }

    /// The only durable read models the S3 funnel needs on a cold resume.
    /// Repeatable events and open voice durations are deliberately not restored.
    pub async fn milestones(&self) -> Result<Vec<FunnelEvent>, sqlx::Error> {
        type Row = (String, String, time::OffsetDateTime, String, Option<String>);
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT event_type, member_id, occurred_at, source, metadata FROM events
             WHERE guild_id = $1 AND member_id IS NOT NULL AND event_type IN
             ('first_message', 'second_message', 'third_message', 'first_voice_session')",
        )
        .bind(&self.guild_id)
        .fetch_all(&self.pool)
        .await?;
        let guild_id = self
            .guild_id
            .parse()
            .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
        rows.into_iter()
            .map(|(kind, member, at, source, metadata)| {
                Ok(FunnelEvent {
                    guild_id,
                    member_id: Some(
                        member
                            .parse()
                            .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
                    ),
                    event_type: serde_json::from_value(serde_json::Value::String(kind))
                        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
                    occurred_at: format_iso_millis((at.unix_timestamp_nanos() / 1_000_000) as i64),
                    source,
                    metadata: metadata
                        .map(|text| serde_json::from_str(&text))
                        .transpose()
                        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
                    dedupe_token: None,
                })
            })
            .collect()
    }
}
