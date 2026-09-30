//! Framework-free feed commands, parsing and polling decisions.
//! Side effects belong to the shared router/REST executor, not this module.

use crate::feeds_http::{validate_source, FetchError, MAX_FEED_BYTES};
use roxmltree::{Document, Node, ParsingOptions};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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
    List,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedCommandPlan {
    Add(FeedRelay),
    Remove { guild_id: String, id: String },
    List { guild_id: String },
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
        FeedCommand::List => Ok(FeedCommandPlan::List {
            guild_id: context.guild_id.into(),
        }),
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

pub fn feed_list_text(feeds: &[FeedRelay]) -> String {
    if feeds.is_empty() {
        return "No feed relays configured.".into();
    }
    truncate_utf16(
        &feeds
            .iter()
            .map(|feed| {
                format!(
                    "`{}` {} → <#{}> {}",
                    feed.id,
                    feed.kind.as_str(),
                    feed.channel_id,
                    feed.source
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        2000,
    )
}

pub fn feed_removed_text(removed: bool) -> &'static str {
    if removed {
        "Feed relay removed."
    } else {
        "No feed relay with that id."
    }
}

pub fn parse_xml_feed(xml: &str) -> Result<Vec<FeedItem>, FeedError> {
    if xml.len() > MAX_FEED_BYTES {
        return Err(FetchError::TooLarge.into());
    }
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
        "rss" => root
            .children()
            .find(|node| named(xml, *node, "channel"))
            .into_iter()
            .flat_map(|channel| channel.children())
            .filter(|node| named(xml, *node, "item"))
            .collect(),
        "feed" => root
            .children()
            .filter(|node| named(xml, *node, "entry"))
            .collect(),
        _ => return Err(FeedError::InvalidXml),
    };
    if entries.len() > MAX_FEED_ITEMS {
        return Err(FeedError::TooManyItems);
    }
    Ok(entries
        .into_iter()
        .filter_map(|entry| {
            let links: Vec<_> = entry
                .children()
                .filter(|node| named(xml, *node, "link"))
                .collect();
            // Legacy `resolveLink` tests the RAW href: only an alternate whose
            // href is itself HTTP(S) is promoted. A mailto alternate must not
            // shadow the valid HTTPS alternate that follows it.
            let link = links
                .iter()
                .find(|node| is_alternate(xml, **node) && is_http_href(&link_href_raw(xml, **node)))
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
                });
            let raw_url = link
                .map(|node| link_href_raw(xml, *node))
                .unwrap_or_default();
            // Legacy `textValue` short-circuits on the raw text: guid, then
            // id, then the raw link. Identity decoding runs after selection
            // so delivery keys hash the exact legacy form.
            let raw_key = [raw_text(xml, entry, "guid"), raw_text(xml, entry, "id")]
                .into_iter()
                .find(|key| !key.is_empty())
                .unwrap_or(raw_url.clone());
            let key = decode_xml_entities(&raw_key);
            let url = decode_xml_entities(&raw_url);
            if key.is_empty() || !is_item_url(&url) {
                return None;
            }
            let raw_title = raw_text(xml, entry, "title");
            let published_at = ["pubDate", "published", "updated"]
                .into_iter()
                .map(|name| raw_text(xml, entry, name))
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

/// Keep only the element's own character data the way legacy `textValue`
/// keeps only `#text`: nested elements contribute nothing (not even their
/// content), like legacy's `#text` read for `<b>Bold</b> tail` yielding
/// just `tail`. CDATA contributes its raw body, markers excluded.
/// Numeric references pass through untouched; NOT decoded here.
fn raw_text(xml: &str, entry: Node<'_, '_>, name: &str) -> String {
    let text = entry
        .children()
        .find(|child| named(xml, *child, name))
        .map(|node| strip_markup(inner_source(xml, node)))
        .unwrap_or_default();
    js_trim(&text).to_owned()
}

/// Keep only the element's own character data the way legacy `textValue`
/// keeps only `#text`: PIs, comments and nested elements (including their
/// content) contribute nothing, and a CDATA section contributes its raw
/// body (markers excluded). Numeric references pass through untouched;
/// they are NOT decoded here.
fn strip_markup(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    while i < bytes.len() {
        if source[i..].starts_with("<![CDATA[") {
            let rest = &source[i + 9..];
            let end = rest.find("]]>").map(|j| i + 9 + j).unwrap_or(source.len());
            out.push_str(&source[i + 9..end]);
            i = (end + 3).min(source.len());
        } else if source[i..].starts_with("<!--") {
            let end = source[i..]
                .find("-->")
                .map(|j| i + j + 3)
                .unwrap_or(source.len());
            i = end;
        } else if source[i..].starts_with("<?") {
            let end = source[i..]
                .find("?>")
                .map(|j| i + j + 2)
                .unwrap_or(source.len());
            i = end;
        } else if source[i..].starts_with("</") {
            i = tag_end(source, i);
        } else if bytes[i] == b'<' {
            // Nested element: drop the whole subtree, not just its tags.
            i = skip_element(source, i);
        } else {
            let end = source[i..].find('<').map(|j| i + j).unwrap_or(source.len());
            out.push_str(&source[i..end]);
            i = end;
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
/// 'alternate'`.
fn is_alternate(xml: &str, node: Node<'_, '_>) -> bool {
    match raw_attribute(xml, node, "rel") {
        None => true,
        Some(rel) => rel.is_empty() || rel.eq_ignore_ascii_case("alternate"),
    }
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
/// present, and an attribute-bearing element without `href` contributes
/// nothing (legacy only consults `record.href`, never the text body).
/// A bare text link resolves to its stripped raw text.
fn link_href_raw(xml: &str, node: Node<'_, '_>) -> String {
    if let Some(href) = raw_attribute(xml, node, "href") {
        return href;
    }
    if node.attributes().next().is_none() {
        return js_trim(&strip_markup(inner_source(xml, node))).to_owned();
    }
    String::new()
}

/// Read one attribute value from the source slice so entity references
/// survive (`href="...?a=1&amp;b=2"` keeps the raw `&amp;`). Matches
/// legacy's attribute-name rule: case-sensitive, first match wins, and a
/// missing `=` contributes nothing.
fn raw_attribute(xml: &str, node: Node<'_, '_>, name: &str) -> Option<String> {
    let range = node.range();
    // Quote-aware: a `>` inside an attribute value must not end the scan.
    let open = open_tag_end(&xml[range.clone()]);
    let mut tag = &xml[range.start..range.start + open];
    // Drop the element name.
    let first = tag.find(|c: char| c.is_whitespace() || c == '/')?;
    tag = &tag[first..];
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
                let value = tag[start..i].to_owned();
                i = (i + 1).min(bytes.len());
                value
            } else {
                let start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                tag[start..i].to_owned()
            };
            if attr_name == name {
                return Some(js_trim(&value).to_owned());
            }
        } else if attr_name == name {
            // Valueless attribute: present but contributes nothing.
            return None;
        }
    }
    None
}

fn is_item_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
    })
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
    let prefix = match feed.kind {
        FeedKind::Youtube => "New YouTube upload",
        FeedKind::Twitch => "Twitch update",
        FeedKind::Rss => "New feed item",
    };
    let title = if item.title.trim().is_empty() {
        "Untitled"
    } else {
        item.title.trim()
    };
    Ok(FeedPost {
        feed_id: feed.id.clone(),
        channel_id: feed.channel_id.clone(),
        nonce: delivery_nonce(&feed.id, &key),
        item_key: key,
        content: truncate_utf16(&format!("{prefix}: **{title}**\n{}", item.url), 2000),
        suppress_mentions: true,
        enforce_nonce: true,
    })
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
            running: false,
        })
    }

    pub fn begin(&mut self, now_ms: i64) -> bool {
        if self.running || now_ms < self.next_ms {
            return false;
        }
        self.running = true;
        self.next_ms = now_ms.saturating_add(self.interval_ms);
        true
    }

    pub fn finish(&mut self) {
        self.running = false;
    }
}
