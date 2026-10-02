//! Pure V11 import diff preview, built from `docs/voice-rooms.md` §V11 only.
//!
//! The caller supplies the current configuration, a candidate incoming
//! configuration and a trusted guild inventory. This module performs no I/O,
//! no Discord work and no persistence. It never validates beyond unknown
//! channel detection; the parent must revalidate the filtered candidate with
//! `voice_config::validate_configuration` before confirmation.
//!
//! Semantics: incoming entries that reference a channel ID absent from the
//! inventory are reported in `skipped_unknown_channels` and excluded from the
//! change lists. The diff is computed between `current` and the filtered
//! incoming, so `apply_diff(current, diff)` equals the incoming with skipped
//! entries dropped (sorted deterministically). Entry order in the input lists
//! is insignificant; only keyed membership and values are compared.

use std::collections::{BTreeMap, BTreeSet};

use crate::voice_config::{
    ChannelTemplates, CreatorConfiguration, GameAlias, GuildInventory, GuildSettings,
    LoggingConfiguration, PermissionSource, RandomList, VoiceConfiguration,
};

/// One changed creator entry with the field names that differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatorChanged {
    pub channel_id: String,
    pub before: CreatorConfiguration,
    pub after: CreatorConfiguration,
    pub fields: Vec<&'static str>,
}

/// One changed standalone-template entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateChanged {
    pub channel_id: String,
    pub before: ChannelTemplates,
    pub after: ChannelTemplates,
    pub fields: Vec<&'static str>,
}

/// One changed alias entry (`game` is the key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasChanged {
    pub game: String,
    pub before: GameAlias,
    pub after: GameAlias,
}

/// One changed named-list entry (`name` is the key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListChanged {
    pub name: String,
    pub before: RandomList,
    pub after: RandomList,
}

/// Changed logging configuration (singleton section).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggingChanged {
    pub before: LoggingConfiguration,
    pub after: LoggingConfiguration,
    pub fields: Vec<&'static str>,
}

/// Changed guild settings (singleton section).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsChanged {
    pub before: GuildSettings,
    pub after: GuildSettings,
    pub fields: Vec<&'static str>,
}

/// Deterministic diff between `current` and the filtered `incoming`.
///
/// All lists are sorted by their key. Section order for rendering is
/// creators, templates, aliases, lists, logging, settings, then skipped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfigDiff {
    pub creators_added: Vec<CreatorConfiguration>,
    pub creators_removed: Vec<CreatorConfiguration>,
    pub creators_changed: Vec<CreatorChanged>,
    pub templates_added: Vec<ChannelTemplates>,
    pub templates_removed: Vec<ChannelTemplates>,
    pub templates_changed: Vec<TemplateChanged>,
    pub aliases_added: Vec<GameAlias>,
    pub aliases_removed: Vec<GameAlias>,
    pub aliases_changed: Vec<AliasChanged>,
    pub lists_added: Vec<RandomList>,
    pub lists_removed: Vec<RandomList>,
    pub lists_changed: Vec<ListChanged>,
    pub logging_added: Option<LoggingConfiguration>,
    pub logging_removed: Option<LoggingConfiguration>,
    pub logging_changed: Option<LoggingChanged>,
    pub settings_changed: Option<SettingsChanged>,
    /// Sorted unique IDs to report: unknown channel IDs referenced by
    /// `incoming`, plus the keys of entries skipped because they touch an
    /// unknown channel (for example a creator whose permission-source channel
    /// is unknown). Skipped entries are excluded from the change lists.
    pub skipped_unknown_channels: Vec<String>,
}

impl ConfigDiff {
    /// Number of added/removed/changed entries, excluding skipped IDs.
    #[must_use]
    pub fn change_count(&self) -> usize {
        self.creators_added.len()
            + self.creators_removed.len()
            + self.creators_changed.len()
            + self.templates_added.len()
            + self.templates_removed.len()
            + self.templates_changed.len()
            + self.aliases_added.len()
            + self.aliases_removed.len()
            + self.aliases_changed.len()
            + self.lists_added.len()
            + self.lists_removed.len()
            + self.lists_changed.len()
            + usize::from(self.logging_added.is_some())
            + usize::from(self.logging_removed.is_some())
            + usize::from(self.logging_changed.is_some())
            + usize::from(self.settings_changed.is_some())
    }

