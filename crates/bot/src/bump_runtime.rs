//! DISBOARD-style bump reminders.
//!
//! Watches for successful server bumps posted by allowlisted bumper-bot
//! accounts, arms exactly one durable reminder per guild for the pinned
//! cooldown, and pings the opt-in bump role once at expiry.
//!
//! Config is process env (see [`BumpConfig`]). When the feature is disabled,
//! message observation is a no-op, status and role commands answer
//! [`disabled_reply`], and help still answers. When enabled without a role,
//! arming, sweeping and role edits stay off and role commands answer
//! [`role_unset_reply`].

use std::collections::HashMap;

use sqlx::PgPool;
use tracing::warn;
use twilight_model::application::interaction::{
    application_command::{CommandDataOption, CommandOptionValue},
    Interaction, InteractionData,
};
use twilight_model::channel::Message;
use two_bot_discord::ActionExecutor;

/// `TWO_BUMP_REMINDERS=1` enables the feature; anything else disables it.
pub(crate) const ENV_ENABLED: &str = "TWO_BUMP_REMINDERS";
/// Comma-separated allowlist of bumper-bot user IDs.
pub(crate) const ENV_BOT_IDS: &str = "TWO_BUMP_BOT_IDS";
/// Opt-in role to ping at expiry.
pub(crate) const ENV_ROLE_ID: &str = "TWO_BUMP_ROLE_ID";
/// Cooldown override in seconds; absent or unparseable means the default.
pub(crate) const ENV_COOLDOWN_SECS: &str = "TWO_BUMP_COOLDOWN_SECS";

/// Success marker: ASCII case-insensitive substring of an embed description.
/// Grounded in DISBOARD's "Bump done!" success reply, which independent
/// reminder bots match the same way (Ayako-v2, dank-vibes-bot, Seraphine,
/// Tanya-Degurechaff) against author `302050872383242240`.
pub(crate) const SUCCESS_MARKER: &str = "bump done";

/// Bump feature configuration, read from process env wherever the bump paths
/// run so production and test runtimes behave the same.
pub(crate) struct BumpConfig {
    pub enabled: bool,
    pub bot_ids: Vec<String>,
    pub role_id: Option<String>,
    pub cooldown_secs: u64,
}

impl BumpConfig {
    /// Build from an explicit var map (pure; unit-tested).
    /// `default_cooldown_secs` backs absent or unparseable overrides.
    pub fn from_map(vars: &HashMap<String, String>, default_cooldown_secs: u64) -> Self {
        let enabled = vars
            .get(ENV_ENABLED)
            .is_some_and(|value| value.trim() == "1");
        let bot_ids = vars
            .get(ENV_BOT_IDS)
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let role_id = vars
            .get(ENV_ROLE_ID)
            .map(|value| value.trim().to_owned())
            .filter(|role| !role.is_empty());
        let cooldown_secs = vars
            .get(ENV_COOLDOWN_SECS)
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(default_cooldown_secs);
        Self {
            enabled,
            bot_ids,
            role_id,
            cooldown_secs,
        }
    }

    /// Build from process env. Thin wrapper over [`BumpConfig::from_map`],
    /// read per call so each test pins its own values.
    pub fn from_env(default_cooldown_secs: u64) -> Self {
        let vars = [ENV_ENABLED, ENV_BOT_IDS, ENV_ROLE_ID, ENV_COOLDOWN_SECS]
            .into_iter()
            .filter_map(|key| std::env::var(key).ok().map(|value| (key.to_owned(), value)))
            .collect();
        Self::from_map(&vars, default_cooldown_secs)
    }

    /// Fully usable: enabled with a role to ping. Arming, sweeping and role
    /// edits require this; status and help need only `enabled`.
    pub fn operational(&self) -> bool {
        self.enabled && self.role_id.is_some()
    }
}

