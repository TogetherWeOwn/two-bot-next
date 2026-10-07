//! Scheduled-message commands (`/schedule`, `/schedule-remove`,
//! `/schedule-list`; parity §1 rows 17–19, slice of TOG-9809).
//!
//! Thin wiring over the `scheduled` domain + `scheduled_store` (TOG-10081):
//! decode options → validate → guild-scoped CRUD → `automation_audit_log`
//! row → ephemeral completion. Routing, the `TWO_AUTOMATIONS` gate and the
//! `ManageGuild` refusal all happen in the shared router before these run,
//! so the handlers assume an authorized automations invocation. A schedule
//! binds to the invoking channel, like `/feed-add` binds its relay.
//!
//! Audit parity with legacy `AutomationService`: creates and replaces audit
//! `scheduled.create` / `scheduled.update` (`ok`, `rejected` on validation
//! failure), removes audit `scheduled.delete` (`ok` / `absent`), and lists
//! write no row. The 15 s ticker is a sibling card's job — nothing here posts
//! to channels.

use sqlx::{Pool, Postgres};
use twilight_model::application::interaction::{
    application_command::CommandOptionValue, Interaction, InteractionData,
};
use two_bot_core::{
    funnel::now_millis_for_test,
    message_safety,
    scheduled::{
        format_iso_ms, next_run_at_ms, no_such_schedule_text, no_unique_match_text,
        schedule_cancelled_text, schedule_confirm_text, schedule_list_line, schedule_list_text,
        validate_schedule, ScheduleInput,
    },
    scheduled_store::{
        audit_scheduled, delete_scheduled, get_scheduled, list_scheduled, put_scheduled,
        resolve_scheduled_id as resolve_scheduled_id_store, ScheduledAuditInput, ScheduledWrite,
    },
};
use two_bot_discord::ActionExecutor;

use crate::command_runtime::{actor_id, new_id};

/// Safe reply when the store fails — never leak sqlx internals to Discord.
const STORE_FAILURE_REPLY: &str = "Schedule command failed; try again.";

/// Reply when the interaction arrives without a channel (pathological —
/// Discord always sends `channel_id` for guild slash commands). Copy of the
/// shared runtime's private constant: this module stays wiring-only so
/// `command_runtime.rs` keeps registration-line edits.
const NO_CHANNEL_REPLY: &str = "This command only works in a channel.";

// Schedule tests live in `command_runtime_tests.rs` alongside the feed
// slice's: they reuse its MockRest double, interaction builders and
// agent-testdb fixture, which cannot be imported from a submodule here.

