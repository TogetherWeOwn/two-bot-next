//! Framework-free feed commands, parsing and polling decisions.
//! Side effects belong to the shared router/REST executor, not this module.

use crate::feeds_http::{validate_source, FetchError, MAX_FEED_BYTES, MAX_FEED_SOURCE_BYTES};
use roxmltree::{Document, Node, ParsingOptions};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::collections::HashSet;
use thiserror::Error;

pub const MAX_FEED_ITEMS: usize = 200;
pub const MAX_FEED_POSTS_PER_POLL: usize = 20;
pub const DELIVERY_CLAIM_LEASE_MS: i64 = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FeedKind {
    Rss,
    Youtube,
    Twitch,
}

impl FeedKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rss => "rss",
            Self::Youtube => "youtube",
            Self::Twitch => "twitch",
        }
    }

    pub fn parse(value: &str) -> Result<Self, FeedError> {
        match value {
            "rss" => Ok(Self::Rss),
            "youtube" => Ok(Self::Youtube),
            "twitch" => Ok(Self::Twitch),
            _ => Err(FeedError::InvalidKind),
        }
    }
}

#[derive(Debug, Error)]
pub enum FeedError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error("Unknown feed kind.")]
    InvalidKind,
    #[error("Manage Server permission is required.")]
    PermissionRequired,
    #[error("Feed command is not enabled for this guild.")]
    Disabled,
    #[error("Invalid feed id.")]
    InvalidId,
    #[error("Feed contains invalid or unsafe XML.")]
    InvalidXml,
    #[error("Feed contains too many items.")]
    TooManyItems,
    #[error("Feed item has no stable key.")]
    MissingKey,
    #[error("Feed item URL is invalid or too long to post.")]
    InvalidItemUrl,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedRelay {
    pub id: String,
    pub guild_id: String,
    pub channel_id: String,
    pub kind: FeedKind,
    pub source: String,
    pub enabled: bool,
    pub last_checked_at: Option<i64>,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedItem {
    pub key: String,
    pub title: String,
    pub url: String,
    pub published_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedCommand {
    Add {
        id: String,
        kind: FeedKind,
        source: String,
    },
    Remove {
        id: String,
    },
    Inspect {
        id: String,
    },
    List,
    Pause {
        id: String,
    },
    Resume {
        id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedCommandPlan {
    Add(FeedRelay),
    Remove { guild_id: String, id: String },
    Inspect { guild_id: String, id: String },
    List { guild_id: String },
    Pause { guild_id: String, id: String },
    Resume { guild_id: String, id: String },
}

pub struct FeedCommandContext<'a> {
    pub enabled: bool,
    pub configured_guild_id: &'a str,
    pub guild_id: &'a str,
    pub channel_id: &'a str,
    pub actor_id: &'a str,
    pub can_manage_guild: bool,
    pub now_ms: i64,
}

pub fn plan_command(
    context: &FeedCommandContext<'_>,
    command: FeedCommand,
) -> Result<FeedCommandPlan, FeedError> {
    if !context.enabled || context.guild_id != context.configured_guild_id {
        return Err(FeedError::Disabled);
    }
    if !context.can_manage_guild {
        return Err(FeedError::PermissionRequired);
    }
    match command {
        FeedCommand::Add { id, kind, source } => {
            validate_id(&id)?;
            Ok(FeedCommandPlan::Add(FeedRelay {
                id,
                guild_id: context.guild_id.into(),
                channel_id: context.channel_id.into(),
                source: normalize_source(kind, &source)?,
                kind,
                enabled: true,
                last_checked_at: None,
                created_by: context.actor_id.into(),
                created_at: context.now_ms,
                updated_at: context.now_ms,
            }))
        }
        FeedCommand::Remove { id } => {
            validate_id(&id)?;
            Ok(FeedCommandPlan::Remove {
                guild_id: context.guild_id.into(),
                id,
            })
        }
        FeedCommand::Inspect { id } => {
            validate_id(&id)?;
            Ok(FeedCommandPlan::Inspect {
                guild_id: context.guild_id.into(),
                id,
            })
        }
        FeedCommand::List => Ok(FeedCommandPlan::List {
            guild_id: context.guild_id.into(),
        }),
        FeedCommand::Pause { id } => {
            validate_id(&id)?;
            Ok(FeedCommandPlan::Pause {
                guild_id: context.guild_id.into(),
                id,
            })
        }
        FeedCommand::Resume { id } => {
            validate_id(&id)?;
            Ok(FeedCommandPlan::Resume {
                guild_id: context.guild_id.into(),
                id,
            })
        }
    }
}

fn validate_id(id: &str) -> Result<(), FeedError> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(FeedError::InvalidId);
    }
    Ok(())
}

pub fn normalize_source(kind: FeedKind, source: &str) -> Result<String, FeedError> {
    if source.len() > MAX_FEED_SOURCE_BYTES {
        return Err(FetchError::InvalidSource.into());
    }
    let source = source.trim();
    if kind == FeedKind::Youtube
        && (20..=32).contains(&source.len())
        && source
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        return Ok(format!(
            "https://www.youtube.com/feeds/videos.xml?channel_id={source}"
        ));
    }
    Ok(validate_source(source)?.to_string())
}

const MAX_FEED_LIST_SOURCE_UNITS: usize = 128;

fn truncate_feed_list_source_with_budget(source: &str, budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }
    let truncated = truncate_utf16(source, budget);
    if truncated == source {
        truncated
    } else {
        format!("{}…", truncate_utf16(&truncated, budget - 1))
    }
}

/// Lifecycle state word for list rows: every stable-ID row carries its own
/// `enabled`/`paused` label so a paused relay stays discoverable with its
/// resume/remove ID, kind, channel and source on one whole row.
fn feed_list_state(feed: &FeedRelay) -> &'static str {
    if feed.enabled {
        "enabled"
    } else {
        "paused"
    }
}

fn feed_list_row_with_budget(feed: &FeedRelay, source_budget: usize) -> String {
    let source = truncate_feed_list_source_with_budget(&feed.source, source_budget);
    format!(
        "`{}` {} {} → <#{}> {source}",
        feed.id,
        feed.kind.as_str(),
        feed_list_state(feed),
        feed.channel_id
    )
}

/// Display-only scheme/host prefix strip for the capacity path. The stored
/// source is unchanged; dropping `https://`/`http://` and a leading `www.`
/// reclaims ~8-12 UTF-16 units for the distinguishing tail.
fn strip_feed_list_display_prefix(source: &str) -> &str {
    let without_scheme = source
        .strip_prefix("https://")
        .or_else(|| source.strip_prefix("http://"))
        .unwrap_or(source);
    without_scheme
        .strip_prefix("www.")
        .unwrap_or(without_scheme)
}

/// Keep the last `limit` UTF-16 units without splitting surrogate pairs.
/// A char straddling the cut is dropped whole, so the result is <= limit.
fn truncate_utf16_tail(value: &str, limit: usize) -> String {
    let units = value.encode_utf16().count();
    if units <= limit {
        return value.to_string();
    }
    let mut skip = units - limit;
    let mut result = String::new();
    for ch in value.chars() {
        let width = ch.len_utf16();
        if skip == 0 {
            result.push(ch);
        } else if skip >= width {
            skip -= width;
        } else {
            // Skip lands inside an astral char: drop it whole.
            skip = 0;
        }
    }
    result
}

/// Capacity-path source rendering that keeps same-host relays
/// distinguishable. Prefix truncation collapses every YouTube row to the same
/// `https://w…` at ~11 units, so an admin sees all ids but cannot map any id
/// to a source. Middle-eliding the scheme-stripped form keeps the
/// distinguishing tail (`channel_id=UC…` / path) plus a recognizable head.
fn truncate_feed_list_source_distinguishing(source: &str, budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }
    if source.encode_utf16().count() <= budget {
        return source.to_string();
    }
    let compact = strip_feed_list_display_prefix(source);
    if compact.encode_utf16().count() <= budget {
        return compact.to_string();
    }
    if budget == 1 {
        return "…".to_string();
    }
    // Below three units there is no room for both a truncation marker and a
    // distinguishing tail, so the tail wins: same-host relays stay mappable
    // to their sources at capacity (the row's fixed frame still carries the
    // exact id, kind, state and channel).
    if budget == 2 {
        return truncate_utf16_tail(compact, 2);
    }
    // Tail-biased split: the tail carries the id/path that differs.
    let remaining = budget - 1;
    let head_units = remaining / 3;
    let tail_units = remaining - head_units;
    let head = truncate_utf16(compact, head_units);
    let tail = truncate_utf16_tail(compact, tail_units);
    format!("{head}…{tail}")
}