/// `/bump` subcommands. Unknown, missing or mistyped input falls back to
/// [`BumpAction::Help`] so the user always gets a useful answer.
pub(crate) enum BumpAction {
    OptIn,
    OptOut,
    Status,
    Help,
}

/// Decode the subcommand from interaction options, mirroring the privacy
/// command: the first option must be a subcommand, matched by name.
pub(crate) fn parse_bump_action(options: &[CommandDataOption]) -> BumpAction {
    let Some(first) = options.first() else {
        return BumpAction::Help;
    };
    if !matches!(first.value, CommandOptionValue::SubCommand(_)) {
        return BumpAction::Help;
    }
    match first.name.as_str() {
        "opt-in" => BumpAction::OptIn,
        "opt-out" => BumpAction::OptOut,
        "status" => BumpAction::Status,
        "help" => BumpAction::Help,
        _ => BumpAction::Help,
    }
}

/// Success predicate (contract-exact): the author is an allowlisted bot and at
/// least one embed description contains [`SUCCESS_MARKER`] (ASCII
/// case-insensitive). Cosmetics, content and attachments are ignored.
pub(crate) fn is_bump_success(
    author_id: &str,
    author_is_bot: bool,
    bot_ids: &[String],
    embed_descriptions: &[&str],
) -> bool {
    author_is_bot
        && bot_ids.iter().any(|id| id == author_id)
        && embed_descriptions
            .iter()
            .any(|description| description.to_ascii_lowercase().contains(SUCCESS_MARKER))
}

/// Armed status: carries `armed`, never the idle phrase, with a relative
/// Discord timestamp for the ping.
pub(crate) fn armed_reply(fires_at_epoch_secs: i64) -> String {
    format!("Bump reminder armed — pinging the bump role <t:{fires_at_epoch_secs}:R>.")
}

/// Idle status: carries the exact phrase `no bump reminder`.
pub(crate) fn idle_reply() -> String {
    "No bump reminder pending. Bump the server to start one.".to_owned()
}

/// Concise `/bump` help (one short screen).
pub(crate) fn help_reply() -> String {
    "Bump reminders:\n\
     /bump opt-in — get pinged when the server can be bumped again\n\
     /bump opt-out — stop the pings\n\
     /bump status — show the pending reminder\n\
     /bump help — this list"
        .to_owned()
}

/// Opt-in confirmation: names the opt-in.
pub(crate) fn opt_in_reply() -> String {
    "You're opted in — you'll get one ping when the next bump cooldown ends.".to_owned()
}

/// Opt-out confirmation: names the opt-out.
pub(crate) fn opt_out_reply() -> String {
    "You're opted out — no more bump pings.".to_owned()
}

/// Answer for status and role commands while the feature is disabled.
pub(crate) fn disabled_reply() -> String {
    "Bump reminders are off on this server.".to_owned()
}

/// Answer for role commands while enabled without a configured role.
pub(crate) fn role_unset_reply() -> String {
    "The bump role isn't set up yet — ask a moderator to finish setup.".to_owned()
}

/// Honest opt-in failure: the role PUT did not go through.
pub(crate) fn role_add_error_reply() -> String {
    "Adding the bump role didn't work right now — please try later.".to_owned()
}

/// Honest opt-out failure: the role DELETE did not go through.
pub(crate) fn role_remove_error_reply() -> String {
    "Removing the bump role didn't work right now — please try later.".to_owned()
}

/// Honest status failure: the pending row could not be read.
pub(crate) fn status_error_reply() -> String {
    "I can't read the reminder state right now — please try later.".to_owned()
}

/// Expiry ping: the role pill plus one plain sentence. The caller must send it
/// with `allowed_mentions.roles` set or Discord stays silent.
pub(crate) fn ping_content(role_id: &str) -> String {
    format!("<@&{role_id}> The bump cooldown is over — the server can be bumped again!")
}