/// `/schedule`: validate → upsert → audit → ephemeral confirmation.
/// Validation failures still audit (`rejected`): the id is minted before
/// validation, exactly like legacy, so the trail names the attempted row.
pub(crate) async fn schedule_create(
    pool: &Pool<Postgres>,
    executor: &ActionExecutor,
    guild_id: &str,
    interaction: &Interaction,
) {
    let Some(channel_id) = interaction_channel(interaction) else {
        finish(executor, interaction, NO_CHANNEL_REPLY).await;
        return;
    };
    let actor = actor_id(interaction);
    let (body, in_minutes, every_minutes) = schedule_options(interaction);
    let id = new_id();
    let now_iso = format_iso_ms(now_millis());

    let validated = match validate_schedule(&ScheduleInput {
        body: body.as_deref().unwrap_or_default(),
        in_minutes,
        every_minutes,
    }) {
        Ok(validated) => validated,
        Err(err) => {
            audit(
                pool,
                guild_id,
                &actor,
                "scheduled.create",
                &id,
                "rejected",
                Some(&err.to_string()),
                &now_iso,
            )
            .await;
            finish(executor, interaction, err.to_string()).await;
            return;
        }
    };

    // The id is fresh, so this read is almost always empty — but like legacy
    // it decides the create/update audit action, the confirm verb, and the
    // creator passthrough on a replace.
    let existing = match get_scheduled(pool, guild_id, &id).await {
        Ok(existing) => existing,
        Err(err) => {
            tracing::warn!(error = %err, "schedule read failed");
            finish(executor, interaction, STORE_FAILURE_REPLY).await;
            return;
        }
    };
    let next_run_at = format_iso_ms(next_run_at_ms(now_millis(), validated.delay_minutes));
    // Whole minutes by construction (`every-minutes * 60`), so this inverts
    // exactly; the confirm helper renders the recurring shape from it.
    let every_m = validated.interval_seconds.map(|s| s / 60);
    let write = ScheduledWrite {
        id: id.clone(),
        guild_id: guild_id.to_owned(),
        channel_id,
        body: validated.body,
        next_run_at: next_run_at.clone(),
        interval_seconds: validated.interval_seconds,
        enabled: true,
        created_by: existing
            .as_ref()
            .map(|row| row.created_by.clone())
            .unwrap_or_else(|| actor.clone()),
        created_at: existing
            .as_ref()
            .map(|row| row.created_at.clone())
            .unwrap_or_else(|| now_iso.clone()),
        updated_by: actor.clone(),
        updated_at: now_iso.clone(),
    };
    match put_scheduled(pool, &write).await {
        Ok(true) => {
            let created = existing.is_none();
            audit(
                pool,
                guild_id,
                &actor,
                if created {
                    "scheduled.create"
                } else {
                    "scheduled.update"
                },
                &id,
                "ok",
                None,
                &now_iso,
            )
            .await;
            finish(
                executor,
                interaction,
                schedule_confirm_text(created, &id, every_m, &next_run_at),
            )
            .await;
        }
        Ok(false) => {
            // The id belongs to another guild (legacy guild-scoped no-match).
            audit(
                pool,
                guild_id,
                &actor,
                "scheduled.create",
                &id,
                "rejected",
                Some("id belongs to another guild"),
                &now_iso,
            )
            .await;
            finish(
                executor,
                interaction,
                "Scheduled message id belongs to another guild.",
            )
            .await;
        }
        Err(err) => {
            tracing::warn!(error = %err, "schedule put failed");
            finish(executor, interaction, STORE_FAILURE_REPLY).await;
        }
    }
}

/// `/schedule-remove`: prefix-resolve the id → guild-scoped delete →
/// `scheduled.delete` audit (`ok` / `absent`) → ephemeral confirmation.
/// Zero or ambiguous prefix matches refuse with the shared no-unique-match
/// text and mutate nothing.
pub(crate) async fn schedule_remove(
    pool: &Pool<Postgres>,
    executor: &ActionExecutor,
    guild_id: &str,
    interaction: &Interaction,
) {
    let prefix = schedule_remove_option(interaction).unwrap_or_default();
    let resolved = match resolve_scheduled_id_store(pool, guild_id, &prefix).await {
        Ok(resolved) => resolved,
        Err(err) => {
            tracing::warn!(error = %err, "schedule resolve failed");
            finish(executor, interaction, STORE_FAILURE_REPLY).await;
            return;
        }
    };
    let Some(id) = resolved else {
        finish(executor, interaction, no_unique_match_text(&prefix)).await;
        return;
    };
    let actor = actor_id(interaction);
    let now_iso = format_iso_ms(now_millis());
    match delete_scheduled(pool, guild_id, &id).await {
        Ok(true) => {
            audit(
                pool,
                guild_id,
                &actor,
                "scheduled.delete",
                &id,
                "ok",
                None,
                &now_iso,
            )
            .await;
            finish(executor, interaction, schedule_cancelled_text()).await;
        }
        Ok(false) => {
            // Resolved but already gone (raced delete): legacy's `absent`.
            audit(
                pool,
                guild_id,
                &actor,
                "scheduled.delete",
                &id,
                "absent",
                None,
                &now_iso,
            )
            .await;
            finish(executor, interaction, no_such_schedule_text(&id)).await;
        }
        Err(err) => {
            tracing::warn!(error = %err, "schedule delete failed");
            finish(executor, interaction, STORE_FAILURE_REPLY).await;
        }
    }
}

