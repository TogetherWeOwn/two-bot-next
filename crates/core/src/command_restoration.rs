//! Command restoration rehearsal (TOG-12143): dry-run harness for the
//! `docs/cutover.md` precondition "Command definitions and separate guild
//! permission recovery".
//!
//! Cutover requires restoration to be rehearsed for **both global and guild
//! scopes**, permission-restore access to be verified upfront (the bot token
//! is insufficient for permission writes), and watch-window registry drift to
//! be reconciled rather than reset to the frozen baseline. No rehearsal
//! harness existed; this module is the pure, framework-free core of it, so
//! every diff, access check and reconcile rule unit-tests without Discord, a
//! database or credentials.
//!
//! Model (one-to-one with the cutover procedure):
//! - [`ScopedRegistry`] — frozen capture of one scope: command definitions
//!   plus the **separate** guild permission overrides, including guild
//!   overrides on global commands. A definition GET/PUT never carries the
//!   overrides; deleting or renaming a command permanently deletes its
//!   permissions, so the two travel together here.
//! - [`diff_scopes`] — frozen baseline vs staged registry, per scope, with
//!   the drift classes cutover rehearses: additions, deletions, renames (only
//!   with approved name lineage — an unexplained delete+add pair stays an
//!   add plus a delete, never a silent rename), definition changes, default
//!   changes and override changes including revoked allows.
//! - [`verify_permission_restore_access`] — the upfront access mapping. Any
//!   missing element fails closed with a named [`AccessGap`]; attempting with
//!   the bot token is an immediate `BotTokenInsufficient`, never a reason to
//!   substitute another credential.
//! - [`reconcile_watch_window`] — rollback reconciliation of live drift
//!   against the frozen baseline: legitimate additions/deletions/renames are
//!   preserved, revocations and tightenings are carried (a removed allow is a
//!   revocation — it is never reintroduced from the baseline), and
//!   unsupported or ambiguous drift keeps commands frozen (`is_go() == false`).
//! - [`MockDiscord`] — mock Discord transport for the dry run: snapshot,
//!   definition PUT (recreated names get fresh server IDs, so the old/current
//!   ID map is exercised), per-command permission PUT (refused without bearer
//!   access, exactly like the real route), and read-back comparison against
//!   the approved reconciled target.
//!
//! Deliberately out of scope: real Discord writes, obtaining credentials,
//! and the authorized REST executor itself (this rehearses the procedure that
//! executor follows). Never point this at production/staging guilds.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::commands::CommandDefinition;

/// Global scope vs one guild scope. Guild overrides on global commands live
/// on the guild scope, matching the cutover capture (definitions per scope,
/// permissions per guild).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum CommandScope {
    Global,
    Guild { guild_id: u64 },
}

impl CommandScope {
    /// Stable key for ID maps and reports (`global` / `guild:<id>`).
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::Global => "global".to_owned(),
            Self::Guild { guild_id } => format!("guild:{guild_id}"),
        }
    }
}

/// Role/user/channel target of a guild permission override. `cutover.md`
/// requires retaining these IDs verbatim, including `guild_id` (`@everyone`)
/// and `guild_id - 1` (All Channels).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum OverrideTarget {
    Role,
    User,
    Channel,
}

/// One explicit per-command guild permission override. `synced == true`
/// means the command inherits the application default (no explicit array);
/// `false` means this row is part of the explicit per-command array that a
/// PUT replaces wholesale — so reconciliation must emit the complete
/// approved array, never a partial patch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOverride {
    /// Command name in the frozen capture (pre-ID-map; IDs resolve at apply).
    pub command_name: String,
    pub resource_id: String,
    pub target: OverrideTarget,
    /// `true` = allow, `false` = deny. A removed `allow` row is a revocation.
    pub allow: bool,
    pub synced: bool,
}

impl PermissionOverride {
    #[must_use]
    pub fn explicit(
        command_name: &str,
        resource_id: &str,
        target: OverrideTarget,
        allow: bool,
    ) -> Self {
        Self {
            command_name: command_name.to_owned(),
            resource_id: resource_id.to_owned(),
            target,
            allow,
            synced: false,
        }
    }
}

/// Frozen capture of one scope: definitions plus the separate permission
/// overrides. Capture both as one baseline; database journals do not record
/// registry edits made directly in Discord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedRegistry {
    pub scope: CommandScope,
    pub definitions: Vec<CommandDefinition>,
    pub overrides: Vec<PermissionOverride>,
}

impl ScopedRegistry {
    #[must_use]
    pub fn definition_names(&self) -> BTreeSet<String> {
        self.definitions
            .iter()
            .map(|def| def.name.clone())
            .collect()
    }

    #[must_use]
    pub fn definition_by_name(&self, name: &str) -> Option<&CommandDefinition> {
        self.definitions.iter().find(|def| def.name == name)
    }

    /// Explicit overrides for one command (synced rows inherit defaults and
    /// are not part of the per-command PUT array).
    #[must_use]
    pub fn explicit_overrides(&self, command_name: &str) -> Vec<&PermissionOverride> {
        self.overrides
            .iter()
            .filter(|row| row.command_name == command_name && !row.synced)
            .collect()
    }
}

/// One classified drift between frozen baseline and a later registry state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryDrift {
    /// Command present in staged/live but absent from the baseline.
    Added { name: String },
    /// Command absent from staged/live but present in the baseline.
    Deleted { name: String },
    /// Delete+add pair explained by approved rename lineage. Without lineage
    /// the pair reports as [`RegistryDrift::Added`] plus
    /// [`RegistryDrift::Deleted`], never a silent rename.
    Renamed { from: String, to: String },
    /// Same name, different wire shape (description, options, …).
    /// `field` names the first differing field (`description`, `options`,
    /// `default_member_permissions`, `dm_permission`, or `wire` for any other
    /// serialized shape difference).
    DefinitionModified { name: String, field: String },
    /// `default_member_permissions` changed: the inherited default moved, so
    /// effective access moved with it even where no explicit override exists.
    DefaultChanged {
        name: String,
        baseline: Option<String>,
        current: Option<String>,
    },
    /// New explicit override row on a command both snapshots share.
    OverrideAdded { name: String, resource_id: String },
    /// Explicit override row removed. A removed `allow` row is a revocation
    /// and must be carried into the reconciled target, never reintroduced.
    OverrideRemoved {
        name: String,
        resource_id: String,
        revoked_allow: bool,
    },
    /// Same resource row, allow/deny flipped.
    OverrideChanged {
        name: String,
        resource_id: String,
        baseline_allow: bool,
        current_allow: bool,
    },
}

