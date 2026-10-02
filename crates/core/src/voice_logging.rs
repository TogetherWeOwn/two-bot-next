//! Pure V10 logging-channel resolution core, written from `docs/voice-rooms.md`
//! §V10 (Logging part) only.
//!
//! The caller supplies plain channel/user IDs and reachability facts; this
//! module performs no I/O, holds no Discord, store or clock types, and never
//! touches room lifecycle, placement, permissions or health evaluation. The
//! runtime owns persistence, sending, wording and permission checks.
//!
//! Decisions:
//! - [`DetailLevel`] (`off`/`brief`/`full`): unknown text is refused
//!   fail-closed by [`parse_detail_level`].
//! - [`resolve_log_target`]: first working destination wins — guild system
//!   channel, then DM, then creator channel chat.
//! - [`RepeatLedger`]: bounded pure-counter repeats, then stop. The caller
//!   owns the clock and persistence; this counts sends only.

use crate::Snowflake;

/// Longest level text echoed back in [`LoggingError::UnknownLevel`].
pub const MAX_LEVEL_ECHO_CHARS: usize = 32;

/// Total sends per tracked failure before notices stop: one initial notice
/// plus two repeats.
pub const MAX_LOG_SENDS: u32 = 3;

/// `/logging` detail level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailLevel {
    /// Logging disabled. The runtime sends nothing.
    Off,
    /// Short notices only.
    Brief,
    /// Full notices.
    Full,
}

/// Typed `/logging` refusals. Unknown level text never falls back to a
/// default: it fails closed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoggingError {
    #[error("Unknown detail level \"{value}\"; expected off, brief or full.")]
    UnknownLevel { value: String },
}

/// Parse a `/logging` detail level. Trimmed ASCII case-insensitive
/// `off`/`brief`/`full`; anything else (including empty) is refused
/// fail-closed.
pub fn parse_detail_level(raw: &str) -> Result<DetailLevel, LoggingError> {
    if raw.trim().eq_ignore_ascii_case("off") {
        return Ok(DetailLevel::Off);
    }
    if raw.trim().eq_ignore_ascii_case("brief") {
        return Ok(DetailLevel::Brief);
    }
    if raw.trim().eq_ignore_ascii_case("full") {
        return Ok(DetailLevel::Full);
    }
    let echo: String = raw.trim().chars().take(MAX_LEVEL_ECHO_CHARS).collect();
    Err(LoggingError::UnknownLevel { value: echo })
}

impl DetailLevel {
    /// True unless the level is [`DetailLevel::Off`].
    #[must_use]
    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// Whether an event is logged under `setting`. `is_detailed` marks events
/// that only `full` wants; brief events log under both `brief` and `full`,
/// and nothing logs under `off`.
#[must_use]
pub fn should_log(setting: DetailLevel, is_detailed: bool) -> bool {
    match setting {
        DetailLevel::Off => false,
        DetailLevel::Brief => !is_detailed,
        DetailLevel::Full => true,
    }
}

fn some_id(id: Option<Snowflake>) -> Option<Snowflake> {
    id.filter(|id| *id != 0)
}

/// Availability snapshot for the V10 log fallback chain. `None` (or zero)
/// IDs and `false` reachability both mean "try the next destination".
/// `setup_user_id` is the caller-supplied ID of whoever last set up the bot;
/// it is only ever used as the system-channel mention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoggingCandidates {
    pub system_channel_id: Option<Snowflake>,
    pub dm_user_id: Option<Snowflake>,
    pub dm_reachable: bool,
    pub creator_channel_id: Option<Snowflake>,
    /// Whoever last set up the bot, for the system-channel mention.
    pub setup_user_id: Option<Snowflake>,
}

/// First working V10 log destination. IDs are numeric only; no names or
/// message text travel here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogTarget {
    SystemChannel {
        channel_id: Snowflake,
        /// Caller-supplied setup user for the mention; the adapter drops the
        /// mention when `None` but still posts to the channel.
        mention_user_id: Option<Snowflake>,
    },
    DirectMessage {
        user_id: Snowflake,
    },
    CreatorChat {
        channel_id: Snowflake,
    },
}

/// V10 fallback order: guild system channel (mentioning whoever last set up
/// the bot), then a DM to the caller-supplied user, then the creator
/// channel's chat. Returns `None` only when no destination is available.
/// Zero IDs count as absent.
#[must_use]
pub fn resolve_log_target(candidates: LoggingCandidates) -> Option<LogTarget> {
    if let Some(channel_id) = some_id(candidates.system_channel_id) {
        return Some(LogTarget::SystemChannel {
            channel_id,
            mention_user_id: some_id(candidates.setup_user_id),
        });
    }
    if candidates.dm_reachable {
        if let Some(user_id) = some_id(candidates.dm_user_id) {
            return Some(LogTarget::DirectMessage { user_id });
        }
    }
    some_id(candidates.creator_channel_id).map(|channel_id| LogTarget::CreatorChat { channel_id })
}

/// Bounded repeat state as pure counters: "a few times, then stop". The
/// caller owns the clock, persistence and per-failure mapping (one ledger per
/// tracked failure); this counts sends only, so the same inputs always give
/// the same answer and refusals change nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RepeatLedger {
    sends: u32,
}

impl RepeatLedger {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sends recorded so far.
    #[must_use]
    pub fn sends(self) -> u32 {
        self.sends
    }

    /// True while fewer than [`MAX_LOG_SENDS`] sends are recorded.
    #[must_use]
    pub fn should_send(self) -> bool {
        self.sends < MAX_LOG_SENDS
    }

    /// Record one sent notice. Returns false without counting when the bound
    /// is already reached; the failure stays listed but silent.
    pub fn record_send(&mut self) -> bool {
        if self.sends >= MAX_LOG_SENDS {
            return false;
        }
        self.sends = self.sends.saturating_add(1);
        true
    }

    /// Restart the budget, e.g. after the failure resolved and recurred.
    pub fn reset(&mut self) {
        self.sends = 0;
    }
}
