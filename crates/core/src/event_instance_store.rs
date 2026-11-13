//! Accepted Discord event lifecycle evidence, independent of mirror retention.
//!
//! Writers hold one guild ordering lock across the mirror and evidence writes.
//! Binding/claim timestamps are never liveness evidence. Instance identity is
//! established once, adopting only an unambiguous legacy binding; rescheduling
//! never advances an occurrence. There is deliberately no inferred recurrence.

use sqlx::{PgConnection, PgPool};

use crate::scheduled_events::ScheduledEvent;

/// Exclude lifecycle/capture writers before retention takes any table locks.
/// Ordinary writers share this transaction lock across guilds; retention takes
/// it exclusively. Acquire it before guild/event, mirror and history locks.
pub async fn lock_retention(connection: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('event_instance_retention', 0))")
        .execute(connection)
        .await?;
    Ok(())
}

/// Serialize mirror swaps, mutations and binding adoption for one guild.
pub async fn lock_guild(connection: &mut PgConnection, guild_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock_shared(hashtextextended('event_instance_retention', 0))",
    )
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('event_instance_windows:' || $1, 0))",
    )
    .bind(guild_id)
    .execute(connection)
    .await?;
    Ok(())
}

/// Serialize initial instance adoption with channel claims. Writers acquire
/// the guild lock first; claims only acquire this event lock, never the guild.
pub async fn lock_event(connection: &mut PgConnection, event_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('event_instance:' || $1, 0))")
        .bind(event_id)
        .execute(connection)
        .await?;
    Ok(())
}

/// Synchronize an accepted mirror row in its writer's transaction. An absent
/// mirror does not defeat retained ordering, omission or revocation fences.
/// Equal-stamp terminal observations win over live evidence, never vice versa.
/// Only a strictly advancing snapshot may establish evidence at its own epoch;
/// live mutations and equal snapshot replays respect the committed absence fence.
/// Completion may advance an established instance's confirmed lifecycle behind
/// an omission, but cannot establish an unknown instance behind that fence.
pub(crate) async fn observe_in(
    connection: &mut PgConnection,
    guild_id: &str,
    observed_at: &str,
    event: &ScheduledEvent,
    advancing_snapshot: bool,
) -> Result<(), sqlx::Error> {
    lock_event(connection, &event.id).await?;
    sqlx::query(
        "INSERT INTO event_instance_windows AS w
           (guild_id, event_id, instance_starts_at, starts_at, ends_at,
            status, observed_at, revoked_at, identity_ambiguous, attendance_recovered_at)
         SELECT $1, $2,
                COALESCE((SELECT MIN(c.instance_starts_at) FROM event_voice_channels c
                          WHERE c.event_id = $2 HAVING COUNT(*) = 1), $3::text::timestamptz),
                $3::text::timestamptz, $4::text::timestamptz, $5, $6,
                CASE WHEN $5 = 'cancelled' THEN $6 ELSE NULL END,
                (SELECT COUNT(*) > 1 FROM event_voice_channels c WHERE c.event_id = $2), NULL
         WHERE $5 = 'cancelled' OR (
           $5 = 'completed' AND EXISTS (
             SELECT 1 FROM event_instance_windows confirmed
             WHERE confirmed.guild_id = $1 AND confirmed.event_id = $2
               AND confirmed.is_current AND confirmed.revoked_at IS NULL
               AND (confirmed.observed_at < $6
                    OR (confirmed.observed_at = $6
                        AND confirmed.status IN ('scheduled', 'active', 'missing'))))
         ) OR (
           NOT EXISTS (
             SELECT 1 FROM scheduled_events_tombstone t
             WHERE t.guild_id = $1 AND t.event_id = $2 AND t.deleted_at >= $6)
           AND NOT EXISTS (
             SELECT 1 FROM event_instance_windows omitted
             WHERE omitted.guild_id = $1 AND omitted.event_id = $2
               AND omitted.is_current AND omitted.omitted_at >= $6)
           AND NOT EXISTS (
             SELECT 1 FROM scheduled_events_claim c WHERE c.guild_id = $1
               AND (c.epoch > $6 OR (c.epoch = $6 AND NOT $7 AND NOT EXISTS (
                 SELECT 1 FROM event_instance_windows confirmed
                 WHERE confirmed.guild_id = $1 AND confirmed.event_id = $2
                   AND confirmed.is_current AND confirmed.observed_at = $6
                   AND confirmed.status IN ('scheduled', 'active'))))))
         ON CONFLICT (guild_id, event_id) WHERE is_current DO UPDATE SET
           starts_at = EXCLUDED.starts_at, ends_at = EXCLUDED.ends_at,
           status = EXCLUDED.status, observed_at = EXCLUDED.observed_at,
           attendance_recovered_at = CASE
             WHEN w.status = 'missing' AND EXCLUDED.status IN ('scheduled', 'active', 'completed')
             THEN GREATEST(w.omitted_at::timestamptz, EXCLUDED.observed_at::timestamptz)
             ELSE w.attendance_recovered_at END,
           revoked_at = CASE WHEN EXCLUDED.status = 'cancelled'
                             THEN EXCLUDED.observed_at ELSE w.revoked_at END
         WHERE w.observed_at <= EXCLUDED.observed_at
           AND (w.revoked_at IS NULL OR EXCLUDED.status = 'cancelled')
           AND (w.observed_at < EXCLUDED.observed_at
                OR (EXCLUDED.status = 'cancelled' AND w.status <> 'deleted')
                OR (EXCLUDED.status = 'completed' AND w.status IN ('scheduled', 'active', 'missing')))",
    )
    .bind(guild_id)
    .bind(&event.id)
    .bind(&event.starts_at)
    .bind(&event.ends_at)
    .bind(event.status.as_str())
    .bind(observed_at)
    .bind(advancing_snapshot)
    .execute(&mut *connection)
    .await?;
    stop_attendance_in(connection, guild_id).await
}

