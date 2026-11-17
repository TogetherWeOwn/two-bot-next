use super::*;

impl PgSendAdmission {
    pub(super) fn claim_strict(
        &self,
    ) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
        Box::pin(async move {
            // This autocommitted statement arms the fence before returning a permit.
            // An unacknowledged commit is never permission to send.
            let row = sqlx::query(
                "INSERT INTO public.discord_send_admission \
                   (token_key, in_flight, generation, in_flight_since_ms, pending_settlement_generation) \
                 VALUES ($1, TRUE, 1, (extract(epoch FROM clock_timestamp()) * 1000)::bigint, 1) \
                 ON CONFLICT (token_key) DO UPDATE SET \
                   in_flight = TRUE, generation = discord_send_admission.generation + 1, \
                   pending_settlement_generation = discord_send_admission.generation + 1, \
                   in_flight_since_ms = (extract(epoch FROM clock_timestamp()) * 1000)::bigint \
                 WHERE discord_send_admission.pending_settlement_generation IS NULL \
                   AND NOT discord_send_admission.indefinite \
                   AND discord_send_admission.hold_until_ms <= (extract(epoch FROM clock_timestamp()) * 1000)::bigint \
                   AND (NOT discord_send_admission.in_flight OR discord_send_admission.in_flight_since_ms <= \
                        (extract(epoch FROM clock_timestamp()) * 1000)::bigint - $2) \
                 RETURNING generation",
            )
            .bind(&self.key.0)
            .bind(IN_FLIGHT_LEASE_MS)
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| {
                metrics::global().db_error("admission");
                AdmissionError::Storage
            })?;
            let Some(row) = row else {
                return Err(AdmissionError::Blocked);
            };
            let generation = row
                .try_get("generation")
                .map_err(|_| AdmissionError::Storage)?;
            Ok(AdmissionPermit::new(Box::new(StrictCompletion {
                gate: self.clone(),
                generation,
            })))
        })
    }
}

struct StrictCompletion {
    gate: PgSendAdmission,
    generation: i64,
}

impl SendCompletion for StrictCompletion {
    fn complete(
        self: Box<Self>,
        cooldown: Option<SendCooldown>,
    ) -> AdmissionFuture<'static, Result<(), AdmissionError>> {
        Box::pin(async move {
            let delay = finite_delay(cooldown);
            let updated = sqlx::query(
                "UPDATE public.discord_send_admission SET \
                   indefinite = indefinite OR $3, \
                   hold_until_ms = GREATEST(hold_until_ms, CASE WHEN $4::bigint IS NULL THEN 0 ELSE \
                     (extract(epoch FROM clock_timestamp()) * 1000)::bigint + $4 END), \
                   in_flight = FALSE, pending_settlement_generation = NULL \
                 WHERE token_key = $1 AND in_flight AND generation = $2 \
                   AND pending_settlement_generation = $2",
            )
            .bind(&self.gate.key.0)
            .bind(self.generation)
            .bind(cooldown.is_some() && delay.is_none())
            .bind(delay)
            .execute(&self.gate.pool)
            .await
            .map_err(|_| {
                metrics::global().db_error("admission");
                AdmissionError::Storage
            })?;
            if updated.rows_affected() != 1 {
                return Err(AdmissionError::StaleClaim);
            }
            Ok(())
        })
    }
}