impl RegistryDrift {
    /// Short class label for reports (`added`, `deleted`, `renamed`, …).
    #[must_use]
    pub fn class(&self) -> &'static str {
        match self {
            Self::Added { .. } => "added",
            Self::Deleted { .. } => "deleted",
            Self::Renamed { .. } => "renamed",
            Self::DefinitionModified { .. } => "definition_modified",
            Self::DefaultChanged { .. } => "default_changed",
            Self::OverrideAdded { .. } => "override_added",
            Self::OverrideRemoved { .. } => "override_removed",
            Self::OverrideChanged { .. } => "override_changed",
        }
    }
}

/// Approved rename lineage: `(from, to)` pairs the cutover/data lead signed
/// off. Only listed pairs classify as renames; everything else is add+delete.
/// Slice parameters (not `&Vec`) so call sites pass `&lineage` or `&[]` and
/// `clippy::ptr_arg` stays quiet under `-D warnings`.
pub type RenameLineage = Vec<(String, String)>;

/// Diff one scope: frozen baseline vs a later (staged or live) registry.
///
/// Name sets diff first (add/delete/rename-via-lineage), then per-name
/// definition and default comparison, then explicit-override comparison.
/// Deterministic order: names ascending, overrides by `(name, resource_id)`.
#[must_use]
pub fn diff_scopes(
    baseline: &ScopedRegistry,
    current: &ScopedRegistry,
    lineage: &[(String, String)],
) -> Vec<RegistryDrift> {
    debug_assert_eq!(
        baseline.scope, current.scope,
        "diff_scopes compares one scope at a time"
    );
    let mut drifts = Vec::new();

    let baseline_names = baseline.definition_names();
    let current_names = current.definition_names();
    let mut added: BTreeSet<String> = current_names.difference(&baseline_names).cloned().collect();
    let mut deleted: BTreeSet<String> =
        baseline_names.difference(&current_names).cloned().collect();

    // Approved lineage first: each pair consumes one add and one delete.
    // Unlisted pairs stay add+delete; ambiguous lineage (one `from` mapping
    // to several live names, or vice versa) is reported by the reconciler as
    // frozen, not guessed here.
    let mut renames: Vec<(String, String)> = Vec::new();
    for (from, to) in lineage.iter() {
        if deleted.remove(from) && added.remove(to) {
            renames.push((from.clone(), to.clone()));
        }
    }
    renames.sort();
    for (from, to) in renames {
        drifts.push(RegistryDrift::Renamed { from, to });
    }
    for name in added {
        drifts.push(RegistryDrift::Added { name });
    }
    for name in deleted {
        drifts.push(RegistryDrift::Deleted { name });
    }

    // Shared names: wire-shape comparison, then the inherited default.
    let mut shared: Vec<String> = baseline_names
        .intersection(&current_names)
        .cloned()
        .collect();
    shared.sort();
    for name in shared {
        let before = baseline.definition_by_name(&name).expect("shared name");
        let after = current.definition_by_name(&name).expect("shared name");
        if let Some(field) = first_definition_field_diff(before, after) {
            // The default bitfield is access-critical, so it gets its own
            // class with both values; every other first-difference shares the
            // generic modified class.
            if field == "default_member_permissions" {
                drifts.push(RegistryDrift::DefaultChanged {
                    name: name.clone(),
                    baseline: before.default_member_permissions.clone(),
                    current: after.default_member_permissions.clone(),
                });
            } else {
                drifts.push(RegistryDrift::DefinitionModified {
                    name: name.clone(),
                    field,
                });
            }
        }
        drifts.extend(diff_overrides(baseline, current, &name));
    }
    drifts
}

/// First differing wire field between two same-named definitions, in the
/// order the registry publishes them. `None` means wire-identical.
fn first_definition_field_diff(
    before: &CommandDefinition,
    after: &CommandDefinition,
) -> Option<String> {
    if before.description != after.description {
        return Some("description".to_owned());
    }
    if before.default_member_permissions != after.default_member_permissions {
        return Some("default_member_permissions".to_owned());
    }
    if before.dm_permission != after.dm_permission {
        return Some("dm_permission".to_owned());
    }
    // Options compare by serialized wire shape so option renames, reorders,
    // bound changes and choice edits all surface with one field label.
    let before_options = serde_json::to_value(&before.options).expect("options serialize");
    let after_options = serde_json::to_value(&after.options).expect("options serialize");
    if before_options != after_options {
        return Some("options".to_owned());
    }
    // Both sides are built from `CommandDefinition`, so any remaining
    // difference is unexpected input shape rather than a named field.
    let before_wire = serde_json::to_value(before).expect("definition serializes");
    let after_wire = serde_json::to_value(after).expect("definition serializes");
    if before_wire != after_wire {
        return Some("wire".to_owned());
    }
    None
}

type OverrideKey = (String, String);

fn override_key(row: &PermissionOverride) -> OverrideKey {
    (row.command_name.clone(), row.resource_id.clone())
}

/// Diff explicit override rows for one shared command name.
fn diff_overrides(
    baseline: &ScopedRegistry,
    current: &ScopedRegistry,
    name: &str,
) -> Vec<RegistryDrift> {
    let before: BTreeMap<OverrideKey, &PermissionOverride> = baseline
        .explicit_overrides(name)
        .into_iter()
        .map(|row| (override_key(row), row))
        .collect();
    let after: BTreeMap<OverrideKey, &PermissionOverride> = current
        .explicit_overrides(name)
        .into_iter()
        .map(|row| (override_key(row), row))
        .collect();
    let mut drifts = Vec::new();
    for (key, row) in &after {
        match before.get(key) {
            None => drifts.push(RegistryDrift::OverrideAdded {
                name: name.to_owned(),
                resource_id: key.1.clone(),
            }),
            Some(previous) if previous.allow != row.allow || previous.target != row.target => {
                drifts.push(RegistryDrift::OverrideChanged {
                    name: name.to_owned(),
                    resource_id: key.1.clone(),
                    baseline_allow: previous.allow,
                    current_allow: row.allow,
                });
            }
            Some(_) => {}
        }
    }
    for (key, row) in &before {
        if !after.contains_key(key) {
            drifts.push(RegistryDrift::OverrideRemoved {
                name: name.to_owned(),
                resource_id: key.1.clone(),
                revoked_allow: row.allow,
            });
        }
    }
    drifts
}

