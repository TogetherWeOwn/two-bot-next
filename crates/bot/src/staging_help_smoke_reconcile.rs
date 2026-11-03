//! Staging-only boot reconcile for the `/help` embed smoke channel.
//!
//! The staging smoke needs a `help-embed-smoke` text channel in the TWO
//! Staging guild where the staging test bot holds a member overwrite of
//! View Channel + Send Messages + Embed Links + Read Message History. No
//! human provisions it in the Discord UI: the staging product bot (a guild
//! admin) reconciles the channel at boot from the token it already holds.
//!
//! Double-gated and idempotent: runs only when `TWO_DEPLOY_ENV` is exactly
//! `"staging"` and the configured guild is the pinned staging guild.
//! Every other combination logs at debug and returns, so production can
//! never create this channel. Token-only (no database); every error is
//! logged and boot continues.

use std::time::Duration;

use twilight_http::Client as TwilightClient;
use twilight_model::channel::permission_overwrite::{
    PermissionOverwrite as ChannelPermissionOverwrite,
    PermissionOverwriteType as ChannelPermissionOverwriteType,
};
use twilight_model::channel::{Channel, ChannelType};
use twilight_model::guild::{Member, Permissions};
use twilight_model::http::permission_overwrite::{
    PermissionOverwrite as HttpPermissionOverwrite,
    PermissionOverwriteType as HttpPermissionOverwriteType,
};
use twilight_model::id::{
    marker::{ChannelMarker, GuildMarker, UserMarker},
    Id,
};

/// TWO Staging guild, parsed from the allowlisted core identity constant
/// (`two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID`). Deliberately
/// not restated as a literal here: the source-snowflake gate rejects new
/// deployment-id literals, and a single pinned value keeps the two fences
/// from drifting apart. Together with the `TWO_DEPLOY_ENV` marker it makes
/// it impossible for this reconcile to create a channel in production.
/// `None` only if the core constant ever stops parsing as a snowflake, in
/// which case the fence below stays closed.
#[must_use]
pub fn staging_guild_id() -> Option<u64> {
    two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID
        .parse()
        .ok()
}

/// Channel the staging smoke posts its brand read-back messages in.
pub const SMOKE_CHANNEL_NAME: &str = "help-embed-smoke";

/// Existing staging setting holding the candidate test-bot user ids.
pub const SYNTHETIC_IDS_VAR: &str = "TWO_TEMP_VOICE_SYNTHETIC_HUMAN_IDS";

/// Upper bound for the whole reconcile so a slow Discord never holds boot.
const RECONCILE_TIMEOUT: Duration = Duration::from_secs(30);

/// What the reconcile did. Logged as a short string on the boot info line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileOutcome {
    /// The channel did not exist and was created.
    Created,
    /// The channel already existed and no overwrite write was needed.
    AlreadyPresent,
    /// The channel already existed and a bot overwrite was written.
    OverwriteEnsured,
}

impl ReconcileOutcome {
    /// Fixed-vocabulary outcome for the boot log line.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            ReconcileOutcome::Created => "created",
            ReconcileOutcome::AlreadyPresent => "already_present",
            ReconcileOutcome::OverwriteEnsured => "overwrite_ensured",
        }
    }
}

/// Pure fence: the staging marker exactly `"staging"` AND the configured
/// guild exactly the staging guild. Every other combination is off,
/// including an unparseable staging constant (fail closed).
#[must_use]
pub fn reconcile_enabled(deploy_env: Option<&str>, guild_id: Option<u64>) -> bool {
    match (deploy_env, guild_id, staging_guild_id()) {
        (Some("staging"), Some(configured), Some(staging)) => configured == staging,
        _ => false,
    }
}

/// The fence read once from the process environment at boot.
#[must_use]
pub fn reconcile_enabled_from_env(guild_id: Option<u64>) -> bool {
    let deploy_env = std::env::var(crate::server::DEPLOY_ENV_VAR).ok();
    reconcile_enabled(deploy_env.as_deref(), guild_id)
}

