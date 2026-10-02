//! Pure V12a template-assistant monthly-cap ledger, derived from
//! `docs/voice-rooms.md` §V12 only.
//!
//! §V12 allows 200 assistant builds per guild per calendar month (UTC),
//! resetting on the 1st at 00:00 UTC, and sends only the admin's request, the
//! guild's templates, the "no game" label and the locale — never member
//! names, presence or IDs.
//!
//! This module is the pure core ahead of the V12 wiring (parent card
//! TOG-10117): it performs no I/O, holds no HTTP/endpoint, store, database or
//! V1 lifecycle types, and knows no Discord wire types. All persistence (the
//! per-guild month-start and build count) and endpoint gating belong to the
//! runtime.
//!
//! Ledger rules, in order:
//!
//! 1. The month key is the UTC year-month of `now_secs`; the reset boundary
//!    is the 1st at 00:00 UTC. Timestamps are `i64` Unix seconds, matching
//!    [`crate::voice_naming::RoomContext::timestamp`].
//! 2. [`CapLedger::record`] takes the caller-supplied persisted row
//!    (`month_start_secs`, `builds_used`) plus `now_secs`. A stored row from
//!    an older month resets to zero for the current month; a clock that moved
//!    before the stored month start is refused ([`CapError::ClockRollback`])
//!    rather than granting a fresh quota.
//! 3. At or over [`MONTHLY_BUILD_LIMIT`] builds the decision is
//!    [`CapDecision::Deny`] with the limit and the next reset time; below it
//!    the build is counted and the decision is [`CapDecision::Allow`] with
//!    the builds remaining after this one.
//! 4. [`validate_request_shape`] is a shape-level allowlist: the request type
//!    carries the prompt, the guild templates, the "no game" label and the
//!    locale, and nothing else — no member fields exist in the type, so member
//!    data cannot be sent by construction.
//!
//! Leap safety comes from civil date math (Howard Hinnant's algorithms, as in
//! the V5/V11 cores): month lengths, including February 29th, never appear as
//! a table, so February caps and March resets are exact with no special case.

use std::collections::{BTreeMap, BTreeSet};

use crate::Snowflake;

/// Assistant builds allowed per guild per UTC calendar month (§V12).
pub const MONTHLY_BUILD_LIMIT: u32 = 200;

/// Longest admin plain-language request, in characters. Generous enough for a
/// description in any language, bounded so the endpoint payload stays small.
pub const MAX_PROMPT_CHARS: usize = 2000;

/// Most guild templates sent with one request. Covers fifty channels per
/// category across several categories; anything larger is a caller bug.
pub const MAX_TEMPLATES: usize = 128;

/// Longest single name or status template, in characters. The V5 engine
/// re-validates the exact byte bound at render; this is the coarse shape
/// guard.
pub const MAX_TEMPLATE_CHARS: usize = 4096;

/// Longest "no game" label, in characters (matches the V11 literal-name
/// bound).
pub const MAX_NO_GAME_LABEL_CHARS: usize = 100;

/// Longest locale tag, in characters. Covers BCP-47 grandfathered tags with
/// extensions.
pub const MAX_LOCALE_CHARS: usize = 35;

/// One UTC calendar month: the ledger key. Ordering is chronological for the
/// valid keys `from_unix_secs` produces (months `1..=12`); anything else is
/// refused by [`MonthKey::start_unix_secs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonthKey {
    pub year: i32,
    pub month: u32,
}

impl MonthKey {
    /// The UTC year-month containing `now_secs`.
    pub fn from_unix_secs(now_secs: i64) -> Result<Self, CapError> {
        let days = now_secs.div_euclid(86_400);
        let (year, month, _) = civil_from_days(days).ok_or(CapError::TimestampOutOfRange)?;
        Ok(Self { year, month })
    }

    /// The 1st of this month at 00:00 UTC, as Unix seconds. A month outside
    /// `1..=12` is refused: the civil math below would otherwise silently wrap
    /// it (month 13 reads as January), and `from_unix_secs` never produces
    /// such a key, so this is caller misuse, not clock data.
    pub fn start_unix_secs(self) -> Result<i64, CapError> {
        if !(1..=12).contains(&self.month) {
            return Err(CapError::InvalidMonthStart);
        }
        days_from_civil(self.year, self.month, 1)
            .checked_mul(86_400)
            .ok_or(CapError::TimestampOutOfRange)
    }