// --- permission-restore access ----------------------------------------------

/// Upfront access mapping for permission restoration. Permission writes
/// require an existing authorized OAuth2 Bearer token carrying
/// `applications.commands.permissions.update`; the authorizing user must have
/// Manage Guild and Manage Roles, permission to run the edited command, and
/// permission to manage the affected resources. The bot token used for
/// command definitions is insufficient for that write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRestoreAccess {
    /// Bearer token with `applications.commands.permissions.update` is
    /// available through the authorized permission executor.
    pub bearer_scope_update: bool,
    pub user_manage_guild: bool,
    pub user_manage_roles: bool,
    pub user_can_run_command: bool,
    pub user_can_manage_resources: bool,
    /// The rehearsal was attempted with the bot token. Fails closed
    /// immediately — never a reason to substitute another credential.
    pub attempted_with_bot_token: bool,
}

impl PermissionRestoreAccess {
    /// Fully provisioned mapping (the rehearsal-harness happy path).
    #[must_use]
    pub fn provisioned() -> Self {
        Self {
            bearer_scope_update: true,
            user_manage_guild: true,
            user_manage_roles: true,
            user_can_run_command: true,
            user_can_manage_resources: true,
            attempted_with_bot_token: false,
        }
    }
}

/// Named access gap: missing scope/user authority/tooling is NO-GO and goes
/// to manager/CISO provisioning review, never to credential substitution.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AccessGap {
    #[error("permission restore attempted with the bot token: obtain the authorized OAuth2 Bearer route instead (missing access is NO-GO, not permission to substitute credentials)")]
    BotTokenInsufficient,
    #[error("missing bearer scope: applications.commands.permissions.update")]
    MissingBearerScope,
    #[error("authorizing user lacks Manage Guild")]
    MissingManageGuild,
    #[error("authorizing user lacks Manage Roles")]
    MissingManageRoles,
    #[error("authorizing user may not run command `{0}`")]
    CannotRunCommand(String),
    #[error("authorizing user may not manage affected resources for command `{0}`")]
    CannotManageResources(String),
    #[error("no access mapping for command `{0}` (unmapped command id)")]
    UnmappedCommand(String),
}

/// Verify the access mapping for one command before any rename/removal/
/// overwrite. Fail-closed: the first missing element wins, and a bot-token
/// attempt short-circuits every other check.
pub fn verify_permission_restore_access(
    command_name: &str,
    access: &PermissionRestoreAccess,
) -> Result<(), AccessGap> {
    if access.attempted_with_bot_token {
        return Err(AccessGap::BotTokenInsufficient);
    }
    if !access.bearer_scope_update {
        return Err(AccessGap::MissingBearerScope);
    }
    if !access.user_manage_guild {
        return Err(AccessGap::MissingManageGuild);
    }
    if !access.user_manage_roles {
        return Err(AccessGap::MissingManageRoles);
    }
    if !access.user_can_run_command {
        return Err(AccessGap::CannotRunCommand(command_name.to_owned()));
    }
    if !access.user_can_manage_resources {
        return Err(AccessGap::CannotManageResources(command_name.to_owned()));
    }
    Ok(())
}

// --- watch-window drift + reconciliation ------------------------------------

/// One watch-window registry/permission change to simulate against the frozen
/// baseline (every class `cutover.md` requires rehearsing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftEvent {
    /// An allow entry was removed (role/user/channel): a revocation.
    RevokedAllow {
        command: String,
        resource_id: String,
    },
    /// `default_member_permissions` moved on a live command.
    ChangedDefault {
        command: String,
        new_default: Option<String>,
    },
    /// A legitimate admin/tool addition during the window.
    AddedCommand { definition: CommandDefinition },
    /// A legitimate admin/tool deletion during the window.
    DeletedCommand { name: String },
    /// A legitimate admin/tool rename during the window (approved lineage).
    RenamedCommand { from: String, to: String },
    /// An explicit override flipped or added during the window.
    ChangedOverride { row: PermissionOverride },
}

/// Apply drift events to a clone of the baseline, producing the simulated
/// live registry plus the approved lineage the events imply.
#[must_use]
pub fn simulate_watch_window(
    baseline: &ScopedRegistry,
    events: &[DriftEvent],
) -> (ScopedRegistry, RenameLineage) {
    let mut live = baseline.clone();
    let mut lineage: RenameLineage = Vec::new();
    for event in events {
        match event {
            DriftEvent::RevokedAllow {
                command,
                resource_id,
            } => {
                live.overrides.retain(|row| {
                    !(row.command_name == *command
                        && row.resource_id == *resource_id
                        && row.allow
                        && !row.synced)
                });
            }
            DriftEvent::ChangedDefault {
                command,
                new_default,
            } => {
                if let Some(def) = live.definitions.iter_mut().find(|def| def.name == *command) {
                    def.default_member_permissions.clone_from(new_default);
                }
            }
            DriftEvent::AddedCommand { definition } => {
                if live.definition_by_name(&definition.name).is_none() {
                    live.definitions.push(definition.clone());
                }
            }
            DriftEvent::DeletedCommand { name } => {
                live.definitions.retain(|def| def.name != *name);
                live.overrides.retain(|row| row.command_name != *name);
            }
            DriftEvent::RenamedCommand { from, to } => {
                if let Some(def) = live.definitions.iter_mut().find(|def| def.name == *from) {
                    def.name = to.clone();
                }
                for row in live.overrides.iter_mut() {
                    if row.command_name == *from {
                        row.command_name = to.clone();
                    }
                }
                lineage.push((from.clone(), to.clone()));
            }
            DriftEvent::ChangedOverride { row } => {
                live.overrides.retain(|existing| {
                    !(existing.command_name == row.command_name
                        && existing.resource_id == row.resource_id)
                });
                live.overrides.push(row.clone());
            }
        }
    }
    (live, lineage)
}

