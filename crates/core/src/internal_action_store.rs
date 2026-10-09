//! Durable guards for internal actions; only a new committed claim permits execution.
//!
//! Stale/unknown intents are never reclaimed. Reconciliation records a proven
//! terminal outcome, not a new execution lease. See `docs/internal-action-store.md`.

use sqlx::{PgPool, Row};
use std::str::FromStr as _;

use crate::clock_guard::CLOCK_SKEW_TOLERANCE_MS;
use crate::internal_actions::{
    body_hash, is_implemented, is_snowflake, valid_event_key, valid_idempotency_key,
    valid_nonce_format, within_skew, CLAIM_STALE_SECONDS, MAX_BODY_BYTES, NONCE_TTL_SECONDS,
    SKEW_SECONDS,
};

/// Clock domain for the durable nonce high-water mark. One guard per clock
/// domain: DB `clock_timestamp()` readings are not comparable with any
/// process-local wall clock, so the durable mark lives in its own domain row.
pub const NONCE_DB_CLOCK_DOMAIN: &str = "internal_nonce_db";

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
    /// Pin the allowlist-resolved role before REST; configuration may later change.
    pub resolved_role_id: Option<DiscordId>,
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

/// Durable website-event outcome: the closed legacy result word for one intent.
/// `None` on success is the announcement shape (its envelope carries
/// `message_id`, never an outcome). Event intents always record one: replay
/// must return the first result byte-identically, and a create replayed after
/// its key was registered must still read `created`, never a re-derived
/// `updated`. Never free text: this column feeds the wire envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventOutcome {
    Created,
    Updated,
    Cancelled,
}

impl EventOutcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Cancelled => "cancelled",
        }
    }
}

impl std::str::FromStr for EventOutcome {
    type Err = InternalStoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "created" => Ok(Self::Created),
            "updated" => Ok(Self::Updated),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(InternalStoreError::Unavailable),
        }
    }
}

