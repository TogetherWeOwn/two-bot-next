//! Cutover rehearsal harness (TOG-12143): dry-run command restoration for
//! both global and guild scopes through the mock Discord transport.
//!
//! Every test is a pure fixture rehearsal: no network, no database, no
//! credentials. The harness diffs frozen baseline vs staged registry per
//! scope, verifies the permission-restore access mapping fails closed with a
//! named gap, simulates all five watch-window drift classes (revoked role
//! allow, changed defaults, add/delete/rename) and checks the reconcile
//! report preserves legitimate window changes without broadening access.

use std::collections::{BTreeMap, BTreeSet};

use two_bot_core::command_restoration::{
    diff_scopes, reconcile_watch_window, rehearse_scope, simulate_watch_window,
    verify_permission_restore_access, AccessGap, CommandScope, DriftEvent, MockDiscord, MockOp,
    OverrideTarget, PermissionOverride, PermissionRestoreAccess, RehearsalInput, ScopedRegistry,
};
use two_bot_core::commands::CommandDefinition;

const GUILD_ID: u64 = 1545644954272137297;

fn global_baseline() -> ScopedRegistry {
    ScopedRegistry {
        scope: CommandScope::Global,
        definitions: vec![
            CommandDefinition::new("rank", "Show your XP, level and server rank."),
            CommandDefinition::new("leaderboard", "Show the server XP leaderboard."),
            CommandDefinition::new("ban", "Ban a member").permissions(4),
        ],
        // Guild overrides on global commands live on the guild scope, not here.
        overrides: Vec::new(),
    }
}

fn guild_baseline() -> ScopedRegistry {
    ScopedRegistry {
        scope: CommandScope::Guild { guild_id: GUILD_ID },
        definitions: vec![
            CommandDefinition::new("rsvp", "RSVP to a Discord scheduled event"),
            CommandDefinition::new("purge", "Delete recent messages").permissions(8192),
        ],
        overrides: vec![
            PermissionOverride::explicit("purge", "111", OverrideTarget::Role, true),
            PermissionOverride::explicit("purge", "222", OverrideTarget::Role, false),
            // Synced row: inherits the application default, never part of a
            // per-command PUT array.
            PermissionOverride {
                command_name: "rsvp".to_owned(),
                resource_id: "333".to_owned(),
                target: OverrideTarget::Role,
                allow: true,
                synced: true,
            },
        ],
    }
}

#[test]
fn harness_reports_definition_drift_classes_on_guild_fixtures() {
    let baseline = guild_baseline();
    // Definition-level drift only: override rows move with the rename, so
    // override classes are covered by the next test on a shared name.
    let (live, lineage) = simulate_watch_window(
        &baseline,
        &[
            DriftEvent::AddedCommand {
                definition: CommandDefinition::new("faq", "Guild FAQ command"),
            },
            DriftEvent::DeletedCommand {
                name: "rsvp".to_owned(),
            },
            DriftEvent::RenamedCommand {
                from: "purge".to_owned(),
                to: "clean".to_owned(),
            },
        ],
    );
    let classes: BTreeSet<&str> = diff_scopes(&baseline, &live, &lineage)
        .iter()
        .map(|drift| drift.class())
        .collect();
    assert_eq!(classes, BTreeSet::from(["added", "deleted", "renamed"]));
}

#[test]
fn harness_reports_override_drift_classes_on_a_shared_command() {
    let baseline = guild_baseline();
    let (live, lineage) = simulate_watch_window(
        &baseline,
        &[
            // Revoked role allow on `purge`.
            DriftEvent::RevokedAllow {
                command: "purge".to_owned(),
                resource_id: "111".to_owned(),
            },
            // New explicit deny row on `purge`.
            DriftEvent::ChangedOverride {
                row: PermissionOverride::explicit("purge", "444", OverrideTarget::Role, false),
            },
            // Existing deny row flipped to allow.
            DriftEvent::ChangedOverride {
                row: PermissionOverride::explicit("purge", "222", OverrideTarget::Role, true),
            },
        ],
    );
    assert!(lineage.is_empty());
    let classes: BTreeSet<&str> = diff_scopes(&baseline, &live, &lineage)
        .iter()
        .map(|drift| drift.class())
        .collect();
    assert_eq!(
        classes,
        BTreeSet::from(["override_added", "override_removed", "override_changed"])
    );
    let removed: Vec<_> = diff_scopes(&baseline, &live, &lineage)
        .into_iter()
        .filter(|drift| drift.class() == "override_removed")
        .collect();
    assert!(
        removed
            .iter()
            .any(|drift| format!("{drift:?}").contains("revoked_allow: true")),
        "a removed allow row must flag the revocation: {removed:?}"
    );
}