/// Approved reconciled target for one scope: the definitions and complete
/// per-command override arrays to PUT, plus what was preserved/carried and
/// why. `frozen_reasons` non-empty means NO-GO — keep commands frozen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileReport {
    pub scope: CommandScope,
    pub target_definitions: Vec<CommandDefinition>,
    pub target_overrides: Vec<PermissionOverride>,
    /// Legitimate window additions preserved (not discarded as drift).
    pub preserved_additions: Vec<String>,
    /// Legitimate window deletions preserved (not recreated from baseline),
    /// plus staged-approved removals of commands the window kept (the
    /// full-replacement PUT applies those deletions, so the report accounts
    /// for them instead of staying silent).
    pub preserved_deletions: Vec<String>,
    /// Approved renames carried with lineage.
    pub applied_renames: Vec<(String, String)>,
    /// Removed allows carried (never reintroduced from the baseline).
    pub carried_revocations: Vec<String>,
    /// Tightened defaults carried (never loosened back to baseline).
    pub carried_default_tightenings: Vec<String>,
    /// Per scope key: `(scope_key, command_name, live_id->restored_id)`.
    /// The third element starts as the live (current) server ID and is
    /// rewritten to `live->restored` once the definition PUT assigns fresh IDs.
    pub id_map: Vec<(String, String, String)>,
    /// Non-empty: unsupported/ambiguous drift — keep frozen, escalate.
    pub frozen_reasons: Vec<String>,
}

impl ReconcileReport {
    /// GO only with zero unexplained mismatches and no frozen reasons.
    #[must_use]
    pub fn is_go(&self) -> bool {
        self.frozen_reasons.is_empty()
    }
}

/// Reconcile the frozen baseline against the final live snapshot for one
/// scope, given the staged (reviewed Next) registry and approved lineage.
///
/// Rules (cutover recovery order):
/// - Start from the staged registry, not an automatic reset to the baseline.
/// - Preserve legitimate window additions/deletions/renames; an unsupported
///   definition or ambiguous map freezes instead of discarding or recreating.
/// - Carry current restrictions and revocations into the target; never
///   broaden current access (removed allows stay removed, tightened defaults
///   stay tight).
/// - `id_map` entries use `(scope_key, live_id, "<restored>")` placeholders:
///   the mock transport fills restored IDs at apply time.
#[must_use]
pub fn reconcile_watch_window(
    baseline: &ScopedRegistry,
    staged: &ScopedRegistry,
    live: &ScopedRegistry,
    lineage: &[(String, String)],
    live_ids: &BTreeMap<String, String>,
) -> ReconcileReport {
    debug_assert_eq!(baseline.scope, staged.scope);
    debug_assert_eq!(baseline.scope, live.scope);
    let scope = baseline.scope.clone();
    let mut report = ReconcileReport {
        scope: scope.clone(),
        target_definitions: staged.definitions.clone(),
        target_overrides: staged.overrides.clone(),
        preserved_additions: Vec::new(),
        preserved_deletions: Vec::new(),
        applied_renames: Vec::new(),
        carried_revocations: Vec::new(),
        carried_default_tightenings: Vec::new(),
        id_map: Vec::new(),
        frozen_reasons: Vec::new(),
    };

    let baseline_names = baseline.definition_names();
    let staged_names = staged.definition_names();
    let live_names = live.definition_names();

    // Live additions missing from staged: preserve legitimate ones, freeze on
    // unsupported definitions (empty description/shape signals a capture the
    // reconciler cannot approve blindly).
    let mut additions: Vec<String> = live_names.difference(&baseline_names).cloned().collect();
    additions.sort();
    for name in additions {
        let live_def = live.definition_by_name(&name).expect("live addition");
        if live_def.description.trim().is_empty() {
            report.frozen_reasons.push(format!(
                "unsupported live addition `{name}` has no approved definition; keep frozen"
            ));
            continue;
        }
        if !staged_names.contains(&name) {
            report.target_definitions.push(live_def.clone());
        }
        // Live overrides on the preserved addition travel with it.
        for row in live.explicit_overrides(&name) {
            if !report
                .target_overrides
                .iter()
                .any(|existing| existing == row)
            {
                report.target_overrides.push(row.clone());
            }
        }
        report.preserved_additions.push(name);
    }

    // Live deletions of baseline names: preserve when the staged registry
    // also drops them (deliberate removal); if staged still wants the command
    // but the window deleted it, that is unexplained drift — freeze.
    let mut deletions: Vec<String> = baseline_names.difference(&live_names).cloned().collect();
    deletions.sort();
    for name in deletions {
        if staged_names.contains(&name) {
            report.frozen_reasons.push(format!(
                "command `{name}` deleted during the watch window but required by staged registry; keep frozen"
            ));
        } else {
            report.preserved_deletions.push(name.clone());
            report.target_definitions.retain(|def| def.name != name);
            report
                .target_overrides
                .retain(|row| row.command_name != name);
        }
    }

    // Approved renames: staged must carry the new name (or the preserved
    // addition above already added it); overrides follow the lineage, and the
    // old name must be gone from the target.
    let mut renames = lineage.to_vec();
    renames.sort();
    for (from, to) in renames {
        let live_has_both = live_names.contains(&from) || live_names.contains(&to);
        if !live_has_both {
            report.frozen_reasons.push(format!(
                "approved rename `{from}` -> `{to}` matches no live command; keep frozen"
            ));
            continue;
        }
        report.applied_renames.push((from.clone(), to.clone()));
        report.target_definitions.retain(|def| def.name != from);
        report
            .target_overrides
            .retain(|row| row.command_name != from);
        if let Some(live_def) = live.definition_by_name(&to) {
            if !report.target_definitions.iter().any(|def| def.name == *to) {
                report.target_definitions.push(live_def.clone());
            }
        }
        for row in live.explicit_overrides(&to) {
            if !report
                .target_overrides
                .iter()
                .any(|existing| existing == row)
            {
                report.target_overrides.push(row.clone());
            }
        }
    }

    // Staged deliberately drops a baseline command the window kept: the
    // full-replacement PUT applies staged's approved removal, so record it as
    // a preserved deletion rather than leaving the report silent about a
    // deletion the target applies. Rename sources are skipped here — they are
    // accounted for in `applied_renames`, not as deletions.
    let mut staged_drops: Vec<String> = baseline_names
        .intersection(&live_names)
        .filter(|name| {
            !staged_names.contains(*name)
                && !report.applied_renames.iter().any(|(from, _)| from == *name)
        })
        .cloned()
        .collect();
    staged_drops.sort();
    for name in staged_drops {
        if !report.preserved_deletions.contains(&name) {
            report.preserved_deletions.push(name);
        }
    }
    report.preserved_deletions.sort();

    // Ambiguous rename shape: several baseline names vanished while unrelated
    // live names appeared, with no lineage explaining them. Guessing a map
    // would risk restoring the wrong permissions onto the wrong command.
    let unexplained_adds: Vec<String> = live_names
        .difference(&baseline_names)
        .filter(|name| !report.preserved_additions.contains(name))
        .cloned()
        .collect();
    let unexplained_dels: Vec<String> = baseline_names
        .difference(&live_names)
        .filter(|name| {
            !report.preserved_deletions.contains(name)
                && !report.applied_renames.iter().any(|(from, _)| from == *name)
        })
        .cloned()
        .collect();
    if !unexplained_adds.is_empty() && !unexplained_dels.is_empty() {
        report.frozen_reasons.push(format!(
            "ambiguous rename map (deleted: {}; added: {}); keep frozen",
            unexplained_dels.join(", "),
            unexplained_adds.join(", ")
        ));
    }

    // Revocations and tightenings: compare live against the staged target and
    // carry the stricter side. A removed allow stays removed; a tightened
    // default (live `Some` narrower than staged, or staged open while live
    // gates) stays tight. Loosenings in live are NOT carried — they need a
    // named disposition, so they freeze.
    for def in live.definitions.clone() {
        let Some(target) = report
            .target_definitions
            .iter_mut()
            .find(|target| target.name == def.name)
        else {
            continue;
        };
        if target.default_member_permissions != def.default_member_permissions
            && is_tightening(
                &target.default_member_permissions,
                &def.default_member_permissions,
            )
        {
            target.default_member_permissions = def.default_member_permissions.clone();
            report.carried_default_tightenings.push(target.name.clone());
        } else if target.default_member_permissions != def.default_member_permissions {
            report.frozen_reasons.push(format!(
                "live default on `{}` loosens the staged default; needs a named disposition",
                def.name
            ));
        }
    }
    let live_override_keys: BTreeSet<OverrideKey> = live
        .overrides
        .iter()
        .filter(|row| !row.synced)
        .map(override_key)
        .collect();
    for row in report.target_overrides.clone() {
        let key = override_key(&row);
        if row.allow && !live_override_keys.contains(&key) {
            // Staged allows what live revoked: carry the revocation.
            report
                .target_overrides
                .retain(|existing| override_key(existing) != key);
            report
                .carried_revocations
                .push(format!("{}:{}", row.command_name, row.resource_id));
        }
    }
    // Live explicit rows missing from the target (added or flipped during the
    // window) join the target; flips toward deny are revocations to carry.
    for row in live.overrides.iter().filter(|row| !row.synced) {
        if !report.target_overrides.iter().any(|existing| {
            existing.command_name == row.command_name && existing.resource_id == row.resource_id
        }) {
            report.target_overrides.push(row.clone());
        }
    }

    // ID map: every target command resolves old (baseline-era) and current
    // (live) server IDs; restored IDs fill in at apply time.
    let mut target_names: Vec<String> = report
        .target_definitions
        .iter()
        .map(|def| def.name.clone())
        .collect();
    target_names.sort();
    for name in target_names {
        let current = live_ids.get(&name).cloned().unwrap_or_default();
        report.id_map.push((scope.key(), name, current));
    }

    report.carried_revocations.sort();
    report.carried_revocations.dedup();
    report.carried_default_tightenings.sort();
    report.carried_default_tightenings.dedup();
    report
}

