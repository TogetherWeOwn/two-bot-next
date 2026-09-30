use super::*;
use sqlx::{PgPool, Row};

/// Every consumer of a credential must use the same authoritative database.
/// Fully qualified table names prevent search_path from splitting a token lane.
#[derive(Clone, Debug)]
pub struct PgSendAdmission {
    pool: PgPool,
    key: TokenKey,
}

impl PgSendAdmission {
    pub fn new(pool: PgPool, token: &str) -> Result<Self, AdmissionError> {
        Ok(Self { pool, key: TokenKey::for_bot_token(token)? })
    }

    /// Monotonic external extension; never clears an occupied/indefinite lane.
    pub async fn extend(&self, cooldown: SendCooldown) -> Result<(), AdmissionError> {
        let delay = finite_delay(Some(cooldown));
        sqlx::query(
            "INSERT INTO public.discord_send_admission (token_key, indefinite, hold_until_ms) \
             VALUES ($1, $2, CASE WHEN $3::bigint IS NULL THEN 0 ELSE \
               (extract(epoch FROM clock_timestamp()) * 1000)::bigint + $3 END) \
             ON CONFLICT (token_key) DO UPDATE SET \
               indefinite = discord_send_admission.indefinite OR EXCLUDED.indefinite, \
               hold_until_ms = GREATEST(discord_send_admission.hold_until_ms, EXCLUDED.hold_until_ms)",
        )
        .bind(&self.key.0)
        .bind(delay.is_none())
        .bind(delay)
        .execute(&self.pool)
        .await
        .map_err(|_| AdmissionError::Storage)?;
        Ok(())
    }
}

// Reserve space for the DB clock too. Unrepresentable timing becomes an
// indefinite hold, never a shorter guessed delay or arithmetic wraparound.
fn finite_delay(cooldown: Option<SendCooldown>) -> Option<i64> {
    match cooldown {
        Some(SendCooldown::FiniteMs(ms)) if ms <= i64::MAX as u64 / 2 => Some(ms as i64),
        _ => None,
    }
}

impl SendAdmission for PgSendAdmission {
    fn token_key(&self) -> &TokenKey { &self.key }

    fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
        Box::pin(async move {
            // One autocommitted statement, not a transaction held over HTTP.
            // Commit precedes sending. Losing/cancelling this query can only
            // leave a durable occupied row; it cannot grant another sender.
            let row = sqlx::query(
                "INSERT INTO public.discord_send_admission (token_key, in_flight, generation) \
                 VALUES ($1, TRUE, 1) ON CONFLICT (token_key) DO UPDATE SET \
                   in_flight = TRUE, generation = discord_send_admission.generation + 1 \
                 WHERE NOT discord_send_admission.in_flight \
                   AND NOT discord_send_admission.indefinite \
                   AND discord_send_admission.hold_until_ms <= \
                     (extract(epoch FROM clock_timestamp()) * 1000)::bigint \
                 RETURNING generation",
            )
            .bind(&self.key.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| AdmissionError::Storage)?
            .ok_or(AdmissionError::Blocked)?;
            let generation: i64 = row.try_get("generation").map_err(|_| AdmissionError::Storage)?;
            Ok(AdmissionPermit::new(Box::new(PgCompletion { gate: self.clone(), generation })))
        })
    }
}

struct PgCompletion {
    gate: PgSendAdmission,
    generation: i64,
}

impl SendCompletion for PgCompletion {
    fn complete(self: Box<Self>, cooldown: Option<SendCooldown>) -> AdmissionFuture<'static, Result<(), AdmissionError>> {
        Box::pin(async move {
            let delay = finite_delay(cooldown);
            let indefinite = cooldown.is_some() && delay.is_none();
            let updated = sqlx::query(
                "UPDATE public.discord_send_admission SET \
                   indefinite = indefinite OR $3, \
                   hold_until_ms = GREATEST(hold_until_ms, CASE WHEN $4::bigint IS NULL THEN 0 ELSE \
                     (extract(epoch FROM clock_timestamp()) * 1000)::bigint + $4 END), \
                   in_flight = FALSE \
                 WHERE token_key = $1 AND generation = $2 AND in_flight",
            )
            .bind(&self.gate.key.0)
            .bind(self.generation)
            .bind(indefinite)
            .bind(delay)
            .execute(&self.gate.pool)
            .await
            .map_err(|_| AdmissionError::Storage)?;
            if updated.rows_affected() != 1 { return Err(AdmissionError::StaleClaim); }
            Ok(())
        })
    }
}
