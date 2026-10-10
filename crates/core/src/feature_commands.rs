//! Feature command definitions: automations, announcements, scorecard.
//!
//! Slice 2 of TOG-9809. Ports the slash-command *shapes* (names, options,
//! permission gates) from legacy two-bot as framework-free data built on the
//! `commands` registry module. Handlers, stores and pollers land in later
//! slices; this module only defines what gets published on
//! `guild.commands.set`, so every shape is unit-testable without Discord.
//!
//! Source files (legacy `two-bot`, frozen `main`):
//! - automations (#14–#21): `src/automations/discord.ts`
//!   (`automationCommandData`) — all eight `ManageGuild`-gated.
//! - announcements (#24–#30): `src/announcements/discord.ts`
//!   (`announcementCommandData`) — `/rsvp` + RSVP totals open to everyone,
//!   `/lfg` pair `ManageEvents`, `/feed-*` trio `ManageGuild`.
//! - scorecard (#12): `src/analytics/communityAttendance.ts`
//!   (`COMMUNITY_ATTENDANCE_COMMAND`) — keeps the `attendance` name.
//!
//! Naming: legacy publishes two commands both named `attendance` (scorecard
//! #12 and RSVP totals #25) and they collide on `guild.commands.set`. The
//! scorecard variant keeps the name (it is the `COMMUNITY_COMMAND_DATA`
//! entry); the RSVP-totals variant is namespaced to `rsvp-attendance` here,
//! matching the decision recorded in `commands.rs`.
//!
//! Env gating travels with the shapes (`FeatureGates`): legacy
//! `src/automations/config.ts` (`TWO_AUTOMATIONS`, `TWO_TEXT_COMMANDS`) and
//! `src/announcements/config.ts` (`TWO_ANNOUNCEMENTS`,
//! `TWO_FEED_POLL_SECONDS`). The registry caller decides which slices to
//! merge based on these flags.
//!
//! Deliberately out of scope: moderation #3–#11 (slice 3 of TOG-9809, LAST),
//! `/voice` subcommands (temp-voice runtime never shipped on legacy `main` —
//! only the S6 staging shape-check ports it, matrix §9 drop 6),
//! `/rota-acknowledge` #13 (dropped with the rota stack).

use std::collections::{HashMap, HashSet};

use super::commands::{
    CommandChoice, CommandDefinition, CommandOption, CommandOptionType, MAX_RESOURCE_ID_CHARS,
    OCCURRENCE_ID_MAX_CHARS, PERM_MANAGE_EVENTS, PERM_MANAGE_GUILD,
};
use super::custom_commands::{
    MAX_COMMAND_NAME_CHARS, MAX_DESCRIPTION_CHARS, MAX_TEMPLATE_CHARS, MAX_TEXT_TRIGGER_CHARS,
};
use super::feeds_http::MAX_FEED_SOURCE_BYTES;
use super::lfg::{MAX_ROLE_SPEC_CHARS, MAX_TITLE_CHARS};
use super::scheduled::MAX_BODY_CHARS;

/// Scorecard check-in command (parity #12). Keeps the `attendance` name; the
/// RSVP-totals variant is the one that renames.
#[must_use]
pub fn scorecard_attendance_command() -> CommandDefinition {
    CommandDefinition::new(
        "attendance",
        "Check in a verified human attendee for a scheduled event (scorecard)",
    )
    .permissions(PERM_MANAGE_EVENTS)
    .options(vec![
        CommandOption::new(
            "event-occurrence",
            "Bare event id binds that event; else {event_id}:{label}. Bare slugs refuse, e.g. 12345:weekly",
            CommandOptionType::String,
        )
        .required()
        .max_length(OCCURRENCE_ID_MAX_CHARS as u32),
        CommandOption::new(
            "member",
            "Human member who attended.",
            CommandOptionType::User,
        )
        .required(),
    ])
}

