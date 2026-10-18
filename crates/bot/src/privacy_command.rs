//! Member privacy self-serve (`/privacy opt-out` / `opt-in` / `status`;
//! parity §1 row 32).
//!
//! Thin wiring over the reviewed `two_bot_cutover::privacy` service: decode
//! the subcommand → confirm the guild plus the invoker identity → run the
//! service on the runtime pool → ephemeral completion. Routing (the `Always`
//! gate, the guild fence and the open-to-everyone permission row) happens in
//! the shared router before this runs, so the handler assumes an authorized
//! guild invocation and still refuses a guild-less interaction itself.
//!
//! The subject is always the invoker: the published definition carries no
//! target option, so one member can never purge another member's rows and no
//! caller-supplied ID ever reaches the service. The audit actor is the fixed
//! `member:self-serve` label — never a subject ID, count, content or secret
//! (the scope columns already name the pair, like every other receipt).
//!
//! Repeat taps never retake the purge's write-blocking locks: an
//! already-suppressed pair gets the already-opted-out reply, and a pair with
//! a purge receipt younger than the cooldown gets the cooldown reply. Either
//! early return writes no new receipt; only a fresh purge locks and deletes.

use sqlx::{Pool, Postgres};
use twilight_model::application::interaction::{
    application_command::CommandOptionValue, Interaction,
};
use two_bot_cutover::{
    db::CutoverDb,
    privacy::{analytics_settings, has_recent_opt_out, is_opted_out, opt_back_in, opt_out_member},
};
use two_bot_discord::ActionExecutor;

use crate::command_runtime::actor_id;
use crate::db_log::{db_class, discord_class};

/// Fixed audit actor for member self-serve: role attribution, never a
/// subject ID (see `docs/privacy.md`: the subject lives only in the scope
/// columns, never in the actor label).
const MEMBER_SELF_SERVE_ACTOR: &str = "member:self-serve";

/// Safe reply when the store fails — never leak sqlx internals to Discord.
const STORE_FAILURE_REPLY: &str = "Privacy command failed; try again.";

const GUILD_ONLY_REPLY: &str = "This command only works in a server.";

const UNKNOWN_MEMBER_REPLY: &str = "Could not identify you for this server; try again.";

const USAGE_REPLY: &str = "Use `/privacy opt-out`, `/privacy opt-in` or `/privacy status`.";

/// Purge cooldown for member self-serve: a pair with a purge receipt younger
/// than this window gets the cooldown reply instead of a fresh locking
/// purge, so alternating opt-in/opt-out cannot loop the write-blocking
/// locks. Assisted operator calls go through the service directly and are
/// unaffected.
const OPT_OUT_COOLDOWN_MINUTES: i32 = 10;

const ALREADY_OPTED_OUT_REPLY: &str = "You are already opted out of analytics for this server. \
     Your stored activity here was deleted and future activity will not be stored. \
     Use `/privacy opt-in` to resume.";

const OPT_OUT_COOLDOWN_REPLY: &str = "You recently changed your privacy setting; \
     try `/privacy opt-out` again in a few minutes.";

/// The `/privacy` subcommand the interaction invoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrivacyAction {
    OptOut,
    OptIn,
    Status,
    Invalid,
}

/// Decode the invoked subcommand: the first option names it
/// (`opt-out` / `opt-in` / `status`). Anything else — missing options, a
/// non-subcommand value, an unknown name — is `Invalid` and refuses before
/// any database access.
pub(crate) fn privacy_action(interaction: &Interaction) -> PrivacyAction {
    let Interaction {
        data:
            Some(twilight_model::application::interaction::InteractionData::ApplicationCommand(data)),
        ..
    } = interaction
    else {
        return PrivacyAction::Invalid;
    };
    let Some(first) = data.options.first() else {
        return PrivacyAction::Invalid;
    };
    if !matches!(first.value, CommandOptionValue::SubCommand(_)) {
        return PrivacyAction::Invalid;
    }
    match first.name.as_str() {
        "opt-out" => PrivacyAction::OptOut,
        "opt-in" => PrivacyAction::OptIn,
        "status" => PrivacyAction::Status,
        _ => PrivacyAction::Invalid,
    }
}

/// Complete the original ephemeral defer with `content`.
async fn finish(executor: &ActionExecutor, interaction: &Interaction, content: impl AsRef<str>) {
    if let Err(err) = executor
        .edit_interaction_response_with_blocked_retry(
            interaction.application_id.get(),
            &interaction.token,
            content.as_ref(),
        )
        .await
    {
        tracing::warn!(interaction_id = %interaction.id.get(), reason = discord_class(&err), "privacy reply edit failed");
    }
}

