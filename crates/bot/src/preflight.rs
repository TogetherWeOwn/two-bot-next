//! Read-only pre-deploy checks. This path never starts the gateway or opens a DB.

use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
};

use serde::{Deserialize, Serialize};
use twilight_gateway::Intents;
use twilight_http::{error::ErrorType, Client};
use twilight_model::{channel::ChannelType, guild::Permissions, id::Id, oauth::ApplicationFlags};
use two_bot_core::onboarding::{
    game_picker_allowed, level_role_writes_allowed, OnboardingGates, GAME_PICKS, PLATFORM_PICKS,
    TWO_GUILD_ID,
};
use two_bot_discord::channel_access::{guild_permissions, resolve_channel_access};

pub const USAGE: &str = "\
  two-bot preflight [--json] [--level-role-ids CSV]
      Read-only Discord REST credential, intent, role hierarchy and configured
      channel checks. No gateway, database, or permission changes.
      Env: DISCORD_TOKEN, GUILD_ID, feature/channel configuration.
      --level-role-ids: exported level_role_rewards role IDs (empty CSV means
      no rewards). Required when onboarding permits level-role writes.
      Exit 0: PASS/WARN only; 1: FAIL; 2: invalid CLI/configuration.
      See docs/preflight.md for coverage and interpretation.
";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
enum Status {
    Pass,
    Warn,
    Fail,
}

#[derive(Serialize)]
struct Check {
    status: Status,
    check: String,
    detail: String,
}

impl Check {
    fn fail(check: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status: Status::Fail,
            check: check.into(),
            detail: detail.into(),
        }
    }
}

#[derive(Default, Serialize)]
struct Report {
    checks: Vec<Check>,
}

impl Report {
    fn add(&mut self, status: Status, check: impl Into<String>, detail: impl Into<String>) {
        self.checks.push(Check {
            status,
            check: check.into(),
            detail: detail.into(),
        });
    }

    fn require(&mut self, ok: bool, check: impl Into<String>, detail: impl Into<String>) {
        self.add(if ok { Status::Pass } else { Status::Fail }, check, detail);
    }

    fn render(&self, json: bool, code: i32) {
        let failures = self
            .checks
            .iter()
            .filter(|check| check.status == Status::Fail)
            .count();
        let warnings = self
            .checks
            .iter()
            .filter(|check| check.status == Status::Warn)
            .count();
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "schema_version": 1, "ready": code == 0, "exit_code": code,
                    "failures": failures, "warnings": warnings, "checks": self.checks
                })
            );
        } else {
            println!("TWO bot preflight\nSTATUS  CHECK                           DETAIL");
            for check in &self.checks {
                let status = match check.status {
                    Status::Pass => "PASS",
                    Status::Warn => "WARN",
                    Status::Fail => "FAIL",
                };
                // Never echo untrusted REST names or configuration values to the terminal.
                println!("{status:<6}  {:<30}  {}", check.check, check.detail);
            }
            println!(
                "\n{}  {failures} fail, {warnings} warn",
                if code == 0 {
                    "Ready for checked configuration."
                } else {
                    "NOT ready."
                }
            );
        }
    }
}

#[derive(Default)]
struct ChannelTarget {
    post: bool,
    moderate: bool,
    // View-only references can be text, forum, category or voice destinations.
    text_only: bool,
}

struct Targets {
    guild_id: u64,
    channels: BTreeMap<u64, ChannelTarget>,
    roles: BTreeMap<u64, &'static str>,
    level_roles_known: bool,
    role_writes: bool,
}

#[derive(Deserialize)]
struct Panel {
    #[serde(rename = "channelId")]
    channel_id: String,
    options: Vec<PanelOption>,
}

#[derive(Deserialize)]
struct PanelOption {
    #[serde(rename = "roleId")]
    role_id: String,
}

fn snowflake(raw: &str) -> Result<u64, &'static str> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("expected a nonzero Discord snowflake");
    }
    raw.parse::<u64>()
        .ok()
        .filter(|id| *id != 0)
        .ok_or("expected a nonzero Discord snowflake")
}

fn ids(raw: &str) -> Result<Vec<u64>, &'static str> {
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    raw.split(',')
        .map(|value| snowflake(value.trim()))
        .collect()
}

fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
}

impl Targets {
    fn from_env(level_ids: Option<&str>) -> Result<Self, &'static str> {
        Self::from_map(&std::env::vars().collect(), level_ids)
    }

    fn from_map(
        vars: &HashMap<String, String>,
        level_ids: Option<&str>,
    ) -> Result<Self, &'static str> {
        let env = |key: &str| vars.get(key).cloned().unwrap_or_default();
        let guild = env("GUILD_ID");
        let guild = if guild.is_empty() {
            env("DISCORD_GUILD_ID")
        } else {
            guild
        };
        let guild_id =
            snowflake(&guild).map_err(|_| "GUILD_ID must be a nonzero Discord snowflake")?;
        let gates = OnboardingGates::from_map(vars).map_err(|_| "invalid TWO_ONBOARDING_MODE")?;
        let role_writes = level_role_writes_allowed(gates.mode) && !gates.dry_run;
        let mut targets = Self {
            guild_id,
            channels: BTreeMap::new(),
            roles: BTreeMap::new(),
            level_roles_known: !role_writes || level_ids.is_some(),
            role_writes,
        };
        if let Some(raw) = level_ids {
            let rewards = ids(raw).map_err(|_| "invalid --level-role-ids CSV")?;
            if role_writes {
                for id in rewards {
                    targets.roles.insert(id, "level reward");
                }
            }
        }

        // All named channel IDs in the settings catalogue; exemption/protection
        // lists are still checked for existence and View, but not for posting.
        for key in [
            "DISCORD_ANCHOR_WELCOME_CHANNEL_ID",
            "DISCORD_AUDIT_LOG_CHANNEL_ID",
            "DISCORD_GOODBYE_CHANNEL_IDS",
            "DISCORD_LANDING_CHANNEL_IDS",
            "DISCORD_MODERATION_LOG_CHANNEL_ID",
            "DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID",
            "DISCORD_STAFF_ALERT_CHANNEL_ID",
            "DISCORD_TICKET_PANEL_CHANNEL_ID",
            "DISCORD_VOICE_LOG_CHANNEL_ID",
            "TWO_TEMP_VOICE_PANEL_CHANNEL_ID",
        ] {
            for id in ids(&env(key)).map_err(|_| "invalid configured posting channel ID")? {
                targets.channel(id, true, true);
            }
        }
        for key in [
            "DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID",
            "DISCORD_TICKET_CATEGORY_ID",
            "TWO_TEMP_VOICE_GENERATOR_CHANNEL_ID",
            "TWO_TEMP_VOICE_CATEGORY_ID",
            "TWO_TEMP_VOICE_PROTECTED_CHANNEL_IDS",
            "TWO_AUTOMOD_EXEMPT_CHANNEL_IDS",
            "TWO_COMMUNITY_HUMAN_CHANNEL_IDS",
            "TWO_COMMUNITY_WELCOME_CHANNEL_IDS",
        ] {
            for id in ids(&env(key)).map_err(|_| "invalid configured channel reference")? {
                targets.channel(id, false, false);
            }
        }
        let raw = env("TWO_SELF_ROLE_PANELS");
        if !raw.trim().is_empty() {
            let panels: Vec<Panel> = serde_json::from_str(&raw)
                .map_err(|_| "invalid TWO_SELF_ROLE_PANELS JSON catalogue")?;
            for panel in panels {
                if panel.options.is_empty() {
                    return Err("self-role panels require options");
                }
                targets.channel(
                    snowflake(&panel.channel_id)
                        .map_err(|_| "invalid self-role panel channel ID")?,
                    true,
                    true,
                );
                for option in panel.options {
                    targets.roles.insert(
                        snowflake(&option.role_id)
                            .map_err(|_| "invalid self-role option role ID")?,
                        "self role",
                    );
                }
            }
        }
        // The shipped game catalogue is production deployment data. Session mode
        // is roleless; never compare production roles to an unrelated test guild.
        if game_picker_allowed(gates.mode) && !gates.dry_run && guild_id.to_string() == TWO_GUILD_ID
        {
            for pick in GAME_PICKS.iter().chain(PLATFORM_PICKS.iter()) {
                targets
                    .roles
                    .insert(snowflake(pick.role_id)?, "onboarding game/platform");
                if let Some(id) = pick.primary_channel_id {
                    targets.channel(snowflake(id)?, false, false);
                }
                targets.channel(snowflake(pick.fallback_channel_id)?, false, false);
            }
        }
        if env("TWO_AUTOMOD") == "1" {
            let exempt = ids(&env("TWO_AUTOMOD_EXEMPT_CHANNEL_IDS"))?;
            for (&id, target) in &mut targets.channels {
                target.moderate = !exempt.contains(&id);
            }
        }
        Ok(targets)
    }

    fn channel(&mut self, id: u64, post: bool, text_only: bool) {
        let target = self.channels.entry(id).or_default();
        target.post |= post;
        target.text_only |= text_only;
    }
}

