//! Read-only, current-channel LFG discovery: authenticated keysets and whole rows.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, KeyInit, Mac as _};
use sha2::Sha256;
use std::sync::OnceLock;
use time::OffsetDateTime;

use crate::{lfg, message_safety, Secret};

pub const PAGE_ROWS: usize = 5;
pub const MAX_CURSOR_LEN: usize = 512;
pub const EMPTY: &str = "No open game posts in this channel.";
pub const INVALID_PAGE: &str = "This list page is invalid or stale. Run /lfg-list again.";
pub const UNAVAILABLE: &str = "Unable to list game posts. Try /lfg-list again.";
const END: &str = "End of list.";
const QUERY: &[u8] = b"lfg-list:v1:open-confirmed:created_at,id:5";

/// Shared within one process, including across runtime instances. Restarts make
/// old pages stale; no credential provisioning or unbounded cursor cache needed.
static CURSOR_KEY: OnceLock<Secret<[u8; 32]>> = OnceLock::new();

#[derive(Debug)]
pub struct ListCursor {
    pub created_at: OffsetDateTime,
    pub id: String,
}

fn cursor_mac(guild: &str, channel: &str, payload: &[u8]) -> Hmac<Sha256> {
    let key = CURSOR_KEY.get_or_init(|| Secret::new(rand::random()));
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key.expose()).expect("HMAC accepts keys of any length");
    mac.update(QUERY);
    for field in [guild.as_bytes(), channel.as_bytes(), payload] {
        mac.update(&(field.len() as u64).to_be_bytes());
        mac.update(field);
    }
    mac
}

impl ListCursor {
    fn encode(&self, guild: &str, channel: &str) -> String {
        let mut payload = vec![1];
        payload.extend(self.created_at.unix_timestamp_nanos().to_be_bytes());
        // Two bytes per UTF-16 unit guarantees even a maximum multibyte ID
        // fits: 1 + 16 + 256 + 32 bytes => at most 407 URL-safe characters.
        for unit in self.id.encode_utf16() {
            payload.extend(unit.to_be_bytes());
        }
        let tag = cursor_mac(guild, channel, &payload).finalize().into_bytes();
        payload.extend(tag);
        URL_SAFE_NO_PAD.encode(payload)
    }

    /// Validate shape, signature, scope, ID and timestamp before any SQL access.
    pub fn decode(token: &str, guild: &str, channel: &str) -> Option<Self> {
        if token.is_empty() || token.len() > MAX_CURSOR_LEN {
            return None;
        }
        let bytes = URL_SAFE_NO_PAD.decode(token).ok()?;
        if bytes.len() < 51 || bytes.len() > 305 || bytes[0] != 1 {
            return None;
        }
        let (payload, tag) = bytes.split_at(bytes.len() - 32);
        cursor_mac(guild, channel, payload).verify_slice(tag).ok()?;
        let nanos = i128::from_be_bytes(payload[1..17].try_into().ok()?);
        let created_at = OffsetDateTime::from_unix_timestamp_nanos(nanos).ok()?;
        let encoded_id = &payload[17..];
        if !encoded_id.len().is_multiple_of(2) {
            return None;
        }
        let units: Vec<_> = encoded_id
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_be_bytes(*pair))
            .collect();
        let id = String::from_utf16(&units).ok()?;
        lfg::valid_post_id(&id).then_some(Self { created_at, id })
    }
}

/// Only public discovery fields, never member, role, signup or audit data.
#[derive(Debug)]
pub struct ListPost {
    pub id: String,
    pub title: String,
    pub message_id: String,
    pub starts_at: Option<i64>,
    pub created_at: OffsetDateTime,
}

pub fn canonical_snowflake(value: &str) -> bool {
    value
        .parse::<u64>()
        .is_ok_and(|id| id != 0 && id.to_string() == value)
}

