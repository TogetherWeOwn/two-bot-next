//! Slash-command registry: framework-free command definitions and merge.
//!
//! Ports `src/discord/commandRegistry.ts` + `src/discord/commandNames.ts` from
//! two-bot. Discord's `guild.commands.set` replaces the *complete* command set,
//! so every feature slice merges its definitions here instead of registering a
//! partial view. This module knows nothing about twilight or HTTP: it produces
//! plain data the adapter publishes, so the merge rules are unit-testable.
//!
//! Slice 1 (TOG-9809) ships the merge machinery plus the leveling definitions
//! (`/rank`, `/leaderboard` — the always-published core). Later slices append
//! their feature definitions; the merge signature does not change.
//!
//! Legacy note: two-bot publishes two different commands both named
//! `attendance` (scorecard check-in and RSVP totals) and they collide on
//! `guild.commands.set` — last writer wins, silently. The port namespaces the
//! RSVP-totals variant to `rsvp-attendance` when its slice lands; the merge
//! below additionally dedupes first-wins so a repeated name can never silently
//! drop a command again.

use serde::{Deserialize, Serialize};

/// Discord's guild application-command ceiling (two-bot
/// `DISCORD_GUILD_COMMAND_LIMIT`).
pub const GUILD_COMMAND_LIMIT: usize = 100;

/// Discord application-command option types (API integers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum CommandOptionType {
    String = 3,
    Integer = 4,
    User = 6,
}

impl CommandOptionType {
    #[must_use]
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// One command option, serialised to Discord's wire shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandOption {
    pub name: String,
    pub description: String,
    #[serde(rename = "type")]
    pub kind: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_value: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_value: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_length: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub choices: Vec<CommandChoice>,
}

impl CommandOption {
    /// New optional option; chain [`CommandOption::required`] etc. as needed.
    /// `pub` so later S4 feature slices build definitions on this module.
    pub fn new(name: &str, description: &str, kind: CommandOptionType) -> Self {
        Self {
            name: name.to_owned(),
            description: description.to_owned(),
            kind: kind.as_u8(),
            required: None,
            min_value: None,
            max_value: None,
            max_length: None,
            choices: Vec::new(),
        }
    }

    #[must_use]
    pub fn required(mut self) -> Self {
        self.required = Some(true);
        self
    }

    #[must_use]
    pub fn int_range(mut self, min: i64, max: i64) -> Self {
        self.min_value = Some(min);
        self.max_value = Some(max);
        self
    }

    /// Lower bound only (legacy `duration_seconds` sets a minimum with no
    /// maximum); chain after [`CommandOption::new`].
    #[must_use]
    pub fn min_value(mut self, min: i64) -> Self {
        self.min_value = Some(min);
        self
    }

    #[must_use]
    pub fn max_length(mut self, max: u32) -> Self {
        self.max_length = Some(max);
        self
    }

    #[must_use]
    pub fn choices(mut self, choices: Vec<CommandChoice>) -> Self {
        self.choices = choices;
        self
    }

    /// Shorthand for the ubiquitous required `reason` audit string (≤512).
    #[must_use]
    pub fn reason() -> Self {
        Self::new(
            "reason",
            "Mandatory audit reason",
            CommandOptionType::String,
        )
        .required()
        .max_length(512)
    }

    /// Shorthand for a required `target` user option.
    #[must_use]
    pub fn target() -> Self {
        Self::new("target", "Member to moderate", CommandOptionType::User).required()
    }
}

/// A fixed choice for a string option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandChoice {
    pub name: String,
    pub value: String,
}

/// One slash-command definition, serialised to Discord's wire shape.
/// `default_member_permissions` is the decimal bitfield string Discord expects;
/// `None` means everyone (no permission gate).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandDefinition {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub options: Vec<CommandOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_member_permissions: Option<String>,
    /// `false` in legacy (`setDMPermission(false)`): guild-only, no DMs.
    pub dm_permission: bool,
}

impl CommandDefinition {
    /// New guild-only definition with no options or permission gate; chain
    /// [`CommandDefinition::permissions`] / [`CommandDefinition::options`].
    /// `pub` so later S4 feature slices build definitions on this module.
    pub fn new(name: &str, description: &str) -> Self {
        Self {
            name: name.to_owned(),
            description: description.to_owned(),
            options: Vec::new(),
            default_member_permissions: None,
            dm_permission: false,
        }
    }

    #[must_use]
    pub fn permissions(mut self, bits: u64) -> Self {
        self.default_member_permissions = Some(bits.to_string());
        self
    }

    #[must_use]
    pub fn options(mut self, options: Vec<CommandOption>) -> Self {
        self.options = options;
        self
    }
}

// Verified against the legacy tree's discord.js (`PermissionFlagsBits`):
// BanMembers=4, KickMembers=2, ModerateMembers=2^40, ManageMessages=8192,
// ManageChannels=16, ManageGuild=32, ManageEvents=2^33.
pub const PERM_BAN_MEMBERS: u64 = 4;
pub const PERM_KICK_MEMBERS: u64 = 2;
pub const PERM_MODERATE_MEMBERS: u64 = 1099511627776;
pub const PERM_MANAGE_MESSAGES: u64 = 8192;
pub const PERM_MANAGE_CHANNELS: u64 = 16;
pub const PERM_MANAGE_GUILD: u64 = 32;
pub const PERM_MANAGE_EVENTS: u64 = 8589934592;

