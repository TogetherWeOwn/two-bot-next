//! Coverage pin for the staging E2E command-scope matrix.
//!
//! The matrix (`crates/core/src/e2e_matrix.rs`, rendered for humans in
//! `docs/staging-e2e-command-matrix.md`) is the scope contract for the future
//! live suite. These tests fail when a command ships without a matrix row:
//! the matrix must equal the all-gates-on `publish_set`, each row's gate and
//! permission bits must match the registry, and the doc must name every row.
//! No guild, no network, no database.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use two_bot_core::command_permissions::command_permission;
use two_bot_core::e2e_matrix::{e2e_command_matrix, E2eGate};
use two_bot_core::router::{InteractionRouter, RouterGates};

fn all_gates_on() -> RouterGates {
    RouterGates {
        configured_guild: None,
        scorecard: true,
        automations: true,
        announcements: true,
        moderation: true,
        tickets: true,
        self_roles: true,
        onboarding_picker: true,
        session_picker: true,
    }
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn matrix_matches_the_published_set_exactly() {
    let published = InteractionRouter::new(all_gates_on())
        .publish_set(&[])
        .expect("full set assembles");
    let mut published_names: Vec<&str> = published.iter().map(|d| d.name.as_str()).collect();
    published_names.sort_unstable();
    let matrix = e2e_command_matrix();
    let mut matrix_names: Vec<&str> = matrix.iter().map(|r| r.command).collect();
    matrix_names.sort_unstable();
    assert_eq!(
        matrix_names, published_names,
        "matrix drift: ship a matrix row with every new command and drop the row with every removed one"
    );
    assert_eq!(matrix.len(), 27, "contract covers the 27 built-in commands");
}

#[test]
fn matrix_gates_and_permissions_match_the_registry() {
    let published = InteractionRouter::new(all_gates_on())
        .publish_set(&[])
        .expect("full set assembles");
    let by_name: HashMap<&str, _> = published.iter().map(|d| (d.name.as_str(), d)).collect();
    for row in e2e_command_matrix() {
        let def = by_name
            .get(row.command)
            .unwrap_or_else(|| panic!("matrix row /{} missing from publish_set", row.command));
        let published_bits: u64 = def
            .default_member_permissions
            .as_deref()
            .map(|raw| raw.parse().expect("decimal permission bitfield"))
            .unwrap_or(0);
        assert_eq!(
            row.required_permissions, published_bits,
            "/{} permission bits drifted from the registry",
            row.command
        );
        let table = command_permission(row.command)
            .unwrap_or_else(|| panic!("/{} missing from command_permissions", row.command));
        assert_eq!(
            table.required_permissions, row.required_permissions,
            "/{} permission row drifted from the permission table",
            row.command
        );
        assert!(
            !def.dm_permission,
            "/{} must stay guild-only (dm_permission false)",
            row.command
        );
        let expected_gate = if ["rank", "leaderboard"].contains(&row.command) {
            E2eGate::Always
        } else if row.command == "attendance" {
            E2eGate::Scorecard
        } else if [
            "command",
            "command-remove",
            "command-list",
            "schedule",
            "schedule-remove",
            "schedule-list",
            "sticky",
            "sticky-remove",
        ]
        .contains(&row.command)
        {
            E2eGate::Automations
        } else if [
            "rsvp",
            "rsvp-attendance",
            "lfg",
            "lfg-close",
            "feed-add",
            "feed-remove",
            "feed-list",
        ]
        .contains(&row.command)
        {
            E2eGate::Announcements
        } else {
            E2eGate::Moderation
        };
        assert_eq!(
            row.gate, expected_gate,
            "/{} gate drifted from the publish wiring",
            row.command
        );
    }
}

#[test]
fn matrix_doc_names_every_row() {
    let doc = std::fs::read_to_string(repository_root().join("docs/staging-e2e-command-matrix.md"))
        .expect("matrix doc exists");
    let mut missing = Vec::new();
    for row in e2e_command_matrix() {
        if !doc.contains(&format!("/{}", row.command)) {
            missing.push(row.command);
        }
    }
    assert!(missing.is_empty(), "matrix doc drops rows for: {missing:?}");
    // The doc must not promise what the registry does not publish.
    let published: HashSet<&str> = InteractionRouter::new(all_gates_on())
        .publish_set(&[])
        .expect("full set assembles")
        .iter()
        .map(|d| d.name.as_str())
        .collect();
    for name in ["create", "templateassistant", "rota-acknowledge"] {
        assert!(
            !published.contains(name),
            "out-of-scope /{name} leaked into the published set under test"
        );
    }
}
