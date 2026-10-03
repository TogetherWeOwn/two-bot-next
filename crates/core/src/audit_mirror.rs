//! Mirror transport contract for the operational audit sink (TOG-10345).
//!
//! Framework-free seam: the service drives any [`AuditMirror`] implementation
//! (the twilight adapter in `two-bot-discord`, a scripted double in tests)
//! through the same crash-safe protocol. Everything a send needs to know about
//! Discord is reduced to three reads/writes — post, channel document, history
//! page — plus the pure gates that decide whether a send is legal at all.

use std::future::Future;

/// `VIEW_CHANNEL` (1 << 10) in an overwrite deny mask: the privacy gate.
pub const VIEW_CHANNEL_BIT: u64 = 1 << 10;
/// Discord's own page-size ceiling for `GET /channels/{c}/messages`.
pub const HISTORY_PAGE_LIMIT: u8 = 100;
/// Pages scanned for the pre-send dedup check when no durable boundary exists
/// (legacy `findMirror`: at most 5 pages of 100 without a cursor).
pub const DEDUP_PAGE_LIMIT: usize = 5;

/// Discord-side failure, classified for the crash protocol.
///
/// `Rejected` is the only verdict that proves no mutation happened: a non-429
/// 4xx, or a client-side refusal before any wire send. `RateLimited` proves a
/// POST never reached the handler (Discord rejects before applying), but is
/// *not* proof on the read side — a rate-limited history read just means "try
/// later". `Uncertain` (timeout, transport, 5xx) leaves delivery ambiguous in
/// both directions. The service maps each variant per operation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MirrorError {
    /// The request was refused: provably nothing was posted / no read ran.
    #[error("mirror request refused: {0}")]
    Rejected(String),
    /// Discord rate-limited the request before applying it.
    #[error("mirror request rate-limited")]
    RateLimited,
    /// Timeout, transport failure or 5xx: the outcome is unknown.
    #[error("mirror request outcome is uncertain: {0}")]
    Uncertain(String),
}

/// The @everyone overwrite row on a mirror channel (decimal-string masks, the
/// shape Discord returns on the wire).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorOverwrite {
    pub allow: String,
    pub deny: String,
}

/// The privacy-relevant fields of a `GET /channels/{c}` document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorChannel {
    /// Guild the channel belongs to. `""` when the document omits the field —
    /// never guessed: a guild-less document cannot pass the guild fence.
    pub guild_id: String,
    /// The `@everyone` row (type 0 whose id is the channel's guild), when the
    /// document carries one.
    pub everyone: Option<MirrorOverwrite>,
}

/// One channel-history entry reduced to what reconciliation matches on:
/// identity, author, and the marked content line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorMessage {
    pub id: String,
    pub author_id: String,
    pub content: String,
}

/// The mirror endpoint: everything the audit service needs from Discord.
///
/// Implementations must suppress allowed mentions and send the given nonce
/// with `enforce_nonce: true` on posts; both are part of the contract, not
/// options. Returning `Ok("")` from `post_mirror` means Discord accepted the
/// post but omitted the id — the caller must reconcile, never resend.
pub trait AuditMirror: Send + Sync {
    /// `POST /channels/{c}/messages` with the formatted audit line and the
    /// stored deterministic nonce. Returns the accepted message id.
    fn post_mirror(
        &self,
        channel_id: &str,
        content: &str,
        nonce: &str,
    ) -> impl Future<Output = Result<String, MirrorError>> + Send {
        async move {
            match self
                .post_mirror_checked(channel_id, content, nonce, async {
                    Ok::<(), std::convert::Infallible>(())
                })
                .await
            {
                Ok(result) => result,
                Err(never) => match never {},
            }
        }
    }

    /// Reserve the shared pacing lane, then authorize immediately before POST.
    /// No further pacing/queue wait may occur after authorization. A refused
    /// authorization makes zero wire calls; the outer error belongs to the
    /// caller (e.g. lost lease, halt or failed DB read), not to Discord.
    fn post_mirror_checked<Fut, E>(
        &self,
        channel_id: &str,
        content: &str,
        nonce: &str,
        authorize: Fut,
    ) -> impl Future<Output = Result<Result<String, MirrorError>, E>> + Send
    where
        Fut: Future<Output = Result<(), E>> + Send,
        E: Send;

    /// `GET /channels/{c}` reduced to the fields the privacy gate reads.
    fn channel_document(
        &self,
        channel_id: &str,
    ) -> impl Future<Output = Result<MirrorChannel, MirrorError>> + Send;

    /// `GET /channels/{c}/messages`, newest-first; `before` is the previous
    /// page's floor id and `limit` is clamped to Discord's 1..=100 range.
    fn channel_history(
        &self,
        channel_id: &str,
        before: Option<&str>,
        limit: u8,
    ) -> impl Future<Output = Result<Vec<MirrorMessage>, MirrorError>> + Send;
}