/// `true` when moving from `staged` to `live` tightens access: a gate appears
/// where none was, or the bitfield changes (any bitfield change is treated
/// as needing the tightening path — the reconciler carries the live value and
/// the read-back verifies it, rather than assuming subset relations between
/// opaque Discord bitfields).
fn is_tightening(staged: &Option<String>, live: &Option<String>) -> bool {
    match (staged, live) {
        (None, Some(_)) => true,
        (Some(_), None) => false,
        (Some(a), Some(b)) => a != b,
        (None, None) => false,
    }
}

// --- mock Discord transport -------------------------------------------------

/// Recorded mock transport operation (dry-run receipt, no network).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockOp {
    Snapshot { scope: String },
    PutDefinitions { scope: String, count: usize },
    PutPermissions { command: String, count: usize },
    ReadBack { scope: String },
}

/// Mock Discord transport for the rehearsal: in-memory definitions with
/// server-assigned IDs plus per-command permission arrays.
///
/// - Recreating a name assigns a **fresh** ID (Discord never reuses IDs), so
///   the old/current/restored ID map is genuinely exercised.
/// - Permission PUT requires [`MockDiscord::grant_bearer_access`]; without it
///   the PUT fails with the same named gap the real route produces.
/// - The batch permissions endpoint stays absent (it is disabled server-side
///   and is not a fallback).
#[derive(Debug, Clone, Default)]
pub struct MockDiscord {
    ids: BTreeMap<(String, String), String>,
    definitions: BTreeMap<String, Vec<CommandDefinition>>,
    permissions: BTreeMap<(String, String), Vec<PermissionOverride>>,
    bearer_granted: bool,
    next_id: u64,
    pub log: Vec<MockOp>,
}

impl MockDiscord {
    /// Seed the transport from the frozen baseline, assigning server IDs.
    #[must_use]
    pub fn from_baseline(baseline: &[ScopedRegistry]) -> Self {
        let mut mock = Self {
            next_id: 1001,
            ..Self::default()
        };
        for scope in baseline {
            let key = scope.scope.key();
            mock.definitions
                .insert(key.clone(), scope.definitions.clone());
            for def in &scope.definitions {
                // Hoisted: `issue_id` borrows `mock` mutably, which conflicts
                // with the `ids` borrow inside `insert`.
                let fresh = mock.issue_id();
                mock.ids.insert((key.clone(), def.name.clone()), fresh);
            }
            for row in scope.overrides.iter().filter(|row| !row.synced) {
                mock.permissions
                    .entry((key.clone(), row.command_name.clone()))
                    .or_default()
                    .push(row.clone());
            }
        }
        mock
    }

    fn issue_id(&mut self) -> String {
        let id = self.next_id;
        self.next_id += 1;
        id.to_string()
    }

    /// Grant the OAuth2 Bearer permission-restore route for this rehearsal.
    pub fn grant_bearer_access(&mut self) {
        self.bearer_granted = true;
    }

