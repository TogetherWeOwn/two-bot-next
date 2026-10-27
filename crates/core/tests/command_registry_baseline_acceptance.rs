//! Baseline characterization of the built-in slash-command contract.
//!
//! This is the BEFORE picture for the command-owned descriptor migration. It
//! observes only public behavior of today's registry, router, `/help`, E2E
//! matrix and permission table, against the independent literal oracle in
//! `support/command_descriptors/frozen_baseline.rs`. It passes on `main` and
//! must keep passing, unedited, after every command has moved into its own
//! descriptor: the migration may change where the metadata lives, never what
//! members, the live E2E suite or the parity ledger observe.
//!
//! Pure checks: no database, no network, no Discord. The same source is meant
//! to run unchanged with and without `--features db`; nothing here may depend
//! on that feature. Every comparison covers only the 43 frozen commands, so an
//! unrelated command added by another change does not break this file.

#[path = "support/command_descriptors/frozen_baseline.rs"]
mod frozen_baseline;

use frozen_baseline::{
    gate_env_flag, gate_refusal, Frozen, Gate, ADMINISTRATOR, FROZEN, HELP_LAYOUT, NON_SLASH_PARITY,
};
use std::collections::BTreeSet;
use two_bot_core::command_permissions::{command_permission, CommandSurface, COMMAND_PERMISSIONS};
use two_bot_core::e2e_matrix::e2e_command_matrix;
use two_bot_core::moderation::moderation_commands;
use two_bot_core::{
    help_text, voice_commands, CommandDefinition, CustomCommand, InteractionRouter, RouterGates,
    SlashContext, SlashOutcome,
};

const GUILD: u64 = 2222;
const OTHER_GUILD: u64 = 3333;

/// The five gate flags that decide which commands exist. Everything else in
/// `RouterGates` is component-surface wiring and stays fixed.
#[derive(Debug, Clone, Copy)]
struct On {
    scorecard: bool,
    automations: bool,
    announcements: bool,
    moderation: bool,
    voice: bool,
}

impl On {
    fn gate(self, gate: Gate) -> bool {
        match gate {
            Gate::Always => true,
            Gate::Scorecard => self.scorecard,
            Gate::Automations => self.automations,
            Gate::Announcements => self.announcements,
            Gate::Moderation => self.moderation,
        }
    }

    fn gates(self) -> RouterGates {
        RouterGates {
            configured_guild: Some(GUILD),
            scorecard: self.scorecard,
            automations: self.automations,
            announcements: self.announcements,
            moderation: self.moderation,
            voice: self.voice,
            voice_assistant: false,
            tickets: true,
            self_roles: true,
            onboarding_picker: true,
            session_picker: true,
        }
    }

    fn router(self) -> InteractionRouter {
        InteractionRouter::new(self.gates())
    }
}

/// All 32 combinations of the five gate flags, in a fixed order.
fn every_gate_set() -> Vec<On> {
    (0u8..32)
        .map(|bits| On {
            scorecard: bits & 1 != 0,
            automations: bits & 2 != 0,
            announcements: bits & 4 != 0,
            moderation: bits & 8 != 0,
            voice: bits & 16 != 0,
        })
        .collect()
}

fn all_on() -> On {
    On {
        scorecard: true,
        automations: true,
        announcements: true,
        moderation: true,
        voice: false,
    }
}

fn route(
    router: &InteractionRouter,
    name: &str,
    guild: Option<u64>,
    permissions: Option<u64>,
    custom_row: Option<bool>,
) -> String {
    format!(
        "{:?}",
        router.route_slash(&SlashContext {
            name,
            guild_id: guild,
            actor_permissions: permissions,
            custom_row,
        })
    )
}

fn handled(f: &Frozen) -> String {
    format!("Handled {{ handler: {} }}", f.handler)
}

fn refuse(refusal: &str) -> String {
    format!("Refuse {{ refusal: {refusal} }}")
}

