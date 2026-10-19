//! Explicit operator reconciliation of an uncertain channel execution.
//!
//! This does not cancel Discord requests or prove an effect did not apply. The
//! operator must first quiesce workers and reconcile Discord's actual state.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::{ChannelModerationStore, Secret};

/// One consistent read of the lane and its request ledger. Ownership tokens stay
/// private/redacted; the printable fingerprint binds a later CLI confirmation to
/// this inspection without publishing either ownership capability.
#[derive(Clone)]
pub struct ChannelLaneInspection {
    guild_id: String,
    channel_id: String,
    claim_key: String,
    lane_token: Secret<String>,
    claim_token: Secret<String>,
    claim: Value,
    generation: String,
}

impl std::fmt::Debug for ChannelLaneInspection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ChannelLaneInspection")
            .field("report", &self.report())
            .finish()
    }
}

impl ChannelLaneInspection {
    #[must_use]
    pub fn generation(&self) -> &str {
        &self.generation
    }

    #[must_use]
    pub fn releasable(&self) -> bool {
        self.claim["state"] == "in_flight" && self.lane_token == self.claim_token
    }

    /// Operator-visible rows, with ownership tokens and the actor-derived request
    /// hash redacted from both stdout and the durable release audit.
    #[must_use]
    pub fn report(&self) -> Value {
        let mut claim = self.claim.clone();
        claim["claim_token"] = json!("[REDACTED]");
        claim["request_hash"] = json!("[REDACTED]");
        json!({
            "expected_generation": self.generation,
            "moderation_channel_executions": {
                "guild_id": self.guild_id,
                "channel_id": self.channel_id,
                "idempotency_key": self.claim_key,
                "claim_token": "[REDACTED]",
            },
            "moderation_idempotency": claim,
        })
    }
}

impl ChannelModerationStore {
    /// Inspect only the exact `(guild, channel, key)` pair, in one SQL snapshot.
    /// A foreign lane/key, missing row or inconsistent ledger is never inferred.
    pub async fn inspect_channel_lane(
        &self,
        guild_id: &str,
        channel_id: &str,
        claim_key: &str,
    ) -> Result<Option<ChannelLaneInspection>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT e.claim_token AS lane_token, i.claim_token,
                    (to_jsonb(i) - 'claim_token')::text AS claim_json
               FROM moderation_channel_executions e
               JOIN moderation_idempotency i
                 ON i.guild_id = e.guild_id AND i.idempotency_key = e.idempotency_key
              WHERE e.guild_id = $1 AND e.channel_id = $2 AND e.idempotency_key = $3",
        )
        .bind(guild_id)
        .bind(channel_id)
        .bind(claim_key)
        .fetch_optional(self.pool())
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let lane_token = Secret::new(row.get::<String, _>("lane_token"));
        let claim_token = Secret::new(row.get::<String, _>("claim_token"));
        let mut claim: Value = serde_json::from_str(row.get::<&str, _>("claim_json"))
            .map_err(|_| sqlx::Error::InvalidArgument("invalid lane inspection".to_owned()))?;
        // Hash a structured, unambiguous snapshot, not a concatenation of inputs.
        let fingerprint = json!([
            guild_id,
            channel_id,
            claim_key,
            lane_token.expose(),
            claim_token.expose(),
            claim
        ]);
        let generation = hex::encode(Sha256::digest(fingerprint.to_string().as_bytes()));
        claim["claim_token"] = json!("[REDACTED]");
        Ok(Some(ChannelLaneInspection {
            guild_id: guild_id.to_owned(),
            channel_id: channel_id.to_owned(),
            claim_key: claim_key.to_owned(),
            lane_token,
            claim_token,
            claim,
            generation,
        }))
    }

    /// Retire an inspected in-flight attempt and release exactly its lane.
    /// Returns the audit request ID, or `None` for a stale/done/inconsistent claim.
    /// Compare-and-delete, ledger retirement and the audit commit together. The
    /// recovery seed is never touched. Retaining a terminal request tombstone
    /// prevents a delayed delivery of the old key from repeating its REST effect.
    ///
    /// Only an authorized, explicitly confirmed operator may invoke this seam;
    /// runtime failure handling must continue to retain ambiguous executions.
    pub async fn force_release_channel_lane(
        &self,
        inspection: &ChannelLaneInspection,
        operator_id: &str,
        reason: &str,
    ) -> Result<Option<String>, sqlx::Error> {
        if !inspection.releasable() {
            return Ok(None);
        }
        if operator_id.trim().is_empty() || reason.trim().is_empty() {
            return Err(sqlx::Error::InvalidArgument(
                "operator identity and reconciliation reason are required".to_owned(),
            ));
        }
        let mut tx = self.pool().begin().await?;
        let removed = sqlx::query(
            "DELETE FROM moderation_channel_executions
              WHERE guild_id = $1 AND channel_id = $2
                AND idempotency_key = $3 AND claim_token = $4",
        )
        .bind(&inspection.guild_id)
        .bind(&inspection.channel_id)
        .bind(&inspection.claim_key)
        .bind(inspection.lane_token.expose())
        .execute(&mut *tx)
        .await?;
        if removed.rows_affected() != 1 {
            return Ok(None);
        }
        // The erased request hash makes claim() refuse every reuse of this key.
        let result = json!({
            "text": "An operator reconciled and released this attempt; no Discord mutation was retried.",
            "outcome": "operator_released",
            "replayed": false,
        });
        let retired = sqlx::query(
            "UPDATE moderation_idempotency
                SET state = 'done', outcome = 'operator_released',
                    claim_token = 'operator_released', request_hash = 'operator_released',
                    result_json = $1, completed_at = NOW()
              WHERE guild_id = $2 AND idempotency_key = $3 AND claim_token = $4
                AND state = 'in_flight' AND action = $5 AND request_hash = $6
              RETURNING current_user::text AS database_role, session_user::text AS database_login",
        )
        .bind(result.to_string())
        .bind(&inspection.guild_id)
        .bind(&inspection.claim_key)
        .bind(inspection.claim_token.expose())
        .bind(inspection.claim["action"].as_str())
        .bind(inspection.claim["request_hash"].as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(retired) = retired else {
            // Dropping the transaction rolls back the lane deletion too.
            return Ok(None);
        };
        let metadata = json!({
            "previous": inspection.report(),
            "database_role": retired.get::<String, _>("database_role"),
            "database_login": retired.get::<String, _>("database_login"),
            "recovery_seed": "untouched",
        });
        // No ON CONFLICT suppression: an absent audit must never free the lane.
        let audit_id: String = sqlx::query_scalar(
            "INSERT INTO moderation_audit
               (request_id, guild_id, actor_id, action, target_id, channel_id, reason,
                outcome, idempotency_key, metadata_json, created_at)
             VALUES ('operator-channel-release:' || pg_catalog.gen_random_uuid()::text,
                     $1, $2, 'moderation.channel_lane_release', NULL, $3, $4,
                     'operator_released', $5, $6, NOW())
             RETURNING request_id",
        )
        .bind(&inspection.guild_id)
        .bind(operator_id)
        .bind(&inspection.channel_id)
        .bind(reason)
        .bind(&inspection.claim_key)
        .bind(metadata.to_string())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(audit_id))
    }
}