fn feed_list_row_distinguishing(feed: &FeedRelay, source_budget: usize) -> String {
    let source = truncate_feed_list_source_distinguishing(&feed.source, source_budget);
    format!(
        "`{}` {} {} → <#{}> {source}",
        feed.id,
        feed.kind.as_str(),
        feed_list_state(feed),
        feed.channel_id
    )
}

/// Latest credible `last_checked_at` in milliseconds (9999-12-31T23:59:59Z).
/// The poller advances the stored stamp after every poll attempt, including
/// failed fetches, so the value is attempt evidence — never a health, success,
/// fetch or delivery signal. Negative and far-future stamps are legacy
/// corruption, not poll times: they render as unknown, never panic and never
/// claim success.
const MAX_ATTEMPT_MS: i64 = 253_402_300_799_000;

fn valid_attempt_ms(stamp: i64) -> bool {
    (0..=MAX_ATTEMPT_MS).contains(&stamp)
}

/// Shared last-attempt phrase for the inspect detail and list rows. Every
/// shape names the attempt: never-attempted relays say never, valid stamps
/// render as Discord-localized relative time, and invalid legacy stamps read
/// as unknown. No shape may use a success label (see the inspect acceptance
/// contract): a failed fetch advances the stored timestamp.
fn attempt_phrase(last_checked_at: Option<i64>) -> String {
    match last_checked_at {
        None => "Last attempt: never".to_owned(),
        Some(stamp) if valid_attempt_ms(stamp) => {
            format!("Last attempt: <t:{}:R>", stamp / 1000)
        }
        Some(_) => "Last attempt: unknown".to_owned(),
    }
}