/// Verdict of the privacy/guild gate on a fetched channel document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorGate {
    /// Private channel inside the event's own guild: mirroring is legal.
    Clear,
    /// The document's guild does not equal the event's guild (or is absent):
    /// the recorded destination cannot belong to this event.
    WrongGuild,
    /// `@everyone` can still view the channel: the channel's privacy was
    /// revoked since the destination was configured.
    PublicChannel,
}

/// Privacy + guild fence (legacy `isPrivateMirror` / `channelFor` gate).
///
/// Clear requires both: `channel.guild_id == event_guild_id`, and an
/// `@everyone` overwrite whose deny mask carries [`VIEW_CHANNEL_BIT`]. A
/// missing overwrite row or a non-numeric deny mask cannot prove privacy, so
/// it is `PublicChannel`. `WrongGuild` wins over `PublicChannel`: a channel in
/// a foreign guild is an evidence conflict, not a permission problem.
#[must_use]
pub fn mirror_channel_policy(channel: &MirrorChannel, event_guild_id: &str) -> MirrorGate {
    if channel.guild_id != event_guild_id {
        return MirrorGate::WrongGuild;
    }
    let Some(overwrite) = &channel.everyone else {
        return MirrorGate::PublicChannel;
    };
    let deny = overwrite.deny.parse::<u64>().unwrap_or(0);
    if deny & VIEW_CHANNEL_BIT != 0 {
        MirrorGate::Clear
    } else {
        MirrorGate::PublicChannel
    }
}

/// `newestMessageCursor` seed: a snowflake's successor (`newest_id + 1`).
/// `None` when `id` is not a u64 snowflake or would overflow — the caller
/// treats that as an unreadable page, not as an empty channel.
#[must_use]
pub fn next_snowflake(id: &str) -> Option<String> {
    let value: u64 = id.parse().ok()?;
    Some(value.checked_add(1)?.to_string())
}

/// Numeric `id >= boundary` over snowflake strings (legacy `findMirror`'s
/// `id >= searchBefore`). Unparseable input never matches.
#[must_use]
pub fn snowflake_at_or_after(id: &str, boundary: &str) -> bool {
    match (id.parse::<u128>(), boundary.parse::<u128>()) {
        (Ok(id), Ok(boundary)) => id >= boundary,
        _ => false,
    }
}

/// Scan one newest-first history page for this event's already-posted mirror:
/// authored by this bot, carrying the exact `audit-event:{entry_id};` marker,
/// and — when a durable boundary exists — at or after it. Returns the newest
/// match's message id (pages arrive newest-first, so the first hit wins).
#[must_use]
pub fn find_mirror_in_page(
    page: &[MirrorMessage],
    entry_id: &str,
    bot_user_id: &str,
    boundary: Option<&str>,
) -> Option<String> {
    page.iter()
        .filter(|message| message.author_id == bot_user_id)
        .filter(|message| crate::audit::has_audit_event_identity(&message.content, entry_id))
        .filter(|message| boundary.is_none_or(|b| snowflake_at_or_after(&message.id, b)))
        .map(|message| message.id.clone())
        .next()
}

/// Hourly mirror recheck cadence (parity §5 / soak s5-09: the
/// `operational_audit_log` hourly mirror recheck). The TOG-12240 retry sweep
/// runtime owns the timer and the Discord reads; this constant is only the
/// pure due-gate boundary it compares against.
pub const MIRROR_RECHECK_INTERVAL_MS: u64 = 60 * 60 * 1000;

/// Pure due gate for the hourly mirror recheck: true once at least one hour
/// has elapsed since `last_recheck_ms`. Times are unix millis. A `now_ms`
/// behind `last_recheck_ms` (clock skew or a fresh process with a persisted
/// cursor) saturates to "not due" rather than underflowing — the recheck is
/// a bounded skip, never a catch-up burst.
#[must_use]
pub fn mirror_recheck_due(last_recheck_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(last_recheck_ms) >= MIRROR_RECHECK_INTERVAL_MS
}

/// Why a mirror recheck tick recorded nothing (house skip pattern per the
/// scheduled-events precedent TOG-12700: a failed or malformed read keeps the
/// last good state in place, only a good response replaces it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorRecheckSkip {
    /// Recheck not due yet: no Discord read runs, last good state keeps.
    NotDue,
    /// Discord answered but the payload was unusable (malformed row): keeps
    /// the last good state, never publishes the malformed snapshot.
    InvalidResponse,
    /// The read itself failed (transport / rate-limit / 5xx): keeps the last
    /// good state. Distinct from `InvalidResponse` so operators can tell a
    /// dead wire apart from a corrupt body.
    DiscordReadFailed,
}

