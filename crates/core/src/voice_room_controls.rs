//! Pure V3 owner room-control decisions derived from `docs/voice-rooms.md` §V3 only.
//!
//! The caller supplies authoritative room facts (the `/limit` argument with a
//! current headcount, per-member bitrate preferences with the guild tier
//! maximum, a name candidate with the current voice-channel names). This module
//! performs no I/O and manages no membership, persistence, permissions, room
//! deletion, interaction delivery or template expansion. It has no Discord or
//! database types and no dependency on V1 lifecycle state.
//!
//! Discord API facts used here (from the spec's API notes): user limits are
//! `0..=99` with `0` meaning unlimited, and bitrates are capped by the guild's
//! boost tier. A headcount of `0` passed as the lock snapshot means the room is
//! empty, which maps to unlimited (there is nobody to lock in).

/// Highest settable room user limit. Discord uses `0` for unlimited, so the
/// settable range is `1..=MAX_ROOM_LIMIT`.
pub const MAX_ROOM_LIMIT: u32 = 99;

/// Lowest valid bitrate preference in bits per second. Preferences must be
/// strictly above 8 kbps, so `8001` is the smallest accepted value.
pub const MIN_BITRATE_BPS: u32 = 8_001;

/// Resetting a per-user bitrate preference clears it (no preference). The room
/// average then ignores that member, falling back to the creator default when
/// nobody has a preference.
pub const RESET_BITRATE_PREFERENCE: Option<u32> = None;

/// Decision returned by [`parse_limit`]. `Unlimited` is encoded as `0` on the
/// Discord API; `Limited(n)` carries a value in `1..=MAX_ROOM_LIMIT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomLimit {
    Unlimited,
    Limited(u32),
}

impl RoomLimit {
    /// Unlimited room (Discord user limit `0`).
    #[must_use]
    pub fn unlimited() -> Self {
        Self::Unlimited
    }

    /// True when no user limit applies.
    #[must_use]
    pub fn is_unlimited(self) -> bool {
        matches!(self, Self::Unlimited)
    }

    /// Discord user-limit value: `0` for unlimited, otherwise the limit.
    #[must_use]
    pub fn user_limit(self) -> u32 {
        match self {
            Self::Unlimited => 0,
            Self::Limited(n) => n,
        }
    }
}

/// Clears the room limit (the `/unlimit` decision).
#[must_use]
pub fn unlimit() -> RoomLimit {
    RoomLimit::Unlimited
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LimitError {
    /// Argument was above [`MAX_ROOM_LIMIT`]. Carries the rejected value.
    #[error("room limit must be 0..=99")]
    OutOfRange(u32),
}

/// Decide the `/limit` outcome from caller-supplied facts only.
///
/// - `Some(0)` means unlimited.
/// - `Some(1..=99)` sets that limit.
/// - `Some(>99)` is [`LimitError::OutOfRange`].
/// - `None` locks the room at the current headcount: a headcount of `0` is
///   unlimited (documented empty-room case); `1..=99` locks at that count;
///   a headcount above `99` clamps to [`MAX_ROOM_LIMIT`] so a lock never
///   produces a limit above 99.
pub fn parse_limit(arg: Option<u32>, headcount: u32) -> Result<RoomLimit, LimitError> {
    match arg {
        Some(0) => Ok(RoomLimit::Unlimited),
        Some(n @ 1..=MAX_ROOM_LIMIT) => Ok(RoomLimit::Limited(n)),
        Some(n) => Err(LimitError::OutOfRange(n)),
        None => {
            if headcount == 0 {
                Ok(RoomLimit::Unlimited)
            } else if headcount <= MAX_ROOM_LIMIT {
                Ok(RoomLimit::Limited(headcount))
            } else {
                Ok(RoomLimit::Limited(MAX_ROOM_LIMIT))
            }
        }
    }
}

/// Guild boost tier, expressed as a table input to bitrate validation. Values
/// are Discord's documented voice bitrate maxima in bits per second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitrateTier {
    Base,
    Level1,
    Level2,
    Level3,
}

