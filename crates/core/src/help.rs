//! `/help` discovery reply.
//!
//! The staging-guild UX walk found 27 builtins publishing with no discovery
//! surface: typing `/help` answered with the stale-interaction reply because
//! no such command existed. This module renders the ephemeral `/help` reply
//! from the last confirmed publication, so omitted features never appear and
//! published-but-restricted commands carry their permission hint.
//!
//! Pure data in, text out: no Discord calls, no store reads. The caller passes
//! the successfully published definitions, not a boot vector or a fresh DB
//! read. Failed refreshes retain the last confirmed snapshot. Groups render
//! in publish order; a group renders only while at least one of its commands
//! is live. Names outside the table (DB custom rows, or a future builtin this
//! table predates) render under Custom rather than being dropped.

use std::collections::HashSet;
use std::fmt::Write as _;

use super::commands::CommandDefinition;

/// One `/help` group: commands that share an audience.
struct HelpGroup {
    title: &'static str,
    /// Permission hint for the group header; `None` means everyone.
    requires: Option<&'static str>,
    commands: &'static [&'static str],
}

/// Groups in publish order. The `requires` labels mirror the legacy refusal
/// texts (`Manage Server` for `ManageGuild`, `Manage Events` for
/// `ManageEvents`) and the Discord permission names for the rest.
const HELP_GROUPS: &[HelpGroup] = &[
    HelpGroup {
        title: "Leveling",
        requires: None,
        commands: &["rank", "leaderboard"],
    },
    HelpGroup {
        title: "Discovery",
        requires: None,
        commands: &["help"],
    },
    HelpGroup {
        title: "Community check-in",
        requires: Some("Manage Events"),
        commands: &["attendance"],
    },
    HelpGroup {
        title: "Automations",
        requires: Some("Manage Server"),
        commands: &[
            "command",
            "command-remove",
            "command-list",
            "schedule",
            "schedule-remove",
            "schedule-list",
            "sticky",
            "sticky-remove",
        ],
    },
    HelpGroup {
        title: "RSVPs",
        requires: None,
        commands: &["rsvp", "rsvp-attendance"],
    },
    HelpGroup {
        title: "Looking for group",
        requires: Some("Manage Events"),
        commands: &["lfg", "lfg-close"],
    },
    HelpGroup {
        title: "Feed relays",
        requires: Some("Manage Server"),
        commands: &["feed-add", "feed-remove", "feed-list"],
    },
    HelpGroup {
        title: "Bans",
        requires: Some("Ban Members"),
        commands: &["ban", "tempban"],
    },
    HelpGroup {
        title: "Kick",
        requires: Some("Kick Members, or a tracked voice room for a vote"),
        commands: &["kick"],
    },
    HelpGroup {
        title: "Timeouts and warnings",
        requires: Some("Moderate Members"),
        commands: &["timeout", "warn"],
    },
    HelpGroup {
        title: "Message cleanup",
        requires: Some("Manage Messages"),
        commands: &["purge"],
    },
    HelpGroup {
        title: "Channel slowmode and locks",
        requires: Some("Manage Channels"),
        commands: &["slowmode", "lockdown", "unlock"],
    },
    HelpGroup {
        title: "Voice rooms",
        requires: None,
        commands: &[
            "setup", "ping", "invite", "reclaim", "transfer", "name", "private", "public", "limit",
            "unlimit",
        ],
    },
    HelpGroup {
        title: "Voice room admin",
        requires: Some("Manage Channels"),
        commands: &[
            "create",
            "textchannels",
            "access",
            "logging",
            "position",
            "group",
            "inheritpermissions",
            "defaultlimit",
            "alwaysprivate",
        ],
    },
    HelpGroup {
        title: "Voice room settings and assistant",
        requires: Some("Manage Server"),
        commands: &["export", "import", "templateassistant"],
    },
];