fn published(on: On) -> Vec<CommandDefinition> {
    on.router().publish_set(&[]).expect("publish set assembles")
}

fn frozen_names() -> BTreeSet<&'static str> {
    FROZEN.iter().map(|f| f.name).collect()
}

/// Frozen names that must be published for a gate set, in publish order.
/// Voice also publishes its own `kick`; with moderation off it is the only
/// moderation-named command published, and it sorts after every other
/// built-in because the voice family merges last.
fn expected_published_frozen(on: On) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = FROZEN
        .iter()
        .filter(|f| on.gate(f.gate))
        .map(|f| f.name)
        .collect();
    if !on.moderation && on.voice {
        names.push("kick");
    }
    names
}

fn published_frozen(defs: &[CommandDefinition]) -> Vec<String> {
    let frozen = frozen_names();
    defs.iter()
        .map(|d| d.name.clone())
        .filter(|n| frozen.contains(n.as_str()))
        .collect()
}

// ---------------------------------------------------------------------------
// Routing: fence -> gate -> permission, with the exact refusal for each.
// ---------------------------------------------------------------------------

#[test]
fn routing_order_and_denials_match_the_frozen_contract_in_every_gate_set() {
    for on in every_gate_set() {
        let router = on.router();
        for f in &FROZEN {
            let ctx = format!("{} with {on:?}", f.name);
            let moderation_family = f.gate == Gate::Moderation;

            // Guild fence first. Moderation answers a refusal, every other
            // built-in stays silent; neither depends on the gate or the bits.
            for (guild, permissions) in [
                (None, None),
                (None, Some(u64::MAX)),
                (Some(OTHER_GUILD), Some(u64::MAX)),
            ] {
                let expected = if moderation_family {
                    refuse("GuildRestricted")
                } else {
                    "Ignore".to_owned()
                };
                assert_eq!(
                    route(&router, f.name, guild, permissions, None),
                    expected,
                    "fence: {ctx} guild={guild:?}"
                );
            }

            // Gate second: a disabled family still routes the name and refuses
            // it, whatever the invoker's bits. It never falls through.
            if !on.gate(f.gate) {
                let expected = refuse(gate_refusal(f.gate).expect("gated family refusal"));
                for permissions in [None, Some(0), Some(u64::MAX)] {
                    assert_eq!(
                        route(&router, f.name, Some(GUILD), permissions, None),
                        expected,
                        "gate: {ctx} permissions={permissions:?}"
                    );
                }
                continue;
            }

            // Permission third.
            let allowed = handled(f);
            assert_eq!(
                route(&router, f.name, Some(GUILD), Some(u64::MAX), None),
                allowed,
                "all bits: {ctx}"
            );
            assert_eq!(
                route(&router, f.name, Some(GUILD), Some(f.mask), None),
                allowed,
                "exact bits: {ctx}"
            );
            if f.mask == 0 {
                for permissions in [None, Some(0)] {
                    assert_eq!(
                        route(&router, f.name, Some(GUILD), permissions, None),
                        allowed,
                        "open command: {ctx} permissions={permissions:?}"
                    );
                }
            } else {
                let denied = refuse(f.denial.expect("restricted command has a refusal"));
                // Every other bit except Administrator still denies;
                // Administrator alone is checked below.
                for permissions in [None, Some(0), Some(!f.mask & !ADMINISTRATOR)] {
                    assert_eq!(
                        route(&router, f.name, Some(GUILD), permissions, None),
                        denied,
                        "denial: {ctx} permissions={permissions:?}"
                    );
                }
                // Administrator alone is not a substitute for the listed bit,
                // except for the Next-only /game-mix staff read.
                let admin_only = route(&router, f.name, Some(GUILD), Some(ADMINISTRATOR), None);
                if f.name == "game-mix" {
                    assert_eq!(admin_only, allowed, "game-mix admits Administrator alone");
                } else {
                    assert_eq!(admin_only, denied, "Administrator alone is denied: {ctx}");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Publication: order, gating, wire shape.
// ---------------------------------------------------------------------------

#[test]
fn publish_order_and_gating_are_frozen_in_every_gate_set() {
    for on in every_gate_set() {
        let defs = published(on);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        let unique: BTreeSet<&str> = names.iter().copied().collect();
        assert_eq!(unique.len(), names.len(), "a name published twice: {on:?}");
        assert_eq!(
            published_frozen(&defs),
            expected_published_frozen(on),
            "frozen subsequence: {on:?}"
        );
    }
}

#[test]
fn published_wire_shape_is_identical_whenever_a_command_is_published() {
    let reference = published(all_on());
    for on in every_gate_set() {
        let defs = published(on);
        for f in &FROZEN {
            let Some(def) = defs.iter().find(|d| d.name == f.name) else {
                continue;
            };
            if f.name == "kick" && !on.moderation {
                // Only the voice-owned kick is published here; the contract for
                // that shadowing is pinned in its own test below.
                continue;
            }
            let expected = reference
                .iter()
                .find(|d| d.name == f.name)
                .expect("all-on set publishes every frozen command");
            assert_eq!(
                serde_json::to_value(def).unwrap(),
                serde_json::to_value(expected).unwrap(),
                "wire shape of /{} drifted with {on:?}",
                f.name
            );
        }
    }
}

#[test]
fn published_permission_string_is_the_frozen_mask_and_commands_stay_guild_only() {
    for def in published(all_on()) {
        let Some(f) = FROZEN.iter().find(|f| f.name == def.name) else {
            continue;
        };
        let expected = (f.mask != 0).then(|| f.mask.to_string());
        assert_eq!(
            def.default_member_permissions, expected,
            "/{} published permission string",
            f.name
        );
        assert!(!def.dm_permission, "/{} must stay guild-only", f.name);
    }
}

#[test]
fn moderation_kick_shadows_voice_kick_and_voice_alone_still_publishes_one() {
    let moderation_kick = moderation_commands()
        .into_iter()
        .find(|d| d.name == "kick")
        .expect("moderation defines /kick");
    for voice in [false, true] {
        let defs = published(On { voice, ..all_on() });
        let kicks: Vec<_> = defs.iter().filter(|d| d.name == "kick").collect();
        assert_eq!(
            kicks.len(),
            1,
            "voice={voice}: /kick publishes exactly once"
        );
        assert_eq!(
            serde_json::to_value(kicks[0]).unwrap(),
            serde_json::to_value(&moderation_kick).unwrap(),
            "voice={voice}: the moderation shape wins first-wins"
        );
    }
    // Voice on, moderation off: the voice set still publishes its own kick,
    // yet dispatch keeps answering the name as a disabled moderation command.
    let on = On {
        moderation: false,
        voice: true,
        ..all_on()
    };
    assert!(published(on).iter().any(|d| d.name == "kick"));
    let router = on.router();
    assert_eq!(
        route(&router, "kick", Some(GUILD), Some(u64::MAX), None),
        refuse("ModerationDisabled")
    );
    assert!(!router.voice_owns_command("kick"));
}

// ---------------------------------------------------------------------------
// Help, E2E matrix and parity metadata.
// ---------------------------------------------------------------------------

type HelpGroups = Vec<(String, Option<String>, Vec<String>)>;

/// Parse the group headers and name lines out of the rendered `/help` text.
fn parse_help(text: &str) -> HelpGroups {
    let mut groups = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("**") else {
            continue;
        };
        let Some((title, tail)) = rest.split_once("**") else {
            continue;
        };
        if title == "Server commands" {
            continue;
        }
        let needs = tail.strip_prefix(" — needs ").map(str::to_owned);
        let names: Vec<String> = lines
            .peek()
            .filter(|next| next.starts_with('/'))
            .map(|next| {
                next.split(" · ")
                    .map(|n| n.trim_start_matches('/').to_owned())
                    .collect()
            })
            .unwrap_or_default();
        groups.push((title.to_owned(), needs, names));
    }
    groups
}

fn frozen_help_projection(groups: HelpGroups) -> HelpGroups {
    let frozen = frozen_names();
    groups
        .into_iter()
        .filter_map(|(title, needs, names)| {
            let names: Vec<String> = names
                .into_iter()
                .filter(|n| frozen.contains(n.as_str()))
                .collect();
            (!names.is_empty()).then_some((title, needs, names))
        })
        .collect()
}

fn expected_help(live: &BTreeSet<&str>) -> HelpGroups {
    HELP_LAYOUT
        .iter()
        .filter_map(|(title, needs, names)| {
            let names: Vec<String> = names
                .iter()
                .filter(|n| live.contains(**n))
                .map(|n| (*n).to_owned())
                .collect();
            (!names.is_empty()).then(|| ((*title).to_owned(), needs.map(str::to_owned), names))
        })
        .collect()
}

#[test]
fn help_groups_titles_and_order_are_frozen_in_every_gate_set() {
    for on in every_gate_set() {
        let defs = published(on);
        let live: BTreeSet<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            frozen_help_projection(parse_help(&help_text(&defs))),
            expected_help(&live),
            "help layout with {on:?}"
        );
    }
}

#[test]
fn help_order_differs_from_publish_order_where_users_see_it() {
    // game-mix publishes in the always-on core block but renders last;
    // welcome-preview renders before the check-in group. Sorting help by
    // publish position, filename or name would be a user-visible change.
    let defs = published(all_on());
    let help = frozen_help_projection(parse_help(&help_text(&defs)));
    let flat: Vec<&str> = help
        .iter()
        .flat_map(|(_, _, names)| names.iter().map(String::as_str))
        .collect();
    assert_eq!(flat.last().copied(), Some("game-mix"));
    let publish: Vec<&str> = defs
        .iter()
        .map(|d| d.name.as_str())
        .filter(|n| frozen_names().contains(n))
        .collect();
    assert_ne!(flat, publish);
}

#[test]
fn e2e_matrix_rows_match_the_frozen_contract() {
    let frozen = frozen_names();
    let rows: Vec<_> = e2e_command_matrix()
        .into_iter()
        .filter(|row| frozen.contains(row.command))
        .collect();
    assert_eq!(
        rows.len(),
        FROZEN.len(),
        "one matrix row per frozen command"
    );
    for (row, f) in rows.iter().zip(FROZEN.iter()) {
        assert_eq!(row.command, f.name, "matrix order");
        assert_eq!(row.parity_row, f.parity_row, "/{} parity row", f.name);
        assert_eq!(
            row.gate.env_flag(),
            gate_env_flag(f.gate),
            "/{} gate",
            f.name
        );
        assert_eq!(row.required_permissions, f.mask, "/{} mask", f.name);
        assert_eq!(row.handler, f.handler, "/{} handler", f.name);
        assert!(!row.success_shape.is_empty(), "/{} success shape", f.name);
        assert_eq!(
            row.permission_name == "Everyone",
            f.mask == 0,
            "/{} permission label",
            f.name
        );
        // The denial vocabulary is the router's own refusal spelling, in the
        // same two families the live suite probes, gate case first.
        let mut expected: Vec<String> = Vec::new();
        if let Some(refusal) = gate_refusal(f.gate) {
            expected.push(format!("Gate:{refusal}"));
        }
        if let Some(refusal) = f.denial {
            expected.push(format!("Perm:{refusal}"));
        }
        let actual: Vec<&str> = row
            .denials
            .iter()
            .copied()
            .filter(|d| d.starts_with("Gate:") || d.starts_with("Perm:"))
            .collect();
        assert_eq!(
            actual, expected,
            "/{} gate and permission denial cases",
            f.name
        );
    }
}

#[test]
fn permission_table_and_parity_dispositions_match_the_frozen_contract() {
    for f in &FROZEN {
        let row = command_permission(f.name)
            .unwrap_or_else(|| panic!("/{} must resolve through the built-in lookup", f.name));
        assert_eq!(row.parity_row, f.parity_row, "/{} parity row", f.name);
        assert_eq!(row.surface, CommandSurface::BuiltinSlash, "/{}", f.name);
        assert_eq!(row.required_permissions, f.mask, "/{} mask", f.name);
        assert_eq!(
            row.policy_hook.map(|hook| format!("{hook:?}")).as_deref(),
            f.policy_hook,
            "/{} policy hook",
            f.name
        );
        assert_eq!(row.allows(None), f.mask == 0, "/{} absent bits", f.name);
        assert!(row.allows(Some(f.mask)), "/{} exact bits", f.name);
        assert_eq!(
            row.allows(Some(ADMINISTRATOR)),
            f.mask == 0 || f.name == "game-mix",
            "/{} administrator alone",
            f.name
        );
    }
    // Several commands may share one parity row; LFG and LFG-edit do.
    let row26: BTreeSet<&str> = COMMAND_PERMISSIONS
        .iter()
        .filter(|row| row.parity_row == 26)
        .map(|row| row.command)
        .collect();
    assert_eq!(row26, BTreeSet::from(["lfg", "lfg-edit"]));
    // Dropped, dynamic and prefix rows keep their disposition and never
    // reserve a built-in slash name.
    for (parity_row, name, surface, mask, hook) in NON_SLASH_PARITY {
        let row = COMMAND_PERMISSIONS
            .iter()
            .find(|row| row.command == name)
            .unwrap_or_else(|| panic!("parity row {parity_row} ({name}) disappeared"));
        assert_eq!(row.parity_row, parity_row, "{name}");
        assert_eq!(format!("{:?}", row.surface), surface, "{name}");
        assert_eq!(row.required_permissions, mask, "{name}");
        assert_eq!(format!("{:?}", row.policy_hook.unwrap()), hook, "{name}");
        assert!(
            command_permission(name).is_none(),
            "{name} is not a built-in"
        );
    }
    // The attendance split: two different commands, never one name.
    assert_eq!(command_permission("attendance").unwrap().parity_row, 12);
    assert_eq!(
        command_permission("rsvp-attendance").unwrap().parity_row,
        25
    );
}

// ---------------------------------------------------------------------------
// Registration is not publication is not execution.
// ---------------------------------------------------------------------------

#[test]
fn production_registration_covers_every_routed_handler_except_the_blessed_gap() {
    for member_present in [true, false] {
        let router = InteractionRouter::production_router(all_on().gates(), member_present);
        let blessed: BTreeSet<String> =
            InteractionRouter::intentional_registration_exceptions(member_present)
                .into_iter()
                .map(|id| format!("{id:?}"))
                .collect();
        assert_eq!(
            blessed.len(),
            if member_present { 0 } else { 5 },
            "member_present={member_present}"
        );
        for f in &FROZEN {
            let SlashOutcome::Handled { handler } = router.route_slash(&SlashContext {
                name: f.name,
                guild_id: Some(GUILD),
                actor_permissions: Some(u64::MAX),
                custom_row: None,
            }) else {
                panic!("/{} must route to a handler with every gate on", f.name);
            };
            assert_eq!(format!("{handler:?}"), f.handler, "/{} owner", f.name);
            let registered = router.handler_for(&handler).is_some();
            let gap = blessed.contains(&format!("{handler:?}"));
            assert_eq!(
                registered, !gap,
                "/{} registered={registered} gap={gap} member_present={member_present}",
                f.name
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Negative controls: resemblance is not registration.
// ---------------------------------------------------------------------------

/// Names that look like a built-in to a human or to a sloppy matcher but are
/// not one. None may ever reach a built-in handler or the permission table.
const LOOKALIKES: &[&str] = &[
    "",
    " ",
    "Rank",
    "RANK",
    "rank ",
    " rank",
    "/rank",
    "rank2",
    "ranks",
    "ran",
    "r\u{430}nk",   // Cyrillic a
    "rank\u{200b}", // zero-width space
    "ban ",
    "Ban",
    "tempban2",
    "kick-user",
    "sticky_remove",
    "stickyremove",
    "command_list",
    "Command-Inspect",
    "lfg-edit ",
    "lfg_edit",
    "lfg-edit2",
    "feed-add\n",
    "feed",
    "rsvp-attendence",
    "attendance ",
    "game_mix",
    "gamemix",
    "welcome_preview",
    "purge-previews",
    "channel_inspect",
    "<custom>",
    "<trigger>",
    "rota-acknowledge",
    "templateassistant",
];

#[test]
fn lookalike_names_never_reach_a_builtin_handler_or_the_permission_table() {
    let frozen = frozen_names();
    for on in every_gate_set() {
        let router = on.router();
        let defs = published(on);
        for name in LOOKALIKES {
            assert!(
                !frozen.contains(name),
                "control name {name:?} is a real built-in"
            );
            assert!(
                command_permission(name).is_none(),
                "{name:?} resolved a permission row"
            );
            assert!(
                !defs.iter().any(|d| d.name == *name),
                "{name:?} was published with {on:?}"
            );
            // No row, any bits: stale/unknown reply inside the configured
            // guild, silence everywhere else.
            assert_eq!(
                route(&router, name, Some(GUILD), Some(u64::MAX), None),
                "Unknown",
                "{name:?} in guild with {on:?}"
            );
            assert_eq!(
                route(&router, name, Some(OTHER_GUILD), Some(u64::MAX), None),
                "Ignore",
                "{name:?} outside the guild with {on:?}"
            );
            assert_eq!(
                route(&router, name, None, None, None),
                "Ignore",
                "{name:?} in a DM with {on:?}"
            );
            assert!(
                !router.voice_owns_command(name),
                "{name:?} claimed by voice"
            );
        }
    }
}

#[test]
fn a_custom_row_never_takes_over_a_builtin_name_in_routing_or_publication() {
    for on in every_gate_set() {
        let router = on.router();
        for f in &FROZEN {
            // Builtins are matched first, so a stored custom row (enabled or
            // not) cannot change the answer, including while the family is off.
            let baseline = route(&router, f.name, Some(GUILD), Some(u64::MAX), None);
            for row in [Some(true), Some(false)] {
                assert_eq!(
                    route(&router, f.name, Some(GUILD), Some(u64::MAX), row),
                    baseline,
                    "/{} with custom row {row:?} and {on:?}",
                    f.name
                );
            }
        }
        // Publication withholds every custom row that squats a built-in name.
        let squatters: Vec<CustomCommand> = FROZEN
            .iter()
            .map(|f| CustomCommand {
                name: f.name.to_owned(),
                description: "squatter".to_owned(),
                enabled: true,
            })
            .collect();
        let with = on
            .router()
            .publish_set(&squatters)
            .expect("squatters publish");
        assert_eq!(
            serde_json::to_value(with).unwrap(),
            serde_json::to_value(published(on)).unwrap(),
            "custom rows named like built-ins changed publication: {on:?}"
        );
    }
}

#[test]
fn a_genuine_custom_name_still_routes_through_the_custom_path_only() {
    let on = all_on();
    let router = on.router();
    assert_eq!(
        route(&router, "faq", Some(GUILD), None, Some(true)),
        "Handled { handler: AutomationCustom }"
    );
    assert_eq!(route(&router, "faq", Some(GUILD), None, None), "Unknown");
    assert_eq!(
        route(&router, "faq", Some(GUILD), None, Some(false)),
        "Unknown"
    );
    assert_eq!(
        route(&router, "faq", Some(OTHER_GUILD), None, Some(true)),
        "Ignore"
    );
    let off = On {
        automations: false,
        ..on
    };
    assert_eq!(
        route(&off.router(), "faq", Some(GUILD), None, Some(true)),
        refuse("AutomationsDisabled")
    );
    let custom = [CustomCommand {
        name: "faq".to_owned(),
        description: "d".to_owned(),
        enabled: true,
    }];
    assert!(on
        .router()
        .publish_set(&custom)
        .unwrap()
        .iter()
        .any(|d| d.name == "faq"));
    assert!(!off
        .router()
        .publish_set(&custom)
        .unwrap()
        .iter()
        .any(|d| d.name == "faq"));
}

#[test]
fn the_assistant_name_stays_unpublished_and_unknown_in_every_gate_state() {
    for on in every_gate_set() {
        for voice_assistant in [false, true] {
            let mut gates = on.gates();
            gates.voice_assistant = voice_assistant;
            let router = InteractionRouter::new(gates);
            assert!(!router
                .publish_set(&[])
                .unwrap()
                .iter()
                .any(|d| d.name == "templateassistant"));
            assert_eq!(
                route(
                    &router,
                    "templateassistant",
                    Some(GUILD),
                    Some(u64::MAX),
                    Some(true)
                ),
                "Unknown",
                "{on:?} assistant={voice_assistant}"
            );
        }
    }
}

#[test]
fn voice_names_belong_to_voice_only_while_the_voice_gate_is_on() {
    let frozen = frozen_names();
    let voice_only: Vec<String> = voice_commands()
        .into_iter()
        .map(|d| d.name)
        .filter(|n| !frozen.contains(n.as_str()))
        .collect();
    assert!(!voice_only.is_empty());
    for on in every_gate_set() {
        let router = on.router();
        for name in &voice_only {
            assert_eq!(
                router.voice_owns_command(name),
                on.voice,
                "{name} with {on:?}"
            );
            let expected = if on.voice { "Ignore" } else { "Unknown" };
            assert_eq!(
                route(&router, name, Some(GUILD), Some(u64::MAX), None),
                expected,
                "/{name} dispatch with {on:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Determinism.
// ---------------------------------------------------------------------------

/// One flat, human-diffable projection of every observable above for a gate
/// set. Equality of two renders is the determinism check; the same render is
/// what the descriptor-derived registry must reproduce for the frozen rows.
fn projection(on: On) -> String {
    use std::fmt::Write as _;
    let router = on.router();
    let defs = published(on);
    let mut out = String::new();
    for def in &defs {
        let _ = writeln!(out, "publish {}", serde_json::to_string(def).unwrap());
    }
    let _ = writeln!(out, "help {:?}", parse_help(&help_text(&defs)));
    for f in &FROZEN {
        for perms in [None, Some(0), Some(f.mask), Some(u64::MAX)] {
            let _ = writeln!(
                out,
                "route {} {perms:?} {}",
                f.name,
                route(&router, f.name, Some(GUILD), perms, None)
            );
        }
    }
    for row in e2e_command_matrix() {
        let _ = writeln!(
            out,
            "matrix {} {} {:?} {} {} {:?}",
            row.command,
            row.parity_row,
            row.gate.env_flag(),
            row.required_permissions,
            row.handler,
            row.denials
        );
    }
    out
}

#[test]
fn repeated_evaluation_is_byte_identical() {
    for on in every_gate_set() {
        let first = projection(on);
        assert_eq!(first, projection(on), "unstable projection: {on:?}");
        assert!(first.contains("publish "), "projection must not be empty");
    }
}

#[test]
fn parallel_evaluation_is_byte_identical() {
    let expected = projection(all_on());
    let handles: Vec<_> = (0..4)
        .map(|_| std::thread::spawn(|| projection(all_on())))
        .collect();
    for handle in handles {
        assert_eq!(handle.join().expect("worker"), expected);
    }
}