    /// True when there is nothing to apply and nothing was skipped.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.change_count() == 0 && self.skipped_unknown_channels.is_empty()
    }
}

/// Discord's per-message character limit for the ephemeral preview body.
pub const PREVIEW_CHAR_LIMIT: usize = 2000;

/// Compute the preview diff. Unknown-channel entries in `incoming` are
/// collected into `skipped_unknown_channels` and excluded from the change
/// lists. Duplicate keys behave as last-wins for diff purposes; the parent
/// revalidation still rejects them.
#[must_use]
pub fn diff_configuration(
    current: &VoiceConfiguration,
    incoming: &VoiceConfiguration,
    inventory: &GuildInventory,
) -> ConfigDiff {
    // Every incoming entry is keyed, but entries touching an unknown channel
    // are skipped: they produce no add/remove/change and leave any current
    // entry with the same key untouched.
    let mut skipped: BTreeSet<String> = BTreeSet::new();
    let incoming_creators: BTreeMap<&str, &CreatorConfiguration> = incoming
        .creators
        .iter()
        .map(|c| (c.channel_id.as_str(), c))
        .collect();
    let skipped_creators: BTreeSet<&str> = incoming_creators
        .values()
        .filter(|c| !creator_usable(c, inventory))
        .map(|c| c.channel_id.as_str())
        .collect();
    for creator in &skipped_creators {
        skipped.insert((*creator).to_owned());
    }
    // The permission-source channel is not keyed by creator: the owning entry
    // is already skipped with its own key above, so report the source too.
    for creator in incoming_creators.values() {
        if let PermissionSource::Channel { channel_id } = &creator.permission_source {
            if !inventory.channels.contains_key(channel_id) {
                skipped.insert(channel_id.clone());
            }
        }
    }
    let current_creators: BTreeMap<&str, &CreatorConfiguration> = current
        .creators
        .iter()
        .map(|c| (c.channel_id.as_str(), c))
        .collect();
    let (creators_added, creators_removed, creators_changed) = diff_keyed(
        &current_creators,
        &incoming_creators,
        &skipped_creators,
        |id, before, after| CreatorChanged {
            channel_id: (*id).to_owned(),
            before: (*before).clone(),
            after: (*after).clone(),
            fields: creator_fields(before, after),
        },
    );

    let incoming_templates: BTreeMap<&str, &ChannelTemplates> = incoming
        .templates
        .iter()
        .map(|t| (t.channel_id.as_str(), t))
        .collect();
    let skipped_templates: BTreeSet<&str> = incoming_templates
        .values()
        .filter(|t| !inventory.channels.contains_key(&t.channel_id))
        .map(|t| t.channel_id.as_str())
        .collect();
    for template in &skipped_templates {
        skipped.insert((*template).to_owned());
    }
    let current_templates: BTreeMap<&str, &ChannelTemplates> = current
        .templates
        .iter()
        .map(|t| (t.channel_id.as_str(), t))
        .collect();
    let (templates_added, templates_removed, templates_changed) = diff_keyed(
        &current_templates,
        &incoming_templates,
        &skipped_templates,
        |id, before, after| TemplateChanged {
            channel_id: (*id).to_owned(),
            before: (*before).clone(),
            after: (*after).clone(),
            fields: template_fields(before, after),
        },
    );

    let current_aliases: BTreeMap<&str, &GameAlias> = current
        .aliases
        .iter()
        .map(|a| (a.game.as_str(), a))
        .collect();
    let incoming_aliases: BTreeMap<&str, &GameAlias> = incoming
        .aliases
        .iter()
        .map(|a| (a.game.as_str(), a))
        .collect();
    let (aliases_added, aliases_removed, aliases_changed) = diff_keyed(
        &current_aliases,
        &incoming_aliases,
        &BTreeSet::new(),
        |game, before, after| AliasChanged {
            game: (*game).to_owned(),
            before: (*before).clone(),
            after: (*after).clone(),
        },
    );

    let current_lists: BTreeMap<&str, &RandomList> =
        current.lists.iter().map(|l| (l.name.as_str(), l)).collect();
    let incoming_lists: BTreeMap<&str, &RandomList> = incoming
        .lists
        .iter()
        .map(|l| (l.name.as_str(), l))
        .collect();
    let (lists_added, lists_removed, lists_changed) = diff_keyed(
        &current_lists,
        &incoming_lists,
        &BTreeSet::new(),
        |name, before, after| ListChanged {
            name: (*name).to_owned(),
            before: (*before).clone(),
            after: (*after).clone(),
        },
    );

    // A logging block whose channel is unknown is skipped, never applied.
    if let Some(logging) = &incoming.logging {
        if !inventory.channels.contains_key(&logging.channel_id) {
            skipped.insert(logging.channel_id.clone());
        }
    }
    let incoming_logging_known = incoming
        .logging
        .as_ref()
        .filter(|l| inventory.channels.contains_key(&l.channel_id));
    let incoming_logging_skipped = incoming.logging.is_some() && incoming_logging_known.is_none();
    let (logging_added, logging_removed, logging_changed) = diff_singleton(
        current.logging.as_ref(),
        incoming_logging_known,
        incoming_logging_skipped,
        |before, after| LoggingChanged {
            before: (*before).clone(),
            after: (*after).clone(),
            fields: logging_fields(before, after),
        },
    );

    let settings_changed = if current.settings == incoming.settings {
        None
    } else {
        Some(SettingsChanged {
            before: current.settings.clone(),
            after: incoming.settings.clone(),
            fields: settings_fields(&current.settings, &incoming.settings),
        })
    };

    ConfigDiff {
        creators_added,
        creators_removed,
        creators_changed,
        templates_added,
        templates_removed,
        templates_changed,
        aliases_added,
        aliases_removed,
        aliases_changed,
        lists_added,
        lists_removed,
        lists_changed,
        logging_added,
        logging_removed,
        logging_changed,
        settings_changed,
        skipped_unknown_channels: skipped.into_iter().collect(),
    }
}

