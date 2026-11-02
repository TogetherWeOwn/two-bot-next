//! Bounded event reminder reads and a durable pre-wire delivery fence.
//!
//! The existing scheduler supplies timing and confirmed channel transport. As
//! with its uncertainty fence and `claim_text_attempt`, admission commits BEFORE
//! one POST; a crash or lost database/Discord reply never grants another send.
//! The immutable system-actor audit fact survives mirror replacement, member
//! erasure and backup/restore. Never prune these facts: they are replay guards,
//! not expiring telemetry. At capacity new reminders fail closed.

use sqlx::PgPool;

use crate::rsvp::MAX_RSVPS_PER_EVENT;
use crate::scheduled::format_iso_ms;

/// Fixed, non-configurable lead time; the first tick inside this window sends.
pub const EVENT_REMINDER_LEAD_MS: u64 = 15 * 60 * 1000;
// Leave room in the existing ticker's timeout for scheduled-message cleanup.
const EVENT_REMINDER_BATCH_LIMIT: i64 = 2;
/// Replay evidence is retained, not recycled. A full guild stops admission.
const DELIVERY_CAP: i64 = 10_000;

#[derive(sqlx::FromRow)]
pub struct EventReminder {
    pub event_id: String,
    pub name: String,
    pub starts_at: String,
    pub channel_id: String,
    pub attendees: Vec<String>,
    /// Exact eligible total, not the overflow sentinel in the capped array.
    pub attendee_count: i64,
}

/// Only scheduled future events in this guild with attendees and a channel.
/// The candidate batch and attendee array are bounded, even for rows that
/// bypassed RSVP admission. A guild/event-scoped scalar count covers the full
/// eligible roster without loading its members. Completed fences never starve
/// later events.
pub async fn due_event_reminders(
    pool: &PgPool,
    guild: &str,
    now: u64,
) -> Result<Vec<EventReminder>, sqlx::Error> {
    sqlx::query_as(
        "SELECT e.event_id, e.name, e.starts_at, e.channel_id,
                ARRAY(SELECT r.user_id FROM event_rsvps r
                      WHERE r.guild_id = e.guild_id AND r.event_id = e.event_id
                        AND r.status IN ('going', 'interested')
                      ORDER BY r.user_id LIMIT $5) AS attendees,
                (SELECT count(*) FROM event_rsvps r
                  WHERE r.guild_id = e.guild_id AND r.event_id = e.event_id
                    AND r.status IN ('going', 'interested')) AS attendee_count
           FROM scheduled_events e
          WHERE e.guild_id = $1 AND e.status = 'scheduled'
            AND e.channel_id IS NOT NULL AND e.starts_at > $2 AND e.starts_at <= $3
            AND EXISTS (SELECT 1 FROM event_rsvps r
                         WHERE r.guild_id = e.guild_id AND r.event_id = e.event_id
                           AND r.status IN ('going', 'interested'))
            AND NOT EXISTS (SELECT 1 FROM automation_audit_log d
                             WHERE d.id = 'event:reminder:' || e.guild_id || ':' || e.event_id)
          ORDER BY e.starts_at, e.event_id LIMIT $4",
    )
    .bind(guild)
    .bind(format_iso_ms(now))
    .bind(format_iso_ms(now.saturating_add(EVENT_REMINDER_LEAD_MS)))
    .bind(EVENT_REMINDER_BATCH_LIMIT)
    .bind(MAX_RSVPS_PER_EVENT + 1)
    .fetch_all(pool)
    .await
}

/// Revalidate the mirror, attendees and time at dispatch, then atomically arm a
/// per-(guild,event) fence in the existing automation audit log. ONLY a committed
/// new claim authorizes POST; an unknown commit result authorizes nothing.
/// Serializing guild admission also makes the retained-fence cap race-safe.
/// NULL actor and event-only target contain no member data to erase.
pub async fn begin_event_reminder_delivery(
    pool: &PgPool,
    guild: &str,
    event: &EventReminder,
    now: u64,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('event_reminders:' || $1, 0))")
        .bind(guild)
        .execute(&mut *tx)
        .await?;
    let inserted = sqlx::query(
        "INSERT INTO automation_audit_log
           (id, guild_id, actor_id, action, target_key, outcome, reason, created_at)
         SELECT 'event:reminder:' || e.guild_id || ':' || e.event_id,
                e.guild_id, NULL, 'rsvp.reminder_attempt', e.event_id,
                'unknown', 'delivery_pending', $5::text::timestamptz
           FROM scheduled_events e
          WHERE e.guild_id = $1 AND e.event_id = $2 AND e.status = 'scheduled'
            AND e.channel_id = $3 AND e.starts_at = $4
            AND e.starts_at > $5 AND e.starts_at <= $6 AND e.name = $7
            AND ARRAY(SELECT r.user_id FROM event_rsvps r
                       WHERE r.guild_id = e.guild_id AND r.event_id = e.event_id
                         AND r.status IN ('going', 'interested')
                       ORDER BY r.user_id LIMIT $10) = $9
            AND (SELECT count(*) FROM event_rsvps r
                  WHERE r.guild_id = e.guild_id AND r.event_id = e.event_id
                    AND r.status IN ('going', 'interested')) = $11
            AND (SELECT count(*) FROM automation_audit_log
                  WHERE guild_id = $1 AND action = 'rsvp.reminder_attempt') < $8
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(guild)
    .bind(&event.event_id)
    .bind(&event.channel_id)
    .bind(&event.starts_at)
    .bind(format_iso_ms(now))
    .bind(format_iso_ms(now.saturating_add(EVENT_REMINDER_LEAD_MS)))
    .bind(&event.name)
    .bind(DELIVERY_CAP)
    .bind(&event.attendees)
    .bind(MAX_RSVPS_PER_EVENT + 1)
    .bind(event.attendee_count)
    .execute(&mut *tx)
    .await?;
    let admitted = inserted.rows_affected() == 1;
    tx.commit().await?;
    Ok(admitted)
}
