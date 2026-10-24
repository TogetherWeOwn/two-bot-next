//! Protected acceptance: the publish set equals the handled set.
//!
//! Every slash name the router publishes must route to a handler, and every
//! name `/help` lists must be routable. The voice set is the single designed
//! exception: those names publish but route `Ignore` (the voice sink answers
//! them; `router.rs` yields silently). `/templateassistant` is published and
//! listed yet routes `Unknown` ("no handler yet") — the only published
//! command in that state — so the contract tests below fail until the builder
//! unpublishes it. The parity and matrix controls pass before and after.
//!
//! No guild, no network, no database. The builder must not edit this file;
//! GREEN comes from the production fix (unpublish), never from test edits.

use std::collections::HashSet;
use two_bot_core::command_permissions::command_permission;
use two_bot_core::e2e_matrix::e2e_command_matrix;
use two_bot_core::help::help_text;
use two_bot_core::moderation::ModerationAction;
use two_bot_core::router::{HandlerId, InteractionRouter, RouterGates, SlashContext, SlashOutcome};
use two_bot_core::voice_rooms::voice_commands;

const GUILD: u64 = 2222;
const PERMS: u64 = u64::MAX;

fn all_gates_on() -> RouterGates {
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

fn parity_gates() -> RouterGates {
    RouterGates {
        voice: false,
        voice_assistant: false,
        ..all_gates_on()
    }
}

fn ctx(name: &str) -> SlashContext<'_> {
    SlashContext {
        name,
        guild_id: Some(GUILD),
        actor_permissions: Some(PERMS),
        custom_row: None,
    }
}

/// Names published by the public voice set. They route `Ignore` by design —
/// the voice sink owns them — so they are outside the handled contract.
fn voice_names() -> HashSet<String> {
    voice_commands().into_iter().map(|def| def.name).collect()
}

#[test]
fn every_published_non_voice_command_routes_to_a_handler() {
    let router = InteractionRouter::new(all_gates_on());
    let defs = router.publish_set(&[]).expect("full set assembles");
    let voice = voice_names();
    let mut unhandled = Vec::new();
    for def in &defs {
        if voice.contains(&def.name) {
            continue;
        }
        match router.route_slash(&ctx(&def.name)) {
            SlashOutcome::Handled { .. } => {}
            other => unhandled.push((def.name.clone(), format!("{other:?}"))),
        }
    }
    assert!(
        unhandled.is_empty(),
        "published but unhandled (no published command may lack a handler): {unhandled:?}"
    );
}

#[test]
fn every_published_non_voice_command_has_a_permission_row() {
    let router = InteractionRouter::new(all_gates_on());
    let defs = router.publish_set(&[]).expect("full set assembles");
    let voice = voice_names();
    let mut missing = Vec::new();
    for def in &defs {
        if voice.contains(&def.name) {
            continue;
        }
        if command_permission(&def.name).is_none() {
            missing.push(def.name.clone());
        }
    }
    assert!(
        missing.is_empty(),
        "published but without a COMMAND_PERMISSIONS row: {missing:?}"
    );
}

#[test]
fn help_lists_only_handled_commands() {
    let router = InteractionRouter::new(all_gates_on());
    let defs = router.publish_set(&[]).expect("full set assembles");
    let text = help_text(&defs);
    let voice = voice_names();
    // Exact slash tokens (same tokenizer as the help unit tests: a substring
    // count would triple-count `/command` inside `/command-remove`).
    let tokens: Vec<&str> = text
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '/' || c == '-' || c == '_'))
        .filter(|t| t.starts_with('/') && t.len() > 1)
        .collect();
    let mut unhandled = Vec::new();
    for token in &tokens {
        let name = &token[1..];
        if voice.contains(name) {
            continue;
        }
        match router.route_slash(&ctx(name)) {
            SlashOutcome::Handled { .. } => {}
            other => unhandled.push(((*token).to_owned(), format!("{other:?}"))),
        }
    }
    assert!(
        unhandled.is_empty(),
        "/help lists commands with no handler: {unhandled:?}"
    );
}

