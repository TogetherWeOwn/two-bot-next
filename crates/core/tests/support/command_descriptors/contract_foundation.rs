//! FUTURE-API contract: command-owned descriptors, foundation phase.
//!
//! NOT compiled today. This file targets the interface pinned in
//! `README.md` (module `two_bot_core::command_descriptor`), which does not
//! exist yet; it sits under `tests/support/` so Cargo does not discover it. The
//! builder enables it with a one-line wrapper (see the README) when the
//! foundation lands, and must never edit this file. Baseline behavior that
//! already holds lives in `tests/command_registry_baseline_acceptance.rs`.
//!
//! Everything here uses SYNTHETIC commands and pure values: no database, no
//! network, no Discord, no central file. A leaf command that needs more than
//! its own descriptor to exist, route, publish, render in `/help`, appear in the
//! E2E matrix and carry parity metadata is a contract failure.

use two_bot_core::command_descriptor::{
    CommandDescriptor, DescriptorBuilder, DescriptorError, DescriptorRegistry, HelpPlacement,
};
use two_bot_core::command_permissions::{CommandPermission, CommandSurface, PolicyHook};
use two_bot_core::commands::{
    CommandOption, CommandOptionType, PERM_MANAGE_EVENTS, PERM_MANAGE_GUILD,
};
use two_bot_core::e2e_matrix::E2eGate;
use two_bot_core::{
    CommandDefinition, HandlerId, InteractionHandler, InteractionRouter, RouterGates,
    RouterRefusal, SlashContext, SlashOutcome,
};

const GUILD: u64 = 2222;
const OTHER_GUILD: u64 = 3333;
const ADMINISTRATOR: u64 = 1 << 3;

fn gates(announcements: bool) -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD),
        scorecard: true,
        automations: true,
        announcements,
        moderation: true,
        voice: false,
        voice_assistant: false,
        tickets: true,
        self_roles: true,
        onboarding_picker: true,
        session_picker: true,
    }
}

// ---------------------------------------------------------------------------
// Synthetic descriptors
// ---------------------------------------------------------------------------

const SUCCESS: &str = "Ephemeral probe reply.";
const DENIALS: &[&str] = &["Gate:AnnouncementsDisabled", "Perm:ManageServerRequired"];
const HELP_NOTE: &str = "Synthetic probe guidance.";

/// A fully specified synthetic command: announcement-gated, Manage Server,
/// one option, its own help group, one E2E case, its own parity row.
fn probe_builder(name: &'static str, order: u32, parity_row: u8) -> DescriptorBuilder {
    let help = HelpPlacement {
        group: "Synthetic probes",
        group_order: 900,
        needs: Some("Manage Server"),
        position: order,
    };
    probe_builder_in(name, order, parity_row, help)
}

/// [`probe_builder`] with an explicit help placement (no reliance on a later
/// call overriding an earlier one).
fn probe_builder_in(
    name: &'static str,
    order: u32,
    parity_row: u8,
    help: HelpPlacement,
) -> DescriptorBuilder {
    CommandDescriptor::builder(name)
        .definition(
            CommandDefinition::new(name, "Synthetic probe command")
                .permissions(PERM_MANAGE_GUILD)
                .options(vec![CommandOption::new(
                    "note",
                    "Free text",
                    CommandOptionType::String,
                )]),
        )
        .handler(HandlerId::Descriptor(name))
        .gate(E2eGate::Announcements)
        .requires(PERM_MANAGE_GUILD, RouterRefusal::ManageServerRequired)
        .help(help)
        .help_note(HELP_NOTE)
        .e2e_case(SUCCESS, DENIALS)
        .parity_row(parity_row)
        .publish_order(order)
}

fn probe(name: &'static str, order: u32, parity_row: u8) -> CommandDescriptor {
    probe_builder(name, order, parity_row)
        .build()
        .unwrap_or_else(|e| panic!("complete probe /{name} must build: {e}"))
}

