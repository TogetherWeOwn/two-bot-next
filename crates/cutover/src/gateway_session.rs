//! Postgres gateway checkpoint + funnel batch transaction. Uses the existing
//! cutover migrations and members projection rather than a second event store.
//!
//! sqlx 0.9 transaction executor: <https://docs.rs/sqlx/0.9.0/sqlx/struct.Transaction.html>

use sqlx::{PgConnection, PgPool};
use two_bot_core::gateway_funnel::{FunnelBatch, SnapshotWrite};
use two_bot_core::gateway_session::{
    dispatch_action, BootDirective, DispatchAction, GatewaySession,
};
use two_bot_core::{format_iso_millis, idempotency_key, FunnelEvent};

use crate::db::{project_event, FunnelWrite};

/// Operator view of the one-shot force-fresh directive for this key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForceIdentifyStatus {
    pub armed_at_ms: i64,
    pub reason: String,
    pub consumed_at_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArmOutcome {
    Armed,
    /// A pending directive already exists; it was left unchanged.
    AlreadyArmed,
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

    /// Boot read: one transaction locks this key's pending directive
    /// (`FOR UPDATE`), reads the checkpoint and consumes the directive. A
    /// concurrent boot read waits on the row lock, then re-checks
    /// `consumed_at IS NULL` and finds nothing armed. The checkpoint is only
    /// read; whether to clear it stays the caller's boot policy.
    pub async fn load_for_boot(
        &self,
    ) -> Result<(Option<GatewaySession>, BootDirective), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let armed: Option<i32> = sqlx::query_scalar(
            "SELECT shard_id FROM gateway_boot_directives
             WHERE guild_id = $1 AND shard_id = $2 AND consumed_at IS NULL FOR UPDATE",
        )
        .bind(&self.guild_id)
        .bind(self.shard_id)
        .fetch_optional(&mut *tx)
        .await?;
        let saved = self.read(&mut tx).await?;
        let directive = if armed.is_some() {
            sqlx::query(
                "UPDATE gateway_boot_directives SET consumed_at = now()
                 WHERE guild_id = $1 AND shard_id = $2 AND consumed_at IS NULL",
            )
            .bind(&self.guild_id)
            .bind(self.shard_id)
            .execute(&mut *tx)
            .await?;
            BootDirective::ForceIdentify
        } else {
            BootDirective::None
        };
        tx.commit().await?;
        Ok((saved, directive))
    }

    /// Arms the next boot of this key to IDENTIFY whatever the checkpoint's
    /// age. A pending directive is kept as it is; a consumed one is re-armed.
    /// Never deletes or rewrites the `gateway_sessions` row.
    pub async fn arm_force_identify(&self, reason: &str) -> Result<ArmOutcome, sqlx::Error> {
        let armed = sqlx::query(
            "INSERT INTO gateway_boot_directives (guild_id, shard_id, armed_at, reason)
             VALUES ($1, $2, now(), $3)
             ON CONFLICT (guild_id, shard_id) DO UPDATE SET
             armed_at = EXCLUDED.armed_at, reason = EXCLUDED.reason, consumed_at = NULL
             WHERE gateway_boot_directives.consumed_at IS NOT NULL",
        )
        .bind(&self.guild_id)
        .bind(self.shard_id)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(if armed.rows_affected() == 1 {
            ArmOutcome::Armed
        } else {
            ArmOutcome::AlreadyArmed
        })
    }

    /// Read-only: the directive row for this key, armed or consumed.
    pub async fn force_identify_status(&self) -> Result<Option<ForceIdentifyStatus>, sqlx::Error> {
        let row: Option<(i64, String, Option<i64>)> = sqlx::query_as(
            "SELECT floor(extract(epoch FROM armed_at) * 1000)::bigint, reason,
                    floor(extract(epoch FROM consumed_at) * 1000)::bigint
             FROM gateway_boot_directives WHERE guild_id = $1 AND shard_id = $2",
        )
        .bind(&self.guild_id)
        .bind(self.shard_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(armed_at_ms, reason, consumed_at_ms)| ForceIdentifyStatus {
                armed_at_ms,
                reason,
                consumed_at_ms,
            },
        ))
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
            return Ok(DispatchAction::Duplicate);
        }
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
        Ok(DispatchAction::Apply)
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