/// Always-published core commands (two-bot `CORE_COMMAND_DATA` = leveling).
#[must_use]
pub fn core_commands() -> Vec<CommandDefinition> {
    vec![
        CommandDefinition::new("rank", "Show your XP, level and server rank.").options(vec![
            CommandOption::new("member", "Show another member.", CommandOptionType::User),
        ]),
        CommandDefinition::new("leaderboard", "Show the server XP leaderboard."),
    ]
}

/// Merge the authoritative guild command set.
///
/// Mirrors two-bot `mergedCommandData`: core first, then each enabled
/// feature's builtins in a fixed order, then DB-backed custom commands.
/// First definition wins on a repeated name (legacy silently let the last
/// writer win — see the `/attendance` collision note above), reserved names
/// shadow custom commands, and the Discord 100-command ceiling is enforced.
pub fn merge_commands(
    additional_builtins: &[Vec<CommandDefinition>],
    custom: &[CustomCommand],
) -> Result<Vec<CommandDefinition>, RegistryError> {
    let mut merged: Vec<CommandDefinition> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for def in core_commands()
        .iter()
        .chain(additional_builtins.iter().flatten())
    {
        if seen.insert(def.name.clone()) {
            merged.push(def.clone());
        }
    }
    if merged.len() > GUILD_COMMAND_LIMIT {
        return Err(RegistryError::BuiltinLimit(merged.len()));
    }

    let mut custom_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for cmd in custom.iter().filter(|c| c.enabled) {
        if seen.contains(&cmd.name) || !custom_seen.insert(cmd.name.clone()) {
            continue;
        }
        merged.push(CommandDefinition::new(&cmd.name, &cmd.description).options(Vec::new()));
    }
    if merged.len() > GUILD_COMMAND_LIMIT {
        return Err(RegistryError::TotalLimit(merged.len()));
    }
    Ok(merged)
}

/// A DB-backed custom command (two-bot `/command` builder rows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommand {
    pub name: String,
    pub description: String,
    pub enabled: bool,
}