/// Apply a diff to `current`, yielding the filtered incoming in deterministic
/// sorted order. `version` and `guild_id` are carried from `current`; the
/// caller must ensure both sides target the same guild before diffing.
#[must_use]
pub fn apply_diff(current: &VoiceConfiguration, diff: &ConfigDiff) -> VoiceConfiguration {
    let mut creators: BTreeMap<&str, CreatorConfiguration> = current
        .creators
        .iter()
        .map(|c| (c.channel_id.as_str(), c.clone()))
        .collect();
    for removed in &diff.creators_removed {
        creators.remove(removed.channel_id.as_str());
    }
    for changed in &diff.creators_changed {
        creators.insert(changed.channel_id.as_str(), changed.after.clone());
    }
    for added in &diff.creators_added {
        creators.insert(added.channel_id.as_str(), added.clone());
    }

    let mut templates: BTreeMap<&str, ChannelTemplates> = current
        .templates
        .iter()
        .map(|t| (t.channel_id.as_str(), t.clone()))
        .collect();
    for removed in &diff.templates_removed {
        templates.remove(removed.channel_id.as_str());
    }
    for changed in &diff.templates_changed {
        templates.insert(changed.channel_id.as_str(), changed.after.clone());
    }
    for added in &diff.templates_added {
        templates.insert(added.channel_id.as_str(), added.clone());
    }

    let mut aliases: BTreeMap<&str, GameAlias> = current
        .aliases
        .iter()
        .map(|a| (a.game.as_str(), a.clone()))
        .collect();
    for removed in &diff.aliases_removed {
        aliases.remove(removed.game.as_str());
    }
    for changed in &diff.aliases_changed {
        aliases.insert(changed.game.as_str(), changed.after.clone());
    }
    for added in &diff.aliases_added {
        aliases.insert(added.game.as_str(), added.clone());
    }

    let mut lists: BTreeMap<&str, RandomList> = current
        .lists
        .iter()
        .map(|l| (l.name.as_str(), l.clone()))
        .collect();
    for removed in &diff.lists_removed {
        lists.remove(removed.name.as_str());
    }
    for changed in &diff.lists_changed {
        lists.insert(changed.name.as_str(), changed.after.clone());
    }
    for added in &diff.lists_added {
        lists.insert(added.name.as_str(), added.clone());
    }

    let logging = if let Some(added) = &diff.logging_added {
        Some(added.clone())
    } else if diff.logging_removed.is_some() {
        None
    } else if let Some(changed) = &diff.logging_changed {
        Some(changed.after.clone())
    } else {
        current.logging.clone()
    };
    let settings = diff
        .settings_changed
        .as_ref()
        .map_or_else(|| current.settings.clone(), |c| c.after.clone());

    VoiceConfiguration {
        version: current.version,
        guild_id: current.guild_id.clone(),
        creators: creators.into_values().collect(),
        templates: templates.into_values().collect(),
        aliases: aliases.into_values().collect(),
        lists: lists.into_values().collect(),
        logging,
        settings,
    }
}