/// Automation admin commands (parity #14–#21): custom-command builder,
/// scheduled messages, stickies. All `ManageGuild`-gated in legacy
/// (`setDefaultMemberPermissions`), guild-only.
#[must_use]
pub fn automation_commands() -> Vec<CommandDefinition> {
    vec![
        CommandDefinition::new("command", "Define or replace a custom command")
            .permissions(PERM_MANAGE_GUILD)
            .options(vec![
                CommandOption::new(
                    "name",
                    "Command name, a-z 0-9 _ -",
                    CommandOptionType::String,
                )
                .required()
                .max_length(MAX_COMMAND_NAME_CHARS as u32),
                CommandOption::new(
                    "template",
                    "What the bot replies; {user} {username} {server} {channel}",
                    CommandOptionType::String,
                )
                .required()
                .max_length(MAX_TEMPLATE_CHARS as u32),
                CommandOption::new(
                    "description",
                    "Shown in the command picker",
                    CommandOptionType::String,
                )
                .max_length(MAX_DESCRIPTION_CHARS as u32),
                CommandOption::new(
                    "text-trigger",
                    "Optional !trigger form, e.g. !faq",
                    CommandOptionType::String,
                )
                .max_length(MAX_TEXT_TRIGGER_CHARS as u32),
            ]),
        CommandDefinition::new("command-remove", "Delete a custom command")
            .permissions(PERM_MANAGE_GUILD)
            .options(vec![CommandOption::new(
                "name",
                "Command to delete",
                CommandOptionType::String,
            )
            .required()
            .max_length(MAX_COMMAND_NAME_CHARS as u32)]),
        CommandDefinition::new("command-list", "List this server's custom commands")
            .permissions(PERM_MANAGE_GUILD),
        CommandDefinition::new("schedule", "Schedule a message, once or recurring")
            .permissions(PERM_MANAGE_GUILD)
            .options(vec![
                CommandOption::new("body", "Message text", CommandOptionType::String)
                    .required()
                    .max_length(MAX_BODY_CHARS as u32),
                CommandOption::new(
                    "in-minutes",
                    "Fire this many minutes from now",
                    CommandOptionType::Integer,
                )
                .int_range(1, 525600),
                CommandOption::new(
                    "every-minutes",
                    "Recur at this interval (60 min minimum)",
                    CommandOptionType::Integer,
                )
                .int_range(60, 525600),
            ]),
        CommandDefinition::new("schedule-remove", "Cancel a scheduled message")
            .permissions(PERM_MANAGE_GUILD)
            .options(vec![CommandOption::new(
                "id",
                "Scheduled message id",
                CommandOptionType::String,
            )
            .required()
            .max_length(MAX_RESOURCE_ID_CHARS as u32)]),
        CommandDefinition::new("schedule-list", "List scheduled messages for this server")
            .permissions(PERM_MANAGE_GUILD),
        CommandDefinition::new("sticky", "Set this channel's sticky message")
            .permissions(PERM_MANAGE_GUILD)
            .options(vec![
                CommandOption::new("body", "Sticky text", CommandOptionType::String)
                    .required()
                    .max_length(MAX_BODY_CHARS as u32),
                CommandOption::new(
                    "debounce",
                    "Quiet seconds before re-posting (default 5, max 300)",
                    CommandOptionType::Integer,
                )
                .int_range(1, 300),
            ]),
        CommandDefinition::new("sticky-remove", "Remove this channel's sticky message")
            .permissions(PERM_MANAGE_GUILD),
    ]
}

