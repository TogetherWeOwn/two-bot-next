//! Onboarding sqlx store (TOG-10086, migration block 0190–0199).
//!
//! Thin persistence over the funnel `events` table (legacy names preserved):
//! the once-per-member `onboarding_prompted` guard plus the three recorder
//! writes from [`crate::onboarding`]. Insert-first arbitration
//! (`ON CONFLICT DO NOTHING` on the idempotency key) decides the winner
//! atomically — the same rule as the cutover seam in `two-bot-cutover` and
//! legacy `EventStore.record` — and the members projection is untouched
//! (onboarding rows project nothing; `members` columns for this flow do not
//! exist, by the same design that keeps `second_message` a log marker).
//!
//! Behind the `db` feature so pure-domain unit tests never need Postgres.

use sqlx::{Pool, Postgres};

use super::onboarding::{
    channel_routed_row, funnel_idempotency_key, game_selected_row, prompted_row,
    session_routed_row, FunnelRow, GameSelection, SessionPlan, EVENT_ONBOARDING_PROMPTED,
};

/// Store failure (transparent sqlx error; messages never include secrets).
#[derive(Debug, thiserror::Error)]
pub enum OnboardingStoreError {
    #[error("onboarding store: {0}")]
    Sqlx(#[from] sqlx::Error),
}

async fn insert_row(pool: &Pool<Postgres>, row: &FunnelRow) -> Result<bool, sqlx::Error> {
    let key = funnel_idempotency_key(row);
    let inserted: Option<(i64,)> = sqlx::query_as(
        "INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
         VALUES ($1, $2, $3, $4::timestamptz, $5, $6, $7)
         ON CONFLICT (idempotency_key) DO NOTHING RETURNING id",
    )
    .bind(row.event_type)
    .bind(&row.member_id)
    .bind(&row.guild_id)
    .bind(&row.occurred_at)
    .bind(&row.source)
    .bind(&row.metadata)
    .bind(&key)
    .fetch_optional(pool)
    .await?;
    Ok(inserted.is_some())
}

/// The once-per-member guard behind [`crate::onboarding::decide_prompt`]:
/// true once the welcome has been recorded (legacy `store.hasEvent(…,
/// 'onboarding_prompted')`). The insert path arbitrates again at write time,
/// so a race between the check and the write still prompts once.
pub async fn has_onboarding_prompt(
    pool: &Pool<Postgres>,
    guild_id: &str,
    member_id: &str,
) -> Result<bool, OnboardingStoreError> {
    let row: Option<(i32,)> = sqlx::query_as(
        "SELECT 1 FROM events WHERE guild_id = $1 AND member_id = $2 AND event_type = $3 LIMIT 1",
    )
    .bind(guild_id)
    .bind(member_id)
    .bind(EVENT_ONBOARDING_PROMPTED)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

/// Record the welcome (legacy `OnboardingRecorder::prompted` /
/// `SessionRecorder::prompted`). Returns false when the row already existed.
pub async fn record_prompted(
    pool: &Pool<Postgres>,
    guild_id: &str,
    member_id: &str,
    channel_id: &str,
    occurred_at: &str,
) -> Result<bool, OnboardingStoreError> {
    Ok(insert_row(
        pool,
        &prompted_row(guild_id, member_id, channel_id, occurred_at),
    )
    .await?)
}

/// Record granted game roles (legacy `OnboardingRecorder::selected`).
pub async fn record_game_selected(
    pool: &Pool<Postgres>,
    guild_id: &str,
    member_id: &str,
    keys: &[String],
    occurred_at: &str,
) -> Result<bool, OnboardingStoreError> {
    Ok(insert_row(
        pool,
        &game_selected_row(guild_id, member_id, keys, occurred_at),
    )
    .await?)
}

/// Record linked destinations (legacy `OnboardingRecorder::routed`).
pub async fn record_channel_routed(
    pool: &Pool<Postgres>,
    guild_id: &str,
    member_id: &str,
    selection: &GameSelection,
    occurred_at: &str,
) -> Result<bool, OnboardingStoreError> {
    Ok(insert_row(
        pool,
        &channel_routed_row(
            guild_id,
            member_id,
            &selection.channel_ids,
            selection.degraded_count,
            occurred_at,
        ),
    )
    .await?)
}

/// Record session routing (legacy `SessionRecorder::routed`).
pub async fn record_session_routed(
    pool: &Pool<Postgres>,
    guild_id: &str,
    member_id: &str,
    plan: &SessionPlan,
    occurred_at: &str,
) -> Result<bool, OnboardingStoreError> {
    Ok(insert_row(
        pool,
        &session_routed_row(guild_id, member_id, plan, occurred_at),
    )
    .await?)
}