/// Current Unix time in whole seconds; 0 only if the clock is unusable.
fn unix_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// Arm (or replace) the guild's pending reminder. A fresh bump overwrites the
/// row — including clearing a consumed flag — so reminders never stack.
pub(crate) async fn arm_bump_reminder(
    pool: &PgPool,
    guild_id: &str,
    channel_id: &str,
    fires_at_epoch_secs: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO bump_reminders (guild_id, channel_id, fires_at, fired)
         VALUES ($1, $2, to_timestamp($3::DOUBLE PRECISION), FALSE)
         ON CONFLICT (guild_id) DO UPDATE SET
           channel_id = excluded.channel_id,
           fires_at   = excluded.fires_at,
           fired      = FALSE",
    )
    .bind(guild_id)
    .bind(channel_id)
    .bind(fires_at_epoch_secs)
    .execute(pool)
    .await?;
    Ok(())
}

/// Read the pending reminder's fire time, if one is armed. Fires-at is not
/// consulted: an expired-but-unswept reminder still reads armed.
pub(crate) async fn pending_bump_reminder(
    pool: &PgPool,
    guild_id: &str,
) -> Result<Option<i64>, sqlx::Error> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT (extract(epoch FROM fires_at))::BIGINT
           FROM bump_reminders WHERE guild_id = $1 AND fired = FALSE",
    )
    .bind(guild_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(fires_at,)| fires_at))
}

/// Atomically claim one due reminder: the fired flip is the lock, so each
/// arming pings at most once across sweeps and restarts. Returns the channel
/// to ping when this caller won the claim.
pub(crate) async fn claim_due_bump_reminder(
    pool: &PgPool,
    guild_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(String,)> = sqlx::query_as(
        "UPDATE bump_reminders SET fired = TRUE
          WHERE guild_id = $1 AND fired = FALSE AND fires_at <= now()
          RETURNING channel_id",
    )
    .bind(guild_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(channel_id,)| channel_id))
}

/// Observe one message for bump activity: arm on bumper-bot success embeds,
/// then lazily sweep the guild's due reminder. No-ops unless the feature is
/// operational and the message is in the configured guild (DMs and foreign
/// guilds never arm and never error).
pub(crate) async fn observe_bump_message(
    pool: &PgPool,
    executor: &ActionExecutor,
    configured_guild_id: u64,
    default_cooldown_secs: u64,
    message: &Message,
) {
    let config = BumpConfig::from_env(default_cooldown_secs);
    if !config.operational() {
        return;
    }
    let Some(guild_id) = message.guild_id.map(|id| id.get()) else {
        return;
    };
    if guild_id != configured_guild_id {
        return;
    }
    let guild_id = guild_id.to_string();
    if is_bump_success(
        &message.author.id.get().to_string(),
        message.author.bot,
        &config.bot_ids,
        &message
            .embeds
            .iter()
            .filter_map(|embed| embed.description.as_deref())
            .collect::<Vec<_>>(),
    ) {
        let cooldown = i64::try_from(config.cooldown_secs).unwrap_or(i64::MAX);
        let fires_at = unix_now_secs().saturating_add(cooldown);
        if let Err(error) = arm_bump_reminder(
            pool,
            &guild_id,
            &message.channel_id.get().to_string(),
            fires_at,
        )
        .await
        {
            warn!(guild_id = %guild_id, reason = %error, "bump arm failed");
        }
    }
    match claim_due_bump_reminder(pool, &guild_id).await {
        Ok(Some(channel_id)) => {
            if let Some(role_id) = config.role_id.as_deref() {
                if let Err(error) = executor
                    .post_message_with_role_mention(&channel_id, &ping_content(role_id), role_id)
                    .await
                {
                    // Claimed rows stay consumed (at-most-once); the miss is logged.
                    warn!(guild_id = %guild_id, reason = %error, "bump ping failed after claim");
                }
            }
        }
        Ok(None) => {}
        Err(error) => {
            warn!(guild_id = %guild_id, reason = %error, "bump sweep failed");
        }
    }
}