/// Announcement commands (parity #24–#30): RSVP, LFG signups, feed relays.
///
/// `/rsvp` and `/rsvp-attendance` are open to everyone; `/lfg` pair requires
/// `ManageEvents`; `/feed-*` trio requires `ManageGuild`. All guild-only.
#[must_use]
pub fn announcement_commands() -> Vec<CommandDefinition> {
    vec![
        CommandDefinition::new("rsvp", "RSVP to a Discord scheduled event").options(vec![
            CommandOption::new(
                "event-id",
                "Discord scheduled event id (number in the event URL), e.g. 12345",
                CommandOptionType::String,
            )
            .required(),
            CommandOption::new("status", "Your response", CommandOptionType::String)
                .required()
                .choices(vec![
                    CommandChoice {
                        name: "Going".to_owned(),
                        value: "going".to_owned(),
                    },
                    CommandChoice {
                        name: "Interested".to_owned(),
                        value: "interested".to_owned(),
                    },
                    CommandChoice {
                        name: "Declined".to_owned(),
                        value: "declined".to_owned(),
                    },
                ]),
        ]),
        // Namespaced: legacy `attendance` (RSVP totals) collides with the
        // scorecard `attendance` (#12) on `guild.commands.set`.
        CommandDefinition::new("rsvp-attendance", "Show RSVP totals for a scheduled event")
            .options(vec![CommandOption::new(
                "event-id",
                "Discord scheduled event id (number in the event URL), e.g. 12345",
                CommandOptionType::String,
            )
            .required()]),
        CommandDefinition::new("lfg", "Post a raid/LFG signup with role slots")
            .permissions(PERM_MANAGE_EVENTS)
            .options(vec![
                CommandOption::new("title", "Event or group title", CommandOptionType::String)
                    .required()
                    .max_length(MAX_TITLE_CHARS as u32),
                CommandOption::new(
                    "starts-at",
                    "ISO-8601 start time, e.g. 2026-10-04T18:00:00Z",
                    CommandOptionType::String,
                )
                .required(),
                CommandOption::new(
                    "roles",
                    "Role slots as role:Label:count, comma-separated, e.g. tank:Tank:2,dps:DPS:6",
                    CommandOptionType::String,
                )
                .required()
                .max_length(MAX_ROLE_SPEC_CHARS as u32),
            ]),
        CommandDefinition::new("lfg-close", "Close a raid/LFG signup")
            .permissions(PERM_MANAGE_EVENTS)
            .options(vec![CommandOption::new(
                "id",
                "LFG id from the posted signup",
                CommandOptionType::String,
            )
            .required()
            .max_length(MAX_RESOURCE_ID_CHARS as u32)]),
        CommandDefinition::new(
            "feed-add",
            "Relay an RSS, YouTube, or Twitch feed into this channel",
        )
        .permissions(PERM_MANAGE_GUILD)
        .options(vec![
            CommandOption::new("kind", "Feed kind", CommandOptionType::String)
                .required()
                .choices(vec![
                    CommandChoice {
                        name: "RSS".to_owned(),
                        value: "rss".to_owned(),
                    },
                    CommandChoice {
                        name: "YouTube".to_owned(),
                        value: "youtube".to_owned(),
                    },
                    CommandChoice {
                        name: "Twitch".to_owned(),
                        value: "twitch".to_owned(),
                    },
                ]),
            CommandOption::new(
                "source",
                "HTTPS URL or YouTube channel id",
                CommandOptionType::String,
            )
            .required()
            .max_length(MAX_FEED_SOURCE_BYTES as u32),
        ]),
        CommandDefinition::new("feed-remove", "Remove a feed relay")
            .permissions(PERM_MANAGE_GUILD)
            .options(vec![CommandOption::new(
                "id",
                "Feed id",
                CommandOptionType::String,
            )
            .required()
            .max_length(MAX_RESOURCE_ID_CHARS as u32)]),
        CommandDefinition::new("feed-list", "List this server's feed relays")
            .permissions(PERM_MANAGE_GUILD),
    ]
}

