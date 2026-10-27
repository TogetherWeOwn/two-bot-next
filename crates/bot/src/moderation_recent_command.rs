//! `/moderation-recent`: a private, staff-only, read-only page of the newest
//! saved moderation ledger rows.
//!
//! The router has already fenced the guild, the moderation gate and the
//! Moderate Members bit, and the caller deferred the reply. Routing is not a
//! freshness guarantee, so this handler decodes its single `page` option
//! strictly (before any Discord or store read), then rechecks the actor's
//! live permissions over REST exactly like the member verbs do, and only
//! then reads the ledger. It never claims, audits, retries, repairs or
//! mutates anything, and every failure answers a fixed private corrective.

use sqlx::PgPool;
use twilight_model::application::interaction::{
    application_command::CommandOptionValue, Interaction, InteractionData,
};
use two_bot_core::{
    commands::PERM_MODERATE_MEMBERS,
    moderation_recent::{
        render_page, RecentView, RECENT_EMPTY_REPLY, RECENT_INVALID_PAGE_REPLY, RECENT_MAX_PAGE,
        RECENT_UNAVAILABLE_REPLY,
    },
    moderation_recent_store::read_recent_moderation,
    router::{RouterRefusal, GUILD_RESTRICTED_REPLY},
};
use two_bot_discord::ActionExecutor;

use crate::{
    command_runtime::actor_id,
    db_log::db_class,
    member_runtime::{actor_authority, fetched_member, guild_facts, sanitize_option_name},
};

const USAGE: &str = "/moderation-recent page:1";
/// Fixed reply when the actor's live standing cannot be established. It
/// names no Discord detail and discloses no rows.
const AUTHORITY_UNAVAILABLE_REPLY: &str =
    "Could not verify your permissions right now, so no moderation rows are shown. Please try again shortly.";

fn refuse(detail: &str) -> String {
    format!("Cannot run /moderation-recent: {detail}. Try: {USAGE}")
}

/// Strictly decode the optional `page` selector. Duplicate, unknown and
/// wrong-typed options refuse with a correction; an integer outside
/// `1..=RECENT_MAX_PAGE` answers the invalid-page reply. Nothing is echoed
/// except a sanitized option name.
fn decode_page(interaction: &Interaction) -> Result<i64, String> {
    let options = match &interaction.data {
        Some(InteractionData::ApplicationCommand(data)) => data.options.as_slice(),
        _ => &[],
    };
    let mut page = None;
    for option in options {
        if option.name != "page" {
            return Err(refuse(&format!(
                "option \"{}\" is not a /moderation-recent option",
                sanitize_option_name(&option.name)
            )));
        }
        if page.is_some() {
            return Err(refuse("option \"page\" was sent twice"));
        }
        match option.value {
            CommandOptionValue::Integer(value) => page = Some(value),
            _ => return Err(refuse("option \"page\" must be a whole number")),
        }
    }
    let page = page.unwrap_or(1);
    if (1..=RECENT_MAX_PAGE).contains(&page) {
        Ok(page)
    } else {
        Err(RECENT_INVALID_PAGE_REPLY.to_owned())
    }
}

/// Recheck the actor's live Moderate Members standing. Interaction-time
/// permission bits never authorize the read.
async fn recheck_staff(
    executor: &ActionExecutor,
    guild_id: &str,
    actor: &str,
) -> Result<(), String> {
    let unavailable = |cause: &'static str| {
        tracing::warn!(
            msg = "moderation_recent_authority_unavailable",
            cause,
            "staff authority could not be established"
        );
        AUTHORITY_UNAVAILABLE_REPLY.to_owned()
    };
    let facts = guild_facts(executor, guild_id)
        .await
        .map_err(|cause| unavailable(cause.token()))?;
    let roles = match fetched_member(executor, guild_id, actor).await {
        Ok(Some((roles, _))) => roles,
        Ok(None) => return Err(unavailable("actor_missing")),
        Err(cause) => return Err(unavailable(cause.token())),
    };
    let authority =
        actor_authority(actor, roles, &facts).ok_or_else(|| unavailable("malformed_roles"))?;
    if authority.permissions & PERM_MODERATE_MEMBERS == 0 {
        return Err(RouterRefusal::ModerationRecentPermission.message());
    }
    Ok(())
}

async fn reply_for(
    pool: &PgPool,
    configured_guild: u64,
    executor: &ActionExecutor,
    interaction: &Interaction,
) -> String {
    let page = match decode_page(interaction) {
        Ok(page) => page,
        Err(reply) => return reply,
    };
    if interaction.guild_id.map(|id| id.get()) != Some(configured_guild) {
        return GUILD_RESTRICTED_REPLY.to_owned();
    }
    let actor = actor_id(interaction);
    if actor.is_empty() {
        return AUTHORITY_UNAVAILABLE_REPLY.to_owned();
    }
    let guild_id = configured_guild.to_string();
    if let Err(reply) = recheck_staff(executor, &guild_id, &actor).await {
        return reply;
    }
    match read_recent_moderation(pool, &guild_id).await {
        Ok(rows) => match render_page(&rows, page) {
            RecentView::Page(text) => text,
            RecentView::Empty => RECENT_EMPTY_REPLY.to_owned(),
            RecentView::OutOfRange => RECENT_INVALID_PAGE_REPLY.to_owned(),
        },
        Err(err) => {
            tracing::warn!(
                msg = "moderation_recent_read_failed",
                reason = db_class(&err),
                "recent moderation ledger read failed"
            );
            RECENT_UNAVAILABLE_REPLY.to_owned()
        }
    }
}

/// Complete the already-deferred interaction with one page. The edit keeps
/// reason text literal for staff; mention safety is the edit's explicit
/// empty `allowed_mentions` plus the code-span rendering.
pub(crate) async fn handle_command(
    pool: &PgPool,
    configured_guild: u64,
    executor: &ActionExecutor,
    interaction: &Interaction,
) {
    let reply = reply_for(pool, configured_guild, executor, interaction).await;
    if let Err(err) = executor
        .edit_interaction_response_without_mention_neutralization(
            interaction.application_id.get(),
            &interaction.token,
            &reply,
        )
        .await
    {
        tracing::warn!(
            interaction_id = %interaction.id.get(),
            error = %err,
            "moderation recent reply edit failed"
        );
    }
}