/// Exact member-overwrite allow bits for the test bot: View Channel, Send
/// Messages, Embed Links and Read Message History. Nothing else.
#[must_use]
pub fn smoke_allow_bits() -> Permissions {
    Permissions::VIEW_CHANNEL
        | Permissions::SEND_MESSAGES
        | Permissions::EMBED_LINKS
        | Permissions::READ_MESSAGE_HISTORY
}

/// Lenient parse of the candidate-id setting: comma-separated, whitespace
/// tolerated, duplicates and non-snowflake entries dropped. Never fails —
/// an unusable setting means "no candidates", reported at the call site.
#[must_use]
pub fn parse_candidate_ids(raw: Option<&str>) -> Vec<u64> {
    let text = match raw {
        Some(text) => text,
        None => return Vec::new(),
    };
    let mut ids = Vec::new();
    for part in text.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() || !trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        match trimmed.parse::<u64>() {
            Ok(id) if id != 0 && !ids.contains(&id) => ids.push(id),
            _ => {}
        }
    }
    ids
}

/// Minimal channel view the find decision needs.
pub struct SmokeChannelView<'a> {
    pub id: u64,
    pub name: &'a str,
    pub kind: ChannelType,
}

/// Find the smoke channel: a guild text channel named exactly
/// [`SMOKE_CHANNEL_NAME`]. First match wins; anything else is ignored.
#[must_use]
pub fn find_smoke_channel_id(channels: &[SmokeChannelView<'_>]) -> Option<u64> {
    channels
        .iter()
        .find(|channel| {
            channel.kind == ChannelType::GuildText && channel.name == SMOKE_CHANNEL_NAME
        })
        .map(|channel| channel.id)
}

/// Bot ids whose member overwrite is missing or wrong, given existing
/// `(user_id, allow_bits, deny_bits)` member rows and the required bits
/// from [`smoke_allow_bits`]. A row covers a bot only when it allows every
/// required bit and denies none of them.
#[must_use]
pub fn bots_needing_overwrite(existing: &[(u64, u64, u64)], bot_ids: &[u64]) -> Vec<u64> {
    let required = smoke_allow_bits().bits();
    bot_ids
        .iter()
        .copied()
        .filter(|bot| {
            !existing.iter().any(|(id, allow, deny)| {
                *id == *bot && (allow & required) == required && (deny & required) == 0
            })
        })
        .collect()
}

/// Token-only entrypoint, called at boot after the command-registry sync.
/// Never blocks or fails boot: every path logs and returns.
pub async fn maybe_reconcile_on_boot(token: Option<&str>, guild_id: Option<u64>) {
    if !reconcile_enabled_from_env(guild_id) {
        tracing::debug!("staging help-embed smoke reconcile skipped: gate closed");
        return;
    }
    let token = match token {
        Some(token) if !token.is_empty() => token,
        _ => {
            tracing::error!("staging help-embed smoke reconcile skipped: DISCORD_TOKEN is missing");
            return;
        }
    };
    let guild = match guild_id {
        Some(guild) => guild,
        None => {
            tracing::error!("staging help-embed smoke reconcile skipped: guild is not configured");
            return;
        }
    };
    let candidates = std::env::var(SYNTHETIC_IDS_VAR).ok();
    match tokio::time::timeout(
        RECONCILE_TIMEOUT,
        reconcile_once(token, guild, candidates.as_deref(), None),
    )
    .await
    {
        Ok(Ok((channel_id, outcome))) => {
            tracing::info!(
                channel_id = %channel_id,
                outcome = outcome.as_str(),
                "staging help-embed smoke channel reconciled"
            );
        }
        Ok(Err(error)) => {
            tracing::error!(error = %error, "staging help-embed smoke reconcile failed");
        }
        Err(_) => {
            tracing::error!("staging help-embed smoke reconcile timed out");
        }
    }
}

/// Build the token-only REST client. `proxy_override` (or `DISCORD_API_BASE`
/// when `None`) points the client at a loopback double in tests.
fn client_for(token: &str, proxy_override: Option<&str>) -> TwilightClient {
    let mut builder = TwilightClient::builder()
        .token(token.to_owned())
        .ratelimiter(None);
    let proxy = proxy_override
        .map(str::to_owned)
        .or_else(|| std::env::var("DISCORD_API_BASE").ok());
    if let Some(base) = proxy {
        let base = base.trim().to_owned();
        if let Some(host) = base.strip_prefix("http://") {
            builder = builder.proxy(host.to_owned(), true);
        } else if let Some(host) = base.strip_prefix("https://") {
            builder = builder.proxy(host.to_owned(), false);
        }
    }
    builder.build()
}

/// One reconcile pass: find or create the channel, then ensure the bot
/// overwrite. Returns the channel id and what happened.
async fn reconcile_once(
    token: &str,
    guild: u64,
    candidate_raw: Option<&str>,
    proxy_override: Option<&str>,
) -> Result<(u64, ReconcileOutcome), String> {
    let guild_id = Id::<GuildMarker>::new_checked(guild)
        .ok_or_else(|| "staging help-embed smoke reconcile refused: invalid guild id".to_owned())?;
    let client = client_for(token, proxy_override);
    let listed: Vec<Channel> = client
        .guild_channels(guild_id)
        .await
        .map_err(|error| format!("staging smoke channel list failed: {error}"))?
        .model()
        .await
        .map_err(|error| format!("staging smoke channel list unreadable: {error}"))?;
    let views: Vec<SmokeChannelView<'_>> = listed
        .iter()
        .map(|channel| SmokeChannelView {
            id: channel.id.get(),
            name: channel.name.as_deref().unwrap_or(""),
            kind: channel.kind,
        })
        .collect();
    let bots = discover_bots(&client, guild_id, candidate_raw).await;
    if let Some(channel_id) = find_smoke_channel_id(&views) {
        return ensure_overwrite(&client, channel_id, &listed, &bots).await;
    }
    create_channel(&client, guild_id, &bots).await
}

/// Fetch each candidate guild member; keep the ones that are bots.
/// Unresolvable ids are skipped with a debug log, never fatal.
async fn discover_bots(
    client: &TwilightClient,
    guild_id: Id<GuildMarker>,
    candidate_raw: Option<&str>,
) -> Vec<u64> {
    let mut bots = Vec::new();
    for candidate in parse_candidate_ids(candidate_raw) {
        let user_id = match Id::<UserMarker>::new_checked(candidate) {
            Some(id) => id,
            None => continue,
        };
        match client.guild_member(guild_id, user_id).await {
            Ok(response) => {
                let member: Result<Member, _> = response.model().await;
                match member {
                    Ok(member) if member.user.bot => {
                        if !bots.contains(&candidate) {
                            bots.push(candidate);
                        }
                    }
                    Ok(_) => {
                        tracing::debug!(
                            user_id = %candidate,
                            "staging smoke reconcile: candidate is not a bot, skipped"
                        );
                    }
                    Err(error) => {
                        tracing::debug!(
                            user_id = %candidate,
                            error = %error,
                            "staging smoke reconcile: member unreadable, skipped"
                        );
                    }
                }
            }
            Err(error) => {
                tracing::debug!(
                    user_id = %candidate,
                    error = %error,
                    "staging smoke reconcile: member fetch failed, skipped"
                );
            }
        }
    }
    bots
}

/// Existing member-overwrite rows `(user_id, allow_bits, deny_bits)` for a
/// listed channel.
fn existing_member_rows(channel: &Channel) -> Vec<(u64, u64, u64)> {
    channel
        .permission_overwrites
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter(|row| row.kind == ChannelPermissionOverwriteType::Member)
        .map(|row| (row.id.get(), row.allow.bits(), row.deny.bits()))
        .collect()
}

/// Ensure the overwrite on an existing channel.
async fn ensure_overwrite(
    client: &TwilightClient,
    channel_id: u64,
    listed: &[Channel],
    bots: &[u64],
) -> Result<(u64, ReconcileOutcome), String> {
    if bots.is_empty() {
        tracing::error!(
            setting = SYNTHETIC_IDS_VAR,
            "staging smoke reconcile: setting holds no staging-guild bot member, overwrite skipped"
        );
        return Ok((channel_id, ReconcileOutcome::AlreadyPresent));
    }
    let existing = listed
        .iter()
        .find(|channel| channel.id.get() == channel_id)
        .map(existing_member_rows)
        .unwrap_or_default();
    let needed = bots_needing_overwrite(&existing, bots);
    if needed.is_empty() {
        return Ok((channel_id, ReconcileOutcome::AlreadyPresent));
    }
    let channel = Id::<ChannelMarker>::new_checked(channel_id)
        .ok_or_else(|| "staging smoke overwrite refused: invalid channel id".to_owned())?;
    for bot in &needed {
        let user = Id::<UserMarker>::new_checked(*bot)
            .ok_or_else(|| "staging smoke overwrite refused: invalid bot id".to_owned())?;
        let overwrite = HttpPermissionOverwrite {
            allow: Some(smoke_allow_bits()),
            deny: Some(Permissions::empty()),
            id: user.cast(),
            kind: HttpPermissionOverwriteType::Member,
        };
        client
            .update_channel_permission(channel, &overwrite)
            .await
            .map_err(|error| format!("staging smoke overwrite failed for {bot}: {error}"))?;
    }
    Ok((channel_id, ReconcileOutcome::OverwriteEnsured))
}

/// Create the channel, carrying the bot overwrites in the same POST.
async fn create_channel(
    client: &TwilightClient,
    guild_id: Id<GuildMarker>,
    bots: &[u64],
) -> Result<(u64, ReconcileOutcome), String> {
    if bots.is_empty() {
        tracing::error!(
            setting = SYNTHETIC_IDS_VAR,
            "staging smoke reconcile: setting holds no staging-guild bot member, channel created without bot overwrite"
        );
    }
    let overwrites: Vec<ChannelPermissionOverwrite> = bots
        .iter()
        .copied()
        .filter_map(Id::<UserMarker>::new_checked)
        .map(|user| ChannelPermissionOverwrite {
            allow: smoke_allow_bits(),
            deny: Permissions::empty(),
            id: user.cast(),
            kind: ChannelPermissionOverwriteType::Member,
        })
        .collect();
    let created: Channel = client
        .create_guild_channel(guild_id, SMOKE_CHANNEL_NAME)
        .kind(ChannelType::GuildText)
        .permission_overwrites(&overwrites)
        .await
        .map_err(|error| format!("staging smoke channel create failed: {error}"))?
        .model()
        .await
        .map_err(|error| format!("staging smoke channel create unreadable: {error}"))?;
    Ok((created.id.get(), ReconcileOutcome::Created))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use twilight_model::channel::ChannelType;

    fn view(id: u64, name: &str, kind: ChannelType) -> SmokeChannelView<'_> {
        SmokeChannelView { id, name, kind }
    }

    fn staging_guild() -> u64 {
        staging_guild_id().expect("core staging guild is a snowflake")
    }

    #[test]
    fn gate_needs_staging_marker_and_staging_guild() {
        let staging = staging_guild();
        for (deploy_env, guild_id, enabled) in [
            (None, None, false),
            (None, Some(staging), false),
            (Some("staging"), None, false),
            (Some("staging"), Some(0), false),
            (Some("staging"), Some(1), false),
            (Some("production"), Some(staging), false),
            (Some("Staging"), Some(staging), false),
            (Some("staging "), Some(staging), false),
            (Some(" staging"), Some(staging), false),
            (Some(""), Some(staging), false),
            (Some("staging"), Some(staging), true),
        ] {
            assert_eq!(
                reconcile_enabled(deploy_env, guild_id),
                enabled,
                "deploy_env={deploy_env:?} guild_id={guild_id:?}"
            );
        }
    }

    #[test]
    fn staging_guild_matches_core_const() {
        let core: u64 = two_bot_core::backup::guild_config::TWO_STAGING_GUILD_ID
            .parse()
            .expect("core staging guild is a snowflake");
        assert_eq!(staging_guild_id(), Some(core));
        assert!(core != 0);
    }

    #[test]
    fn allow_bits_are_exactly_the_four_smoke_grants() {
        let allow = smoke_allow_bits();
        assert!(allow.contains(Permissions::VIEW_CHANNEL));
        assert!(allow.contains(Permissions::SEND_MESSAGES));
        assert!(allow.contains(Permissions::EMBED_LINKS));
        assert!(allow.contains(Permissions::READ_MESSAGE_HISTORY));
        let expected = Permissions::VIEW_CHANNEL
            | Permissions::SEND_MESSAGES
            | Permissions::EMBED_LINKS
            | Permissions::READ_MESSAGE_HISTORY;
        assert_eq!(allow, expected);
        assert_eq!(allow.bits(), 1024 | 2048 | 16384 | 65536);
        assert!(!allow.contains(Permissions::MANAGE_CHANNELS));
        assert!(!allow.contains(Permissions::ADMINISTRATOR));
    }

    #[test]
    fn candidate_parsing_is_lenient_and_dedupes() {
        assert!(parse_candidate_ids(None).is_empty());
        assert!(parse_candidate_ids(Some("")).is_empty());
        assert!(parse_candidate_ids(Some("   ")).is_empty());
        assert_eq!(parse_candidate_ids(Some("123")), vec![123]);
        assert_eq!(parse_candidate_ids(Some(" 123 , 456 ")), vec![123, 456]);
        assert_eq!(parse_candidate_ids(Some("123,123,456")), vec![123, 456]);
        assert_eq!(parse_candidate_ids(Some("123,, ,456,")), vec![123, 456]);
        assert!(parse_candidate_ids(Some("0")).is_empty());
        assert!(parse_candidate_ids(Some("abc")).is_empty());
        assert_eq!(parse_candidate_ids(Some("12a,34")), vec![34]);
        assert_eq!(parse_candidate_ids(Some("-5,42")), vec![42]);
        assert!(parse_candidate_ids(Some("18446744073709551616")).is_empty());
    }

    #[test]
    fn find_needs_exact_text_name() {
        assert_eq!(find_smoke_channel_id(&[]), None);
        assert_eq!(
            find_smoke_channel_id(&[view(1, "general", ChannelType::GuildText)]),
            None
        );
        assert_eq!(
            find_smoke_channel_id(&[view(1, SMOKE_CHANNEL_NAME, ChannelType::GuildVoice)]),
            None
        );
        assert_eq!(
            find_smoke_channel_id(&[view(1, "Help-Embed-Smoke", ChannelType::GuildText)]),
            None
        );
        assert_eq!(
            find_smoke_channel_id(&[view(7, SMOKE_CHANNEL_NAME, ChannelType::GuildText)]),
            Some(7)
        );
        assert_eq!(
            find_smoke_channel_id(&[
                view(1, "general", ChannelType::GuildText),
                view(2, SMOKE_CHANNEL_NAME, ChannelType::GuildText),
            ]),
            Some(2)
        );
    }

    #[test]
    fn overwrite_needed_only_when_cover_is_missing() {
        let required = smoke_allow_bits().bits();
        assert_eq!(bots_needing_overwrite(&[], &[10]), vec![10]);
        assert_eq!(
            bots_needing_overwrite(&[(10, required, 0)], &[10]),
            Vec::<u64>::new()
        );
        assert_eq!(
            bots_needing_overwrite(&[(10, required & !2048, 0)], &[10]),
            vec![10]
        );
        assert_eq!(
            bots_needing_overwrite(&[(10, required, 1024)], &[10]),
            vec![10]
        );
        assert_eq!(
            bots_needing_overwrite(&[(11, required, 0)], &[10]),
            vec![10]
        );
        assert_eq!(
            bots_needing_overwrite(&[(10, required, 0), (11, 0, 0)], &[10, 11]),
            vec![11]
        );
    }

    #[test]
    fn outcome_strings_are_fixed_vocabulary() {
        assert_eq!(ReconcileOutcome::Created.as_str(), "created");
        assert_eq!(ReconcileOutcome::AlreadyPresent.as_str(), "already_present");
        assert_eq!(
            ReconcileOutcome::OverwriteEnsured.as_str(),
            "overwrite_ensured"
        );
    }

    #[test]
    fn gate_closed_boot_returns_without_network() {
        // TWO_DEPLOY_ENV is unset in test: only the debug arm runs.
        assert!(!reconcile_enabled_from_env(Some(staging_guild())));
    }

    fn member_doc(user_id: &str, bot: bool) -> serde_json::Value {
        json!({
            "deaf": false,
            "flags": 0,
            "joined_at": null,
            "mute": false,
            "roles": [],
            "user": {
                "id": user_id,
                "username": "synth",
                "discriminator": "0",
                "bot": bot,
            },
        })
    }

    fn channel_doc(
        id: &str,
        name: &str,
        kind: u8,
        overwrites: serde_json::Value,
    ) -> serde_json::Value {
        json!({
            "id": id,
            "guild_id": staging_guild().to_string(),
            "type": kind,
            "name": name,
            "permission_overwrites": overwrites,
        })
    }

    #[tokio::test]
    async fn missing_channel_is_created_with_bot_overwrite() {
        use crate::discord_test_common::{MockRest, ScriptedResponse};
        use twilight_model::guild::Permissions;

        let bot: u64 = 111222333444555666;
        let guild = staging_guild();
        let mock = MockRest::start(
            vec![
                ScriptedResponse::json(200, json!([channel_doc("111", "general", 0, json!([]))])),
                ScriptedResponse::json(200, member_doc(&bot.to_string(), true)),
                ScriptedResponse::json(201, channel_doc("999", SMOKE_CHANNEL_NAME, 0, json!([]))),
            ],
            ScriptedResponse::status(404),
        )
        .await;
        let result = reconcile_once(
            "reconcile-fixture-token",
            guild,
            Some(&bot.to_string()),
            Some(&mock.origin()),
        )
        .await;
        assert_eq!(result, Ok((999, ReconcileOutcome::Created)));
        let requests = mock.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(
            requests[0].path,
            format!("/api/v10/guilds/{guild}/channels")
        );
        assert_eq!(requests[1].method, "GET");
        assert_eq!(
            requests[1].path,
            format!("/api/v10/guilds/{guild}/members/{bot}")
        );
        assert_eq!(requests[2].method, "POST");
        assert_eq!(
            requests[2].path,
            format!("/api/v10/guilds/{guild}/channels")
        );
        let body: serde_json::Value =
            serde_json::from_slice(&requests[2].body).expect("create body is json");
        assert_eq!(body["name"], json!(SMOKE_CHANNEL_NAME));
        let overwrites = body["permission_overwrites"]
            .as_array()
            .expect("create carries overwrites");
        assert_eq!(overwrites.len(), 1);
        assert_eq!(overwrites[0]["id"], json!(bot.to_string()));
        assert_eq!(overwrites[0]["type"], json!(1));
        // Twilight serializes permission bitfields as strings on the wire.
        let allow = Permissions::VIEW_CHANNEL.bits()
            | Permissions::SEND_MESSAGES.bits()
            | Permissions::EMBED_LINKS.bits()
            | Permissions::READ_MESSAGE_HISTORY.bits();
        assert_eq!(overwrites[0]["allow"], json!(allow.to_string()));
        mock.shutdown().await;
    }

    #[tokio::test]
    async fn present_channel_gets_missing_bot_overwrite() {
        use crate::discord_test_common::{MockRest, ScriptedResponse};

        let bot: u64 = 222333444555666777;
        let mock = MockRest::start(
            vec![
                ScriptedResponse::json(
                    200,
                    json!([channel_doc("555", SMOKE_CHANNEL_NAME, 0, json!([]))]),
                ),
                ScriptedResponse::json(200, member_doc(&bot.to_string(), true)),
                ScriptedResponse::status(204),
            ],
            ScriptedResponse::status(404),
        )
        .await;
        let result = reconcile_once(
            "reconcile-fixture-token",
            staging_guild(),
            Some(&bot.to_string()),
            Some(&mock.origin()),
        )
        .await;
        assert_eq!(result, Ok((555, ReconcileOutcome::OverwriteEnsured)));
        let requests = mock.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[2].method, "PUT");
        assert_eq!(
            requests[2].path,
            format!("/api/v10/channels/555/permissions/{bot}")
        );
        mock.shutdown().await;
    }

    #[tokio::test]
    async fn present_channel_with_cover_needs_no_write() {
        use crate::discord_test_common::{MockRest, ScriptedResponse};

        let bot: u64 = 333444555666777888;
        let allow = smoke_allow_bits().bits();
        let mock = MockRest::start(
            vec![
                ScriptedResponse::json(
                    200,
                    json!([channel_doc(
                        "556",
                        SMOKE_CHANNEL_NAME,
                        0,
                        json!([{"id": bot.to_string(), "type": 1,
                                "allow": allow.to_string(), "deny": "0"}])
                    )]),
                ),
                ScriptedResponse::json(200, member_doc(&bot.to_string(), true)),
            ],
            ScriptedResponse::status(404),
        )
        .await;
        let result = reconcile_once(
            "reconcile-fixture-token",
            staging_guild(),
            Some(&bot.to_string()),
            Some(&mock.origin()),
        )
        .await;
        assert_eq!(result, Ok((556, ReconcileOutcome::AlreadyPresent)));
        assert_eq!(mock.requests().len(), 2);
        mock.shutdown().await;
    }

    #[tokio::test]
    async fn non_bot_candidates_are_skipped_but_bot_still_ensured() {
        use crate::discord_test_common::{MockRest, ScriptedResponse};

        let human: u64 = 444555666777888999;
        let bot: u64 = 555666777888999000;
        let mock = MockRest::start(
            vec![
                ScriptedResponse::json(
                    200,
                    json!([channel_doc("557", SMOKE_CHANNEL_NAME, 0, json!([]))]),
                ),
                ScriptedResponse::json(200, member_doc(&human.to_string(), false)),
                ScriptedResponse::json(200, member_doc(&bot.to_string(), true)),
                ScriptedResponse::status(204),
            ],
            ScriptedResponse::status(404),
        )
        .await;
        let result = reconcile_once(
            "reconcile-fixture-token",
            staging_guild(),
            Some(&format!("{human},{bot}")),
            Some(&mock.origin()),
        )
        .await;
        assert_eq!(result, Ok((557, ReconcileOutcome::OverwriteEnsured)));
        let requests = mock.requests();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[3].method, "PUT");
        assert!(requests[3].path.ends_with(&bot.to_string()));
        mock.shutdown().await;
    }

    #[tokio::test]
    async fn unresolvable_candidates_still_report_the_channel() {
        use crate::discord_test_common::{MockRest, ScriptedResponse};

        let mock = MockRest::start(
            vec![ScriptedResponse::json(
                200,
                json!([channel_doc("558", SMOKE_CHANNEL_NAME, 0, json!([]))]),
            )],
            ScriptedResponse::status(404),
        )
        .await;
        let result = reconcile_once(
            "reconcile-fixture-token",
            staging_guild(),
            None,
            Some(&mock.origin()),
        )
        .await;
        assert_eq!(result, Ok((558, ReconcileOutcome::AlreadyPresent)));
        assert_eq!(mock.requests().len(), 1);
        mock.shutdown().await;
    }
}
