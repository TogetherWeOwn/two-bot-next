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

use frozen_baseline::{gate_env_flag, FROZEN};
use two_bot_core::command_descriptor::DescriptorRegistry;
use two_bot_core::command_permissions::{CommandSurface, COMMAND_PERMISSIONS};
use two_bot_core::e2e_matrix::e2e_command_matrix;
use two_bot_core::{help_text, InteractionRouter, RouterGates, SlashContext};

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
    for (row, f) in rows.iter().zip(FROZEN.iter()) {
        assert_eq!(row.command, f.name);
        assert_eq!(row.parity_row, f.parity_row, "/{}", f.name);
        assert_eq!(row.gate.env_flag(), gate_env_flag(f.gate), "/{}", f.name);
        assert_eq!(row.required_permissions, f.mask, "/{}", f.name);
        assert_eq!(row.handler, f.handler, "/{}", f.name);
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
fn help_is_rendered_from_the_registry_with_the_legacy_text() {
    let reg = DescriptorRegistry::builtin();
    for gates in gate_sets() {
        let defs = InteractionRouter::new(gates).publish_set(&[]).unwrap();
        assert_eq!(reg.help_text(&defs), help_text(&defs), "{gates:?}");
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