#[test]
fn harness_reports_changed_defaults_on_global_fixtures() {
    let baseline = global_baseline();
    let (live, lineage) = simulate_watch_window(
        &baseline,
        &[DriftEvent::ChangedDefault {
            command: "ban".to_owned(),
            new_default: Some("32".to_owned()),
        }],
    );
    let drifts = diff_scopes(&baseline, &live, &lineage);
    assert!(
        drifts
            .iter()
            .any(|drift| drift.class() == "default_changed"),
        "changed default must classify; got {drifts:?}"
    );
}

#[test]
fn missing_permission_access_fails_closed_with_a_named_gap() {
    // Bot-token attempt: immediate fail-closed, even fully provisioned.
    let bot_token = PermissionRestoreAccess {
        attempted_with_bot_token: true,
        ..PermissionRestoreAccess::provisioned()
    };
    assert_eq!(
        verify_permission_restore_access("purge", &bot_token),
        Err(AccessGap::BotTokenInsufficient)
    );

    // Every missing element names its own gap.
    let gaps = [
        (
            PermissionRestoreAccess {
                bearer_scope_update: false,
                ..PermissionRestoreAccess::provisioned()
            },
            "MissingBearerScope",
        ),
        (
            PermissionRestoreAccess {
                user_manage_guild: false,
                ..PermissionRestoreAccess::provisioned()
            },
            "MissingManageGuild",
        ),
        (
            PermissionRestoreAccess {
                user_manage_roles: false,
                ..PermissionRestoreAccess::provisioned()
            },
            "MissingManageRoles",
        ),
        (
            PermissionRestoreAccess {
                user_can_run_command: false,
                ..PermissionRestoreAccess::provisioned()
            },
            "CannotRunCommand",
        ),
        (
            PermissionRestoreAccess {
                user_can_manage_resources: false,
                ..PermissionRestoreAccess::provisioned()
            },
            "CannotManageResources",
        ),
    ];
    for (access, gap) in gaps {
        let err = verify_permission_restore_access("purge", &access).expect_err("must fail");
        assert!(
            format!("{err:?}").contains(gap),
            "expected {gap}, got {err:?}"
        );
    }
}

#[test]
fn reconciled_target_preserves_window_changes_without_broadening() {
    let baseline = guild_baseline();
    // Staged registry: the reviewed Next set (keeps `purge` with its frozen
    // allow row so the window revocation has something to carry, drops `rsvp`
    // deliberately, adds nothing yet).
    let staged = ScopedRegistry {
        scope: baseline.scope.clone(),
        definitions: vec![
            CommandDefinition::new("purge", "Delete recent messages").permissions(8192)
        ],
        overrides: vec![
            PermissionOverride::explicit("purge", "111", OverrideTarget::Role, true),
            PermissionOverride::explicit("purge", "222", OverrideTarget::Role, false),
        ],
    };
    let (live, lineage) = simulate_watch_window(
        &baseline,
        &[
            DriftEvent::RevokedAllow {
                command: "purge".to_owned(),
                resource_id: "111".to_owned(),
            },
            DriftEvent::AddedCommand {
                definition: CommandDefinition::new("faq", "Guild FAQ command"),
            },
        ],
    );
    let mock = MockDiscord::from_baseline(&[baseline.clone()]);
    let live_ids: BTreeMap<String, String> = mock.live_ids_for_scope(&baseline.scope);
    let report = reconcile_watch_window(&baseline, &staged, &live, &lineage, &live_ids);
    assert!(report.is_go(), "frozen: {:?}", report.frozen_reasons);
    // Legitimate addition preserved, deliberate deletion preserved, revoked
    // allow carried (never reintroduced from the baseline).
    assert!(report.preserved_additions.contains(&"faq".to_owned()));
    assert!(report.preserved_deletions.contains(&"rsvp".to_owned()));
    assert!(report.carried_revocations.contains(&"purge:111".to_owned()));
    assert!(report
        .target_overrides
        .iter()
        .all(|row| !(row.command_name == "purge" && row.resource_id == "111" && row.allow)));
}