/// Same as [`probe_builder`] with exactly one required field left out.
fn probe_missing(name: &'static str, skip: &str) -> Result<CommandDescriptor, DescriptorError> {
    let mut b = CommandDescriptor::builder(name);
    if skip != "definition" {
        b = b.definition(
            CommandDefinition::new(name, "Synthetic probe command").permissions(PERM_MANAGE_GUILD),
        );
    }
    if skip != "handler" {
        b = b.handler(HandlerId::Descriptor(name));
    }
    if skip != "gate" {
        b = b.gate(E2eGate::Announcements);
    }
    if skip != "permission" {
        b = b.requires(PERM_MANAGE_GUILD, RouterRefusal::ManageServerRequired);
    }
    if skip != "help" {
        b = b.help(HelpPlacement {
            group: "Synthetic probes",
            group_order: 900,
            needs: Some("Manage Server"),
            position: 1,
        });
    }
    if skip != "e2e" {
        b = b.e2e_case(SUCCESS, DENIALS);
    }
    if skip != "parity" {
        b = b.parity_row(90);
    }
    if skip != "publish_order" {
        b = b.publish_order(1);
    }
    b.build()
}

// ---------------------------------------------------------------------------
// One flat projection of everything a registry drives
// ---------------------------------------------------------------------------

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

/// Everything observable from a registry, as text, for the given gate state
/// and probe names. Two registries with equal snapshots are indistinguishable
/// to publication, dispatch, permissions, `/help`, the E2E matrix and parity.
fn snapshot(reg: &DescriptorRegistry, announcements: bool, names: &[&str]) -> String {
    use std::fmt::Write as _;
    let router = InteractionRouter::with_registry(gates(announcements), reg);
    let defs = router.publish_set(&[]).expect("publish set assembles");
    let mut out = String::new();
    let _ = writeln!(out, "names {:?}", reg.names());
    for def in &defs {
        let _ = writeln!(out, "publish {}", serde_json::to_string(def).unwrap());
    }
    let _ = writeln!(out, "help {}", reg.help_text(&defs));
    for row in reg.e2e_matrix() {
        let _ = writeln!(out, "matrix {row:?}");
    }
    for row in reg.permissions() {
        let _ = writeln!(out, "parity {row:?}");
    }
    for name in names {
        for bits in [None, Some(0), Some(PERM_MANAGE_GUILD), Some(u64::MAX)] {
            let _ = writeln!(
                out,
                "route {name} {bits:?} {}",
                route(&router, name, Some(GUILD), bits)
            );
        }
        let _ = writeln!(out, "permission {name} {:?}", reg.permission(name));
    }
    out
}

// ---------------------------------------------------------------------------
// A descriptor alone drives every projection
// ---------------------------------------------------------------------------

#[test]
fn one_complete_descriptor_reaches_every_projection() {
    let reg = DescriptorRegistry::builtin()
        .with(probe("probe-alpha", 9_001, 90))
        .expect("complete descriptor registers");
    assert!(reg.names().contains(&"probe-alpha"));

    // Publication, with the gate on and off.
    let on = InteractionRouter::with_registry(gates(true), &reg);
    let published = on.publish_set(&[]).unwrap();
    let def = published
        .iter()
        .find(|d| d.name == "probe-alpha")
        .expect("gate on: published");
    assert_eq!(
        def.default_member_permissions,
        Some(PERM_MANAGE_GUILD.to_string())
    );
    assert_eq!(def.options.len(), 1);
    assert!(!def.dm_permission);
    let off = InteractionRouter::with_registry(gates(false), &reg);
    assert!(!off
        .publish_set(&[])
        .unwrap()
        .iter()
        .any(|d| d.name == "probe-alpha"));

    // Dispatch: fence, then gate, then permission, then the owner handler.
    let owner = "Handled { handler: Descriptor(\"probe-alpha\") }";
    assert_eq!(
        route(&on, "probe-alpha", Some(GUILD), Some(PERM_MANAGE_GUILD)),
        owner
    );
    assert_eq!(
        route(&on, "probe-alpha", Some(GUILD), Some(u64::MAX)),
        owner
    );
    assert_eq!(
        route(&on, "probe-alpha", Some(GUILD), Some(0)),
        "Refuse { refusal: ManageServerRequired }"
    );
    assert_eq!(
        route(&on, "probe-alpha", Some(GUILD), None),
        "Refuse { refusal: ManageServerRequired }"
    );
    assert_eq!(
        route(&on, "probe-alpha", Some(GUILD), Some(PERM_MANAGE_EVENTS)),
        "Refuse { refusal: ManageServerRequired }"
    );
    assert_eq!(
        route(&on, "probe-alpha", Some(OTHER_GUILD), Some(u64::MAX)),
        "Ignore"
    );
    assert_eq!(route(&on, "probe-alpha", None, None), "Ignore");
    assert_eq!(
        route(&off, "probe-alpha", Some(GUILD), Some(u64::MAX)),
        "Refuse { refusal: AnnouncementsDisabled }",
        "a gated-off probe is still routed and refused, never silently unknown"
    );

    // Permission table / parity.
    let row = reg.permission("probe-alpha").expect("parity row");
    assert_eq!(row.command, "probe-alpha");
    assert_eq!(row.parity_row, 90);
    assert_eq!(row.surface, CommandSurface::BuiltinSlash);
    assert_eq!(row.required_permissions, PERM_MANAGE_GUILD);
    assert_eq!(row.policy_hook, None);
    assert!(row.allows(Some(PERM_MANAGE_GUILD)));
    assert!(!row.allows(Some(ADMINISTRATOR)));
    assert!(!row.allows(None));

    // E2E matrix: one row for the single case, derived from the same fields.
    let rows: Vec<_> = reg
        .e2e_matrix()
        .into_iter()
        .filter(|r| r.command == "probe-alpha")
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].parity_row, 90);
    assert_eq!(rows[0].gate, E2eGate::Announcements);
    assert_eq!(rows[0].required_permissions, PERM_MANAGE_GUILD);
    assert_eq!(rows[0].permission_name, "ManageGuild");
    assert_eq!(rows[0].handler, "Descriptor(\"probe-alpha\")");
    assert_eq!(rows[0].success_shape, SUCCESS);
    assert_eq!(rows[0].denials, DENIALS);

    // /help: its own group, in the declared group slot, with the needs label.
    let help = reg.help_text(&published);
    assert!(
        help.contains("**Synthetic probes** — needs Manage Server\n/probe-alpha\n"),
        "help text:\n{help}"
    );
    assert!(
        help.contains("/probe-alpha\nSynthetic probe guidance.\n"),
        "the descriptor owns guidance as well as its group: {help}"
    );
    assert!(!reg
        .help_text(&off.publish_set(&[]).unwrap())
        .contains(HELP_NOTE));
    let baseline_help = DescriptorRegistry::builtin().help_text(&published);
    assert!(
        !baseline_help.contains("probe-alpha"),
        "builtin help must not know the probe"
    );
    assert!(!baseline_help.contains(HELP_NOTE));
}

