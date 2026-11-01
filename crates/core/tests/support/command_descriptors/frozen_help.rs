//! Independent full-reply oracle for the legacy `/help` index.
//!
//! The non-voice layout comes from the frozen table, not the renderer under
//! test. These additional literals preserve the voice anchor, guidance,
//! unique-name header count, ordered Custom fallback and final notice.

use super::frozen_baseline::HELP_LAYOUT;
use std::collections::BTreeSet;
use std::fmt::Write as _;

type Group = (&'static str, Option<&'static str>, &'static [&'static str]);

const VOICE_LAYOUT: &[Group] = &[
    (
        "Voice rooms",
        None,
        &[
            "setup", "ping", "invite", "reclaim", "transfer", "name", "private", "public", "limit",
            "unlimit",
        ],
    ),
    (
        "Voice room admin",
        Some("Manage Channels"),
        &[
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
    ),
    (
        "Voice room settings",
        Some("Manage Server"),
        &["export", "import"],
    ),
];

pub const RSVP_GUIDANCE: &str = "Paste the event id or its link, e.g. https://discord.com/events/<server-id>/<event-id>.\nTotals include your saved response and saved time; live event availability is not checked.\n";

pub const FOOTER: &str =
    "\nThis is the last confirmed command list. Your picker hides gated commands you cannot use.";

/// The 21 voice-only names. The shared `/kick` belongs to the frozen table.
pub fn voice_names() -> impl Iterator<Item = &'static str> {
    VOICE_LAYOUT
        .iter()
        .flat_map(|(_, _, names)| names.iter().copied())
}

/// Expected bytes for an independently chosen input, including repeated
/// Custom names. Do not pass names obtained from the actual rendered reply.
pub fn expected_help_text(names: &[&str]) -> String {
    let live: BTreeSet<&str> = names.iter().copied().collect();
    let mut out = format!(
        "**Server commands** ({} live)\nGated commands need the listed permission.\n",
        live.len()
    );
    let voice = live.contains("textchannels");
    let groups = HELP_LAYOUT
        .iter()
        .chain(VOICE_LAYOUT.iter().filter(|_| voice));
    let mut grouped = BTreeSet::new();
    for (title, needs, members) in groups {
        let present: Vec<&str> = members
            .iter()
            .copied()
            .filter(|name| live.contains(name))
            .collect();
        if present.is_empty() {
            continue;
        }
        let _ = write!(out, "\n**{title}**");
        if let Some(needs) = needs {
            let _ = write!(out, " — needs {needs}");
        }
        out.push('\n');
        out.push_str(
            &present
                .iter()
                .map(|name| format!("/{name}"))
                .collect::<Vec<_>>()
                .join(" · "),
        );
        out.push('\n');
        if present.contains(&"rsvp-attendance") {
            out.push_str(RSVP_GUIDANCE);
        }
        grouped.extend(present);
    }
    let custom: Vec<String> = names
        .iter()
        .filter(|name| !grouped.contains(**name))
        .map(|name| format!("/{name}"))
        .collect();
    if !custom.is_empty() {
        out.push_str("\n**Custom**\n");
        out.push_str(&custom.join(" · "));
        out.push('\n');
    }
    out.push_str(FOOTER);
    out
}