    /// IDs keyed by bare command name (single-scope reconciler input).
    #[must_use]
    pub fn live_ids_for_scope(&self, scope: &CommandScope) -> BTreeMap<String, String> {
        let key = scope.key();
        self.ids
            .iter()
            .filter(|((scope_key, _), _)| *scope_key == key)
            .map(|((_, name), id)| (name.clone(), id.clone()))
            .collect()
    }

    /// Capture the current live registry for one scope (definitions plus
    /// separate per-command permission arrays).
    #[must_use]
    pub fn snapshot(&mut self, scope: &CommandScope) -> ScopedRegistry {
        self.log.push(MockOp::Snapshot { scope: scope.key() });
        let key = scope.key();
        ScopedRegistry {
            scope: scope.clone(),
            definitions: self.definitions.get(&key).cloned().unwrap_or_default(),
            overrides: self
                .permissions
                .iter()
                .filter(|((scope_key, _), _)| *scope_key == key)
                .flat_map(|(_, rows)| rows.clone())
                .collect(),
        }
    }

    /// Full-replacement definition PUT for one scope. Omitted commands are
    /// deleted with their permissions (mirroring the real route); recreated
    /// names receive fresh IDs.
    pub fn put_definitions(&mut self, scope: &CommandScope, target: &[CommandDefinition]) {
        let key = scope.key();
        // Collect the removals first so the ID map below cannot observe a
        // half-applied transport state.
        let removed: Vec<String> = self
            .definitions
            .get(&key)
            .map(|defs| {
                defs.iter()
                    .map(|def| def.name.clone())
                    .filter(|name| !target.iter().any(|want| want.name == *name))
                    .collect()
            })
            .unwrap_or_default();
        for name in removed {
            self.ids.remove(&(key.clone(), name.clone()));
            self.permissions.remove(&(key.clone(), name));
        }
        for def in target {
            if !self.ids.contains_key(&(key.clone(), def.name.clone())) {
                let fresh = self.issue_id();
                self.ids.insert((key.clone(), def.name.clone()), fresh);
            }
        }
        self.definitions.insert(key.clone(), target.to_vec());
        self.log.push(MockOp::PutDefinitions {
            scope: key,
            count: target.len(),
        });
    }

    /// Per-command permission PUT (complete array, replacing). Refused without
    /// bearer access — the bot token cannot take this path.
    pub fn put_permissions(
        &mut self,
        scope: &CommandScope,
        command_name: &str,
        rows: &[PermissionOverride],
    ) -> Result<(), AccessGap> {
        if !self.bearer_granted {
            return Err(AccessGap::MissingBearerScope);
        }
        self.permissions
            .insert((scope.key(), command_name.to_owned()), rows.to_vec());
        self.log.push(MockOp::PutPermissions {
            command: command_name.to_owned(),
            count: rows.len(),
        });
        Ok(())
    }

    /// Read back one scope and compare definitions plus every explicit
    /// override array against the approved reconciled target. Returns the
    /// unexplained mismatches (empty means verified).
    #[must_use]
    pub fn read_back(&mut self, target: &ScopedRegistry) -> Vec<String> {
        self.log.push(MockOp::ReadBack {
            scope: target.scope.key(),
        });
        // Inline capture (not `snapshot`, which logs separately): read-back
        // is one logged operation comparing live state against the target.
        let key = target.scope.key();
        let live = ScopedRegistry {
            scope: target.scope.clone(),
            definitions: self.definitions.get(&key).cloned().unwrap_or_default(),
            overrides: self
                .permissions
                .iter()
                .filter(|((scope_key, _), _)| *scope_key == key)
                .flat_map(|(_, rows)| rows.clone())
                .collect(),
        };
        let mut mismatches = Vec::new();
        let live_names = live.definition_names();
        let target_names = target.definition_names();
        for name in target_names.difference(&live_names) {
            mismatches.push(format!(
                "target command `{name}` missing from live registry"
            ));
        }
        for name in live_names.difference(&target_names) {
            mismatches.push(format!("live command `{name}` not in reconciled target"));
        }
        for name in target_names.intersection(&live_names) {
            let want = target.definition_by_name(name).expect("target name");
            let got = live.definition_by_name(name).expect("live name");
            if first_definition_field_diff(want, got).is_some() {
                mismatches.push(format!("live definition `{name}` differs from target"));
            }
            let mut want_rows: Vec<&PermissionOverride> = target.explicit_overrides(name);
            let mut got_rows: Vec<&PermissionOverride> = live.explicit_overrides(name);
            want_rows.sort_by_key(override_key);
            got_rows.sort_by_key(override_key);
            if want_rows != got_rows {
                mismatches.push(format!("live overrides for `{name}` differ from target"));
            }
        }
        mismatches
    }
}

// --- rehearsal ----------------------------------------------------------------

/// One-scope dry-run rehearsal: diff, access check, drift, reconcile, apply,
/// read-back. Every step is in-memory; the only writes are mock-transport
/// calls recorded in [`MockDiscord::log`].
#[derive(Debug, Clone)]
pub struct RehearsalInput {
    pub baseline: ScopedRegistry,
    pub staged: ScopedRegistry,
    pub access: PermissionRestoreAccess,
    pub drift: Vec<DriftEvent>,
}

/// Full rehearsal report for one scope.
#[derive(Debug, Clone)]
pub struct RehearsalReport {
    pub scope: CommandScope,
    pub staged_drifts: Vec<RegistryDrift>,
    pub access_gaps: Vec<AccessGap>,
    pub live_drifts: Vec<RegistryDrift>,
    pub reconcile: ReconcileReport,
    pub readback_mismatches: Vec<String>,
}

impl RehearsalReport {
    /// GO only when access verified, reconciliation unfrozen, and read-back
    /// matches the approved target with zero unexplained mismatches.
    #[must_use]
    pub fn is_go(&self) -> bool {
        self.access_gaps.is_empty() && self.reconcile.is_go() && self.readback_mismatches.is_empty()
    }
}

