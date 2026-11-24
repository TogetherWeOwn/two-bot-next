//! Durable reservation for the ordered producer in one environment's database.
//!
//! Only the ordered guild owner publishes. The database arbitrates reservations,
//! while the owner serializes publication and retires intake after a failed
//! reservation. A lost acknowledgement can leave a durable gap: read the
//! watermark again and rebuild through [super::recovery::Recovery] before
//! offering more deltas.

use sqlx::PgPool;

use super::{Id, MAX_SEQ};

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReservationError {
    #[error("invalid live sequence advance")]
    InvalidAdvance,
    #[error("live sequence watermark changed")]
    Conflict,
    #[error("live sequence storage unavailable")]
    Storage,
}

/// Read the last reserved sequence, or zero for a guild with no reservations.
/// The watermark can be ahead of the last delta the hub received.
pub async fn read(pool: &PgPool, guild: Id) -> Result<u64, ReservationError> {
    let stored: Option<i64> =
        sqlx::query_scalar("SELECT seq FROM public.live_sequence_watermarks WHERE guild_id = $1")
            .bind(guild.0.to_string())
            .fetch_optional(pool)
            .await
            .map_err(|_| ReservationError::Storage)?;
    match stored {
        None => Ok(0),
        Some(seq) => u64::try_from(seq)
            .ok()
            .filter(|seq| (1..=MAX_SEQ).contains(seq))
            .ok_or(ReservationError::Storage),
    }
}

/// Compare and advance by one for a delta or by two for a recovery marker.
/// Success is returned only after commit. Only an expected zero can create a
/// row. A losing comparison returns [ReservationError::Conflict], even when
/// the winner stored the same target sequence.
pub async fn reserve(
    pool: &PgPool,
    guild: Id,
    expected: u64,
    next: u64,
) -> Result<(), ReservationError> {
    if next > MAX_SEQ || !matches!(next.checked_sub(expected), Some(1 | 2)) {
        return Err(ReservationError::InvalidAdvance);
    }
    let next = i64::try_from(next).map_err(|_| ReservationError::InvalidAdvance)?;
    let expected = i64::try_from(expected).map_err(|_| ReservationError::InvalidAdvance)?;
    let mut tx = pool.begin().await.map_err(|_| ReservationError::Storage)?;
    // A successful reservation must survive a crash even if the session's
    // default allows asynchronous commits.
    sqlx::query("SET LOCAL synchronous_commit = 'on'")
        .execute(&mut *tx)
        .await
        .map_err(|_| ReservationError::Storage)?;
    let changed = if expected == 0 {
        sqlx::query(
            "INSERT INTO public.live_sequence_watermarks (guild_id, seq)
             VALUES ($1, $2) ON CONFLICT (guild_id) DO NOTHING",
        )
        .bind(guild.0.to_string())
        .bind(next)
        .execute(&mut *tx)
        .await
    } else {
        sqlx::query(
            "UPDATE public.live_sequence_watermarks SET seq = $2
             WHERE guild_id = $1 AND seq = $3",
        )
        .bind(guild.0.to_string())
        .bind(next)
        .bind(expected)
        .execute(&mut *tx)
        .await
    }
    .map_err(|_| ReservationError::Storage)?
    .rows_affected();
    if changed == 0 {
        return Err(ReservationError::Conflict);
    }
    tx.commit().await.map_err(|_| ReservationError::Storage)
}