/// Keep Unicode and complete names, but not mentions, links or active markdown.
fn inert_title(text: &str) -> String {
    let mut safe = String::new();
    for ch in text.chars() {
        match ch {
            '@' => safe.push_str("@\u{200b}"),
            ':' => safe.push_str(":\u{200b}"),
            '\n' | '\r' | '\t' => safe.push(' '),
            '\\' | '*' | '_' | '~' | '`' | '[' | ']' | '(' | ')' | '<' | '>' | '#' | '|' | '!' => {
                safe.push('\\');
                safe.push(ch);
            }
            ch if ch.is_control()
                || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') =>
            {
                safe.push_str(&ch.escape_unicode().to_string());
            }
            _ => safe.push(ch),
        }
    }
    safe
}

fn quoted_id(id: &str) -> String {
    // A delimiter longer than any run in the ID cannot be closed by that ID.
    // Space padding preserves IDs that begin/end in backticks. Control chars
    // use their explicit Unicode spelling so a stored ID cannot add fake rows.
    let longest = id.split(|ch| ch != '`').map(str::len).max().unwrap_or(0);
    let delimiter = "`".repeat(longest + 1);
    let visible: String = id
        .chars()
        .flat_map(|ch| {
            if ch.is_control() || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                ch.escape_unicode().to_string().chars().collect::<Vec<_>>()
            } else {
                vec![ch]
            }
        })
        .collect();
    if longest == 0 {
        format!("`{visible}`")
    } else {
        format!("{delimiter} {visible} {delimiter}")
    }
}

fn footer(post: &ListPost, guild: &str, channel: &str) -> String {
    let token = ListCursor {
        created_at: post.created_at,
        id: post.id.clone(),
    }
    .encode(guild, channel);
    format!("Continue: `/lfg-list cursor:{token}`")
}

/// At most five whole rows plus a continuation. The caller supplies at most
/// six rows (one lookahead). Never silently truncate a row, ID, title or footer.
/// Malformed persisted identities/oversized individual rows fail explicitly.
pub fn render_page(
    guild: &str,
    channel: &str,
    posts: &[ListPost],
    continued: bool,
    now: i64,
) -> Result<String, &'static str> {
    if !canonical_snowflake(guild) || !canonical_snowflake(channel) || posts.len() > PAGE_ROWS + 1 {
        return Err(UNAVAILABLE);
    }
    if posts.is_empty() {
        return Ok(if continued { END } else { EMPTY }.into());
    }
    let mut content = String::new();
    let mut rendered = 0_usize;
    for post in posts.iter().take(PAGE_ROWS) {
        if !lfg::valid_post_id(&post.id) || !canonical_snowflake(&post.message_id) {
            return Err(UNAVAILABLE);
        }
        let start = match post.starts_at {
            Some(start) if start < now => format!("<t:{start}:f> (past)"),
            Some(start) => format!("<t:{start}:f>"),
            None => "Start time unknown".into(),
        };
        let row = format!(
            "{} — {} — {start} — <https://discord.com/channels/{guild}/{channel}/{}>\n",
            quoted_id(&post.id),
            inert_title(&post.title),
            post.message_id,
        );
        // Account for the same mention normalization as the REST boundary,
        // including unusual persisted IDs, before reserving the whole row.
        let row = message_safety::neutralize_mentions(&row);
        // Reserve this row's actual continuation, not a rough byte estimate.
        let end = if rendered + 1 < posts.len() {
            footer(post, guild, channel)
        } else {
            END.into()
        };
        if message_safety::text_len(&content)
            + message_safety::text_len(&row)
            + message_safety::text_len(&end)
            > message_safety::CONTENT_LIMIT
        {
            break;
        }
        content.push_str(&row);
        rendered += 1;
    }
    let Some(last) = rendered.checked_sub(1).and_then(|n| posts.get(n)) else {
        return Err(UNAVAILABLE);
    };
    if rendered < posts.len() {
        content.push_str(&footer(last, guild, channel));
    } else {
        content.push_str(END);
    }
    Ok(content)
}
