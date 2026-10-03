//! RSVP and scorecard check-in execution over the shared router and REST executor.

use sqlx::{Pool, Postgres};
use twilight_model::{
    application::interaction::{
        application_command::CommandOptionValue, Interaction, InteractionData,
    },
    channel::message::MessageFlags,
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
};
use two_bot_core::{
    attendance_totals_text, checkin_duplicate_text, checkin_recorded_text, classify, list_rsvps,
    now_iso, partition_rsvps, put_rsvp, record_checkin, require_manage_events, rsvp_saved_text,
    validate_event_id, validate_occurrence_id, write_audit, AttendanceProof, CheckinWrite,
    ClassifierConfig, ClassifyInput, HandlerId, InteractionRouter, RsvpAudit, RsvpRecord,
    RsvpStatus, SlashOutcome,
};

use crate::{
    executor::{ActionExecutor, DiscordError},
    interactions::{response_for_slash, route_interaction, RoutedInteraction},
};

/// Acknowledgement is separate from ordered effects: the gateway prepares
/// queued commands immediately, then completes them in dispatch order. The
/// private deferred fields can only be constructed after a successful callback.
///
/// The held [`Interaction`] embeds its callback token, so [`PreparedRsvp`]
/// never derives [`std::fmt::Debug`]: diagnostics name the owning handler and
/// whether a deferred completion is pending, never the credential.
pub struct PreparedRsvp {
    handled: bool,
    deferred: Option<(HandlerId, Interaction)>,
}

impl std::fmt::Debug for PreparedRsvp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedRsvp")
            .field("handled", &self.handled)
            .field(
                "deferred",
                &self.deferred.as_ref().map(|(handler, _)| handler),
            )
            .finish()
    }
}

impl PreparedRsvp {
    /// Fenced-out interaction: no callback, no store work, no completion.
    pub(crate) fn ignored() -> Self {
        Self {
            handled: false,
            deferred: None,
        }
    }
}

/// Slash commands owned by the RSVP path. A refusal for any other command must
/// be answered by the shared routed path: this path ignores those names, so
/// leaving the refusal here would drop the denial silently.
#[must_use]
pub fn is_rsvp_command(name: &str) -> bool {
    matches!(name, "rsvp" | "rsvp-attendance" | "attendance")
}

pub async fn prepare_rsvp_interaction(
    router: &InteractionRouter,
    executor: &ActionExecutor,
    interaction: Interaction,
) -> Result<PreparedRsvp, DiscordError> {
    let RoutedInteraction::Slash { name, outcome } = route_interaction(router, &interaction, None)
    else {
        return Ok(PreparedRsvp::ignored());
    };
    if !is_rsvp_command(name.as_str()) {
        return Ok(PreparedRsvp::ignored());
    }
    if let Some(response) = response_for_slash(&outcome) {
        // Receipt callbacks wait out brief governed-lane occupancy instead of
        // dropping the acknowledgement; exhaustion still fails the command.
        executor
            .answer_interaction_with_blocked_retry(
                interaction.id.get(),
                &interaction.token,
                &response,
            )
            .await?;
        return Ok(PreparedRsvp {
            handled: true,
            deferred: None,
        });
    }
    let SlashOutcome::Handled { handler } = outcome else {
        return Ok(PreparedRsvp::ignored());
    };
    if !matches!(
        handler,
        HandlerId::Rsvp | HandlerId::RsvpAttendance | HandlerId::ScorecardAttendance
    ) {
        return Ok(PreparedRsvp::ignored());
    }
    executor
        .answer_interaction_with_blocked_retry(
            interaction.id.get(),
            &interaction.token,
            &InteractionResponse {
                kind: InteractionResponseType::DeferredChannelMessageWithSource,
                data: Some(InteractionResponseData {
                    flags: Some(MessageFlags::EPHEMERAL),
                    ..Default::default()
                }),
            },
        )
        .await?;
    Ok(PreparedRsvp {
        handled: true,
        deferred: Some((handler, interaction)),
    })
}

pub async fn complete_rsvp_interaction(
    prepared: PreparedRsvp,
    pool: &Pool<Postgres>,
    executor: &ActionExecutor,
    classifier: &ClassifierConfig,
) -> Result<bool, DiscordError> {
    let Some((handler, interaction)) = prepared.deferred else {
        return Ok(prepared.handled);
    };
    let content = execute(handler, pool, executor, classifier, &interaction)
        .await
        .unwrap_or_else(|message| message);
    executor
        .edit_interaction_response(
            interaction.application_id.get(),
            &interaction.token,
            &content,
        )
        .await?;
    Ok(true)
}

/// Returns false for another feature; authorization and acknowledgement must
/// succeed before any store or paced Discord read.
pub async fn handle_rsvp_interaction(
    router: &InteractionRouter,
    pool: &Pool<Postgres>,
    executor: &ActionExecutor,
    classifier: &ClassifierConfig,
    interaction: &Interaction,
) -> Result<bool, DiscordError> {
    let prepared = prepare_rsvp_interaction(router, executor, interaction.clone()).await?;
    complete_rsvp_interaction(prepared, pool, executor, classifier).await
}

