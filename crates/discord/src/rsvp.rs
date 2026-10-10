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
    attendance_totals_text, checkin_duplicate_text, checkin_idempotency_key,
    checkin_occurrence_full_text, checkin_recorded_text, classify, compensate_checkin_write,
    compensate_rsvp_write, list_rsvps, now_iso, parse_attendance_occurrence, partition_rsvps,
    put_rsvp, record_checkin, require_manage_events, rsvp_event_full_text, rsvp_rate_limited_text,
    rsvp_saved_text, validate_event_id, validate_occurrence_id, write_audit, AttendanceProof,
    CheckinWrite, ClassifierConfig, ClassifyInput, HandlerId, InteractionRouter, RsvpAudit,
    RsvpRecord, RsvpStatus, RsvpStoreError, SlashOutcome,
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
        .edit_interaction_response_with_blocked_retry(
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

/// RA-01 live-membership gate: the acting (and, for host check-in, target)
/// member must currently belong to the interaction's guild. Only 404 means
/// absent; transport, authorization, rate-limit and malformed evidence fail
/// closed with no mutation or audit write. The `user.id` echo check keeps a
/// proxy/cache returning another member from passing the gate.
enum Membership {
    Current,
    Absent,
    Unavailable,
}

async fn guild_membership(executor: &ActionExecutor, guild_id: &str, user_id: &str) -> Membership {
    // Mutation-free evidence: a pre-wire admission-Blocked attempt waits out
    // brief governed-lane occupancy (mixed RSVP/custom contention) instead of
    // refusing. Every other outcome keeps the RA-01 fail-closed mapping.
    match executor
        .get_json_strict_with_blocked_retry(&format!("/guilds/{guild_id}/members/{user_id}"))
        .await
    {
        Ok(Some(member))
            if member
                .get("user")
                .and_then(|user| user.get("id"))
                .and_then(|id| id.as_str())
                == Some(user_id) =>
        {
            Membership::Current
        }
        Ok(Some(_)) => Membership::Unavailable,
        Ok(None) => Membership::Absent,
        Err(_) => Membership::Unavailable,
    }
}

/// RA-03 consistency contract (TOG-19773): every mutation path observes live
/// evidence immediately before its write and re-reads the same evidence
/// immediately after. Any loss, removal, or lookup/store failure refuses the
/// attempt with zero rows from that attempt, never a success reply. Lookup
/// errors fail closed before any write; store errors fail closed without a
/// success reply; a loss racing the write is compensated (the attempt's
/// exact rows are deleted) and refused. The check-in fence re-reads the
/// RA-02 anchor event plus the target membership. Historical totals reads
/// (`list_rsvps`) perform no lookup and no write.
async fn check_live_event(
    executor: &ActionExecutor,
    guild_id: &str,
    event_id: &str,
) -> Result<(), String> {
    let event = executor
        .get_scheduled_event(guild_id, event_id)
        .await?
        .ok_or("No scheduled event with that id exists in this server.")?;
    // A malformed or mismatched response is not evidence of a live event.
    if event["id"].as_str() != Some(event_id) || event["guild_id"].as_str() != Some(guild_id) {
        return Err("Discord returned an invalid scheduled event status.".to_owned());
    }
    match event["status"].as_u64() {
        Some(4) => Err("That scheduled event is cancelled.".to_owned()),
        Some(1..=3) => Ok(()),
        _ => Err("Discord returned an invalid scheduled event status.".to_owned()),
    }
}

/// RA-03 post-write fence for RSVP: the actor must still belong to the
/// guild and the event must still be live after the commit.
async fn revalidate_rsvp(
    executor: &ActionExecutor,
    guild_id: &str,
    event_id: &str,
    actor_id: &str,
) -> Result<(), String> {
    match guild_membership(executor, guild_id, actor_id).await {
        Membership::Current => {}
        Membership::Absent => {
            return Err("You are no longer a member of this server.".to_owned());
        }
        Membership::Unavailable => {
            return Err("Unable to verify server membership.".to_owned());
        }
    }
    check_live_event(executor, guild_id, event_id).await
}

/// RA-03 post-write fence for host check-in: the RA-02 anchor event must
/// still be live and the target must still belong to the guild after the
/// commit. The acting host needs no membership re-read: authority on this
/// path is Manage Events, checked pre-write, not guild belonging.
async fn revalidate_attendance(
    executor: &ActionExecutor,
    guild_id: &str,
    anchor_event_id: &str,
    target_id: &str,
) -> Result<(), String> {
    check_live_event(executor, guild_id, anchor_event_id).await?;
    match guild_membership(executor, guild_id, target_id).await {
        Membership::Current => Ok(()),
        Membership::Absent => Err("That member is no longer in this server.".to_owned()),
        Membership::Unavailable => Err("Unable to verify attendance membership.".to_owned()),
    }
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
            let actor_id = interaction
                .author_id()
                .ok_or("Missing command member.")?
                .to_string();
            // Membership precedes the event lookup so a departed member or a
            // failed lookup learns nothing about the event and writes nothing.
            match guild_membership(executor, &guild_id, &actor_id).await {
                Membership::Current => {}
                Membership::Absent => {
                    return Err("You are no longer a member of this server.".to_owned());
                }
                Membership::Unavailable => {
                    return Err("Unable to verify server membership.".to_owned());
                }
            }
            check_live_event(executor, &guild_id, &event_id).await?;
            let record = RsvpRecord {
                guild_id,
                event_id,
                user_id: actor_id,
                status,
                responded_at: now_iso(),
            };
            if let Err(err) = put_rsvp(pool, &record).await {
                return Err(match err {
                    RsvpStoreError::EventAtCapacity => rsvp_event_full_text(),
                    RsvpStoreError::RsvpRateLimited => rsvp_rate_limited_text(),
                    // Occurrence capacity and retention floors cannot come
                    // out of an RSVP write; transport/parse failures stay
                    // generic. Every variant is mapped: a refused or failed
                    // write is never reported as saved.
                    RsvpStoreError::OccurrenceAtCapacity
                    | RsvpStoreError::CutoffTooRecent
                    | RsvpStoreError::Db(_)
                    | RsvpStoreError::UnknownStatus(_) => "Unable to save RSVP.".to_owned(),
                });
            }
            let audit_id = format!("rsvp:{}", interaction.id);
            write_audit(pool, &RsvpAudit::for_rsvp(&audit_id, &record))
                .await
                .map_err(|_| "Unable to audit RSVP.")?;
            // RA-03 race fence: the event or membership may have vanished
            // between the pre-write lookups and the commit. Re-read the same
            // evidence; on loss or lookup failure compensate (remove exactly
            // this attempt's rows) and refuse.
            if let Err(refusal) = revalidate_rsvp(
                executor,
                &record.guild_id,
                &record.event_id,
                &record.user_id,
            )
            .await
            {
                let _ = compensate_rsvp_write(
                    pool,
                    &record.guild_id,
                    &record.event_id,
                    &record.user_id,
                    &record.responded_at,
                    &audit_id,
                )
                .await;
                return Err(refusal);
            }
            Ok(rsvp_saved_text(status))
        }
        HandlerId::ScorecardAttendance => {
            // RA-02: Manage Events first, for every host check-in including
            // self-select. Selecting oneself takes this same path — there is
            // no unprivileged self-check-in branch — and the proof below stays
            // `HostCheckin` on all of them.
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
            // Trusted guild-scoped occurrence form (RA-02): a bare event id or
            // `{event_id}:{label}`. Bare slugs refuse here, before any network
            // or store work.
            let resolved = parse_attendance_occurrence(&occurrence).map_err(|e| e.to_string())?;
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
            // Discord resolves the selected User; fail closed before any
            // network rather than looking up a forged id.
            let user_known = data
                .resolved
                .as_ref()
                .is_some_and(|r| r.users.contains_key(&member_id));
            if !user_known {
                return Err("Unable to resolve attendance member.".to_owned());
            }
            // Trusted guild-scoped event binding (RA-02): the anchor must be a
            // live scheduled event in this guild. Unknown, foreign, cancelled
            // or unreadable events refuse with zero writes, mirroring the
            // `/rsvp` event-gate replies.
            let event = executor
                .get_scheduled_event(&guild_id, &resolved.anchor_event_id)
                .await?
                .ok_or("No scheduled event with that id exists in this server.")?;
            // A malformed or mismatched response is not evidence of a live event.
            if event["id"].as_str() != Some(resolved.anchor_event_id.as_str())
                || event["guild_id"].as_str() != Some(&guild_id)
            {
                return Err("Discord returned an invalid scheduled event status.".to_owned());
            }
            match event["status"].as_u64() {
                Some(4) => return Err("That scheduled event is cancelled.".to_owned()),
                Some(1..=3) => {}
                _ => return Err("Discord returned an invalid scheduled event status.".to_owned()),
            }
            // RA-01 acting-host gate: the invoker must currently belong to the
            // interaction's guild. It runs after the landed RA-02 event
            // binding so that block stays verbatim, and still precedes every
            // attendance write. A self check-in skips this lookup: the live
            // target read below verifies the same membership.
            let actor_id = interaction
                .author_id()
                .ok_or("Missing command member.")?
                .to_string();
            if member_id.to_string() != actor_id {
                match guild_membership(executor, &guild_id, &actor_id).await {
                    Membership::Current => {}
                    Membership::Absent => {
                        return Err(
                            "You must still belong to this server to record attendance.".to_owned()
                        );
                    }
                    Membership::Unavailable => {
                        return Err("Unable to verify server membership.".to_owned());
                    }
                }
            }
            // Current guild membership, live (RA-02): the resolved User proves
            // identity, not membership. A 404 is a departed, never-joined or
            // cross-guild target; transport or shape failures fail closed. The
            // bot flag comes from the same live read so a stale resolved copy
            // can never smuggle a bot into human attendance.
            let member_id_str = member_id.to_string();
            let live_member = executor
                .get_json_strict(&format!("/guilds/{guild_id}/members/{member_id_str}"))
                .await
                .map_err(|_| "Unable to verify attendance member.")?
                .ok_or("That member is not in this server.")?;
            let live_user = live_member
                .get("user")
                .ok_or_else(|| "Unable to verify attendance member.".to_owned())?;
            if live_user.get("id").and_then(serde_json::Value::as_str)
                != Some(member_id_str.as_str())
            {
                return Err("Unable to verify attendance member.".to_owned());
            }
            let member_is_bot = live_user
                .get("bot")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let verdict = classify(
                classifier,
                &ClassifyInput {
                    guild_id: guild_id.clone(),
                    actor_id: member_id_str.clone(),
                    is_bot: member_is_bot,
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
                    guild_id: guild_id.clone(),
                    event_occurrence_id: resolved.canonical_id.clone(),
                    member_id: member_id_str.clone(),
                    occurred_at: now_iso(),
                    proof: AttendanceProof::HostCheckin,
                    classifier_version: verdict.classifier_version,
                    classification: verdict.classification,
                    matched_rule: verdict.matched_rule,
                },
            )
            .await
            .map_err(|err| match err {
                RsvpStoreError::OccurrenceAtCapacity => checkin_occurrence_full_text(),
                // Event capacity, RSVP rate and retention floors cannot come
                // out of a check-in write; transport failures stay generic.
                // Every variant is mapped: a refused or failed write is never
                // reported as recorded.
                RsvpStoreError::EventAtCapacity
                | RsvpStoreError::RsvpRateLimited
                | RsvpStoreError::CutoffTooRecent
                | RsvpStoreError::Db(_)
                | RsvpStoreError::UnknownStatus(_) => "Attendance was not recorded.".to_owned(),
            })?;
            if !inserted {
                return Ok(checkin_duplicate_text(
                    &member_id_str,
                    &resolved.canonical_id,
                ));
            }
            // RA-03 race fence: the anchor event or target membership may have
            // vanished between the pre-write RA-02 reads and the commit. The
            // `record_checkin` transaction only bounds the occurrence count —
            // it cannot see Discord-side removal — so re-read the same live
            // evidence here; on loss or lookup failure compensate (delete
            // exactly this attempt's fact) and refuse. Duplicates return above
            // without revalidation: they wrote nothing.
            if let Err(refusal) = revalidate_attendance(
                executor,
                &guild_id,
                &resolved.anchor_event_id,
                &member_id_str,
            )
            .await
            {
                let _ = compensate_checkin_write(
                    pool,
                    &checkin_idempotency_key(&resolved.canonical_id, &member_id_str),
                )
                .await;
                return Err(refusal);
            }
            Ok(checkin_recorded_text(
                &member_id_str,
                &resolved.canonical_id,
            ))
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
