//! Durable guards for internal actions; only a new committed claim permits execution.
//!
//! Stale/unknown intents are never reclaimed. Reconciliation records a proven
//! terminal outcome, not a new execution lease. See `docs/internal-action-store.md`.

use sqlx::{PgPool, Row};

use crate::internal_actions::{
    body_hash, is_implemented, valid_idempotency_key, valid_nonce_format, CLAIM_STALE_SECONDS,
    MAX_BODY_BYTES, NONCE_TTL_SECONDS, SKEW_SECONDS,
};

/// Public errors deliberately omit SQLx sources and all request/provider details.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InternalStoreError {
    #[error("invalid internal-action store input")]
    InvalidInput,
    #[error("internal-action storage unavailable")]
    Unavailable,
    #[error("internal-action transition refused")]
    TransitionRefused,
}

impl From<sqlx::Error> for InternalStoreError {
    fn from(_: sqlx::Error) -> Self {
        Self::Unavailable
    }
}

/// A scalar Discord ID, never an arbitrary response/log string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordId(String);

impl DiscordId {
    pub fn new(value: &str) -> Result<Self, InternalStoreError> {
        if !crate::internal_actions::is_snowflake(value) {
            return Err(InternalStoreError::InvalidInput);
        }
        Ok(Self(value.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Construct only after authorization. Hash exact signed bytes, not parsed JSON.
/// Debug/DB never contain the caller's raw key ID, idempotency key or payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestIdentity {
    caller_hash: String,
    key_hash: String,
    action: String,
    payload_hash: String,
}

impl RequestIdentity {
    pub fn new(
        caller: &str,
        key: &str,
        action: &str,
        authenticated_payload: &[u8],
    ) -> Result<Self, InternalStoreError> {
        if caller.is_empty()
            || caller.len() > 128
            || !caller
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
            || !valid_idempotency_key(key)
            || !is_implemented(action)
            || authenticated_payload.len() > MAX_BODY_BYTES
        {
            return Err(InternalStoreError::InvalidInput);
        }
        Ok(Self {
            caller_hash: body_hash(caller.as_bytes()),
            key_hash: body_hash(key.as_bytes()),
            action: action.to_owned(),
            payload_hash: body_hash(authenticated_payload),
        })
    }
}

/// Subject scalars validated separately from the request body; no free-text reason.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditSubject {
    pub guild_id: Option<DiscordId>,
    pub actor_id: Option<DiscordId>,
    pub target_id: Option<DiscordId>,
}

/// A definitive failure, not a timeout with an unknown Discord outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalFailure {
    Malformed,
    ActionNotAllowed,
    DiscordRejected,
    /// The executor has proved that no side effect occurred.
    NoEffect,
}

/// Minimal replayable response contract. Extend with typed scalars, never raw JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalResponse {
    Success {
        resource_id: Option<DiscordId>,
        affected: u32,
    },
    Failure(TerminalFailure),
}

impl TerminalResponse {
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Success { .. } => "success",
            Self::Failure(TerminalFailure::Malformed) => "malformed",
            Self::Failure(TerminalFailure::ActionNotAllowed) => "action_not_allowed",
            Self::Failure(TerminalFailure::DiscordRejected) => "discord_rejected",
            Self::Failure(TerminalFailure::NoEffect) => "no_effect",
        }
    }

    #[must_use]
    pub fn status(&self) -> u16 {
        match self {
            Self::Success { .. } => 200,
            Self::Failure(TerminalFailure::Malformed) => 400,
            Self::Failure(TerminalFailure::ActionNotAllowed) => 403,
            Self::Failure(TerminalFailure::DiscordRejected) => 422,
            Self::Failure(TerminalFailure::NoEffect) => 502,
        }
    }

    fn resource_id(&self) -> Option<&str> {
        match self {
            Self::Success { resource_id, .. } => resource_id.as_ref().map(DiscordId::as_str),
            Self::Failure(_) => None,
        }
    }

    fn affected(&self) -> Option<i64> {
        match self {
            Self::Success { affected, .. } => Some(i64::from(*affected)),
            Self::Failure(_) => None,
        }
    }

    fn from_row(row: &sqlx::postgres::PgRow) -> Result<Self, InternalStoreError> {
        let code: &str = row.try_get("response_code")?;
        let response = match code {
            "success" => Self::Success {
                resource_id: row
                    .try_get::<Option<&str>, _>("resource_id")?
                    .map(DiscordId::new)
                    .transpose()
                    .map_err(|_| InternalStoreError::Unavailable)?,
                affected: u32::try_from(row.try_get::<i64, _>("affected")?)
                    .map_err(|_| InternalStoreError::Unavailable)?,
            },
            "malformed" => Self::Failure(TerminalFailure::Malformed),
            "action_not_allowed" => Self::Failure(TerminalFailure::ActionNotAllowed),
            "discord_rejected" => Self::Failure(TerminalFailure::DiscordRejected),
            "no_effect" => Self::Failure(TerminalFailure::NoEffect),
            _ => return Err(InternalStoreError::Unavailable),
        };
        if row.try_get::<i32, _>("http_status")? != i32::from(response.status()) {
            return Err(InternalStoreError::Unavailable);
        }
        Ok(response)
    }
}