/// Parse a `!<trigger>` prefix from message text (parity §1 #23).
///
/// Ports legacy `triggerWord` (`src/automations/gateway.ts`): `text` must
/// start with `!` (no leading whitespace), the word is everything up to the
/// first JS-`\s` character, and it is lowercased (`str::to_lowercase`, the
/// same Unicode fold as JS `toLowerCase`, so `!\u{212A}ick` folds to `kick`).
/// The folded word must match legacy `TRIGGER_PATTERN =
/// /^![a-z0-9_-]{1,32}$/` ([`crate::leveling::valid_command_name`]); stored
/// triggers always do, so a miss here is a miss in the store. The returned
/// name excludes the leading `!`.
///
/// - `text_commands_enabled` is the `FeatureGates::text_commands` flag
///   (`TWO_AUTOMATIONS=1` AND `TWO_TEXT_COMMANDS=1`). Off rejects everything.
/// - `builtin_names` holds bare builtin slash names (no `!` prefix: `rank`,
///   `ban`, `rsvp-attendance`, …). A folded trigger colliding with a builtin
///   is rejected so prefix traffic can never shadow a slash command.
/// - First token only: `"!faq extra"` yields `faq`; `"hi !faq"` and
///   `"  !faq"` yield nothing.
///
/// Gateway wiring and custom-command storage are out of scope (follow-ups
/// under TOG-10080); the caller resolves the returned name against its store.
#[must_use]
pub fn parse_prefix_trigger(
    text: &str,
    text_commands_enabled: bool,
    builtin_names: &HashSet<String>,
) -> Option<String> {
    if !text_commands_enabled {
        return None;
    }
    let rest = text.strip_prefix('!')?;
    let word = rest.split(is_js_whitespace).next()?;
    let name = word.to_lowercase();
    if !crate::leveling::valid_command_name(&name) || builtin_names.contains(&name) {
        return None;
    }
    Some(name)
}

/// JS regex `\s`: Unicode `White_Space` minus U+0085 (NEL), plus U+FEFF
/// (BOM). `char::is_whitespace` alone differs on exactly those two.
fn is_js_whitespace(c: char) -> bool {
    c == '\u{feff}' || (c != '\u{85}' && c.is_whitespace())
}

/// Slice-2 definitions in legacy publish order (community, automation,
/// announcement — mirrors `BUILTIN_COMMAND_NAMES` in
/// `src/discord/commandNames.ts`). The registry caller merges this via
/// `additional_builtins`.
#[must_use]
pub fn feature_commands() -> Vec<CommandDefinition> {
    let mut defs = vec![scorecard_attendance_command()];
    defs.extend(automation_commands());
    defs.extend(announcement_commands());
    defs
}

/// Per-feature env gates (legacy `src/automations/config.ts` +
/// `src/announcements/config.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeatureGates {
    /// `TWO_AUTOMATIONS=1` — publish + serve automation commands.
    pub automations: bool,
    /// `TWO_ANNOUNCEMENTS=1` — publish + serve announcement commands.
    pub announcements: bool,
    /// `TWO_TEXT_COMMANDS=1`, only effective while automations are on
    /// (legacy `textCommandsEnabled = enabled && ...`).
    pub text_commands: bool,
    /// `TWO_FEED_POLL_SECONDS`, default 300, validated 60–86400.
    pub feed_poll_seconds: u64,
}

/// Invalid feature-gate environment.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GateError {
    #[error("TWO_FEED_POLL_SECONDS must be an integer between 60 and 86400, got {0:?}")]
    InvalidFeedPoll(String),
}

impl FeatureGates {
    /// Read gates from the process environment.
    pub fn from_env() -> Result<Self, GateError> {
        Self::from_map(&std::env::vars().collect())
    }

