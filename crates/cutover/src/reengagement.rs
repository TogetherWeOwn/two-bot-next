//! On-demand reengagement list (parity §4/§9: on-demand CLI only, never
//! scheduled; §9 drops the scheduled runtime but keeps the on-demand query).
//!
//! Read-only query behind the `reengagement-list` bin: the inactivity
//! selector's quiet members for one guild, rendered row-for-row by the core
//! CSV layer. This module performs no writes (no `member_inactive` events,
//! no `inactive_flagged_at` projection — that is the hourly sweep's job in
//! [`two_bot_core::inactivity_store`]) and registers no timer, service, or
//! job: the bin is invoked by an operator, runs once, and exits. Nothing
//! here messages anybody (parity forbids DMs); the outcome type carries no
//! channel, message, or DM field.

use sqlx::PgPool;
use two_bot_core::{
    funnel::{format_iso_millis, parse_iso_millis},
    inactivity::{flag_inactive, inactivity_cutoff_ms, FlaggedMember, InactivityCandidate},
};

/// Guild-scoped, read-only reengagement list: quiet members past the
/// `days` cutoff, in `member_id` order. The SQL mirrors the sweep's WHERE
/// clause ([`two_bot_core::inactivity_store`]) restricted to one guild, and
/// the rows are passed through the pure [`flag_inactive`] selector so the
/// output matches the selector row-for-row by construction.
pub async fn list_reengagement(
    pool: &PgPool,
    guild: &str,
    now: &str,
    days: u64,
) -> Result<Vec<FlaggedMember>, sqlx::Error> {
    let now_ms = parse_iso_millis(now)
        .ok_or_else(|| sqlx::Error::InvalidArgument("--now must be RFC3339".into()))?;
    let cutoff = format_iso_millis(inactivity_cutoff_ms(now_ms, days));
    /// One list row: guild, member, last-seen, flagged-at, bot flag.
    /// Timestamps decode as `OffsetDateTime` and format in Rust (the
    /// `rsvp_store` `iso_millis` pattern): no `to_char` quoting in SQL.
    type ReengagementRow = (
        String,
        String,
        Option<sqlx::types::time::OffsetDateTime>,
        Option<sqlx::types::time::OffsetDateTime>,
        bool,
    );
    let rows: Vec<ReengagementRow> = sqlx::query_as(
        "SELECT guild_id, member_id, COALESCE(last_active_at, joined_at),
                inactive_flagged_at, is_bot
           FROM members
          WHERE guild_id = $1
            AND left_at IS NULL
            AND NOT is_bot
            AND COALESCE(last_active_at, joined_at) < $2::timestamptz
            AND (inactive_flagged_at IS NULL OR inactive_flagged_at < $2::timestamptz)
          ORDER BY member_id",
    )
    .bind(guild)
    .bind(&cutoff)
    .fetch_all(pool)
    .await?;
    /// `OffsetDateTime` to epoch millis for the shared ISO formatter.
    fn epoch_millis(dt: &sqlx::types::time::OffsetDateTime) -> i64 {
        dt.unix_timestamp() * 1000 + i64::from(dt.millisecond())
    }
    let candidates: Vec<InactivityCandidate> = rows
        .into_iter()
        .map(
            |(guild_id, member_id, last_seen, flagged_at, is_bot)| InactivityCandidate {
                guild_id,
                member_id,
                last_seen_ms: last_seen.as_ref().map(epoch_millis),
                flagged_at_ms: flagged_at.as_ref().map(epoch_millis),
                is_bot,
                has_left: false,
            },
        )
        .collect();
    Ok(flag_inactive(&candidates, now_ms, days).flagged)
}