#[test]
fn restoring_the_baseline_must_not_undo_window_changes() {
    // A naive "reset to T_f" would resurrect the deleted `rsvp`, drop the
    // added `faq` and reintroduce the revoked allow. The reconciler must not:
    // `rsvp` deleted in the window but still required by staged is
    // unexplained drift, so the scope freezes instead of silently
    // resurrecting or dropping it.
    let baseline = guild_baseline();
    let staged = ScopedRegistry {
        scope: baseline.scope.clone(),
        definitions: baseline.definitions.clone(),
        overrides: baseline.overrides.clone(),
    };
    let (live, lineage) = simulate_watch_window(
        &baseline,
        &[
            DriftEvent::RevokedAllow {
                command: "purge".to_owned(),
                resource_id: "111".to_owned(),
            },
            DriftEvent::AddedCommand {
                definition: CommandDefinition::new("faq", "Guild FAQ command"),
            },
            DriftEvent::DeletedCommand {
                name: "rsvp".to_owned(),
            },
        ],
    );
    let mock = MockDiscord::from_baseline(&[baseline.clone()]);
    let live_ids = mock.live_ids_for_scope(&baseline.scope);
    let report = reconcile_watch_window(&baseline, &staged, &live, &lineage, &live_ids);
    assert!(!report.is_go());
    assert!(report
        .frozen_reasons
        .iter()
        .any(|reason| reason.contains("rsvp")));
}

#[test]
fn full_rehearsal_is_go_on_both_scopes_with_provisioned_access() {
    for baseline in [global_baseline(), guild_baseline()] {
        let staged = baseline.clone();
        let mut mock = MockDiscord::from_baseline(&[baseline.clone()]);
        let input = RehearsalInput {
            baseline: baseline.clone(),
            staged: staged.clone(),
            access: PermissionRestoreAccess::provisioned(),
            drift: vec![DriftEvent::AddedCommand {
                definition: CommandDefinition::new("faq", "Window FAQ command"),
            }],
        };
        let report = rehearse_scope(&input, &mut mock);
        assert!(
            report.access_gaps.is_empty(),
            "access gaps on {:?}: {:?}",
            report.scope,
            report.access_gaps
        );
        assert!(
            report.reconcile.is_go(),
            "frozen on {:?}: {:?}",
            report.scope,
            report.reconcile.frozen_reasons
        );
        assert!(
            report.readback_mismatches.is_empty(),
            "read-back mismatches on {:?}: {:?}",
            report.scope,
            report.readback_mismatches
        );
        assert!(report.is_go(), "rehearsal not GO on {:?}", report.scope);
        // The reconciled `faq` addition survived the mock PUT + read-back.
        let live_names = mock.snapshot(&report.scope).definition_names();
        assert!(live_names.contains("faq"));
    }
}

#[test]
fn full_rehearsal_stops_before_reconciled_puts_without_access() {
    let baseline = guild_baseline();
    let mut mock = MockDiscord::from_baseline(&[baseline.clone()]);
    let input = RehearsalInput {
        baseline: baseline.clone(),
        staged: baseline.clone(),
        access: PermissionRestoreAccess {
            bearer_scope_update: false,
            ..PermissionRestoreAccess::provisioned()
        },
        drift: Vec::new(),
    };
    let report = rehearse_scope(&input, &mut mock);
    assert!(!report.is_go());
    assert!(!report.access_gaps.is_empty());
    // The live-world setup PUT (empty drift re-applies the baseline) is
    // scaffolding, not a rehearsal write: no reconciled definition PUT beyond
    // it, no permission PUT and no read-back may follow a failed access check.
    let reconciled_puts = mock
        .log
        .iter()
        .filter(|op| matches!(op, MockOp::PutDefinitions { .. }))
        .count();
    assert_eq!(
        reconciled_puts, 1,
        "exactly the live-world setup PUT is allowed: {:?}",
        mock.log
    );
    assert!(
        mock.log
            .iter()
            .all(|op| !matches!(op, MockOp::PutPermissions { .. } | MockOp::ReadBack { .. })),
        "permission writes and read-back must not follow a failed access check: {:?}",
        mock.log
    );
}