/// Compact ephemeral-message body. Shows at most `max_lines` entry lines in
/// section-then-id order, then a `+N more` trailer. The result never exceeds
/// [`PREVIEW_CHAR_LIMIT`] characters. An empty diff renders `No changes`.
#[must_use]
pub fn render_preview(diff: &ConfigDiff, max_lines: usize) -> String {
    if diff.is_empty() {
        return "No changes".to_owned();
    }
    let mut entries: Vec<String> = Vec::new();
    push_merged(
        &mut entries,
        "creator",
        diff.creators_added.iter().map(|c| c.channel_id.as_str()),
        diff.creators_removed.iter().map(|c| c.channel_id.as_str()),
        diff.creators_changed
            .iter()
            .map(|c| (c.channel_id.as_str(), join_fields(&c.fields))),
    );
    push_merged(
        &mut entries,
        "template",
        diff.templates_added.iter().map(|c| c.channel_id.as_str()),
        diff.templates_removed.iter().map(|c| c.channel_id.as_str()),
        diff.templates_changed
            .iter()
            .map(|c| (c.channel_id.as_str(), join_fields(&c.fields))),
    );
    push_simple(
        &mut entries,
        "alias",
        diff.aliases_added.iter().map(|a| display_key(&a.game)),
        diff.aliases_removed.iter().map(|a| display_key(&a.game)),
        diff.aliases_changed.iter().map(|c| display_key(&c.game)),
    );
    push_simple(
        &mut entries,
        "list",
        diff.lists_added.iter().map(|l| display_key(&l.name)),
        diff.lists_removed.iter().map(|l| display_key(&l.name)),
        diff.lists_changed.iter().map(|c| display_key(&c.name)),
    );
    if diff.logging_added.is_some() {
        entries.push("+ logging".to_owned());
    } else if diff.logging_removed.is_some() {
        entries.push("- logging".to_owned());
    } else if let Some(changed) = &diff.logging_changed {
        entries.push(truncate_line(&format!(
            "~ logging ({})",
            join_fields(&changed.fields)
        )));
    }
    if let Some(changed) = &diff.settings_changed {
        entries.push(truncate_line(&format!(
            "~ settings ({})",
            join_fields(&changed.fields)
        )));
    }
    for id in &diff.skipped_unknown_channels {
        entries.push(truncate_line(&format!("! skipped channel {id}")));
    }

    let header = format!(
        "Voice config import preview ({} changes):",
        diff.change_count()
    );
    fit_preview(&header, &entries, max_lines)
}