/// Query parameters whose values are credentials until proven otherwise.
/// Match decoded names case-insensitively, including compound names such as
/// `accessToken` and `clientSecret`; `channel_id` and `format` survive.
/// Names survive redaction — only values are replaced — so the admin still
/// sees which secret-bearing parameter a relay carries.
fn is_credential_query_param(name: &str) -> bool {
    let normalized = name.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "passwd",
        "pwd",
        "pass",
        "key",
        "auth",
        "credential",
        "signature",
        "sig",
        "private",
        "bearer",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

/// Markdown-structure characters escaped inside untrusted source segments.
/// Dots, slashes and dashes stay literal for readability; the host bypasses
/// this entirely so safe host identity stays copyable.
fn escape_source_segment(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(
            ch,
            '\\' | '`' | '*' | '_' | '~' | '[' | ']' | '(' | ')' | '<' | '>'
        ) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

/// Replace embedded `http(s)://` runs inside an untrusted segment — a hostile
/// path or query value smuggling a second link (often paired with masked-link
/// markdown). Returns the replacement plus whether anything was removed.
fn strip_embedded_links(segment: &str) -> (String, bool) {
    let bytes = segment.as_bytes();
    let mut out = String::with_capacity(segment.len());
    let mut index = 0;
    let mut removed = false;
    while index < bytes.len() {
        let rest = &segment[index..];
        let lower = rest.as_bytes();
        let is_embedded = lower.len() >= 7
            && (lower[..7].eq_ignore_ascii_case(b"http://")
                || (lower.len() >= 8 && lower[..8].eq_ignore_ascii_case(b"https://")));
        if !is_embedded {
            let ch = rest.chars().next().unwrap_or_default();
            out.push(ch);
            index += ch.len_utf8().max(1);
            continue;
        }
        removed = true;
        out.push_str("[embedded link removed]");
        // Consume to whitespace, backtick or end; the run is hostile either way.
        let mut end = index;
        for ch in rest.chars() {
            if ch.is_whitespace() || ch == '`' {
                break;
            }
            end += ch.len_utf8();
        }
        index = if end == index { index + 1 } else { end };
    }
    (out, removed)
}

/// One untrusted source segment (path, query name or value, opaque source):
/// mentions neutralized, embedded links removed, hostile markdown escaped.
/// Idempotent on the mention pass so truncation can re-run neutralization.
fn sanitize_source_segment(segment: &str) -> (String, bool) {
    let neutralized = crate::message_safety::neutralize_mentions(segment);
    let (stripped, removed) = strip_embedded_links(&neutralized);
    (escape_source_segment(&stripped), removed)
}

/// Rebuild one raw query string with credential-like values redacted. Names
/// survive (escaped); only values are replaced, so relays stay
/// distinguishable by their non-secret parameters.
fn redact_source_query(raw: &str, reasons: &mut Vec<&'static str>) -> String {
    let mut query = String::new();
    // Some origins (Rack/Rails, Perl CGI) treat `;` as a pair separator too;
    // splitting only on `&` would leave a credential pair hidden inside a
    // safe-looking value. Preserve the original separator in the projection.
    for chunk in raw.split_inclusive(['&', ';']) {
        let (pair, separator) = if let Some(stripped) = chunk.strip_suffix('&') {
            (stripped, "&")
        } else if let Some(stripped) = chunk.strip_suffix(';') {
            (stripped, ";")
        } else {
            (chunk, "")
        };
        let (name, value) = pair
            .split_once('=')
            .map_or((pair, None), |(n, v)| (n, Some(v)));
        let (safe_name, name_had_link) = sanitize_source_segment(name);
        if name_had_link {
            push_redaction_reason(reasons, "embedded link");
        }
        query.push_str(&safe_name);
        if let Some(raw_value) = value {
            query.push('=');
            // Decode only for classification; keep the original name
            // and non-secret value spelling in the safe projection.
            let decoded_name = url::form_urlencoded::parse(pair.as_bytes())
                .next()
                .map(|(name, _)| name)
                .unwrap_or_default();
            if is_credential_query_param(&decoded_name) {
                push_redaction_reason(reasons, "secret query values");
                query.push_str("[redacted]");
            } else {
                let (safe_value, value_had_link) = sanitize_source_segment(raw_value);
                if value_had_link {
                    push_redaction_reason(reasons, "embedded link");
                }
                query.push_str(&safe_value);
            }
        }
        query.push_str(separator);
    }
    query
}

fn push_redaction_reason(reasons: &mut Vec<&'static str>, reason: &'static str) {
    if !reasons.contains(&reason) {
        reasons.push(reason);
    }
}

/// Bounded safe projection of an untrusted stored source plus whether anything
/// was redacted and why. Userinfo, fragments and credential-like query values
/// never survive; the host, port, scheme, path and non-secret query identity
/// do, in escaped form, so relays stay distinguishable. Sources that do not
/// parse as absolute URLs (bare channel ids, legacy garbage) render through
/// the same sanitize-and-truncate pipeline as opaque text.
///
/// Truncation is tail-biased (head…tail): prefix truncation would collapse
/// every same-host relay to an identical head and hide the distinguishing
/// query tail.
fn safe_source_projection(source: &str, budget: usize) -> (String, Vec<&'static str>) {
    let mut reasons: Vec<&'static str> = Vec::new();
    let parsed = url::Url::parse(source.trim())
        .ok()
        .filter(|url| url.host_str().is_some());
    let Some(url) = parsed else {
        let (sanitized, had_link) = sanitize_source_segment(source.trim());
        if had_link {
            push_redaction_reason(&mut reasons, "embedded link");
        }
        return (truncate_safe_source(&sanitized, budget), reasons);
    };
    if !url.username().is_empty() || url.password().is_some() {
        push_redaction_reason(&mut reasons, "login");
    }
    if url.fragment().is_some() {
        push_redaction_reason(&mut reasons, "fragment");
    }
    let (safe_path, path_had_link) = sanitize_source_segment(url.path());
    if path_had_link {
        push_redaction_reason(&mut reasons, "embedded link");
    }
    // `host_str` already includes the brackets around an IPv6 literal.
    let host = url.host_str().unwrap_or_default();
    let mut projection = format!("{}://{host}", url.scheme());
    if let Some(port) = url.port() {
        projection.push_str(&format!(":{port}"));
    }
    projection.push_str(&safe_path);
    if let Some(raw_query) = url.query() {
        let safe_query = redact_source_query(raw_query, &mut reasons);
        if !safe_query.is_empty() {
            projection.push('?');
            projection.push_str(&safe_query);
        }
    }
    // Neutralize once more after truncation: elision can only cut, never join,
    // but the pass is idempotent and keeps the guarantee local.
    let truncated = truncate_safe_source(&projection, budget);
    (
        crate::message_safety::neutralize_mentions(&truncated),
        reasons,
    )
}

/// Tail-biased truncation for a sanitized source projection: a recognizable
/// head plus the distinguishing tail, joined by an explicit `…`. Astral
/// scalars are never split.
fn truncate_safe_source(value: &str, budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }
    if value.encode_utf16().count() <= budget {
        return value.to_string();
    }
    if budget == 1 {
        return "…".to_string();
    }
    let remaining = budget - 1;
    let head_units = remaining / 3;
    let tail_units = remaining - head_units;
    format!(
        "{}…{}",
        truncate_utf16(value, head_units),
        truncate_utf16_tail(value, tail_units)
    )
}

/// Source budget inside the inspect detail: the fixed identity, attempt and
/// disclaimer lines stay near ~400 units, so 1400 keeps the whole bounded
/// reply under Discord's 2,000-unit cap with slack for the redaction notice.
const MAX_INSPECT_SOURCE_UNITS: usize = 1400;

impl FeedRelay {
    /// Bounded private detail for one saved relay: exact id, kind,
    /// destination, enabled state, safe source and last attempted poll. The
    /// attempt is attempt evidence only — a saved source is not proof of
    /// current remote availability, and the copy says so.
    pub fn inspect_text(&self) -> String {
        let (source, reasons) = safe_source_projection(&self.source, MAX_INSPECT_SOURCE_UNITS);
        let mut lines = vec![
            format!("Feed relay `{}`", self.id),
            format!(
                "Kind: {} · Channel: <#{}> · State: {}",
                self.kind.as_str(),
                self.channel_id,
                if self.enabled {
                    "enabled"
                } else {
                    "disabled (paused)"
                }
            ),
            attempt_phrase(self.last_checked_at),
            format!("Source: {source}"),
        ];
        if !reasons.is_empty() {
            lines.push(format!("(redacted: {})", reasons.join(", ")));
        }
        lines.push(
            "Saved relays show the last attempt only; they do not prove the source is reachable now."
                .to_owned(),
        );
        // Defensive: the per-part budgets already bound this, but legacy rows
        // are untrusted and the reply must never exceed the cap. The id leads,
        // so a clamp can only cost trailing context, never identity.
        truncate_utf16(&lines.join("\n"), DISCORD_MESSAGE_CONTENT_LIMIT)
    }
}

pub fn feed_list_text(feeds: &[FeedRelay]) -> String {
    if feeds.is_empty() {
        return "No feed relays configured.".into();
    }
    // Fast path: today's per-row 128-unit rendering when the whole list fits.
    // This preserves empty/short output exactly.
    let full = feeds
        .iter()
        .map(|feed| feed_list_row_with_budget(feed, MAX_FEED_LIST_SOURCE_UNITS))
        .collect::<Vec<_>>()
        .join("\n");
    if full.encode_utf16().count() <= DISCORD_MESSAGE_CONTENT_LIMIT {
        return full;
    }
    // At capacity the fixed per-row overhead alone (~66-70 units for a
    // 32-hex id plus kind/channel) fits, but 25 long sources do not. Shrink
    // every source to a uniform budget so all exact `id` removal ids stay
    // discoverable in list order within the single 2,000-unit response,
    // instead of truncating trailing rows away. Short sources keep their
    // full text; only long ones shrink to the shared budget. The capacity
    // path middle-elides the scheme-stripped source (head…tail) so same-host
    // relays stay pairwise distinguishable; the 128-unit fast path above
    // keeps legacy prefix truncation byte-identical.
    let fixed_units = feeds
        .iter()
        .map(|feed| {
            format!(
                "`{}` {} {} → <#{}> ",
                feed.id,
                feed.kind.as_str(),
                feed_list_state(feed),
                feed.channel_id
            )
            .encode_utf16()
            .count()
        })
        .sum::<usize>()
        + feeds.len().saturating_sub(1);
    let available = DISCORD_MESSAGE_CONTENT_LIMIT.saturating_sub(fixed_units);
    let budget = (available / feeds.len()).min(MAX_FEED_LIST_SOURCE_UNITS);
    let bounded = feeds
        .iter()
        .map(|feed| feed_list_row_distinguishing(feed, budget))
        .collect::<Vec<_>>()
        .join("\n");
    if bounded.encode_utf16().count() <= DISCORD_MESSAGE_CONTENT_LIMIT {
        return bounded;
    }
    // Defensive: fixed overhead alone exceeds the limit (e.g. pathological
    // max-length ids or legacy-restored rows beyond the 25-relay cap). Emit
    // the smallest exact-id rows, bounded to the message cap; the executor's
    // message-safety clamp enforces the final 2,000-unit bound.
    let mut id_only = String::new();
    for feed in feeds {
        let row = format!("`{}`", feed.id);
        let extra = if id_only.is_empty() { 0 } else { 1 };
        if id_only.encode_utf16().count() + extra + row.encode_utf16().count()
            > DISCORD_MESSAGE_CONTENT_LIMIT
        {
            break;
        }
        if !id_only.is_empty() {
            id_only.push('\n');
        }
        id_only.push_str(&row);
    }
    // Second fence so the return is bounded even if a single id row alone
    // exceeds the cap (max 128-char ids cannot, but legacy data is untrusted).
    truncate_utf16(&id_only, DISCORD_MESSAGE_CONTENT_LIMIT)
}

impl FeedRelay {
    /// Pack whole relay rows into bounded page bodies in input order.
    ///
    /// Each body holds as many whole rows as fit in `content_budget` UTF-16
    /// units, joined with `\n`; rows are never split and ids never dropped,
    /// so paging — not per-row shrinking — absorbs capacity. The caller
    /// reserves its own footer/navigation overhead before choosing the
    /// budget. An empty guild yields the useful first page
    /// (`No feed relays configured.`). Bodies carry no footer; see
    /// [`feed_list_page`] for the composed reply.
    pub fn list_pages(rows: &[FeedRelay], content_budget: usize) -> Vec<String> {
        if rows.is_empty() {
            return vec![feed_list_text(rows)];
        }
        let rendered: Vec<String> = rows
            .iter()
            .map(|feed| feed_list_page_row(feed, content_budget))
            .collect();
        pack_page_bodies(&rendered, content_budget)
    }

    /// Pack whole relay rows like [`FeedRelay::list_pages`], with each row
    /// carrying the shared last-attempt phrase. The attempt suffix reserves
    /// its own units before the row renders, so rows (and their exact ids)
    /// still fit the budget; when the suffix alone would eat the budget, the
    /// legacy row wins and the id stays reachable without the attempt.
    pub fn list_pages_with_attempt(rows: &[FeedRelay], content_budget: usize) -> Vec<String> {
        if rows.is_empty() {
            return vec![feed_list_text(rows)];
        }
        let rendered: Vec<String> = rows
            .iter()
            .map(|feed| feed_list_page_row_with_attempt(feed, content_budget))
            .collect();
        pack_page_bodies(&rendered, content_budget)
    }
}

/// Pack pre-rendered whole rows into page bodies in input order. Rows are
/// never split and never dropped; callers reserve footer overhead up front.
fn pack_page_bodies(rendered: &[String], content_budget: usize) -> Vec<String> {
    let mut pages: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_units = 0usize;
    for row in rendered {
        let row_units = row.encode_utf16().count();
        if !current.is_empty() && current_units + 1 + row_units > content_budget {
            pages.push(std::mem::take(&mut current));
            current_units = 0;
        }
        if !current.is_empty() {
            current.push('\n');
            current_units += 1;
        }
        current.push_str(row);
        current_units += row_units;
    }
    pages.push(current);
    pages
}

/// One attempt-suffixed page-body row: the legacy row frame (exact id, kind,
/// channel mention, source) plus the shared [`attempt_phrase`], so list
/// pages and the inspect detail speak the same attempt vocabulary. The id
/// stays first, so a suffix can never hide it.
fn feed_list_page_row_with_attempt(feed: &FeedRelay, content_budget: usize) -> String {
    let suffix = format!(" · {}", attempt_phrase(feed.last_checked_at));
    let suffix_units = suffix.encode_utf16().count();
    let Some(row_budget) = content_budget.checked_sub(suffix_units) else {
        return feed_list_page_row(feed, content_budget);
    };
    if row_budget == 0 {
        return feed_list_page_row(feed, content_budget);
    }
    format!("{}{suffix}", feed_list_page_row(feed, row_budget))
}

/// One page-body row: the exact 128-unit legacy rendering when it fits the
/// page budget, so short lists stay byte-identical to [`feed_list_text`]. A
/// single row wider than the budget keeps its whole fixed frame (exact id,
/// kind, channel mention) and middle-elides the source with an explicit `…`.
/// When even the frame cannot fit, the exact id alone stays reachable within
/// the budget instead of hiding trailing rows.
fn feed_list_page_row(feed: &FeedRelay, content_budget: usize) -> String {
    let full = feed_list_row_with_budget(feed, MAX_FEED_LIST_SOURCE_UNITS);
    if full.encode_utf16().count() <= content_budget {
        return full;
    }
    let frame_units = format!(
        "`{}` {} {} → <#{}> ",
        feed.id,
        feed.kind.as_str(),
        feed_list_state(feed),
        feed.channel_id
    )
    .encode_utf16()
    .count();
    if frame_units > content_budget {
        // Defensive: reachable for restored/legacy rows whose fixed frame
        // (an overlong channel id) exceeds the page budget. A tiny caller
        // budget must still keep the removal id reachable, never drop the row.
        return truncate_utf16(&format!("`{}`", feed.id), content_budget);
    }
    feed_list_row_distinguishing(feed, content_budget - frame_units)
}

/// Hint appended to single-page lists and baked into the multi-page footer,
/// so the stable-ID inspect operation stays discoverable from every list.
/// Uses the same attempt vocabulary as the rows: the detail shows the last
/// attempt, not a health verdict.
pub const FEED_LIST_INSPECT_HINT: &str =
    "Run /feed-inspect id:<relay-id> for source and last-attempt detail.";

/// Full `/feed-list` reply for one 1-based page over already-fetched rows.
///
/// The bounded guild read stays the caller's; this composes bodies from
/// [`FeedRelay::list_pages_with_attempt`] after reserving footer overhead, so
/// every reply including navigation fits Discord's 2,000 UTF-16 units. Rows
/// keep their legacy frame (exact id, kind, channel, source) plus the shared
/// last-attempt phrase. A single page ends with the inspect hint and no
/// `Page n of total` footer; every page of a multi-page list ends with
/// [`feed_list_page_footer`], which carries the same hint. Nonpositive and
/// out-of-range pages refuse shortly with usable navigation and list no rows.
#[must_use]
pub fn feed_list_page(feeds: &[FeedRelay], page: i64) -> String {
    if page < 1 {
        return feed_list_page_underflow_text().to_owned();
    }
    if feeds.is_empty() {
        if page > 1 {
            return feed_list_page_out_of_range_text(page, 1);
        }
        return feed_list_text(feeds);
    }
    // Converge on the page count: the footer length depends on the total,
    // and the total depends on the reserved footer overhead. Rows re-render
    // per budget, so packing is NOT monotonic in the budget: a smaller
    // budget can collapse frame-inflated rows to tiny id-only rows and
    // shrink the page count. Accept fewer bodies than guessed instead of
    // looping: the reserved overhead upper-bounds the actual footer (its
    // length only grows with the total), and a single body packed under
    // budget fits with its hint. The guess only ever increases and is bounded
    // by one page per row, so this always terminates.
    let mut total_guess: usize = 1;
    loop {
        let overhead = if total_guess <= 1 {
            FEED_LIST_INSPECT_HINT.encode_utf16().count() + 1 // joining newline
        } else {
            feed_list_page_footer(total_guess as i64, total_guess as i64)
                .encode_utf16()
                .count()
                + 1 // joining newline
        };
        let budget = DISCORD_MESSAGE_CONTENT_LIMIT.saturating_sub(overhead);
        let bodies = FeedRelay::list_pages_with_attempt(feeds, budget);
        if bodies.len() <= total_guess {
            let total = bodies.len() as i64;
            if page > total {
                return feed_list_page_out_of_range_text(page, total);
            }
            if bodies.len() == 1 {
                return format!("{}\n{FEED_LIST_INSPECT_HINT}", bodies[0]);
            }
            return format!(
                "{}\n{}",
                bodies[(page - 1) as usize],
                feed_list_page_footer(page, total)
            );
        }
        // Bodies outnumber the guess, so the guess strictly increases.
        total_guess = bodies.len();
    }
}

/// `Page n of total` footer with usable navigation for multi-page lists,
/// plus the inspect hint so per-relay detail stays one command away.
/// Stays off single-page replies, which carry the hint on its own line.
#[must_use]
pub fn feed_list_page_footer(current: i64, total: i64) -> String {
    format!(
        "Page {current} of {total}. Run /feed-list page:<number> for another page. {FEED_LIST_INSPECT_HINT}"
    )
}

/// `/feed-list` refusal for `page < 1` (Discord's registry minimum is 1,
/// but the runtime still refuses a bypassed value with this exact text).
#[must_use]
pub fn feed_list_page_underflow_text() -> &'static str {
    "Page must be at least 1. Use /feed-list page:1 to start."
}