/// Guild tier maximum in bits per second: Base 64 kbps, Level 1 128 kbps,
/// Level 2 256 kbps, Level 3 384 kbps.
#[must_use]
pub fn tier_max_bps(tier: BitrateTier) -> u32 {
    match tier {
        BitrateTier::Base => 64_000,
        BitrateTier::Level1 => 128_000,
        BitrateTier::Level2 => 256_000,
        BitrateTier::Level3 => 384_000,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BitrateError {
    /// Value is at or below 8 kbps. Carries the rejected value.
    #[error("bitrate preference must be above 8000 bps")]
    TooLow(u32),
    /// Value exceeds the guild tier maximum.
    #[error("bitrate preference exceeds the guild tier maximum")]
    ExceedsTierMax { value: u32, tier_max: u32 },
}

/// Reset a per-user bitrate preference (clears it to no preference).
#[must_use]
pub fn reset_bitrate_preference() -> Option<u32> {
    None
}

/// Validate one per-user bitrate preference against the guild tier maximum.
/// The value must be strictly above 8 kbps (`> 8000`) and at most `tier_max`.
/// `None` (the reset value) is handled by the caller storing no preference;
/// this function validates a concrete value only.
pub fn validate_bitrate_preference(value: u32, tier_max: u32) -> Result<u32, BitrateError> {
    if value <= 8_000 {
        return Err(BitrateError::TooLow(value));
    }
    if value > tier_max {
        return Err(BitrateError::ExceedsTierMax { value, tier_max });
    }
    Ok(value)
}

/// Clamp a bitrate into the valid range for the guild tier. Callers must
/// supply `tier_max >= MIN_BITRATE_BPS`; a degenerate smaller maximum is
/// clamped defensively without panicking.
#[must_use]
pub fn clamp_bitrate(value: u32, tier_max: u32) -> u32 {
    let lower = MIN_BITRATE_BPS.min(tier_max);
    let upper = MIN_BITRATE_BPS.max(tier_max);
    value.clamp(lower, upper)
}

/// Decide the room bitrate as the average of the members' set preferences
/// (`Some` entries of `prefs`; `None` means that member has no preference).
/// With no preferences present, falls back to the creator channel's bitrate.
///
/// Rounding: the average uses integer division and rounds down (floor); e.g.
/// preferences `[8001, 8002]` average to `8001`. The sum accumulates in `u64`
/// so any number of `u32` preferences cannot overflow.
///
/// The result (including the fallback) is always clamped into the valid
/// range via [`clamp_bitrate`]. Out-of-range stored preferences are tolerated
/// here through that final clamp; range enforcement at set time belongs to
/// [`validate_bitrate_preference`].
#[must_use]
pub fn room_bitrate(prefs: &[Option<u32>], creator_default: u32, tier_max: u32) -> u32 {
    let mut sum: u64 = 0;
    let mut count: u64 = 0;
    for pref in prefs.iter().flatten() {
        sum += u64::from(*pref);
        count += 1;
    }
    let raw = if count == 0 {
        creator_default
    } else {
        // Floor division; `sum` is the exact total of `u32` inputs, so the
        // quotient always fits in `u32`.
        (sum / count) as u32
    };
    clamp_bitrate(raw, tier_max)
}

/// Decide whether a literal name candidate conflicts under the guild's
/// "unique names" setting. Literal names only: no template expansion,
/// normalization or trimming is applied here.
///
/// Comparison is case-sensitive byte equality (`"Room" != "room"`).
/// Returns `false` whenever `unique_names_enabled` is off, even on an exact
/// match. The caller supplies the current voice-channel names to compare
/// against (whether the candidate's own channel is included is the parent's
/// routing decision; typically it is excluded for renames).
pub fn name_conflicts<S: AsRef<str>>(
    candidate: &str,
    existing_voice_names: &[S],
    unique_names_enabled: bool,
) -> bool {
    if !unique_names_enabled {
        return false;
    }
    existing_voice_names
        .iter()
        .any(|name| name.as_ref() == candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_boundaries_are_exact() {
        assert_eq!(parse_limit(Some(0), 5), Ok(RoomLimit::Unlimited));
        assert_eq!(parse_limit(Some(1), 5), Ok(RoomLimit::Limited(1)));
        assert_eq!(parse_limit(Some(99), 5), Ok(RoomLimit::Limited(99)));
        assert_eq!(parse_limit(Some(100), 5), Err(LimitError::OutOfRange(100)));
    }

    #[test]
    fn lock_uses_headcount_with_documented_empty_case() {
        assert_eq!(parse_limit(None, 0), Ok(RoomLimit::Unlimited));
        assert_eq!(parse_limit(None, 1), Ok(RoomLimit::Limited(1)));
        assert_eq!(parse_limit(None, 42), Ok(RoomLimit::Limited(42)));
        assert_eq!(parse_limit(None, 99), Ok(RoomLimit::Limited(99)));
    }

    #[test]
    fn unlimit_clears_to_unlimited() {
        assert_eq!(unlimit(), RoomLimit::Unlimited);
        assert!(unlimit().is_unlimited());
        assert_eq!(unlimit().user_limit(), 0);
        assert_eq!(RoomLimit::Limited(4).user_limit(), 4);
    }
}
