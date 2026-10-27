//! Scheduled-message management (`/schedule-inspect`, `/schedule-edit`).
//!
//! The read-mostly siblings of [`schedule_runtime`](crate::schedule_runtime):
//! inspect is read-only including audits (no audit row on any inspect path),
//! and a correction audits exactly one content-free `scheduled.edit` / `ok`
//! row, committed atomically with the body change — never a refusal audit,
//! so a failed audit rolls the body back instead of recording beside it.
//!
//! A separate module (not part of `schedule_runtime`) because the
//! `schedule_preview_acceptance` integration target path-includes
//! `schedule_runtime.rs` without exercising the management handlers; keeping
//! them here keeps `-D warnings` green there with no waiver attributes.

use sqlx::{Pool, Postgres};
use twilight_model::application::interaction::{
    application_command::CommandOptionValue, Interaction, InteractionData,
};
use two_bot_core::{
    commands::MAX_RESOURCE_ID_CHARS,
    scheduled::{
        format_iso_ms, parse_inspect_view, schedule_delivery_state_word, schedule_edit_fenced_text,
        schedule_edit_revision_required_text, schedule_edit_saved_text, schedule_edit_stale_text,
        schedule_edit_unchanged_text, schedule_inspect_cadence_text,
        schedule_inspect_last_run_text, schedule_inspect_metadata_text, schedule_unknown_id_text,
        validate_schedule_body, InspectView, MAX_SCHEDULE_REVISION_CHARS,
    },
    scheduled_store::{
        get_scheduled, resolve_scheduled_id as resolve_scheduled_id_store, schedule_revision_token,
        try_update_scheduled_body, BodyEdit, BodyEditOutcome,
    },
};
use two_bot_discord::ActionExecutor;

use crate::command_runtime::{actor_id, new_id};
use crate::db_log::{db_class, discord_class};
use crate::schedule_runtime::{finish, now_millis, STORE_FAILURE_REPLY};

/// `/schedule-inspect`: prefix-resolve the id → render one private view.
/// The metadata view reports the canonical id, destination, due instant,
/// cadence, truthful delivery state, last-run evidence and the opaque
/// edit revision; the body view renders the whole saved definition
/// verbatim, with no header budget loss. Read-only including audits: no
/// inspect path writes a schedule or audit row, and every refusal echoes
/// no user input.
pub(crate) async fn schedule_inspect(
    pool: &Pool<Postgres>,
    executor: &ActionExecutor,
    guild_id: &str,
    interaction: &Interaction,
) {
    let (id, view) = match schedule_inspect_options_strict(interaction) {
        Ok(decoded) => decoded,
        Err(refusal) => {
            finish(executor, interaction, refusal).await;
            return;
        }
    };
    let view = match parse_inspect_view(view.as_deref()) {
        Ok(view) => view,
        Err(refusal) => {
            finish(executor, interaction, refusal).await;
            return;
        }
    };
    let id = id.unwrap_or_default();
    if id.is_empty() || utf16_len(&id) > MAX_RESOURCE_ID_CHARS {
        finish(executor, interaction, schedule_unknown_id_text()).await;
        return;
    }
    let resolved = match resolve_scheduled_id_store(pool, guild_id, &id).await {
        Ok(resolved) => resolved,
        Err(err) => {
            tracing::warn!(reason = db_class(&err), "schedule inspect resolve failed");
            finish(executor, interaction, STORE_FAILURE_REPLY).await;
            return;
        }
    };
    let Some(canonical) = resolved else {
        finish(executor, interaction, schedule_unknown_id_text()).await;
        return;
    };
    let row = match get_scheduled(pool, guild_id, &canonical).await {
        Ok(row) => row,
        Err(err) => {
            tracing::warn!(reason = db_class(&err), "schedule inspect read failed");
            finish(executor, interaction, STORE_FAILURE_REPLY).await;
            return;
        }
    };
    let Some(row) = row else {
        finish(executor, interaction, schedule_unknown_id_text()).await;
        return;
    };
    match view {
        InspectView::Body => {
            finish_verbatim(executor, interaction, &row.body).await;
        }
        InspectView::Metadata => {
            let state = schedule_delivery_state_word(
                row.claim_token.is_some(),
                row.occurrence_nonce.is_some(),
                row.delivery_uncertain,
                row.enabled,
            );
            finish(
                executor,
                interaction,
                schedule_inspect_metadata_text(
                    &row.id,
                    &row.channel_id,
                    &row.next_run_at,
                    &schedule_inspect_cadence_text(row.interval_seconds),
                    &state,
                    &schedule_inspect_last_run_text(
                        row.last_run_at.as_deref(),
                        row.last_message_id.as_deref(),
                    ),
                    &schedule_revision_token(&row),
                ),
            )
            .await;
        }
    }
}

