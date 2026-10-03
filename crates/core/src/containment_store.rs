//! Durable containment claims (docs/containment.md "Required runtime fences" #4).
//!
//! Ports legacy `two-bot` at `d5d1179348feb9157bcac8c875de9399d4f5c76a`:
//! `src/moderation/containmentStore.ts` (`claimEvent`, `beginIncident`,
//! `completeIncident`) over the legacy tables recreated by migration `0370`.
//! Decisions stay in [`ContainmentPolicy`]; this store makes them atomic and
//! restart-safe. It registers no listener, executes nothing and does not arm
//! containment.
//!
//! Advisory-lock keys use the legacy derivation (first two big-endian `int4`s of
//! SHA-256 over `guild:executor` or `incident:guild:executor`), so a legacy
//! process on the same database serializes against this store. Timestamps are
//! canonical `YYYY-MM-DDTHH:MM:SS.sssZ` TEXT, compared lexicographically like
//! legacy; any other recorded form is refused as malformed evidence.

use sha2::{Digest, Sha256};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, PgPool, Row};

use crate::containment::{
    ClaimedContainmentEvent, ContainmentDisposition, ContainmentEventState, ContainmentIncident,
    ContainmentIncidentState, ContainmentPolicy, DestructiveAction, DestructiveAuditEvent,
    CONTAINMENT_FUTURE_SKEW_MS,
};
use crate::funnel::{format_iso_millis, parse_iso_millis};

/// `containment_events` columns read by [`recorded_event`]; a macro so every
/// query stays a `&'static str` literal (sqlx `SqlSafeStr`).
macro_rules! event_columns {
    () => {
        "audit_entry_id, guild_id, executor_id, action, target_id, occurred_at, state, reason, \
         created_at"
    };
}

/// `0000-01-01T00:00:00.000Z`: the earliest four-digit-year ISO instant.
const MIN_ISO_MS: i64 = -62_167_219_200_000;
/// `9999-12-31T23:59:59.999Z`: later instants lose lexicographic ordering.
const MAX_ISO_MS: i64 = 253_402_300_799_999;

#[derive(Debug, thiserror::Error)]
pub enum ContainmentStoreError {
    #[error("containment store database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("invalid containment record: {0}")]
    Invalid(&'static str),
}

/// Result of [`ContainmentStore::claim_event`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventClaim {
    /// This call claimed the audit ID and persisted `disposition` exactly.
    Claimed {
        event: ClaimedContainmentEvent,
        disposition: ContainmentDisposition,
        /// Occurrence heat at claim time; zero unless the event counts.
        heat: u64,
    },
    /// The audit ID was already claimed. Its recorded disposition stands and
    /// nothing is recounted (legacy `{ claimed: false, heat: 0 }`).
    Duplicate(RecordedContainmentEvent),
}

/// A `containment_events` row as persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedContainmentEvent {
    pub claimed: ClaimedContainmentEvent,
    /// Reason text exactly as persisted; legacy rows keep their wording.
    pub reason: String,
    pub created_at_ms: i64,
}

/// Result of [`ContainmentStore::begin_incident`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncidentClaim {
    /// Incident inserted as `containing`; the trigger is now recorded `contain`.
    Started(ContainmentIncident),
    /// A same-guild/executor incident still blocks (uncertain, or cooling down
    /// at processing time). Log the suppression; do not alert or execute.
    Blocked {
        incident_id: String,
        state: ContainmentIncidentState,
    },
    /// Below threshold, unattributable, or the trigger is not a recorded
    /// `observe` row for this guild/executor (including one already contained).
    NotEligible,
}

/// Durable claims over the legacy containment tables.
#[derive(Debug, Clone)]
pub struct ContainmentStore {
    pool: PgPool,
}