    /// Read gates from an explicit map (tests, staged config).
    pub fn from_map(vars: &HashMap<String, String>) -> Result<Self, GateError> {
        let flag = |key: &str| vars.get(key).is_some_and(|v| v == "1");
        let automations = flag("TWO_AUTOMATIONS");
        let feed_poll_seconds = match vars.get("TWO_FEED_POLL_SECONDS") {
            None => 300,
            Some(raw) => raw
                .parse::<u64>()
                .ok()
                .filter(|&n| (60..=86400).contains(&n))
                .ok_or_else(|| GateError::InvalidFeedPoll(raw.clone()))?,
        };
        Ok(Self {
            automations,
            announcements: flag("TWO_ANNOUNCEMENTS"),
            text_commands: automations && flag("TWO_TEXT_COMMANDS"),
            feed_poll_seconds,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::merge_commands;

    fn names(defs: &[CommandDefinition]) -> Vec<&str> {
        defs.iter().map(|d| d.name.as_str()).collect()
    }

    #[test]
    fn slice2_names_match_legacy_in_order() {
        assert_eq!(
            names(&feature_commands()),
            [
                "attendance",
                "command",
                "command-remove",
                "command-list",
                "schedule",
                "schedule-remove",
                "schedule-list",
                "sticky",
                "sticky-remove",
                "rsvp",
                "rsvp-attendance",
                "lfg",
                "lfg-close",
                "feed-add",
                "feed-remove",
                "feed-list",
            ]
        );
    }

    #[test]
    fn no_collision_with_leveling_core_or_within_slice() {
        let merged = merge_commands(&[feature_commands()], &[]).expect("slice 2 merges cleanly");
        // 3 core + 1 scorecard + 8 automation + 7 announcement.
        assert_eq!(merged.len(), 19);
        assert_eq!(
            &names(&merged)[..4],
            ["rank", "leaderboard", "help", "attendance"]
        );
        assert!(!merged.iter().any(|d| d.dm_permission));
    }

    #[test]
    fn permission_gates_match_legacy() {
        let is = |defs: &[CommandDefinition], name: &str| {
            defs.iter()
                .find(|d| d.name == name)
                .expect("command exists")
                .default_member_permissions
                .clone()
        };
        let auto = automation_commands();
        let ann = announcement_commands();
        let manage_guild = Some(PERM_MANAGE_GUILD.to_string());
        let manage_events = Some(PERM_MANAGE_EVENTS.to_string());
        for name in [
            "command",
            "command-remove",
            "command-list",
            "schedule",
            "schedule-remove",
            "schedule-list",
            "sticky",
            "sticky-remove",
        ] {
            assert_eq!(is(&auto, name), manage_guild, "{name} gates ManageGuild");
        }
        assert_eq!(is(&ann, "lfg"), manage_events);
        assert_eq!(is(&ann, "lfg-close"), manage_events);
        for name in ["feed-add", "feed-remove", "feed-list"] {
            assert_eq!(is(&ann, name), manage_guild, "{name} gates ManageGuild");
        }
        // Open to everyone: no permission gate.
        assert_eq!(is(&ann, "rsvp"), None);
        assert_eq!(is(&ann, "rsvp-attendance"), None);
        assert_eq!(
            is(&[scorecard_attendance_command()], "attendance"),
            manage_events
        );
    }

    #[test]
    fn option_shapes_match_legacy() {
        fn get<'a>(defs: &'a [CommandDefinition], name: &str) -> &'a CommandDefinition {
            defs.iter()
                .find(|d| d.name == name)
                .expect("command exists")
        }
        let auto = automation_commands();
        for (command, option, max_length) in [
            ("command", 0, 32),
            ("command", 1, 2000),
            ("command", 2, 100),
            ("command", 3, 33),
            ("command-remove", 0, 32),
            ("schedule", 0, 2000),
            ("schedule-remove", 0, 128),
            ("sticky", 0, 2000),
        ] {
            assert_eq!(
                get(&auto, command).options[option].max_length,
                Some(max_length),
                "{command} option {} max_length",
                get(&auto, command).options[option].name
            );
        }
        // /schedule: one timing option required, both bounded.
        let sched = get(&auto, "schedule").options.clone();
        assert!(sched[0].required == Some(true));
        assert_eq!(
            (sched[1].min_value, sched[1].max_value),
            (Some(1), Some(525600))
        );
        assert_eq!(
            (sched[2].min_value, sched[2].max_value),
            (Some(60), Some(525600))
        );
        assert!(sched[1].required.is_none() && sched[2].required.is_none());
        // /sticky debounce 1–300, optional.
        let sticky = get(&auto, "sticky").options.clone();
        assert_eq!(
            (sticky[1].min_value, sticky[1].max_value),
            (Some(1), Some(300))
        );
        assert!(sticky[1].required.is_none());
        // /rsvp status choices.
        let ann = announcement_commands();
        for (command, option, max_length) in [
            ("lfg", 0, 100),
            ("lfg", 2, 2339),
            ("lfg-close", 0, 128),
            ("feed-add", 1, 2048),
            ("feed-remove", 0, 128),
        ] {
            assert_eq!(
                get(&ann, command).options[option].max_length,
                Some(max_length),
                "{command} option {} max_length",
                get(&ann, command).options[option].name
            );
        }
        let status = get(&ann, "rsvp").options[1].clone();
        let values: Vec<_> = status.choices.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, ["going", "interested", "declined"]);
        // /feed-add kind choices.
        let kind = get(&ann, "feed-add").options[0].clone();
        let values: Vec<_> = kind.choices.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, ["rss", "youtube", "twitch"]);
        // Scorecard attendance: both options required, user-typed member.
        let score = scorecard_attendance_command();
        assert!(score.options.iter().all(|o| o.required == Some(true)));
        assert_eq!(score.options[1].kind, CommandOptionType::User.as_u8());
        // event-occurrence advertises the shared occurrence-ID bound (pinned
        // literal: a const change must update docs/parity.md and commands.md).
        assert_eq!(score.options[0].max_length, Some(128));
        assert_eq!(
            score.options[0].max_length,
            Some(OCCURRENCE_ID_MAX_CHARS as u32)
        );
    }

    #[test]
    fn custom_command_option_lengths_match_runtime_caps() {
        use crate::custom_commands::{validate_put_input, validate_template, PutCommandInput};
        use std::collections::HashSet;

        // Pinned literals: tampering with any shared bound trips this test.
        // A const change must update the slash-option advertisement, the
        // validators and docs/commands.md together.
        assert_eq!(MAX_COMMAND_NAME_CHARS, 32);
        assert_eq!(MAX_TEMPLATE_CHARS, 2000);
        assert_eq!(MAX_DESCRIPTION_CHARS, 100);
        assert_eq!(MAX_TEXT_TRIGGER_CHARS, 33);
        assert_eq!(MAX_TEXT_TRIGGER_CHARS, MAX_COMMAND_NAME_CHARS + 1);

        fn get<'a>(defs: &'a [CommandDefinition], name: &str) -> &'a CommandDefinition {
            defs.iter()
                .find(|d| d.name == name)
                .expect("command exists")
        }
        fn option<'a>(
            defs: &'a [CommandDefinition],
            command: &str,
            option: &str,
        ) -> &'a crate::commands::CommandOption {
            get(defs, command)
                .options
                .iter()
                .find(|o| o.name == option)
                .expect("option exists")
        }
        let auto = automation_commands();
        // Advertised bounds come from the same constants the validators use.
        assert_eq!(
            option(&auto, "command", "name").max_length,
            Some(MAX_COMMAND_NAME_CHARS as u32)
        );
        assert_eq!(
            option(&auto, "command", "template").max_length,
            Some(MAX_TEMPLATE_CHARS as u32)
        );
        assert_eq!(
            option(&auto, "command", "description").max_length,
            Some(MAX_DESCRIPTION_CHARS as u32)
        );
        assert_eq!(
            option(&auto, "command", "text-trigger").max_length,
            Some(MAX_TEXT_TRIGGER_CHARS as u32)
        );
        assert_eq!(
            option(&auto, "command-remove", "name").max_length,
            Some(MAX_COMMAND_NAME_CHARS as u32)
        );
        // Pinned wire values: a silent advertisement change trips here too.
        assert_eq!(option(&auto, "command", "name").max_length, Some(32));
        assert_eq!(option(&auto, "command", "template").max_length, Some(2000));
        assert_eq!(
            option(&auto, "command", "description").max_length,
            Some(100)
        );
        assert_eq!(
            option(&auto, "command", "text-trigger").max_length,
            Some(33)
        );
        assert_eq!(option(&auto, "command-remove", "name").max_length, Some(32));

        // Runtime agrees at the boundary: name shape, trigger shape, template
        // ceiling and description bounds.
        assert!(crate::leveling::valid_command_name(
            &"a".repeat(MAX_COMMAND_NAME_CHARS)
        ));
        assert!(!crate::leveling::valid_command_name(
            &"a".repeat(MAX_COMMAND_NAME_CHARS + 1)
        ));
        let trigger_at_ceiling = format!("!{}", "a".repeat(MAX_COMMAND_NAME_CHARS));
        let trigger_past_ceiling = format!("!{}", "a".repeat(MAX_COMMAND_NAME_CHARS + 1));
        assert!(crate::leveling::valid_text_trigger(&trigger_at_ceiling));
        assert!(!crate::leveling::valid_text_trigger(&trigger_past_ceiling));
        assert_eq!(MAX_TEXT_TRIGGER_CHARS, trigger_at_ceiling.len());
        validate_template(&"x".repeat(MAX_TEMPLATE_CHARS)).expect("ceiling ok");
        assert!(validate_template(&"x".repeat(MAX_TEMPLATE_CHARS + 1)).is_err());

        // End-to-end through the shared validator: ceilings pass, one past
        // any ceiling fails. A tampered constant that moves the advertisement
        // without moving validation (or vice versa) trips either the pinned
        // literals above or these boundary rejections.
        let builtins = HashSet::new();
        let ceiling = PutCommandInput {
            name: "a".repeat(MAX_COMMAND_NAME_CHARS),
            description: "x".repeat(MAX_DESCRIPTION_CHARS),
            template: "x".repeat(MAX_TEMPLATE_CHARS),
            text_trigger: Some(trigger_at_ceiling.clone()),
        };
        assert!(validate_put_input(&ceiling, &builtins).is_ok());
        for tampered in [
            PutCommandInput {
                name: "a".repeat(MAX_COMMAND_NAME_CHARS + 1),
                ..ceiling.clone()
            },
            PutCommandInput {
                description: "x".repeat(MAX_DESCRIPTION_CHARS + 1),
                ..ceiling.clone()
            },
            PutCommandInput {
                template: "x".repeat(MAX_TEMPLATE_CHARS + 1),
                ..ceiling.clone()
            },
            PutCommandInput {
                text_trigger: Some(trigger_past_ceiling.clone()),
                ..ceiling.clone()
            },
        ] {
            assert!(validate_put_input(&tampered, &builtins).is_err());
        }
    }

    #[test]
    fn feature_commands_serialize_to_wire_shape() {
        let json = serde_json::to_value(feature_commands()).expect("serializes");
        // Scorecard attendance keeps ManageEvents bits as a decimal string,
        // RSVP-totals carries the namespaced name.
        assert_eq!(json[0]["name"], "attendance");
        assert_eq!(json[0]["default_member_permissions"], "8589934592");
        assert_eq!(json[0]["options"][0]["name"], "event-occurrence");
        assert_eq!(json[0]["options"][0]["max_length"], 128);
        assert_eq!(json[10]["name"], "rsvp-attendance");
        assert!(json[10].get("default_member_permissions").is_none());
        assert_eq!(json[10]["options"][0]["name"], "event-id");
    }

    #[test]
    fn gates_default_off_with_300s_feed_poll() {
        let gates = FeatureGates::from_map(&HashMap::new()).expect("defaults");
        assert!(!gates.automations && !gates.announcements && !gates.text_commands);
        assert_eq!(gates.feed_poll_seconds, 300);
    }

    #[test]
    fn gates_enable_and_text_commands_require_automations() {
        let vars: HashMap<String, String> = [
            ("TWO_AUTOMATIONS", "1"),
            ("TWO_ANNOUNCEMENTS", "1"),
            ("TWO_TEXT_COMMANDS", "1"),
            ("TWO_FEED_POLL_SECONDS", "600"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
        let gates = FeatureGates::from_map(&vars).expect("parses");
        assert!(gates.automations && gates.announcements && gates.text_commands);
        assert_eq!(gates.feed_poll_seconds, 600);

        // Text commands alone do not enable themselves (legacy AND).
        let vars: HashMap<String, String> = [("TWO_TEXT_COMMANDS", "1")]
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let gates = FeatureGates::from_map(&vars).expect("parses");
        assert!(!gates.text_commands);
    }

    #[test]
    fn gates_reject_bad_feed_poll() {
        for raw in ["59", "86401", "abc", ""] {
            let vars: HashMap<String, String> = [("TWO_FEED_POLL_SECONDS", raw)]
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            assert!(
                matches!(
                    FeatureGates::from_map(&vars),
                    Err(GateError::InvalidFeedPoll(_))
                ),
                "{raw:?} rejected"
            );
        }
    }
}