// Only status codes escape Twilight errors: bodies, URLs and tokens must not.
fn rest_error(check: &'static str, error: twilight_http::Error) -> Check {
    let detail = match error.kind() {
        ErrorType::Response { status, .. } => {
            format!("Discord HTTP {} (checks stopped)", status.get())
        }
        _ => "Discord request failed (checks stopped)".to_owned(),
    };
    Check::fail(check, detail)
}

async fn check_discord(
    client: &Client,
    targets: &Targets,
    intents: Intents,
    report: &mut Report,
) -> Result<(), Check> {
    let user = client
        .current_user()
        .await
        .map_err(|error| rest_error("token", error))?
        .model()
        .await
        .map_err(|_| Check::fail("token", "invalid Discord user response"))?;
    report.add(Status::Pass, "token", format!("bot ID {}", user.id));
    let app = client
        .current_user_application()
        .await
        .map_err(|error| rest_error("application", error))?
        .model()
        .await
        .map_err(|_| Check::fail("application", "invalid Discord application response"))?;
    report.add(
        Status::Pass,
        "application",
        format!("application ID {}", app.id),
    );
    let flags = app.flags.unwrap_or_else(ApplicationFlags::empty);
    // Source: https://docs.discord.com/developers/resources/application#application-object-application-flags
    for (name, requested, enabled) in [
        (
            "Guild Members intent",
            intents.contains(Intents::GUILD_MEMBERS),
            flags.intersects(
                ApplicationFlags::GATEWAY_GUILD_MEMBERS
                    | ApplicationFlags::GATEWAY_GUILD_MEMBERS_LIMITED,
            ),
        ),
        (
            "Message Content intent",
            intents.contains(Intents::MESSAGE_CONTENT),
            flags.intersects(
                ApplicationFlags::GATEWAY_MESSAGE_CONTENT
                    | ApplicationFlags::GATEWAY_MESSAGE_CONTENT_LIMITED,
            ),
        ),
    ] {
        let status = if requested && !enabled {
            Status::Fail
        } else if !requested && enabled {
            Status::Warn
        } else {
            Status::Pass
        };
        report.add(
            status,
            name,
            format!("runtime requested={requested}, portal enabled={enabled}"),
        );
    }
    let guild_id = Id::new(targets.guild_id);
    let member = client
        .guild_member(guild_id, user.id)
        .await
        .map_err(|error| rest_error("guild membership", error))?
        .model()
        .await
        .map_err(|_| Check::fail("guild membership", "invalid Discord member response"))?;
    let roles = client
        .roles(guild_id)
        .await
        .map_err(|error| rest_error("guild roles", error))?
        .models()
        .await
        .map_err(|_| Check::fail("guild roles", "invalid Discord role response"))?;
    if !roles.iter().any(|role| role.id.get() == targets.guild_id)
        || member
            .roles
            .iter()
            .any(|id| !roles.iter().any(|role| role.id == *id))
    {
        return Err(Check::fail(
            "guild roles",
            "incomplete role snapshot; cannot resolve permissions",
        ));
    }
    report.add(
        Status::Pass,
        "guild membership",
        format!("guild ID {}", targets.guild_id),
    );
    let base = guild_permissions(targets.guild_id, &member.roles, &roles);
    let admin = base.contains(Permissions::ADMINISTRATOR);
    if admin {
        report.add(
            Status::Warn,
            "Administrator",
            "bypasses channel overwrites, NOT role hierarchy; trim unnecessary access separately",
        );
    }
    // Legacy funnel + internal-action grant (preflight.ts), with Administrator
    // treated as granting permissions but always called out independently.
    for (permission, name) in [
        (Permissions::MANAGE_GUILD, "Manage Server"),
        (Permissions::VIEW_CHANNEL, "View Channels"),
        (Permissions::CREATE_INVITE, "Create Instant Invite"),
        (Permissions::MANAGE_ROLES, "Manage Roles"),
        (Permissions::MANAGE_EVENTS, "Manage Events"),
        (Permissions::SEND_MESSAGES, "Send Messages"),
    ] {
        report.require(
            admin || base.contains(permission),
            name,
            "guild-level funnel/internal-action permission",
        );
    }
    let invites = client
        .guild_invites(guild_id)
        .await
        .map_err(|error| rest_error("invite list", error))?
        .models()
        .await
        .map_err(|_| Check::fail("invite list", "invalid Discord invite response"))?;
    report.add(
        Status::Pass,
        "invite list",
        format!("{} invites readable", invites.len()),
    );
    report.require(targets.level_roles_known, "level reward coverage", if targets.role_writes { "supply --level-role-ids from the guild's level_role_rewards, including an explicit empty CSV when none" } else { "role writes disabled by onboarding mode/dry-run" });
    let highest = roles
        .iter()
        .filter(|role| role.id.get() == targets.guild_id || member.roles.contains(&role.id))
        .max();
    for (&id, source) in &targets.roles {
        // Twilight Role::Ord handles equal-position roles by snowflake order.
        let manageable = roles
            .iter()
            .find(|role| role.id.get() == id)
            .is_some_and(|role| {
                id != targets.guild_id
                    && !role.managed
                    && highest.is_some_and(|highest| role < highest)
                    && (admin || base.contains(Permissions::MANAGE_ROLES))
            });
        report.require(
            manageable,
            format!("role {id}"),
            format!("{source}: must exist, be unmanaged, and below the bot's highest role"),
        );
    }
    if targets.channels.is_empty() {
        report.add(
            Status::Warn,
            "channel coverage",
            "no configured channel IDs; feature destinations were not checked",
        );
    }
    for (&id, target) in &targets.channels {
        let channel = client
            .channel(Id::new(id))
            .await
            .map_err(|error| rest_error("configured channel", error))?
            .model()
            .await
            .map_err(|_| Check::fail("configured channel", "invalid Discord channel response"))?;
        let correct_guild = channel.guild_id == Some(guild_id);
        let text = matches!(
            channel.kind,
            ChannelType::GuildText | ChannelType::GuildAnnouncement
        );
        let access = resolve_channel_access(
            targets.guild_id,
            user.id.get(),
            &member.roles,
            &roles,
            channel.permission_overwrites.as_deref().unwrap_or_default(),
        );
        let usable = correct_guild
            && (!target.text_only || text)
            && access.view
            && (!target.post || (access.send && access.embed))
            && (!target.moderate || !text || access.manage_messages);
        report.require(usable, format!("channel {id}"), format!(
            "guild={correct_guild} type={:?} View={} Send={} Embed={} ManageMessages={} required: View{}{}",
            channel.kind, access.view, access.send, access.embed, access.manage_messages,
            if target.post { "/Send/Embed (text or announcement)" } else { "" },
            if target.moderate && text { "/ManageMessages" } else { "" },
        ));
    }
    Ok(())
}