/// Answer one `/bump` interaction with an immediate ephemeral reply.
pub(crate) async fn answer_bump_command(
    pool: &PgPool,
    executor: &ActionExecutor,
    configured_guild_id: u64,
    default_cooldown_secs: u64,
    interaction: &Interaction,
) {
    let config = BumpConfig::from_env(default_cooldown_secs);
    let Some(InteractionData::ApplicationCommand(data)) = interaction.data.as_ref() else {
        reply(executor, interaction, &help_reply()).await;
        return;
    };
    if !config.enabled {
        reply(executor, interaction, &disabled_reply()).await;
        return;
    }
    let guild_id = interaction
        .guild_id
        .map(|id| id.get())
        .unwrap_or(configured_guild_id)
        .to_string();
    match parse_bump_action(&data.options) {
        BumpAction::Help => reply(executor, interaction, &help_reply()).await,
        BumpAction::Status => match pending_bump_reminder(pool, &guild_id).await {
            Ok(Some(fires_at)) => reply(executor, interaction, &armed_reply(fires_at)).await,
            Ok(None) => reply(executor, interaction, &idle_reply()).await,
            Err(error) => {
                warn!(guild_id = %guild_id, reason = %error, "bump status read failed");
                reply(executor, interaction, &status_error_reply()).await;
            }
        },
        BumpAction::OptIn => {
            let Some(role_id) = config.role_id.as_deref() else {
                reply(executor, interaction, &role_unset_reply()).await;
                return;
            };
            let Some(member_id) = interaction.author_id().map(|id| id.get().to_string()) else {
                reply(executor, interaction, &role_add_error_reply()).await;
                return;
            };
            match executor
                .set_member_role(&guild_id, &member_id, role_id, true, "bump opt-in")
                .await
            {
                Ok(()) => reply(executor, interaction, &opt_in_reply()).await,
                Err(error) => {
                    warn!(guild_id = %guild_id, reason = %error, "bump opt-in failed");
                    reply(executor, interaction, &role_add_error_reply()).await;
                }
            }
        }
        BumpAction::OptOut => {
            let Some(role_id) = config.role_id.as_deref() else {
                reply(executor, interaction, &role_unset_reply()).await;
                return;
            };
            let Some(member_id) = interaction.author_id().map(|id| id.get().to_string()) else {
                reply(executor, interaction, &role_remove_error_reply()).await;
                return;
            };
            match executor
                .set_member_role(&guild_id, &member_id, role_id, false, "bump opt-out")
                .await
            {
                Ok(()) => reply(executor, interaction, &opt_out_reply()).await,
                Err(error) => {
                    warn!(guild_id = %guild_id, reason = %error, "bump opt-out failed");
                    reply(executor, interaction, &role_remove_error_reply()).await;
                }
            }
        }
    }
}