    /// The next reset boundary: the 1st of the following month at 00:00 UTC.
    /// This is the `resets_at` carried by [`CapDecision::Deny`].
    pub fn next_start_unix_secs(self) -> Result<i64, CapError> {
        let (year, month) = if self.month == 12 {
            (
                self.year
                    .checked_add(1)
                    .ok_or(CapError::TimestampOutOfRange)?,
                1,
            )
        } else {
            (self.year, self.month + 1)
        };
        MonthKey { year, month }.start_unix_secs()
    }
}

/// The verdict for one build attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapDecision {
    /// The build is counted. `remaining` is the builds left after this one:
    /// zero on the month's last allowed build.
    Allow { remaining: u32 },
    /// The guild already used `limit` builds this month. `resets_at` is the
    /// next 1st 00:00 UTC, when the next attempt is allowed again.
    Deny { limit: u32, resets_at: i64 },
}

impl CapDecision {
    #[must_use]
    pub fn allowed(self) -> bool {
        matches!(self, Self::Allow { .. })
    }

    #[must_use]
    pub fn remaining(self) -> Option<u32> {
        match self {
            Self::Allow { remaining } => Some(remaining),
            Self::Deny { .. } => None,
        }
    }

    #[must_use]
    pub fn resets_at(self) -> Option<i64> {
        match self {
            Self::Allow { .. } => None,
            Self::Deny { resets_at, .. } => Some(resets_at),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CapError {
    #[error("guild id must be nonzero")]
    InvalidGuildId,
    #[error("month start must be the 1st of a UTC month at 00:00:00")]
    InvalidMonthStart,
    #[error("timestamp is out of range")]
    TimestampOutOfRange,
    #[error("clock moved before the stored month start")]
    ClockRollback,
}

/// One guild template sent with an assistant request: the channel's name and
/// optional status template. Guild configuration only — no member data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssistantTemplate<'a> {
    pub channel_id: Snowflake,
    pub name_template: &'a str,
    pub status_template: Option<&'a str>,
}

/// Exactly what §V12 sends to the endpoint: the admin's plain-language
/// request, the guild's templates, the "no game" label and the locale. There
/// are no member-name, presence or ID fields on this type, so the privacy
/// rule holds by construction; [`validate_request_shape`] additionally bounds
/// every field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssistantRequest<'a> {
    pub prompt: &'a str,
    pub guild_templates: &'a [AssistantTemplate<'a>],
    pub no_game_label: &'a str,
    pub locale: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RequestShapeError {
    #[error("prompt must not be blank")]
    EmptyPrompt,
    #[error("prompt must be at most 2000 characters")]
    PromptTooLong,
    #[error("at most 128 guild templates may be sent")]
    TooManyTemplates,
    #[error("template channel id must be nonzero")]
    InvalidChannelId,
    #[error("template channel id appears more than once")]
    DuplicateTemplateChannel,
    #[error("template name must not be blank")]
    EmptyTemplate,
    #[error("template must be at most 4096 characters")]
    TemplateTooLong,
    #[error("status template must be at most 4096 characters")]
    StatusTemplateTooLong,
    #[error("\"no game\" label must not be blank")]
    EmptyNoGameLabel,
    #[error("\"no game\" label must be at most 100 characters")]
    NoGameLabelTooLong,
    #[error("locale must not be blank")]
    EmptyLocale,
    #[error("locale must be 2..=35 ASCII letters, digits or hyphens starting with a letter")]
    InvalidLocale,
}

/// Shape-level allowlist for the endpoint payload: every field present and
/// bounded, nothing else representable. Guild routing and cap keying happen
/// server-side from the authenticated guild, never from this payload.
pub fn validate_request_shape(request: &AssistantRequest<'_>) -> Result<(), RequestShapeError> {
    if request.prompt.trim().is_empty() {
        return Err(RequestShapeError::EmptyPrompt);
    }
    if request.prompt.chars().count() > MAX_PROMPT_CHARS {
        return Err(RequestShapeError::PromptTooLong);
    }
    if request.guild_templates.len() > MAX_TEMPLATES {
        return Err(RequestShapeError::TooManyTemplates);
    }
    let mut seen = BTreeSet::new();
    for template in request.guild_templates {
        if template.channel_id == 0 {
            return Err(RequestShapeError::InvalidChannelId);
        }
        if !seen.insert(template.channel_id) {
            return Err(RequestShapeError::DuplicateTemplateChannel);
        }
        if template.name_template.trim().is_empty() {
            return Err(RequestShapeError::EmptyTemplate);
        }
        if template.name_template.chars().count() > MAX_TEMPLATE_CHARS {
            return Err(RequestShapeError::TemplateTooLong);
        }
        if template
            .status_template
            .is_some_and(|status| status.chars().count() > MAX_TEMPLATE_CHARS)
        {
            return Err(RequestShapeError::StatusTemplateTooLong);
        }
    }
    if request.no_game_label.trim().is_empty() {
        return Err(RequestShapeError::EmptyNoGameLabel);
    }
    if request.no_game_label.chars().count() > MAX_NO_GAME_LABEL_CHARS {
        return Err(RequestShapeError::NoGameLabelTooLong);
    }
    if request.locale.trim().is_empty() {
        return Err(RequestShapeError::EmptyLocale);
    }
    if !valid_locale(request.locale) {
        return Err(RequestShapeError::InvalidLocale);
    }
    Ok(())
}

fn valid_locale(locale: &str) -> bool {
    let len = locale.chars().count();
    if !(2..=MAX_LOCALE_CHARS).contains(&len) {
        return false;
    }
    if !matches!(locale.chars().next(), Some(c) if c.is_ascii_alphabetic()) {
        return false;
    }
    locale
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !locale.ends_with('-')
        && !locale.contains("--")
}

/// One persisted row mirrored in memory: the month the count belongs to and
/// the builds used in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LedgerEntry {
    month: MonthKey,
    builds_used: u32,
}