pub async fn dispatch(args: &[String]) -> i32 {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{USAGE}");
        return 0;
    }
    let json = args.iter().any(|arg| arg == "--json");
    let mut report = Report::default();
    let mut level_ids = None;
    let mut position = 0;
    while position < args.len() {
        match args[position].as_str() {
            "--json" => {}
            "--level-role-ids" if level_ids.is_none() && position + 1 < args.len() => {
                position += 1;
                level_ids = Some(args[position].as_str());
            }
            _ => {
                report.checks.push(Check::fail(
                    "arguments",
                    "use preflight --help for supported options",
                ));
                report.render(json, 2);
                return 2;
            }
        }
        position += 1;
    }
    let targets = match Targets::from_env(level_ids) {
        Ok(targets) => targets,
        Err(error) => {
            report.checks.push(Check::fail("configuration", error));
            report.render(json, 2);
            return 2;
        }
    };
    // The alias is a legacy fallback for an absent primary only. A
    // present-but-empty primary is a configuration error, never permission
    // to try another credential.
    let token = match std::env::var("DISCORD_TOKEN") {
        Ok(token) => token,
        Err(std::env::VarError::NotPresent) => env("DISCORD_BOT_TOKEN"),
        Err(_) => String::new(),
    };
    if token.trim().is_empty() {
        report
            .checks
            .push(Check::fail("token", "set DISCORD_TOKEN"));
        report.render(json, 2);
        return 2;
    }
    let mut builder = Client::builder().token(token);
    let proxy = env("DISCORD_PREFLIGHT_API_BASE");
    if !proxy.is_empty() {
        // A credential-bearing test seam must not redirect to an arbitrary host.
        let address = proxy
            .strip_prefix("http://")
            .and_then(|raw| raw.parse::<SocketAddr>().ok())
            .filter(|address| address.ip().is_loopback() && address.port() != 0);
        let Some(address) = address else {
            report.checks.push(Check::fail(
                "test endpoint",
                "DISCORD_PREFLIGHT_API_BASE must be a loopback http://IP:port",
            ));
            report.render(json, 2);
            return 2;
        };
        builder = builder.proxy(address.to_string(), true);
    }
    if let Err(check) = check_discord(
        &builder.build(),
        &targets,
        crate::gateway::intents_from_env(),
        &mut report,
    )
    .await
    {
        report.checks.push(check);
    }
    let code = i32::from(
        report
            .checks
            .iter()
            .any(|check| check.status == Status::Fail),
    );
    report.render(json, code);
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(guild: &str, mode: &str) -> HashMap<String, String> {
        [
            ("GUILD_ID".into(), guild.into()),
            ("TWO_ONBOARDING_MODE".into(), mode.into()),
        ]
        .into()
    }

    #[test]
    fn onboarding_catalogue_is_guild_scoped_and_roleless_session_is_honored() {
        for (guild, mode, expected_roles) in [
            (TWO_GUILD_ID, "legacy", 15),
            (TWO_GUILD_ID, "anchor", 15),
            (TWO_GUILD_ID, "session", 0),
            ("2222", "legacy", 0),
        ] {
            let targets = Targets::from_map(&vars(guild, mode), Some("")).unwrap();
            assert_eq!(targets.roles.len(), expected_roles, "{guild}/{mode}");
            if expected_roles > 0 {
                assert_eq!(targets.channels.len(), 4);
            }
        }
        let mut env = vars(TWO_GUILD_ID, "legacy");
        env.insert("TWO_ONBOARDING_DRY_RUN".into(), "1".into());
        let targets = Targets::from_map(&env, None).unwrap();
        assert!(
            targets.roles.is_empty() && targets.channels.is_empty() && targets.level_roles_known
        );
    }

    #[test]
    fn target_collection_deduplicates_and_combines_channel_requirements() {
        let mut env = vars("2222", "session");
        env.insert("DISCORD_LANDING_CHANNEL_IDS".into(), "6666,7777".into());
        env.insert("TWO_COMMUNITY_HUMAN_CHANNEL_IDS".into(), "6666".into());
        env.insert("TWO_AUTOMOD".into(), "1".into());
        env.insert("TWO_AUTOMOD_EXEMPT_CHANNEL_IDS".into(), "7777".into());
        env.insert(
            "TWO_SELF_ROLE_PANELS".into(),
            r#"[{"channelId":"8888","options":[{"roleId":"5555"}]}]"#.into(),
        );
        let targets = Targets::from_map(&env, None).unwrap();
        assert_eq!(targets.channels.len(), 3);
        assert!(targets.channels[&6666].post && targets.channels[&6666].moderate);
        assert!(targets.channels[&7777].post && !targets.channels[&7777].moderate);
        assert!(targets.channels[&8888].post && targets.channels[&8888].moderate);
        assert_eq!(targets.roles[&5555], "self role");
    }
}