// ---------------------------------------------------------------------------
// Add / remove locality
// ---------------------------------------------------------------------------

#[test]
fn adding_then_removing_a_descriptor_restores_every_projection_byte_for_byte() {
    let before = DescriptorRegistry::builtin();
    let added = before.with(probe("probe-alpha", 9_001, 90)).unwrap();
    let after = added.without("probe-alpha");
    for announcements in [true, false] {
        assert_eq!(
            snapshot(&before, announcements, &["probe-alpha", "rank"]),
            snapshot(&after, announcements, &["probe-alpha", "rank"]),
            "announcements={announcements}: removal left a stale entry behind"
        );
        assert_ne!(
            snapshot(&before, announcements, &["probe-alpha"]),
            snapshot(&added, announcements, &["probe-alpha"]),
            "announcements={announcements}: the addition changed nothing"
        );
    }
    assert!(!after.names().contains(&"probe-alpha"));
    assert!(after.permission("probe-alpha").is_none());
    assert!(!after
        .e2e_matrix()
        .iter()
        .any(|r| r.command == "probe-alpha"));
    assert!(!after
        .permissions()
        .iter()
        .any(|r| r.command == "probe-alpha"));
}

#[test]
fn an_addition_changes_only_its_own_entries() {
    let before = DescriptorRegistry::builtin();
    let after = before.with(probe("probe-alpha", 9_001, 90)).unwrap();
    // Every pre-existing name, in the same relative order.
    let kept: Vec<_> = after
        .names()
        .into_iter()
        .filter(|n| *n != "probe-alpha")
        .collect();
    assert_eq!(kept, before.names());
    // Exactly one more matrix row, one more permission row, one more definition.
    assert_eq!(after.e2e_matrix().len(), before.e2e_matrix().len() + 1);
    assert_eq!(after.permissions().len(), before.permissions().len() + 1);
    let published = |reg: &DescriptorRegistry| {
        InteractionRouter::with_registry(gates(true), reg)
            .publish_set(&[])
            .unwrap()
    };
    assert_eq!(published(&after).len(), published(&before).len() + 1);
    // Existing commands route exactly as before.
    let (rb, ra) = (
        InteractionRouter::with_registry(gates(true), &before),
        InteractionRouter::with_registry(gates(true), &after),
    );
    for name in before.names() {
        for bits in [None, Some(0), Some(u64::MAX)] {
            assert_eq!(
                route(&rb, name, Some(GUILD), bits),
                route(&ra, name, Some(GUILD), bits),
                "/{name} changed when an unrelated command was added"
            );
        }
    }
}

