//! Minimized observations at existing automation audit boundaries. These are
//! independent of the feature ledger write and never contain its free text.

use serde_json::json;
use sha2::{Digest, Sha256};
use two_bot_core::audit::{AuditEvent, AuditKind};

#[derive(Clone, Copy)]
pub(crate) enum Family {
    Scheduled,
    Feed,
    Sticky,
}

impl Family {
    fn token(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled_run",
            Self::Feed => "feed_poll",
            Self::Sticky => "sticky_run",
        }
    }
}

fn snowflake(value: &str) -> Option<String> {
    value
        .parse::<u64>()
        .ok()
        .filter(|id| *id != 0 && id.to_string() == value)?;
    Some(value.to_owned())
}

fn outcome_token(outcome: &str) -> &'static str {
    match outcome {
        "ok" => "ok",
        "retry_scheduled" => "retry_scheduled",
        "delivery_uncertain" => "delivery_uncertain",
        "post_failed" => "post_failed",
        "stale_completion" => "stale_completion",
        "persistence_failed_cleaned" => "persistence_failed_cleaned",
        "persistence_failed_uncertain" => "persistence_failed_uncertain",
        "item_failed" => "item_failed",
        "poll_failed" => "poll_failed",
        "poll_result" => "poll_result",
        "recovery_required" => "recovery_required",
        _ => "unclassified",
    }
}

pub(crate) fn event(
    family: Family,
    guild: &str,
    channel: Option<&str>,
    entity_key: &str,
    outcome: &str,
) -> Option<AuditEvent> {
    let guild = snowflake(guild)?;
    // Entity keys are opaque schedule/feed/channel IDs, not member content.
    // Bound hashing work and never retain the input or a feature's reason.
    if entity_key.len() > 128 {
        return None;
    }
    let mut row = AuditEvent::new(
        format!("automation:{:032x}", rand::random::<u128>()),
        AuditKind::AutomationAction,
        guild,
        two_bot_core::now_iso(),
    );
    row.action = Some("automation".to_owned());
    row.source_channel_id = channel.and_then(snowflake);
    row.target_id = row.source_channel_id.clone();
    row.metadata_json = json!({
        "family": family.token(), "outcome": outcome_token(outcome),
        "entityFingerprint": format!("{:x}", Sha256::digest(entity_key.as_bytes())),
        "correlationId": row.entry_id,
        "actorType": "bot", "actorConfidence": "unknown",
        "source": "feature_audit_boundary",
        "semantics": "observed_feature_outcome_not_feature_audit_persistence",
    })
    .to_string();
    Some(row)
}

pub(crate) fn record(
    family: Family,
    guild: &str,
    channel: Option<&str>,
    entity_key: &str,
    outcome: &str,
) {
    if let Some(event) = event(family, guild, channel, entity_key, outcome) {
        crate::audit_capture::submit(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn outcomes_keep_failure_and_uncertainty_without_free_text_or_verified_actor() {
        for outcome in [
            "ok",
            "retry_scheduled",
            "delivery_uncertain",
            "post_failed",
            "stale_completion",
            "persistence_failed_cleaned",
            "persistence_failed_uncertain",
            "item_failed",
            "poll_failed",
            "poll_result",
            "recovery_required",
        ] {
            for family in [Family::Scheduled, Family::Feed, Family::Sticky] {
                let row = event(family, "1", Some("2"), "opaque-entity", outcome).unwrap();
                assert_eq!(row.kind, AuditKind::AutomationAction);
                assert_eq!(row.guild_id, "1");
                assert_eq!(row.source_channel_id.as_deref(), Some("2"));
                assert_eq!(row.target_id, row.source_channel_id);
                assert_eq!(row.action.as_deref(), Some("automation"));
                assert_eq!(row.actor_id, None);
                let metadata: Value = serde_json::from_str(&row.metadata_json).unwrap();
                assert_eq!(metadata["outcome"], outcome);
                assert_eq!(metadata["family"], family.token());
                assert_eq!(metadata["actorConfidence"], "unknown");
                assert_eq!(metadata["correlationId"], row.entry_id);
                assert_eq!(metadata["entityFingerprint"].as_str().unwrap().len(), 64);
                assert!(!row.metadata_json.contains("opaque-entity"));
            }
        }
    }

    #[test]
    fn malformed_ids_and_future_outcomes_cannot_smuggle_content() {
        for guild in ["0", "01", " 1", "-1", "private-body"] {
            assert!(event(Family::Feed, guild, None, "id", "ok").is_none());
        }
        assert!(event(Family::Feed, "1", None, &"x".repeat(129), "ok").is_none());
        let a = event(
            Family::Scheduled,
            "1",
            Some("private-channel"),
            "id",
            "SECRET URL",
        )
        .unwrap();
        let b = event(Family::Scheduled, "1", None, "id", "ok").unwrap();
        let a_meta: Value = serde_json::from_str(&a.metadata_json).unwrap();
        let b_meta: Value = serde_json::from_str(&b.metadata_json).unwrap();
        assert_eq!(a_meta["outcome"], "unclassified");
        assert_eq!(a.source_channel_id, None);
        assert_eq!(a_meta["entityFingerprint"], b_meta["entityFingerprint"]);
        assert_ne!(
            a.entry_id, b.entry_id,
            "distinct observations survive rapid chains"
        );
        assert!(!a.metadata_json.contains("SECRET URL"));
    }
}