/// Opaque ownership of a committed intent. Never created from a duplicate claim.
#[derive(Debug)]
pub struct ExecutionClaim {
    intent_id: i64,
    identity: RequestIdentity,
}

impl ExecutionClaim {
    #[must_use]
    pub fn intent_id(&self) -> i64 {
        self.intent_id
    }
}

#[derive(Debug)]
pub enum InternalClaim {
    Claimed(ExecutionClaim),
    InFlight,
    NeedsReconciliation,
    Replay(TerminalResponse),
    Mismatch,
}

/// Only an independently proven outcome can be reconciled. No free-text evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconciliationEvidence {
    DiscordConfirmedEffect,
    DiscordConfirmedNoEffect,
    ProvenNotSent,
}

impl ReconciliationEvidence {
    fn as_str(self) -> &'static str {
        match self {
            Self::DiscordConfirmedEffect => "discord_confirmed_effect",
            Self::DiscordConfirmedNoEffect => "discord_confirmed_no_effect",
            Self::ProvenNotSent => "proven_not_sent",
        }
    }

    fn supports(self, response: &TerminalResponse) -> bool {
        matches!(
            (self, response),
            (
                Self::DiscordConfirmedEffect,
                TerminalResponse::Success { .. }
            ) | (
                Self::DiscordConfirmedNoEffect | Self::ProvenNotSent,
                TerminalResponse::Failure(_)
            )
        )
    }
}

/// Reuses the runtime pool. Methods never migrate or contact Discord.
#[derive(Clone)]
pub struct InternalActionStore {
    pool: PgPool,
}