impl ContainmentStore {
    /// Reuse the runtime's pool instead of opening a separate pool per handler.
    #[must_use]
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Atomically claim `event.audit_entry_id` and persist the policy's
    /// disposition at `now_ms` (legacy `claimEvent`).
    ///
    /// Claims for one guild/executor are serialized; the disposition and heat
    /// are computed inside that lock, so concurrent deliveries count each audit
    /// ID once. An event without a valid occurrence time is refused and nothing
    /// is persisted (legacy threw before reaching its store).
    pub async fn claim_event(
        &self,
        policy: &ContainmentPolicy,
        event: &DestructiveAuditEvent,
        now_ms: i64,
    ) -> Result<EventClaim, ContainmentStoreError> {
        let occurred_ms = event
            .occurred_at_ms
            .filter(|ms| (MIN_ISO_MS..=MAX_ISO_MS).contains(ms))
            .ok_or(ContainmentStoreError::Invalid("occurrence time"))?;
        let now = iso_in_range(now_ms).ok_or(ContainmentStoreError::Invalid("processing time"))?;
        let mut tx = self.pool.begin().await?;
        if let Some(executor) = event.executor_id.as_deref() {
            advisory_lock(
                &mut tx,
                &format!("{guild}:{executor}", guild = event.guild_id),
            )
            .await?;
        }
        let disposition = policy.disposition(event, now_ms);
        let weight = i32::try_from(event.action.weight())
            .map_err(|_| ContainmentStoreError::Invalid("action weight"))?;
        let inserted = sqlx::query(
            "INSERT INTO containment_events \
               (audit_entry_id, guild_id, executor_id, action, target_id, weight, \
                occurred_at, state, reason, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (audit_entry_id) DO NOTHING",
        )
        .bind(&event.audit_entry_id)
        .bind(&event.guild_id)
        .bind(event.executor_id.as_deref())
        .bind(event.action.as_str())
        .bind(event.target_id.as_deref())
        .bind(weight)
        .bind(format_iso_millis(occurred_ms))
        .bind(event_state_name(disposition.state))
        .bind(disposition.reason.description())
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if inserted != 1 {
            let row = sqlx::query(concat!(
                "SELECT ",
                event_columns!(),
                " FROM containment_events WHERE audit_entry_id = $1"
            ))
            .bind(&event.audit_entry_id)
            .fetch_one(&mut *tx)
            .await?;
            let recorded = recorded_event(&row)?;
            tx.commit().await?;
            return Ok(EventClaim::Duplicate(recorded));
        }
        let claimed = ClaimedContainmentEvent {
            event: event.clone(),
            state: disposition.state,
        };
        let heat = match event.executor_id.as_deref() {
            Some(executor) if disposition.state == ContainmentEventState::Observe => {
                let evidence =
                    heat_evidence(&mut tx, policy, event, executor, occurred_ms, now_ms).await?;
                policy.occurrence_heat(&claimed, &evidence, now_ms)
            }
            _ => 0,
        };
        tx.commit().await?;
        Ok(EventClaim::Claimed {
            event: claimed,
            disposition,
            heat,
        })
    }

    /// Start an incident for `trigger` (legacy `beginIncident`).
    ///
    /// Incident claims for one guild/executor are serialized separately from
    /// event claims. Blockers are rechecked inside this transaction at `now_ms`;
    /// the cooldown is measured from processing time. On success the trigger is
    /// persisted as `contain` in the same transaction.
    pub async fn begin_incident(
        &self,
        policy: &ContainmentPolicy,
        trigger: &ClaimedContainmentEvent,
        heat: u64,
        now_ms: i64,
    ) -> Result<IncidentClaim, ContainmentStoreError> {
        let Some(executor) = trigger.event.executor_id.as_deref() else {
            return Ok(IncidentClaim::NotEligible);
        };
        if trigger.state != ContainmentEventState::Observe || heat < policy.heat_threshold() {
            return Ok(IncidentClaim::NotEligible);
        }
        let heat_column =
            i32::try_from(heat).map_err(|_| ContainmentStoreError::Invalid("incident heat"))?;
        let now = iso_in_range(now_ms).ok_or(ContainmentStoreError::Invalid("processing time"))?;
        let cooldown_until_ms = now_ms.saturating_add(policy.window_ms());
        let cooldown = iso_in_range(cooldown_until_ms)
            .ok_or(ContainmentStoreError::Invalid("cooldown time"))?;
        let guild = trigger.event.guild_id.as_str();
        let id = trigger.event.audit_entry_id.as_str();

        let mut tx = self.pool.begin().await?;
        advisory_lock(&mut tx, &format!("incident:{guild}:{executor}")).await?;
        let recorded: Option<(String, Option<String>, String)> = sqlx::query_as(
            "SELECT guild_id, executor_id, state FROM containment_events \
             WHERE audit_entry_id = $1 FOR UPDATE",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let eligible = recorded.is_some_and(|(row_guild, row_executor, state)| {
            row_guild == guild && row_executor.as_deref() == Some(executor) && state == "observe"
        });
        if !eligible {
            tx.commit().await?;
            return Ok(IncidentClaim::NotEligible);
        }
        let blocker: Option<(String, String)> = sqlx::query_as(
            "SELECT id, state FROM containment_incidents \
             WHERE guild_id = $1 AND executor_id = $2 \
               AND (state = 'uncertain' OR cooldown_until > $3) \
             ORDER BY started_at DESC, id LIMIT 1",
        )
        .bind(guild)
        .bind(executor)
        .bind(&now)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((incident_id, state)) = blocker {
            let state = incident_state(&state)?;
            tx.commit().await?;
            return Ok(IncidentClaim::Blocked { incident_id, state });
        }
        let inserted = sqlx::query(
            "INSERT INTO containment_incidents \
               (id, guild_id, executor_id, trigger_audit_entry_id, heat, state, \
                started_at, cooldown_until) \
             VALUES ($1, $2, $3, $1, $4, 'containing', $5, $6) \
             ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(guild)
        .bind(executor)
        .bind(heat_column)
        .bind(now)
        .bind(cooldown)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if inserted != 1 {
            tx.commit().await?;
            return Ok(IncidentClaim::NotEligible);
        }
        sqlx::query("UPDATE containment_events SET state = 'contain' WHERE audit_entry_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(IncidentClaim::Started(ContainmentIncident {
            id: id.to_owned(),
            guild_id: guild.to_owned(),
            executor_id: executor.to_owned(),
            heat,
            state: ContainmentIncidentState::Containing,
            cooldown_until_ms,
        }))
    }