// Retain finalization boundaries before a later positive observation can replace
// missing evidence. This is historical credit metadata, never capture authority.
// At most one stop after the last capture is needed; repeated omission polls
// cannot grow a captured record indefinitely.
async fn stop_attendance_in(
    connection: &mut PgConnection,
    guild_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE event_attendance_windows h
         SET capture_stops = array_append(h.capture_stops, s.stopped_at),
             changed_at = clock_timestamp()
         FROM event_instance_windows w
         CROSS JOIN LATERAL (
           SELECT LEAST(w.revoked_at::timestamptz,
                        CASE WHEN w.status = 'missing' THEN w.omitted_at::timestamptz END) AS stopped_at
         ) s
         WHERE h.guild_id = $1 AND w.guild_id = h.guild_id AND w.event_id = h.event_id
           AND w.instance_starts_at = h.instance_starts_at AND s.stopped_at IS NOT NULL
           AND NOT (s.stopped_at = ANY(h.capture_stops))
           AND NOT EXISTS (SELECT 1 FROM unnest(h.capture_stops) stopped
                           WHERE stopped >= h.updated_at)",
    )
    .bind(guild_id)
    .execute(connection)
    .await?;
    Ok(())
}

/// An accepted exact replacement makes omitted live evidence missing. Only a
/// positively completed, finite window remains eligible; its end is never
/// restamped. Preserve even unknown-end completion status so omission cannot
/// turn an equal-stamp completion replay into new finite grace.
/// The guild snapshot epoch fences omission replays. Do not advance the last
/// confirmed lifecycle stamp here: a delayed terminal observation can still be
/// newer than that evidence without being newer than an ordinary omission.
/// The caller holds the guild lock and all affected event locks.
pub(crate) async fn omit_in(
    connection: &mut PgConnection,
    guild_id: &str,
    observed_at: &str,
    incoming_ids: &[&str],
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE event_instance_windows
         SET status = CASE WHEN revoked_at IS NOT NULL THEN status
                           WHEN status = 'completed' THEN status
                           ELSE 'missing' END,
             omitted_at = $2
         WHERE guild_id = $1 AND is_current AND observed_at <= $2
           AND (omitted_at IS NULL OR omitted_at <= $2)
           AND NOT (event_id = ANY($3::text[]))",
    )
    .bind(guild_id)
    .bind(observed_at)
    .bind(incoming_ids)
    .execute(&mut *connection)
    .await?;
    stop_attendance_in(connection, guild_id).await
}