/// `/schedule-edit`: revision-bound body-only correction. The id resolves
/// like remove; the revision must equal the token inspect issued for the
/// current definition AND its delivery evidence; the replacement body
/// passes the creation policy. The store compares the revision, rechecks
/// the delivery fences and writes body/updater/timestamp plus the
/// content-free audit under one row lock, so a racing ticker sees either
/// the old or the new body and racing edits produce exactly one winner.
/// Every refusal leaves the entire row unchanged and audits nothing.
pub(crate) async fn schedule_edit(
    pool: &Pool<Postgres>,
    executor: &ActionExecutor,
    guild_id: &str,
    interaction: &Interaction,
) {
    let (id, revision, body) = match schedule_edit_options_strict(interaction) {
        Ok(decoded) => decoded,
        Err(refusal) => {
            finish(executor, interaction, refusal).await;
            return;
        }
    };
    let id = id.unwrap_or_default();
    if id.is_empty() || utf16_len(&id) > MAX_RESOURCE_ID_CHARS {
        finish(executor, interaction, schedule_unknown_id_text()).await;
        return;
    }
    let revision = revision.unwrap_or_default();
    if revision.is_empty() || utf16_len(&revision) > MAX_SCHEDULE_REVISION_CHARS {
        finish(
            executor,
            interaction,
            schedule_edit_revision_required_text(),
        )
        .await;
        return;
    }
    let validated = match validate_schedule_body(body.as_deref().unwrap_or_default()) {
        Ok(validated) => validated,
        Err(err) => {
            finish(executor, interaction, err.to_string()).await;
            return;
        }
    };
    let resolved = match resolve_scheduled_id_store(pool, guild_id, &id).await {
        Ok(resolved) => resolved,
        Err(err) => {
            tracing::warn!(reason = db_class(&err), "schedule edit resolve failed");
            finish(executor, interaction, STORE_FAILURE_REPLY).await;
            return;
        }
    };
    let Some(canonical) = resolved else {
        finish(executor, interaction, schedule_unknown_id_text()).await;
        return;
    };
    let actor = actor_id(interaction);
    let now_iso = format_iso_ms(now_millis());
    let audit_id = new_id();
    let edit = BodyEdit {
        guild_id,
        id: &canonical,
        expected_revision: &revision,
        body: &validated,
        actor: &actor,
        now_iso: &now_iso,
        audit_id: &audit_id,
    };
    match try_update_scheduled_body(pool, &edit).await {
        Ok(BodyEditOutcome::Saved { row }) => {
            finish(
                executor,
                interaction,
                schedule_edit_saved_text(&row.id, &schedule_revision_token(&row)),
            )
            .await;
        }
        Ok(BodyEditOutcome::Stale) => {
            finish(executor, interaction, schedule_edit_stale_text(&canonical)).await;
        }
        Ok(BodyEditOutcome::Fenced { fence }) => {
            finish(
                executor,
                interaction,
                schedule_edit_fenced_text(&canonical, fence.detail()),
            )
            .await;
        }
        Ok(BodyEditOutcome::Unchanged) => {
            finish(executor, interaction, schedule_edit_unchanged_text()).await;
        }
        Ok(BodyEditOutcome::Missing) => {
            finish(executor, interaction, schedule_unknown_id_text()).await;
        }
        Err(err) => {
            tracing::warn!(reason = db_class(&err), "schedule edit write failed");
            finish(executor, interaction, STORE_FAILURE_REPLY).await;
        }
    }
}

/// UTF-16 length, capped one past the id bound so hostile input cannot buy
/// more than a bounded scan. Mirrors the remove handler's inline check.
fn utf16_len(text: &str) -> usize {
    text.encode_utf16().take(MAX_RESOURCE_ID_CHARS + 1).count()
}