/// Registry compatibility: all 36 parity and Next-only builtins plus the
/// separate lfg-list discovery case route to their handlers;
/// templateassistant stays unpublished.
#[test]
fn parity_builtins_route_to_their_handlers() {
    let router = InteractionRouter::new(parity_gates());
    let cases: &[(&str, HandlerId)] = &[
        ("rank", HandlerId::Rank),
        ("leaderboard", HandlerId::Leaderboard),
        ("help", HandlerId::Help),
        ("privacy", HandlerId::Privacy),
        ("reopen", HandlerId::Reopen),
        ("ban", HandlerId::Moderation(ModerationAction::Ban)),
        ("tempban", HandlerId::Moderation(ModerationAction::TempBan)),
        ("kick", HandlerId::Moderation(ModerationAction::Kick)),
        ("timeout", HandlerId::Moderation(ModerationAction::Timeout)),
        ("warn", HandlerId::Moderation(ModerationAction::Warn)),
        ("purge", HandlerId::Moderation(ModerationAction::Purge)),
        (
            "slowmode",
            HandlerId::Moderation(ModerationAction::Slowmode),
        ),
        (
            "lockdown",
            HandlerId::Moderation(ModerationAction::Lockdown),
        ),
        ("unlock", HandlerId::Moderation(ModerationAction::Unlock)),
        ("purge-preview", HandlerId::PurgePreview),
        ("channel-inspect", HandlerId::ChannelInspect),
        ("attendance", HandlerId::ScorecardAttendance),
        ("command", HandlerId::AutomationAdmin),
        ("command-remove", HandlerId::AutomationAdmin),
        ("command-list", HandlerId::AutomationAdmin),
        ("command-inspect", HandlerId::AutomationAdmin),
        ("command-disable", HandlerId::AutomationAdmin),
        ("command-enable", HandlerId::AutomationAdmin),
        ("schedule", HandlerId::AutomationAdmin),
        ("schedule-remove", HandlerId::AutomationAdmin),
        ("schedule-list", HandlerId::AutomationAdmin),
        ("sticky", HandlerId::AutomationAdmin),
        ("sticky-remove", HandlerId::AutomationAdmin),
        ("giveaway", HandlerId::Giveaway),
        ("rsvp", HandlerId::Rsvp),
        ("rsvp-attendance", HandlerId::RsvpAttendance),
        ("lfg", HandlerId::Lfg),
        ("lfg-close", HandlerId::LfgClose),
        ("feed-add", HandlerId::FeedAdd),
        ("feed-remove", HandlerId::FeedRemove),
        ("feed-list", HandlerId::FeedList),
    ];
    assert_eq!(
        cases.len(),
        36,
        "all builtins except the separate lfg-list case covered"
    );
    for (name, handler) in cases {
        assert_eq!(
            router.route_slash(&ctx(name)),
            SlashOutcome::Handled { handler: *handler },
            "{name} routes to its handler",
        );
    }
    // Next-only `/lfg-list`: name-based so the baseline still compiles before
    // the production `HandlerId::LfgList` arm lands. Resolves via the publish
    // registry when available instead of naming the enum.
    let defs = router.publish_set(&[]).expect("parity set assembles");
    let lfg_list = defs
        .iter()
        .find(|d| d.name == "lfg-list")
        .expect("lfg-list is published right after lfg-close");
    assert_eq!(
        lfg_list.description.as_str(),
        "List open game posts in this channel"
    );
    assert!(
        matches!(
            router.route_slash(&ctx("lfg-list")),
            SlashOutcome::Handled { .. }
        ),
        "lfg-list routes to its handler"
    );
}

/// The Next-only `/giveaway` timed entry routes to its registered handler.
/// No direct `HandlerId::Giveaway` reference: the variant does not exist at
/// baseline, so the pin matches the handler's `Debug` name. This compiles on
/// the pre-slice router and fails behaviorally (`Unknown`) until the
/// giveaway slice registers its handler.
#[test]
fn giveaway_routes_to_its_handler_by_debug_name() {
    let router = InteractionRouter::new(parity_gates());
    match router.route_slash(&ctx("giveaway")) {
        SlashOutcome::Handled { handler } => assert_eq!(
            format!("{handler:?}"),
            "Giveaway",
            "/giveaway routes to its handler"
        ),
        other => panic!("/giveaway must route to its handler, got {other:?}"),
    }
}

/// CONTROL (green before and after): the staging E2E matrix still equals the
/// voice-off publish set. The builder's unpublish must not disturb it.
#[test]
fn e2e_matrix_still_matches_the_publish_set() {
    let published = InteractionRouter::new(parity_gates())
        .publish_set(&[])
        .expect("parity set assembles");
    let mut published_names: Vec<&str> = published.iter().map(|d| d.name.as_str()).collect();
    published_names.sort_unstable();
    let matrix = e2e_command_matrix();
    let mut matrix_names: Vec<&str> = matrix.iter().map(|r| r.command).collect();
    matrix_names.sort_unstable();
    assert_eq!(
        matrix_names, published_names,
        "matrix drift: the fix must not change the parity publish set"
    );
    assert_eq!(matrix.len(), 37, "contract covers the 37 built-in commands");
}