#[test]
fn registry_values_are_immutable_snapshots() {
    let before = DescriptorRegistry::builtin();
    let snap = snapshot(&before, true, &["probe-alpha"]);
    let _added = before.with(probe("probe-alpha", 9_001, 90)).unwrap();
    let _removed = before.without("rank");
    assert_eq!(snapshot(&before, true, &["probe-alpha"]), snap);
}

// ---------------------------------------------------------------------------
// Fail closed
// ---------------------------------------------------------------------------

#[test]
fn a_duplicate_name_is_refused_and_leaves_the_registry_usable() {
    let base = DescriptorRegistry::builtin();
    let one = base.with(probe("probe-alpha", 9_001, 90)).unwrap();
    let snap = snapshot(&one, true, &["probe-alpha"]);
    let err = one.with(probe("probe-alpha", 9_002, 91)).unwrap_err();
    assert!(
        matches!(err, DescriptorError::DuplicateName { .. }),
        "{err:?}"
    );
    assert!(
        err.to_string().contains("probe-alpha"),
        "error must name the command: {err}"
    );
    assert_eq!(
        snapshot(&one, true, &["probe-alpha"]),
        snap,
        "a refused add must not partially register"
    );
    let err = DescriptorRegistry::from_descriptors([
        probe("probe-beta", 9_001, 90),
        probe("probe-beta", 9_002, 91),
    ])
    .unwrap_err();
    assert!(
        matches!(err, DescriptorError::DuplicateName { .. }),
        "{err:?}"
    );
    // The same descriptor name as a shipping command is refused too: there is
    // exactly one canonical owner per name.
    // (Vacuous only while no shipping command has migrated; the consumer
    // contract proves it against the full built-in set.)
    for name in base.names() {
        let err = base.with(probe(name, 9_003, 92)).unwrap_err();
        assert!(
            matches!(err, DescriptorError::DuplicateName { .. }),
            "/{name}: {err:?}"
        );
    }
}

#[test]
fn every_required_field_is_enforced_and_named() {
    for field in [
        "definition",
        "handler",
        "gate",
        "permission",
        "help",
        "e2e",
        "parity",
        "publish_order",
    ] {
        let err = probe_missing("probe-gamma", field)
            .err()
            .unwrap_or_else(|| panic!("a descriptor without `{field}` must not build"));
        let text = err.to_string();
        assert!(
            text.contains("probe-gamma"),
            "error must name the command: {text}"
        );
        if field == "handler" {
            assert!(
                matches!(err, DescriptorError::MissingHandler { .. }),
                "{err:?}"
            );
        } else {
            match err {
                DescriptorError::Incomplete {
                    field: reported, ..
                } => {
                    assert_eq!(reported, field, "{text}");
                }
                other => panic!("`{field}` missing must be Incomplete, got {other:?}"),
            }
        }
    }
    // Sanity: leaving nothing out builds.
    assert!(probe_missing("probe-gamma", "none").is_ok());
}

#[test]
fn ambiguous_permissions_are_refused() {
    let base = || {
        CommandDescriptor::builder("probe-delta")
            .handler(HandlerId::Descriptor("probe-delta"))
            .gate(E2eGate::Always)
            .help(HelpPlacement {
                group: "Synthetic probes",
                group_order: 900,
                needs: None,
                position: 1,
            })
            .e2e_case(SUCCESS, &[])
            .parity_row(91)
            .publish_order(1)
    };
    let def = |bits: Option<u64>| {
        let d = CommandDefinition::new("probe-delta", "d");
        match bits {
            Some(bits) => d.permissions(bits),
            None => d,
        }
    };
    let ambiguous = |r: Result<CommandDescriptor, DescriptorError>, what: &str| match r {
        Err(DescriptorError::AmbiguousPermission { .. }) => {}
        other => panic!("{what}: expected AmbiguousPermission, got {other:?}"),
    };
    // Published picker default says Manage Server, runtime says everyone.
    ambiguous(
        base()
            .definition(def(Some(PERM_MANAGE_GUILD)))
            .open()
            .build(),
        "published bits but open runtime",
    );
    // Published bits and runtime bits disagree.
    ambiguous(
        base()
            .definition(def(Some(PERM_MANAGE_GUILD)))
            .requires(PERM_MANAGE_EVENTS, RouterRefusal::ManageEventsRequired)
            .build(),
        "published and runtime masks differ",
    );
    // Runtime restricted, nothing published.
    ambiguous(
        base()
            .definition(def(None))
            .requires(PERM_MANAGE_GUILD, RouterRefusal::ManageServerRequired)
            .build(),
        "restricted runtime without a published default",
    );
    // Open and restricted declared together.
    ambiguous(
        base()
            .definition(def(Some(PERM_MANAGE_GUILD)))
            .open()
            .requires(PERM_MANAGE_GUILD, RouterRefusal::ManageServerRequired)
            .build(),
        "open() and requires() both declared",
    );
    // A requirement with no bits is not a requirement.
    ambiguous(
        base()
            .definition(def(None))
            .requires(0, RouterRefusal::ManageServerRequired)
            .build(),
        "requires(0, ..)",
    );
    // An alternative cannot widen an open command.
    ambiguous(
        base()
            .definition(def(None))
            .open()
            .also_allowed_by(ADMINISTRATOR)
            .build(),
        "alternative on an open command",
    );
    // The unambiguous forms build.
    assert!(base().definition(def(None)).open().build().is_ok());
    assert!(base()
        .definition(def(Some(PERM_MANAGE_GUILD)))
        .requires(PERM_MANAGE_GUILD, RouterRefusal::ManageServerRequired)
        .build()
        .is_ok());
}