/// `/feed-list` refusal for a present-but-unusable `page` option (a wrong
/// wire type): it must refuse, never silently list the default page.
#[must_use]
pub fn feed_list_page_invalid_text() -> &'static str {
    "Invalid page. Use /feed-list page:<number> with a whole number from 1."
}

/// `/feed-list` refusal for `page > total`. Lists no rows and never
/// overflows on extreme page values.
#[must_use]
pub fn feed_list_page_out_of_range_text(page: i64, total: i64) -> String {
    format!("Page {page} is out of range: use /feed-list page:<n> with n from 1 to {total}.")
}

pub fn feed_removed_text(removed: bool) -> &'static str {
    if removed {
        "Feed relay removed."
    } else {
        "No feed relay with that id."
    }
}

/// Fixed reply when removal is refused: the relay exists but carries
/// unresolved pending delivery evidence, so relay and ledger are retained
/// for nonce reconciliation. The member waits for recovery; no repost.
///
/// Legacy fixed text. New pause/removal refusals render through
/// [`feed_pending_evidence_refusal_text`], which carries the fenced
/// pending-evidence count; this constant stays for backward compatibility.
pub const FEED_REMOVAL_RECOVERY_REQUIRED_TEXT: &str =
    "Feed relay kept: unresolved deliveries are still pending recovery.";

/// Fixed reply when pause is refused: the relay stays enabled so its pending
/// delivery recovery remains discoverable. Already-admitted work is not
/// cancelled; the member retries after recovery settles.
///
/// Legacy fixed text. New pause/removal refusals render through
/// [`feed_pending_evidence_refusal_text`]; this constant stays for backward
/// compatibility.
pub const FEED_PAUSE_RECOVERY_REQUIRED_TEXT: &str =
    "Feed relay kept enabled: deliveries are still pending recovery. Retry after recovery settles.";

/// Lifecycle operation refused for pending delivery evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedPendingEvidenceOperation {
    /// `/feed-pause`: the relay stays enabled.
    Pause,
    /// `/feed-remove`: the relay stays saved.
    Remove,
}

/// Shared safe renderer for pause/removal refusals carrying a fenced
/// pending-evidence count.
///
/// The reply keeps the saved relay addressable by its stable id, calls the
/// count a snapshot of unresolved delivery evidence (never failed deliveries
/// or successful posts), states explicitly that no pause/removal occurred,
/// and gives the safe next action: wait for reconciliation and inspect the
/// saved relay via `/feed-inspect`; do not delete/re-add, blindly retry
/// sends, or promise a completion time. It carries exactly one number (the
/// guild/relay-scoped pending count) and no item bodies, source URLs, item
/// keys, nonces, lease/claim tokens, or other-guild evidence. The single
/// summary stays within Discord's 2,000 UTF-16-unit cap for the maximum
/// 128-character relay id.
#[must_use]
pub fn feed_pending_evidence_refusal_text(
    operation: FeedPendingEvidenceOperation,
    id: &str,
    pending: usize,
) -> String {
    let unchanged = match operation {
        FeedPendingEvidenceOperation::Pause => "was not paused and remains enabled",
        FeedPendingEvidenceOperation::Remove => "was not removed and remains saved",
    };
    format!(
        "Feed relay `{id}` {unchanged}: snapshot of {pending} unresolved delivery evidence. \
         Wait for reconciliation and recovery to settle, then inspect via /feed-inspect. \
         Retry after recovery settles."
    )
}

/// Pause/resume confirmations. The relay id is interpolated by the caller so
/// the member can copy the stable id back into `/feed-resume`, `/feed-remove`
/// or `/feed-list page:<n>`.
#[must_use]
pub fn feed_paused_text(id: &str) -> String {
    format!("Feed relay paused: `{id}`.")
}

#[must_use]
pub fn feed_already_paused_text(id: &str) -> String {
    format!("Feed relay is already paused: `{id}`.")
}

#[must_use]
pub fn feed_resumed_text(id: &str) -> String {
    format!("Feed relay resumed: `{id}`.")
}

