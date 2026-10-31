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

pub use crate::analytics_family::{
    category_of_event, community_category, toggle_column, AnalyticsCategory,
};

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
    category_off_in(connection, guild_id, category).await
}

/// True when an analytics family is disabled for the guild, addressed by
/// family rather than by event-type string. Community-facts writers map
/// their own fact types through [`community_category`] and gate here, since
/// their wire names differ from the funnel vocabulary.
pub async fn category_off_in(
    connection: &mut PgConnection,
    guild_id: &str,
    category: AnalyticsCategory,
) -> Result<bool, sqlx::Error> {
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

/// Extract the `inviterId` attribution from a `member_join` metadata blob.
/// String and numeric encodings both count (the funnel writes strings;
/// hand-written rows may carry numbers). `None` when the blob is absent,
/// unparsable, or carries no inviter.
#[must_use]
pub fn inviter_in_metadata(metadata: Option<&str>) -> Option<String> {
    let text = metadata?;
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let inviter = value.get("inviterId")?;
    if let Some(id) = inviter.as_str() {
        return Some(id.to_owned());
    }
    inviter.as_u64().map(|id| id.to_string())
}

/// Remove a suppressed inviter's attribution from a `member_join` metadata
/// blob, keeping every other key. A blob holding only the inviter becomes
/// `None` (no attribution), matching the blanked `inviter_id` in
/// invite snapshots. Blobs without that inviter return unchanged.
#[must_use]
pub fn scrubbed_member_join_metadata(metadata: Option<&str>, inviter: &str) -> Option<String> {
    let text = metadata?;
    let mut value: serde_json::Value = serde_json::from_str(text).ok()?;
    let hit = value.get("inviterId").is_some_and(|current| {
        current.as_str() == Some(inviter)
            || current.as_u64().is_some_and(|id| id.to_string() == inviter)
    });
    if !hit {
        return Some(text.to_owned());
    }
    value.as_object_mut()?.remove("inviterId");
    if value.as_object().is_some_and(|obj| obj.is_empty()) {
        None
    } else {
        Some(value.to_string())
    }
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
    fn inviter_attribution_extracts_and_scrubs() {
        assert_eq!(
            inviter_in_metadata(Some(r#"{"inviterId":"123"}"#)),
            Some("123".to_owned())
        );
        assert_eq!(
            inviter_in_metadata(Some(r#"{"inviterId":123}"#)),
            Some("123".to_owned())
        );
        assert_eq!(inviter_in_metadata(Some("{}")), None);
        assert_eq!(inviter_in_metadata(None), None);
        assert_eq!(inviter_in_metadata(Some("not json")), None);
        // A blob holding only the suppressed inviter becomes unattributed.
        assert_eq!(
            scrubbed_member_join_metadata(Some(r#"{"inviterId":"123"}"#), "123"),
            None
        );
        // Other keys survive the scrub.
        let scrubbed =
            scrubbed_member_join_metadata(Some(r#"{"inviterId":"123","source":"x"}"#), "123")
                .expect("kept keys");
        assert!(!scrubbed.contains("inviterId"), "{scrubbed}");
        assert!(scrubbed.contains("\"source\":\"x\""), "{scrubbed}");
        // A different inviter leaves the blob byte-identical.
        let untouched = r#"{"inviterId":"999"}"#;
        assert_eq!(
            scrubbed_member_join_metadata(Some(untouched), "123"),
            Some(untouched.to_owned())
        );
    }
}
