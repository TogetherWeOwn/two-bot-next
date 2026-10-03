//! Pure V11 import diff preview, written from `docs/voice-rooms.md` §V11 only.
//!
//! The caller supplies the current configuration, an incoming candidate and a
//! trusted guild inventory. Incoming entries that reference a channel ID absent
//! from the inventory are reported and skipped; the diff compares `current`
//! with what remains. No Discord, persistence or I/O lives here, and nothing is
//! validated beyond channel presence: the integration layer revalidates the
//! remaining candidate with `voice_config::validate_configuration` before it
//! asks for confirmation and again before it writes.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use sha2::{Digest, Sha256};

use crate::voice_config::{
    ChannelTemplates, CreatorConfiguration, GameAlias, GuildInventory, GuildSettings,
    LoggingConfiguration, PermissionSource, RandomList, VoiceConfiguration,
};

/// Length of [`diff_content_hash`] in hex chars (also the custom-id hash
/// segment; see `voice_custom_id::IMPORT_HASH_CHARS`).
pub const DIFF_HASH_CHARS: usize = 16;

/// Discord's message length limit, applied to the whole preview body.
pub const PREVIEW_CHAR_LIMIT: usize = 2000;

/// No single preview line is longer than this many characters.
const LINE_CHAR_LIMIT: usize = 200;

/// Free-text keys (game and list names) are shown up to this many characters.
const KEY_CHAR_LIMIT: usize = 80;

/// One keyed entry present on both sides with a different value. `fields`
/// names the differing fields in declaration order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryChange<T> {
    pub key: String,
    pub before: T,
    pub after: T,
    pub fields: Vec<&'static str>,
}

/// Added, removed and changed entries of one section, each sorted by key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionDiff<T> {
    pub added: Vec<T>,
    pub removed: Vec<T>,
    pub changed: Vec<EntryChange<T>>,
}

impl<T> Default for SectionDiff<T> {
    fn default() -> Self {
        Self {
            added: Vec::new(),
            removed: Vec::new(),
            changed: Vec::new(),
        }
    }
}

impl<T> SectionDiff<T> {
    #[must_use]
    pub fn len(&self) -> usize {
        self.added.len() + self.removed.len() + self.changed.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Deterministic difference between `current` and the incoming candidate after
/// unknown-channel entries are skipped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfigDiff {
    pub creators: SectionDiff<CreatorConfiguration>,
    pub templates: SectionDiff<ChannelTemplates>,
    pub aliases: SectionDiff<GameAlias>,
    pub lists: SectionDiff<RandomList>,
    /// A singleton section: at most one entry in total.
    pub logging: SectionDiff<LoggingConfiguration>,
    /// Settings always exist on both sides, so they can only change.
    pub settings: Option<EntryChange<GuildSettings>>,
    /// Sorted, unique channel IDs referenced by `incoming` but absent from the
    /// inventory. Every entry referencing one was left out of the diff.
    pub skipped_unknown_channels: Vec<String>,
}

impl ConfigDiff {
    /// Added, removed and changed entries across all sections.
    #[must_use]
    pub fn change_count(&self) -> usize {
        self.creators.len()
            + self.templates.len()
            + self.aliases.len()
            + self.lists.len()
            + self.logging.len()
            + usize::from(self.settings.is_some())
    }

    /// Nothing to apply and nothing skipped.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.change_count() == 0 && self.skipped_unknown_channels.is_empty()
    }
}

/// Removes every incoming entry that references a channel ID absent from the
/// inventory: a creator whose room or permission-source channel is unknown, a
/// template on an unknown channel, and logging to an unknown channel. Returns
/// the remaining candidate and the sorted, unique unknown IDs. Cross-guild or
/// wrong-kind channels are present, so they stay for the codec to reject.
#[must_use]
pub fn skip_unknown_channels(
    incoming: &VoiceConfiguration,
    inventory: &GuildInventory,
) -> (VoiceConfiguration, Vec<String>) {
    let mut unknown = BTreeSet::new();
    let mut known = |id: &str| {
        let present = inventory.channels.contains_key(id);
        if !present {
            unknown.insert(id.to_owned());
        }
        present
    };
    let mut remaining = incoming.clone();
    remaining.creators.retain(|creator| {
        let room = known(&creator.channel_id);
        let source = match &creator.permission_source {
            PermissionSource::Channel { channel_id } => known(channel_id),
            PermissionSource::Creator {} | PermissionSource::Category {} => true,
        };
        room && source
    });
    remaining
        .templates
        .retain(|template| known(&template.channel_id));
    if remaining
        .logging
        .as_ref()
        .is_some_and(|logging| !known(&logging.channel_id))
    {
        remaining.logging = None;
    }
    (remaining, unknown.into_iter().collect())
}

