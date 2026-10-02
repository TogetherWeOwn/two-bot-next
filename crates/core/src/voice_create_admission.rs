//! Pure room-create admission core: caps, cooldown and rolling burst limits.
//!
//! Frozen legacy source: `TogetherWeOwn/two-bot` revision
//! `bffccf3e3a9f56a3da37de67c6f272ac10ecb3b3`, `src/tempVoice/store.ts:27-32`
//! (burst constants and refusal codes) and `src/tempVoice/store.ts:77-126`
//! (the atomic `reserveIfUnderCaps` claim order), `src/tempVoice/service.ts`
//! (the user-facing refusal messages built from the claim reason) and
//! `src/tempVoice/config.ts:143-147` (the `maxPerUser` 1..10, `maxPerGuild`
//! 1..45 and `createCooldownSeconds` 0..3600 bounds).
//!
//! The caller supplies the clock (`now_secs`) and the history: live-plus-
//! reserved room counts, the member's last accepted create time, and the
//! accepted `create_reservation` audit entries. This module performs no I/O,
//! holds no Discord, store, database or clock types, and knows nothing about
//! room lifecycle, naming, permissions or ownership. Serializing concurrent
//! claims per guild, persisting the reservation and the audit row, and
//! translating refusals into replies belong to the runtime.
//!
//! Admission rules, evaluated in this fixed order (the first trip wins, as in
//! legacy):
//!
//! 1. **User cap**: `owned_by_user >= max_per_user` refuses with
//!    [`RefusalReason::UserCap`]. Reservations count: a create already in
//!    flight holds its owner's slot.
//! 2. **Guild cap**: `rooms_in_guild >= max_per_guild` refuses with
//!    [`RefusalReason::GuildCap`].
//! 3. **Cooldown**: when `cooldown_secs > 0` and the member has a previous
//!    create, `now_secs - last_created_at_secs < cooldown_secs` refuses with
//!    [`RefusalReason::Cooldown`]. Equality is allowed: waiting exactly the
//!    cooldown passes. A cooldown of zero disables this check entirely, and a
//!    member with no previous create is never throttled by it.
//! 4. **User burst**: accepted reservations by this member with
//!    `created_at_secs` strictly inside the trailing [`CREATE_BURST_WINDOW_SECS`]
//!    window, at or over [`CREATE_BURST_PER_USER`], refuse with
//!    [`RefusalReason::UserBurst`].
//! 5. **Guild burst**: accepted reservations by anyone in the guild inside the
//!    same window, at or over [`CREATE_BURST_PER_GUILD`], refuse with
//!    [`RefusalReason::GuildBurst`].
//!
//! The burst window is strict: a reservation stamped exactly
//! `now_secs - CREATE_BURST_WINDOW_SECS` has already left the window, matching
//! legacy's `created_at > now - window` scan. Burst history counts accepted
//! reservations, not live rooms: deleting a room, rolling a reservation back,
//! or restarting frees no burst slot, and the current attempt is never part of
//! the history it is checked against (legacy counts before inserting the new
//! audit row). The caller must therefore pass only previously accepted
//! reservations, excluding the attempt under decision.
//!
//! Timestamps are `i64` Unix seconds. Window and cooldown subtraction saturate
//! rather than overflow; a `last_created_at_secs` in the future yields a
//! negative elapsed time, which is below any positive cooldown and refuses,
//! exactly as legacy's `nowMs - lastAt < cooldown * 1000` does.

use crate::Snowflake;

/// Strict trailing window for burst counting, in seconds.
/// Legacy: `CREATE_BURST_WINDOW_SECONDS = 60`.
pub const CREATE_BURST_WINDOW_SECS: i64 = 60;

/// Accepted reservations per member inside the window.
/// Legacy: `CREATE_BURST_PER_USER = 3`.
pub const CREATE_BURST_PER_USER: u32 = 3;

/// Accepted reservations per guild inside the window.
/// Legacy: `CREATE_BURST_PER_GUILD = 10`.
pub const CREATE_BURST_PER_GUILD: u32 = 10;

/// Smallest `max_per_user` a config accepts. Legacy: `TWO_TEMP_VOICE_MAX_PER_USER` min 1.
pub const MIN_ROOMS_PER_USER: u32 = 1;

/// Largest `max_per_user` a config accepts. Legacy: `TWO_TEMP_VOICE_MAX_PER_USER` max 10.
pub const MAX_ROOMS_PER_USER: u32 = 10;

/// Smallest `max_per_guild` a config accepts. Legacy: `TWO_TEMP_VOICE_MAX_PER_GUILD` min 1.
pub const MIN_ROOMS_PER_GUILD: u32 = 1;

