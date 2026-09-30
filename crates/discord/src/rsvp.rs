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
#[derive(Debug)]
pub struct PreparedRsvp {
    handled: bool,
    deferred: Option<(HandlerId, Interaction)>,
}

pub async fn prepare_rsvp_interaction(
    router: &InteractionRouter,
    executor: &ActionExecutor,
    interaction: Interaction,
) -> Result<PreparedRsvp, DiscordError> {
    let ignored = || PreparedRsvp {
        handled: false,
        deferred: None,
    };
    let RoutedInteraction::Slash { name, outcome } = route_interaction(router, &interaction, None)
    else {
        return Ok(ignored());
    };
    if !matches!(name.as_str(), "rsvp" | "rsvp-attendance" | "attendance") {
        return Ok(ignored());
    }
    if let Some(response) = response_for_slash(&outcome) {
        executor
            .answer_interaction(interaction.id.get(), &interaction.token, &response)
            .await?;
        return Ok(PreparedRsvp {
            handled: true,
            deferred: None,
        });
    }
    let SlashOutcome::Handled { handler } = outcome else {
        return Ok(ignored());
    };
    if !matches!(
        handler,
        HandlerId::Rsvp | HandlerId::RsvpAttendance | HandlerId::ScorecardAttendance
    ) {
        return Ok(ignored());
    }
    executor
        .answer_interaction(
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
