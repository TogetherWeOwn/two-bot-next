//! FUTURE-API contract: central consumers derive from the descriptor registry.
//!
//! NOT compiled today (see `README.md`). Phase 2 of the descriptor migration:
//! it needs the foundation interface AND the shipping commands moved into their
//! own descriptors. Enable it only for the PR that completes that move; before
//! then the foundation file alone is the gate. The builder must never edit this
//! file.
//!
//! The oracle is `frozen_baseline.rs`, hand-written from the legacy behavior.
//! Nothing here derives an expectation from the registry under test.

#[path = "frozen_baseline.rs"]
mod frozen_baseline;
#[path = "frozen_e2e.rs"]
mod frozen_e2e;

use frozen_baseline::{gate_env_flag, FROZEN};
use frozen_e2e::FROZEN_E2E;
use two_bot_core::command_descriptor::{CommandDescriptor, DescriptorRegistry, HelpPlacement};
use two_bot_core::command_permissions::{CommandSurface, COMMAND_PERMISSIONS};
use two_bot_core::commands::PERM_MANAGE_GUILD;
use two_bot_core::e2e_matrix::{e2e_command_matrix, E2eGate};
use two_bot_core::{
    help_text, help_text_with_registry, CommandDefinition, HandlerId, InteractionRouter,
    RouterGates, RouterRefusal, SlashContext,
};

const GUILD: u64 = 2222;

#[derive(Debug, Clone, Copy)]
struct On {
    scorecard: bool,
    automations: bool,
    announcements: bool,
    moderation: bool,
    voice: bool,
}

fn gate_sets() -> Vec<RouterGates> {
    (0u8..32)
        .map(|bits| {
            let on = On {
                scorecard: bits & 1 != 0,
                automations: bits & 2 != 0,
                announcements: bits & 4 != 0,
                moderation: bits & 8 != 0,
                voice: bits & 16 != 0,
            };
            RouterGates {
                configured_guild: Some(GUILD),
                scorecard: on.scorecard,
                automations: on.automations,
                announcements: on.announcements,
                moderation: on.moderation,
                voice: on.voice,
                voice_assistant: false,
                tickets: true,
                self_roles: true,
                onboarding_picker: true,
                session_picker: true,
            }
        })
        .collect()
}

fn route(router: &InteractionRouter, name: &str, guild: Option<u64>, bits: Option<u64>) -> String {
    format!(
        "{:?}",
        router.route_slash(&SlashContext {
            name,
            guild_id: guild,
            actor_permissions: bits,
            custom_row: None,
        })
    )
}

// ---------------------------------------------------------------------------
// The shipping registry reproduces the frozen contract
// ---------------------------------------------------------------------------

#[test]
fn the_shipping_registry_projects_exactly_the_frozen_rows() {
    let reg = DescriptorRegistry::builtin();
    let frozen: std::collections::BTreeSet<_> = FROZEN.iter().map(|f| f.name).collect();

    // Names and canonical order: the frozen publish order, for the frozen rows.
    let ours: Vec<_> = reg
        .names()
        .into_iter()
        .filter(|n| frozen.contains(n))
        .collect();
    assert_eq!(ours, FROZEN.iter().map(|f| f.name).collect::<Vec<_>>());

    // Permission / parity rows.
    for f in &FROZEN {
        let row = reg
            .permission(f.name)
            .unwrap_or_else(|| panic!("/{} lost its permission row", f.name));
        assert_eq!(row.parity_row, f.parity_row, "/{}", f.name);
        assert_eq!(row.surface, CommandSurface::BuiltinSlash, "/{}", f.name);
        assert_eq!(row.required_permissions, f.mask, "/{}", f.name);
        assert_eq!(
            row.policy_hook.map(|h| format!("{h:?}")).as_deref(),
            f.policy_hook,
            "/{}",
            f.name
        );
    }

    // E2E matrix: one row per frozen command, same order and metadata, and the
    // registry projection equals the public matrix function.
    let rows: Vec<_> = reg
        .e2e_matrix()
        .into_iter()
        .filter(|r| frozen.contains(r.command))
        .collect();
    assert_eq!(rows.len(), FROZEN.len());
    assert_eq!(FROZEN_E2E.len(), FROZEN.len(), "complete metadata oracle");
    for ((row, f), e2e) in rows.iter().zip(FROZEN.iter()).zip(FROZEN_E2E) {
        assert_eq!(e2e.name, f.name, "metadata oracle order");
        assert_eq!(row.command, f.name);
        assert_eq!(row.parity_row, f.parity_row, "/{}", f.name);
        assert_eq!(row.gate.env_flag(), gate_env_flag(f.gate), "/{}", f.name);
        assert_eq!(row.required_permissions, f.mask, "/{}", f.name);
        assert_eq!(row.handler, f.handler, "/{}", f.name);
        assert_eq!(
            row.success_shape, e2e.success_shape,
            "/{} success shape",
            f.name
        );
        assert_eq!(
            row.permission_name, e2e.permission_name,
            "/{} permission label",
            f.name
        );
        assert_eq!(
            row.denials, e2e.denials,
            "/{} complete ordered denials",
            f.name
        );
    }
    assert_eq!(reg.e2e_matrix(), e2e_command_matrix());
}