#[test]
fn an_explicit_alternative_widens_only_the_command_that_declares_it() {
    let staff = CommandDescriptor::builder("probe-staff")
        .definition(CommandDefinition::new("probe-staff", "d").permissions(PERM_MANAGE_GUILD))
        .handler(HandlerId::Descriptor("probe-staff"))
        .gate(E2eGate::Always)
        .requires(PERM_MANAGE_GUILD, RouterRefusal::ManageServerRequired)
        .also_allowed_by(ADMINISTRATOR)
        .help(HelpPlacement {
            group: "Synthetic probes",
            group_order: 900,
            needs: Some("Manage Server"),
            position: 5,
        })
        .e2e_case(SUCCESS, &["Perm:ManageServerRequired"])
        .parity_row(92)
        .publish_order(5)
        .build()
        .unwrap();
    let reg = DescriptorRegistry::builtin()
        .with(staff)
        .unwrap()
        .with(probe("probe-alpha", 9_001, 90))
        .unwrap();
    assert!(reg
        .permission("probe-staff")
        .unwrap()
        .allows(Some(ADMINISTRATOR)));
    assert!(reg
        .permission("probe-staff")
        .unwrap()
        .allows(Some(PERM_MANAGE_GUILD)));
    assert!(!reg
        .permission("probe-staff")
        .unwrap()
        .allows(Some(PERM_MANAGE_EVENTS)));
    assert!(!reg
        .permission("probe-alpha")
        .unwrap()
        .allows(Some(ADMINISTRATOR)));
}

#[test]
fn legal_name_boundaries_are_accepted() {
    for name in [
        "-",
        "_",
        "0",
        "a",
        "a-z_09",
        "abcdefghijklmnopqrstuvwxyz012345",
    ] {
        let result = probe_builder(name, 1, 93).build();
        assert!(result.is_ok(), "{name:?}: {result:?}");
    }
}

#[test]
fn malformed_names_are_refused() {
    let too_long: &'static str = Box::leak("a".repeat(33).into_boxed_str());
    for name in [
        "",
        "Upper",
        "has space",
        "slash/name",
        "dot.name",
        too_long,
        "emoji-\u{1F600}",
    ] {
        let result = CommandDescriptor::builder(name)
            .definition(CommandDefinition::new(name, "d"))
            .handler(HandlerId::Descriptor("x"))
            .gate(E2eGate::Always)
            .open()
            .help(HelpPlacement {
                group: "Synthetic probes",
                group_order: 900,
                needs: None,
                position: 1,
            })
            .e2e_case(SUCCESS, &[])
            .parity_row(93)
            .publish_order(1)
            .build();
        assert!(
            matches!(result, Err(DescriptorError::InvalidName { .. })),
            "{name:?}: {result:?}"
        );
    }
    // The wire definition must carry the same name as the descriptor.
    let mismatch = CommandDescriptor::builder("probe-one")
        .definition(CommandDefinition::new("probe-two", "d"))
        .handler(HandlerId::Descriptor("probe-one"))
        .gate(E2eGate::Always)
        .open()
        .help(HelpPlacement {
            group: "Synthetic probes",
            group_order: 900,
            needs: None,
            position: 1,
        })
        .e2e_case(SUCCESS, &[])
        .parity_row(93)
        .publish_order(1)
        .build();
    assert!(
        mismatch.is_err(),
        "definition name must equal descriptor name"
    );
}

