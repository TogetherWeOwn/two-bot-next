//! One correlated diagnostic per rename decision/attempt/outcome. Channel
//! names belong only in the existing private mirror, never tracing output.

use serde_json::{json, Value};
use two_bot_core::audit::{AuditEvent, AuditKind};
use two_bot_discord::voice_rooms::RoomHttpError;

pub(crate) struct RenameSnapshot<'a> {
    pub guild_id: u64,
    pub channel_id: u64,
    pub bot_id: Option<u64>,
    pub old_name: Option<&'a str>,
    pub new_name: &'a str,
    pub human_count: usize,
    pub ready: bool,
    pub generation: u64,
    pub reason: &'static str,
}

pub(crate) fn snapshot(input: RenameSnapshot<'_>) -> AuditEvent {
    let mut event = AuditEvent::new(
        format!(
            "voice-rename:{}:{}:{:032x}",
            input.guild_id,
            input.channel_id,
            rand::random::<u128>()
        ),
        AuditKind::AutomationAction,
        input.guild_id.to_string(),
        two_bot_core::now_iso(),
    );
    event.actor_id = input.bot_id.map(|id| id.to_string());
    event.target_id = Some(input.channel_id.to_string());
    event.source_channel_id = Some(input.channel_id.to_string());
    event.action = Some("voice_room_rename".to_owned());
    event.metadata_json = json!({
        "oldName": input.old_name.map(crate::audit_changes::channel_name),
        "newName": crate::audit_changes::channel_name(input.new_name),
        "humanCount": input.human_count,
        "occupancyEvidence": if input.ready { "live_voice_snapshot" } else { "unready_snapshot_not_proof_of_empty" },
        "generation": input.generation, "botReason": input.reason,
        "actorConfidence": if input.bot_id.is_some() { "verified" } else { "unknown" },
        "actorType": "bot", "source": "voice_worker",
        "correlationId": event.entry_id,
    }).to_string();
    event
}

fn with_decision(base: &AuditEvent, decision: &str, retry_after_ms: u64) -> AuditEvent {
    let mut event = base.clone();
    event.entry_id = format!("{}:{decision}", base.entry_id);
    event.occurred_at = two_bot_core::now_iso();
    let mut metadata: Value =
        serde_json::from_str(&base.metadata_json).unwrap_or_else(|_| json!({}));
    metadata["decision"] = json!(decision);
    metadata["retryAfterMs"] = json!(retry_after_ms);
    event.metadata_json = metadata.to_string();
    event
}

pub(crate) fn decision(base: &AuditEvent, decision: &str, retry_after_ms: u64) {
    crate::audit_capture::submit(with_decision(base, decision, retry_after_ms));
}

fn result_class(result: &Result<(), RoomHttpError>) -> (&'static str, u64) {
    match result {
        Ok(()) => ("accepted", 0),
        Err(RoomHttpError::RateLimited { retry_after_ms, .. }) => ("rate_limited", *retry_after_ms),
        Err(RoomHttpError::AccessDenied) => ("permission_denied", 0),
        Err(RoomHttpError::NotFound) => ("not_found", 0),
        Err(RoomHttpError::Unauthorized) => ("credential_refused", 0),
        Err(RoomHttpError::Cancelled) => ("cancelled", 0),
        Err(RoomHttpError::RenameDeferred) => ("outcome_deferred", 0),
        Err(RoomHttpError::UnknownOutcome) => ("outcome_unknown", 0),
        Err(RoomHttpError::InvalidRequest | RoomHttpError::Rejected { .. }) => ("rejected", 0),
    }
}

pub(crate) fn outcome(base: &AuditEvent, result: &Result<(), RoomHttpError>) {
    let (class, retry) = result_class(result);
    decision(base, class, retry);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempt_and_failure_share_names_occupancy_and_correlation_but_not_identity() {
        let base = snapshot(RenameSnapshot {
            guild_id: 1,
            channel_id: 2,
            bot_id: Some(3),
            old_name: Some("Occupied"),
            new_name: "Open seat",
            human_count: 2,
            ready: true,
            generation: 7,
            reason: "template_reconcile",
        });
        let attempted = with_decision(&base, "attempted", 0);
        let (class, retry) = result_class(&Err(RoomHttpError::RateLimited {
            retry_after_ms: 3000,
            global: false,
        }));
        let failed = with_decision(&base, class, retry);
        assert_ne!(attempted.entry_id, failed.entry_id);
        let a: Value = serde_json::from_str(&attempted.metadata_json).expect("metadata");
        let b: Value = serde_json::from_str(&failed.metadata_json).expect("metadata");
        for key in [
            "oldName",
            "newName",
            "correlationId",
            "humanCount",
            "generation",
            "botReason",
        ] {
            assert_eq!(a[key], b[key]);
        }
        assert_eq!(b["oldName"], "Occupied");
        assert_eq!(b["humanCount"], 2);
        assert_eq!(b["retryAfterMs"], 3000);
        assert_eq!(b["actorConfidence"], "verified");
        assert_eq!(failed.actor_id.as_deref(), Some("3"));
        assert_ne!(
            base.entry_id,
            snapshot(RenameSnapshot {
                guild_id: 1,
                channel_id: 2,
                bot_id: None,
                old_name: None,
                new_name: "Open seat",
                human_count: 0,
                ready: false,
                generation: 8,
                reason: "template_reconcile"
            })
            .entry_id
        );
    }

    #[test]
    fn unknown_outcomes_never_become_success_and_errors_are_fixed_tokens() {
        for (result, expected) in [
            (Ok(()), "accepted"),
            (Err(RoomHttpError::AccessDenied), "permission_denied"),
            (Err(RoomHttpError::NotFound), "not_found"),
            (Err(RoomHttpError::Unauthorized), "credential_refused"),
            (Err(RoomHttpError::Cancelled), "cancelled"),
            (Err(RoomHttpError::RenameDeferred), "outcome_deferred"),
            (Err(RoomHttpError::UnknownOutcome), "outcome_unknown"),
            (Err(RoomHttpError::InvalidRequest), "rejected"),
            (
                Err(RoomHttpError::Rejected {
                    status: 400,
                    code: 50035,
                }),
                "rejected",
            ),
        ] {
            assert_eq!(result_class(&result), (expected, 0));
        }
    }
}