/// Revoke a positively confirmed deletion, including retained completion grace.
/// This is not the poller's omission path. Keep the row even when no mirror is
/// left so older/equal observations cannot recreate live evidence.
pub async fn confirm_deleted(
    pool: &PgPool,
    guild_id: &str,
    event_id: &str,
    observed_at: &str,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    lock_guild(&mut tx, guild_id).await?;
    lock_event(&mut tx, event_id).await?;
    let known: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM event_instance_windows WHERE guild_id = $1 AND event_id = $2)",
    )
    .bind(guild_id)
    .bind(event_id)
    .fetch_one(&mut *tx)
    .await?;
    if !known {
        // No confirmed initial start exists to establish an occurrence. The
        // caller must reconcile, not acknowledge a fence we did not persist.
        return Err(sqlx::Error::RowNotFound);
    }
    sqlx::query(
        "UPDATE event_instance_windows SET status = 'deleted',
             observed_at = $3, revoked_at = $3
         WHERE guild_id = $1 AND event_id = $2 AND observed_at <= $3",
    )
    .bind(guild_id)
    .bind(event_id)
    .bind(observed_at)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM scheduled_events WHERE guild_id = $1 AND event_id = $2 AND updated_at <= $3",
    )
    .bind(guild_id)
    .bind(event_id)
    .bind(observed_at)
    .execute(&mut *tx)
    .await?;
    stop_attendance_in(&mut tx, guild_id).await?;
    tx.commit().await
}

/// Delete at most 25 expired, unreferenced evidence rows per accepted poll.
/// A remaining binding, key or mirror pins its ordering/revocation fence,
/// independently of the mirror tombstone table's age and row-count hygiene.
/// The retention horizon uses the window start, not the latest observation or
/// guild snapshot epoch. Repeated polls cannot postpone cleanup forever.
pub(crate) async fn maintain_in(
    connection: &mut PgConnection,
    guild_id: &str,
    cutoff: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM event_instance_windows w
         WHERE (w.guild_id, w.event_id, w.instance_starts_at) IN (
           SELECT e.guild_id, e.event_id, e.instance_starts_at FROM event_instance_windows e
           WHERE e.guild_id = $1 AND e.starts_at < $2::text::timestamptz
             AND e.status NOT IN ('scheduled', 'active')
             AND (e.ends_at IS NULL OR e.ends_at < NOW() - INTERVAL '15 minutes')
             AND NOT EXISTS (SELECT 1 FROM event_voice_channels c WHERE c.event_id = e.event_id)
             AND NOT EXISTS (SELECT 1 FROM event_attendance_windows h
                             WHERE h.guild_id = e.guild_id AND h.event_id = e.event_id
                               AND h.instance_starts_at = e.instance_starts_at)
             AND NOT EXISTS (SELECT 1 FROM internal_event_keys k
                             WHERE k.guild_id = e.guild_id AND k.event_id = e.event_id)
             AND NOT EXISTS (SELECT 1 FROM scheduled_events s
                             WHERE s.guild_id = e.guild_id AND s.event_id = e.event_id)
           ORDER BY e.starts_at, e.event_id LIMIT 25)",
    )
    .bind(guild_id)
    .bind(cutoff)
    .execute(connection)
    .await?;
    Ok(())
}
