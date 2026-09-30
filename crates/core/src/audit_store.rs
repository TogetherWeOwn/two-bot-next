//! Durable operational audit rows; no Discord client or runtime activation.
//!
//! `record` must succeed before downstream delivery. Every claim is an atomic
//! UPDATE RETURNING, and all owner writes compare both token and generation.
//! Prepared/accepted deliveries can only be reconciled, never blindly resent.
//! See `docs/audit-store.md` for the downstream protocol and failure policies.

use sqlx::{postgres::PgRow, PgPool, Postgres, Row, Transaction};
use time::OffsetDateTime;

use crate::audit::{delivery_nonce, AuditChannel, AuditEvent, AuditKind};

pub const DELIVERY_LEASE_SECONDS: i64 = 300;
pub const MAX_PENDING_ROWS: i64 = 25;

#[derive(Debug, thiserror::Error)]
pub enum AuditStoreError {
    #[error("audit store database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("invalid audit row: {0}")]
    Invalid(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryState {
    None,
    Pending,
    Delivering,
    Delivered,
    Quarantined,
}

impl DeliveryState {
    fn parse(value: &str) -> Result<Self, AuditStoreError> {
        match value {
            "none" => Ok(Self::None),
            "pending" => Ok(Self::Pending),
            "delivering" => Ok(Self::Delivering),
            "delivered" => Ok(Self::Delivered),
            "quarantined" => Ok(Self::Quarantined),
            _ => Err(AuditStoreError::Invalid("delivery state")),
        }
    }
}

/// Recovery never authorizes a POST, even if a bounded scan finds no marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryIntent {
    Send,
    Reconcile,
}

fn delivery_intent(search_before: Option<&str>, message_id: Option<&str>) -> DeliveryIntent {
    if search_before.is_some() || message_id.is_some() {
        DeliveryIntent::Reconcile
    } else {
        DeliveryIntent::Send
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAudit {
    pub event: AuditEvent,
    /// Immutable destination captured by the first insert; never rerouted.
    pub mirror_channel_id: Option<String>,
    pub state: DeliveryState,
    pub attempts: i32,
    pub attempted_at: Option<OffsetDateTime>,
    pub nonce: Option<String>,
    pub search_before: Option<String>,
    pub mirror_message_id: Option<String>,
    pub accepted_at: Option<OffsetDateTime>,
    pub mirrored_at: Option<OffsetDateTime>,
    pub mirror_checked_at: Option<OffsetDateTime>,
    pub last_error: Option<String>,
}

/// Opaque ownership capability, constructed only by a successful store claim.
/// A cloned capability still belongs to the same attempt, not a new worker.
#[derive(Debug, Clone)]
pub struct AuditClaim {
    row: StoredAudit,
    token: String,
    generation: i64,
}

impl AuditClaim {
    pub fn row(&self) -> &StoredAudit {
        &self.row
    }

    pub fn intent(&self) -> DeliveryIntent {
        delivery_intent(
            self.row.search_before.as_deref(),
            self.row.mirror_message_id.as_deref(),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrepareSend {
    Prepared,
    Halted,
    LostClaim,
}

/// Classifications are bounded constants, not Discord errors or free-form text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryFailure {
    /// Caller has authoritative evidence no message was accepted (e.g. 4xx).
    DefinitelyRejected,
    /// Timeout, disconnect, or any uncertainty about acceptance.
    UncertainAcceptance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuarantineReason {
    MarkerMissing,
    PermissionRevoked,
    EvidenceConflict,
}

impl QuarantineReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::MarkerMissing => "discord_marker_missing",
            Self::PermissionRevoked => "mirror_channel_permission_revoked",
            Self::EvidenceConflict => "delivery_evidence_conflict",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryHalt {
    pub engaged_at: OffsetDateTime,
    pub engaged_by: String,
}

#[derive(Debug, Clone)]
pub struct AuditStore {
    pool: PgPool,
}

impl AuditStore {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    /// Atomically insert facts + pending delivery. Replays never merge or
    /// overwrite original facts, metadata, routing, or delivery identity.
    /// Metadata must come from the metadata-only classifiers, not raw payloads.
    pub async fn record(
        &self,
        event: &AuditEvent,
        mirror_channel_id: Option<&str>,
    ) -> Result<bool, AuditStoreError> {
        let metadata: serde_json::Value = serde_json::from_str(&event.metadata_json)
            .map_err(|_| AuditStoreError::Invalid("metadata JSON"))?;
        if !metadata.is_object() || event.channel != AuditChannel::for_kind(event.kind) {
            return Err(AuditStoreError::Invalid("metadata object or kind/channel"));
        }
        let mirror = mirror_channel_id.filter(|id| !id.is_empty());
        let nonce = mirror.map(|_| delivery_nonce(&event.entry_id));
        Ok(sqlx::query(
            "INSERT INTO operational_audit_log
             (entry_id, event_kind, guild_id, occurred_at, actor_id, target_id,
              source_channel_id, destination_channel_id, message_id, action,
              metadata_json, created_at, mirror_channel_id, delivery_state, delivery_nonce)
             VALUES ($1,$2,$3,$4::timestamptz,$5,$6,$7,$8,$9,$10,$11,clock_timestamp(),$12,$13,$14)
             ON CONFLICT (entry_id) DO NOTHING",
        )
        .bind(&event.entry_id)
        .bind(event.kind.as_str())
        .bind(&event.guild_id)
        .bind(&event.occurred_at)
        .bind(&event.actor_id)
        .bind(&event.target_id)
        .bind(&event.source_channel_id)
        .bind(&event.destination_channel_id)
        .bind(&event.message_id)
        .bind(&event.action)
        .bind(&event.metadata_json)
        .bind(mirror)
        .bind(if mirror.is_some() { "pending" } else { "none" })
        .bind(nonce)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    pub async fn get(&self, entry_id: &str) -> Result<Option<StoredAudit>, AuditStoreError> {
        let row = sqlx::query(
            "SELECT *, to_char(occurred_at AT TIME ZONE 'UTC',
               'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS occurred_iso
             FROM operational_audit_log WHERE entry_id = $1",
        )
        .bind(entry_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| stored(&row)).transpose()
    }

    /// Bounded queue discovery, not ownership or send authorization. A switch
    /// engaged after discovery is observed again by each claim/prepare.
    pub async fn pending_ids(&self) -> Result<Vec<String>, AuditStoreError> {
        Ok(sqlx::query_scalar(
            "SELECT entry_id FROM operational_audit_log
             WHERE mirror_channel_id IS NOT NULL AND event_kind = ANY($1)
               AND (delivery_state = 'pending' OR (delivery_state = 'delivering'
                 AND delivery_lease_until <= clock_timestamp()))
               AND NOT EXISTS (SELECT 1 FROM audit_kill_switch WHERE id = 1)
             ORDER BY delivery_attempted_at NULLS FIRST, created_at, entry_id LIMIT 25",
        )
        .bind(KINDS)
        .fetch_all(&self.pool)
        .await?)
    }

    /// UPDATE RETURNING returns only this winner's ownership. Lease expiry
    /// rotates both fences but retains every ambiguity/acceptance marker.
    pub async fn claim(&self, entry_id: &str) -> Result<Option<AuditClaim>, AuditStoreError> {
        let mut tx = self.pool.begin().await?;
        lock_halt(&mut tx, false).await?;
        let row = sqlx::query(
            "UPDATE operational_audit_log SET delivery_state = 'delivering',
               delivery_claim_token = gen_random_uuid()::text,
               delivery_generation = delivery_generation + 1,
               delivery_lease_until = clock_timestamp() + interval '5 minutes',
               delivery_nonce = COALESCE(delivery_nonce, $2)
             WHERE entry_id = $1 AND mirror_channel_id IS NOT NULL AND event_kind = ANY($3)
               AND (delivery_state = 'pending' OR (delivery_state = 'delivering'
                 AND delivery_lease_until <= clock_timestamp()))
               AND NOT EXISTS (SELECT 1 FROM audit_kill_switch WHERE id = 1)
             RETURNING *, to_char(occurred_at AT TIME ZONE 'UTC',
               'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS occurred_iso",
        )
        .bind(entry_id)
        .bind(delivery_nonce(entry_id))
        .bind(KINDS)
        .fetch_optional(&mut *tx)
        .await?;
        let claim = row
            .map(|row| -> Result<_, AuditStoreError> {
                Ok(AuditClaim {
                    row: stored(&row)?,
                    token: row.try_get("delivery_claim_token")?,
                    generation: row.try_get("delivery_generation")?,
                })
            })
            .transpose()?;
        tx.commit().await?;
        Ok(claim)
    }

    /// Persist recovery boundary and count the attempt BEFORE any Discord POST.
    /// Repeated calls, expired owners, and recovery claims cannot prepare a send.
    /// A halted claim remains unattempted and may be released safely.
    pub async fn prepare_send(
        &self,
        claim: &AuditClaim,
        search_before: &str,
    ) -> Result<PrepareSend, AuditStoreError> {
        if !snowflake_cursor(search_before) {
            return Err(AuditStoreError::Invalid("search cursor"));
        }
        if claim.intent() != DeliveryIntent::Send {
            return Ok(PrepareSend::LostClaim);
        }
        let mut tx = self.pool.begin().await?;
        lock_halt(&mut tx, false).await?;
        let halted: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM audit_kill_switch WHERE id = 1)")
                .fetch_one(&mut *tx)
                .await?;
        if halted {
            tx.commit().await?;
            return Ok(PrepareSend::Halted);
        }
        let updated = sqlx::query(
            "UPDATE operational_audit_log SET delivery_search_before = $4,
               delivery_attempts = delivery_attempts + 1,
               delivery_attempted_at = clock_timestamp(), delivery_last_error = NULL,
               delivery_lease_until = clock_timestamp() + interval '5 minutes'
             WHERE entry_id = $1 AND delivery_claim_token = $2 AND delivery_generation = $3
               AND delivery_state = 'delivering' AND delivery_lease_until > clock_timestamp()
               AND delivery_search_before IS NULL AND mirror_message_id IS NULL",
        )
        .bind(&claim.row.event.entry_id)
        .bind(&claim.token)
        .bind(claim.generation)
        .bind(search_before)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        tx.commit().await?;
        Ok(if updated {
            PrepareSend::Prepared
        } else {
            PrepareSend::LostClaim
        })
    }

    /// Record the ID returned by Discord, or verified through marker recovery,
    /// separately from completion. First evidence wins; conflicting/repeated
    /// acknowledgements cannot replace it. Halt does not discard acceptance.
    pub async fn note_accepted(
        &self,
        claim: &AuditClaim,
        message_id: &str,
    ) -> Result<bool, AuditStoreError> {
        if !snowflake_id(message_id) {
            return Err(AuditStoreError::Invalid("accepted message ID"));
        }
        Ok(sqlx::query(
            "UPDATE operational_audit_log SET mirror_message_id = $4,
               delivery_accepted_at = clock_timestamp(), delivery_last_error = NULL
             WHERE entry_id = $1 AND delivery_claim_token = $2 AND delivery_generation = $3
               AND delivery_state = 'delivering' AND delivery_lease_until > clock_timestamp()
               AND delivery_search_before IS NOT NULL AND mirror_message_id IS NULL",
        )
        .bind(&claim.row.event.entry_id)
        .bind(&claim.token)
        .bind(claim.generation)
        .bind(message_id)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Complete only persisted acceptance. Recovery can finish an earlier
    /// accepted ID after restart. Delivered rows are terminal and immutable.
    pub async fn complete(&self, claim: &AuditClaim) -> Result<bool, AuditStoreError> {
        Ok(sqlx::query(
            "UPDATE operational_audit_log SET delivery_state = 'delivered',
               mirrored_at = clock_timestamp(), delivery_last_error = NULL,
               delivery_claim_token = NULL, delivery_lease_until = NULL
             WHERE entry_id = $1 AND delivery_claim_token = $2 AND delivery_generation = $3
               AND delivery_state = 'delivering' AND delivery_lease_until > clock_timestamp()
               AND mirror_message_id IS NOT NULL AND delivery_accepted_at IS NOT NULL",
        )
        .bind(&claim.row.event.entry_id)
        .bind(&claim.token)
        .bind(claim.generation)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Release only a claim for which no send was prepared. Safe for halt or
    /// preflight failure; never clears earlier ambiguous acceptance evidence.
    pub async fn release_unattempted(&self, claim: &AuditClaim) -> Result<bool, AuditStoreError> {
        Ok(sqlx::query(
            "UPDATE operational_audit_log SET delivery_state = 'pending',
               delivery_claim_token = NULL, delivery_lease_until = NULL
             WHERE entry_id = $1 AND delivery_claim_token = $2 AND delivery_generation = $3
               AND delivery_state = 'delivering' AND delivery_lease_until > clock_timestamp()
               AND delivery_search_before IS NULL AND mirror_message_id IS NULL",
        )
        .bind(&claim.row.event.entry_id)
        .bind(&claim.token)
        .bind(claim.generation)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Only an authoritative non-acceptance permits another send. Uncertainty
    /// preserves the boundary so the next claim is reconciliation-only.
    /// Neither classification can clear an already accepted message ID.
    pub async fn fail_attempt(
        &self,
        claim: &AuditClaim,
        failure: DeliveryFailure,
    ) -> Result<bool, AuditStoreError> {
        let definite = failure == DeliveryFailure::DefinitelyRejected;
        // A read rejection during reconciliation says nothing about whether
        // the ORIGINAL POST was accepted. Only its sending owner can prove
        // non-acceptance and clear that attempt's boundary.
        if definite && claim.intent() != DeliveryIntent::Send {
            return Ok(false);
        }
        Ok(sqlx::query(
            "UPDATE operational_audit_log SET delivery_state = 'pending',
               delivery_claim_token = NULL, delivery_lease_until = NULL,
               delivery_last_error = $4,
               delivery_search_before = CASE WHEN $5 THEN NULL ELSE delivery_search_before END
             WHERE entry_id = $1 AND delivery_claim_token = $2 AND delivery_generation = $3
               AND delivery_state = 'delivering' AND delivery_lease_until > clock_timestamp()
               AND delivery_search_before IS NOT NULL AND mirror_message_id IS NULL",
        )
        .bind(&claim.row.event.entry_id)
        .bind(&claim.token)
        .bind(claim.generation)
        .bind(if definite {
            "discord_send_rejected"
        } else {
            "discord_post_ambiguous"
        })
        .bind(definite)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Renew an active owner only; expiration cannot be undone by a stale
    /// worker. Recovery may renew while scanning, but may never POST.
    pub async fn renew(&self, claim: &AuditClaim) -> Result<bool, AuditStoreError> {
        Ok(sqlx::query(
            "UPDATE operational_audit_log SET delivery_lease_until = clock_timestamp() + interval '5 minutes'
             WHERE entry_id = $1 AND delivery_claim_token = $2 AND delivery_generation = $3
               AND delivery_state = 'delivering' AND delivery_lease_until > clock_timestamp()",
        ).bind(&claim.row.event.entry_id).bind(&claim.token).bind(claim.generation)
            .execute(&self.pool).await?.rows_affected() == 1)
    }

    pub async fn quarantine(
        &self,
        claim: &AuditClaim,
        reason: QuarantineReason,
    ) -> Result<bool, AuditStoreError> {
        Ok(sqlx::query(
            "UPDATE operational_audit_log SET delivery_state = 'quarantined',
               delivery_last_error = $4, delivery_claim_token = NULL, delivery_lease_until = NULL
             WHERE entry_id = $1 AND delivery_claim_token = $2 AND delivery_generation = $3
               AND delivery_state = 'delivering' AND delivery_lease_until > clock_timestamp()",
        )
        .bind(&claim.row.event.entry_id)
        .bind(&claim.token)
        .bind(claim.generation)
        .bind(reason.as_str())
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Return errors unchanged. The pure core models legacy switch-read
    /// fail-open; it must not convert a failed audit write into send permission.
    pub async fn delivery_halt(&self) -> Result<Option<DeliveryHalt>, AuditStoreError> {
        let row = sqlx::query("SELECT engaged_at, engaged_by FROM audit_kill_switch WHERE id = 1")
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| {
            Ok(DeliveryHalt {
                engaged_at: r.try_get("engaged_at")?,
                engaged_by: r.try_get("engaged_by")?,
            })
        })
        .transpose()
    }

    pub async fn engage_halt(&self, engaged_by: &str) -> Result<bool, AuditStoreError> {
        if !snowflake_id(engaged_by) {
            return Err(AuditStoreError::Invalid("halt actor ID"));
        }
        let mut tx = self.pool.begin().await?;
        lock_halt(&mut tx, true).await?;
        let inserted = sqlx::query(
            "INSERT INTO audit_kill_switch (id, engaged_at, engaged_by)
             VALUES (1,clock_timestamp(),$1) ON CONFLICT (id) DO NOTHING",
        )
        .bind(engaged_by)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        tx.commit().await?;
        Ok(inserted)
    }

    pub async fn disengage_halt(&self) -> Result<bool, AuditStoreError> {
        let mut tx = self.pool.begin().await?;
        lock_halt(&mut tx, true).await?;
        let removed = sqlx::query("DELETE FROM audit_kill_switch WHERE id = 1")
            .execute(&mut *tx)
            .await?
            .rows_affected()
            == 1;
        tx.commit().await?;
        Ok(removed)
    }
}

// Serialize halt changes with claim/send preparation even when the switch row
// is absent. Locks end before any network send; this is not a Discord lock.
// Source: https://www.postgresql.org/docs/current/explicit-locking.html
async fn lock_halt(tx: &mut Transaction<'_, Postgres>, write: bool) -> Result<(), sqlx::Error> {
    // Shared readers allow independent workers to race the row-level UPDATE.
    // A database-wide key also works when pools use different search paths
    // that resolve to the same tables. Hash collision only adds contention.
    let query = if write {
        "SELECT pg_advisory_xact_lock(hashtextextended('two-bot-next:audit-delivery-halt', 0))"
    } else {
        "SELECT pg_advisory_xact_lock_shared(hashtextextended('two-bot-next:audit-delivery-halt', 0))"
    };
    sqlx::query(query).execute(&mut **tx).await?;
    Ok(())
}

const KINDS: &[&str] = &[
    "message_edit",
    "message_delete",
    "member_update",
    "voice_join",
    "voice_leave",
    "voice_move",
    "moderation_action",
];

fn stored(row: &PgRow) -> Result<StoredAudit, AuditStoreError> {
    let kind = match row.try_get::<&str, _>("event_kind")? {
        "message_edit" => AuditKind::MessageEdit,
        "message_delete" => AuditKind::MessageDelete,
        "member_update" => AuditKind::MemberUpdate,
        "voice_join" => AuditKind::VoiceJoin,
        "voice_leave" => AuditKind::VoiceLeave,
        "voice_move" => AuditKind::VoiceMove,
        "moderation_action" => AuditKind::ModerationAction,
        _ => return Err(AuditStoreError::Invalid("operational kind")),
    };
    Ok(StoredAudit {
        event: AuditEvent {
            entry_id: row.try_get("entry_id")?,
            kind,
            channel: AuditChannel::for_kind(kind),
            guild_id: row.try_get("guild_id")?,
            occurred_at: row.try_get("occurred_iso")?,
            actor_id: row.try_get("actor_id")?,
            target_id: row.try_get("target_id")?,
            source_channel_id: row.try_get("source_channel_id")?,
            destination_channel_id: row.try_get("destination_channel_id")?,
            message_id: row.try_get("message_id")?,
            action: row.try_get("action")?,
            metadata_json: row.try_get("metadata_json")?,
        },
        mirror_channel_id: row.try_get("mirror_channel_id")?,
        state: DeliveryState::parse(row.try_get("delivery_state")?)?,
        attempts: row.try_get("delivery_attempts")?,
        attempted_at: row.try_get("delivery_attempted_at")?,
        nonce: row.try_get("delivery_nonce")?,
        search_before: row.try_get("delivery_search_before")?,
        mirror_message_id: row.try_get("mirror_message_id")?,
        accepted_at: row.try_get("delivery_accepted_at")?,
        mirrored_at: row.try_get("mirrored_at")?,
        mirror_checked_at: row.try_get("mirror_checked_at")?,
        last_error: row.try_get("delivery_last_error")?,
    })
}

fn snowflake_cursor(value: &str) -> bool {
    !value.is_empty() && value.len() <= 20 && value.bytes().all(|b| b.is_ascii_digit())
}

fn snowflake_id(value: &str) -> bool {
    snowflake_cursor(value) && value.bytes().any(|b| b != b'0')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_recovery_evidence_forbids_a_send() {
        assert_eq!(delivery_intent(None, None), DeliveryIntent::Send);
        for (cursor, message) in [
            (Some("0"), None),
            (None, Some("12")),
            (Some("1"), Some("2")),
        ] {
            assert_eq!(delivery_intent(cursor, message), DeliveryIntent::Reconcile);
        }
    }

    #[test]
    fn legacy_states_and_cursor_validation_are_explicit() {
        for value in ["none", "pending", "delivering", "delivered", "quarantined"] {
            assert!(DeliveryState::parse(value).is_ok());
        }
        assert!(DeliveryState::parse("resend").is_err());
        assert!(snowflake_cursor("0"));
        assert!(snowflake_id("18446744073709551615"));
        for bad in ["", "0", "name", "123456789012345678901", "-1"] {
            assert!(!snowflake_id(bad));
        }
    }
}