/// Run the dry-run rehearsal for one scope through the mock transport.
///
/// Order follows the cutover recovery procedure: freeze writers (vacuous on
/// the mock), capture the live snapshot, verify permission-restore access
/// **before** any reconciled PUT, reconcile the drift into an approved
/// target, apply definitions first, reapply per-command overrides, then read
/// back against the target. A failed access check stops before any reconciled
/// PUT (the live-world setup PUT is scaffolding, not a rehearsal write).
#[must_use]
pub fn rehearse_scope(input: &RehearsalInput, mock: &mut MockDiscord) -> RehearsalReport {
    let scope = input.baseline.scope.clone();
    let staged_drifts = diff_scopes(&input.baseline, &input.staged, &[]);

    // Access first: one failing command fails the rehearsal closed, before
    // any definition PUT. Every staged command with explicit overrides (or a
    // changed default) needs the permission route.
    let mut access_gaps = Vec::new();
    let mut seen = BTreeSet::new();
    for row in input
        .staged
        .overrides
        .iter()
        .filter(|row| !row.synced)
        .chain(input.baseline.overrides.iter().filter(|row| !row.synced))
    {
        if seen.insert(row.command_name.clone()) {
            if let Err(gap) = verify_permission_restore_access(&row.command_name, &input.access) {
                access_gaps.push(gap);
            }
        }
    }
    // A changed default also needs the permission route (defaults restore
    // through the separately authorized path, never the definition PUT).
    for drift in &staged_drifts {
        if let RegistryDrift::DefaultChanged { name, .. } = drift {
            if seen.insert(name.clone()) {
                if let Err(gap) = verify_permission_restore_access(name, &input.access) {
                    access_gaps.push(gap);
                }
            }
        }
    }

    // Simulate the watch window on the mock's live state, then recapture.
    let (simulated_live, lineage) = simulate_watch_window(&input.baseline, &input.drift);
    mock.put_definitions(&scope, &simulated_live.definitions);
    if input.access.bearer_scope_update && !input.access.attempted_with_bot_token {
        mock.grant_bearer_access();
    }
    // Mirror the simulated live overrides into the mock, one complete
    // per-command array at a time (a PUT replaces the array wholesale, so a
    // command whose rows were all revoked gets an empty array). Commands come
    // from both snapshots so revocations of baseline-only rows are mirrored
    // too. The mirror applies the same fail-closed rule: without bearer
    // access the live drift itself cannot land, which the report surfaces as
    // an access gap rather than silently dropped drift.
    let mut drifted_commands: BTreeSet<String> = BTreeSet::new();
    for row in input
        .baseline
        .overrides
        .iter()
        .chain(simulated_live.overrides.iter())
        .filter(|row| !row.synced)
    {
        drifted_commands.insert(row.command_name.clone());
    }
    for command in drifted_commands {
        let rows: Vec<PermissionOverride> = simulated_live
            .explicit_overrides(&command)
            .into_iter()
            .cloned()
            .collect();
        let _ = mock.put_permissions(&scope, &command, &rows);
    }
    let live = mock.snapshot(&scope);
    let live_drifts = diff_scopes(&input.baseline, &live, &lineage);

    let mut reconcile = reconcile_watch_window(
        &input.baseline,
        &input.staged,
        &live,
        &lineage,
        &mock.live_ids_for_scope(&scope),
    );

    if access_gaps.is_empty() && reconcile.is_go() {
        mock.put_definitions(&scope, &reconcile.target_definitions);
        // Fill restored IDs into the map now that the PUT assigned them.
        let restored = mock.live_ids_for_scope(&scope);
        for (_, name, current) in reconcile.id_map.iter_mut() {
            if let Some(new_id) = restored.get(name) {
                *current = format!("{current}->{new_id}");
            }
        }
        // Reapply each reconciled explicit array to its mapped command.
        let mut commands: Vec<String> = reconcile
            .target_overrides
            .iter()
            .map(|row| row.command_name.clone())
            .collect();
        commands.sort();
        commands.dedup();
        for command in commands {
            let rows: Vec<PermissionOverride> = reconcile
                .target_overrides
                .iter()
                .filter(|row| row.command_name == command)
                .cloned()
                .collect();
            if let Err(gap) = mock.put_permissions(&scope, &command, &rows) {
                access_gaps.push(gap);
            }
        }
    }

    let target = ScopedRegistry {
        scope: scope.clone(),
        definitions: reconcile.target_definitions.clone(),
        overrides: reconcile.target_overrides.clone(),
    };
    let readback_mismatches = if access_gaps.is_empty() && reconcile.is_go() {
        mock.read_back(&target)
    } else {
        Vec::new()
    };

    RehearsalReport {
        scope,
        staged_drifts,
        access_gaps,
        live_drifts,
        reconcile,
        readback_mismatches,
    }
}

#[cfg(test)]
mod tests {
    use super::super::commands::{CommandOption, CommandOptionType};
    use super::*;

    fn def(name: &str) -> CommandDefinition {
        CommandDefinition::new(name, "Test command")
    }

    fn gated(name: &str, bits: u64) -> CommandDefinition {
        CommandDefinition::new(name, "Gated command").permissions(bits)
    }

    fn scoped(defs: Vec<CommandDefinition>) -> ScopedRegistry {
        ScopedRegistry {
            scope: CommandScope::Guild { guild_id: 7 },
            definitions: defs,
            overrides: Vec::new(),
        }
    }

    #[test]
    fn identical_registries_have_no_drift() {
        let baseline = scoped(vec![def("rank"), gated("ban", 4)]);
        assert!(diff_scopes(&baseline, &baseline, &[]).is_empty());
    }

    #[test]
    fn delete_plus_add_without_lineage_is_not_a_rename() {
        let baseline = scoped(vec![def("old-cmd")]);
        let current = scoped(vec![def("new-cmd")]);
        let drifts = diff_scopes(&baseline, &current, &[]);
        assert!(drifts.contains(&RegistryDrift::Deleted {
            name: "old-cmd".to_owned()
        }));
        assert!(drifts.contains(&RegistryDrift::Added {
            name: "new-cmd".to_owned()
        }));
        assert!(!drifts.iter().any(|drift| drift.class() == "renamed"));
    }

    #[test]
    fn approved_lineage_classifies_the_rename() {
        let baseline = scoped(vec![def("old-cmd")]);
        let current = scoped(vec![def("new-cmd")]);
        let lineage = vec![("old-cmd".to_owned(), "new-cmd".to_owned())];
        assert_eq!(
            diff_scopes(&baseline, &current, &lineage),
            vec![RegistryDrift::Renamed {
                from: "old-cmd".to_owned(),
                to: "new-cmd".to_owned(),
            }]
        );
    }