    /// Record an incident outcome once (legacy `completeIncident`). Returns
    /// `false` when the incident is unknown or already completed, so a stray
    /// retry can never overwrite (and thereby release) an `uncertain` outcome.
    pub async fn complete_incident(
        &self,
        incident_id: &str,
        state: ContainmentIncidentState,
        result: &serde_json::Value,
        now_ms: i64,
    ) -> Result<bool, ContainmentStoreError> {
        if state == ContainmentIncidentState::Containing {
            return Err(ContainmentStoreError::Invalid("completion state"));
        }
        let now = iso_in_range(now_ms).ok_or(ContainmentStoreError::Invalid("processing time"))?;
        let updated = sqlx::query(
            "UPDATE containment_incidents \
                SET state = $2, result_json = $3, completed_at = $4 \
              WHERE id = $1 AND state = 'containing'",
        )
        .bind(incident_id)
        .bind(state.outcome_label())
        .bind(result.to_string())
        .bind(now)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    /// Read one recorded event, e.g. after a restart.
    pub async fn recorded_event(
        &self,
        audit_entry_id: &str,
    ) -> Result<Option<RecordedContainmentEvent>, ContainmentStoreError> {
        let row = sqlx::query(concat!(
            "SELECT ",
            event_columns!(),
            " FROM containment_events WHERE audit_entry_id = $1"
        ))
        .bind(audit_entry_id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(recorded_event).transpose()
    }
}

/// Legacy interval `(occurred − window, min(occurred + window, now + 5s)]`
/// over counted rows; the policy recomputes the sliding maximum from these.
async fn heat_evidence(
    tx: &mut PgConnection,
    policy: &ContainmentPolicy,
    event: &DestructiveAuditEvent,
    executor: &str,
    occurred_ms: i64,
    now_ms: i64,
) -> Result<Vec<ClaimedContainmentEvent>, ContainmentStoreError> {
    let window = policy.window_ms();
    let lower = occurred_ms.saturating_sub(window).max(MIN_ISO_MS);
    let upper = occurred_ms
        .saturating_add(window)
        .min(now_ms.saturating_add(CONTAINMENT_FUTURE_SKEW_MS))
        .min(MAX_ISO_MS);
    let rows = sqlx::query(concat!(
        "SELECT ",
        event_columns!(),
        " FROM containment_events \
         WHERE guild_id = $1 AND executor_id = $2 AND state IN ('observe', 'contain') \
           AND occurred_at > $3 AND occurred_at <= $4 \
         ORDER BY occurred_at, audit_entry_id"
    ))
    .bind(&event.guild_id)
    .bind(executor)
    .bind(format_iso_millis(lower))
    .bind(format_iso_millis(upper))
    .fetch_all(&mut *tx)
    .await?;
    rows.iter()
        .map(|row| recorded_event(row).map(|recorded| recorded.claimed))
        .collect()
}

async fn advisory_lock(tx: &mut PgConnection, scope: &str) -> Result<(), sqlx::Error> {
    let (high, low) = legacy_lock_key(scope);
    sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
        .bind(high)
        .bind(low)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// Legacy `createHash('sha256')…readInt32BE(0)` / `readInt32BE(4)`.
fn legacy_lock_key(scope: &str) -> (i32, i32) {
    let digest = Sha256::digest(scope.as_bytes());
    (
        i32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]),
        i32::from_be_bytes([digest[4], digest[5], digest[6], digest[7]]),
    )
}

fn iso_in_range(ms: i64) -> Option<String> {
    (MIN_ISO_MS..=MAX_ISO_MS)
        .contains(&ms)
        .then(|| format_iso_millis(ms))
}

/// Only the exact `toISOString` form orders correctly as TEXT.
fn canonical_millis(text: &str) -> Option<i64> {
    let ms = parse_iso_millis(text)?;
    (iso_in_range(ms).as_deref() == Some(text)).then_some(ms)
}

fn event_state_name(state: ContainmentEventState) -> &'static str {
    match state {
        ContainmentEventState::Observe => "observe",
        ContainmentEventState::Contain => "contain",
        ContainmentEventState::Ignored => "ignored",
        ContainmentEventState::Stale => "stale",
    }
}