/// Compares `current` with `incoming` minus its unknown-channel entries.
/// Entry order within a section is insignificant. A duplicated incoming key
/// counts once (last wins); the codec rejects duplicates on revalidation.
#[must_use]
pub fn diff_configuration(
    current: &VoiceConfiguration,
    incoming: &VoiceConfiguration,
    inventory: &GuildInventory,
) -> ConfigDiff {
    let (incoming, skipped_unknown_channels) = skip_unknown_channels(incoming, inventory);
    ConfigDiff {
        creators: diff_section(&current.creators, &incoming.creators),
        templates: diff_section(&current.templates, &incoming.templates),
        aliases: diff_section(&current.aliases, &incoming.aliases),
        lists: diff_section(&current.lists, &incoming.lists),
        logging: diff_section(current.logging.as_slice(), incoming.logging.as_slice()),
        settings: change(&current.settings, &incoming.settings),
        skipped_unknown_channels,
    }
}

/// Applies `diff` to `current`. For a diff produced against `current`, this
/// equals the remaining incoming candidate with each section sorted by key.
/// `version` and `guild_id` come from `current`.
#[must_use]
pub fn apply_diff(current: &VoiceConfiguration, diff: &ConfigDiff) -> VoiceConfiguration {
    VoiceConfiguration {
        version: current.version,
        guild_id: current.guild_id.clone(),
        creators: apply_section(&current.creators, &diff.creators),
        templates: apply_section(&current.templates, &diff.templates),
        aliases: apply_section(&current.aliases, &diff.aliases),
        lists: apply_section(&current.lists, &diff.lists),
        logging: apply_section(current.logging.as_slice(), &diff.logging).pop(),
        settings: diff
            .settings
            .as_ref()
            .map_or_else(|| current.settings.clone(), |change| change.after.clone()),
    }
}

/// Content hash binding a preview to the exact (`current`, `candidate`) pair
/// it was rendered from: the first [`DIFF_HASH_CHARS`] hex chars of sha256
/// over the compact JSON of both, separated by a zero byte. The confirm step
/// recomputes it against freshly read state; any concurrent change yields a
/// different hash, and the runtime re-previews instead of applying.
#[must_use]
pub fn diff_content_hash(current: &VoiceConfiguration, candidate: &VoiceConfiguration) -> String {
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(current).unwrap_or_default());
    hasher.update([0x00]);
    hasher.update(serde_json::to_vec(candidate).unwrap_or_default());
    let digest = format!("{:x}", hasher.finalize());
    digest[..DIFF_HASH_CHARS].to_owned()
}

/// Compact ephemeral-message body: a summary line, then at most `max_lines`
/// entry lines in section order (creators, templates, aliases, lists, logging,
/// settings, skipped channels) and key order within a section, then `+N more`
/// for the lines left out. The body never exceeds [`PREVIEW_CHAR_LIMIT`]
/// characters. An empty diff renders `No changes`.
///
/// Free-text keys are quoted and escaped, but the text is not Markdown-escaped;
/// send it with mentions disabled.
#[must_use]
pub fn render_preview(diff: &ConfigDiff, max_lines: usize) -> String {
    if diff.is_empty() {
        return "No changes".to_owned();
    }
    let mut lines = Vec::new();
    section_lines(&mut lines, &diff.creators);
    section_lines(&mut lines, &diff.templates);
    section_lines(&mut lines, &diff.aliases);
    section_lines(&mut lines, &diff.lists);
    section_lines(&mut lines, &diff.logging);
    if let Some(change) = &diff.settings {
        lines.push(changed_line(change));
    }
    for id in &diff.skipped_unknown_channels {
        lines.push(cap_line(format!(
            "! skipped unknown channel {}",
            display_key(id)
        )));
    }
    let skipped = diff.skipped_unknown_channels.len();
    let summary = format!(
        "Import preview: {}, {} skipped",
        counted(diff.change_count(), "change"),
        counted(skipped, "unknown channel"),
    );
    fit_preview(summary, &lines, max_lines)
}

/// A keyed configuration entry. Singletons use their section name as the key.
trait Entry: Clone + PartialEq {
    const SECTION: &'static str;
    const SINGLETON: bool = false;