/// Oldest message id on the page — the next page's `before` cursor, and the
/// value compared against the boundary to know when a scan has passed below
/// every id that could still match.
#[must_use]
pub fn page_floor_id(page: &[MirrorMessage]) -> Option<String> {
    page.last().map(|message| message.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, author: &str, content: &str) -> MirrorMessage {
        MirrorMessage {
            id: id.to_owned(),
            author_id: author.to_owned(),
            content: content.to_owned(),
        }
    }

    const BOT: &str = "999";
    const ENTRY: &str = "18446744073709551615";

    #[test]
    fn private_same_guild_channel_is_clear() {
        let channel = MirrorChannel {
            guild_id: "2222".to_owned(),
            everyone: Some(MirrorOverwrite {
                allow: "0".to_owned(),
                deny: VIEW_CHANNEL_BIT.to_string(),
            }),
        };
        assert_eq!(mirror_channel_policy(&channel, "2222"), MirrorGate::Clear);
    }

    #[test]
    fn viewable_everyone_row_is_public() {
        for deny in ["0", "2048", &(VIEW_CHANNEL_BIT - 1).to_string()] {
            let channel = MirrorChannel {
                guild_id: "2222".to_owned(),
                everyone: Some(MirrorOverwrite {
                    allow: "0".to_owned(),
                    deny: deny.to_owned(),
                }),
            };
            assert_eq!(
                mirror_channel_policy(&channel, "2222"),
                MirrorGate::PublicChannel,
                "deny={deny}"
            );
        }
    }

    #[test]
    fn missing_or_malformed_overwrite_is_public() {
        let no_row = MirrorChannel {
            guild_id: "2222".to_owned(),
            everyone: None,
        };
        assert_eq!(
            mirror_channel_policy(&no_row, "2222"),
            MirrorGate::PublicChannel
        );
        let garbage_deny = MirrorChannel {
            guild_id: "2222".to_owned(),
            everyone: Some(MirrorOverwrite {
                allow: "0".to_owned(),
                deny: "nope".to_owned(),
            }),
        };
        assert_eq!(
            mirror_channel_policy(&garbage_deny, "2222"),
            MirrorGate::PublicChannel
        );
    }

    #[test]
    fn foreign_or_missing_guild_is_wrong_guild() {
        let foreign = MirrorChannel {
            guild_id: "3333".to_owned(),
            everyone: Some(MirrorOverwrite {
                allow: "0".to_owned(),
                deny: VIEW_CHANNEL_BIT.to_string(),
            }),
        };
        assert_eq!(
            mirror_channel_policy(&foreign, "2222"),
            MirrorGate::WrongGuild
        );
        let no_guild = MirrorChannel {
            guild_id: String::new(),
            everyone: Some(MirrorOverwrite {
                allow: "0".to_owned(),
                deny: VIEW_CHANNEL_BIT.to_string(),
            }),
        };
        assert_eq!(
            mirror_channel_policy(&no_guild, "2222"),
            MirrorGate::WrongGuild
        );
    }

    #[test]
    fn next_snowflake_is_numeric_successor() {
        assert_eq!(next_snowflake("41"), Some("42".to_owned()));
        assert_eq!(next_snowflake("0"), Some("1".to_owned()));
        assert_eq!(next_snowflake("18446744073709551615"), None);
        assert_eq!(next_snowflake("not-a-snowflake"), None);
        assert_eq!(next_snowflake(""), None);
    }

    #[test]
    fn snowflake_comparison_is_numeric() {
        assert!(snowflake_at_or_after("100", "100"));
        assert!(snowflake_at_or_after("101", "100"));
        assert!(!snowflake_at_or_after("99", "100"));
        assert!(!snowflake_at_or_after("x", "100"));
        assert!(!snowflake_at_or_after("100", "x"));
    }

    #[test]
    fn find_mirror_requires_marker_author_and_boundary() {
        let marked = format!("audit-event:{ENTRY}; · something happened");
        let page = vec![
            msg("300", BOT, "unrelated line"),
            msg("250", "555", &marked), // right marker, wrong author
            msg("200", BOT, "audit-event:other; · x"), // wrong marker
            msg("150", BOT, &marked),   // matches, below boundary
            msg("120", BOT, &marked),   // matches, below boundary
        ];
        // No boundary: newest bot-authored marked message wins.
        assert_eq!(
            find_mirror_in_page(&page, ENTRY, BOT, None),
            Some("150".to_owned())
        );
        // Boundary excludes the hits; nothing at/above 160 carries the marker.
        assert_eq!(find_mirror_in_page(&page, ENTRY, BOT, Some("160")), None);
        // Boundary admits only the newest marker.
        assert_eq!(
            find_mirror_in_page(&page, ENTRY, BOT, Some("130")),
            Some("150".to_owned())
        );
        // Boundary equal to the match's id still admits it.
        assert_eq!(
            find_mirror_in_page(&page, ENTRY, BOT, Some("150")),
            Some("150".to_owned())
        );
    }

    #[test]
    fn page_floor_is_oldest_id() {
        let page = vec![msg("300", BOT, "a"), msg("120", BOT, "b")];
        assert_eq!(page_floor_id(&page), Some("120".to_owned()));
        assert_eq!(page_floor_id(&[]), None);
    }
}
