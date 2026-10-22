//! Event voice channel claims and presence spans (migration 0454).
//!
//! One row per event instance in `event_voice_channels`, keyed
//! `UNIQUE (event_id, instance_starts_at)`: the create path claims with
//! `INSERT ... ON CONFLICT DO NOTHING` holding `owner_tick_lease` before any
//! Discord call, so two workers or a double tick yield one channel. A
//! `starts_at` shift updates the row in place (channel kept); a new
//! occurrence starts a new row. Per-member join/leave spans persist in
//! `event_voice_presence` on the voice packets themselves; the 60s flush is a
//! repair-only backstop.
//!
//! Staleness is lease age (`updated_at`), not claim age (`created_at`): every
//! bind/clear/orphan write moves `updated_at`, and the ticker's re-lease
//! must too, so the reconcile pass never probes (or clears) a freshly leased
//! row. `created_at` stays the immutable claim instant for the rollback
//! measure. All Discord verification lives with the caller: these are pure
//! SQL primitives, and the orchestration in the bot crate owns the probes.

use super::voice_rooms::PgRoomStore;
use time::OffsetDateTime;

/// Claim one event-voice create: `INSERT ... ON CONFLICT DO NOTHING` before
/// any Discord call. Returns true when this call won the insert and holds
/// `lease`; a false loser must not call Discord.
pub async fn claim_event_voice_channel(
    pool: &sqlx::PgPool,
    event_id: &str,
    instance_starts_at: OffsetDateTime,
    starts_at: OffsetDateTime,
    ends_at: Option<OffsetDateTime>,
    lease: &str,
) -> Result<bool, sqlx::Error> {
    let won = sqlx::query(
        "INSERT INTO event_voice_channels
         (event_id, instance_starts_at, starts_at, ends_at, status,
          owner_tick_lease, voice_channel_id, created_at, updated_at)
         VALUES ($1,$2,$3,$4,'creating',$5,NULL,NOW(),NOW())
         ON CONFLICT DO NOTHING
         RETURNING owner_tick_lease",
    )
    .bind(event_id)
    .bind(instance_starts_at)
    .bind(starts_at)
    .bind(ends_at)
    .bind(lease)
    .fetch_optional(pool)
    .await?
    .is_some();
    Ok(won)
}

/// `creating` rows with no channel untouched for longer than `stale_secs`,
/// as `(event_id, instance_starts_at, lease)`, oldest first. The caller
/// verifies each via Discord GET: bind the found channel, else clear the
/// lease so the next tick retries.
pub async fn stale_event_voice_claims(
    pool: &sqlx::PgPool,
    stale_secs: i64,
) -> Result<Vec<(String, OffsetDateTime, Option<String>)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT event_id, instance_starts_at, owner_tick_lease
         FROM event_voice_channels
         WHERE status = 'creating' AND voice_channel_id IS NULL
           AND updated_at < NOW() - make_interval(secs => $1::integer)
         ORDER BY updated_at",
    )
    .bind(stale_secs)
    .fetch_all(pool)
    .await
}

/// Bind a Discord-confirmed channel to an unbound `creating` row. Returns
/// false when the row was already bound (or vanished), so a racing tick
/// never overwrites a live binding.
pub async fn bind_event_voice_channel(
    pool: &sqlx::PgPool,
    event_id: &str,
    instance_starts_at: OffsetDateTime,
    channel_id: &str,
) -> Result<bool, sqlx::Error> {
    let bound = sqlx::query(
        "UPDATE event_voice_channels
         SET voice_channel_id = $3, updated_at = NOW()
         WHERE event_id = $1 AND instance_starts_at = $2
           AND status = 'creating' AND voice_channel_id IS NULL",
    )
    .bind(event_id)
    .bind(instance_starts_at)
    .bind(channel_id)
    .execute(pool)
    .await?
    .rows_affected()
        != 0;
    Ok(bound)
}

/// Clear the lease on an unbound row the Discord GET could not confirm, so
/// the next tick retries. Fenced to `expected_lease`: a live newer lease is
/// never wiped by a stale probe.
pub async fn clear_event_voice_lease(
    pool: &sqlx::PgPool,
    event_id: &str,
    instance_starts_at: OffsetDateTime,
    expected_lease: &str,
) -> Result<bool, sqlx::Error> {
    let cleared = sqlx::query(
        "UPDATE event_voice_channels
         SET owner_tick_lease = NULL, updated_at = NOW()
         WHERE event_id = $1 AND instance_starts_at = $2
           AND owner_tick_lease = $3",
    )
    .bind(event_id)
    .bind(instance_starts_at)
    .bind(expected_lease)
    .execute(pool)
    .await?
    .rows_affected()
        != 0;
    Ok(cleared)
}