/// Immediate ephemeral interaction callback; failures are logged, never fatal.
async fn reply(executor: &ActionExecutor, interaction: &Interaction, text: &str) {
    if let Err(error) = executor
        .answer_interaction_with_blocked_retry(
            interaction.id.get(),
            &interaction.token,
            &crate::command_runtime::ephemeral(text),
        )
        .await
    {
        warn!(interaction_id = %interaction.id.get(), reason = %error, "bump reply failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Failure language banned from success-path replies (mirrors acceptance).
    const ERROR_MARKERS: [&str; 14] = [
        "unknown",
        "unavailable",
        "unrecognized",
        "not found",
        "no such",
        "failed",
        "failure",
        "went wrong",
        "try again",
        "error occurred",
        "couldn't",
        "could not",
        "unable to",
        "not implemented",
    ];

    /// Bumper internals banned from every reply and ping (mirrors acceptance;
    /// the test-only bumper ID is covered by never rendering IDs but the role).
    const INTERNALS: [&str; 6] = [
        "two_bump",
        "fires_at",
        "bump_reminders",
        "sqlx",
        "postgres",
        "stack trace",
    ];

    fn contains_ci(haystack: &str, needle: &str) -> bool {
        haystack.to_lowercase().contains(&needle.to_lowercase())
    }

    fn assert_clean(text: &str, context: &str) {
        for marker in ERROR_MARKERS {
            assert!(
                !contains_ci(text, marker),
                "{context} must not carry failure language {marker:?}: {text:?}"
            );
        }
        for marker in INTERNALS {
            assert!(
                !contains_ci(text, marker),
                "{context} must not leak internals {marker:?}: {text:?}"
            );
        }
    }

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn config_reads_full_map() {
        let config = BumpConfig::from_map(
            &vars(&[
                (ENV_ENABLED, "1"),
                (ENV_BOT_IDS, "111, 222 ,,"),
                (ENV_ROLE_ID, " 8888 "),
                (ENV_COOLDOWN_SECS, " 30 "),
            ]),
            7200,
        );
        assert!(config.enabled);
        assert_eq!(config.bot_ids, vec!["111".to_owned(), "222".to_owned()]);
        assert_eq!(config.role_id.as_deref(), Some("8888"));
        assert_eq!(config.cooldown_secs, 30);
        assert!(config.operational());
    }

    #[test]
    fn config_absent_map_means_disabled_defaults() {
        let config = BumpConfig::from_map(&vars(&[]), 7200);
        assert!(!config.enabled);
        assert!(config.bot_ids.is_empty());
        assert_eq!(config.role_id, None);
        assert_eq!(config.cooldown_secs, 7200);
        assert!(!config.operational());
    }

    #[test]
    fn config_unparseable_cooldown_falls_back_to_default() {
        for override_value in ["soon", "", "-5", "2.5"] {
            let config = BumpConfig::from_map(&vars(&[(ENV_COOLDOWN_SECS, override_value)]), 7200);
            assert_eq!(
                config.cooldown_secs, 7200,
                "override {override_value:?} must fall back"
            );
        }
    }

    #[test]
    fn config_operational_needs_enabled_and_role() {
        let enabled_no_role = BumpConfig::from_map(&vars(&[(ENV_ENABLED, "1")]), 7200);
        assert!(enabled_no_role.enabled);
        assert!(!enabled_no_role.operational());

        let disabled_with_role =
            BumpConfig::from_map(&vars(&[(ENV_ENABLED, "0"), (ENV_ROLE_ID, "8888")]), 7200);
        assert!(!disabled_with_role.enabled);
        assert!(!disabled_with_role.operational());
    }

    #[test]
    fn success_predicate_matches_contract_exactly() {
        let bots = vec!["111".to_owned()];
        // Allowlisted bot + marker, any case and cosmetics.
        assert!(is_bump_success(
            "111",
            true,
            &bots,
            &["Bump done! :thumbsup:"]
        ));
        assert!(is_bump_success("111", true, &bots, &["BUMP DONE! Thanks."]));
        assert!(is_bump_success(
            "111",
            true,
            &bots,
            &["nothing here", "bump done somewhere inside"]
        ));
        // Every other combination refuses.
        assert!(!is_bump_success("999", true, &bots, &["Bump done!"]));
        assert!(!is_bump_success("111", false, &bots, &["Bump done!"]));
        assert!(!is_bump_success(
            "111",
            true,
            &bots,
            &["Please wait another 42 minutes."]
        ));
        assert!(!is_bump_success("111", true, &bots, &[]));
        assert!(!is_bump_success("111", true, &[], &["Bump done!"]));
    }

    fn subcommand_option(name: &str) -> CommandDataOption {
        CommandDataOption {
            name: name.to_owned(),
            value: CommandOptionValue::SubCommand(Vec::new()),
        }
    }

    #[test]
    fn subcommand_parse_routes_all_four() {
        assert!(matches!(
            parse_bump_action(&[subcommand_option("opt-in")]),
            BumpAction::OptIn
        ));
        assert!(matches!(
            parse_bump_action(&[subcommand_option("opt-out")]),
            BumpAction::OptOut
        ));
        assert!(matches!(
            parse_bump_action(&[subcommand_option("status")]),
            BumpAction::Status
        ));
        assert!(matches!(
            parse_bump_action(&[subcommand_option("help")]),
            BumpAction::Help
        ));
    }

    #[test]
    fn subcommand_parse_falls_back_to_help() {
        assert!(matches!(parse_bump_action(&[]), BumpAction::Help));
        assert!(matches!(
            parse_bump_action(&[subcommand_option("frobnicate")]),
            BumpAction::Help
        ));
        let string_option = CommandDataOption {
            name: "status".to_owned(),
            value: CommandOptionValue::String("status".to_owned()),
        };
        assert!(matches!(
            parse_bump_action(&[string_option]),
            BumpAction::Help
        ));
    }

    #[test]
    fn armed_reply_marks_armed_state() {
        let reply = armed_reply(1_789_000_000);
        assert!(
            contains_ci(&reply, "armed"),
            "armed reply must carry `armed`: {reply:?}"
        );
        assert!(
            !contains_ci(&reply, "no bump reminder"),
            "armed reply must not read idle: {reply:?}"
        );
        assert!(
            reply.contains("<t:1789000000:R>"),
            "armed reply shows the relative ping time: {reply:?}"
        );
        assert_clean(&reply, "armed");
    }

    #[test]
    fn idle_reply_marks_idle_state() {
        let reply = idle_reply();
        assert!(
            contains_ci(&reply, "no bump reminder"),
            "idle reply must carry `no bump reminder`: {reply:?}"
        );
        assert_clean(&reply, "idle");
    }

    #[test]
    fn help_reply_is_concise_and_names_bump() {
        let reply = help_reply();
        assert!(
            contains_ci(&reply, "bump"),
            "help must describe the bump commands: {reply:?}"
        );
        assert!(
            reply.len() <= 1000,
            "concise help fits one message, got {} chars",
            reply.len()
        );
        assert_clean(&reply, "help");
    }

    #[test]
    fn confirmations_name_the_opt() {
        let opt_in = opt_in_reply();
        assert!(
            contains_ci(&opt_in, "opt"),
            "opt-in must name the opt-in: {opt_in:?}"
        );
        assert_clean(&opt_in, "opt-in");

        let opt_out = opt_out_reply();
        assert!(
            contains_ci(&opt_out, "opt"),
            "opt-out must name the opt-out: {opt_out:?}"
        );
        assert_clean(&opt_out, "opt-out");
    }

    #[test]
    fn fallback_replies_stay_clean() {
        for (name, text) in [
            ("disabled", disabled_reply()),
            ("role-unset", role_unset_reply()),
            ("role-add-error", role_add_error_reply()),
            ("role-remove-error", role_remove_error_reply()),
            ("status-error", status_error_reply()),
        ] {
            assert!(!text.is_empty(), "{name} reply must not be empty");
            assert_clean(&text, name);
        }
    }

    #[test]
    fn ping_carries_only_the_role_pill() {
        let content = ping_content("8888");
        assert!(
            content.contains("<@&8888>"),
            "ping must carry the role pill: {content:?}"
        );
        assert!(
            !content.contains("@everyone") && !content.contains("@here"),
            "ping must not blast the server: {content:?}"
        );
        let mut rest = content.as_str();
        while let Some(at) = rest.find("<@") {
            rest = &rest[at + 2..];
            assert!(
                rest.starts_with("&8888>"),
                "ping must mention only the bump role: {content:?}"
            );
        }
        assert_clean(&content, "ping");
    }
}