// ---------------------------------------------------------------------------
// Determinism, ordering, grouping
// ---------------------------------------------------------------------------

fn permutations(items: Vec<CommandDescriptor>) -> Vec<Vec<CommandDescriptor>> {
    fn go(
        rest: &mut Vec<CommandDescriptor>,
        current: &mut Vec<CommandDescriptor>,
        out: &mut Vec<Vec<CommandDescriptor>>,
    ) {
        if rest.is_empty() {
            out.push(current.clone());
            return;
        }
        for i in 0..rest.len() {
            let item = rest.remove(i);
            current.push(item.clone());
            go(rest, current, out);
            current.pop();
            rest.insert(i, item);
        }
    }
    let mut out = Vec::new();
    go(&mut items.clone(), &mut Vec::new(), &mut out);
    out
}

#[test]
fn registration_order_never_changes_any_projection() {
    // Two share a publish order on purpose: ties break by name, never by the
    // order the descriptors happened to be supplied.
    let set = || {
        vec![
            probe("probe-d", 9_300, 94),
            probe("probe-a", 9_100, 91),
            probe("probe-c", 9_200, 93),
            probe("probe-b", 9_200, 92),
        ]
    };
    let names = ["probe-a", "probe-b", "probe-c", "probe-d"];
    let mut snapshots = Vec::new();
    for perm in permutations(set()) {
        let reg = DescriptorRegistry::from_descriptors(perm)
            .expect("distinct descriptors register in any order");
        snapshots.push(snapshot(&reg, true, &names));
    }
    assert_eq!(snapshots.len(), 24);
    assert!(
        snapshots.windows(2).all(|w| w[0] == w[1]),
        "projection depends on input order"
    );
    let empty =
        DescriptorRegistry::from_descriptors(Vec::new()).expect("an empty registry is valid");
    assert!(empty.names().is_empty());
    let reg = empty
        .with(probe("probe-d", 9_300, 94))
        .unwrap()
        .with(probe("probe-a", 9_100, 91))
        .unwrap()
        .with(probe("probe-c", 9_200, 93))
        .unwrap()
        .with(probe("probe-b", 9_200, 92))
        .unwrap();
    assert_eq!(reg.names(), ["probe-a", "probe-b", "probe-c", "probe-d"]);
    assert_eq!(
        snapshots[0],
        snapshot(&reg, true, &names),
        "incremental add equals bulk build"
    );
}

#[test]
fn repeated_builds_are_byte_identical() {
    let build = || {
        DescriptorRegistry::builtin()
            .with(probe("probe-alpha", 9_001, 90))
            .unwrap()
    };
    let first = snapshot(&build(), true, &["probe-alpha"]);
    for _ in 0..3 {
        assert_eq!(snapshot(&build(), true, &["probe-alpha"]), first);
    }
    let handles: Vec<_> = (0..4)
        .map(|_| std::thread::spawn(move || snapshot(&build(), true, &["probe-alpha"])))
        .collect();
    for handle in handles {
        assert_eq!(handle.join().unwrap(), first);
    }
}

#[test]
fn a_shared_parity_row_is_allowed_for_different_names_but_never_a_shared_name() {
    let reg = DescriptorRegistry::builtin()
        .with(probe("probe-lfg", 9_001, 90))
        .unwrap()
        .with(probe("probe-lfg-edit", 9_002, 90))
        .expect("two commands may share one parity row (LFG and LFG-edit do today)");
    let shared: Vec<_> = reg
        .permissions()
        .into_iter()
        .filter(|row| row.parity_row == 90)
        .map(|row| row.command)
        .collect();
    assert_eq!(shared, ["probe-lfg", "probe-lfg-edit"]);
    assert!(reg.permission("probe-lfg").is_some() && reg.permission("probe-lfg-edit").is_some());
}