/// Bound non-orphaned rows as `(event_id, instance_starts_at, channel_id)`,
/// for the orphan pass. Each channel id is verified via Discord GET.
pub async fn bound_event_voice_channels(
    pool: &sqlx::PgPool,
) -> Result<Vec<(String, OffsetDateTime, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT event_id, instance_starts_at, voice_channel_id
         FROM event_voice_channels
         WHERE voice_channel_id IS NOT NULL AND status <> 'orphaned'
         ORDER BY voice_channel_id",
    )
    .fetch_all(pool)
    .await
}

/// Mark a 404'd row `orphaned` and clear its lease for one retry. The dead
/// channel id stays as evidence. This procedure contains no DELETE: an
/// occupied room is never deleted.
pub async fn mark_event_voice_orphaned(
    pool: &sqlx::PgPool,
    event_id: &str,
    instance_starts_at: OffsetDateTime,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE event_voice_channels
         SET status = 'orphaned', owner_tick_lease = NULL, updated_at = NOW()
         WHERE event_id = $1 AND instance_starts_at = $2",
    )
    .bind(event_id)
    .bind(instance_starts_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Restore an orphaned row to `creating` when the retry GET found the
/// channel (transient 404). The lease stays cleared; the next tick re-leases.
pub async fn revive_event_voice_orphan(
    pool: &sqlx::PgPool,
    event_id: &str,
    instance_starts_at: OffsetDateTime,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE event_voice_channels
         SET status = 'creating', updated_at = NOW()
         WHERE event_id = $1 AND instance_starts_at = $2 AND status = 'orphaned'",
    )
    .bind(event_id)
    .bind(instance_starts_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert an open span for a join/move-in, resolving (event, instance) from
/// the channel binding. A channel with no event mapping writes nothing, and
/// an already-open span is kept (no duplicate): rejoins after a leave open a
/// second span because the first is closed.
pub async fn record_event_voice_join(
    pool: &sqlx::PgPool,
    channel_id: &str,
    user_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO event_voice_presence (event_id, instance, user_id, joined_at)
         SELECT c.event_id, c.instance_starts_at, $2, NOW()
         FROM event_voice_channels c
         WHERE c.voice_channel_id = $1
           AND NOT EXISTS (
             SELECT 1 FROM event_voice_presence p
             WHERE p.event_id = c.event_id AND p.instance = c.instance_starts_at
               AND p.user_id = $2 AND p.left_at IS NULL)
         ON CONFLICT DO NOTHING",
    )
    .bind(channel_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Stamp `left_at` on the member's open span in the channel's event. A
/// channel with no mapping, or a member with no open span, updates nothing.
pub async fn record_event_voice_leave(
    pool: &sqlx::PgPool,
    channel_id: &str,
    user_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE event_voice_presence p
         SET left_at = NOW()
         FROM event_voice_channels c
         WHERE c.voice_channel_id = $1
           AND p.event_id = c.event_id AND p.instance = c.instance_starts_at
           AND p.user_id = $2 AND p.left_at IS NULL",
    )
    .bind(channel_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Repair-only backstop: ensure one open span per occupant of the instance.
/// Never closes spans (leave packets own that); returns rows inserted.
pub async fn flush_event_voice_presence(
    pool: &sqlx::PgPool,
    event_id: &str,
    instance_starts_at: OffsetDateTime,
    occupant_user_ids: &[String],
) -> Result<u64, sqlx::Error> {
    let inserted = sqlx::query(
        "INSERT INTO event_voice_presence (event_id, instance, user_id, joined_at)
         SELECT $1, $2, occupant, NOW()
         FROM UNNEST($3::TEXT[]) AS occupant
         WHERE NOT EXISTS (
           SELECT 1 FROM event_voice_presence p
           WHERE p.event_id = $1 AND p.instance = $2
             AND p.user_id = occupant AND p.left_at IS NULL)
         ON CONFLICT DO NOTHING",
    )
    .bind(event_id)
    .bind(instance_starts_at)
    .bind(occupant_user_ids)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(inserted)
}

impl PgRoomStore {
    /// [`claim_event_voice_channel`] over this store's pool.
    pub async fn claim_event_voice_channel(
        &self,
        event_id: &str,
        instance_starts_at: OffsetDateTime,
        starts_at: OffsetDateTime,
        ends_at: Option<OffsetDateTime>,
        lease: &str,
    ) -> Result<bool, sqlx::Error> {
        claim_event_voice_channel(
            self.pool(),
            event_id,
            instance_starts_at,
            starts_at,
            ends_at,
            lease,
        )
        .await
    }

    /// [`record_event_voice_join`] over this store's pool.
    pub async fn record_event_voice_join(
        &self,
        channel_id: &str,
        user_id: &str,
    ) -> Result<(), sqlx::Error> {
        record_event_voice_join(self.pool(), channel_id, user_id).await
    }

    /// [`record_event_voice_leave`] over this store's pool.
    pub async fn record_event_voice_leave(
        &self,
        channel_id: &str,
        user_id: &str,
    ) -> Result<(), sqlx::Error> {
        record_event_voice_leave(self.pool(), channel_id, user_id).await
    }
}