#[must_use]
pub fn feed_already_resumed_text(id: &str) -> String {
    format!("Feed relay is already enabled: `{id}`.")
}

const MAX_XML_DEPTH: usize = 64;
const MAX_XML_ELEMENT_ATTRIBUTES: usize = 64;
const MAX_XML_TOTAL_ATTRIBUTES: usize = 4096;

/// roxmltree's tokenizer recurses per element and its duplicate-attribute
/// checks are quadratic per tag. Byte/node ceilings alone do not bound either
/// path. This allocation-free, iterative preflight runs BEFORE construction;
/// roxmltree still validates XML syntax, names, namespaces and matching tags.
fn bound_xml_resources(xml: &str) -> Result<(), FeedError> {
    let mut depth = 0usize;
    let mut total_attributes = 0usize;
    let mut offset = 0usize;
    while let Some(relative) = xml[offset..].find('<') {
        let start = offset + relative;
        let rest = &xml[start..];
        let opaque = if rest.starts_with("<!--") {
            Some((4, "-->"))
        } else if rest.starts_with("<![CDATA[") {
            Some((9, "]]>"))
        } else if rest.starts_with("<?") {
            Some((2, "?>"))
        } else {
            None
        };
        if let Some((prefix_len, terminator)) = opaque {
            let end = rest[prefix_len..]
                .find(terminator)
                .ok_or(FeedError::InvalidXml)?;
            offset = start + prefix_len + end + terminator.len();
            continue;
        }
        // Reject DTDs before any parser work, including internal subsets.
        if rest.starts_with("<!") {
            return Err(FeedError::InvalidXml);
        }
        let end = open_tag_end(rest);
        let tag = &rest[..end];
        if !tag.ends_with('>') {
            return Err(FeedError::InvalidXml);
        }
        if tag.starts_with("</") {
            depth = depth.checked_sub(1).ok_or(FeedError::InvalidXml)?;
        } else {
            // A self-closing element also occupies a level while parsing.
            if depth >= MAX_XML_DEPTH {
                return Err(FeedError::InvalidXml);
            }
            let name_end = tag
                .find(|c: char| c.is_ascii_whitespace() || c == '/' || c == '>')
                .ok_or(FeedError::InvalidXml)?;
            let mut attributes = 0usize;
            if !scan_attributes(&tag[name_end..], &mut |_, _| {
                attributes += 1;
                total_attributes += 1;
                attributes <= MAX_XML_ELEMENT_ATTRIBUTES
                    && total_attributes <= MAX_XML_TOTAL_ATTRIBUTES
            }) {
                return Err(FeedError::InvalidXml);
            }
            if !tag.ends_with("/>") {
                depth += 1;
            }
        }
        offset = start + end;
    }
    if depth != 0 {
        return Err(FeedError::InvalidXml);
    }
    Ok(())
}

pub fn parse_xml_feed(xml: &str) -> Result<Vec<FeedItem>, FeedError> {
    // Without the relay kind, use the widest budget accepted by any feed kind.
    parse_xml_feed_with_url_limit(xml, MAX_RSS_TWITCH_ITEM_URL_UTF16_UNITS, &Cell::new(0))
}

pub fn parse_xml_feed_for_kind(xml: &str, kind: FeedKind) -> Result<Vec<FeedItem>, FeedError> {
    parse_xml_feed_report(xml, kind).map(|parsed| parsed.items)
}

/// Parsed items and how many entries were dropped for an unsafe item URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFeed {
    pub items: Vec<FeedItem>,
    pub unsafe_urls_filtered: usize,
}

pub fn parse_xml_feed_report(xml: &str, kind: FeedKind) -> Result<ParsedFeed, FeedError> {
    let filtered = Cell::new(0);
    let items = parse_xml_feed_with_url_limit(xml, max_feed_item_url_utf16_units(kind), &filtered)?;
    Ok(ParsedFeed {
        items,
        unsafe_urls_filtered: filtered.get(),
    })
}

fn parse_xml_feed_with_url_limit(
    xml: &str,
    max_url_utf16_units: usize,
    unsafe_urls_filtered: &Cell<usize>,
) -> Result<Vec<FeedItem>, FeedError> {
    if xml.len() > MAX_FEED_BYTES {
        return Err(FetchError::TooLarge.into());
    }
    bound_xml_resources(xml)?;
    let doc = Document::parse_with_options(
        xml,
        ParsingOptions {
            allow_dtd: false,
            nodes_limit: 100_000,
            entity_resolver: None,
        },
    )
    .map_err(|_| FeedError::InvalidXml)?;
    let root = doc.root_element();
    // Qualified root name: legacy looks up `parsed.rss`/`parsed.feed`, so a
    // prefixed root is not a feed at all rather than a local-name match.
    let entries: Vec<_> = match raw_tag_name(xml, root) {
        "rss" => {
            // Legacy requires channel to be a single record. An attribute
            // shadows its children; repeated channels project to an array,
            // not a record. Item/entry arrays, in contrast, are supported.
            if raw_attribute(xml, root, "channel").is_some() {
                return Ok(Vec::new());
            }
            let mut channels = root.children().filter(|node| named(xml, *node, "channel"));
            let Some(channel) = channels.next() else {
                return Ok(Vec::new());
            };
            if channels.next().is_some() || raw_attribute(xml, channel, "item").is_some() {
                return Ok(Vec::new());
            }
            channel
                .children()
                .filter(|node| named(xml, *node, "item"))
                .collect()
        }
        "feed" => {
            if raw_attribute(xml, root, "entry").is_some() {
                return Ok(Vec::new());
            }
            root.children()
                .filter(|node| named(xml, *node, "entry"))
                .collect()
        }
        _ => return Err(FeedError::InvalidXml),
    };
    if entries.len() > MAX_FEED_ITEMS {
        return Err(FeedError::TooManyItems);
    }
    Ok(entries
        .into_iter()
        .filter_map(|entry| {
            // An entry-level `link` attribute overwrites child `<link>`
            // elements on the legacy record, so it shadows them entirely
            // (even when empty, which then resolves to no URL).
            let raw_url = match raw_attribute(xml, entry, "link") {
                // Already line-ending-normalized and JS-trimmed by
                // `raw_attribute`, mirroring legacy's normalize-then-trim.
                Some(attr) => attr,
                None => {
                    let links: Vec<_> = entry
                        .children()
                        .filter(|node| named(xml, *node, "link"))
                        .collect();
                    // Legacy `resolveLink` tests the RAW href: only an alternate whose
                    // href is itself HTTP(S) is promoted. A mailto alternate must not
                    // shadow the valid HTTPS alternate that follows it.
                    links
                        .iter()
                        .find(|node| {
                            is_alternate(xml, **node) && is_http_href(&link_href_raw(xml, **node))
                        })
                        .or_else(|| {
                            links
                                .iter()
                                .find(|node| is_http_href(&link_href_raw(xml, **node)))
                        })
                        // Legacy falls back to the first non-empty href, not the
                        // first link element: `hrefs[0]` is drawn from the filtered list.
                        .or_else(|| {
                            links
                                .iter()
                                .find(|node| !link_href_raw(xml, **node).is_empty())
                        })
                        .map(|node| link_href_raw(xml, *node))
                        .unwrap_or_default()
                }
            };
            // Legacy `textValue` short-circuits on the raw text: guid, then
            // id, then the raw link. Entry-level attributes override
            // same-named children and repeated children become an array that
            // textValue rejects, so selection mirrors the legacy record
            // shape, not just the first matching child. Identity decoding
            // runs after selection so delivery keys hash the exact legacy form.
            let raw_key = [
                raw_scalar_field(xml, entry, "guid"),
                raw_scalar_field(xml, entry, "id"),
            ]
            .into_iter()
            .find(|key| !key.is_empty())
            .unwrap_or(raw_url.clone());
            let key = decode_xml_entities(&raw_key);
            let url = decode_xml_entities(&raw_url);
            if key.is_empty() || !is_item_url(&url, max_url_utf16_units) {
                if has_unsafe_message_url(&url) {
                    unsafe_urls_filtered.set(unsafe_urls_filtered.get() + 1);
                }
                return None;
            }
            let raw_title = raw_scalar_field(xml, entry, "title");
            let published_at = ["pubDate", "published", "updated"]
                .into_iter()
                .map(|name| raw_scalar_field(xml, entry, name))
                .find(|value| !value.is_empty())
                .map(|value| decode_xml_entities(&value));
            Some(FeedItem {
                key,
                title: if raw_title.is_empty() {
                    "Untitled".into()
                } else {
                    decode_xml_entities(&raw_title)
                },
                url,
                published_at,
            })
        })
        .collect())
}