/// Inspect body-view completion: same retry/error contract as [`finish`],
/// without mention neutralization — the ephemeral reply's explicit empty
/// `allowed_mentions` is the mention safety, and the admin must see the
/// saved body exactly as stored (legacy rows predate creation-time
/// neutralization). Every other schedule reply keeps the neutralizing path.
async fn finish_verbatim(
    executor: &ActionExecutor,
    interaction: &Interaction,
    content: impl AsRef<str>,
) {
    if let Err(err) = executor
        .edit_interaction_response_without_mention_neutralization_with_blocked_retry(
            interaction.application_id.get(),
            &interaction.token,
            content.as_ref(),
        )
        .await
    {
        tracing::warn!(interaction_id = %interaction.id.get(), reason = discord_class(&err), "schedule reply edit failed");
    }
}

const SCHEDULE_INSPECT_INVALID_ID: &str =
    "Invalid id option. Use /schedule-inspect id:<schedule-id> view:<metadata|body>.";
const SCHEDULE_INSPECT_DUPLICATE_ID: &str =
    "Duplicate id option. Use /schedule-inspect id:<schedule-id> view:<metadata|body>.";
const SCHEDULE_INSPECT_INVALID_VIEW: &str =
    "Invalid view option. Use /schedule-inspect id:<schedule-id> view:<metadata|body>.";
const SCHEDULE_INSPECT_DUPLICATE_VIEW: &str =
    "Duplicate view option. Use /schedule-inspect id:<schedule-id> view:<metadata|body>.";
const SCHEDULE_EDIT_INVALID_ID: &str =
    "Invalid id option. Use /schedule-edit id:<schedule-id> revision:<revision> body:<text>.";
const SCHEDULE_EDIT_DUPLICATE_ID: &str =
    "Duplicate id option. Use /schedule-edit id:<schedule-id> revision:<revision> body:<text>.";
const SCHEDULE_EDIT_INVALID_REVISION: &str =
    "Invalid revision option. Use /schedule-edit id:<schedule-id> revision:<revision> body:<text>.";
const SCHEDULE_EDIT_DUPLICATE_REVISION: &str =
    "Duplicate revision option. Use /schedule-edit id:<schedule-id> revision:<revision> body:<text>.";
const SCHEDULE_EDIT_INVALID_BODY: &str =
    "Invalid body option. Use /schedule-edit id:<schedule-id> revision:<revision> body:<text>.";
const SCHEDULE_EDIT_DUPLICATE_BODY: &str =
    "Duplicate body option. Use /schedule-edit id:<schedule-id> revision:<revision> body:<text>.";

/// Decoded `/schedule-inspect` options: `id` + `view`, both optional at
/// decode time (missing `id` refuses before resolution, missing `view`
/// selects metadata).
type ScheduleInspectOptions = (Option<String>, Option<String>);

/// Strictly decode `/schedule-inspect`'s `id` and `view` options. A
/// present option with the wrong wire type, or an option name sent twice,
/// refuses with the literal naming that option; name-count wins over
/// type-check, and `id` wins over `view`. Missing options decode to
/// `None` and unknown names stay ignored.
pub(crate) fn schedule_inspect_options_strict(
    interaction: &Interaction,
) -> Result<ScheduleInspectOptions, &'static str> {
    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return Ok((None, None));
    };
    let mut ids: Vec<&CommandOptionValue> = Vec::new();
    let mut views: Vec<&CommandOptionValue> = Vec::new();
    for option in &data.options {
        match option.name.as_str() {
            "id" => ids.push(&option.value),
            "view" => views.push(&option.value),
            _ => {}
        }
    }
    if ids.len() > 1 {
        return Err(SCHEDULE_INSPECT_DUPLICATE_ID);
    }
    if views.len() > 1 {
        return Err(SCHEDULE_INSPECT_DUPLICATE_VIEW);
    }
    let id = match ids.first() {
        None => None,
        Some(CommandOptionValue::String(value)) => Some((*value).clone()),
        Some(_) => return Err(SCHEDULE_INSPECT_INVALID_ID),
    };
    let view = match views.first() {
        None => None,
        Some(CommandOptionValue::String(value)) => Some((*value).clone()),
        Some(_) => return Err(SCHEDULE_INSPECT_INVALID_VIEW),
    };
    Ok((id, view))
}

