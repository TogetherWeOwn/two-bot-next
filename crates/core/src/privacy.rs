//! Guild-scoped privacy gate: per-user opt-out suppression plus per-guild
//! analytics ingestion toggles.
//!
//! Suppression is keyed on the `(guild_id, member_id)` pair and is never
//! global: a suppressed member stops per-user persistence in the opting guild
//! only, while other members' rows and guild aggregates keep counting.
//! Removing the suppression row (opt-back-in) restores persistence.
//!
//! The toggles disable ingestion per analytics family, per guild. A missing
//! `guild_analytics_settings` row means every category is enabled, which
//! preserves the behavior from before the toggles existed:
//!
//! | Family   | Switch             | Gated event types                                    |
//! | -------- | ------------------ | ---------------------------------------------------- |
//! | Presence | `presence_enabled` | `presence_update`                                    |
//! | Activity | `activity_enabled` | `first/second/third_message`                        |
//! | Voice    | `voice_enabled`    | `voice_session_start/end`, `first_voice_session`     |
//!
//! Every other event type (joins, leaves, gate clears, onboarding prompts,
//! invite clicks, routing) still persists: those rows serve
//! safety/moderation and onboarding delivery, not analytics. Membership
//! lifecycle rows in particular stay unconditional so a disabled analytics
//! category can never blind moderation to a join or a leave.
//!
//! Both checks fail closed: a database error propagates to the caller, and
//! the sync store seams surface it as a panic rather than silently writing
//! per-user rows for a member who opted out.

use sqlx::{PgConnection, Pool, Postgres};

/// One analytics ingestion family behind a per-guild toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalyticsCategory {
    Presence,
    Activity,
    Voice,
}

/// The ingestion family an event type belongs to, if any. Unknown strings
/// map to no family and always persist (see the module docs for why only
/// these families are gated).
#[must_use]
pub fn category_of_event(event_type: &str) -> Option<AnalyticsCategory> {
    match event_type {
        "presence_update" => Some(AnalyticsCategory::Presence),
        "first_message" | "second_message" | "third_message" => Some(AnalyticsCategory::Activity),
        "voice_session_start" | "voice_session_end" | "first_voice_session" => {
            Some(AnalyticsCategory::Voice)
        }
        _ => None,
    }
}

/// The `guild_analytics_settings` column guarding a family. The column name
/// comes from this fixed 3-way match, so every call site below is a static
/// string (sqlx 0.9 audits dynamic SQL).
#[must_use]
pub fn toggle_column(category: AnalyticsCategory) -> &'static str {
    match category {
        AnalyticsCategory::Presence => "presence_enabled",
        AnalyticsCategory::Activity => "activity_enabled",
        AnalyticsCategory::Voice => "voice_enabled",
    }
}

/// True when the pair holds a guild-scoped suppression row. Anonymous writes
/// (`member_id = None`) are never suppressed: there is no member to match.
pub async fn suppressed_in(
    connection: &mut PgConnection,
    guild_id: &str,
    member_id: &str,
) -> Result<bool, sqlx::Error> {
    let hit: Option<(i32,)> =
        sqlx::query_as("SELECT 1 FROM privacy_opt_outs WHERE guild_id = $1 AND member_id = $2")
            .bind(guild_id)
            .bind(member_id)
            .fetch_optional(&mut *connection)
            .await?;
    Ok(hit.is_some())
}

/// [`suppressed_in`] over a pool for call sites with no open transaction.
pub async fn suppressed(
    pool: &Pool<Postgres>,
    guild_id: &str,
    member_id: &str,
) -> Result<bool, sqlx::Error> {
    suppressed_in(&mut *pool.acquire().await?, guild_id, member_id).await
}

/// True when the event's family is disabled for the guild. Events outside
/// every family are never disabled. A missing settings row means enabled.
pub async fn category_disabled_in(
    connection: &mut PgConnection,
    guild_id: &str,
    event_type: &str,
) -> Result<bool, sqlx::Error> {
    let Some(category) = category_of_event(event_type) else {
        return Ok(false);
    };
    let sql = format!(
        "SELECT {} FROM guild_analytics_settings WHERE guild_id = $1",
        toggle_column(category)
    );
    let enabled: Option<(bool,)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(guild_id)
        .fetch_optional(&mut *connection)
        .await?;
    Ok(enabled.is_some_and(|(flag,)| !flag))
}

/// [`category_disabled_in`] over a pool for call sites with no open transaction.
pub async fn category_disabled(
    pool: &Pool<Postgres>,
    guild_id: &str,
    event_type: &str,
) -> Result<bool, sqlx::Error> {
    category_disabled_in(&mut *pool.acquire().await?, guild_id, event_type).await
}

/// True when a per-user write for this pair must be dropped: the member is
/// suppressed, or the event's analytics family is disabled for the guild.
/// `None` members (anonymous counters) are never blocked.
pub async fn ingestion_blocked_in(
    connection: &mut PgConnection,
    guild_id: &str,
    member_id: Option<&str>,
    event_type: &str,
) -> Result<bool, sqlx::Error> {
    let Some(member_id) = member_id else {
        return Ok(false);
    };
    if suppressed_in(connection, guild_id, member_id).await? {
        return Ok(true);
    }
    category_disabled_in(connection, guild_id, event_type).await
}

/// [`ingestion_blocked_in`] over a pool for call sites with no open transaction.
pub async fn ingestion_blocked(
    pool: &Pool<Postgres>,
    guild_id: &str,
    member_id: Option<&str>,
    event_type: &str,
) -> Result<bool, sqlx::Error> {
    ingestion_blocked_in(&mut *pool.acquire().await?, guild_id, member_id, event_type).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn representatives_map_to_their_families() {
        assert_eq!(
            category_of_event("presence_update"),
            Some(AnalyticsCategory::Presence)
        );
        for event_type in ["first_message", "second_message", "third_message"] {
            assert_eq!(
                category_of_event(event_type),
                Some(AnalyticsCategory::Activity),
                "{event_type}"
            );
        }
        for event_type in [
            "voice_session_start",
            "voice_session_end",
            "first_voice_session",
        ] {
            assert_eq!(
                category_of_event(event_type),
                Some(AnalyticsCategory::Voice),
                "{event_type}"
            );
        }
    }

    #[test]
    fn lifecycle_and_unknown_events_belong_to_no_family() {
        for event_type in [
            "member_join",
            "member_leave",
            "gate_cleared",
            "onboarding_prompted",
            "invite_click",
            "channel_routed",
            "game_roles_selected",
            "presence_probe",
            "",
        ] {
            assert_eq!(category_of_event(event_type), None, "{event_type}");
        }
    }

    #[test]
    fn toggle_columns_are_static_settings_names() {
        assert_eq!(
            toggle_column(AnalyticsCategory::Presence),
            "presence_enabled"
        );
        assert_eq!(
            toggle_column(AnalyticsCategory::Activity),
            "activity_enabled"
        );
        assert_eq!(toggle_column(AnalyticsCategory::Voice), "voice_enabled");
    }
}