/// `/schedule-list`: guild-scoped list → ephemeral text. Legacy lists every
/// row for the guild and writes no audit row.
pub(crate) async fn schedule_list(
    pool: &Pool<Postgres>,
    executor: &ActionExecutor,
    guild_id: &str,
    interaction: &Interaction,
) {
    match list_scheduled(pool, guild_id).await {
        Ok(rows) => {
            let lines: Vec<String> = rows
                .iter()
                .map(|row| {
                    schedule_list_line(
                        &row.id,
                        &row.channel_id,
                        &row.next_run_at,
                        row.interval_seconds,
                        row.enabled,
                    )
                })
                .collect();
            finish(
                executor,
                interaction,
                message_safety::truncate(&schedule_list_text(&lines), 2000),
            )
            .await;
        }
        Err(err) => {
            tracing::warn!(error = %err, "schedule list failed");
            finish(executor, interaction, STORE_FAILURE_REPLY).await;
        }
    }
}

/// Current epoch millis, clamped non-negative for the ISO formatter.
fn now_millis() -> u64 {
    now_millis_for_test().max(0) as u64
}

/// Invoking channel as a snowflake string. `channel_id` is deprecated on the
/// wire but still the field Discord populates for guild slash commands; the
/// replacement `channel` object carries the id plus a payload we never read.
/// Mirrors the shared runtime's private helper for the same reason as
/// [`NO_CHANNEL_REPLY`].
#[allow(deprecated)]
fn interaction_channel(interaction: &Interaction) -> Option<String> {
    interaction
        .channel
        .as_ref()
        .map(|channel| channel.id.get().to_string())
        .or_else(|| interaction.channel_id.map(|id| id.get().to_string()))
}

/// Append one `scheduled.*` audit row; a write failure is logged but never
/// fails the operation it records (same posture as the sticky/feed slices).
#[allow(clippy::too_many_arguments)]
async fn audit(
    pool: &Pool<Postgres>,
    guild_id: &str,
    actor_id: &str,
    action: &str,
    target_key: &str,
    outcome: &str,
    reason: Option<&str>,
    now_iso: &str,
) {
    let input = ScheduledAuditInput {
        guild_id: guild_id.to_owned(),
        actor_id: Some(actor_id.to_owned()).filter(|actor| !actor.is_empty()),
        action: action.to_owned(),
        target_key: Some(target_key.to_owned()),
        outcome: outcome.to_owned(),
        reason: reason.map(str::to_owned),
    };
    if let Err(err) = audit_scheduled(pool, &input, now_iso, &new_id()).await {
        tracing::warn!(action, outcome, error = %err, "schedule audit write failed");
    }
}

/// Complete the original ephemeral defer, never issue a second callback.
async fn finish(executor: &ActionExecutor, interaction: &Interaction, content: impl AsRef<str>) {
    if let Err(err) = executor
        .edit_interaction_response_with_blocked_retry(
            interaction.application_id.get(),
            &interaction.token,
            content.as_ref(),
        )
        .await
    {
        tracing::warn!(interaction_id = %interaction.id.get(), error = %err, "schedule reply edit failed");
    }
}

/// Extract `/schedule`'s options: `body` (required string) + `in-minutes` /
/// `every-minutes` (optional integers). Missing values decode to `None`;
/// validation owns the refusal text.
pub(crate) fn schedule_options(
    interaction: &Interaction,
) -> (Option<String>, Option<i64>, Option<i64>) {
    let mut body = None;
    let mut in_minutes = None;
    let mut every_minutes = None;
    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return (body, in_minutes, every_minutes);
    };
    for option in &data.options {
        match (option.name.as_str(), &option.value) {
            ("body", CommandOptionValue::String(value)) => body = Some(value.clone()),
            ("in-minutes", CommandOptionValue::Integer(value)) => in_minutes = Some(*value),
            ("every-minutes", CommandOptionValue::Integer(value)) => {
                every_minutes = Some(*value);
            }
            _ => {}
        }
    }
    (body, in_minutes, every_minutes)
}

/// `/schedule-remove`'s `id` option; `None` resolves against an empty prefix,
/// which matches nothing and gets the no-unique-match refusal.
pub(crate) fn schedule_remove_option(interaction: &Interaction) -> Option<String> {
    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return None;
    };
    data.options
        .iter()
        .find_map(|option| match (option.name.as_str(), &option.value) {
            ("id", CommandOptionValue::String(value)) => Some(value.clone()),
            _ => None,
        })
}
