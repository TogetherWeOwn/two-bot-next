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
//! table predates) render under Custom rather than being dropped. The voice
//! groups also wait for the voice set itself, so a stored custom row that
//! shares a voice name stays under Custom while voice is off.

use std::collections::HashSet;
use std::fmt::Write as _;

use super::commands::{
    CommandDefinition, CommandOption, PERM_ADMINISTRATOR, PERM_BAN_MEMBERS,
    PERM_CREATE_INSTANT_INVITE, PERM_KICK_MEMBERS, PERM_MANAGE_CHANNELS, PERM_MANAGE_EVENTS,
    PERM_MANAGE_GUILD, PERM_MANAGE_MESSAGES, PERM_MODERATE_MEMBERS,
};
use super::message_safety;

/// One `/help` group: commands that share an audience.
struct HelpGroup {
    title: &'static str,
    /// Permission hint for the group header; `None` means everyone.
    requires: Option<&'static str>,
    commands: &'static [&'static str],
    /// The group renders only while this name is also live. Voice names are
    /// ordinary user-choosable words (`/ping`, `/invite`), so with voice off a
    /// stored custom row of that name must stay under Custom.
    anchor: Option<&'static str>,
}

/// Groups in publish order. The `requires` labels mirror the legacy refusal
/// texts (`Manage Server` for `ManageGuild`, `Manage Events` for
/// `ManageEvents`) and the Discord permission names for the rest.
const HELP_GROUPS: &[HelpGroup] = &[
    HelpGroup {
        title: "Leveling",
        requires: None,
        commands: &["rank", "leaderboard"],
        anchor: None,
    },
    HelpGroup {
        title: "Discovery",
        requires: None,
        commands: &["help"],
        anchor: None,
    },
    HelpGroup {
        title: "Privacy",
        requires: None,
        commands: &["privacy"],
        anchor: None,
    },
    HelpGroup {
        title: "Pickers",
        requires: None,
        commands: &["reopen"],
        anchor: None,
    },
    HelpGroup {
        title: "Welcome preview",
        requires: Some("Manage Server"),
        commands: &["welcome-preview"],
        anchor: None,
    },
    HelpGroup {
        title: "Community check-in",
        requires: Some("Manage Events"),
        commands: &["attendance"],
        anchor: None,
    },
    HelpGroup {
        title: "Automations",
        requires: Some("Manage Server"),
        commands: &[
            "command",
            "command-remove",
            "command-list",
            "command-inspect",
            "command-disable",
            "command-enable",
            "schedule",
            "schedule-remove",
            "schedule-list",
            "sticky",
            "sticky-remove",
        ],
        anchor: None,
    },
    HelpGroup {
        title: "Giveaways",
        requires: Some("Manage Server"),
        commands: &["giveaway"],
        anchor: None,
    },
    HelpGroup {
        title: "RSVPs",
        requires: None,
        commands: &["rsvp", "rsvp-attendance"],
        anchor: None,
    },
    HelpGroup {
        title: "Looking for group",
        requires: Some("Manage Events"),
        commands: &["lfg", "lfg-close", "lfg-edit", "lfg-list"],
        anchor: None,
    },
    HelpGroup {
        title: "Feed relays",
        requires: Some("Manage Server"),
        commands: &[
            "feed-add",
            "feed-remove",
            "feed-pause",
            "feed-resume",
            "feed-list",
            "feed-inspect",
        ],
        anchor: None,
    },
    HelpGroup {
        title: "Bans",
        requires: Some("Ban Members"),
        commands: &["ban", "tempban"],
        anchor: None,
    },
    HelpGroup {
        title: "Kick",
        requires: Some("Kick Members, or a tracked voice room for a vote"),
        commands: &["kick"],
        anchor: None,
    },
    HelpGroup {
        title: "Timeouts and warnings",
        requires: Some("Moderate Members"),
        commands: &["timeout", "warn"],
        anchor: None,
    },
    HelpGroup {
        title: "Message cleanup",
        requires: Some("Manage Messages"),
        commands: &["purge", "purge-preview"],
        anchor: None,
    },
    HelpGroup {
        title: "Channel slowmode and locks",
        requires: Some("Manage Channels"),
        commands: &["slowmode", "lockdown", "unlock", "channel-inspect"],
        anchor: None,
    },
    HelpGroup {
        title: "Game activity (private, live guild unavailable)",
        requires: Some("Manage Server"),
        commands: &["game-mix"],
        anchor: None,
    },
    HelpGroup {
        title: "Voice rooms",
        requires: None,
        commands: &[
            "setup", "ping", "invite", "reclaim", "transfer", "name", "private", "public", "limit",
            "unlimit", "myroom",
        ],
        anchor: Some("textchannels"),
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
        anchor: Some("textchannels"),
    },
    // `/templateassistant` stays out until its slice lands with a handler;
    // the slice restores the name and the "and assistant" title.
    HelpGroup {
        title: "Voice room settings",
        requires: Some("Manage Server"),
        commands: &["export", "import"],
        anchor: Some("textchannels"),
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
    let mut rendered: HashSet<&str> = HashSet::new();
    for group in HELP_GROUPS {
        if group.anchor.is_some_and(|anchor| !live.contains(anchor)) {
            continue;
        }
        let present: Vec<_> = group
            .commands
            .iter()
            .filter(|name| live.contains(**name))
            .collect();
        if present.is_empty() {
            continue;
        }
        rendered.extend(present.iter().map(|name| **name));
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
        if group.commands.contains(&"rsvp-attendance") && live.contains("rsvp-attendance") {
            out.push_str("Paste the event id or its link, e.g. https://discord.com/events/<server-id>/<event-id>.\n");
            out.push_str("Totals include your saved response and saved time; live event availability is not checked.\n");
        }
    }
    // DB custom rows publish alongside the builtins; anything else ungrouped
    // (a future builtin this table predates) still renders rather than
    // vanishing from discovery.
    let mut custom = Vec::new();
    for def in defs {
        if !rendered.contains(def.name.as_str()) {
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

// ---------------------------------------------------------------------------
// Confirmed-publication command detail (`/help command:name`).
//
// The renderer below is definition-driven: every description, option,
// constraint, choice, subcommand path and permission sentence comes from the
// single confirmed definition the caller passes in. There is no per-command
// duplicate source and nothing is guessed: an omitted `required` flag reads
// as optional, an unknown option type says so with its wire integer, and
// unknown permission bits keep their decimal bitfield.
// ---------------------------------------------------------------------------

/// UTF-16 budget for one detail reply: the embed description ceiling minus
/// room for the truncation notice.
const DETAIL_BUDGET: usize = message_safety::EMBED_DESCRIPTION_LIMIT - 64;
/// One hostile field must not eat the whole budget.
const DESCRIPTION_CAP: usize = 400;
const OPTION_DESCRIPTION_CAP: usize = 200;
const CHOICE_TEXT_CAP: usize = 100;
/// Structural caps; every cut emits an explicit omission notice.
const DETAIL_OPTIONS_CAP: usize = 12;
const CHOICES_PER_OPTION_CAP: usize = 8;
const USAGE_LINES_CAP: usize = 20;
const NESTING_CAP: usize = 4;

/// Canonical `/help command:` selector rule, as documented in the command
/// reference: trim surrounding whitespace, accept one optional leading
/// slash, compare ASCII case-insensitively. No aliases or prefix expansion.
/// Returns `None` for an empty selector, which answers the correction.
#[must_use]
pub fn canonical_help_selector(input: &str) -> Option<String> {
    let trimmed = input.trim();
    let bare = trimmed.strip_prefix('/').unwrap_or(trimmed);
    if bare.is_empty() {
        return None;
    }
    Some(bare.to_ascii_lowercase())
}

/// Concise correction for unknown or unpublished `/help command:` input.
/// Points at the confirmed index without echoing the input (which may be
/// hostile) or advertising unpublished names.
#[must_use]
pub fn unknown_help_correction() -> String {
    "Unknown command — no confirmed command matches that name.\n\
     Use /help to see this server's last confirmed command list."
        .to_owned()
}

/// Render untrusted definition text inert: strip markdown/mention-active
/// punctuation and flatten newlines so a hostile description cannot forge
/// markup, links or mentions. Reply-level mention suppression stays the
/// mention backstop. Generated framing (usage brackets, section headers)
/// never passes through here.
fn inert(text: &str) -> String {
    text.chars()
        .filter_map(|ch| match ch {
            '*' | '`' | '<' | '>' | '[' | ']' | '(' | ')' | '\\' => None,
            '\n' | '\r' | '\t' => Some(' '),
            _ => Some(ch),
        })
        .collect()
}

/// Truncate already-inert text to `limit` UTF-16 units, with an explicit
/// marker when anything was cut.
fn cap(text: &str, limit: usize) -> String {
    if message_safety::text_len(text) <= limit {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut remaining = limit;
    for ch in text.chars() {
        let width = ch.len_utf16();
        if width > remaining {
            break;
        }
        out.push(ch);
        remaining -= width;
    }
    out.push_str("…(truncated)");
    out
}

/// Discord application-command option type names by wire integer (1 and 2
/// are sub-commands; 9 and 10 have no builder yet but already publish).
/// Anything else is reported truthfully with its integer, never guessed.
fn option_type_name(kind: u8) -> String {
    match kind {
        1 => "subcommand".to_owned(),
        2 => "subcommand group".to_owned(),
        3 => "string".to_owned(),
        4 => "integer".to_owned(),
        5 => "boolean".to_owned(),
        6 => "user".to_owned(),
        7 => "channel".to_owned(),
        8 => "role".to_owned(),
        9 => "mentionable".to_owned(),
        10 => "number".to_owned(),
        11 => "attachment".to_owned(),
        _ => format!("unknown type {kind}"),
    }
}

/// Discord channel type names by wire integer; unmapped integers keep
/// their number instead of guessing a channel kind.
fn channel_type_name(kind: u8) -> String {
    match kind {
        0 => "text".to_owned(),
        1 => "DM".to_owned(),
        2 => "voice".to_owned(),
        4 => "category".to_owned(),
        5 => "announcement".to_owned(),
        10..=12 => "thread".to_owned(),
        13 => "stage".to_owned(),
        15 => "forum".to_owned(),
        16 => "media".to_owned(),
        _ => format!("type {kind}"),
    }
}

/// Declared bounds for one leaf option, from the definition only.
fn constraint_suffix(option: &CommandOption) -> String {
    match option.kind {
        4 | 10 => match (option.min_value, option.max_value) {
            (Some(min), Some(max)) => format!(", {min} to {max}"),
            (Some(min), None) => format!(", min {min}"),
            (None, Some(max)) => format!(", max {max}"),
            (None, None) => String::new(),
        },
        3 => match (option.min_length, option.max_length) {
            (Some(min), Some(max)) => format!(", length {min} to {max}"),
            (Some(min), None) => format!(", min {min} chars"),
            (None, Some(max)) => format!(", max {max} chars"),
            (None, None) => String::new(),
        },
        7 if !option.channel_types.is_empty() => {
            let names: Vec<String> = option
                .channel_types
                .iter()
                .map(|kind| channel_type_name(*kind))
                .collect();
            format!(", {}", names.join(", "))
        }
        _ => String::new(),
    }
}

const PERMISSION_CAVEAT: &str = "This help does not evaluate your effective permissions; \
     your picker hides gated commands you cannot use.";

/// The definition's declared permission requirement. `None` means Discord
/// carries no gate; explicit zero bits are administrator-only; unknown bits
/// keep their decimal bitfield instead of reading as unrestricted.
fn permission_sentence(permissions: Option<&str>) -> String {
    let mut out = String::from("Permissions: ");
    match permissions {
        None => out.push_str("none declared. "),
        Some(raw) => match raw.parse::<u64>() {
            Ok(0) => out.push_str("administrator-only (explicit zero permission bits). "),
            Ok(bits) => {
                let mut names = Vec::new();
                let mut rest = bits;
                for (bit, name) in [
                    (PERM_CREATE_INSTANT_INVITE, "Create Instant Invite"),
                    (PERM_KICK_MEMBERS, "Kick Members"),
                    (PERM_BAN_MEMBERS, "Ban Members"),
                    (PERM_ADMINISTRATOR, "Administrator"),
                    (PERM_MANAGE_CHANNELS, "Manage Channels"),
                    (PERM_MANAGE_GUILD, "Manage Server"),
                    (PERM_MANAGE_MESSAGES, "Manage Messages"),
                    (PERM_MANAGE_EVENTS, "Manage Events"),
                    (PERM_MODERATE_MEMBERS, "Moderate Members"),
                ] {
                    if rest & bit != 0 {
                        names.push(name.to_owned());
                        rest &= !bit;
                    }
                }
                if rest != 0 {
                    names.push(format!("unknown bits {rest}"));
                }
                if names.is_empty() {
                    names.push(format!("unknown bits {bits}"));
                }
                out.push_str(&names.join(", "));
                out.push_str(". ");
            }
            Err(_) => out.push_str("unknown permission value. "),
        },
    }
    out.push_str(PERMISSION_CAVEAT);
    out
}

/// A detail reply under construction: chunks stop at the Discord budget and
/// the finished text always says when it was cut.
#[derive(Default)]
struct Detail {
    text: String,
    used: usize,
    truncated: bool,
}

impl Detail {
    fn push(&mut self, chunk: &str) {
        if self.truncated {
            return;
        }
        let len = message_safety::text_len(chunk);
        if self.used + len <= DETAIL_BUDGET {
            self.text.push_str(chunk);
            self.used += len;
            return;
        }
        let mut remaining = DETAIL_BUDGET.saturating_sub(self.used);
        for ch in chunk.chars() {
            let width = ch.len_utf16();
            if width > remaining {
                break;
            }
            self.text.push(ch);
            remaining -= width;
        }
        self.used = DETAIL_BUDGET.saturating_sub(remaining);
        self.truncated = true;
    }

    fn finish(mut self) -> String {
        if self.truncated {
            // Cut back to a line boundary so the notice never continues a
            // half-word.
            if let Some(pos) = self.text.rfind('\n') {
                self.text.truncate(pos + 1);
            }
            self.text
                .push_str("…(detail truncated; /help lists every confirmed command.)\n");
        }
        self.text
    }
}

/// How many option-detail lines rendered, for the structural omission notice.
#[derive(Default)]
struct DetailState {
    shown: usize,
    omitted: usize,
}

fn is_nested(option: &CommandOption) -> bool {
    option.kind == 1 || option.kind == 2
}

fn requiredness(option: &CommandOption) -> &'static str {
    if option.required.unwrap_or(false) {
        "required"
    } else {
        "optional"
    }
}

/// `<required>` and `[optional]` placeholders in definition order.
fn usage_suffix(leaves: &[&CommandOption]) -> String {
    let mut out = String::new();
    for leaf in leaves {
        if leaf.required.unwrap_or(false) {
            let _ = write!(out, " <{}>", inert(&leaf.name));
        } else {
            let _ = write!(out, " [{}]", inert(&leaf.name));
        }
    }
    out
}

/// One usage line per sub-command leaf path, bounded and deterministic.
fn collect_usage(
    detail: &mut Detail,
    path: &str,
    option: &CommandOption,
    depth: usize,
    lines: &mut usize,
    omitted: &mut usize,
) {
    let here = format!("{path} {}", inert(&option.name));
    let (nested, leaves): (Vec<&CommandOption>, Vec<&CommandOption>) =
        option.options.iter().partition(|child| is_nested(child));
    if nested.is_empty() {
        if *lines < USAGE_LINES_CAP {
            detail.push(&format!("- {here}{}\n", usage_suffix(&leaves)));
            *lines += 1;
        } else {
            *omitted += 1;
        }
        return;
    }
    if depth >= NESTING_CAP {
        *omitted += 1;
        return;
    }
    for child in &nested {
        collect_usage(detail, &here, child, depth + 1, lines, omitted);
    }
    if !leaves.is_empty() && *lines < USAGE_LINES_CAP {
        detail.push(&format!("- {here}{}\n", usage_suffix(&leaves)));
        *lines += 1;
    } else if !leaves.is_empty() {
        *omitted += 1;
    }
}

fn push_usage(detail: &mut Detail, def: &CommandDefinition) {
    let name = inert(&def.name);
    let (nested, leaves): (Vec<&CommandOption>, Vec<&CommandOption>) =
        def.options.iter().partition(|option| is_nested(option));
    if nested.is_empty() {
        detail.push(&format!("Usage: /{name}{}\n", usage_suffix(&leaves)));
        return;
    }
    detail.push("Usage:\n");
    let mut lines = 0usize;
    let mut omitted = 0usize;
    for option in &nested {
        collect_usage(
            detail,
            &format!("/{name}"),
            option,
            0,
            &mut lines,
            &mut omitted,
        );
    }
    if !leaves.is_empty() {
        // Discord forbids mixing, but a defensive fixture still renders.
        detail.push(&format!("- /{name}{}\n", usage_suffix(&leaves)));
    }
    if omitted > 0 {
        detail.push(&format!("- …({omitted} more paths omitted.)\n"));
    }
}

/// One detail line per leaf argument plus its bounded choice list.
fn push_option_lines(detail: &mut Detail, leaves: &[&CommandOption], state: &mut DetailState) {
    for leaf in leaves {
        if state.shown >= DETAIL_OPTIONS_CAP {
            state.omitted += 1;
            continue;
        }
        state.shown += 1;
        let description = cap(&inert(&leaf.description), OPTION_DESCRIPTION_CAP);
        detail.push(&format!(
            "- {} ({}, {}{}): {description}\n",
            inert(&leaf.name),
            option_type_name(leaf.kind),
            requiredness(leaf),
            constraint_suffix(leaf),
        ));
        if leaf.choices.is_empty() {
            continue;
        }
        let shown: Vec<String> = leaf
            .choices
            .iter()
            .take(CHOICES_PER_OPTION_CAP)
            .map(|choice| {
                let name = cap(&inert(&choice.name), CHOICE_TEXT_CAP);
                let value = cap(&inert(&choice.value), CHOICE_TEXT_CAP);
                format!("{name} ({value})")
            })
            .collect();
        let mut line = format!("  Choices: {}", shown.join(", "));
        if leaf.choices.len() > CHOICES_PER_OPTION_CAP {
            let _ = write!(
                line,
                " (+{} more choices, omitted)",
                leaf.choices.len() - CHOICES_PER_OPTION_CAP
            );
        }
        line.push('\n');
        detail.push(&line);
    }
}

/// Sub-command and group descriptions in definition order with their paths.
fn push_nested(
    detail: &mut Detail,
    path: &str,
    option: &CommandOption,
    depth: usize,
    state: &mut DetailState,
) {
    if state.shown >= DETAIL_OPTIONS_CAP {
        state.omitted += 1;
        return;
    }
    state.shown += 1;
    let here = format!("{path} {}", inert(&option.name));
    let description = cap(&inert(&option.description), OPTION_DESCRIPTION_CAP);
    detail.push(&format!("- {here} — {description}\n"));
    let (nested, leaves): (Vec<&CommandOption>, Vec<&CommandOption>) =
        option.options.iter().partition(|child| is_nested(child));
    if !leaves.is_empty() {
        detail.push(&format!("Arguments for {here}:\n"));
        push_option_lines(detail, &leaves, state);
    }
    if depth >= NESTING_CAP {
        state.omitted += nested.len();
        return;
    }
    for child in &nested {
        push_nested(detail, &here, child, depth + 1, state);
    }
}

fn push_arguments(detail: &mut Detail, def: &CommandDefinition) {
    let (nested, leaves): (Vec<&CommandOption>, Vec<&CommandOption>) =
        def.options.iter().partition(|option| is_nested(option));
    let mut state = DetailState::default();
    if !leaves.is_empty() {
        detail.push("Arguments:\n");
        push_option_lines(detail, &leaves, &mut state);
    }
    if !nested.is_empty() {
        detail.push("Subcommands:\n");
        let root = format!("/{}", inert(&def.name));
        for option in &nested {
            push_nested(detail, &root, option, 0, &mut state);
        }
    }
    if state.omitted > 0 {
        detail.push(&format!(
            "(+{} more options, omitted from this detail.)\n",
            state.omitted
        ));
    }
}

impl CommandDefinition {
    /// Render one confirmed command as private help detail: description,
    /// formable usage, per-argument types with requiredness and declared
    /// constraints, bounded choices, nested sub-command paths,
    /// definition-carried examples and the declared permission requirement.
    ///
    /// Pure data in, text out: the caller passes the last confirmed
    /// definition, never a fresh read. Output stays within one Discord
    /// message budget with an explicit truncation notice, and untrusted
    /// definition text renders inert.
    #[must_use]
    pub fn help_detail_text(&self) -> String {
        let mut detail = Detail::default();
        let description = cap(&inert(&self.description), DESCRIPTION_CAP);
        detail.push(&format!("/{0} — {description}\n", inert(&self.name)));
        push_usage(&mut detail, self);
        push_arguments(&mut detail, self);
        if !self.examples.is_empty() {
            detail.push("Examples:\n");
            for example in &self.examples {
                detail.push(&format!("- {example}\n"));
            }
        }
        detail.push(&permission_sentence(
            self.default_member_permissions.as_deref(),
        ));
        detail.push("\n");
        detail.finish()
    }
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
        // The RSVP note carries a copyable event-link example whose slashes
        // tokenize as path fragments, not commands; exclude it from the
        // exact-once command count.
        let without_example = text.replace("https://discord.com/events/<server-id>/<event-id>", "");
        // Exact slash tokens: a substring count would triple-count `/command`
        // inside `/command-remove` and `/command-list`.
        let tokens: Vec<&str> = without_example
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
        // 43 non-voice commands (7 core + 25 feature + 9 moderation + 2 reads),
        // including inspection, reopen, welcome-preview, pause/resume, giveaway, lfg-edit, lfg-list, feed-inspect,
        // purge-preview and channel-inspect, plus 22 additional voice commands (voice
        // `kick` loses first-wins to moderation `/kick`). `/templateassistant` stays
        // unpublished until its slice lands: matches the regenerated `docs/commands.md` total.
        assert!(text.contains("**Server commands** (65 live)"));
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
            ("Giveaways", "Manage Server"),
            ("Bans", "Ban Members"),
            ("Message cleanup", "Manage Messages"),
            ("Voice room admin", "Manage Channels"),
            ("Voice room settings", "Manage Server"),
        ] {
            assert!(
                text.contains(&format!("**{group}** — needs {perm}")),
                "{group} hint missing"
            );
        }
        for line in [
            "**Giveaways** — needs Manage Server\n/giveaway\n",
            "**Message cleanup** — needs Manage Messages\n/purge · /purge-preview\n",
            "**Channel slowmode and locks** — needs Manage Channels\n/slowmode · /lockdown · /unlock · /channel-inspect\n",
            "**Voice rooms**\n/setup · /ping · /invite · /reclaim · /transfer · /name · /private · /public · /limit · /unlimit · /myroom\n",
            "**Voice room admin** — needs Manage Channels\n/create · /textchannels · /access · /logging · /position · /group · /inheritpermissions · /defaultlimit · /alwaysprivate\n",
            "**Voice room settings** — needs Manage Server\n/export · /import\n",
        ] {
            assert!(text.contains(line), "{line}");
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
        assert!(text.contains("**Server commands** (7 live)"));
        assert!(text.contains("/rank"));
        assert!(text.contains("/help"));
        assert!(!text.contains("Automations"));
        assert!(!text.contains("Giveaways"));
        assert!(!text.contains("/giveaway"));
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
    fn voice_named_custom_rows_stay_custom_while_voice_is_off() {
        let rows: Vec<_> = ["ping", "export", "invite"]
            .into_iter()
            .map(|name| crate::commands::CustomCommand {
                name: name.to_owned(),
                description: "Stored before the voice reservation".to_owned(),
                enabled: true,
            })
            .collect();
        let off = InteractionRouter::new(RouterGates {
            voice: false,
            voice_assistant: false,
            ..all_on()
        });
        let text = help_text(&off.publish_set(&rows).expect("set with rows"));
        assert!(!text.contains("Voice room"), "{text}");
        let custom = text.split("**Custom**\n").nth(1).expect("custom section");
        assert!(
            custom.starts_with("/ping · /export · /invite\n"),
            "{custom}"
        );
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
        // 43 non-voice names, including inspection, reopen, welcome-preview, pause/resume,
        // giveaway, lfg-edit, lfg-list, feed-inspect, purge-preview and channel-inspect, plus
        // 22 additional voice names (voice `kick` shares the Kick group).
        // `/templateassistant` stays out until its slice lands.
        assert_eq!(grouped.len(), 65);
        for name in [
            "rank",
            "leaderboard",
            "help",
            "privacy",
            "reopen",
            "welcome-preview",
            "game-mix",
            "attendance",
            "command",
            "command-disable",
            "command-enable",
            "command-inspect",
            "schedule-list",
            "sticky-remove",
            "rsvp",
            "rsvp-attendance",
            "giveaway",
            "lfg",
            "lfg-close",
            "lfg-edit",
            "lfg-list",
            "feed-add",
            "feed-inspect",
            "ban",
            "tempban",
            "kick",
            "timeout",
            "warn",
            "purge",
            "slowmode",
            "lockdown",
            "unlock",
            "purge-preview",
            "channel-inspect",
        ]
        .into_iter()
        .chain(
            crate::voice_rooms::voice_commands()
                .iter()
                .map(|def| def.name.as_str()),
        ) {
            assert!(grouped.contains(name), "{name} has a help group");
        }
    }

    #[test]
    fn selector_rule_trims_one_slash_and_ascii_case() {
        for (input, expected) in [
            ("rank", Some("rank")),
            ("RANK", Some("rank")),
            ("/rank", Some("rank")),
            ("  /RaNk  ", Some("rank")),
            ("//rank", Some("/rank")),
            ("ran", Some("ran")),
            ("", None),
            ("   ", None),
            ("/", None),
        ] {
            assert_eq!(
                canonical_help_selector(input).as_deref(),
                expected,
                "{input:?}"
            );
        }
    }

    #[test]
    fn correction_points_at_confirmed_index_without_echoing_input() {
        let text = unknown_help_correction();
        assert!(text.contains("/help"));
        assert!(text.to_lowercase().contains("confirmed"));
        assert!(text.to_lowercase().contains("unknown"));
        assert!(!text.contains("templateassistant"));
        assert!(!text.contains("Ban a"));
    }

    #[test]
    fn inert_text_keeps_words_drops_markup() {
        assert_eq!(inert("**bold** <@123>"), "bold @123");
        assert_eq!(
            inert("[label](https://invalid.example/x) `code`"),
            "labelhttps://invalid.example/x code"
        );
        assert_eq!(inert("line\nbreak"), "line break");
    }

    #[test]
    fn help_detail_carries_its_examples_and_selector() {
        let help = crate::commands::core_commands()
            .into_iter()
            .find(|def| def.name == "help")
            .expect("help stays registered");
        let text = help.help_detail_text();
        assert!(text.contains("/help command:rank"));
        assert!(text.contains("/help command:privacy"));
        assert!(text.contains("max 32 chars"));
    }

    #[test]
    fn wide_rendered_detail_fits_single_embed_description() {
        use crate::commands::{CommandChoice, CommandOption, CommandOptionType};

        let options = (0..25)
            .map(|n| {
                CommandOption::new(
                    &format!("option{n:02}"),
                    &format!("Argument {n:02}"),
                    CommandOptionType::String,
                )
                .choices(
                    (0..25)
                        .map(|c| CommandChoice {
                            name: format!("Label {c:02} {}", "😀".repeat(40)),
                            value: format!("value_{c:02}"),
                        })
                        .collect(),
                )
            })
            .collect();
        let def = CommandDefinition::new("maximum", "Valid wide definition").options(options);
        let text = def.help_detail_text();
        let units = text.encode_utf16().count();
        assert!(
            units > message_safety::CONTENT_LIMIT,
            "exercise the embed path"
        );
        assert!(
            units <= message_safety::EMBED_DESCRIPTION_LIMIT,
            "rendered detail exceeds one embed description: {units} UTF-16 units"
        );
        assert!(text.ends_with("\n…(detail truncated; /help lists every confirmed command.)\n"));
        assert_eq!(text, def.help_detail_text(), "deterministic truncation");
    }

    #[test]
    fn declared_min_length_renders_alongside_max_length() {
        let mut option = crate::commands::CommandOption::new(
            "label",
            "Short text",
            crate::commands::CommandOptionType::String,
        );
        option.min_length = Some(2);
        option.max_length = Some(37);
        let text = CommandDefinition::new("bounds", "Limits").help_detail_text();
        assert!(text.contains("Usage: /bounds"));
        let with_option = CommandDefinition::new("bounds", "Limits").options(vec![option]);
        let rendered = with_option.help_detail_text();
        assert!(rendered.contains("length 2 to 37"));
    }

    #[test]
    fn defensive_shapes_stay_truthful_without_guessing() {
        let mut mention = crate::commands::CommandOption::new(
            "witness",
            "Who to mention",
            crate::commands::CommandOptionType::String,
        );
        mention.kind = 9;
        let channel = crate::commands::CommandOption::new(
            "where",
            "Where",
            crate::commands::CommandOptionType::Channel,
        )
        .channel_types(vec![0, 99]);
        let mut def = CommandDefinition::new("shapes", "Defensive shapes")
            .options(vec![mention, channel])
            .permissions(crate::commands::PERM_MANAGE_GUILD | (1 << 59));
        let text = def.help_detail_text();
        assert!(text.contains("mentionable"));
        assert!(text.contains("text") && text.contains("type 99"));
        assert!(text.contains("Manage Server") && text.contains("unknown bits"));
        def.default_member_permissions = Some("not-a-bitfield".to_owned());
        assert!(def.help_detail_text().contains("unknown permission value"));
        // Mixed sub-command and leaf options still render every path.
        let mixed = CommandDefinition::new("mixed", "Mixed nesting").options(vec![
            crate::commands::CommandOption::new(
                "group",
                "Group",
                crate::commands::CommandOptionType::SubCommand,
            )
            .sub_options(vec![
                crate::commands::CommandOption::new(
                    "leaf",
                    "Leaf",
                    crate::commands::CommandOptionType::SubCommand,
                ),
                crate::commands::CommandOption::new(
                    "note",
                    "Note",
                    crate::commands::CommandOptionType::String,
                ),
            ]),
            crate::commands::CommandOption::new(
                "top",
                "Top",
                crate::commands::CommandOptionType::String,
            ),
        ]);
        let rendered = mixed.help_detail_text();
        assert!(rendered.contains("/mixed group leaf"));
        assert!(rendered.contains("[top]"));
    }
}
