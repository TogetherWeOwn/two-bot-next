//! Pure V7a game-alias and `/nick` decisions, written from `docs/voice-rooms.md`
//! §V5 and §V7 only.
//!
//! The alias table maps a raw activity name (the **key**) to the canonical game
//! name (the **target**) that `@@game_name@@` and `GAME` conditions see. The
//! caller loads the guild's stored entries, applies one `/alias` edit and
//! persists the result; this module performs no I/O and holds no Discord, store
//! or clock types. Choosing the majority game (V5), rendering templates and
//! storing nicknames belong to the parent runtime.

use unicode_normalization::UnicodeNormalization;

/// Longest alias key, in Unicode scalar values after trimming.
pub const MAX_ALIAS_KEY_CHARS: usize = 100;
/// Longest alias target, in Unicode scalar values after trimming. Targets are
/// shown in channel names, which Discord caps at 100 characters.
pub const MAX_ALIAS_TARGET_CHARS: usize = 100;
/// Most alias entries one guild may store.
pub const MAX_ALIASES_PER_GUILD: usize = 100;
/// Most input text any error in this module repeats back. Only validated keys
/// are ever echoed, so this equals [`MAX_ALIAS_KEY_CHARS`].
pub const MAX_ERROR_ECHO_CHARS: usize = MAX_ALIAS_KEY_CHARS;
/// Longest `/nick` name, in Unicode scalar values after trimming. Matches
/// Discord's own nickname length.
pub const MAX_NICK_CHARS: usize = 32;
/// Keyword that clears a member's `/nick` name.
pub const NICK_RESET_KEYWORD: &str = "reset";

/// Which half of an alias entry an error refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasField {
    Key,
    Target,
}

impl std::fmt::Display for AliasField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Key => "game name",
            Self::Target => "alias",
        })
    }
}

/// Typed `/alias` refusals. Variants that carry text only carry a key that has
/// already passed validation, so no error repeats more than
/// [`MAX_ERROR_ECHO_CHARS`] of input or any control character.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AliasError {
    #[error("The {0} cannot be empty.")]
    Empty(AliasField),
    #[error("The {field} is {chars} characters long; the maximum is {max}.")]
    TooLong {
        field: AliasField,
        chars: usize,
        max: usize,
    },
    #[error("The {0} cannot contain control characters or line breaks.")]
    ControlCharacter(AliasField),
    #[error("This server already has the maximum of {max} aliases.")]
    TableFull { max: usize },
    #[error("An alias for \"{existing}\" already exists.")]
    DuplicateKey { existing: String },
    #[error("There is no alias for \"{key}\".")]
    UnknownKey { key: String },
    /// The target names an aliased game whose own alias differs (a chain).
    #[error("\"{key}\" already has a different alias; point at that alias instead.")]
    TargetIsKey { key: String },
    /// The key is another entry's target, and the targets differ (a chain).
    #[error("This name is the alias of \"{key}\"; give it the same alias or none.")]
    KeyIsTarget { key: String },
}

/// One stored alias. Both halves are trimmed of surrounding whitespace and
/// otherwise kept exactly as the admin typed them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasEntry {
    pub key: String,
    pub target: String,
}

/// A guild's alias table in insertion order.
///
/// Invariants, upheld by every constructor and operation:
/// - at most [`MAX_ALIASES_PER_GUILD`] entries, each valid on its own;
/// - keys are unique under [`fold_name`] (case-insensitive);
/// - **chain rule:** every target resolves to itself. A target may fold to
///   another entry's key only when that entry's target is exactly the same
///   text, so it adds no second hop. A target equal to its own key (a case
///   correction such as `APEX LEGENDS` → `Apex Legends`) is allowed, and other
///   keys may then point at the same exact `Apex Legends`.
///
/// Together these make resolution a single hop with no chains or cycles, so
/// resolving a canonical name always returns it unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AliasTable {
    entries: Vec<AliasEntry>,
}

/// The comparison form of a game name: surrounding whitespace trimmed,
/// lowercased, then NFC-normalised. Canonically equivalent spellings (such as
/// a precomposed `é` and `e` plus a combining accent) therefore match.
/// Compatibility forms (NFKC: full-width letters, ligatures, `™`) and interior
/// whitespace are **not** folded; they look different and stay distinct.
#[must_use]
pub fn fold_name(name: &str) -> String {
    name.trim().to_lowercase().nfc().collect()
}