impl InternalActionStore {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Signature/freshness first, then durable burn, BEFORE body parsing/buckets.
    /// Scope is global, matching NonceCache. Only expiry allows replacement.
    pub async fn burn_nonce(&self, nonce: &str) -> Result<bool, InternalStoreError> {
        if !valid_nonce_format(nonce) {
            return Err(InternalStoreError::InvalidInput);
        }
        let ttl = i64::try_from(NONCE_TTL_SECONDS.max(2 * SKEW_SECONDS + 1))
            .map_err(|_| InternalStoreError::InvalidInput)?;
        let mut tx = self.pool.begin().await?;
        let nonce_hash = body_hash(nonce.as_bytes());
        let burned = sqlx::query(
            "WITH instant AS MATERIALIZED (SELECT clock_timestamp() AS now) \
             INSERT INTO internal_nonces (nonce_hash, burned_at, expires_at) \
             SELECT $1, now, now + $2::bigint * INTERVAL '1 second' FROM instant \
             ON CONFLICT (nonce_hash) DO UPDATE \
             SET (burned_at, expires_at) = (\
                 WITH fresh AS MATERIALIZED (SELECT clock_timestamp() AS now) \
                 SELECT now, now + $2::bigint * INTERVAL '1 second' FROM fresh\
             ) WHERE internal_nonces.expires_at <= clock_timestamp() \
             RETURNING nonce_hash",
        )
        .bind(&nonce_hash)
        .bind(ttl)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if burned {
            // A speculative INSERT may wait on another transaction which then
            // rolls back. Its VALUES clock was sampled BEFORE that wait. Refresh
            // only the winning, still-locked row so retention starts after it.
            sqlx::query(
                "WITH fresh AS MATERIALIZED (SELECT clock_timestamp() AS now) \
                 UPDATE internal_nonces SET burned_at = now, \
                 expires_at = now + $2::bigint * INTERVAL '1 second' \
                 FROM fresh WHERE nonce_hash = $1",
            )
            .bind(&nonce_hash)
            .bind(ttl)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(burned)
    }

    /// Insert and audit share a transaction. Only committed new claims may run.
    pub async fn claim(
        &self,
        identity: &RequestIdentity,
        subject: &AuditSubject,
    ) -> Result<InternalClaim, InternalStoreError> {
        let mut tx = self.pool.begin().await?;
        let inserted: Option<i64> = sqlx::query_scalar(
            "INSERT INTO internal_idempotency \
             (caller_hash, key_hash, action, payload_hash, state, guild_id, actor_id, target_id) \
             VALUES ($1, $2, $3, $4, 'in_flight', $5, $6, $7) \
             ON CONFLICT (caller_hash, key_hash) DO NOTHING RETURNING intent_id",
        )
        .bind(&identity.caller_hash)
        .bind(&identity.key_hash)
        .bind(&identity.action)
        .bind(&identity.payload_hash)
        .bind(subject.guild_id.as_ref().map(DiscordId::as_str))
        .bind(subject.actor_id.as_ref().map(DiscordId::as_str))
        .bind(subject.target_id.as_ref().map(DiscordId::as_str))
        .fetch_optional(&mut *tx)
        .await?;
        let result = if let Some(intent_id) = inserted {
            // As with nonce insertion, start diagnostics after any speculative
            // uniqueness wait, not before a competing transaction rolled back.
            sqlx::query(
                "WITH fresh AS MATERIALIZED (SELECT clock_timestamp() AS now) \
                 UPDATE internal_idempotency SET created_at = now, updated_at = now \
                 FROM fresh WHERE intent_id = $1",
            )
            .bind(intent_id)
            .execute(&mut *tx)
            .await?;
            Self::audit(&mut tx, intent_id, "intent", None).await?;
            InternalClaim::Claimed(ExecutionClaim {
                intent_id,
                identity: identity.clone(),
            })
        } else {
            let row = Self::lock_identity(&mut tx, identity).await?;
            if !Self::matches(&row, identity)? {
                InternalClaim::Mismatch
            } else {
                match row.try_get::<&str, _>("state")? {
                    "completed" => InternalClaim::Replay(TerminalResponse::from_row(&row)?),
                    "unknown" => InternalClaim::NeedsReconciliation,
                    "in_flight" if row.try_get::<bool, _>("stale")? => {
                        InternalClaim::NeedsReconciliation
                    }
                    "in_flight" => InternalClaim::InFlight,
                    _ => return Err(InternalStoreError::Unavailable),
                }
            }
        };
        tx.commit().await?;
        Ok(result)
    }

    /// Record a definitive outcome. Late completion cannot overwrite reconciliation.
    pub async fn finish(
        &self,
        claim: &ExecutionClaim,
        response: &TerminalResponse,
    ) -> Result<(), InternalStoreError> {
        self.terminalize(&claim.identity, Some(claim.intent_id), response, None)
            .await
    }

    /// A timeout/crash path retains the intent; it never releases execution rights.
    pub async fn mark_unknown(&self, claim: &ExecutionClaim) -> Result<(), InternalStoreError> {
        let mut tx = self.pool.begin().await?;
        let row = Self::lock_identity(&mut tx, &claim.identity).await?;
        if !Self::matches(&row, &claim.identity)?
            || row.try_get::<i64, _>("intent_id")? != claim.intent_id
            || row.try_get::<&str, _>("state")? == "completed"
        {
            return Err(InternalStoreError::TransitionRefused);
        }
        sqlx::query(
            "UPDATE internal_idempotency SET state = 'unknown', updated_at = clock_timestamp() \
             WHERE intent_id = $1",
        )
        .bind(claim.intent_id)
        .execute(&mut *tx)
        .await?;
        Self::audit(&mut tx, claim.intent_id, "unknown", None).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Caller owns proving the evidence. This method only records a terminal result;
    /// it cannot reset an intent or return an ExecutionClaim. Fresh intents refuse.
    pub async fn reconcile(
        &self,
        identity: &RequestIdentity,
        response: &TerminalResponse,
        evidence: ReconciliationEvidence,
    ) -> Result<(), InternalStoreError> {
        if !evidence.supports(response) {
            return Err(InternalStoreError::InvalidInput);
        }
        self.terminalize(identity, None, response, Some(evidence))
            .await
    }

    async fn terminalize(
        &self,
        identity: &RequestIdentity,
        owner_id: Option<i64>,
        response: &TerminalResponse,
        evidence: Option<ReconciliationEvidence>,
    ) -> Result<(), InternalStoreError> {
        let mut tx = self.pool.begin().await?;
        let row = Self::lock_identity(&mut tx, identity).await?;
        let intent_id: i64 = row.try_get("intent_id")?;
        let state: &str = row.try_get("state")?;
        if !Self::matches(&row, identity)?
            || state == "completed"
            || owner_id.is_some_and(|id| id != intent_id)
            || (owner_id.is_none() && state != "unknown" && !row.try_get::<bool, _>("stale")?)
        {
            return Err(InternalStoreError::TransitionRefused);
        }
        sqlx::query(
            "UPDATE internal_idempotency SET state = 'completed', response_code = $2, \
             http_status = $3, resource_id = $4, affected = $5, updated_at = clock_timestamp() \
             WHERE intent_id = $1",
        )
        .bind(intent_id)
        .bind(response.code())
        .bind(i32::from(response.status()))
        .bind(response.resource_id())
        .bind(response.affected())
        .execute(&mut *tx)
        .await?;
        Self::audit(
            &mut tx,
            intent_id,
            "terminal",
            Some(evidence.map_or("executor", ReconciliationEvidence::as_str)),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn lock_identity(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        identity: &RequestIdentity,
    ) -> Result<sqlx::postgres::PgRow, InternalStoreError> {
        Ok(sqlx::query(
            "SELECT *, clock_timestamp() >= created_at + $3::bigint * INTERVAL '1 second' AS stale \
             FROM internal_idempotency WHERE caller_hash = $1 AND key_hash = $2 FOR UPDATE",
        )
        .bind(&identity.caller_hash)
        .bind(&identity.key_hash)
        .bind(i64::try_from(CLAIM_STALE_SECONDS).map_err(|_| InternalStoreError::InvalidInput)?)
        .fetch_one(&mut **tx)
        .await?)
    }

    fn matches(
        row: &sqlx::postgres::PgRow,
        identity: &RequestIdentity,
    ) -> Result<bool, InternalStoreError> {
        Ok(row.try_get::<&str, _>("action")? == identity.action
            && row.try_get::<&str, _>("payload_hash")? == identity.payload_hash)
    }

    async fn audit(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        intent_id: i64,
        phase: &'static str,
        evidence: Option<&'static str>,
    ) -> Result<(), InternalStoreError> {
        sqlx::query(
            "INSERT INTO internal_action_log \
             (intent_id, phase, caller_hash, action, guild_id, actor_id, target_id, \
              response_code, http_status, evidence_code) \
             SELECT intent_id, $2, caller_hash, action, guild_id, actor_id, target_id, \
                    response_code, http_status, $3 \
             FROM internal_idempotency WHERE intent_id = $1 \
             ON CONFLICT (intent_id, phase) DO NOTHING",
        )
        .bind(intent_id)
        .bind(phase)
        .bind(evidence)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Global first-wins burn. Retained indefinitely; duplicate deliveries may not run.
    /// Use a stable namespaced event identity, NOT a random ID generated per delivery.
    pub async fn claim_discord_event(&self, event_id: &str) -> Result<bool, InternalStoreError> {
        if event_id.is_empty()
            || event_id.len() > 200
            || !event_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
        {
            return Err(InternalStoreError::InvalidInput);
        }
        Ok(sqlx::query(
            "INSERT INTO internal_discord_events (event_hash) VALUES ($1) \
             ON CONFLICT (event_hash) DO NOTHING RETURNING event_hash",
        )
        .bind(body_hash(event_id.as_bytes()))
        .fetch_optional(&self.pool)
        .await?
        .is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundary_and_debug_are_secret_safe() {
        let payload = br#"{"access_token":"member-oauth-secret","reason":"sensitive reason"}"#;
        let identity = RequestIdentity::new(
            "website-key-id",
            "private-key:123",
            "guild.add_member",
            payload,
        )
        .unwrap();
        let debug = format!("{identity:?}");
        for raw in [
            "website-key-id",
            "private-key:123",
            "member-oauth-secret",
            "sensitive reason",
        ] {
            assert!(!debug.contains(raw));
        }
        assert!(RequestIdentity::new("caller", "short", "guild.add_member", payload).is_err());
        assert!(RequestIdentity::new("caller", "valid-key", "unknown-action", payload).is_err());
        assert!(DiscordId::new("member-oauth-secret").is_err());
        assert!(DiscordId::new("123456789012345678").is_ok());
        let source = sqlx::Error::Protocol("member-oauth-secret HMAC-header DB-credential".into());
        let error = InternalStoreError::from(source);
        assert_eq!(error.to_string(), "internal-action storage unavailable");
        assert_eq!(format!("{error:?}"), "Unavailable");
        assert!(std::error::Error::source(&error).is_none());
    }

    #[test]
    fn reconciliation_evidence_cannot_authorize_execution() {
        let success = TerminalResponse::Success {
            resource_id: None,
            affected: 1,
        };
        let failure = TerminalResponse::Failure(TerminalFailure::NoEffect);
        assert!(ReconciliationEvidence::DiscordConfirmedEffect.supports(&success));
        assert!(!ReconciliationEvidence::ProvenNotSent.supports(&success));
        assert!(ReconciliationEvidence::ProvenNotSent.supports(&failure));
        assert!(!ReconciliationEvidence::DiscordConfirmedEffect.supports(&failure));
        assert_eq!(success.status(), 200);
        assert_eq!(failure.status(), 502);
    }
}