/// Render the ephemeral `/help` reply for the given live publish set.
///
/// Only commands present in `defs` render, in publish order within each
/// group; empty groups are skipped. Large custom-command sets exceed the
/// plain content ceiling; the Discord adapter uses an embed to preserve names.
#[must_use]
pub fn help_text(defs: &[CommandDefinition]) -> String {
    let live: HashSet<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    let mut out = String::new();
    let _ = writeln!(out, "**Server commands** ({} live)", live.len(),);
    out.push_str("Gated commands need the listed permission.\n");
    for group in HELP_GROUPS {
        let present: Vec<_> = group
            .commands
            .iter()
            .filter(|name| live.contains(**name))
            .collect();
        if present.is_empty() {
            continue;
        }
        out.push('\n');
        match group.requires {
            Some(perm) => {
                let _ = writeln!(out, "**{}** — needs {}", group.title, perm);
            }
            None => {
                let _ = writeln!(out, "**{}**", group.title);
            }
        }
        let names: Vec<String> = present.iter().map(|name| format!("/{name}")).collect();
        out.push_str(&names.join(" · "));
        out.push('\n');
    }
    // DB custom rows publish alongside the builtins; anything else ungrouped
    // (a future builtin this table predates) still renders rather than
    // vanishing from discovery.
    let grouped: HashSet<&str> = HELP_GROUPS
        .iter()
        .flat_map(|g| g.commands.iter().copied())
        .collect();
    let mut custom = Vec::new();
    for def in defs {
        if !grouped.contains(def.name.as_str()) {
            custom.push(def.name.as_str());
        }
    }
    if !custom.is_empty() {
        out.push_str("\n**Custom**\n");
        out.push_str(
            &custom
                .iter()
                .map(|name| format!("/{name}"))
                .collect::<Vec<_>>()
                .join(" · "),
        );
        out.push('\n');
    }
    out.push_str(
        "\nThis is the last confirmed command list. Your picker hides gated commands you cannot use.",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::merge_commands;
    use crate::feature_commands::feature_commands;
    use crate::moderation::moderation_commands;
    use crate::router::{InteractionRouter, RouterGates};

    const GUILD: u64 = 2222;

    fn all_on() -> RouterGates {
        RouterGates {
            configured_guild: Some(GUILD),
            scorecard: true,
            automations: true,
            announcements: true,
            moderation: true,
            voice: true,
            voice_assistant: true,
            tickets: true,
            self_roles: true,
            onboarding_picker: true,
            session_picker: true,
        }
    }

    #[test]
    fn full_set_lists_every_live_command_exactly_once() {
        let defs = InteractionRouter::new(all_on())
            .publish_set(&[])
            .expect("full set assembles");
        let text = help_text(&defs);
        // Exact slash tokens: a substring count would triple-count `/command`
        // inside `/command-remove` and `/command-list`.
        let tokens: Vec<&str> = text
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '/' || c == '-' || c == '_'))
            .filter(|t| t.starts_with('/') && t.len() > 1)
            .collect();
        assert_eq!(tokens.len(), defs.len());
        for def in &defs {
            let token = format!("/{}", def.name);
            assert_eq!(
                tokens.iter().filter(|t| **t == token).count(),
                1,
                "{token} appears exactly once"
            );
        }
        // 28 legacy builtins plus the 21 gated voice commands and
        // `/templateassistant` (voice `kick` loses first-wins to moderation
        // `/kick`): matches the regenerated `docs/commands.md` total.
        assert!(text.contains("**Server commands** (50 live)"));
        assert!(
            !text.contains("**Custom**"),
            "every published name is grouped"
        );
        assert!(text.len() < 2000, "fits Discord's content ceiling");
    }

    #[test]
    fn gated_groups_carry_permission_hints() {
        let defs = InteractionRouter::new(all_on())
            .publish_set(&[])
            .expect("full set assembles");
        let text = help_text(&defs);
        for (group, perm) in [
            ("Automations", "Manage Server"),
            ("Feed relays", "Manage Server"),
            ("Bans", "Ban Members"),
            ("Message cleanup", "Manage Messages"),
            ("Voice room admin", "Manage Channels"),
            ("Voice room settings and assistant", "Manage Server"),
        ] {
            assert!(
                text.contains(&format!("**{group}** — needs {perm}")),
                "{group} hint missing"
            );
        }
        // Open groups carry no gate.
        assert!(text.contains("**Leveling**\n"));
        assert!(text.contains("**Discovery**\n/help\n"));
    }

    #[test]
    fn gated_off_features_disappear() {
        let off = InteractionRouter::new(RouterGates {
            scorecard: false,
            automations: false,
            announcements: false,
            moderation: false,
            voice: false,
            voice_assistant: false,
            ..all_on()
        });
        let defs = off.publish_set(&[]).expect("core-only set");
        let text = help_text(&defs);
        assert!(text.contains("**Server commands** (3 live)"));
        assert!(text.contains("/rank"));
        assert!(text.contains("/help"));
        assert!(!text.contains("Automations"));
        assert!(!text.contains("/ban"));
        assert!(!text.contains("Custom"));
    }

    #[test]
    fn custom_commands_render_under_custom() {
        let defs = InteractionRouter::new(all_on())
            .publish_set(&[crate::commands::CustomCommand {
                name: "faq".to_owned(),
                description: "FAQ".to_owned(),
                enabled: true,
            }])
            .expect("set with custom");
        let text = help_text(&defs);
        // The voice set is grouped, so the Custom section holds only the row.
        let custom = text
            .split("**Custom**\n")
            .nth(1)
            .expect("custom section renders");
        assert!(custom.starts_with("/faq\n"), "{custom}");
    }

    #[test]
    fn ungrouped_names_render_under_custom_not_dropped() {
        let mut defs = merge_commands(&[feature_commands(), moderation_commands()], &[])
            .expect("slices merge");
        defs.push(CommandDefinition::new(
            "future-cmd",
            "A builtin this table predates",
        ));
        let text = help_text(&defs);
        assert!(text.contains("**Custom**\n/future-cmd\n"));
    }

    #[test]
    fn every_known_builtin_is_grouped() {
        let grouped: HashSet<&str> = HELP_GROUPS
            .iter()
            .flat_map(|g| g.commands.iter().copied())
            .collect();
        // 27 legacy builtins plus the Next-only help command, then the 21
        // voice names (voice `kick` shares the Kick group) and
        // `/templateassistant`.
        assert_eq!(grouped.len(), 50);
        for name in [
            "rank",
            "leaderboard",
            "help",
            "attendance",
            "command",
            "schedule-list",
            "sticky-remove",
            "rsvp",
            "rsvp-attendance",
            "lfg",
            "lfg-close",
            "feed-add",
            "ban",
            "tempban",
            "kick",
            "timeout",
            "warn",
            "purge",
            "slowmode",
            "lockdown",
            "unlock",
        ]
        .into_iter()
        .chain(
            crate::voice_rooms::voice_commands()
                .iter()
                .map(|def| def.name.as_str()),
        )
        .chain(["templateassistant"])
        {
            assert!(grouped.contains(name), "{name} has a help group");
        }
    }
}