#[test]
fn help_groups_must_agree_and_render_in_declared_positions() {
    let in_group = |name: &'static str, position: u32, order: u32, needs: Option<&'static str>| {
        probe_builder_in(
            name,
            position,
            90 + position as u8,
            HelpPlacement {
                group: "Synthetic probes",
                group_order: order,
                needs,
                position,
            },
        )
        .build()
        .unwrap()
    };
    // Conflicting group metadata for one title is ambiguous: refuse.
    let conflict = DescriptorRegistry::builtin()
        .with(in_group("probe-a", 1, 900, Some("Manage Server")))
        .unwrap()
        .with(in_group("probe-b", 2, 901, Some("Manage Server")));
    assert!(
        matches!(conflict, Err(DescriptorError::ConflictingHelpGroup { .. })),
        "{conflict:?}"
    );
    let conflict = DescriptorRegistry::builtin()
        .with(in_group("probe-a", 1, 900, Some("Manage Server")))
        .unwrap()
        .with(in_group("probe-b", 2, 900, None));
    assert!(
        matches!(conflict, Err(DescriptorError::ConflictingHelpGroup { .. })),
        "{conflict:?}"
    );
    // Agreeing members render under one header, ordered by position even when
    // registered in the opposite order.
    let reg = DescriptorRegistry::builtin()
        .with(in_group("probe-b", 2, 900, Some("Manage Server")))
        .unwrap()
        .with(in_group("probe-a", 1, 900, Some("Manage Server")))
        .unwrap();
    let published = InteractionRouter::with_registry(gates(true), &reg)
        .publish_set(&[])
        .unwrap();
    let help = reg.help_text(&published);
    assert_eq!(help.matches("**Synthetic probes**").count(), 1, "{help}");
    assert!(
        help.contains("**Synthetic probes** — needs Manage Server\n/probe-a · /probe-b\n"),
        "{help}"
    );
}

#[test]
fn every_declared_e2e_case_survives_in_declaration_order() {
    let multi = probe_builder("probe-multi", 9_005, 95)
        .e2e_case("Second observable shape.", &["Valid:note:max100"])
        .e2e_case("Third observable shape.", &[])
        .build()
        .unwrap();
    let reg = DescriptorRegistry::builtin().with(multi).unwrap();
    let rows: Vec<_> = reg
        .e2e_matrix()
        .into_iter()
        .filter(|r| r.command == "probe-multi")
        .collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.iter().map(|r| r.success_shape).collect::<Vec<_>>(),
        [
            SUCCESS,
            "Second observable shape.",
            "Third observable shape."
        ]
    );
    for row in &rows {
        assert_eq!(row.parity_row, 95);
        assert_eq!(row.gate, E2eGate::Announcements);
        assert_eq!(row.required_permissions, PERM_MANAGE_GUILD);
        assert_eq!(row.handler, "Descriptor(\"probe-multi\")");
    }
    assert_eq!(rows[1].denials, ["Valid:note:max100"]);
    // Cases are metadata, not commands: still exactly one published definition.
    let published = InteractionRouter::with_registry(gates(true), &reg)
        .publish_set(&[])
        .unwrap();
    assert_eq!(
        published.iter().filter(|d| d.name == "probe-multi").count(),
        1
    );
}

#[test]
fn a_policy_hook_travels_with_the_descriptor() {
    let hooked = probe_builder("probe-hooked", 9_006, 96)
        .policy_hook(PolicyHook::MemberModeration)
        .build()
        .unwrap();
    let reg = DescriptorRegistry::builtin().with(hooked).unwrap();
    assert_eq!(
        reg.permission("probe-hooked")
            .map(|r: CommandPermission| r.policy_hook),
        Some(Some(PolicyHook::MemberModeration))
    );
}

// ---------------------------------------------------------------------------
// Negative control: resemblance is not registration
// ---------------------------------------------------------------------------