async fn execute(
    handler: HandlerId,
    pool: &Pool<Postgres>,
    executor: &ActionExecutor,
    classifier: &ClassifierConfig,
    interaction: &Interaction,
) -> Result<String, String> {
    let Some(InteractionData::ApplicationCommand(data)) = interaction.data.as_ref() else {
        return Err("Invalid command payload.".to_owned());
    };
    let guild_id = interaction
        .guild_id
        .ok_or("This command requires a guild.")?
        .to_string();
    let string_option = |name: &str| -> Result<&str, String> {
        data.options
            .iter()
            .find_map(|option| {
                if option.name == name {
                    if let CommandOptionValue::String(value) = &option.value {
                        return Some(value.as_str());
                    }
                }
                None
            })
            .ok_or_else(|| format!("Missing or invalid {name} option."))
    };
    match handler {
        HandlerId::Rsvp | HandlerId::RsvpAttendance => {
            let event_id =
                validate_event_id(string_option("event-id")?).map_err(|e| e.to_string())?;
            // Legacy totals read persisted responses even after cancellation or
            // deletion; only an RSVP mutation requires live-event validation.
            if handler == HandlerId::RsvpAttendance {
                let rows = list_rsvps(pool, &guild_id, &event_id)
                    .await
                    .map_err(|_| "Unable to read RSVP totals.")?;
                return Ok(attendance_totals_text(&partition_rsvps(&rows)));
            }
            let status = RsvpStatus::parse(string_option("status")?).map_err(|e| e.to_string())?;
            let event = executor
                .get_scheduled_event(&guild_id, &event_id)
                .await?
                .ok_or("No scheduled event with that id exists in this server.")?;
            // A malformed or mismatched response is not evidence of a live event.
            if event["id"].as_str() != Some(&event_id)
                || event["guild_id"].as_str() != Some(&guild_id)
            {
                return Err("Discord returned an invalid scheduled event status.".to_owned());
            }
            match event["status"].as_u64() {
                Some(4) => return Err("That scheduled event is cancelled.".to_owned()),
                Some(1..=3) => {}
                _ => return Err("Discord returned an invalid scheduled event status.".to_owned()),
            }
            let record = RsvpRecord {
                guild_id,
                event_id,
                user_id: interaction
                    .author_id()
                    .ok_or("Missing command member.")?
                    .to_string(),
                status,
                responded_at: now_iso(),
            };
            put_rsvp(pool, &record)
                .await
                .map_err(|_| "Unable to save RSVP.")?;
            write_audit(
                pool,
                &RsvpAudit::for_rsvp(&format!("rsvp:{}", interaction.id), &record),
            )
            .await
            .map_err(|_| "Unable to audit RSVP.")?;
            Ok(rsvp_saved_text(status))
        }
        HandlerId::ScorecardAttendance => {
            require_manage_events(
                interaction
                    .member
                    .as_ref()
                    .and_then(|m| m.permissions)
                    .map_or(0, |p| p.bits()),
            )
            .map_err(|e| e.to_string())?;
            let occurrence = validate_occurrence_id(string_option("event-occurrence")?)
                .map_err(|e| e.to_string())?;
            let member_id = data
                .options
                .iter()
                .find_map(|option| {
                    if option.name == "member" {
                        if let CommandOptionValue::User(id) = option.value {
                            return Some(id);
                        }
                    }
                    None
                })
                .ok_or("Missing or invalid member option.")?;
            // Discord resolves the selected User; fail closed rather than
            // silently counting an unresolved bot as an eligible human.
            let member = data
                .resolved
                .as_ref()
                .and_then(|r| r.users.get(&member_id))
                .ok_or("Unable to resolve attendance member.")?;
            let verdict = classify(
                classifier,
                &ClassifyInput {
                    guild_id: guild_id.clone(),
                    actor_id: member_id.to_string(),
                    is_bot: member.bot,
                    webhook_id: None,
                    is_staff_automation: false,
                    is_raid: false,
                    is_staging: false,
                    is_test: false,
                },
            );
            let inserted = record_checkin(
                pool,
                &CheckinWrite {
                    guild_id,
                    event_occurrence_id: occurrence.clone(),
                    member_id: member_id.to_string(),
                    occurred_at: now_iso(),
                    proof: AttendanceProof::HostCheckin,
                    classifier_version: verdict.classifier_version,
                    classification: verdict.classification,
                    matched_rule: verdict.matched_rule,
                },
            )
            .await
            .map_err(|_| "Attendance was not recorded.")?;
            Ok(if inserted {
                checkin_recorded_text(&member_id.to_string(), &occurrence)
            } else {
                checkin_duplicate_text(&member_id.to_string(), &occurrence)
            })
        }
        _ => unreachable!("only RSVP handlers execute here"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_debug_names_the_handler_never_the_callback_token() {
        let interaction: Interaction = serde_json::from_value(serde_json::json!({
            "application_id": "1111", "authorizing_integration_owners": {"0": "2222"},
            "id": "100", "token": "synthetic-secret-token", "type": 2,
            "version": 1, "guild_id": "2222",
            "member": {"permissions": "0", "roles": [], "deaf": false, "mute": false,
                "flags": 0, "user": {"id": "77", "username": "human", "discriminator": "0"}},
            "data": {"id": "4444", "name": "rsvp", "type": 1, "options": []}
        }))
        .expect("wire interaction");
        for prepared in [
            PreparedRsvp::ignored(),
            PreparedRsvp {
                handled: true,
                deferred: Some((HandlerId::Rsvp, interaction)),
            },
        ] {
            let shown = format!("{prepared:?}");
            assert!(!shown.contains("synthetic-secret-token"), "{shown}");
            assert!(shown.contains("PreparedRsvp"), "{shown}");
        }
    }
}