fn join_fields(fields: &[&'static str]) -> String {
    fields.join(", ")
}

/// Single-line display for free-text keys: newlines flattened, capped at 80
/// characters and quoted when they contain spaces (readable in the preview).
fn display_key(key: &str) -> String {
    let flat: String = key
        .chars()
        .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
        .collect();
    let truncated: String = flat.chars().take(80).collect();
    if truncated.contains(' ') || truncated.chars().count() >= 20 {
        format!("\"{truncated}\"")
    } else {
        truncated
    }
}

fn truncate_line(line: &str) -> String {
    if line.chars().count() <= 200 {
        line.to_owned()
    } else {
        line.chars().take(197).collect::<String>() + "..."
    }
}

/// Interleave added/removed/changed for one ID-keyed section in ascending ID
/// order: a changed ID renders once with its field list.
fn push_merged<'a>(
    entries: &mut Vec<String>,
    section: &str,
    added: impl Iterator<Item = &'a str>,
    removed: impl Iterator<Item = &'a str>,
    changed: impl Iterator<Item = (&'a str, String)>,
) {
    let mut order: BTreeSet<&'a str> = BTreeSet::new();
    let mut changed_fields: BTreeMap<&'a str, String> = BTreeMap::new();
    let mut added_ids: BTreeSet<&'a str> = BTreeSet::new();
    for id in added {
        order.insert(id);
        added_ids.insert(id);
    }
    for id in removed {
        order.insert(id);
    }
    for (id, fields) in changed {
        order.insert(id);
        changed_fields.insert(id, fields);
    }
    for id in order {
        if let Some(fields) = changed_fields.get(id) {
            entries.push(truncate_line(&format!("~ {section} {id} ({fields})")));
        } else if added_ids.contains(id) {
            entries.push(truncate_line(&format!("+ {section} {id}")));
        } else {
            entries.push(truncate_line(&format!("- {section} {id}")));
        }
    }
}

/// Interleave added/removed/changed for one key-keyed section in ascending
/// key order. Keys are pre-formatted with [`display_key`].
fn push_simple(
    entries: &mut Vec<String>,
    section: &str,
    added: impl Iterator<Item = String>,
    removed: impl Iterator<Item = String>,
    changed: impl Iterator<Item = String>,
) {
    let mut order: BTreeSet<String> = BTreeSet::new();
    let mut added_keys: BTreeSet<String> = BTreeSet::new();
    let mut changed_keys: BTreeSet<String> = BTreeSet::new();
    for key in added {
        order.insert(key.clone());
        added_keys.insert(key);
    }
    for key in removed {
        order.insert(key);
    }
    for key in changed {
        order.insert(key.clone());
        changed_keys.insert(key);
    }
    for key in &order {
        if changed_keys.contains(key) {
            entries.push(truncate_line(&format!("~ {section} {key}")));
        } else if added_keys.contains(key) {
            entries.push(truncate_line(&format!("+ {section} {key}")));
        } else {
            entries.push(truncate_line(&format!("- {section} {key}")));
        }
    }
}

/// Emit the header plus at most `max_lines` entry lines, then a `+N more`
/// trailer counting the hidden lines. Drops trailing lines until the whole
/// body fits [`PREVIEW_CHAR_LIMIT`] characters.
fn fit_preview(header: &str, entries: &[String], max_lines: usize) -> String {
    let mut shown: Vec<&str> = if entries.len() <= max_lines {
        entries.iter().map(String::as_str).collect()
    } else if max_lines == 0 {
        Vec::new()
    } else {
        entries[..max_lines.saturating_sub(1)]
            .iter()
            .map(String::as_str)
            .collect()
    };
    let mut hidden = entries.len().saturating_sub(shown.len());
    loop {
        let mut output = String::from(header);
        for line in &shown {
            output.push('\n');
            output.push_str(line);
        }
        if hidden > 0 {
            output.push('\n');
            output.push_str(&format!("+{hidden} more"));
        }
        if output.chars().count() <= PREVIEW_CHAR_LIMIT {
            return output;
        }
        if shown.is_empty() {
            // Header plus trailer alone exceed the limit; truncate the body.
            return output.chars().take(PREVIEW_CHAR_LIMIT).collect();
        }
        shown.pop();
        hidden += 1;
    }
}

/// A creator entry is usable unless it or its permission-source channel is
/// unknown. Cross-guild or wrong-kind channels stay the codec's concern; only
/// absence from the inventory skips here.
fn creator_usable(creator: &CreatorConfiguration, inventory: &GuildInventory) -> bool {
    inventory.channels.contains_key(&creator.channel_id)
        && match &creator.permission_source {
            PermissionSource::Channel { channel_id } => inventory.channels.contains_key(channel_id),
            PermissionSource::Creator {} | PermissionSource::Category {} => true,
        }
}

/// Sorted added/removed/changed between two keyed maps. Keys in `skipped`
/// are ignored on both sides: an incoming skipped entry is not added or
/// changed, and a current entry under a skipped key is not removed.
/// Duplicate keys in the input lists were already collapsed by the caller
/// collecting into a map.
fn diff_keyed<'a, T: Clone + PartialEq, C>(
    current: &BTreeMap<&'a str, &'a T>,
    incoming: &BTreeMap<&'a str, &'a T>,
    skipped: &BTreeSet<&'a str>,
    on_changed: impl Fn(&'a str, &'a T, &'a T) -> C,
) -> (Vec<T>, Vec<T>, Vec<C>) {
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    for (id, next) in incoming {
        if skipped.contains(*id) {
            continue;
        }
        match current.get(*id) {
            None => added.push((**next).clone()),
            Some(prev) if **prev != **next => changed.push(on_changed(*id, *prev, *next)),
            Some(_) => {}
        }
    }
    for (id, prev) in current {
        if !incoming.contains_key(*id) && !skipped.contains(*id) {
            removed.push((**prev).clone());
        }
    }
    (added, removed, changed)
}

/// Singleton diff with a skip switch: when `skipped` is true the incoming
/// value is dropped and the current value is kept, producing no change.
fn diff_singleton<T: Clone + PartialEq, C>(
    current: Option<&T>,
    incoming: Option<&T>,
    skipped: bool,
    on_changed: impl Fn(&T, &T) -> C,
) -> (Option<T>, Option<T>, Option<C>) {
    if skipped {
        return (None, None, None);
    }
    match (current, incoming) {
        (None, None) => (None, None, None),
        (None, Some(next)) => (Some(next.clone()), None, None),
        (Some(_), None) => (None, current.cloned(), None),
        (Some(prev), Some(next)) if prev == next => (None, None, None),
        (Some(prev), Some(next)) => (None, None, Some(on_changed(prev, next))),
    }
}

fn creator_fields(
    before: &CreatorConfiguration,
    after: &CreatorConfiguration,
) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if before.name_template != after.name_template {
        fields.push("name_template");
    }
    if before.status_template != after.status_template {
        fields.push("status_template");
    }
    if before.default_limit != after.default_limit {
        fields.push("default_limit");
    }
    if before.always_private != after.always_private {
        fields.push("always_private");
    }
    if before.text_channels != after.text_channels {
        fields.push("text_channels");
    }
    if before.position != after.position {
        fields.push("position");
    }
    if before.first_number != after.first_number {
        fields.push("first_number");
    }
    if before.group_by_category != after.group_by_category {
        fields.push("group_by_category");
    }
    if before.permission_source != after.permission_source {
        fields.push("permission_source");
    }
    fields
}