fn on_off(enabled: bool) -> &'static str {
    if enabled {
        "on"
    } else {
        "off"
    }
}

/// `/privacy`: run the invoked subcommand for the invoker in this guild.
/// The caller defers first; every path below completes the defer with one
/// ephemeral edit and writes nothing for anyone but the invoker.
pub(crate) async fn privacy_command(
    pool: &Pool<Postgres>,
    executor: &ActionExecutor,
    interaction: &Interaction,
) {
    let Some(guild_id) = interaction.guild_id.map(|id| id.get().to_string()) else {
        finish(executor, interaction, GUILD_ONLY_REPLY).await;
        return;
    };
    let member = actor_id(interaction);
    if member.is_empty() {
        finish(executor, interaction, UNKNOWN_MEMBER_REPLY).await;
        return;
    }
    // Borrow the runtime pool for one service call: no new connections, no
    // migrations, no fallback credential — exactly the pool the gateway runs.
    let db = CutoverDb::from_pool(pool.clone());
    match privacy_action(interaction) {
        PrivacyAction::OptOut => {
            // Cheap read-only guards first: either early return skips the
            // locking purge (and its receipt) entirely, so repeat taps and
            // rapid opt-in/opt-out loops cannot hold or queue the
            // SHARE ROW EXCLUSIVE locks on every invocation.
            match is_opted_out(&db, &guild_id, &member).await {
                Err(err) => {
                    tracing::warn!(reason = db_class(&err), "privacy opt-out guard failed");
                    finish(executor, interaction, STORE_FAILURE_REPLY).await;
                }
                Ok(true) => {
                    finish(executor, interaction, ALREADY_OPTED_OUT_REPLY).await;
                }
                Ok(false) => {
                    match has_recent_opt_out(&db, &guild_id, &member, OPT_OUT_COOLDOWN_MINUTES)
                        .await
                    {
                        Err(err) => {
                            tracing::warn!(
                                reason = db_class(&err),
                                "privacy opt-out cooldown check failed"
                            );
                            finish(executor, interaction, STORE_FAILURE_REPLY).await;
                        }
                        Ok(true) => {
                            finish(executor, interaction, OPT_OUT_COOLDOWN_REPLY).await;
                        }
                        Ok(false) => {
                            match opt_out_member(&db, &guild_id, &member, MEMBER_SELF_SERVE_ACTOR)
                                .await
                            {
                                Ok(_) => {
                                    finish(
                                        executor,
                                        interaction,
                                        "You are opted out of analytics for this server. \
                     Your stored activity here was deleted and future activity will not be stored. \
                     Moderation, safety and delivery records are unchanged. \
                     Use `/privacy opt-in` to resume.",
                                    )
                                    .await;
                                }
                                Err(err) => {
                                    tracing::warn!(
                                        reason = db_class(&err),
                                        "privacy opt-out failed"
                                    );
                                    finish(executor, interaction, STORE_FAILURE_REPLY).await;
                                }
                            }
                        }
                    }
                }
            }
        }
        PrivacyAction::OptIn => match opt_back_in(&db, &guild_id, &member).await {
            Ok(true) => {
                finish(
                    executor,
                    interaction,
                    "Analytics persistence is back on for this server. \
                     Rows deleted while you were opted out are not restored.",
                )
                .await;
            }
            Ok(false) => {
                finish(
                    executor,
                    interaction,
                    "You were not opted out; analytics persistence was already on for this server.",
                )
                .await;
            }
            Err(err) => {
                tracing::warn!(reason = db_class(&err), "privacy opt-in failed");
                finish(executor, interaction, STORE_FAILURE_REPLY).await;
            }
        },
        PrivacyAction::Status => {
            // Both reads are independent, so run them concurrently.
            let (suppressed, switches) = tokio::join!(
                is_opted_out(&db, &guild_id, &member),
                analytics_settings(&db, &guild_id)
            );
            match (suppressed, switches) {
                (Ok(suppressed), Ok((presence, activity, voice))) => {
                    finish(
                        executor,
                        interaction,
                        format!(
                            "Analytics for you in this server: {}. \
                             Server switches: presence {}, activity {}, voice {}.",
                            if suppressed { "opted out" } else { "on" },
                            on_off(presence),
                            on_off(activity),
                            on_off(voice),
                        ),
                    )
                    .await;
                }
                (Err(err), _) | (_, Err(err)) => {
                    tracing::warn!(reason = db_class(&err), "privacy status failed");
                    finish(executor, interaction, STORE_FAILURE_REPLY).await;
                }
            }
        }
        PrivacyAction::Invalid => {
            finish(executor, interaction, USAGE_REPLY).await;
        }
    }
}