/// Largest `max_per_guild` a config accepts (legacy max 45), staying under
/// Discord's 50-channels-per-category ceiling with room for the generator
/// itself.
pub const MAX_ROOMS_PER_GUILD: u32 = 45;

/// Smallest `cooldown_secs` a config accepts: zero disables the cooldown check.
/// Legacy: `TWO_TEMP_VOICE_CREATE_COOLDOWN_SECONDS` min 0.
pub const MIN_CREATE_COOLDOWN_SECS: u32 = 0;

/// Largest `cooldown_secs` a config accepts.
/// Legacy: `TWO_TEMP_VOICE_CREATE_COOLDOWN_SECONDS` max 3600.
pub const MAX_CREATE_COOLDOWN_SECS: u32 = 3600;

/// Legacy config defaults: `maxPerUser` 1, `maxPerGuild` 40, `createCooldownSeconds` 30.
pub const DEFAULT_MAX_PER_USER: u32 = 1;
/// Legacy config defaults: `maxPerUser` 1, `maxPerGuild` 40, `createCooldownSeconds` 30.
pub const DEFAULT_MAX_PER_GUILD: u32 = 40;
/// Legacy config defaults: `maxPerUser` 1, `maxPerGuild` 40, `createCooldownSeconds` 30.
pub const DEFAULT_CREATE_COOLDOWN_SECS: u32 = 30;

/// Why a room create was refused, before any Discord call is made. The
/// [`RefusalReason::code`] strings are the stable legacy `TempVoiceRefusal`
/// codes, also used as the durable audit reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefusalReason {
    UserCap,
    GuildCap,
    Cooldown,
    UserBurst,
    GuildBurst,
}

impl RefusalReason {
    /// The stable reason code: `user_cap`, `guild_cap`, `cooldown`,
    /// `user_burst` or `guild_burst`.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::UserCap => "user_cap",
            Self::GuildCap => "guild_cap",
            Self::Cooldown => "cooldown",
            Self::UserBurst => "user_burst",
            Self::GuildBurst => "guild_burst",
        }
    }

    /// The legacy user-facing message for this refusal, rendered against the
    /// active config. Only the user-cap and cooldown messages vary; the rest
    /// are fixed text quoting the burst constants.
    #[must_use]
    pub fn user_message(self, config: &CreateAdmissionConfig) -> String {
        match self {
            Self::UserCap => {
                let max = config.max_per_user();
                if max == 1 {
                    "You already have a temporary voice channel.".to_owned()
                } else {
                    format!("You already have {max} temporary voice channels.")
                }
            }
            Self::GuildCap => {
                "This server has reached its temporary voice channel limit. Try again shortly."
                    .to_owned()
            }
            Self::Cooldown => format!(
                "Please wait {} seconds between creating channels.",
                config.cooldown_secs()
            ),
            Self::UserBurst => format!(
                "Temporary voice rate limit: {CREATE_BURST_PER_USER} creates per \
                 {CREATE_BURST_WINDOW_SECS} seconds. Please try again shortly."
            ),
            Self::GuildBurst => format!(
                "This server's temporary voice rate limit is {CREATE_BURST_PER_GUILD} creates per \
                 {CREATE_BURST_WINDOW_SECS} seconds. Please try again shortly."
            ),
        }
    }
}

/// Typed config-bound refusals. The bounds mirror legacy's `integer(env, key,
/// fallback, min, max)` ranges; out-of-range values are refused here instead
/// of at env-parse time so any caller benefits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionConfigError {
    #[error("max rooms per user must be between 1 and 10")]
    MaxPerUser,
    #[error("max rooms per guild must be between 1 and 45")]
    MaxPerGuild,
    #[error("create cooldown must be between 0 and 3600 seconds")]
    Cooldown,
}

/// One validated admission snapshot. Construct with [`CreateAdmissionConfig::new`];
/// [`CreateAdmissionConfig::default`] carries the legacy defaults (1, 40, 30).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateAdmissionConfig {
    max_per_user: u32,
    max_per_guild: u32,
    cooldown_secs: u32,
}

impl CreateAdmissionConfig {
    /// Validate one config. Each bound is refused independently so a caller
    /// learns exactly which knob is out of range.
    pub fn new(
        max_per_user: u32,
        max_per_guild: u32,
        cooldown_secs: u32,
    ) -> Result<Self, AdmissionConfigError> {
        if !(MIN_ROOMS_PER_USER..=MAX_ROOMS_PER_USER).contains(&max_per_user) {
            return Err(AdmissionConfigError::MaxPerUser);
        }
        if !(MIN_ROOMS_PER_GUILD..=MAX_ROOMS_PER_GUILD).contains(&max_per_guild) {
            return Err(AdmissionConfigError::MaxPerGuild);
        }
        if !(MIN_CREATE_COOLDOWN_SECS..=MAX_CREATE_COOLDOWN_SECS).contains(&cooldown_secs) {
            return Err(AdmissionConfigError::Cooldown);
        }
        Ok(Self {
            max_per_user,
            max_per_guild,
            cooldown_secs,
        })
    }

