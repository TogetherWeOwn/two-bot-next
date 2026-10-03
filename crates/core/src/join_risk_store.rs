//! Durable join-risk claims (`docs/raid-port.md` runtime obligations).
//!
//! Ports legacy `two-bot` at `d5d1179348feb9157bcac8c875de9399d4f5c76a`:
//! `src/moderation/containmentStore.ts` (`recordJoinRisk`, blob
//! `283e877060ca4398c8f700a6752b00026c1e73b1`) over the legacy table
//! recreated by migration `0360_join_risk_flags.sql`. Scoring stays in
//! [`JoinRiskObservation::score`]; this store makes the
//! check-duplicate / count / score / insert sequence atomic and restart-safe.
//! It registers no listener, executes nothing and does not arm containment.
//!
//! The advisory-lock key uses the legacy derivation (first two big-endian
//! `int4`s of SHA-256 over `joins:{guild}`), so a legacy process on the same
//! database serializes against this store. Timestamps are canonical
//! `YYYY-MM-DDTHH:MM:SS.sssZ` TEXT, compared lexicographically like legacy.

use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};

use crate::funnel::format_iso_millis;
use crate::raid::{JoinRiskEvidence, JoinRiskObservation};

/// `0000-01-01T00:00:00.000Z`: the earliest four-digit-year ISO instant.
const MIN_ISO_MS: i64 = -62_167_219_200_000;
/// `9999-12-31T23:59:59.999Z`: later instants lose lexicographic ordering.
const MAX_ISO_MS: i64 = 253_402_300_799_999;

#[derive(Debug, thiserror::Error)]
pub enum JoinRiskStoreError {
    #[error("join-risk store database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("invalid join-risk record: {0}")]
    Invalid(&'static str),
}

/// Result of [`JoinRiskStore::record`].
#[derive(Debug, Clone, PartialEq)]
pub enum JoinRiskClaim {
    /// This call inserted the observation and persisted its evidence.
    /// Pass `persisted = true` to [`JoinRiskEvidence::staff_message`].
    Persisted {
        evidence: JoinRiskEvidence,
        /// Prior claimed rows in the window plus this join (legacy `joinCount`).
        join_count: u64,
    },
    /// The event ID was already claimed. Nothing is recounted or reinserted
    /// (legacy `{ persisted: false, score: 0, reasons: [], flagged: false }`).
    /// A failed send must not turn this replay into a second alert.
    Duplicate,
}

/// Durable claims over the legacy `join_risk_flags` table.
#[derive(Debug, Clone)]
pub struct JoinRiskStore {
    pool: PgPool,
}

impl JoinRiskStore {
    /// Reuse the runtime's pool instead of opening a separate pool per handler.
    #[must_use]
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Atomically claim `observation.event_id` and persist its evidence at
    /// `now_ms` (legacy `recordJoinRisk`).
    ///
    /// Claims for one guild are serialized; the recent-row count and score are
    /// computed inside that lock, so concurrent joins count each event ID once
    /// and every claim sees every earlier claim exactly once. All claimed rows
    /// count — including unflagged and bulk-suppressed ones — and the current
    /// join contributes one, exactly like the legacy `COUNT(*)` plus one.
    pub async fn record(
        &self,
        observation: JoinRiskObservation,
        now_ms: i64,
    ) -> Result<JoinRiskClaim, JoinRiskStoreError> {
        let now = iso_in_range(now_ms).ok_or(JoinRiskStoreError::Invalid("processing time"))?;
        // Legacy `new Date(now - windowSeconds * 1000)`: fractional windows
        // truncate toward the epoch, matching the `Date` constructor.
        let cutoff_ms = (now_ms as f64 - observation.window_seconds() * 1000.0) as i64;
        let cutoff = iso_in_range(cutoff_ms.max(MIN_ISO_MS))
            .ok_or(JoinRiskStoreError::Invalid("window cutoff"))?;
        let mut tx = self.pool.begin().await?;
        advisory_lock(
            &mut tx,
            &format!("joins:{guild}", guild = observation.guild_id),
        )
        .await?;
        let existing: Option<String> =
            sqlx::query_scalar("SELECT event_id FROM join_risk_flags WHERE event_id = $1")
                .bind(&observation.event_id)
                .fetch_optional(&mut *tx)
                .await?;
        if existing.is_some() {
            tx.commit().await?;
            return Ok(JoinRiskClaim::Duplicate);
        }
        let recent: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM join_risk_flags \
             WHERE guild_id = $1 AND created_at > $2 AND created_at <= $3",
        )
        .bind(&observation.guild_id)
        .bind(&cutoff)
        .bind(&now)
        .fetch_one(&mut *tx)
        .await?;
        let evidence = observation.score(recent.max(0) as u64);
        let reasons = serde_json::to_string(&evidence.reasons)
            .map_err(|_| JoinRiskStoreError::Invalid("reasons"))?;
        sqlx::query(
            "INSERT INTO join_risk_flags \
               (event_id, guild_id, member_id, account_created_at, joined_at, source, score, \
                reasons_json, bulk_join_window, flagged, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(&evidence.observation.event_id)
        .bind(&evidence.observation.guild_id)
        .bind(&evidence.observation.member_id)
        .bind(&evidence.observation.account_created_at)
        .bind(&evidence.observation.joined_at)
        .bind(&evidence.observation.source)
        .bind(i32::from(evidence.score))
        .bind(&reasons)
        .bind(evidence.observation.bulk_join_window)
        .bind(evidence.flagged)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(JoinRiskClaim::Persisted {
            join_count: evidence.join_count,
            evidence,
        })
    }
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
        assert_ne!(legacy_lock_key("joins:g"), legacy_lock_key("g:e"));
    }

    #[test]
    fn only_canonical_iso_text_is_accepted() {
        assert_eq!(
            iso_in_range(1_785_578_400_000),
            Some("2026-08-01T10:00:00.000Z".to_owned())
        );
        assert_eq!(iso_in_range(MIN_ISO_MS - 1), None);
        assert_eq!(iso_in_range(MAX_ISO_MS + 1), None);
    }
}