fn is_refused_control(c: char) -> bool {
    // Cc covers C0/C1 (including \n, \r, \t and NEL); add the Unicode line and
    // paragraph separators and the bidirectional embedding/override/isolate
    // controls, which reorder surrounding text in channel names and messages.
    c.is_control()
        || matches!(
            c,
            '\u{2028}' | '\u{2029}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
        )
}

fn validate_part(raw: &str, field: AliasField, max: usize) -> Result<&str, AliasError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AliasError::Empty(field));
    }
    let chars = trimmed.chars().count();
    if chars > max {
        return Err(AliasError::TooLong { field, chars, max });
    }
    if trimmed.chars().any(is_refused_control) {
        return Err(AliasError::ControlCharacter(field));
    }
    Ok(trimmed)
}

impl AliasTable {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild a table from stored or imported pairs, applying [`Self::add`]
    /// in order. The first refused pair fails the whole load, so a table that
    /// violates the invariants is never constructed.
    pub fn from_entries<K, T, I>(pairs: I) -> Result<Self, AliasError>
    where
        K: AsRef<str>,
        T: AsRef<str>,
        I: IntoIterator<Item = (K, T)>,
    {
        let mut table = Self::new();
        for (key, target) in pairs {
            table.add(key.as_ref(), target.as_ref())?;
        }
        Ok(table)
    }

    #[must_use]
    pub fn entries(&self) -> &[AliasEntry] {
        &self.entries
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entry whose key matches `name` under [`fold_name`], if any.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&AliasEntry> {
        let folded = fold_name(name);
        self.position(&folded).map(|index| &self.entries[index])
    }

    fn position(&self, folded_key: &str) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| fold_name(&entry.key) == folded_key)
    }

    /// Chain rule for a would-be entry `key` → `target`, checked against every
    /// entry except `skip` (the entry being edited). After the change every
    /// target must still resolve to exactly itself: the new target through
    /// another key, and every other target through the new key.
    fn check_chain(
        &self,
        folded_key: &str,
        target: &str,
        skip: Option<usize>,
    ) -> Result<(), AliasError> {
        let folded_target = fold_name(target);
        for (index, entry) in self.entries.iter().enumerate() {
            if Some(index) == skip || entry.target == target {
                continue;
            }
            if fold_name(&entry.key) == folded_target {
                return Err(AliasError::TargetIsKey {
                    key: entry.key.clone(),
                });
            }
            if fold_name(&entry.target) == folded_key {
                return Err(AliasError::KeyIsTarget {
                    key: entry.key.clone(),
                });
            }
        }
        Ok(())
    }

    /// Add `key` → `target`. Refuses an invalid half, a full table, a key that
    /// already exists in any case, and either direction of an alias chain.
    pub fn add(&mut self, key: &str, target: &str) -> Result<&AliasEntry, AliasError> {
        let key = validate_part(key, AliasField::Key, MAX_ALIAS_KEY_CHARS)?;
        let target = validate_part(target, AliasField::Target, MAX_ALIAS_TARGET_CHARS)?;
        let folded_key = fold_name(key);
        if let Some(index) = self.position(&folded_key) {
            return Err(AliasError::DuplicateKey {
                existing: self.entries[index].key.clone(),
            });
        }
        if self.entries.len() >= MAX_ALIASES_PER_GUILD {
            return Err(AliasError::TableFull {
                max: MAX_ALIASES_PER_GUILD,
            });
        }
        self.check_chain(&folded_key, target, None)?;
        self.entries.push(AliasEntry {
            key: key.to_owned(),
            target: target.to_owned(),
        });
        Ok(&self.entries[self.entries.len() - 1])
    }

    /// Point an existing key at a new target. The key keeps its stored
    /// spelling and position; rename a key with remove then add. Returns the
    /// previous entry.
    pub fn edit(&mut self, key: &str, target: &str) -> Result<AliasEntry, AliasError> {
        let key = validate_part(key, AliasField::Key, MAX_ALIAS_KEY_CHARS)?;
        let target = validate_part(target, AliasField::Target, MAX_ALIAS_TARGET_CHARS)?;
        let folded_key = fold_name(key);
        let index = self
            .position(&folded_key)
            .ok_or_else(|| AliasError::UnknownKey {
                key: key.to_owned(),
            })?;
        self.check_chain(&folded_key, target, Some(index))?;
        let previous = self.entries[index].clone();
        self.entries[index].target = target.to_owned();
        Ok(previous)
    }

    /// Remove the entry for `key` (any case) and return it.
    pub fn remove(&mut self, key: &str) -> Result<AliasEntry, AliasError> {
        let key = validate_part(key, AliasField::Key, MAX_ALIAS_KEY_CHARS)?;
        let index = self
            .position(&fold_name(key))
            .ok_or_else(|| AliasError::UnknownKey {
                key: key.to_owned(),
            })?;
        Ok(self.entries.remove(index))
    }
}