    #[test]
    fn default_change_gets_its_own_class() {
        let baseline = scoped(vec![gated("ban", 4)]);
        let current = scoped(vec![gated("ban", 32)]);
        assert_eq!(
            diff_scopes(&baseline, &current, &[]),
            vec![RegistryDrift::DefaultChanged {
                name: "ban".to_owned(),
                baseline: Some("4".to_owned()),
                current: Some("32".to_owned()),
            }]
        );
    }

    #[test]
    fn option_shape_change_is_a_definition_modification() {
        let baseline = scoped(vec![def("rank")]);
        let mut current = scoped(vec![CommandDefinition::new("rank", "Test command")
            .options(vec![CommandOption::new(
                "member",
                "Member",
                CommandOptionType::User,
            )])]);
        current.definitions[0].description = "Test command".to_owned();
        let drifts = diff_scopes(&baseline, &current, &[]);
        assert_eq!(
            drifts,
            vec![RegistryDrift::DefinitionModified {
                name: "rank".to_owned(),
                field: "options".to_owned(),
            }]
        );
    }

    #[test]
    fn revoked_allow_reports_as_override_removed_with_revocation_flag() {
        let mut baseline = scoped(vec![gated("ban", 4)]);
        baseline.overrides.push(PermissionOverride::explicit(
            "ban",
            "99",
            OverrideTarget::Role,
            true,
        ));
        let current = scoped(vec![gated("ban", 4)]);
        assert_eq!(
            diff_scopes(&baseline, &current, &[]),
            vec![RegistryDrift::OverrideRemoved {
                name: "ban".to_owned(),
                resource_id: "99".to_owned(),
                revoked_allow: true,
            }]
        );
    }

    #[test]
    fn bot_token_attempt_fails_closed_first() {
        let access = PermissionRestoreAccess {
            attempted_with_bot_token: true,
            ..PermissionRestoreAccess::provisioned()
        };
        assert_eq!(
            verify_permission_restore_access("ban", &access),
            Err(AccessGap::BotTokenInsufficient)
        );
    }

    #[test]
    fn each_missing_element_has_a_named_gap() {
        let cases = [
            (
                PermissionRestoreAccess {
                    bearer_scope_update: false,
                    ..PermissionRestoreAccess::provisioned()
                },
                AccessGap::MissingBearerScope,
            ),
            (
                PermissionRestoreAccess {
                    user_manage_guild: false,
                    ..PermissionRestoreAccess::provisioned()
                },
                AccessGap::MissingManageGuild,
            ),
            (
                PermissionRestoreAccess {
                    user_manage_roles: false,
                    ..PermissionRestoreAccess::provisioned()
                },
                AccessGap::MissingManageRoles,
            ),
            (
                PermissionRestoreAccess {
                    user_can_run_command: false,
                    ..PermissionRestoreAccess::provisioned()
                },
                AccessGap::CannotRunCommand("ban".to_owned()),
            ),
            (
                PermissionRestoreAccess {
                    user_can_manage_resources: false,
                    ..PermissionRestoreAccess::provisioned()
                },
                AccessGap::CannotManageResources("ban".to_owned()),
            ),
        ];
        for (access, expected) in cases {
            assert_eq!(
                verify_permission_restore_access("ban", &access),
                Err(expected)
            );
        }
        assert!(
            verify_permission_restore_access("ban", &PermissionRestoreAccess::provisioned())
                .is_ok()
        );
    }

    #[test]
    fn reconciler_carries_revocations_and_never_reintroduces_allows() {
        let mut baseline = scoped(vec![gated("ban", 4)]);
        baseline.overrides.push(PermissionOverride::explicit(
            "ban",
            "99",
            OverrideTarget::Role,
            true,
        ));
        let staged = baseline.clone();
        let (live, lineage) = simulate_watch_window(
            &baseline,
            &[DriftEvent::RevokedAllow {
                command: "ban".to_owned(),
                resource_id: "99".to_owned(),
            }],
        );
        let mock = MockDiscord::from_baseline(&[baseline.clone()]);
        let report = reconcile_watch_window(
            &baseline,
            &staged,
            &live,
            &lineage,
            &mock.live_ids_for_scope(&baseline.scope),
        );
        assert!(report.is_go());
        assert_eq!(report.carried_revocations, vec!["ban:99".to_owned()]);
        assert!(report.target_overrides.is_empty());
    }

    #[test]
    fn ambiguous_rename_map_freezes_instead_of_guessing() {
        let baseline = scoped(vec![def("aaa"), def("bbb")]);
        let staged = baseline.clone();
        // Two deletes plus an unapproved add with no usable definition and no
        // lineage: the add cannot be preserved, so both sides stay unexplained
        // and the map freezes instead of guessing which delete became `ccc`.
        let (live, lineage) = simulate_watch_window(
            &baseline,
            &[
                DriftEvent::DeletedCommand {
                    name: "aaa".to_owned(),
                },
                DriftEvent::DeletedCommand {
                    name: "bbb".to_owned(),
                },
                DriftEvent::AddedCommand {
                    definition: CommandDefinition::new("ccc", ""),
                },
            ],
        );
        let mock = MockDiscord::from_baseline(&[baseline.clone()]);
        let report = reconcile_watch_window(
            &baseline,
            &staged,
            &live,
            &lineage,
            &mock.live_ids_for_scope(&baseline.scope),
        );
        assert!(!report.is_go());
        assert!(report
            .frozen_reasons
            .iter()
            .any(|reason| reason.contains("ambiguous rename map")));
    }

    #[test]
    fn recreated_names_get_fresh_ids_on_the_mock() {
        let baseline = scoped(vec![def("rank")]);
        let mut mock = MockDiscord::from_baseline(&[baseline.clone()]);
        let before = mock.live_ids_for_scope(&baseline.scope);
        mock.put_definitions(&baseline.scope, &[]);
        mock.put_definitions(&baseline.scope, &[def("rank")]);
        let after = mock.live_ids_for_scope(&baseline.scope);
        assert_ne!(before["rank"], after["rank"]);
    }

    #[test]
    fn mock_permission_put_refuses_without_bearer_access() {
        let baseline = scoped(vec![def("rank")]);
        let mut mock = MockDiscord::from_baseline(&[baseline]);
        let err = mock
            .put_permissions(
                &CommandScope::Guild { guild_id: 7 },
                "rank",
                &[PermissionOverride::explicit(
                    "rank",
                    "99",
                    OverrideTarget::Role,
                    false,
                )],
            )
            .expect_err("must refuse without bearer access");
        assert_eq!(err, AccessGap::MissingBearerScope);
    }
}
