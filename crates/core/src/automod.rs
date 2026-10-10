//! Automod core: matcher, sanctions, env gates, and export validation.
//!
//! Slice 4 of TOG-9809. Ports the DB-free, framework-free heart of legacy
//! two-bot automod (`src/automod/matcher.ts`, `config.ts`, `types.ts`,
//! `rulesExport.ts`) as pure functions over plain data. The stateful service
//! (`service.ts`: idempotency claims, Discord deletion, sanction execution,
//! audit rows) stays with S6, which calls into this module; the repeat
//! tracker here is pure (explicit timestamps, no timers) so every filter is
//! unit-testable without Discord or a clock.
//!
//! Source files (legacy `two-bot`, frozen `main`):
//! - filters + order: `src/automod/matcher.ts` (`matchAutomod`,
//!   `MemoryRepeatTracker`) — `bad_words`, `repeated_message`, `mention_spam`,
//!   `invite_link`, `external_link`, `attachment_type`, in that order.
//! - policy + gates: `src/automod/config.ts` (`loadAutomodConfig`) —
//!   `TWO_AUTOMOD=1` enables, dry-run unless `TWO_AUTOMOD_ENFORCE=1`.
//! - export validation: `src/automod/rulesExport.ts` (`validateAutomodRules`,
//!   TOG-5700) — pure and offline.
//!
//! Automod needs both `TWO_AUTOMOD=1` (dry-run by default) and permission from
//! [`crate::activation::evaluate_activation`]. Bot boot evaluates this in
//! `activation::BootActivation`; a future automod runtime must consult its
//! `permitted(Automod)` before registering. No automod handler is wired in the
//! current bot composition, and automod is not cleared live.
//!
//! Deliberately out of scope: the inspect pipeline (claim/release/audit via
//! S6 stores), Discord deletion + sanction execution (twilight adapter),
//! containment/anti-nuke heat scoring (slice 5).

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use regex::Regex;
use unicode_normalization::UnicodeNormalization;
use url::Url;

/// Automod match reason, in legacy evaluation order (legacy `AutomodFilter`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AutomodFilter {
    BadWords,
    RepeatedMessage,
    MentionSpam,
    InviteLink,
    ExternalLink,
    AttachmentType,
}

impl AutomodFilter {
    /// Machine name (`bad_words`, … — legacy `AUTOMOD_FILTERS` values).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BadWords => "bad_words",
            Self::RepeatedMessage => "repeated_message",
            Self::MentionSpam => "mention_spam",
            Self::InviteLink => "invite_link",
            Self::ExternalLink => "external_link",
            Self::AttachmentType => "attachment_type",
        }
    }

    /// Inverse of [`Self::as_str`]: parses a persisted filter name back into
    /// the match reason. Returns `None` for unknown names so a stored
    /// decision can never become an unhandled match.
    #[must_use]
    pub fn from_persisted_name(s: &str) -> Option<Self> {
        match s {
            "bad_words" => Some(Self::BadWords),
            "repeated_message" => Some(Self::RepeatedMessage),
            "mention_spam" => Some(Self::MentionSpam),
            "invite_link" => Some(Self::InviteLink),
            "external_link" => Some(Self::ExternalLink),
            "attachment_type" => Some(Self::AttachmentType),
            _ => None,
        }
    }
}