fn template_fields(before: &ChannelTemplates, after: &ChannelTemplates) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if before.name_template != after.name_template {
        fields.push("name_template");
    }
    if before.status_template != after.status_template {
        fields.push("status_template");
    }
    fields
}

fn logging_fields(
    before: &LoggingConfiguration,
    after: &LoggingConfiguration,
) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if before.channel_id != after.channel_id {
        fields.push("channel_id");
    }
    if before.detail != after.detail {
        fields.push("detail");
    }
    if before.mention_member_ids != after.mention_member_ids {
        fields.push("mention_member_ids");
    }
    if before.mention_role_ids != after.mention_role_ids {
        fields.push("mention_role_ids");
    }
    fields
}

#[allow(clippy::too_many_lines)]
fn settings_fields(before: &GuildSettings, after: &GuildSettings) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if before.creation_enabled != after.creation_enabled {
        fields.push("creation_enabled");
    }
    if before.unique_names != after.unique_names {
        fields.push("unique_names");
    }
    if before.no_game_label != after.no_game_label {
        fields.push("no_game_label");
    }
    if before.force_single_game != after.force_single_game {
        fields.push("force_single_game");
    }
    if before.count_members_without_activity != after.count_members_without_activity {
        fields.push("count_members_without_activity");
    }
    if before.time_zone != after.time_zone {
        fields.push("time_zone");
    }
    if before.text_channel_name != after.text_channel_name {
        fields.push("text_channel_name");
    }
    if before.text_viewer_role_id != after.text_viewer_role_id {
        fields.push("text_viewer_role_id");
    }
    if before.command_role_id != after.command_role_id {
        fields.push("command_role_id");
    }
    if before.command_roles != after.command_roles {
        fields.push("command_roles");
    }
    fields
}
