//! Analytics family taxonomy: which ingestion family an event belongs to.
//!
//! This module is intentionally free of the `db` feature gate. The family
//! tag travels with every gateway activity touch from handlers through the
//! funnel buffer (`gateway_funnel::ActivityTouch`, `FunnelStore::touch_activity`),
//! and that plumbing must compile for consumers without a Postgres driver
//! (examples, pure domain-logic tests). The database-backed gates that act
//! on the taxonomy — suppression rows, per-guild toggles — stay in
//! [`crate::privacy`], which re-exports everything defined here so existing
//! `crate::privacy::…` paths keep working.
//!
//! | Family   | Switch             | Gated event types                                    |
//! | -------- | ------------------ | ---------------------------------------------------- |
//! | Presence | `presence_enabled` | `presence_update`                                    |
//! | Activity | `activity_enabled` | `first/second/third_message`                        |
//! | Voice    | `voice_enabled`    | `voice_session_start/end`, `first_voice_session`     |
//!
//! Every other event type (joins, leaves, gate clears, onboarding prompts,
//! invite clicks, routing) still persists: those rows serve
//! safety/moderation and onboarding delivery, not analytics.

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
/// comes from this fixed 3-way match, so every database call site passes a
/// static string (sqlx 0.9 audits dynamic SQL).
#[must_use]
pub fn toggle_column(category: AnalyticsCategory) -> &'static str {
    match category {
        AnalyticsCategory::Presence => "presence_enabled",
        AnalyticsCategory::Activity => "activity_enabled",
        AnalyticsCategory::Voice => "voice_enabled",
    }
}

/// The analytics family a `community_facts` row belongs to, if any. Message
/// facts follow the activity switch, voice facts the voice switch;
/// join/rules facts are lifecycle-like and suppression-only (no toggle
/// drops them), mirroring [`category_of_event`] leaving lifecycle types
/// ungated.
#[must_use]
pub fn community_category(event_type: &str) -> Option<AnalyticsCategory> {
    match event_type {
        "message_created" => Some(AnalyticsCategory::Activity),
        "voice_session_started" | "voice_session_ended" => Some(AnalyticsCategory::Voice),
        _ => None,
    }
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
    fn community_facts_map_to_activity_and_voice_only() {
        assert_eq!(
            community_category("message_created"),
            Some(AnalyticsCategory::Activity)
        );
        for event_type in ["voice_session_started", "voice_session_ended"] {
            assert_eq!(
                community_category(event_type),
                Some(AnalyticsCategory::Voice),
                "{event_type}"
            );
        }
        for event_type in ["member_joined", "rules_accepted", "event_attended", ""] {
            assert_eq!(community_category(event_type), None, "{event_type}");
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