/// Registry merge failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error("built-in commands ({0}) exceed Discord's guild limit of 100")]
    BuiltinLimit(usize),
    #[error("merged command set ({0}) exceeds Discord's guild limit of 100")]
    TotalLimit(usize),
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn property_option_builders_preserve_wire_values(
            name in "[a-z_-]{1,32}",
            description in proptest::collection::vec(any::<char>(), 0..100)
                .prop_map(|chars| chars.into_iter().collect::<String>()),
            min in any::<i64>(), max in any::<i64>(), length in any::<u32>(),
        ) {
            let option = CommandOption::new(&name, &description, CommandOptionType::Integer)
                .required().int_range(min, max).max_length(length);
            let wire = serde_json::to_vec(&option).unwrap();
            let decoded: CommandOption = serde_json::from_slice(&wire).unwrap();
            prop_assert_eq!(decoded, option);
            let lower_only = CommandOption::new(&name, &description, CommandOptionType::Integer).min_value(min);
            prop_assert_eq!(lower_only.min_value, Some(min));
            prop_assert_eq!(lower_only.max_value, None);
        }

        #[test]
        fn property_published_numeric_options_match_parity_section_one(value in any::<i64>()) {
            let definitions = crate::moderation::moderation_commands().into_iter()
                .chain(crate::feature_commands::automation_commands()).collect::<Vec<_>>();
            for (command, option, min, max, required) in [
                ("tempban", "duration_seconds", 60, None, true),
                ("timeout", "duration_seconds", 60, None, true),
                ("purge", "count", 1, Some(100), true),
                ("slowmode", "seconds", 0, Some(21_600), true),
                ("schedule", "in-minutes", 1, Some(525_600), false),
                ("schedule", "every-minutes", 60, Some(525_600), false),
                ("sticky", "debounce", 1, Some(300), false),
            ] {
                let definition = definitions.iter().find(|d| d.name == command).unwrap();
                let published = definition.options.iter().find(|o| o.name == option).unwrap();
                prop_assert_eq!(published.kind, CommandOptionType::Integer.as_u8());
                prop_assert_eq!(published.required, required.then_some(true));
                prop_assert_eq!(published.min_value, Some(min));
                prop_assert_eq!(published.max_value, max);
                // Duration builders deliberately have no max; runtime validators
                // enforce the separate Discord/service caps in internal_actions.
                for n in [min - 1, min, max.unwrap_or(i64::MAX), value] {
                    let advertised = published.min_value.is_none_or(|lower| n >= lower)
                        && published.max_value.is_none_or(|upper| n <= upper);
                    prop_assert_eq!(advertised, n >= min && max.is_none_or(|upper| n <= upper));
                }
            }
            for definition in crate::moderation::moderation_commands() {
                let reason = definition.options.iter().find(|o| o.name == "reason").unwrap();
                prop_assert_eq!(reason.required, Some(true));
                prop_assert_eq!(reason.max_length, Some(512));
                prop_assert!(!definition.dm_permission);
            }
        }

        #[test]
        fn property_registry_merge_is_first_wins_and_respects_the_ceiling(
            builtin_count in 0usize..=102,
            custom in proptest::collection::vec((
                prop_oneof![Just("rank".to_owned()), Just("leaderboard".to_owned()), "[a-z]{1,6}"],
                any::<bool>(),
            ), 0..=110),
        ) {
            let builtins = (0..builtin_count).map(|i| CommandDefinition::new(&format!("builtin-{i}"), "first"))
                .collect::<Vec<_>>();
            let duplicates = builtins.iter().map(|d| CommandDefinition::new(&d.name, "shadowed")).collect::<Vec<_>>();
            let custom = custom.into_iter().map(|(name, enabled)| CustomCommand {
                name, description: "custom".to_owned(), enabled,
            }).collect::<Vec<_>>();
            let mut expected = core_commands().into_iter().chain(builtins.iter().cloned()).collect::<Vec<_>>();
            for cmd in &custom {
                if cmd.enabled && !expected.iter().any(|d| d.name == cmd.name) {
                    expected.push(CommandDefinition::new(&cmd.name, &cmd.description));
                }
            }
            let actual = merge_commands(&[builtins, duplicates], &custom);
            if builtin_count + 2 > 100 {
                prop_assert_eq!(actual, Err(RegistryError::BuiltinLimit(builtin_count + 2)));
            } else if expected.len() > 100 {
                prop_assert_eq!(actual, Err(RegistryError::TotalLimit(expected.len())));
            } else {
                prop_assert_eq!(actual, Ok(expected));
            }
        }
    }

    #[test]
    fn core_commands_match_legacy_names() {
        let core = core_commands();
        assert_eq!(core.len(), 2);
        assert_eq!(core[0].name, "rank");
        assert_eq!(core[1].name, "leaderboard");
        // Guild-only, DM off (legacy `setDMPermission(false)`).
        assert!(core.iter().all(|c| !c.dm_permission));
        // `rank` has one optional `member` user option; `leaderboard` none.
        assert_eq!(core[0].options.len(), 1);
        assert_eq!(core[0].options[0].kind, 6);
        assert!(core[0].options[0].required.is_none());
        assert!(core[1].options.is_empty());
    }

    #[test]
    fn merge_orders_core_first_then_features_then_custom() {
        let extra = vec![CommandDefinition::new(
            "rsvp",
            "RSVP to a Discord scheduled event",
        )];
        let custom = vec![CustomCommand {
            name: "faq".to_owned(),
            description: "FAQ".to_owned(),
            enabled: true,
        }];
        let merged = merge_commands(&[extra], &custom).expect("merges");
        let names: Vec<_> = merged.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["rank", "leaderboard", "rsvp", "faq"]);
    }

    #[test]
    fn first_definition_wins_on_name_collision() {
        // Legacy `/attendance` collision (parity §1 #12 vs #25): whichever
        // slice registers first keeps the name; the other must rename.
        let scorecard = vec![CommandDefinition::new(
            "attendance",
            "Record a verified attendee",
        )];
        let rsvp_totals = vec![CommandDefinition::new("attendance", "Show RSVP totals")];
        let merged = merge_commands(&[scorecard, rsvp_totals], &[]).expect("merges");
        assert_eq!(merged.len(), 3); // rank, leaderboard, attendance×1
        assert_eq!(merged[2].description, "Record a verified attendee");
    }

    #[test]
    fn custom_never_shadows_builtins_and_dedupes() {
        let custom = vec![
            CustomCommand {
                name: "rank".to_owned(),
                description: "shadow attempt".to_owned(),
                enabled: true,
            },
            CustomCommand {
                name: "faq".to_owned(),
                description: "FAQ".to_owned(),
                enabled: true,
            },
            CustomCommand {
                name: "faq".to_owned(),
                description: "FAQ dup".to_owned(),
                enabled: true,
            },
            CustomCommand {
                name: "off".to_owned(),
                description: "disabled".to_owned(),
                enabled: false,
            },
        ];
        let merged = merge_commands(&[], &custom).expect("merges");
        let names: Vec<_> = merged.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["rank", "leaderboard", "faq"]);
    }

    #[test]
    fn builtin_limit_enforced() {
        let extra: Vec<CommandDefinition> = (0..99)
            .map(|i| CommandDefinition::new(&format!("cmd-{i}"), "filler"))
            .collect();
        let err = merge_commands(&[extra], &[]).expect_err("must exceed 100");
        assert!(matches!(err, RegistryError::BuiltinLimit(101)));
    }

    #[test]
    fn definitions_serialize_to_discord_wire_shape() {
        let json = serde_json::to_value(core_commands()).expect("serializes");
        assert_eq!(json[0]["name"], "rank");
        assert_eq!(json[0]["options"][0]["type"], 6);
        assert_eq!(json[0]["dm_permission"], false);
    }
}