/// Pure in-memory monthly-cap ledger. The caller owns the persisted
/// `(month_start, builds_used)` row per guild (DB column on the V12 wiring);
/// [`CapLedger::record`] reconciles that row against `now_secs`, decides, and
/// mirrors the outcome in memory, counting an allowed build. Dropping the
/// ledger loses only the mirror — the persisted row stays authoritative, and
/// restart recovery belongs to the parent.
#[derive(Debug, Default)]
pub struct CapLedger {
    entries: BTreeMap<Snowflake, LedgerEntry>,
}

impl CapLedger {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide one build attempt for `guild_id` at `now_secs`.
    ///
    /// `month_start_secs` and `builds_used` are the caller-supplied persisted
    /// row: the start validates as a 1st 00:00 UTC. A row from an older month
    /// resets to zero for the current month, so the first build after the 1st
    /// is always allowed. `guild_id` scopes the mirror entry; it does not
    /// affect the decision, because per-guild isolation is the caller's
    /// persisted-row keying.
    pub fn record(
        &mut self,
        guild_id: Snowflake,
        month_start_secs: i64,
        builds_used: u32,
        now_secs: i64,
    ) -> Result<CapDecision, CapError> {
        if guild_id == 0 {
            return Err(CapError::InvalidGuildId);
        }
        let stored = MonthKey::from_unix_secs(month_start_secs)?;
        if stored.start_unix_secs()? != month_start_secs {
            return Err(CapError::InvalidMonthStart);
        }
        if now_secs < month_start_secs {
            return Err(CapError::ClockRollback);
        }
        let current = MonthKey::from_unix_secs(now_secs)?;
        let effective = if current == stored { builds_used } else { 0 };
        let decision = if effective >= MONTHLY_BUILD_LIMIT {
            CapDecision::Deny {
                limit: MONTHLY_BUILD_LIMIT,
                resets_at: current.next_start_unix_secs()?,
            }
        } else {
            CapDecision::Allow {
                remaining: MONTHLY_BUILD_LIMIT - effective - 1,
            }
        };
        let entry = match decision {
            CapDecision::Allow { .. } => LedgerEntry {
                month: current,
                builds_used: effective + 1,
            },
            CapDecision::Deny { .. } => LedgerEntry {
                month: stored,
                builds_used,
            },
        };
        self.entries.insert(guild_id, entry);
        Ok(decision)
    }

    /// The mirrored row for `guild_id`, if this ledger has decided for it.
    #[must_use]
    pub fn usage(&self, guild_id: Snowflake) -> Option<(MonthKey, u32)> {
        self.entries
            .get(&guild_id)
            .map(|entry| (entry.month, entry.builds_used))
    }
}

/// Days since 1970-01-01 (Howard Hinnant's `days_from_civil`, as in the
/// V5/V11 cores).
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = if month <= 2 {
        i64::from(year) - 1
    } else {
        i64::from(year)
    };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (i64::from(month) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`]: `(year, month, day)` for a day count.
/// `None` outside ±200k years, matching the funnel core's sanity bound.
fn civil_from_days(days: i64) -> Option<(i32, u32, u32)> {
    if days.checked_abs()? > 73_000_000 {
        return None;
    }
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = i32::try_from(if m <= 2 { y + 1 } else { y }).ok()?;
    Some((year, m, d))
}