fn named(xml: &str, node: Node<'_, '_>, name: &str) -> bool {
    node.is_element() && raw_tag_name(xml, node) == name
}

/// Qualified element name straight from the source slice: legacy
/// fast-xml-parser keeps namespace prefixes on property names, so `row.guid`
/// never sees `<vendor:guid>` and only an unprefixed element is a core feed
/// field. Cuts land on ASCII markup delimiters, so slicing is UTF-8-safe.
fn raw_tag_name<'a>(xml: &'a str, node: Node<'_, '_>) -> &'a str {
    let rest = xml[node.range()].strip_prefix('<').unwrap_or("");
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
        .unwrap_or(rest.len());
    &rest[..end]
}

/// Legacy parses with `processEntities: false`, so its key/url/title text is
/// the RAW source form: `post&#49;` stays `post&#49;` and `<guid>` identity
/// decoding only expands the five named escapes. roxmltree always decodes
/// predefined and numeric references, so read element text from the source
/// slice instead. Slicing stays ASCII-safe: every cut lands on a markup
/// boundary (`<`/`>`), which is always a char boundary in UTF-8.
fn inner_source<'a>(xml: &'a str, node: Node<'_, '_>) -> &'a str {
    let range = node.range();
    let inner = &xml[range];
    // Strip one matching open/close tag pair; fall back to the full slice
    // (self-closing or text node) when the shape is unexpected.
    let after_open = open_tag_end(inner);
    let before_close = inner.rfind('<').unwrap_or(inner.len());
    if after_open <= before_close {
        &inner[after_open..before_close]
    } else {
        inner
    }
}

/// Index just past the open tag's `>`, honouring quoted attribute values
/// so a `>` inside `href="a>b"` does not end the tag early.
fn open_tag_end(tag_and_rest: &str) -> usize {
    let bytes = tag_and_rest.as_bytes();
    let mut i = 0;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
        } else if b == b'\'' || b == b'"' {
            quote = Some(b);
        } else if b == b'>' {
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

/// Scalar field the way the legacy record shape resolves it: an
/// entry-level attribute wins over a same-named child, a repeated child
/// name is an array that `textValue` rejects, and otherwise the value is
/// the child's own `#text`. Returns the raw source form (line endings
/// normalized, numeric references untouched).
fn raw_scalar_field(xml: &str, entry: Node<'_, '_>, name: &str) -> String {
    // Entry-level attributes land on the parsed item record first, so
    // `<item guid="attribute-key">` shadows `<guid>element-key</guid>`.
    // A missing `=` contributes nothing (legacy `allowBooleanAttributes`
    // is false), which `raw_attribute` already models as `None`.
    if let Some(attr) = raw_attribute(xml, entry, name) {
        return attr;
    }
    let mut matching = entry.children().filter(|child| named(xml, *child, name));
    let Some(first) = matching.next() else {
        return String::new();
    };
    if matching.next().is_some() {
        // Repeated children compress to an array; `textValue` only reads
        // strings, numbers and single records, so the field is rejected and
        // identity falls through exactly like legacy.
        return String::new();
    }
    raw_own_text(xml, first)
}

/// Keep only the element's own character data the way legacy `#text` keeps
/// it: nested elements split flush boundaries (their content contributes
/// nothing), comments glue neighbours without flushing, and each plain-text
/// run is JS-trimmed on flush. CDATA appends its raw body untrimmed, PIs
/// flush-split through their tagged node, and a final JS trim mirrors
/// `textValue`. Numeric references pass through untouched; NOT decoded here.
fn raw_own_text(xml: &str, node: Node<'_, '_>) -> String {
    raw_own_text_inner(normalize_line_endings(inner_source(xml, node)).as_ref())
}

fn raw_own_text_inner(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut segment = String::new();
    let flush_segment = |segment: &mut String, out: &mut String| {
        let trimmed = js_trim(segment);
        if !trimmed.is_empty() {
            out.push_str(trimmed);
        }
        segment.clear();
    };
    let mut i = 0;
    while i < bytes.len() {
        if source[i..].starts_with("<![CDATA[") {
            let rest = &source[i + 9..];
            let end = rest.find("]]>").map(|j| i + 9 + j).unwrap_or(source.len());
            // A text run buffered before CDATA keeps its position: flush it
            // first so `pre<!--x--><![CDATA[ mid ]]>` stays `pre mid` instead
            // of reordering to `mid pre`. CDATA is stored raw (`dontTrim`),
            // so it appends even when empty or padding-only: `<![CDATA[]]>`
            // still joins neighbours.
            flush_segment(&mut segment, &mut out);
            out.push_str(&source[i + 9..end]);
            i = (end + 3).min(source.len());
        } else if source[i..].starts_with("<!--") {
            // Comments produce no node and flush nothing: neighbours join.
            let end = source[i..]
                .find("-->")
                .map(|j| i + j + 3)
                .unwrap_or(source.len());
            i = end;
        } else if source[i..].starts_with("<?") {
            // PIs are tagged nodes: plain text flushes around them and their
            // content contributes nothing.
            flush_segment(&mut segment, &mut out);
            let end = source[i..]
                .find("?>")
                .map(|j| i + j + 2)
                .unwrap_or(source.len());
            i = end;
        } else if source[i..].starts_with("</") {
            i = tag_end(source, i);
        } else if bytes[i] == b'<' {
            // Nested element: its content contributes nothing, but the plain
            // text around it flushes as separate runs.
            flush_segment(&mut segment, &mut out);
            i = skip_element(source, i);
        } else {
            let end = source[i..].find('<').map(|j| i + j).unwrap_or(source.len());
            segment.push_str(&source[i..end]);
            // Each plain-text run flushes when it meets markup: `pre <v>x</v>
            // post` yields `pre` + `post`, not a trimmed `pre  post`.
            // Comments are the exception: legacy skips them without flushing,
            // so neighbours join into one run (`pre <!--c--> post` stays
            // `pre  post`).
            if end < source.len() && !source[end..].starts_with("<!--") {
                flush_segment(&mut segment, &mut out);
            }
            i = end;
        }
    }
    flush_segment(&mut segment, &mut out);
    js_trim(&out).to_owned()
}

/// Legacy `parseXml` normalizes line endings before any other parsing:
/// `xmlData.replace(/\r\n?/g, "\n")`. Every downstream text, CDATA body
/// and attribute value therefore sees LF only. Apply the same
/// normalization to source-derived identity paths (text, CDATA, raw
/// attribute values) so hashes match the historic rows.
fn normalize_line_endings(value: &str) -> String {
    if !value.as_bytes().contains(&b'\r') {
        return value.to_owned();
    }
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            out.push('\n');
            // A CRLF pair is one line break: skip the LF that follows CR.
            if chars.clone().next() == Some('\n') {
                chars.next();
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Index just past the `>` ending the tag that starts at `start`,
/// honouring single/double quotes so `>` inside an attribute value does
/// not end the tag early.
fn tag_end(source: &str, start: usize) -> usize {
    open_tag_end(&source[start..]) + start
}

/// Skip a nested `<name ...>...</name>` subtree starting at its open tag.
/// Depth counting handles same-name nesting; comments, PIs and CDATA inside
/// are skipped as opaque spans so their `>` bytes cannot unbalance the scan.
/// A self-closing open tag contributes no subtree.
fn skip_element(source: &str, start: usize) -> usize {
    let open_end = tag_end(source, start);
    if source[start..open_end].trim_end().ends_with("/>") {
        return open_end;
    }
    let mut depth = 1usize;
    let mut i = open_end;
    while i < source.len() && depth > 0 {
        if source[i..].starts_with("<![CDATA[") {
            let end = source[i..]
                .find("]]>")
                .map(|j| i + j + 3)
                .unwrap_or(source.len());
            i = end;
        } else if source[i..].starts_with("<!--") {
            i = source[i..]
                .find("-->")
                .map(|j| i + j + 3)
                .unwrap_or(source.len());
        } else if source[i..].starts_with("<?") {
            i = source[i..]
                .find("?>")
                .map(|j| i + j + 2)
                .unwrap_or(source.len());
        } else if source[i..].starts_with("</") {
            depth -= 1;
            i = tag_end(source, i);
        } else if source.as_bytes()[i] == b'<' {
            let end = tag_end(source, i);
            if !source[i..end].trim_end().ends_with("/>") {
                depth += 1;
            }
            i = end;
        } else {
            i = source[i..].find('<').map(|j| i + j).unwrap_or(source.len());
        }
    }
    i
}

/// Legacy trims with JavaScript `String.trim()`: ECMAScript WhiteSpace plus
/// LineTerminator. That set differs from Rust `str::trim()` in exactly two
/// code points: JS strips U+FEFF but keeps U+0085, Rust does the opposite.
/// Rust trim here would hash `\u{feff}post1` instead of `post1` (or drop a
/// leading U+0085 legacy keeps), missing restored delivery rows and
/// reposting. Used on the identity path only: extraction/selection
/// (`raw_text`, link hrefs, attributes) and final key/fallback hashing
/// (`item_key`), mirroring legacy `textValue`/`resolveLink`/`String.trim`.
fn js_trim(value: &str) -> &str {
    value.trim_matches(js_trim_char)
}

fn js_trim_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{0009}'..='\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

/// Legacy `decodeXml`: exactly the five named replacements, applied in
/// order so `&amp;lt;` chains to `<`. General numeric references such as
/// `&#49;` or `&#x27;` are intentionally NOT decoded.
fn decode_xml_entities(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// Whether a `<link>` element is an alternate in the legacy sense: a
/// missing or empty `rel` defaults to alternate, matching `entry.rel ||
/// 'alternate'`. The `rel` attribute wins over a `<rel>` child (attributes
/// overwrite same-named children on the legacy record); a repeated `<rel>`
/// child is an array that `textValue` rejects, so the field is missing and
/// the link defaults to alternate. Standalone text or tagged (`?pi`) nodes
/// under `<link>` are not `rel` fields, so they never block the default.
fn is_alternate(xml: &str, node: Node<'_, '_>) -> bool {
    match raw_attribute(xml, node, "rel") {
        Some(rel) => rel.is_empty() || rel.eq_ignore_ascii_case("alternate"),
        None => match raw_rel_child(xml, node) {
            None => true,
            Some(rel) => rel.is_empty() || rel.eq_ignore_ascii_case("alternate"),
        },
    }
}

/// The `<rel>` child of a `<link>` record the way legacy `resolveLink` reads
/// it: `textValue(record.rel)` over the single named child, where a repeated
/// child is an array that rejects to missing. Returns `None` when there is
/// no `<rel>` element child at all.
fn raw_rel_child(xml: &str, node: Node<'_, '_>) -> Option<String> {
    let mut matching = node.children().filter(|child| named(xml, *child, "rel"));
    let first = matching.next()?;
    if matching.next().is_some() {
        return None;
    }
    Some(raw_own_text(xml, first))
}

/// Legacy tests the raw href with `/^https?:\/\//i` — before any entity
/// decoding and without URL parsing. Case-insensitivity matters:
/// `HTTPS://` counts.
fn is_http_href(raw_href: &str) -> bool {
    let href = js_trim(raw_href).as_bytes();
    href.get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"http://"))
        || href
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"https://"))
}