impl std::fmt::Display for AutomodFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Sanction rung (legacy `AutomodSanction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutomodSanction {
    pub violations: u32,
    pub action: SanctionAction,
    pub timeout_seconds: Option<u64>,
}

/// Sanction verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SanctionAction {
    Delete,
    Warn,
    Timeout,
}

impl SanctionAction {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Delete => "delete",
            Self::Warn => "warn",
            Self::Timeout => "timeout",
        }
    }
}

/// Default ladder: 1 → delete, 2 → warn, 3 → timeout 600s (legacy
/// `DEFAULT_SANCTIONS`).
pub const DEFAULT_SANCTIONS: [AutomodSanction; 3] = [
    AutomodSanction {
        violations: 1,
        action: SanctionAction::Delete,
        timeout_seconds: None,
    },
    AutomodSanction {
        violations: 2,
        action: SanctionAction::Warn,
        timeout_seconds: None,
    },
    AutomodSanction {
        violations: 3,
        action: SanctionAction::Timeout,
        timeout_seconds: Some(600),
    },
];

/// Highest rung whose threshold is at or below `count` (legacy
/// `sanctionFor`; `count` is clamped to ≥1 by the caller in legacy, and to
/// ≥1 here as well so a dry-run count of 0 still names the first rung).
#[must_use]
pub fn sanction_for(count: u64, sanctions: &[AutomodSanction]) -> AutomodSanction {
    let count = count.max(1);
    let mut selected = sanctions.first().copied().unwrap_or(AutomodSanction {
        violations: 1,
        action: SanctionAction::Delete,
        timeout_seconds: None,
    });
    for sanction in sanctions {
        if u64::from(sanction.violations) <= count {
            selected = *sanction;
        }
    }
    selected
}

/// Static automod policy (legacy `AutomodPolicy`). Bad words are stored
/// normalized (NFKC + lowercase, empties dropped); attachment extensions are
/// lowercase with no leading dot; allowed domains are lowercase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomodPolicy {
    pub bad_words: Vec<String>,
    pub blocked_attachment_extensions: Vec<String>,
    pub allowed_domains: Vec<String>,
    pub repeated_message_count: u32,
    pub repeated_message_window_seconds: u64,
    pub mention_limit: usize,
    pub bypass_role_ids: HashSet<String>,
    pub exempt_channel_ids: HashSet<String>,
    pub sanctions: Vec<AutomodSanction>,
}

impl Default for AutomodPolicy {
    fn default() -> Self {
        Self {
            bad_words: Vec::new(),
            blocked_attachment_extensions: DEFAULT_BLOCKED_ATTACHMENTS
                .iter()
                .map(ToString::to_string)
                .collect(),
            allowed_domains: Vec::new(),
            repeated_message_count: 3,
            repeated_message_window_seconds: 30,
            mention_limit: 5,
            bypass_role_ids: HashSet::new(),
            exempt_channel_ids: HashSet::new(),
            sanctions: DEFAULT_SANCTIONS.to_vec(),
        }
    }
}

/// Default attachment blocklist (legacy `DEFAULT_BLOCKED_ATTACHMENTS`).
pub const DEFAULT_BLOCKED_ATTACHMENTS: [&str; 11] = [
    "bat", "cmd", "com", "exe", "js", "jse", "msi", "ps1", "scr", "vbs", "wsf",
];

/// A message under inspection (legacy `AutomodMessage`). `observed_timestamp`
/// is millis since the Unix epoch; only explicit mentions are supplied in
/// `mentioned_user_ids` (a reply reference without a ping does not count —
/// legacy `matcher.ts` comment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomodMessage {
    pub guild_id: String,
    pub channel_id: String,
    pub message_id: String,
    pub author_id: String,
    pub author_is_bot: bool,
    pub role_ids: Vec<String>,
    pub content: String,
    pub mentioned_user_ids: Vec<String>,
    pub attachment_names: Vec<String>,
    pub observed_timestamp_ms: u64,
}

impl AutomodPolicy {
    /// Load only the content restrictions used for channel names. Invalid
    /// chat counts, sanctions or exemptions must not discard configured words
    /// or domains. This is not a validated chat-moderation configuration.
    #[must_use]
    pub fn name_policy_from_map(vars: &HashMap<String, String>) -> Self {
        let get = |key: &str| vars.get(key).map(String::as_str);
        Self {
            bad_words: csv(get("TWO_AUTOMOD_BAD_WORDS"))
                .into_iter()
                .map(|word| normalize_content(&word))
                .filter(|word| !word.is_empty())
                .collect(),
            allowed_domains: csv(get("TWO_AUTOMOD_ALLOWED_DOMAINS"))
                .into_iter()
                .map(|domain| domain.to_lowercase())
                .collect(),
            ..Self::default()
        }
    }

    /// Service-level exemptions, pure and unit-testable (legacy
    /// `AutomodService.inspect` early returns): bots, exempt channels, and
    /// bypass-role holders are never inspected.
    #[must_use]
    pub fn is_exempt(&self, message: &AutomodMessage) -> bool {
        if message.author_is_bot {
            return true;
        }
        if self.exempt_channel_ids.contains(&message.channel_id) {
            return true;
        }
        if message
            .role_ids
            .iter()
            .any(|role| self.bypass_role_ids.contains(role))
        {
            return true;
        }
        false
    }
}

/// Zero-width / invisible characters legacy strips or skips: U+200B ZWSP,
/// U+200C ZWNJ, U+200D ZWJ, U+2060 word joiner, U+FEFF BOM (legacy
/// `ZERO_WIDTH` plus the inter-word `[\s​-‍⁠﻿]*` class).
const ZERO_WIDTH: [char; 5] = ['\u{200B}', '\u{200C}', '\u{200D}', '\u{2060}', '\u{FEFF}'];

/// Trailing punctuation stripped from URL candidates (legacy
/// `TRAILING_URL_PUNCTUATION`).
const TRAILING_URL_PUNCTUATION: [char; 8] = ['>', ')', ',', '.', '!', '?', ':', ';'];

/// Bare-domain TLD allowlist (legacy `BARE_DOMAIN_PATTERN` suffix).
const BARE_TLDS: [&str; 14] = [
    "app", "ca", "co", "com", "dev", "gg", "io", "me", "net", "org", "tv", "uk", "us", "xyz",
];

/// Filename stems that must not turn a dotted filename into a link verdict
/// (legacy `COMMON_FILENAME_STEMS`).
const COMMON_FILENAME_STEMS: [&str; 6] = [
    "changelog",
    "config",
    "license",
    "package",
    "readme",
    "tsconfig",
];

/// Normalize message text: NFKC, lowercase, collapse whitespace, trim
/// (legacy `normalize`).
#[must_use]
pub fn normalize_content(value: &str) -> String {
    let lowered: String = value.nfkc().collect::<String>().to_lowercase();
    lowered.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Normalize one configured bad word: NFKC + lowercase, then strip
/// zero-width characters and all whitespace (legacy `hasBadWord` word
/// preparation). Empty results never match.
fn normalize_bad_word(raw: &str) -> String {
    normalize_content(raw)
        .chars()
        .filter(|c| !ZERO_WIDTH.contains(c) && !c.is_whitespace())
        .collect()
}

/// Word character for boundary checks: Unicode letter/number or `_`
/// (legacy `[^\p{L}\p{N}_]` boundary class).
fn is_word_char(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

/// Separator legacy allows *between* bad-word characters: whitespace or a
/// zero-width character (legacy `[\s​-‍⁠﻿]*` join).
fn is_word_gap(c: char) -> bool {
    c.is_whitespace() || ZERO_WIDTH.contains(&c)
}

/// Literal bad-word match with legacy boundary semantics: each word character
/// must appear in order (gaps skipped), the character before the first and
/// after the last must not be a word character (or the string edge).
fn matches_bad_word(content: &[char], word: &[char]) -> bool {
    if word.is_empty() || word.len() > content.len() {
        return false;
    }
    for start in 0..content.len() {
        if start > 0 && is_word_char(content[start - 1]) {
            continue;
        }
        if content[start] != word[0] {
            continue;
        }
        let mut pos = start + 1;
        let mut ok = true;
        for wc in &word[1..] {
            while pos < content.len() && is_word_gap(content[pos]) {
                pos += 1;
            }
            if pos >= content.len() || content[pos] != *wc {
                ok = false;
                break;
            }
            pos += 1;
        }
        if ok && (pos >= content.len() || !is_word_char(content[pos])) {
            return true;
        }
    }
    false
}

fn has_bad_word(normalized: &str, words: &[String]) -> bool {
    let content: Vec<char> = normalized.chars().collect();
    words.iter().any(|raw| {
        let word: Vec<char> = normalize_bad_word(raw).chars().collect();
        !word.is_empty() && matches_bad_word(&content, &word)
    })
}

/// Pure repeat tracker (legacy `MemoryRepeatTracker` minus timers): explicit
/// timestamps and at most `repeated_message_count` rows per guild+author.
/// Only creates advance history. Updates inspect their bounded interval without
/// recording, replacing or expiring any CREATE observation.
///
/// Content identity is a non-cryptographic hash of the normalized text; the
/// legacy HMAC key only avoids storing message text, which this tracker
/// already avoids. Never persisted, never sent anywhere.
#[derive(Debug, Default)]
pub struct RepeatTracker {
    rows: HashMap<String, Vec<RepeatRow>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepeatObservation {
    Create,
    Revision,
    UnstampedUpdate,
}

#[derive(Debug, Clone)]
struct RepeatRow {
    message_id: String,
    digest: u64,
    at_ms: u64,
}

fn digest_content(normalized: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    normalized.hash(&mut hasher);
    hasher.finish()
}

impl RepeatTracker {
    /// Expire inactive authors as well as rows for the next observed author.
    /// The runtime calls this with the message/observation clock (never
    /// gateway receipt time); an adapter can also call it on its regular
    /// sweep so an idle guild retains no stale repeat history.
    pub fn expire(&mut self, now_ms: u64, window_seconds: u64) {
        let cutoff = now_ms.saturating_sub(window_seconds.saturating_mul(1000));
        self.rows.retain(|_, rows| {
            rows.retain(|row| row.at_ms >= cutoff);
            !rows.is_empty()
        });
    }

    /// Record one message; `true` when `repeated_message_count` identical
    /// messages from this author fall inside the window (legacy
    /// `MemoryRepeatTracker.observe`). Position matters: the caller must
    /// invoke this only after the bad-word check, so non-matching repeats
    /// still count (legacy `matchAutomod` order).
    pub fn observe(
        &mut self,
        message: &AutomodMessage,
        normalized: &str,
        policy: &AutomodPolicy,
    ) -> bool {
        self.observe_with_clock(message, normalized, policy, RepeatObservation::Create)
    }

    fn observe_with_clock(
        &mut self,
        message: &AutomodMessage,
        normalized: &str,
        policy: &AutomodPolicy,
        observation: RepeatObservation,
    ) -> bool {
        if normalized.is_empty() {
            return false;
        }
        let window_ms = policy.repeated_message_window_seconds.saturating_mul(1000);
        let cutoff = message.observed_timestamp_ms.saturating_sub(window_ms);
        let key = format!("{}:{}", message.guild_id, message.author_id);
        let digest = digest_content(normalized);
        let keep = policy.repeated_message_count.max(1) as usize;
        let matches_window = |rows: &[RepeatRow]| {
            // Include this revision once, then up to `keep` preceding rows
            // in its interval, matching legacy `MemoryRepeatTracker.observe`
            // (the new row plus up to `count` earlier rows).
            rows.iter()
                .rev()
                .filter(|row| {
                    row.message_id != message.message_id
                        && row.at_ms >= cutoff
                        && row.at_ms <= message.observed_timestamp_ms
                })
                .take(keep)
                .filter(|row| row.digest == digest)
                .count()
                + 1
                >= keep
        };
        if observation != RepeatObservation::Create {
            // REST may return a future revision while older CREATEs are queued.
            // Inspect edits against CREATE history without replacing a row or
            // inserting one that could evict history needed by that batch.
            return matches_window(self.rows.get(&key).map_or(&[], Vec::as_slice));
        }
        let rows = self.rows.entry(key).or_default();
        if rows.iter().any(|row| {
            row.message_id == message.message_id && row.at_ms > message.observed_timestamp_ms
        }) {
            // An older CREATE cannot replace a newer retained observation
            // of the same message.
            return matches_window(rows);
        }
        rows.retain(|row| row.message_id != message.message_id && row.at_ms >= cutoff);
        let matched = matches_window(rows);
        rows.push(RepeatRow {
            message_id: message.message_id.clone(),
            digest,
            at_ms: message.observed_timestamp_ms,
        });
        // Retain the newest CREATE observations by message clock, not arrival
        // order, after evaluating this message's bounded interval.
        rows.sort_by_key(|row| row.at_ms);
        if rows.len() > keep {
            rows.drain(..rows.len() - keep);
        }
        matched
    }
}

/// Evaluate one message against the policy in legacy filter order
/// (`bad_words` → `repeated_message` → `mention_spam` → `invite_link` →
/// `external_link` → `attachment_type`); exemptions are the caller's
/// responsibility via [`AutomodPolicy::is_exempt`]. `link_content` is the
/// normalized text with zero-width characters stripped (legacy
/// `matchAutomod`).
pub fn match_automod(
    message: &AutomodMessage,
    policy: &AutomodPolicy,
    repeats: &mut RepeatTracker,
) -> Option<AutomodFilter> {
    match_automod_with_clock(message, policy, repeats, RepeatObservation::Create)
}

pub(crate) fn match_automod_with_clock(
    message: &AutomodMessage,
    policy: &AutomodPolicy,
    repeats: &mut RepeatTracker,
    observation: RepeatObservation,
) -> Option<AutomodFilter> {
    let normalized = normalize_content(&message.content);
    let link_content: String = normalized
        .chars()
        .filter(|c| !ZERO_WIDTH.contains(c))
        .collect();
    if has_bad_word(&normalized, &policy.bad_words) {
        return Some(AutomodFilter::BadWords);
    }
    if repeats.observe_with_clock(message, &normalized, policy, observation) {
        return Some(AutomodFilter::RepeatedMessage);
    }
    if message.mentioned_user_ids.len() >= policy.mention_limit {
        return Some(AutomodFilter::MentionSpam);
    }
    if has_invite(&link_content) {
        return Some(AutomodFilter::InviteLink);
    }
    if has_external_link(&link_content, &policy.allowed_domains) {
        return Some(AutomodFilter::ExternalLink);
    }
    if has_blocked_attachment(
        &message.attachment_names,
        &policy.blocked_attachment_extensions,
    ) {
        return Some(AutomodFilter::AttachmentType);
    }
    None
}

/// Discord invite links: `discord.gg/<code>`, `discord.com/invite/<code>`,
/// `discordapp.com/invite/<code>`, each with optional scheme/`www.` prefix
/// (legacy `INVITE`). Content is already lowercased.
fn has_invite(content: &str) -> bool {
    for marker in [
        "discord.gg/",
        "discord.com/invite/",
        "discordapp.com/invite/",
    ] {
        let mut rest = content;
        while let Some(idx) = rest.find(marker) {
            let after = &rest[idx + marker.len()..];
            let code_len: usize = after
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .map(char::len_utf8)
                .sum();
            if code_len > 0 {
                return true;
            }
            rest = after;
        }
    }
    false
}

fn host_allowed(host: &str, allowed: &[String]) -> bool {
    allowed
        .iter()
        .any(|domain| host == domain || host.ends_with(&format!(".{domain}")))
}

/// Unanchored explicit and bare-domain scans (legacy `hasExternalLink`).
/// Preserve the legacy TLD alternation order and lack of a right boundary;
/// URL parsing supplies host canonicalization, ignoring query/path/userinfo.
fn has_external_link(content: &str, allowed: &[String]) -> bool {
    static EXPLICIT: OnceLock<Regex> = OnceLock::new();
    static BARE: OnceLock<Regex> = OnceLock::new();
    static BARE_LEFT_EXCLUSION: OnceLock<Regex> = OnceLock::new();
    let explicit = EXPLICIT.get_or_init(|| {
        Regex::new(r"(?iu)(?:https?://|www\.)[^\s<]+").expect("static explicit URL pattern")
    });
    let bare = BARE.get_or_init(|| {
        Regex::new(&format!(
            r"(?iu)(?:[\p{{L}}\p{{N}}](?:[\p{{L}}\p{{N}}-]{{0,61}}[\p{{L}}\p{{N}}])?\.)+(?:{})(?:/[^\s<]*)?",
            BARE_TLDS.join("|")
        ))
        .expect("static bare-domain pattern")
    });
    let excluded = BARE_LEFT_EXCLUSION.get_or_init(|| {
        Regex::new(r"[\p{L}\p{N}@._/\\-]").expect("static bare-domain left exclusion")
    });
    for (pattern, is_bare) in [(explicit, false), (bare, true)] {
        let mut offset = 0;
        while let Some(found) = pattern.find_at(content, offset) {
            // Rust regex has no lookbehind. On a rejected left boundary,
            // advance one character, not the entire match: its optional path
            // can contain a later candidate with a valid boundary.
            if is_bare
                && content[..found.start()]
                    .char_indices()
                    .next_back()
                    .is_some_and(|(at, _)| excluded.is_match(&content[at..found.start()]))
            {
                offset = found.start()
                    + content[found.start()..]
                        .chars()
                        .next()
                        .expect("nonempty match")
                        .len_utf8();
                continue;
            }
            offset = found.end();
            let candidate = found.as_str().trim_end_matches(TRAILING_URL_PUNCTUATION);
            let parsed = if candidate.starts_with("http://") || candidate.starts_with("https://") {
                Url::parse(candidate)
            } else {
                Url::parse(&format!("https://{candidate}"))
            };
            let Ok(parsed) = parsed else { return true };
            let Some(host) = parsed.host_str() else {
                return true;
            };
            let host = host.strip_prefix("www.").unwrap_or(host);
            if is_bare
                && host.rsplit_once('.').is_some_and(|(labels, _)| {
                    labels
                        .split('.')
                        .all(|label| COMMON_FILENAME_STEMS.contains(&label))
                })
            {
                continue;
            }
            if !host_allowed(host, allowed) {
                return true;
            }
        }
    }
    false
}

fn has_blocked_attachment(names: &[String], blocked: &[String]) -> bool {
    names.iter().any(|name| {
        let lowered = name.to_lowercase();
        let extension = lowered.rsplit('.').next().unwrap_or("");
        !extension.is_empty() && blocked.iter().any(|b| b == extension)
    })
}

/// Automod env gates (legacy `loadAutomodConfig` → `{enabled, dryRun,
/// policy}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomodConfig {
    /// `TWO_AUTOMOD=1` — inspect + enforce automod.
    pub enabled: bool,
    /// `true` unless `TWO_AUTOMOD_ENFORCE=1` — record only, mutate nothing.
    pub dry_run: bool,
    pub policy: AutomodPolicy,
}

/// Invalid automod-gate environment. Messages mirror the legacy throws.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AutomodGateError {
    #[error("{0} must be an integer between {1} and {2}.")]
    InvalidInteger(&'static str, u64, u64),
    #[error("{0} must contain Discord ids.")]
    InvalidSnowflakes(&'static str),
    #[error("TWO_AUTOMOD_SANCTIONS thresholds must be integers between 1 and 100.")]
    InvalidSanctionThreshold,
    #[error("TWO_AUTOMOD_SANCTIONS actions must be delete, warn, or timeout.")]
    InvalidSanctionAction,
    #[error("TWO_AUTOMOD_SANCTIONS must start at violation 1.")]
    SanctionsMustStartAtOne,
    #[error("TWO_AUTOMOD_SANCTIONS thresholds must be unique.")]
    DuplicateSanctionThreshold,
    #[error("automod timeout seconds must be an integer between 60 and 2419200.")]
    InvalidSanctionTimeout,
}

fn is_snowflake(value: &str) -> bool {
    (17..=20).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit())
}

fn csv(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn integer(
    value: Option<&str>,
    fallback: u64,
    min: u64,
    max: u64,
    name: &'static str,
) -> Result<u64, AutomodGateError> {
    let parsed = match value {
        None | Some("") => fallback,
        Some(raw) => raw
            .parse::<u64>()
            .ok()
            .filter(|n| (*n >= min) && (*n <= max))
            .ok_or(AutomodGateError::InvalidInteger(name, min, max))?,
    };
    Ok(parsed)
}

fn snowflakes(
    value: Option<&str>,
    name: &'static str,
) -> Result<HashSet<String>, AutomodGateError> {
    let mut ids = HashSet::new();
    for id in csv(value) {
        if !is_snowflake(&id) {
            return Err(AutomodGateError::InvalidSnowflakes(name));
        }
        ids.insert(id);
    }
    Ok(ids)
}

const MAX_TIMEOUT_SECONDS: u64 = 28 * 24 * 60 * 60;

/// Parse one JSON-form sanction rung — the shape the configuration reference
/// renders for the parsed default (e.g.
/// `{"violations":1,"action":"delete","timeout_seconds":null}`). Ranges mirror
/// the CSV form: a `timeout` rung without a usable value defaults to 600 s,
/// any other out-of-range value fails with the matching CSV error.
fn parse_json_sanction(value: &serde_json::Value) -> Result<AutomodSanction, AutomodGateError> {
    let violations: u32 = value
        .get("violations")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| (1..=100).contains(n))
        .ok_or(AutomodGateError::InvalidSanctionThreshold)?;
    let action = match value.get("action").and_then(serde_json::Value::as_str) {
        Some("delete") => SanctionAction::Delete,
        Some("warn") => SanctionAction::Warn,
        Some("timeout") => SanctionAction::Timeout,
        _ => return Err(AutomodGateError::InvalidSanctionAction),
    };
    let timeout_seconds = if action == SanctionAction::Timeout {
        match value.get("timeout_seconds") {
            None | Some(serde_json::Value::Null) => Some(600),
            Some(raw) => Some(
                raw.as_u64()
                    .filter(|n| (60..=MAX_TIMEOUT_SECONDS).contains(n))
                    .ok_or(AutomodGateError::InvalidSanctionTimeout)?,
            ),
        }
    } else {
        None
    };
    Ok(AutomodSanction {
        violations,
        action,
        timeout_seconds,
    })
}

/// Shared ladder validation: sort by threshold, then require a rung at
/// violation 1 with unique thresholds.
fn finish_sanctions(
    mut sanctions: Vec<AutomodSanction>,
) -> Result<Vec<AutomodSanction>, AutomodGateError> {
    sanctions.sort_by_key(|s| s.violations);
    if sanctions.first().is_none_or(|s| s.violations != 1) {
        return Err(AutomodGateError::SanctionsMustStartAtOne);
    }
    for window in sanctions.windows(2) {
        if window[0].violations == window[1].violations {
            return Err(AutomodGateError::DuplicateSanctionThreshold);
        }
    }
    Ok(sanctions)
}

fn parse_sanctions(value: Option<&str>) -> Result<Vec<AutomodSanction>, AutomodGateError> {
    let raw = value.map(str::trim).unwrap_or("");
    if raw.is_empty() {
        return Ok(DEFAULT_SANCTIONS.to_vec());
    }
    if raw.starts_with('[') {
        let parsed: Vec<serde_json::Value> =
            serde_json::from_str(raw).map_err(|_| AutomodGateError::InvalidSanctionThreshold)?;
        return finish_sanctions(
            parsed
                .iter()
                .map(parse_json_sanction)
                .collect::<Result<_, _>>()?,
        );
    }
    let mut sanctions = Vec::new();
    for part in raw.split(',') {
        let mut pieces = part.trim().split(':');
        let violations: u32 = pieces
            .next()
            .and_then(|s| s.parse().ok())
            .filter(|n| (1..=100).contains(n))
            .ok_or(AutomodGateError::InvalidSanctionThreshold)?;
        let action = match pieces.next() {
            Some("delete") => SanctionAction::Delete,
            Some("warn") => SanctionAction::Warn,
            Some("timeout") => SanctionAction::Timeout,
            _ => return Err(AutomodGateError::InvalidSanctionAction),
        };
        let timeout_seconds = if action == SanctionAction::Timeout {
            Some(
                pieces
                    .next()
                    .map(|s| {
                        s.parse::<u64>()
                            .ok()
                            .filter(|n| (60..=MAX_TIMEOUT_SECONDS).contains(n))
                    })
                    .unwrap_or(Some(600))
                    .ok_or(AutomodGateError::InvalidSanctionTimeout)?,
            )
        } else {
            None
        };
        sanctions.push(AutomodSanction {
            violations,
            action,
            timeout_seconds,
        });
    }
    finish_sanctions(sanctions)
}

impl AutomodConfig {
    /// Read gates from the process environment.
    pub fn from_env() -> Result<Self, AutomodGateError> {
        Self::from_map(&std::env::vars().collect())
    }

    /// Read gates from an explicit map (tests, staged config).
    pub fn from_map(vars: &HashMap<String, String>) -> Result<Self, AutomodGateError> {
        let get = |key: &str| vars.get(key).map(String::as_str);
        let policy = AutomodPolicy {
            blocked_attachment_extensions: {
                let raw = get("TWO_AUTOMOD_BLOCKED_ATTACHMENT_EXTENSIONS");
                let list = if raw.is_none_or(|s| s.is_empty()) {
                    DEFAULT_BLOCKED_ATTACHMENTS
                        .iter()
                        .map(ToString::to_string)
                        .collect()
                } else {
                    csv(raw)
                };
                list.into_iter()
                    .map(|v| v.to_lowercase().trim_start_matches('.').to_owned())
                    .collect()
            },
            repeated_message_count: integer(
                get("TWO_AUTOMOD_REPEAT_COUNT"),
                3,
                2,
                20,
                "TWO_AUTOMOD_REPEAT_COUNT",
            )? as u32,
            repeated_message_window_seconds: integer(
                get("TWO_AUTOMOD_REPEAT_WINDOW_SECONDS"),
                30,
                1,
                3600,
                "TWO_AUTOMOD_REPEAT_WINDOW_SECONDS",
            )?,
            mention_limit: integer(
                get("TWO_AUTOMOD_MENTION_LIMIT"),
                5,
                1,
                50,
                "TWO_AUTOMOD_MENTION_LIMIT",
            )? as usize,
            bypass_role_ids: snowflakes(
                get("TWO_AUTOMOD_BYPASS_ROLE_IDS"),
                "TWO_AUTOMOD_BYPASS_ROLE_IDS",
            )?,
            exempt_channel_ids: snowflakes(
                get("TWO_AUTOMOD_EXEMPT_CHANNEL_IDS"),
                "TWO_AUTOMOD_EXEMPT_CHANNEL_IDS",
            )?,
            sanctions: parse_sanctions(get("TWO_AUTOMOD_SANCTIONS"))?,
            ..AutomodPolicy::name_policy_from_map(vars)
        };
        Ok(Self {
            enabled: vars.get("TWO_AUTOMOD").is_some_and(|v| v == "1"),
            dry_run: vars.get("TWO_AUTOMOD_ENFORCE").is_none_or(|v| v != "1"),
            policy,
        })
    }
}

/// One validated export rule: `id` + `name` are required, everything else
/// passes through (legacy `AutomodExportRule`, whose index signature keeps
/// every extra field). `raw` is the rule object unchanged, so a validated
/// export written back out is byte-faithful to what Discord returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomodExportRule {
    pub id: String,
    pub name: String,
    pub raw: serde_json::Value,
}