    fn key(&self) -> &str;

    fn fields(&self, after: &Self) -> Vec<&'static str>;
}

impl Entry for CreatorConfiguration {
    const SECTION: &'static str = "creator";

    fn key(&self) -> &str {
        &self.channel_id
    }

    fn fields(&self, after: &Self) -> Vec<&'static str> {
        let Self {
            channel_id: _,
            name_template,
            status_template,
            default_limit,
            always_private,
            text_channels,
            position,
            first_number,
            group_by_category,
            permission_source,
        } = self;
        differing([
            ("name_template", name_template != &after.name_template),
            ("status_template", status_template != &after.status_template),
            ("default_limit", default_limit != &after.default_limit),
            ("always_private", always_private != &after.always_private),
            ("text_channels", text_channels != &after.text_channels),
            ("position", position != &after.position),
            ("first_number", first_number != &after.first_number),
            (
                "group_by_category",
                group_by_category != &after.group_by_category,
            ),
            (
                "permission_source",
                permission_source != &after.permission_source,
            ),
        ])
    }
}

impl Entry for ChannelTemplates {
    const SECTION: &'static str = "template";

    fn key(&self) -> &str {
        &self.channel_id
    }

    fn fields(&self, after: &Self) -> Vec<&'static str> {
        let Self {
            channel_id: _,
            name_template,
            status_template,
        } = self;
        differing([
            ("name_template", name_template != &after.name_template),
            ("status_template", status_template != &after.status_template),
        ])
    }
}

impl Entry for GameAlias {
    const SECTION: &'static str = "alias";

    fn key(&self) -> &str {
        &self.game
    }

    fn fields(&self, after: &Self) -> Vec<&'static str> {
        let Self { game: _, alias } = self;
        differing([("alias", alias != &after.alias)])
    }
}

impl Entry for RandomList {
    const SECTION: &'static str = "list";

    fn key(&self) -> &str {
        &self.name
    }

    fn fields(&self, after: &Self) -> Vec<&'static str> {
        let Self { name: _, choices } = self;
        differing([("choices", choices != &after.choices)])
    }
}

impl Entry for LoggingConfiguration {
    const SECTION: &'static str = "logging";
    const SINGLETON: bool = true;

    fn key(&self) -> &str {
        Self::SECTION
    }

    fn fields(&self, after: &Self) -> Vec<&'static str> {
        let Self {
            channel_id,
            detail,
            mention_member_ids,
            mention_role_ids,
        } = self;
        differing([
            ("channel_id", channel_id != &after.channel_id),
            ("detail", detail != &after.detail),
            (
                "mention_member_ids",
                mention_member_ids != &after.mention_member_ids,
            ),
            (
                "mention_role_ids",
                mention_role_ids != &after.mention_role_ids,
            ),
        ])
    }
}

impl Entry for GuildSettings {
    const SECTION: &'static str = "settings";
    const SINGLETON: bool = true;

    fn key(&self) -> &str {
        Self::SECTION
    }

    fn fields(&self, after: &Self) -> Vec<&'static str> {
        let Self {
            creation_enabled,
            unique_names,
            no_game_label,
            force_single_game,
            count_members_without_activity,
            time_zone,
            text_channel_name,
            text_viewer_role_id,
            command_role_id,
            command_roles,
        } = self;
        differing([
            (
                "creation_enabled",
                creation_enabled != &after.creation_enabled,
            ),
            ("unique_names", unique_names != &after.unique_names),
            ("no_game_label", no_game_label != &after.no_game_label),
            (
                "force_single_game",
                force_single_game != &after.force_single_game,
            ),
            (
                "count_members_without_activity",
                count_members_without_activity != &after.count_members_without_activity,
            ),
            ("time_zone", time_zone != &after.time_zone),
            (
                "text_channel_name",
                text_channel_name != &after.text_channel_name,
            ),
            (
                "text_viewer_role_id",
                text_viewer_role_id != &after.text_viewer_role_id,
            ),
            ("command_role_id", command_role_id != &after.command_role_id),
            ("command_roles", command_roles != &after.command_roles),
        ])
    }
}

