//! Pure V10 `/ping` and `/invite` reply builders, plus the read-only
//! `/myroom` owned-room listing.
//!
//! The runtime answers these utilities with ephemeral replies; this module
//! computes only the reply text from the spec behaviour in
//! `docs/voice-rooms.md` (V10 utilities). It performs no I/O, holds no
//! Discord, store or clock types, and depends on no voice runtime state.
//!
//! - [`ping_render`] turns a measured round-trip time into a bounded
//!   human-readable latency line. Absurd values saturate at
//!   [`MAX_PING_DISPLAY_MS`], never panic.
//! - [`invite_render`] turns the configured guild invite code into an invite
//!   line, or into a fixed notice when none is configured. Codes are treated
//!   as opaque tokens: only charset/length-validated codes are rendered, full
//!   URLs and other malformed input are refused, and refused input is never
//!   echoed back (a stale code may be a rotated secret).
//! - [`myroom_render`] turns the caller's owned live rooms into one line per
//!   room channel with its runtime privacy word, or into [`MYROOM_EMPTY`]
//!   when they own nothing live. The caller supplies the pairs from live
//!   worker state; only the caller's own rooms may be passed in.

use crate::Snowflake;

/// Round-trip times above this render as this value with a `+` suffix, so
/// absurd measurements saturate instead of printing unbounded digits.
pub const MAX_PING_DISPLAY_MS: u64 = 30_000;

/// Invite codes shorter than this are refused; a code this short cannot be a
/// real guild invite.
pub const MIN_INVITE_CODE_CHARS: usize = 2;

/// Invite codes longer than this are refused without being echoed.
pub const MAX_INVITE_CODE_CHARS: usize = 32;

/// Reply when no invite code is configured (or the configured value is empty).
pub const NO_INVITE_CONFIGURED: &str =
    "No invite is configured for this server yet; ask an admin to set one up.";

/// Reply when the configured code fails validation. The code itself is never
/// included, so a stale or rotated secret is not leaked into chat.
pub const INVALID_INVITE_CODE: &str =
    "The configured invite code is not usable; ask an admin to check the server settings.";

/// Human-readable latency line for a measured round-trip time.
///
/// Under one second the reply names milliseconds; at or above it names
/// seconds with one decimal. Inputs above [`MAX_PING_DISPLAY_MS`] saturate
/// there and gain a `+` suffix, so even `u64::MAX` renders a short line and
/// the function cannot panic.
#[must_use]
pub fn ping_render(rtt_ms: u64) -> String {
    let saturated = rtt_ms > MAX_PING_DISPLAY_MS;
    let shown = rtt_ms.min(MAX_PING_DISPLAY_MS);
    let mut out = if shown < 1_000 {
        format!("Pong! {shown}ms")
    } else {
        format!("Pong! {:.1}s", shown as f64 / 1_000.0)
    };
    if saturated {
        out.push('+');
    }
    out
}

/// Whether `code` is renderable as an opaque invite token: ASCII alphanumeric
/// plus `-`/`_`, within length bounds. Anything URL-shaped (`://`, `/`, `.`,
/// whitespace, query or fragment markers) fails the charset, so a pasted link
/// is refused rather than embedded or fetched.
#[must_use]
pub fn is_valid_invite_code(code: &str) -> bool {
    (MIN_INVITE_CODE_CHARS..=MAX_INVITE_CODE_CHARS).contains(&code.len())
        && code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Invite line for the configured guild invite code, or a fixed notice.
///
/// - `None` (or an empty string) means no invite is configured.
/// - A charset/length-valid code renders as a `discord.gg` link line; the
///   code is embedded as an opaque token and never fetched or normalised.
/// - Anything else renders [`INVALID_INVITE_CODE`] verbatim, without echoing
///   the rejected value.
#[must_use]
pub fn invite_render(guild_invite_code: Option<&str>) -> String {
    match guild_invite_code {
        None | Some("") => NO_INVITE_CONFIGURED.to_owned(),
        Some(code) if is_valid_invite_code(code) => {
            format!("Join the server: https://discord.gg/{code}")
        }
        Some(_) => INVALID_INVITE_CODE.to_owned(),
    }
}

/// Reply when the `/myroom` caller owns no live room: truthful, with the
/// next action (join a creator lobby).
pub const MYROOM_EMPTY: &str =
    "You own no live voice room right now. Join a creator lobby to start one.";

/// Owned-room listing for `/myroom`.
///
/// `rooms` carries `(room channel, runtime private)` pairs in channel order
/// from live worker state. Each owned room renders on its own line with its
/// privacy word; an empty list renders [`MYROOM_EMPTY`]. Long lists omit only
/// whole rows and state the omitted count, staying below 2000 scalars before
/// the completion adapter. The function performs no lookup of its own, so it
/// can neither leak another member's rooms nor invent channels.
#[must_use]
pub fn myroom_render(rooms: &[(Snowflake, bool)]) -> String {
    if rooms.is_empty() {
        return MYROOM_EMPTY.to_owned();
    }
    let mut out = if rooms.len() == 1 {
        "You own 1 live voice room:\n".to_owned()
    } else {
        format!("You own {} live voice rooms:\n", rooms.len())
    };
    // Reserve the largest possible omission footer before adding whole rows.
    // The completion adapter's 2000-scalar clamp must never split a room label.
    let footer_bound = format!("{} more rooms omitted.", rooms.len())
        .chars()
        .count();
    let mut shown = 0;
    let mut length = out.chars().count();
    for (channel, private) in rooms {
        let state = if *private { "private" } else { "public" };
        let line = format!("<#{channel}> — {state}\n");
        let line_length = line.chars().count();
        if length + line_length + footer_bound > 2000 {
            break;
        }
        out.push_str(&line);
        length += line_length;
        shown += 1;
    }
    if shown < rooms.len() {
        out.push_str(&format!("{} more rooms omitted.", rooms.len() - shown));
    }
    out.trim_end_matches('\n').to_owned()
}