/// Decoded `/schedule-edit` options: `id` + `revision` + `body`, all
/// optional at decode time (missing values refuse with their own
/// correction before any store read).
type ScheduleEditOptions = (Option<String>, Option<String>, Option<String>);

/// Strictly decode `/schedule-edit`'s `id`, `revision` and `body`
/// options. A present option with the wrong wire type, or an option name
/// sent twice, refuses with the literal naming that option; name-count
/// wins over type-check, and `id` wins over `revision` wins over `body`.
/// Missing options decode to `None` and unknown names stay ignored.
pub(crate) fn schedule_edit_options_strict(
    interaction: &Interaction,
) -> Result<ScheduleEditOptions, &'static str> {
    let Some(InteractionData::ApplicationCommand(data)) = &interaction.data else {
        return Ok((None, None, None));
    };
    let mut ids: Vec<&CommandOptionValue> = Vec::new();
    let mut revisions: Vec<&CommandOptionValue> = Vec::new();
    let mut bodies: Vec<&CommandOptionValue> = Vec::new();
    for option in &data.options {
        match option.name.as_str() {
            "id" => ids.push(&option.value),
            "revision" => revisions.push(&option.value),
            "body" => bodies.push(&option.value),
            _ => {}
        }
    }
    if ids.len() > 1 {
        return Err(SCHEDULE_EDIT_DUPLICATE_ID);
    }
    if revisions.len() > 1 {
        return Err(SCHEDULE_EDIT_DUPLICATE_REVISION);
    }
    if bodies.len() > 1 {
        return Err(SCHEDULE_EDIT_DUPLICATE_BODY);
    }
    let id = match ids.first() {
        None => None,
        Some(CommandOptionValue::String(value)) => Some((*value).clone()),
        Some(_) => return Err(SCHEDULE_EDIT_INVALID_ID),
    };
    let revision = match revisions.first() {
        None => None,
        Some(CommandOptionValue::String(value)) => Some((*value).clone()),
        Some(_) => return Err(SCHEDULE_EDIT_INVALID_REVISION),
    };
    let body = match bodies.first() {
        None => None,
        Some(CommandOptionValue::String(value)) => Some((*value).clone()),
        Some(_) => return Err(SCHEDULE_EDIT_INVALID_BODY),
    };
    Ok((id, revision, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_runtime_tests::slash;
    use twilight_model::application::interaction::application_command::CommandDataOption;

    fn text(name: &str, value: &str) -> CommandDataOption {
        CommandDataOption {
            name: name.to_owned(),
            value: CommandOptionValue::String(value.to_owned()),
        }
    }

    fn number(name: &str, value: i64) -> CommandDataOption {
        CommandDataOption {
            name: name.to_owned(),
            value: CommandOptionValue::Integer(value),
        }
    }

    #[test]
    fn inspect_decoder_passes_valid_and_ignores_unknown() {
        let interaction = slash(
            "schedule-inspect",
            Some(3333),
            vec![text("id", "abc"), text("view", "body"), text("other", "x")],
        );
        assert_eq!(
            schedule_inspect_options_strict(&interaction),
            Ok((Some("abc".to_owned()), Some("body".to_owned())))
        );
        let bare = slash("schedule-inspect", Some(3333), Vec::new());
        assert_eq!(schedule_inspect_options_strict(&bare), Ok((None, None)));
    }

    #[test]
    fn inspect_decoder_refuses_wrong_types_and_duplicates() {
        let wrong_id = slash(
            "schedule-inspect",
            Some(3333),
            vec![number("id", 1), text("view", "body")],
        );
        assert_eq!(
            schedule_inspect_options_strict(&wrong_id),
            Err(SCHEDULE_INSPECT_INVALID_ID)
        );
        let wrong_view = slash(
            "schedule-inspect",
            Some(3333),
            vec![text("id", "abc"), number("view", 1)],
        );
        assert_eq!(
            schedule_inspect_options_strict(&wrong_view),
            Err(SCHEDULE_INSPECT_INVALID_VIEW)
        );
        let dupe_id = slash(
            "schedule-inspect",
            Some(3333),
            vec![text("id", "a"), text("id", "b")],
        );
        assert_eq!(
            schedule_inspect_options_strict(&dupe_id),
            Err(SCHEDULE_INSPECT_DUPLICATE_ID)
        );
        let dupe_view = slash(
            "schedule-inspect",
            Some(3333),
            vec![text("view", "body"), text("view", "metadata")],
        );
        assert_eq!(
            schedule_inspect_options_strict(&dupe_view),
            Err(SCHEDULE_INSPECT_DUPLICATE_VIEW)
        );
        // Name-count wins over type-check, and id wins over view.
        let both = slash(
            "schedule-inspect",
            Some(3333),
            vec![number("view", 1), text("id", "a"), text("id", "b")],
        );
        assert_eq!(
            schedule_inspect_options_strict(&both),
            Err(SCHEDULE_INSPECT_DUPLICATE_ID)
        );
    }

    #[test]
    fn edit_decoder_passes_valid_and_ignores_unknown() {
        let interaction = slash(
            "schedule-edit",
            Some(3333),
            vec![
                text("id", "abc"),
                text("revision", "rev"),
                text("body", "new"),
                text("other", "x"),
            ],
        );
        assert_eq!(
            schedule_edit_options_strict(&interaction),
            Ok((
                Some("abc".to_owned()),
                Some("rev".to_owned()),
                Some("new".to_owned())
            ))
        );
        let bare = slash("schedule-edit", Some(3333), Vec::new());
        assert_eq!(schedule_edit_options_strict(&bare), Ok((None, None, None)));
    }

    #[test]
    fn edit_decoder_refuses_wrong_types_and_duplicates() {
        for (options, expected) in [
            (vec![number("id", 1)], SCHEDULE_EDIT_INVALID_ID),
            (
                vec![text("id", "a"), text("id", "b")],
                SCHEDULE_EDIT_DUPLICATE_ID,
            ),
            (vec![number("revision", 1)], SCHEDULE_EDIT_INVALID_REVISION),
            (
                vec![text("revision", "a"), text("revision", "b")],
                SCHEDULE_EDIT_DUPLICATE_REVISION,
            ),
            (vec![number("body", 1)], SCHEDULE_EDIT_INVALID_BODY),
            (
                vec![text("body", "a"), text("body", "b")],
                SCHEDULE_EDIT_DUPLICATE_BODY,
            ),
        ] {
            let interaction = slash("schedule-edit", Some(3333), options);
            assert_eq!(schedule_edit_options_strict(&interaction), Err(expected));
        }
        // Priority: id duplicates beat revision duplicates beat body
        // duplicates; duplicates beat wrong types.
        let interaction = slash(
            "schedule-edit",
            Some(3333),
            vec![
                number("body", 1),
                text("revision", "a"),
                text("revision", "b"),
                text("id", "a"),
                text("id", "b"),
            ],
        );
        assert_eq!(
            schedule_edit_options_strict(&interaction),
            Err(SCHEDULE_EDIT_DUPLICATE_ID)
        );
        let interaction = slash(
            "schedule-edit",
            Some(3333),
            vec![
                number("body", 1),
                text("body", "a"),
                text("revision", "a"),
                text("revision", "b"),
            ],
        );
        assert_eq!(
            schedule_edit_options_strict(&interaction),
            Err(SCHEDULE_EDIT_DUPLICATE_REVISION)
        );
    }

    #[test]
    fn non_command_interactions_decode_to_absent_options() {
        let mut interaction = slash("schedule-inspect", Some(3333), Vec::new());
        interaction.data = None;
        assert_eq!(
            schedule_inspect_options_strict(&interaction),
            Ok((None, None))
        );
        assert_eq!(
            schedule_edit_options_strict(&interaction),
            Ok((None, None, None))
        );
    }

    #[test]
    fn id_and_revision_bounds_match_the_registry() {
        assert_eq!(utf16_len("abc"), 3);
        assert_eq!(utf16_len(&"😀".repeat(64)), 128);
        // The scan caps one past the bound: over-long input still refuses.
        assert!(utf16_len(&"😀".repeat(65)) > MAX_RESOURCE_ID_CHARS);
        assert!(utf16_len(&"x".repeat(129)) > MAX_RESOURCE_ID_CHARS);
        assert!(utf16_len(&"x".repeat(129)) > MAX_SCHEDULE_REVISION_CHARS);
        assert_eq!(MAX_SCHEDULE_REVISION_CHARS, 128);
    }
}