fn differing<const N: usize>(checks: [(&'static str, bool); N]) -> Vec<&'static str> {
    checks
        .into_iter()
        .filter_map(|(field, differs)| differs.then_some(field))
        .collect()
}

fn change<T: Entry>(before: &T, after: &T) -> Option<EntryChange<T>> {
    (before != after).then(|| EntryChange {
        key: after.key().to_owned(),
        before: before.clone(),
        after: after.clone(),
        fields: before.fields(after),
    })
}

fn by_key<T: Entry>(entries: &[T]) -> BTreeMap<&str, &T> {
    entries.iter().map(|entry| (entry.key(), entry)).collect()
}

fn diff_section<T: Entry>(current: &[T], incoming: &[T]) -> SectionDiff<T> {
    let current = by_key(current);
    let incoming = by_key(incoming);
    let mut diff = SectionDiff::default();
    for (key, after) in &incoming {
        match current.get(key) {
            None => diff.added.push(T::clone(after)),
            Some(before) => diff.changed.extend(change(*before, *after)),
        }
    }
    diff.removed = current
        .iter()
        .filter(|(key, _)| !incoming.contains_key(*key))
        .map(|(_, before)| T::clone(before))
        .collect();
    diff
}

fn apply_section<T: Entry>(current: &[T], diff: &SectionDiff<T>) -> Vec<T> {
    let mut entries = by_key(current);
    for removed in &diff.removed {
        entries.remove(removed.key());
    }
    for entry in diff
        .added
        .iter()
        .chain(diff.changed.iter().map(|change| &change.after))
    {
        entries.insert(entry.key(), entry);
    }
    entries.into_values().cloned().collect()
}

/// Appends one line per entry, ordered by key across added/removed/changed.
fn section_lines<T: Entry>(lines: &mut Vec<String>, diff: &SectionDiff<T>) {
    let mut keyed: Vec<(&str, String)> = Vec::with_capacity(diff.len());
    keyed.extend(
        diff.added
            .iter()
            .map(|entry| (entry.key(), entry_line::<T>('+', entry.key()))),
    );
    keyed.extend(
        diff.removed
            .iter()
            .map(|entry| (entry.key(), entry_line::<T>('-', entry.key()))),
    );
    keyed.extend(
        diff.changed
            .iter()
            .map(|change| (change.key.as_str(), changed_line(change))),
    );
    keyed.sort_by_key(|(key, _)| *key);
    lines.extend(keyed.into_iter().map(|(_, line)| line));
}

fn entry_line<T: Entry>(sign: char, key: &str) -> String {
    if T::SINGLETON {
        format!("{sign} {}", T::SECTION)
    } else {
        cap_line(format!("{sign} {} {}", T::SECTION, display_key(key)))
    }
}

fn changed_line<T: Entry>(change: &EntryChange<T>) -> String {
    let fields = change.fields.join(", ");
    cap_line(format!("{} ({fields})", entry_line::<T>('~', &change.key)))
}

/// Snowflake-shaped keys are shown bare; anything else is quoted, escaped and
/// shortened, so uploaded text cannot break the line layout.
fn display_key(key: &str) -> String {
    if (1..=20).contains(&key.len()) && key.bytes().all(|byte| byte.is_ascii_digit()) {
        return key.to_owned();
    }
    let shown: String = key.chars().take(KEY_CHAR_LIMIT).collect();
    let cut = if shown.len() < key.len() { "…" } else { "" };
    format!("{shown:?}{cut}")
}

fn cap_line(line: String) -> String {
    if line.chars().count() <= LINE_CHAR_LIMIT {
        return line;
    }
    let mut capped: String = line.chars().take(LINE_CHAR_LIMIT - 1).collect();
    capped.push('…');
    capped
}

fn counted(count: usize, noun: &str) -> String {
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {noun}{plural}")
}

/// Length in UTF-16 code units. That is never less than the character count,
/// so the limit holds whichever of the two the message is measured in.
fn message_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Keeps the longest prefix of at most `max_lines` lines that fits the limit
/// together with a `+N more` trailer for the rest.
fn fit_preview(summary: String, lines: &[String], max_lines: usize) -> String {
    let trailer_room = message_len(&format!("\n+{} more", lines.len()));
    let mut body = summary;
    let mut used = message_len(&body);
    let mut shown = 0;
    for line in lines.iter().take(max_lines) {
        let needed = 1 + message_len(line);
        let reserve = if shown + 1 < lines.len() {
            trailer_room
        } else {
            0
        };
        if used + needed + reserve > PREVIEW_CHAR_LIMIT {
            break;
        }
        body.push('\n');
        body.push_str(line);
        used += needed;
        shown += 1;
    }
    let hidden = lines.len() - shown;
    if hidden > 0 {
        write!(body, "\n+{hidden} more").unwrap();
    }
    body
}