fn event_state(name: &str) -> Result<ContainmentEventState, ContainmentStoreError> {
    match name {
        "observe" => Ok(ContainmentEventState::Observe),
        "contain" => Ok(ContainmentEventState::Contain),
        "ignored" => Ok(ContainmentEventState::Ignored),
        "stale" => Ok(ContainmentEventState::Stale),
        _ => Err(ContainmentStoreError::Invalid("event state")),
    }
}

fn incident_state(name: &str) -> Result<ContainmentIncidentState, ContainmentStoreError> {
    match name {
        "containing" => Ok(ContainmentIncidentState::Containing),
        "contained" => Ok(ContainmentIncidentState::Contained),
        "dry_run" => Ok(ContainmentIncidentState::DryRun),
        "refused" => Ok(ContainmentIncidentState::Refused),
        "uncertain" => Ok(ContainmentIncidentState::Uncertain),
        "failed" => Ok(ContainmentIncidentState::Failed),
        _ => Err(ContainmentStoreError::Invalid("incident state")),
    }
}

fn recorded_event(row: &PgRow) -> Result<RecordedContainmentEvent, ContainmentStoreError> {
    let action: String = row.try_get("action")?;
    let occurred_at: String = row.try_get("occurred_at")?;
    let state: String = row.try_get("state")?;
    let created_at: String = row.try_get("created_at")?;
    Ok(RecordedContainmentEvent {
        claimed: ClaimedContainmentEvent {
            event: DestructiveAuditEvent {
                audit_entry_id: row.try_get("audit_entry_id")?,
                guild_id: row.try_get("guild_id")?,
                executor_id: row.try_get("executor_id")?,
                action: DestructiveAction::from_name(&action)
                    .ok_or(ContainmentStoreError::Invalid("action"))?,
                target_id: row.try_get("target_id")?,
                occurred_at_ms: Some(
                    canonical_millis(&occurred_at)
                        .ok_or(ContainmentStoreError::Invalid("occurred_at"))?,
                ),
            },
            state: event_state(&state)?,
        },
        reason: row.try_get("reason")?,
        created_at_ms: canonical_millis(&created_at)
            .ok_or(ContainmentStoreError::Invalid("created_at"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_key_matches_legacy_big_endian_prefix() {
        // sha256("") = e3b0c442 98fc1c14 …; readInt32BE(0/4) are signed.
        assert_eq!(
            legacy_lock_key(""),
            (
                i32::from_be_bytes([0xe3, 0xb0, 0xc4, 0x42]),
                i32::from_be_bytes([0x98, 0xfc, 0x1c, 0x14]),
            )
        );
        assert_ne!(legacy_lock_key("g:e"), legacy_lock_key("incident:g:e"));
    }

    #[test]
    fn only_canonical_iso_text_is_accepted() {
        assert_eq!(
            canonical_millis("2026-08-01T10:00:00.000Z"),
            Some(1_785_578_400_000)
        );
        assert_eq!(canonical_millis("2026-08-01T10:00:00Z"), None);
        assert_eq!(canonical_millis("2026-08-01T12:00:00.000+02:00"), None);
        assert_eq!(canonical_millis(" 2026-08-01T10:00:00.000Z"), None);
        assert_eq!(canonical_millis("garbage"), None);
        assert_eq!(
            iso_in_range(MAX_ISO_MS).as_deref(),
            Some("9999-12-31T23:59:59.999Z")
        );
        assert_eq!(
            iso_in_range(MIN_ISO_MS).as_deref(),
            Some("0000-01-01T00:00:00.000Z")
        );
        assert_eq!(iso_in_range(MAX_ISO_MS + 1), None);
    }

    #[test]
    fn state_names_roundtrip() {
        for state in [
            ContainmentEventState::Observe,
            ContainmentEventState::Contain,
            ContainmentEventState::Ignored,
            ContainmentEventState::Stale,
        ] {
            assert_eq!(event_state(event_state_name(state)).unwrap(), state);
        }
        for state in [
            ContainmentIncidentState::Containing,
            ContainmentIncidentState::Contained,
            ContainmentIncidentState::DryRun,
            ContainmentIncidentState::Refused,
            ContainmentIncidentState::Uncertain,
            ContainmentIncidentState::Failed,
        ] {
            assert_eq!(incident_state(state.outcome_label()).unwrap(), state);
        }
    }
}