    #[must_use]
    pub fn max_per_user(self) -> u32 {
        self.max_per_user
    }

    #[must_use]
    pub fn max_per_guild(self) -> u32 {
        self.max_per_guild
    }

    #[must_use]
    pub fn cooldown_secs(self) -> u32 {
        self.cooldown_secs
    }
}

impl Default for CreateAdmissionConfig {
    fn default() -> Self {
        Self {
            max_per_user: DEFAULT_MAX_PER_USER,
            max_per_guild: DEFAULT_MAX_PER_GUILD,
            cooldown_secs: DEFAULT_CREATE_COOLDOWN_SECS,
        }
    }
}

/// One previously accepted `create_reservation` audit entry: who reserved and
/// when, in Unix seconds. Entries survive room deletion, rollback and restart
/// by design — that is what makes the burst limit durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcceptedReservation {
    pub user_id: Snowflake,
    pub created_at_secs: i64,
}

/// Everything the core needs for one decision. All history is caller-supplied:
/// the core never queries a store or a clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionRequest<'a> {
    /// The requesting member. Zero is refused with
    /// [`AdmissionError::InvalidUserId`].
    pub user_id: Snowflake,
    /// Live plus reserved rooms this member owns in the guild. Legacy counts
    /// `temp_voice_channels` rows, so in-flight reservations hold a slot.
    pub owned_by_user: u32,
    /// Live plus reserved rooms in the guild.
    pub rooms_in_guild: u32,
    /// The member's last accepted create time (`temp_voice_creates` row), or
    /// `None` when they never created. Disables the cooldown check for them.
    pub last_created_at_secs: Option<i64>,
    /// Previously accepted reservations for burst counting. Must exclude the
    /// attempt under decision; may include reservations whose rooms were since
    /// deleted, which still consume burst slots.
    pub accepted_reservations: &'a [AcceptedReservation],
}

/// The verdict for one create attempt: allowed, or refused with the first
/// tripped reason in the fixed evaluation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionDecision {
    Allow,
    Deny { reason: RefusalReason },
}

impl AdmissionDecision {
    #[must_use]
    pub fn allowed(self) -> bool {
        matches!(self, Self::Allow)
    }

    #[must_use]
    pub fn refusal(self) -> Option<RefusalReason> {
        match self {
            Self::Allow => None,
            Self::Deny { reason } => Some(reason),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionError {
    #[error("requesting user id must be nonzero")]
    InvalidUserId,
}

/// Evaluate exactly one create attempt against caller-supplied state at
/// `now_secs` (Unix seconds). The checks run in the fixed refusal order
/// documented at the top of this module; the first trip wins. Evaluating the
/// same inputs twice decides the same way, and a refusal changes nothing.
pub fn decide_admission(
    config: &CreateAdmissionConfig,
    request: &AdmissionRequest<'_>,
    now_secs: i64,
) -> Result<AdmissionDecision, AdmissionError> {
    if request.user_id == 0 {
        return Err(AdmissionError::InvalidUserId);
    }
    if request.owned_by_user >= config.max_per_user {
        return Ok(AdmissionDecision::Deny {
            reason: RefusalReason::UserCap,
        });
    }
    if request.rooms_in_guild >= config.max_per_guild {
        return Ok(AdmissionDecision::Deny {
            reason: RefusalReason::GuildCap,
        });
    }
    let cooling_down = config.cooldown_secs > 0
        && request
            .last_created_at_secs
            .is_some_and(|last| now_secs.saturating_sub(last) < i64::from(config.cooldown_secs));
    if cooling_down {
        return Ok(AdmissionDecision::Deny {
            reason: RefusalReason::Cooldown,
        });
    }
    let cutoff = now_secs.saturating_sub(CREATE_BURST_WINDOW_SECS);
    let mut mine = 0u32;
    let mut total = 0u32;
    for reservation in request.accepted_reservations {
        if reservation.created_at_secs > cutoff {
            total = total.saturating_add(1);
            if reservation.user_id == request.user_id {
                mine = mine.saturating_add(1);
            }
        }
    }
    if mine >= CREATE_BURST_PER_USER {
        return Ok(AdmissionDecision::Deny {
            reason: RefusalReason::UserBurst,
        });
    }
    if total >= CREATE_BURST_PER_GUILD {
        return Ok(AdmissionDecision::Deny {
            reason: RefusalReason::GuildBurst,
        });
    }
    Ok(AdmissionDecision::Allow)
}