/// Resolve one `<link>` element the way legacy `resolveLink` resolves a
/// single record, but in RAW source form: the `href` attribute wins when
/// present (a nested `<href>` child only matters when NO `href` attribute
/// is present, since attributes overwrite same-named children on the
/// record). A repeated `<href>` child is an array that `textValue`
/// rejects, yielding nothing. Any other attribute means the record has
/// fields but no `href`, so it contributes nothing (legacy only consults
/// `record.href`, never the text body). Likewise a nested element or PI
/// child projects the link to a record: `<link>url<other>v</other></link>`
/// is `{other, #text}`, so without an `href` attribute or `<href>` child
/// there is no `href` field and the link is discarded rather than falling
/// back to its own text. Comments and CDATA are not fields (comments
/// produce no node, CDATA is `#text`), so they never block the fallback.
/// A bare text link resolves to its own `#text` only when the element is
/// attribute- and field-free; `xmlns`/`xml:*` bookkeeping attributes count
/// as attributes but resolve to the bare text in the legacy reader, which
/// drops them at this level.
fn link_href_raw(xml: &str, node: Node<'_, '_>) -> String {
    if let Some(href) = raw_attribute(xml, node, "href") {
        // Already line-ending-normalized and JS-trimmed by `raw_attribute`,
        // mirroring legacy's normalize-then-`trimValues` order.
        return href;
    }
    let href_children: Vec<_> = node
        .children()
        .filter(|child| named(xml, *child, "href"))
        .collect();
    if !href_children.is_empty() {
        if href_children.len() > 1 {
            return String::new();
        }
        return raw_own_text(xml, href_children[0]);
    }
    if node
        .children()
        .any(|child| child.is_element() || child.is_pi())
    {
        return String::new();
    }
    if has_meaningful_attributes(xml, node) {
        return String::new();
    }
    raw_own_text(xml, node)
}

/// Any valued attribute on the record blocks the bare-text fallback the way
/// legacy single-record `resolveLink` does: it only reads `record.href`,
/// so `<link rel="x">text</link>` — and equally `<link
/// xmlns="…">text</link>` — resolve to nothing. Valueless attributes never
/// reach the legacy record (`allowBooleanAttributes` is false), so they do
/// not block the fallback.
fn has_meaningful_attributes(xml: &str, node: Node<'_, '_>) -> bool {
    !raw_attribute_names(xml, node).is_empty()
}

/// Names on the open tag in source order (duplicates kept, the LAST wins
/// in the legacy record). A missing `=` is a valueless attribute: legacy
/// `allowBooleanAttributes` is false, so the legacy reader drops it and it
/// never appears on the record.
fn raw_attribute_names(xml: &str, node: Node<'_, '_>) -> Vec<String> {
    let mut names = Vec::new();
    let Some(tag) = open_tag_attributes(xml, node) else {
        return names;
    };
    scan_attributes(&tag, &mut |attr_name, has_value| {
        if has_value.is_some() {
            names.push(attr_name.to_owned());
        }
        true
    });
    names
}

/// Read one attribute value from the source slice so entity references
/// survive (`href="...?a=1&amp;b=2"` keeps the raw `&amp;`), and a duplicate
/// name resolves to the LAST value like the legacy record. Matches legacy's
/// attribute-name rule: case-sensitive; a missing `=` contributes nothing
/// (`None`, whether or not the name matches). Line endings are normalized
/// here because legacy normalizes CRLF/CR to LF before parsing, so every
/// downstream attribute value sees LF only; normalization runs before the
/// JS trim exactly like legacy's parse-then-`trimValues` order.
fn raw_attribute(xml: &str, node: Node<'_, '_>, name: &str) -> Option<String> {
    let tag = open_tag_attributes(xml, node)?;
    let mut found: Option<String> = None;
    scan_attributes(&tag, &mut |attr_name, has_value| {
        if attr_name == name {
            // Valueless: legacy drops it, so it clears any earlier valued
            // duplicate and never resolves.
            found = has_value.map(|value| js_trim(&normalize_line_endings(value)).to_owned());
        }
        true
    });
    found
}

/// The open tag's attribute span: everything after the element name up to
/// the closing `>` (quote-aware so `>` inside a value does not end the
/// scan, e.g. `href="a>b"`).
fn open_tag_attributes(xml: &str, node: Node<'_, '_>) -> Option<String> {
    let range = node.range();
    let open = open_tag_end(&xml[range.clone()]);
    let mut tag = &xml[range.start..range.start + open];
    // Drop the element name.
    let first = tag.find(|c: char| c.is_whitespace() || c == '/')?;
    tag = &tag[first..];
    Some(tag.to_owned())
}

/// Walk one open tag's attribute span, calling `visit` with each name and
/// its raw value (`None` when the attribute has no `=`). Quoted values may
/// contain anything except their own quote; unquoted values run to the next
/// ASCII whitespace. Returns `false` when the visitor stops early.
fn scan_attributes(tag: &str, visit: &mut dyn FnMut(&str, Option<&str>) -> bool) -> bool {
    let bytes = tag.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        let start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'/'
            && bytes[i] != b'>'
        {
            i += 1;
        }
        let attr_name = &tag[start..i];
        if attr_name.is_empty() {
            // Reached the closing `>` (or a stray separator): no more names.
            break;
        }
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let value = if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                let value = &tag[start..i];
                i = (i + 1).min(bytes.len());
                value
            } else {
                let start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                &tag[start..i]
            };
            if !visit(attr_name, Some(value)) {
                return false;
            }
        } else if !visit(attr_name, None) {
            return false;
        }
    }
    true
}

const DISCORD_MESSAGE_CONTENT_LIMIT: usize = 2000;
const MAX_YOUTUBE_ITEM_URL_UTF16_UNITS: usize =
    DISCORD_MESSAGE_CONTENT_LIMIT - "New YouTube upload: ****\n".len() - 1;