#[test]
fn the_shipping_registry_keeps_non_command_parity_rows_and_shared_identity() {
    let reg = DescriptorRegistry::builtin();
    // LFG and LFG-edit still share parity row 26.
    let row26: Vec<_> = reg
        .permissions()
        .into_iter()
        .filter(|r| r.parity_row == 26)
        .map(|r| r.command)
        .collect();
    assert_eq!(row26, ["lfg", "lfg-edit"]);
    // Dropped / dynamic / prefix rows are not slash built-ins and must still
    // appear in the permission table exactly as before.
    for (row, name, _surface, mask, _hook) in frozen_baseline::NON_SLASH_PARITY {
        let found = COMMAND_PERMISSIONS
            .iter()
            .find(|r| r.command == name)
            .unwrap_or_else(|| panic!("row {row} ({name}) missing from the public table"));
        assert_eq!(found.required_permissions, mask);
        assert!(
            reg.permission(name).is_none(),
            "{name} is not a built-in lookup"
        );
    }
}

// ---------------------------------------------------------------------------
// Consumers are derived, not parallel
// ---------------------------------------------------------------------------

#[test]
fn router_construction_paths_agree_in_every_gate_set() {
    let reg = DescriptorRegistry::builtin();
    for gates in gate_sets() {
        let direct = InteractionRouter::new(gates);
        let derived = InteractionRouter::with_registry(gates, &reg);
        assert_eq!(
            serde_json::to_value(direct.publish_set(&[]).unwrap()).unwrap(),
            serde_json::to_value(derived.publish_set(&[]).unwrap()).unwrap(),
            "{gates:?}"
        );
        for f in &FROZEN {
            for bits in [None, Some(0), Some(f.mask), Some(u64::MAX)] {
                for guild in [None, Some(GUILD), Some(GUILD + 1)] {
                    assert_eq!(
                        route(&direct, f.name, guild, bits),
                        route(&derived, f.name, guild, bits),
                        "/{} {gates:?} guild={guild:?} bits={bits:?}",
                        f.name
                    );
                }
            }
        }
    }
}

#[test]
fn router_construction_uses_the_supplied_registry_in_every_gate_set() {
    let reg = DescriptorRegistry::builtin().without("rank");
    for gates in gate_sets() {
        let direct = InteractionRouter::new(gates);
        let derived = InteractionRouter::with_registry(gates, &reg);
        assert!(
            direct
                .publish_set(&[])
                .unwrap()
                .iter()
                .any(|d| d.name == "rank"),
            "positive control: the default router publishes /rank with {gates:?}"
        );
        assert!(!derived
            .publish_set(&[])
            .unwrap()
            .iter()
            .any(|d| d.name == "rank"));
        assert_eq!(
            route(&direct, "rank", Some(GUILD), Some(u64::MAX)),
            "Handled { handler: Rank }",
            "positive control: the default router routes /rank with {gates:?}"
        );
        assert_eq!(
            route(&derived, "rank", Some(GUILD), Some(u64::MAX)),
            "Unknown",
            "the supplied registry removed /rank with {gates:?}"
        );
    }
}

#[test]
fn help_is_rendered_from_the_registry_with_the_legacy_text() {
    let reg = DescriptorRegistry::builtin();
    for gates in gate_sets() {
        let defs = InteractionRouter::new(gates).publish_set(&[]).unwrap();
        // Registry-only help deliberately ignores non-descriptor names. The
        // public consumer additionally preserves voice groups and Custom rows.
        if !gates.voice {
            assert_eq!(reg.help_text(&defs), help_text(&defs), "{gates:?}");
        }
        assert_eq!(
            help_text_with_registry(&defs, &reg),
            help_text(&defs),
            "the default consumer uses the built-in registry with {gates:?}"
        );
    }
}