/// Unusable export payload (legacy `AutomodExportError`): every bad row, not
/// just the first, so a 50-rule export does not take 50 runs to fix.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("AutoMod export is not usable:\n  {}", problems.join("\n  "))]
pub struct AutomodExportError {
    pub problems: Vec<String>,
}

/// Refuse a payload that is not a list of identifiable rules; return the
/// rules unchanged when all are usable (legacy `validateAutomodRules`).
pub fn validate_automod_rules(
    payload: &serde_json::Value,
) -> Result<Vec<AutomodExportRule>, AutomodExportError> {
    let rows = payload.as_array().ok_or_else(|| AutomodExportError {
        problems: vec!["export must be an array of rules".to_owned()],
    })?;
    let mut problems = Vec::new();
    let mut out = Vec::new();
    for (index, raw) in rows.iter().enumerate() {
        let at = format!("row {}", index + 1);
        let rule = match raw.as_object() {
            Some(rule) => rule,
            None => {
                problems.push(format!("{at} is not an object"));
                continue;
            }
        };
        let id_ok = rule
            .get("id")
            .and_then(serde_json::Value::as_str)
            .is_some_and(is_snowflake);
        if !id_ok {
            problems.push(format!(
                "{at} has an invalid Discord rule id: {}",
                rule.get("id")
                    .map_or("missing".to_owned(), |v| v.to_string())
            ));
            continue;
        }
        let name_ok = rule
            .get("name")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|n| !n.trim().is_empty());
        if !name_ok {
            problems.push(format!("{at} has no name"));
            continue;
        }
        out.push(AutomodExportRule {
            id: rule["id"].as_str().expect("checked").to_owned(),
            name: rule["name"].as_str().expect("checked").to_owned(),
            raw: raw.clone(),
        });
    }
    if problems.is_empty() {
        Ok(out)
    } else {
        Err(AutomodExportError { problems })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(content: &str) -> AutomodMessage {
        AutomodMessage {
            guild_id: "111111111111111111".to_owned(),
            channel_id: "222222222222222222".to_owned(),
            message_id: "333333333333333333".to_owned(),
            author_id: "444444444444444444".to_owned(),
            author_is_bot: false,
            role_ids: vec![],
            content: content.to_owned(),
            mentioned_user_ids: vec![],
            attachment_names: vec![],
            observed_timestamp_ms: 1_000_000,
        }
    }

    fn policy() -> AutomodPolicy {
        AutomodPolicy {
            bad_words: vec!["spamword".to_owned()],
            ..AutomodPolicy::default()
        }
    }

    fn check(content: &str, policy: &AutomodPolicy) -> Option<AutomodFilter> {
        match_automod(&message(content), policy, &mut RepeatTracker::default())
    }

    #[test]
    fn normalize_handles_fullwidth_and_whitespace() {
        assert_eq!(normalize_content("Ｈｅｌｌｏ  　world"), "hello world");
        assert_eq!(normalize_content("  a\n\tb  "), "a b");
    }

    #[test]
    fn bad_words_respect_boundaries_and_evasion() {
        let policy = policy();
        assert_eq!(
            check("this is spamword here", &policy),
            Some(AutomodFilter::BadWords)
        );
        // Substring of a longer word does not match.
        assert_eq!(check("this is spamwordish", &policy), None);
        assert_eq!(
            check("as spamword!", &policy),
            Some(AutomodFilter::BadWords)
        );
        // Evasion: zero-width joiners and spaces between letters still match.
        assert_eq!(
            check("s\u{200B}p\u{200C}am\u{200D}word", &policy),
            Some(AutomodFilter::BadWords)
        );
        assert_eq!(
            check("s p a m w o r d", &policy),
            Some(AutomodFilter::BadWords)
        );
        // Full-width input normalizes before matching.
        assert_eq!(
            check("ＳＰＡＭＷＯＲＤ", &policy),
            Some(AutomodFilter::BadWords)
        );
        // Clean text passes all filters.
        assert_eq!(check("hello world, good game tonight", &policy), None);
    }

    #[test]
    fn bad_words_reject_punctuation_separators() {
        // Legacy only allows whitespace and zero-width characters between
        // word letters (`is_word_gap`): punctuation breaks the word, so these
        // must not match even though the spaced variant above does.
        let policy = policy();
        for content in [
            "s.p.a.m.w.o.r.d",
            "s-p-a-m-w-o-r-d",
            "s/p/a/m/w/o/r/d",
            "buy s,p,a,m,w,o,r,d now",
        ] {
            assert_eq!(check(content, &policy), None, "{content:?}");
        }
        // Non-ASCII letters are word characters too (`is_word_char` is
        // Unicode-aware), so they close the boundary on either side.
        assert_eq!(check("éspamword", &policy), None);
        assert_eq!(check("spamwordé", &policy), None);
    }

    #[test]
    fn filter_order_bad_words_first() {
        // A message that is both an invite and a bad word reports bad_words.
        let policy = policy();
        assert_eq!(
            check("spamword https://discord.gg/abc123", &policy),
            Some(AutomodFilter::BadWords)
        );
    }

    #[test]
    fn repeats_count_identical_messages_in_window() {
        let policy = policy();
        let mut repeats = RepeatTracker::default();
        let mut msg = message("same text here");
        msg.message_id = "100000000000000001".to_owned();
        msg.observed_timestamp_ms = 1_000_000;
        assert_eq!(match_automod(&msg, &policy, &mut repeats), None);
        msg.message_id = "100000000000000002".to_owned();
        assert_eq!(match_automod(&msg, &policy, &mut repeats), None);
        // Third identical message trips the default count of 3.
        msg.message_id = "100000000000000003".to_owned();
        assert_eq!(
            match_automod(&msg, &policy, &mut repeats),
            Some(AutomodFilter::RepeatedMessage)
        );
        // Different text does not trip.
        let mut other = message("entirely different words");
        other.message_id = "100000000000000004".to_owned();
        assert_eq!(match_automod(&other, &policy, &mut repeats), None);
        // Outside the 30s window the streak resets.
        msg.message_id = "100000000000000005".to_owned();
        msg.observed_timestamp_ms = 1_000_000 + 31_000;
        assert_eq!(match_automod(&msg, &policy, &mut repeats), None);
    }

    #[test]
    fn repeat_sweep_expires_inactive_authors_without_another_message() {
        let policy = policy();
        let mut repeats = RepeatTracker::default();
        let msg = message("same text");
        repeats.observe(&msg, "same text", &policy);
        let mut other = msg.clone();
        other.author_id = "other".to_owned();
        repeats.observe(&other, "same text", &policy);
        assert_eq!(repeats.rows.len(), 2);
        repeats.expire(msg.observed_timestamp_ms + 30_000, 30);
        assert_eq!(repeats.rows.len(), 2);
        repeats.expire(msg.observed_timestamp_ms + 30_001, 30);
        assert!(repeats.rows.is_empty());
    }

    #[test]
    fn mention_spam_uses_explicit_mentions_only() {
        let mut msg = message("hey everyone");
        let policy = policy();
        msg.mentioned_user_ids = (0..5).map(|i| format!("9000000000000000{i:02}")).collect();
        assert_eq!(
            match_automod(&msg, &policy, &mut RepeatTracker::default()),
            Some(AutomodFilter::MentionSpam)
        );
        msg.mentioned_user_ids.pop();
        assert_eq!(
            match_automod(&msg, &policy, &mut RepeatTracker::default()),
            None
        );
    }

    #[test]
    fn invite_links_detected() {
        let policy = AutomodPolicy::default();
        assert_eq!(
            check("join https://discord.gg/abc123 now", &policy),
            Some(AutomodFilter::InviteLink)
        );
        assert_eq!(
            check("see discord.com/invite/xyz789 ok", &policy),
            Some(AutomodFilter::InviteLink)
        );
        // A bare `discord.gg` with no invite code is not an invite — but the
        // bare-domain rule still flags it as an external link (legacy parity).
        assert_eq!(
            check("plain discord.gg with no code", &policy),
            Some(AutomodFilter::ExternalLink)
        );
        assert_eq!(check("plain discord with no code", &policy), None);
    }

    #[test]
    fn external_links_checked_against_allowlist() {
        let policy = AutomodPolicy {
            allowed_domains: vec!["two.gg".to_owned()],
            ..AutomodPolicy::default()
        };
        assert_eq!(check("read https://two.gg/news today", &policy), None);
        assert_eq!(check("read https://sub.two.gg/news today", &policy), None);
        assert_eq!(
            check("visit https://evil.example.com now", &policy),
            Some(AutomodFilter::ExternalLink)
        );
        // Bare domains count; dotted filenames do not.
        assert_eq!(
            check("see evil.example.com ok", &policy),
            Some(AutomodFilter::ExternalLink)
        );
        assert_eq!(check("open package.json diff", &policy), None);
        assert_eq!(
            check("bad site badexample.io!", &policy),
            Some(AutomodFilter::ExternalLink)
        );
    }

    #[test]
    fn external_links_scan_inside_text_and_punctuation() {
        for content in [
            "xhttps://evil.com",
            "go(https://evil.com)",
            "(evil.com)",
            "see,evil.com",
            "xwww.evil.com",
            "🦀https://evil.com",
            "see,évil.gg",
            "(evil.com).",
            "evil.gg/path",
            "foo@ignored.gg/path(other.gg)",
        ] {
            assert_eq!(
                check(content, &AutomodPolicy::default()),
                Some(AutomodFilter::ExternalLink),
                "{content}"
            );
        }
    }

    #[test]
    fn external_links_preserve_allowlisted_hosts_and_boundaries() {
        let policy = AutomodPolicy {
            allowed_domains: vec!["two.gg".to_owned()],
            ..AutomodPolicy::default()
        };
        for content in [
            "xhttps://two.gg/news",
            "go(https://sub.two.gg/news)",
            "xwww.two.gg",
            "(two.gg)",
            "see,two.gg",
            "https://two.gg?q=hello",
            "https://two.gg#hello",
            "https://two.gg:443/news",
            "email@evil.gg",
            "foo/evil.gg",
            r"foo\evil.gg",
            "foo_evil.gg",
            "-evil.gg",
            ".evil.gg",
            "package.json",
            "readme.md",
            "config.app",
            "config.package.app",
        ] {
            assert_eq!(check(content, &policy), None, "{content}");
        }
        for content in [
            "https://two.gg.evil.gg/news",
            "https://two.gg@evil.gg/news",
            "two.gg https://evil.gg",
            "https://two.gg/news see,evil.gg",
            "config.evil.app",
            "évil.gg",
        ] {
            assert_eq!(
                check(content, &policy),
                Some(AutomodFilter::ExternalLink),
                "{content}"
            );
        }
    }

    #[test]
    fn external_links_use_url_hostname_canonicalization() {
        let policy = AutomodPolicy {
            allowed_domains: vec!["xn--vil-9la.gg".to_owned()],
            ..AutomodPolicy::default()
        };
        assert_eq!(check("(évil.gg)", &policy), None);
        assert_eq!(check("https://évil.gg/news", &policy), None);
        assert_eq!(
            check("https://[invalid]", &policy),
            Some(AutomodFilter::ExternalLink)
        );
    }

    #[test]
    fn external_links_preserve_legacy_tld_matching() {
        let policy = AutomodPolicy::default();
        for content in ["evil.appsuffix", "evil.gg.", "evil.app.appsuffix"] {
            assert_eq!(
                check(content, &policy),
                Some(AutomodFilter::ExternalLink),
                "{content}"
            );
        }
        let allowed = AutomodPolicy {
            allowed_domains: vec!["evil.co".to_owned()],
            ..policy
        };
        // The legacy alternation tries `co` before `com`, without a right boundary.
        assert_eq!(check("evil.com", &allowed), None);
    }

    #[test]
    fn blocked_attachments_by_extension() {
        let policy = AutomodPolicy::default();
        let mut msg = message("see attached");
        msg.attachment_names = vec!["Setup.EXE".to_owned()];
        assert_eq!(
            match_automod(&msg, &policy, &mut RepeatTracker::default()),
            Some(AutomodFilter::AttachmentType)
        );
        msg.attachment_names = vec!["photo.png".to_owned()];
        assert_eq!(
            match_automod(&msg, &policy, &mut RepeatTracker::default()),
            None
        );
    }

    #[test]
    fn exemptions_skip_inspection() {
        let policy = AutomodPolicy {
            exempt_channel_ids: HashSet::from(["222222222222222222".to_owned()]),
            bypass_role_ids: HashSet::from(["555555555555555555".to_owned()]),
            ..policy()
        };
        assert!(policy.is_exempt(&message("spamword here")));
        let mut bot = message("spamword here");
        bot.author_is_bot = true;
        assert!(policy.is_exempt(&bot));
        let mut role = message("spamword here");
        role.channel_id = "999999999999999999".to_owned();
        role.role_ids = vec!["555555555555555555".to_owned()];
        assert!(policy.is_exempt(&role));
        role.role_ids = vec![];
        assert!(!policy.is_exempt(&role));
    }

    #[test]
    fn sanction_selection_picks_highest_rung() {
        let sanctions = DEFAULT_SANCTIONS.to_vec();
        assert_eq!(sanction_for(0, &sanctions).action, SanctionAction::Delete);
        assert_eq!(sanction_for(1, &sanctions).action, SanctionAction::Delete);
        assert_eq!(sanction_for(2, &sanctions).action, SanctionAction::Warn);
        assert_eq!(sanction_for(99, &sanctions).action, SanctionAction::Timeout);
        assert_eq!(sanction_for(99, &sanctions).timeout_seconds, Some(600));
    }

    #[test]
    fn sanction_parsing_matches_legacy() {
        assert_eq!(
            parse_sanctions(None).expect("defaults"),
            DEFAULT_SANCTIONS.to_vec()
        );
        let parsed = parse_sanctions(Some("1:delete,2:timeout:300")).expect("parses");
        assert_eq!(parsed[1].timeout_seconds, Some(300));
        assert_eq!(
            parse_sanctions(Some("2:warn")),
            Err(AutomodGateError::SanctionsMustStartAtOne)
        );
        assert_eq!(
            parse_sanctions(Some("1:delete,1:warn")),
            Err(AutomodGateError::DuplicateSanctionThreshold)
        );
        assert_eq!(
            parse_sanctions(Some("1:banhammer")),
            Err(AutomodGateError::InvalidSanctionAction)
        );
        assert_eq!(
            parse_sanctions(Some("0:delete")),
            Err(AutomodGateError::InvalidSanctionThreshold)
        );
        assert_eq!(
            parse_sanctions(Some("1:timeout:30")),
            Err(AutomodGateError::InvalidSanctionTimeout)
        );
    }

    #[test]
    fn sanction_parsing_accepts_the_reference_json_default() {
        let rendered = r#"[{"violations":1,"action":"delete","timeout_seconds":null},{"violations":2,"action":"warn","timeout_seconds":null},{"violations":3,"action":"timeout","timeout_seconds":600}]"#;
        assert_eq!(
            parse_sanctions(Some(rendered)).expect("json parses"),
            parse_sanctions(Some("1:delete,2:warn,3:timeout:600")).expect("csv parses")
        );
        assert_eq!(
            parse_sanctions(Some(
                r#"[{"violations":1,"action":"banhammer","timeout_seconds":null}]"#
            )),
            Err(AutomodGateError::InvalidSanctionAction)
        );
        assert_eq!(
            parse_sanctions(Some(
                r#"[{"violations":0,"action":"delete","timeout_seconds":null}]"#
            )),
            Err(AutomodGateError::InvalidSanctionThreshold)
        );
        assert_eq!(
            parse_sanctions(Some(
                r#"[{"violations":2,"action":"delete","timeout_seconds":null}]"#
            )),
            Err(AutomodGateError::SanctionsMustStartAtOne)
        );
        assert_eq!(
            parse_sanctions(Some(
                r#"[{"violations":1,"action":"timeout","timeout_seconds":30}]"#
            )),
            Err(AutomodGateError::InvalidSanctionTimeout)
        );
        assert_eq!(
            parse_sanctions(Some(
                r#"[{"violations":1,"action":"timeout","timeout_seconds":null}]"#
            ))
            .expect("null timeout defaults")[0]
                .timeout_seconds,
            Some(600)
        );
    }

    #[test]
    fn sanction_parsing_accepts_a_dashboard_stored_real_array() {
        // The dashboard may store the ladder as a real JSON array rather than
        // a string. The live snapshot renders stored values through
        // `settings::to_env_string`, which must hand the parser the JSON
        // shape instead of comma-joined objects.
        let stored = serde_json::json!([
            {"violations": 1, "action": "delete", "timeout_seconds": null},
            {"violations": 2, "action": "warn", "timeout_seconds": null},
            {"violations": 3, "action": "timeout", "timeout_seconds": 600},
        ]);
        let rendered = crate::settings::to_env_string(&stored).expect("renders");
        assert_eq!(
            parse_sanctions(Some(&rendered)).expect("stored array parses"),
            parse_sanctions(Some("1:delete,2:warn,3:timeout:600")).expect("csv parses")
        );
    }

    #[test]
    fn name_policy_preserves_normalized_content_rules_when_chat_config_is_invalid() {
        let vars: HashMap<String, String> = [
            ("TWO_AUTOMOD", "0"),
            (
                "TWO_AUTOMOD_BAD_WORDS",
                " ＢＬＯＲＰ , blocked   phrase , , ",
            ),
            ("TWO_AUTOMOD_ALLOWED_DOMAINS", " Trusted.GG , , "),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
        let expected = AutomodPolicy::name_policy_from_map(&vars);
        assert_eq!(expected.bad_words, ["blorp", "blocked phrase"]);
        assert_eq!(expected.allowed_domains, ["trusted.gg"]);
        let validated = AutomodConfig::from_map(&vars).unwrap();
        assert_eq!(validated.policy.bad_words, expected.bad_words);
        assert_eq!(validated.policy.allowed_domains, expected.allowed_domains);
        for (key, value) in [
            ("TWO_AUTOMOD_REPEAT_COUNT", "21"),
            ("TWO_AUTOMOD_REPEAT_WINDOW_SECONDS", "0"),
            ("TWO_AUTOMOD_MENTION_LIMIT", "0"),
            ("TWO_AUTOMOD_SANCTIONS", "1:banhammer"),
            ("TWO_AUTOMOD_BYPASS_ROLE_IDS", "nope"),
            ("TWO_AUTOMOD_EXEMPT_CHANNEL_IDS", "nope"),
        ] {
            let mut invalid = vars.clone();
            invalid.insert(key.to_owned(), value.to_owned());
            assert!(AutomodConfig::from_map(&invalid).is_err(), "{key}");
            assert_eq!(
                AutomodPolicy::name_policy_from_map(&invalid),
                expected,
                "{key}"
            );
        }
    }

    #[test]
    fn gates_default_to_disabled_dry_run() {
        let config = AutomodConfig::from_map(&HashMap::new()).expect("defaults");
        assert!(!config.enabled);
        assert!(config.dry_run);
        assert_eq!(config.policy.repeated_message_count, 3);
        assert_eq!(config.policy.repeated_message_window_seconds, 30);
        assert_eq!(config.policy.mention_limit, 5);
        assert_eq!(config.policy.sanctions, DEFAULT_SANCTIONS.to_vec());
    }

    #[test]
    fn gates_enable_and_validate() {
        let vars: HashMap<String, String> = [
            ("TWO_AUTOMOD", "1"),
            ("TWO_AUTOMOD_ENFORCE", "1"),
            ("TWO_AUTOMOD_BAD_WORDS", "SpamWord, ,junk"),
            ("TWO_AUTOMOD_ALLOWED_DOMAINS", "Two.GG"),
            ("TWO_AUTOMOD_REPEAT_COUNT", "4"),
            ("TWO_AUTOMOD_BYPASS_ROLE_IDS", "555555555555555555"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
        let config = AutomodConfig::from_map(&vars).expect("parses");
        assert!(config.enabled);
        assert!(!config.dry_run);
        assert_eq!(config.policy.bad_words, vec!["spamword", "junk"]);
        assert_eq!(config.policy.allowed_domains, vec!["two.gg"]);
        assert_eq!(config.policy.repeated_message_count, 4);
        assert!(config.policy.bypass_role_ids.contains("555555555555555555"));

        let vars: HashMap<String, String> = [("TWO_AUTOMOD_REPEAT_COUNT", "21")]
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        assert_eq!(
            AutomodConfig::from_map(&vars),
            Err(AutomodGateError::InvalidInteger(
                "TWO_AUTOMOD_REPEAT_COUNT",
                2,
                20
            ))
        );
        let vars: HashMap<String, String> = [("TWO_AUTOMOD_BYPASS_ROLE_IDS", "nope")]
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        assert_eq!(
            AutomodConfig::from_map(&vars),
            Err(AutomodGateError::InvalidSnowflakes(
                "TWO_AUTOMOD_BYPASS_ROLE_IDS"
            ))
        );
    }

    #[test]
    fn export_validation_collects_all_problems() {
        let payload = serde_json::json!([
            {"id": "123456789012345678", "name": "spam rule", "extra": true},
            {"id": "bad", "name": "broken id"},
            {"name": "missing id"},
            "not an object",
        ]);
        let err = validate_automod_rules(&payload).expect_err("must fail");
        assert_eq!(err.problems.len(), 3);
        assert!(err.problems[0].contains("row 2"));
        assert!(err.problems[2].contains("row 4 is not an object"));
        let ok = serde_json::json!([{"id": "123456789012345678", "name": "spam rule"}]);
        let rules = validate_automod_rules(&ok).expect("valid");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "123456789012345678");
        assert_eq!(rules[0].name, "spam rule");
        assert_eq!(
            validate_automod_rules(&serde_json::json!({"nope": true}))
                .expect_err("object")
                .problems,
            vec!["export must be an array of rules"]
        );
    }

    #[test]
    fn export_validation_accepts_empty_and_refuses_non_arrays() {
        // An empty guild export is usable: zero rules, no problems.
        let rules = validate_automod_rules(&serde_json::json!([])).expect("empty is valid");
        assert!(rules.is_empty());
        // `null` and scalar payloads are refused, not treated as zero rules.
        for payload in [
            serde_json::json!(null),
            serde_json::json!("rules"),
            serde_json::json!(42),
            serde_json::json!(true),
        ] {
            assert_eq!(
                validate_automod_rules(&payload)
                    .expect_err("must fail")
                    .problems,
                vec!["export must be an array of rules"],
                "{payload}"
            );
        }
    }

    #[test]
    fn export_validation_keeps_extra_fields_and_repeats_identically() {
        let payload = serde_json::json!([
            {
                "id": "123456789012345678",
                "name": "spam rule",
                "event_type": 1,
                "trigger_metadata": {"keyword_filter": ["spam"]},
                "actions": [{"type": 1}],
            },
            {"id": "223456789012345678", "name": "invite rule"},
        ]);
        let first = validate_automod_rules(&payload).expect("valid");
        assert_eq!(first.len(), 2);
        // The extra fields pass through: the raw rule is unchanged.
        assert_eq!(first[0].raw, payload[0]);
        assert_eq!(first[1].raw, payload[1]);
        // Repeated validation is identical — the validator is a pure check.
        let second = validate_automod_rules(&payload).expect("valid again");
        assert_eq!(first, second);
    }
}