#[test]
fn an_unregistered_command_cannot_become_reachable_because_its_name_resembles_a_registered_one() {
    let reg = DescriptorRegistry::builtin()
        .with(probe("probe-alpha", 9_001, 90))
        .unwrap();
    let router = InteractionRouter::with_registry(gates(true), &reg);
    let published = router.publish_set(&[]).unwrap();
    // Near-misses of a synthetic registration and of a shipping built-in.
    for name in [
        "Probe-Alpha",
        "probe-alpha ",
        " probe-alpha",
        "probe_alpha",
        "probe-alph",
        "probe-alpha2",
        "probealpha",
        "/probe-alpha",
        "Rank",
        "rank ",
        "rank2",
        "r\u{430}nk",
    ] {
        assert!(
            reg.permission(name).is_none(),
            "{name:?} resolved a permission row"
        );
        assert!(!reg.names().contains(&name), "{name:?} is registered");
        assert!(
            !published.iter().any(|d| d.name == name),
            "{name:?} was published"
        );
        assert!(!reg.e2e_matrix().iter().any(|r| r.command == name));
        assert_eq!(
            route(&router, name, Some(GUILD), Some(u64::MAX)),
            "Unknown",
            "{name:?}"
        );
        assert_eq!(
            route(&router, name, Some(OTHER_GUILD), Some(u64::MAX)),
            "Ignore",
            "{name:?}"
        );
    }
    // Registering one near-miss does not leak its reachability to the rest.
    let wider = reg.with(probe("probe-alpha2", 9_002, 91)).unwrap();
    let wider_router = InteractionRouter::with_registry(gates(true), &wider);
    assert_eq!(
        route(&wider_router, "probe-alpha", Some(GUILD), Some(u64::MAX)),
        "Handled { handler: Descriptor(\"probe-alpha\") }"
    );
    assert_eq!(
        route(&wider_router, "probe-alpha2", Some(GUILD), Some(u64::MAX)),
        "Handled { handler: Descriptor(\"probe-alpha2\") }"
    );
    assert_eq!(
        route(&wider_router, "probe-alph", Some(GUILD), Some(u64::MAX)),
        "Unknown"
    );
    // A removed command vanishes from dispatch, not just from the listing.
    let gone = wider.without("probe-alpha");
    let gone_router = InteractionRouter::with_registry(gates(true), &gone);
    assert_eq!(
        route(&gone_router, "probe-alpha", Some(GUILD), Some(u64::MAX)),
        "Unknown"
    );
    assert_eq!(
        route(&gone_router, "probe-alpha2", Some(GUILD), Some(u64::MAX)),
        "Handled { handler: Descriptor(\"probe-alpha2\") }"
    );
}

#[test]
fn a_custom_row_cannot_shadow_or_borrow_a_descriptor_name() {
    let reg = DescriptorRegistry::builtin()
        .with(probe("probe-alpha", 9_001, 90))
        .unwrap();
    let router = InteractionRouter::with_registry(gates(true), &reg);
    let squatter = [two_bot_core::CustomCommand {
        name: "probe-alpha".to_owned(),
        description: "squatter".to_owned(),
        enabled: true,
    }];
    let with = router.publish_set(&squatter).unwrap();
    assert_eq!(with.iter().filter(|d| d.name == "probe-alpha").count(), 1);
    assert_eq!(
        with.iter()
            .find(|d| d.name == "probe-alpha")
            .unwrap()
            .description,
        "Synthetic probe command",
        "the descriptor's definition wins over a same-named custom row"
    );
    let ctx = SlashContext {
        name: "probe-alpha",
        guild_id: Some(GUILD),
        actor_permissions: Some(u64::MAX),
        custom_row: Some(true),
    };
    assert!(matches!(
        router.route_slash(&ctx),
        SlashOutcome::Handled {
            handler: HandlerId::Descriptor("probe-alpha")
        }
    ));
    // Once the descriptor is gone the name is an ordinary free custom name.
    let gone = InteractionRouter::with_registry(gates(true), &reg.without("probe-alpha"));
    assert!(matches!(
        gone.route_slash(&ctx),
        SlashOutcome::Handled {
            handler: HandlerId::AutomationCustom
        }
    ));
}

// ---------------------------------------------------------------------------
// Registered metadata is not an executing handler
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Stub(HandlerId);

impl InteractionHandler for Stub {
    fn id(&self) -> HandlerId {
        self.0
    }
}

#[test]
fn metadata_alone_never_counts_as_an_executing_handler() {
    let reg = DescriptorRegistry::builtin()
        .with(probe("probe-alpha", 9_001, 90))
        .unwrap();
    let mut router = InteractionRouter::with_registry(gates(true), &reg);
    // Routing names the owner, as it does for every builtin ...
    assert_eq!(
        route(&router, "probe-alpha", Some(GUILD), Some(u64::MAX)),
        "Handled { handler: Descriptor(\"probe-alpha\") }"
    );
    // ... but the registry reports the owner as not registered until a real
    // handler is, so conformance tests can fail a published command that
    // nobody executes.
    assert!(router
        .handler_for(&HandlerId::Descriptor("probe-alpha"))
        .is_none());
    assert!(reg.unregistered_handlers(&router).contains(&"probe-alpha"));
    router.register(Box::new(Stub(HandlerId::Descriptor("probe-alpha"))));
    assert!(router
        .handler_for(&HandlerId::Descriptor("probe-alpha"))
        .is_some());
    assert!(!reg.unregistered_handlers(&router).contains(&"probe-alpha"));
}