#[test]
fn the_help_consumer_uses_supplied_metadata_instead_of_a_custom_name_fallback() {
    let descriptor = CommandDescriptor::builder("probe-alpha")
        .definition(
            CommandDefinition::new("probe-alpha", "Synthetic probe command")
                .permissions(PERM_MANAGE_GUILD),
        )
        .handler(HandlerId::Descriptor("probe-alpha"))
        .gate(E2eGate::Always)
        .requires(PERM_MANAGE_GUILD, RouterRefusal::ManageServerRequired)
        .help(HelpPlacement {
            group: "Synthetic probes",
            group_order: 900,
            needs: Some("Manage Server"),
            position: 1,
        })
        .help_note("Synthetic probe guidance.")
        .e2e_case("Ephemeral probe reply.", &["Perm:ManageServerRequired"])
        .parity_row(90)
        .publish_order(9_001)
        .build()
        .unwrap();
    let reg = DescriptorRegistry::builtin().with(descriptor).unwrap();
    for gates in gate_sets() {
        let mut defs = InteractionRouter::with_registry(gates, &reg)
            .publish_set(&[])
            .unwrap();
        defs.push(CommandDefinition::new(
            "probe-custom",
            "Synthetic custom command",
        ));
        let help = help_text_with_registry(&defs, &reg);
        assert!(
            help.contains("**Synthetic probes** — needs Manage Server\n/probe-alpha\nSynthetic probe guidance.\n"),
            "the consumer lost descriptor-owned help metadata with {gates:?}:\n{help}"
        );
        assert_eq!(help.matches("\n**Custom**\n").count(), 1);
        assert_eq!(
            help.split_once("\n**Custom**\n").unwrap().1.lines().next(),
            Some("/probe-custom"),
            "registered probes must not be relegated to Custom"
        );
        assert!(help.starts_with(&format!("**Server commands** ({} live)\n", defs.len())));
        assert!(help.ends_with("\nThis is the last confirmed command list. Your picker hides gated commands you cannot use."));
        assert!(!reg.help_text(&defs).contains("probe-custom"));
        assert_ne!(
            help,
            help_text(&defs),
            "positive control: the default consumer has no probe descriptor"
        );
        let removed = help_text_with_registry(&defs, &reg.without("probe-alpha"));
        assert!(!removed.contains("Synthetic probes"));
        assert!(!removed.contains("Synthetic probe guidance."));
        assert!(removed.contains("\n**Custom**\n/probe-alpha · /probe-custom\n"));
    }
}

#[test]
fn every_routed_handler_is_registered_or_a_blessed_gap() {
    let reg = DescriptorRegistry::builtin();
    for member_present in [true, false] {
        let gates = gate_sets()
            .into_iter()
            .find(|g| g.moderation && g.automations && g.announcements && g.scorecard && !g.voice)
            .expect("the all-on, voice-off gate set exists");
        let router = InteractionRouter::production_router(gates, member_present);
        let blessed: Vec<String> =
            InteractionRouter::intentional_registration_exceptions(member_present)
                .into_iter()
                .map(|id| format!("{id:?}"))
                .collect();
        let missing = reg.unregistered_handlers(&router);
        let expected: std::collections::BTreeSet<_> = FROZEN
            .iter()
            .filter(|f| blessed.iter().any(|b| *b == f.handler))
            .map(|f| f.name)
            .collect();
        let actual: std::collections::BTreeSet<_> = missing.iter().copied().collect();
        assert_eq!(
            actual, expected,
            "the complete registration gap set for member_present={member_present}"
        );
        assert_eq!(missing.len(), expected.len(), "each gap appears once");
        // `unregistered_handlers` names commands; every one must be a command
        // whose owner is a blessed gap, and nothing else may be missing.
        for name in missing {
            let f = FROZEN
                .iter()
                .find(|f| f.name == name)
                .unwrap_or_else(|| panic!("/{name}: unregistered handler outside the frozen set"));
            assert!(
                blessed.iter().any(|b| *b == f.handler),
                "/{name} ({}) is routed but has no registered handler",
                f.handler
            );
        }
    }
}

// ---------------------------------------------------------------------------
// End state: no parallel per-command lists in the central consumers
// ---------------------------------------------------------------------------

/// The production part of a source file: everything before its test module,
/// with line comments removed so prose cannot hide or fake a match.
fn production_source(source: &str) -> String {
    let cut = source.find("#[cfg(test)]").unwrap_or(source.len());
    source[..cut]
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn central_consumers_name_no_individual_migrated_command() {
    let consumers = [
        ("router.rs", include_str!("../../../src/router.rs")),
        ("help.rs", include_str!("../../../src/help.rs")),
        ("e2e_matrix.rs", include_str!("../../../src/e2e_matrix.rs")),
        (
            "command_permissions.rs",
            include_str!("../../../src/command_permissions.rs"),
        ),
    ];
    let mut offenders = Vec::new();
    for (file, source) in consumers {
        let production = production_source(source);
        for f in &FROZEN {
            if production.contains(&format!("\"{}\"", f.name)) {
                offenders.push(format!("{file}: \"{}\"", f.name));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "central consumers still keep a per-command list (derive from the registry instead):\n{}",
        offenders.join("\n")
    );
    // The gate and handler enums are shared vocabulary, not a command list; the
    // E2E gate enum keeps exactly the frozen families.
    let gates: std::collections::BTreeSet<_> =
        FROZEN.iter().map(|f| format!("{:?}", f.gate)).collect();
    assert_eq!(
        gates,
        [
            "Always",
            "Announcements",
            "Automations",
            "Moderation",
            "Scorecard"
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
}
