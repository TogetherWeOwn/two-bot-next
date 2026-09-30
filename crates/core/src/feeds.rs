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
    let entries: Vec<_> = match root.tag_name().name() {
        "rss" => root
            .children()
            .find(|node| named(*node, "channel"))
            .into_iter()
            .flat_map(|channel| channel.children())
            .filter(|node| named(*node, "item"))
            .collect(),
        "feed" => root
            .children()
            .filter(|node| named(*node, "entry"))
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
                .filter(|node| named(*node, "link"))
                .collect();
            let link = links
                .iter()
                .find(|node| {
                    node.attribute("href").is_some()
                        && node
                            .attribute("rel")
                            .unwrap_or("alternate")
                            .eq_ignore_ascii_case("alternate")
                })
                .or_else(|| {
                    links
                        .iter()
                        .find(|node| node.attribute("href").is_some_and(is_item_url))
                })
                .or_else(|| links.first());
            let url = link
                .map(|node| {
                    node.attribute("href")
                        .map(str::to_owned)
                        .unwrap_or_else(|| text(*node))
                })
                .unwrap_or_default()
                .trim()
                .to_owned();
            let key = [
                child_text(entry, "guid"),
                child_text(entry, "id"),
                url.clone(),
            ]
            .into_iter()
            .find(|key| !key.is_empty())
            .unwrap_or_default();
            if key.is_empty() || !is_item_url(&url) {
                return None;
            }
            let title = child_text(entry, "title");
            let published_at = ["pubDate", "published", "updated"]
                .into_iter()
                .map(|name| child_text(entry, name))
                .find(|value| !value.is_empty());
            Some(FeedItem {
                key,
                title: if title.is_empty() {
                    "Untitled".into()
                } else {
                    title
                },
                url,
                published_at,
            })
        })
        .collect())
}

fn named(node: Node<'_, '_>, name: &str) -> bool {
    node.is_element() && node.tag_name().name() == name
}

fn text(node: Node<'_, '_>) -> String {
    node.descendants()
        .filter(|node| node.is_text())
        .filter_map(|node| node.text())
        .collect::<String>()
        .trim()
        .to_owned()
}

fn child_text(node: Node<'_, '_>, name: &str) -> String {
    node.children()
        .find(|child| named(*child, name))
        .map(text)
        .unwrap_or_default()
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
    let key = if item.key.trim().is_empty() {
        item.url.trim()
    } else {
        item.key.trim()
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