/// The game name `@@game_name@@` and `GAME` conditions use for one raw
/// activity name.
///
/// A raw name whose [`fold_name`] form equals a key's returns that entry's
/// target. Anything else, including blank input, is returned exactly as
/// given (not trimmed). Picking the majority game across a room is V5's job;
/// call this per activity before counting.
#[must_use]
pub fn resolve_game<'a>(raw_activity_name: &'a str, aliases: &'a AliasTable) -> &'a str {
    match aliases.get(raw_activity_name) {
        Some(entry) => &entry.target,
        None => raw_activity_name,
    }
}

/// Typed `/nick` refusals. None of them echo the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NickError {
    #[error("The name cannot be empty.")]
    Empty,
    #[error("The name is {chars} characters long; the maximum is {max}.")]
    TooLong { chars: usize, max: usize },
    #[error("The name cannot contain control characters or line breaks.")]
    ControlCharacter,
    #[error("The name cannot contain @everyone, @here or a mention.")]
    Mention,
}

/// A validated `/nick` name: trimmed, 1..=[`MAX_NICK_CHARS`] scalars, with no
/// refused control character and no mention syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nick(String);

impl Nick {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl AsRef<str> for Nick {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// What one `/nick` invocation asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NickUpdate {
    Set(Nick),
    /// Forget the stored name so `@@owner@@` shows the display name again.
    Reset,
}

/// `<@digits>`, `<@!digits>` (member) and `<@&digits>` (role) mentions.
fn contains_mention_syntax(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut start = 0;
    while let Some(offset) = text[start..].find("<@") {
        let mut index = start + offset + 2;
        if matches!(bytes.get(index), Some(b'!' | b'&')) {
            index += 1;
        }
        let digits_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index > digits_start && bytes.get(index) == Some(&b'>') {
            return true;
        }
        start += offset + 2;
    }
    false
}

fn contains_mass_mention(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("@everyone") || lower.contains("@here")
}

/// Validate a `/nick` name. Surrounding whitespace is trimmed; nothing else is
/// normalised. Refuses an empty result, more than [`MAX_NICK_CHARS`] scalars,
/// control characters (including every newline form and bidi overrides), and
/// `@everyone`, `@here` (any case) or `<@…>` mention syntax anywhere.
pub fn validate_nick(raw: &str) -> Result<Nick, NickError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(NickError::Empty);
    }
    let chars = trimmed.chars().count();
    if chars > MAX_NICK_CHARS {
        return Err(NickError::TooLong {
            chars,
            max: MAX_NICK_CHARS,
        });
    }
    if trimmed.chars().any(is_refused_control) {
        return Err(NickError::ControlCharacter);
    }
    if contains_mass_mention(trimmed) || contains_mention_syntax(trimmed) {
        return Err(NickError::Mention);
    }
    Ok(Nick(trimmed.to_owned()))
}

/// Parse the `/nick name|reset` argument. The keyword [`NICK_RESET_KEYWORD`]
/// (trimmed, ASCII case-insensitive) means [`NickUpdate::Reset`]; anything
/// else must pass [`validate_nick`]. A runtime with a separate reset
/// subcommand may construct `NickUpdate::Reset` directly.
pub fn parse_nick_command(raw: &str) -> Result<NickUpdate, NickError> {
    if raw.trim().eq_ignore_ascii_case(NICK_RESET_KEYWORD) {
        return Ok(NickUpdate::Reset);
    }
    validate_nick(raw).map(NickUpdate::Set)
}

/// The name `@@owner@@` shows: the member's `/nick` name when one is stored
/// and still valid, otherwise their display name. A stored value that no
/// longer passes [`validate_nick`] (for example, saved before a rule change)
/// is ignored rather than rendered.
#[must_use]
pub fn owner_display<'a>(nick: Option<&'a str>, display_name: &'a str) -> &'a str {
    match nick {
        Some(nick) if validate_nick(nick).is_ok() => nick.trim(),
        _ => display_name,
    }
}