/// Minimal replayable response contract. Extend with typed scalars, never raw JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalResponse {
    Success {
        resource_id: Option<DiscordId>,
        affected: u32,
        outcome: Option<EventOutcome>,
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

    fn outcome(&self) -> Option<&'static str> {
        match self {
            Self::Success { outcome, .. } => outcome.map(EventOutcome::as_str),
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
                outcome: row
                    .try_get::<Option<&str>, _>("outcome")?
                    .map(EventOutcome::from_str)
                    .transpose()?,
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
    ///
    /// `timestamp` is the exact authenticated timestamp header (unix seconds).
    /// A pool, row-lock or uniqueness wait can outlast the skew window, so the
    /// expiry predicate alone cannot decide replays: after the burn statement
    /// the store re-checks `within_skew` against database time in the SAME
    /// transaction and rolls back with `InvalidInput` when the wait crossed
    /// out of the window. A delayed replay can therefore never win a second
    /// burn, even if the row it waited on expired mid-wait. Callers must treat
    /// every error (including this one) as refusal: only `Ok(true)` with a
    /// fresh timestamp is a successful burn.
    ///
    /// F8 fail-closed clock policy: the commit instant also advances the
    /// persisted [`NONCE_DB_CLOCK_DOMAIN`] high-water mark, and a DB-time
    /// regression past [`CLOCK_SKEW_TOLERANCE_MS`] rolls the whole burn back
    /// with `InvalidInput` — even for a fresh nonce. Within tolerance the mark
    /// (not the regressed reading) decides freshness, so a sweep can never
    /// reopen a signed window the mark has already passed. Restart/failover
    /// re-derives the mark from the table (see [`Self::nonce_high_water_ms`]),
    /// so persisted time cannot move backwards past burned nonces.
    pub async fn burn_nonce(
        &self,
        nonce: &str,
        timestamp: &str,
    ) -> Result<bool, InternalStoreError> {
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
        // Commit-time freshness AND clock policy: the wait above may have
        // crossed the skew window (or expiry) after the receiver's pre-burn
        // check passed, and the DB clock itself may have regressed. Decide
        // against database time — never the caller's clock — with the same
        // whole-second `within_skew` semantics as the domain check, and roll
        // the whole burn back when the attempt is no longer fresh. An invalid
        // (non-numeric) timestamp fails `within_skew` and is refused the same
        // way; format-validated nonces are unaffected.
        //
        // The guard compares the commit instant against the persisted
        // high-water mark in the SAME transaction (locked via FOR UPDATE), so
        // a regression check cannot race a concurrent advancing burn. The
        // tolerance covers a commit that sampled its instant before an
        // earlier-committed burn advanced the mark: within tolerance the mark
        // decides freshness. Past tolerance the burn refuses fail-closed.
        let commit_ms: i64 = sqlx::query_scalar(
            "SELECT floor(extract(epoch FROM clock_timestamp()) * 1000)::bigint",
        )
        .fetch_one(&mut *tx)
        .await?;
        let commit_ms = u64::try_from(commit_ms).map_err(|_| InternalStoreError::InvalidInput)?;
        let mark_ms: Option<i64> = sqlx::query_scalar(
            "SELECT high_water_ms FROM internal_clock_high_water WHERE domain = $1 FOR UPDATE",
        )
        .bind(NONCE_DB_CLOCK_DOMAIN)
        .fetch_optional(&mut *tx)
        .await?;
        let guarded_ms = match mark_ms {
            Some(mark) => {
                let mark = u64::try_from(mark).map_err(|_| InternalStoreError::InvalidInput)?;
                if commit_ms >= mark {
                    commit_ms
                } else if mark - commit_ms <= CLOCK_SKEW_TOLERANCE_MS {
                    mark
                } else {
                    tx.rollback().await?;
                    return Err(InternalStoreError::InvalidInput);
                }
            }
            None => commit_ms,
        };
        let guarded_secs = guarded_ms / 1000;
        if !within_skew(timestamp, SKEW_SECONDS, guarded_secs) {
            tx.rollback().await?;
            return Err(InternalStoreError::InvalidInput);
        }
        let guarded_ms = i64::try_from(guarded_ms).map_err(|_| InternalStoreError::InvalidInput)?;
        sqlx::query(
            "INSERT INTO internal_clock_high_water (domain, high_water_ms, observed_at) \
             VALUES ($1, $2, clock_timestamp()) \
             ON CONFLICT (domain) DO UPDATE \
             SET high_water_ms = EXCLUDED.high_water_ms, observed_at = clock_timestamp() \
             WHERE internal_clock_high_water.high_water_ms < EXCLUDED.high_water_ms",
        )
        .bind(NONCE_DB_CLOCK_DOMAIN)
        .bind(guarded_ms)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(burned)
    }

    /// Highest DB freshness instant observed by [`Self::burn_nonce`],
    /// milliseconds since epoch. A new process restores its guard from this
    /// (or equivalently from the newest `burned_at`) before evaluating
    /// freshness, so an earlier clock refuses old captures instead of
    /// treating its first read as new.
    pub async fn nonce_high_water_ms(&self) -> Result<Option<u64>, InternalStoreError> {
        let mark: Option<i64> = sqlx::query_scalar(
            "SELECT high_water_ms FROM internal_clock_high_water WHERE domain = $1",
        )
        .bind(NONCE_DB_CLOCK_DOMAIN)
        .fetch_optional(&self.pool)
        .await?
        .flatten();
        mark.map(|ms| u64::try_from(ms).map_err(|_| InternalStoreError::InvalidInput))
            .transpose()
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
             (caller_hash, key_hash, action, payload_hash, state, guild_id, actor_id, target_id, resolved_role_id) \
             VALUES ($1, $2, $3, $4, 'in_flight', $5, $6, $7, $8) \
             ON CONFLICT (caller_hash, key_hash) DO NOTHING RETURNING intent_id",
        )
        .bind(&identity.caller_hash)
        .bind(&identity.key_hash)
        .bind(&identity.action)
        .bind(&identity.payload_hash)
        .bind(subject.guild_id.as_ref().map(DiscordId::as_str))
        .bind(subject.actor_id.as_ref().map(DiscordId::as_str))
        .bind(subject.target_id.as_ref().map(DiscordId::as_str))
        .bind(subject.resolved_role_id.as_ref().map(DiscordId::as_str))
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
                    "not_sent" if !Self::matches_subject(&row, subject)? => InternalClaim::Mismatch,
                    "not_sent" => {
                        sqlx::query(
                            "WITH fresh AS MATERIALIZED (SELECT clock_timestamp() AS now) \
                             UPDATE internal_idempotency SET state = 'in_flight', \
                             created_at = now, updated_at = now FROM fresh WHERE intent_id = $1",
                        )
                        .bind(row.try_get::<i64, _>("intent_id")?)
                        .execute(&mut *tx)
                        .await?;
                        InternalClaim::Claimed(ExecutionClaim {
                            intent_id: row.try_get("intent_id")?,
                            identity: identity.clone(),
                        })
                    }
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

    /// Release only when the caller proves the mutation was never dispatched.
    /// Consumes the opaque, non-Clone claim even on failure: no old owner can
    /// finish or mark unknown after a new claimant acquires this same intent.
    /// Nonces, payload/subject binding and per-intent audit evidence are retained.
    ///
    /// ```compile_fail
    /// use two_bot_core::internal_action_store::{ExecutionClaim, InternalActionStore};
    /// async fn old_owner(store: &InternalActionStore, claim: ExecutionClaim) {
    ///     store.release_proven_not_sent(claim).await.unwrap();
    ///     store.mark_unknown(&claim).await.unwrap(); // claim was consumed
    /// }
    /// ```
    pub async fn release_proven_not_sent(
        &self,
        claim: ExecutionClaim,
    ) -> Result<(), InternalStoreError> {
        let mut tx = self.pool.begin().await?;
        let row = Self::lock_identity(&mut tx, &claim.identity).await?;
        if !Self::matches(&row, &claim.identity)?
            || row.try_get::<i64, _>("intent_id")? != claim.intent_id
            || row.try_get::<&str, _>("state")? != "in_flight"
            || row.try_get::<bool, _>("stale")?
        {
            return Err(InternalStoreError::TransitionRefused);
        }
        sqlx::query(
            "UPDATE internal_idempotency SET state = 'not_sent', updated_at = clock_timestamp() \
             WHERE intent_id = $1",
        )
        .bind(claim.intent_id)
        .execute(&mut *tx)
        .await?;
        Self::audit(
            &mut tx,
            claim.intent_id,
            "released",
            Some("proven_not_sent"),
        )
        .await?;
        tx.commit().await?;
        Ok(())
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
            || !matches!(row.try_get::<&str, _>("state")?, "in_flight" | "unknown")
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
            || !matches!(state, "in_flight" | "unknown")
            || owner_id.is_some_and(|id| id != intent_id)
            || (owner_id.is_none() && state != "unknown" && !row.try_get::<bool, _>("stale")?)
        {
            return Err(InternalStoreError::TransitionRefused);
        }
        sqlx::query(
            "UPDATE internal_idempotency SET state = 'completed', response_code = $2, \
             http_status = $3, resource_id = $4, affected = $5, outcome = $6, \
             updated_at = clock_timestamp() WHERE intent_id = $1",
        )
        .bind(intent_id)
        .bind(response.code())
        .bind(i32::from(response.status()))
        .bind(response.resource_id())
        .bind(response.affected())
        .bind(response.outcome())
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

    fn matches_subject(
        row: &sqlx::postgres::PgRow,
        subject: &AuditSubject,
    ) -> Result<bool, InternalStoreError> {
        Ok(row.try_get::<Option<&str>, _>("guild_id")?
            == subject.guild_id.as_ref().map(DiscordId::as_str)
            && row.try_get::<Option<&str>, _>("actor_id")?
                == subject.actor_id.as_ref().map(DiscordId::as_str)
            && row.try_get::<Option<&str>, _>("target_id")?
                == subject.target_id.as_ref().map(DiscordId::as_str)
            && row.try_get::<Option<&str>, _>("resolved_role_id")?
                == subject.resolved_role_id.as_ref().map(DiscordId::as_str))
    }

    async fn audit(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        intent_id: i64,
        phase: &'static str,
        evidence: Option<&'static str>,
    ) -> Result<(), InternalStoreError> {
        sqlx::query(
            "INSERT INTO internal_action_log \
             (intent_id, phase, caller_hash, action, guild_id, actor_id, target_id, resolved_role_id, \
              response_code, http_status, evidence_code) \
             SELECT intent_id, $2, caller_hash, action, guild_id, actor_id, target_id, resolved_role_id, \
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

    /// Guild-fenced website event-key to Discord scheduled-event ID map
    /// (legacy `discordEventId`). The website never names a snowflake: the
    /// signed `event.read` verifier resolves its `event_key` in the request
    /// guild through this table, and `event.upsert` registers the key after
    /// Discord confirms a create. A key is an address only inside its guild.
    fn check_event_key_shape(guild_id: &str, event_key: &str) -> Result<(), InternalStoreError> {
        if !is_snowflake(guild_id) || !valid_event_key(event_key) {
            return Err(InternalStoreError::InvalidInput);
        }
        Ok(())
    }

    /// Register (or re-point) one key in one guild. `event.upsert` calls this
    /// only after Discord confirms the create/update the mapping names.
    pub async fn put_event_key(
        &self,
        guild_id: &str,
        event_key: &str,
        event_id: &str,
    ) -> Result<(), InternalStoreError> {
        Self::check_event_key_shape(guild_id, event_key)?;
        if !is_snowflake(event_id) {
            return Err(InternalStoreError::InvalidInput);
        }
        sqlx::query(
            "INSERT INTO internal_event_keys (guild_id, event_key, event_id) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (guild_id, event_key) DO UPDATE \
             SET event_id = EXCLUDED.event_id, updated_at = clock_timestamp()",
        )
        .bind(guild_id)
        .bind(event_key)
        .bind(event_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Resolve one key inside one guild. `Ok(None)` is an unmapped key: the
    /// caller refuses `action_not_allowed` before any Discord call, exactly
    /// like legacy's mapped-event verifier.
    pub async fn event_id_for_key(
        &self,
        guild_id: &str,
        event_key: &str,
    ) -> Result<Option<String>, InternalStoreError> {
        Self::check_event_key_shape(guild_id, event_key)?;
        Ok(sqlx::query_scalar(
            "SELECT event_id FROM internal_event_keys WHERE guild_id = $1 AND event_key = $2",
        )
        .bind(guild_id)
        .bind(event_key)
        .fetch_optional(&self.pool)
        .await?)
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
    fn event_outcome_round_trips_the_closed_legacy_words() {
        for (word, outcome) in [
            ("created", EventOutcome::Created),
            ("updated", EventOutcome::Updated),
            ("cancelled", EventOutcome::Cancelled),
        ] {
            assert_eq!(outcome.as_str(), word);
            assert_eq!(EventOutcome::from_str(word).unwrap(), outcome);
        }
        assert!(EventOutcome::from_str("posted").is_err());
        assert!(EventOutcome::from_str("").is_err());
    }

    #[test]
    fn reconciliation_evidence_cannot_authorize_execution() {
        let success = TerminalResponse::Success {
            resource_id: None,
            affected: 1,
            outcome: None,
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