const MAX_RSS_TWITCH_ITEM_URL_UTF16_UNITS: usize =
    DISCORD_MESSAGE_CONTENT_LIMIT - "Twitch update: ****\n".len() - 1;

fn max_feed_item_url_utf16_units(kind: FeedKind) -> usize {
    match kind {
        FeedKind::Youtube => MAX_YOUTUBE_ITEM_URL_UTF16_UNITS,
        FeedKind::Rss | FeedKind::Twitch => MAX_RSS_TWITCH_ITEM_URL_UTF16_UNITS,
    }
}

fn parse_item_url(value: &str) -> Option<url::Url> {
    url::Url::parse(value).ok().filter(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
    })
}

fn is_item_url(value: &str, max_url_utf16_units: usize) -> bool {
    parse_item_url(value).is_some_and(|url| {
        item_url_for_message(&url)
            .is_some_and(|message_url| message_url.encode_utf16().count() <= max_url_utf16_units)
    })
}

/// Drops URLs the REST sanitizer rewrites and `](`, which Discord renders as a masked link.
fn item_url_for_message(url: &url::Url) -> Option<&str> {
    let url = url.as_str();
    let unsafe_for_message =
        crate::message_safety::contains_mass_mention(url) || url.contains("](");
    (!unsafe_for_message).then_some(url)
}

fn has_unsafe_message_url(value: &str) -> bool {
    parse_item_url(value).is_some_and(|url| item_url_for_message(&url).is_none())
}

pub fn item_key(item: &FeedItem) -> Result<String, FeedError> {
    let key = if js_trim(&item.key).is_empty() {
        js_trim(&item.url)
    } else {
        js_trim(&item.key)
    };
    if key.is_empty() {
        return Err(FeedError::MissingKey);
    }
    Ok(hex::encode(Sha256::digest(key.as_bytes())))
}

pub fn delivery_nonce(feed_id: &str, key: &str) -> String {
    hex::encode(Sha256::digest(format!("{feed_id}\0{key}").as_bytes()))[..24].to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedPost {
    pub feed_id: String,
    pub channel_id: String,
    pub item_key: String,
    pub nonce: String,
    pub content: String,
    pub suppress_mentions: bool,
    pub enforce_nonce: bool,
}

pub fn plan_post(feed: &FeedRelay, item: &FeedItem) -> Result<FeedPost, FeedError> {
    let key = item_key(item)?;
    let parsed_url = parse_item_url(&item.url).ok_or(FeedError::InvalidItemUrl)?;
    let item_url = item_url_for_message(&parsed_url).ok_or(FeedError::InvalidItemUrl)?;
    if item_url.encode_utf16().count() > max_feed_item_url_utf16_units(feed.kind) {
        return Err(FeedError::InvalidItemUrl);
    }
    let prefix = match feed.kind {
        FeedKind::Youtube => "New YouTube upload",
        FeedKind::Twitch => "Twitch update",
        FeedKind::Rss => "New feed item",
    };
    let raw_title = item.title.trim();
    let raw_title = if raw_title.is_empty() {
        "Untitled"
    } else {
        raw_title
    };
    let fixed_units = format!("{prefix}: ****\n{item_url}").encode_utf16().count();
    if fixed_units >= DISCORD_MESSAGE_CONTENT_LIMIT {
        return Err(FeedError::InvalidItemUrl);
    }
    let title = crate::message_safety::neutralize_mentions(raw_title);
    let title = truncate_escaped_markdown(&title, DISCORD_MESSAGE_CONTENT_LIMIT - fixed_units);
    Ok(FeedPost {
        feed_id: feed.id.clone(),
        channel_id: feed.channel_id.clone(),
        nonce: delivery_nonce(&feed.id, &key),
        item_key: key,
        content: truncate_utf16(
            &format!("{prefix}: **{title}**\n{item_url}"),
            DISCORD_MESSAGE_CONTENT_LIMIT,
        ),
        suppress_mentions: true,
        enforce_nonce: true,
    })
}

/// Escape punctuation and flatten line breaks in feed-controlled text before
/// inserting it into Discord's Markdown message content.
fn escape_markdown(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\r' | '\n') {
            escaped.push(' ');
        } else {
            if ch.is_ascii_punctuation() {
                escaped.push('\\');
            }
            escaped.push(ch);
        }
    }
    escaped
}

fn truncate_escaped_markdown(value: &str, limit: usize) -> String {
    let escaped = escape_markdown(value);
    if escaped.encode_utf16().count() <= limit {
        return escaped;
    }
    if limit == 0 {
        return String::new();
    }

    let mut truncated = truncate_utf16(&escaped, limit - 1);
    let trailing_backslashes = truncated.chars().rev().take_while(|ch| *ch == '\\').count();
    if trailing_backslashes % 2 == 1 {
        truncated.pop();
    }
    truncated.push('…');
    truncated
}

fn truncate_utf16(value: &str, limit: usize) -> String {
    let mut units = 0;
    value
        .chars()
        .take_while(|ch| {
            units += ch.len_utf16();
            units <= limit
        })
        .collect()
}

/// Walk the entire newest-first listing in legacy oldest-first order. The
/// executor applies the 20-post cap *after* claim checks, not to this listing.
pub fn poll_candidates(feed: &FeedRelay, items: &[FeedItem]) -> Vec<Result<FeedPost, FeedError>> {
    let mut seen = HashSet::new();
    items
        .iter()
        .rev()
        .filter_map(|item| {
            let plan = plan_post(feed, item);
            match &plan {
                Ok(post) if !seen.insert(post.item_key.clone()) => None,
                _ => Some(plan),
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryClaim {
    Fresh,
    Recovered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryAction {
    Send,
    ReconcileByNonce,
    Defer,
}

pub fn delivery_action(claim: DeliveryClaim, posted_this_poll: usize) -> DeliveryAction {
    match claim {
        // An expired claim might have posted before the crash. Never blindly
        // resend: Discord nonce enforcement has only a short dedupe window.
        DeliveryClaim::Recovered => DeliveryAction::ReconcileByNonce,
        DeliveryClaim::Fresh if posted_this_poll >= MAX_FEED_POSTS_PER_POLL => {
            DeliveryAction::Defer
        }
        DeliveryClaim::Fresh => DeliveryAction::Send,
    }
}

/// Injectable-clock equivalent of the legacy immediate tick + non-overlapping
/// interval. The adapter obtains seconds from FeatureGates::feed_poll_seconds,
/// and always calls finish after a pass, including a failed one.
#[derive(Debug, Clone)]
pub struct FeedPollSchedule {
    interval_ms: i64,
    next_ms: i64,
    last_begin_ms: Option<i64>,
    running: bool,
}

impl FeedPollSchedule {
    pub fn new(seconds: u64) -> Result<Self, crate::feature_commands::GateError> {
        if !(60..=86400).contains(&seconds) {
            return Err(crate::feature_commands::GateError::InvalidFeedPoll(
                seconds.to_string(),
            ));
        }
        Ok(Self {
            interval_ms: seconds as i64 * 1000,
            next_ms: i64::MIN,
            last_begin_ms: None,
            running: false,
        })
    }

    pub fn begin(&mut self, now_ms: i64) -> bool {
        if self.running || now_ms < self.next_ms {
            return false;
        }
        self.running = true;
        self.last_begin_ms = Some(now_ms);
        self.next_ms = now_ms.saturating_add(self.interval_ms);
        true
    }

    pub fn finish(&mut self) {
        self.running = false;
    }

    /// Hot-reload the poll interval without a restart (TOG-19027): a changed
    /// interval reschedules the pending run to last-begin + new interval, in
    /// both directions, mirroring the supervisor's cadence policy so the
    /// timer and this gate admit the same slot. Before the first run there
    /// is no pending slot to move: the first `begin` stays immediate and
    /// counts the new interval from itself. Out-of-range and unchanged
    /// values are refused and keep the current schedule. Returns true when
    /// the interval changed.
    pub fn set_interval_seconds(&mut self, seconds: u64) -> bool {
        if !(60..=86400).contains(&seconds) {
            return false;
        }
        let interval_ms = seconds as i64 * 1000;
        if interval_ms == self.interval_ms {
            return false;
        }
        self.interval_ms = interval_ms;
        if let Some(last_begin_ms) = self.last_begin_ms {
            self.next_ms = last_begin_ms.saturating_add(interval_ms);
        }
        true
    }

    /// Current interval, for live-reload tests.
    #[must_use]
    pub fn interval_seconds(&self) -> u64 {
        (self.interval_ms / 1000).max(0) as u64
    }
}
