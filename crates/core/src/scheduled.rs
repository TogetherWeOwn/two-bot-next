//! Scheduled-message domain: validation, timing, id resolution, ticker math.
//!
//! Slice TOG-10081 of TOG-9809 (S4). Ports the DB-free core of the legacy
//! `two-bot` automations scheduler (`src/automations/service.ts` scheduled
//! section, `src/automations/scheduler.ts`, and the `/schedule`,
//! `/schedule-remove`, `/schedule-list` handler shapes in
//! `src/automations/discord.ts`) as framework-free data plus pure functions in
//! the same style as `leveling.rs`/`moderation.rs`.
//!
//! Source behaviour (legacy `two-bot`, frozen `main`):
//! - `/schedule`: `body` required (1–2000 chars); `in-minutes` 1–525600 and
//!   `every-minutes` 60–525600, exactly one of the two required. One-shot when
//!   only `in-minutes` is given, recurring when `every-minutes` is given.
//! - `/schedule-remove`: `id` prefix-resolved; zero or ambiguous matches are
//!   refused with the same "no unique match" reply.
//! - `/schedule-list`: rows ordered by `next_run_at`.
//! - Ticker: 15 s poll, `next_run_at` is the queue, one occurrence claimed at
//!   a time with a 60 s lease, at most 10 attempts per tick, recurring rows
//!   advance from the run time (no catch-up burst), one-shot rows disable.
//!
//! The interaction router (TOG-10075) and the REST executor (TOG-10076) have
//! not landed yet, so this module exposes outcome enums
//! ([`OccurrenceOutcome`]) and reply builders the wiring slices consume —
//! there is no private dispatcher or HTTP client here. The sqlx store lives in
//! `scheduled_store` (behind the `db` feature); the table in
//! `crates/cutover/migrations/0140_scheduled_messages.sql` (reserved block
//! 0140–0149).
//!
//! Staging gate: these shapes publish only while `TWO_AUTOMATIONS=1`
//! ([`FeatureGates`](crate::feature_commands::FeatureGates)); the boot adapter
//! enforces the guild fence, same posture as the other S4 slices.

/// Slash `in-minutes` bounds (parity §1 #17).
pub const IN_MINUTES_MIN: i64 = 1;
/// Slash `in-minutes` upper bound: one year.
pub const IN_MINUTES_MAX: i64 = 525_600;
/// Slash `every-minutes` lower bound: hourly minimum.
pub const EVERY_MINUTES_MIN: i64 = 60;
/// Slash `every-minutes` upper bound: one year.
pub const EVERY_MINUTES_MAX: i64 = 525_600;
/// Admin-authored message body ceiling (legacy `MAX_BODY`, Discord's ceiling).
pub const MAX_BODY_CHARS: usize = 2000;
/// Store `interval_seconds` bounds (legacy `scheduled_messages_interval`:
/// 60 s to 365 days; `NULL` = one-shot).
pub const INTERVAL_SECONDS_MIN: i64 = 60;
/// Store `interval_seconds` upper bound (365 days).
pub const INTERVAL_SECONDS_MAX: i64 = 31_536_000;
/// Ticker poll cadence (legacy `SCHEDULER_TICK_MS`).
pub const SCHEDULER_TICK_MS: u64 = 15_000;
/// Claim lease: a claimed occurrence is parked this far in the future so a
/// second scheduler (or a restart) cannot reclaim it mid-post (legacy: one
/// minute, ample margin over the 15 s Discord abort).
pub const CLAIM_LEASE_MS: u64 = 60_000;
/// Max occurrences attempted per tick (legacy `runDueScheduled` loop cap).
pub const TICKER_BATCH_LIMIT: i64 = 10;
/// Default retry delay for a retryable failure without a `retry-after` hint
/// (legacy `retryDelayMs` fallback, 30 s).
pub const RETRY_DEFAULT_MS: u64 = 30_000;
/// Retry delay floor (legacy clamp, 1 s).
pub const RETRY_MIN_MS: u64 = 1_000;
/// Retry delay ceiling (legacy clamp, 15 min).
pub const RETRY_MAX_MS: u64 = 15 * 60_000;

/// `/schedule` input: raw option values straight off the interaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleInput<'a> {
    pub body: &'a str,
    pub in_minutes: Option<i64>,
    pub every_minutes: Option<i64>,
}

/// Validated schedule: which delay sets the first run and whether it recurs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedSchedule {
    pub body: String,
    pub delay_minutes: i64,
    pub interval_seconds: Option<i64>,
}

/// `/schedule` validation failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error("Message body must be between 1 and 2000 characters.")]
    BodyEmpty,
    #[error("Message body must be between 1 and 2000 characters.")]
    BodyTooLong,
    #[error("Give either in-minutes (one-shot) or every-minutes (recurring).")]
    MissingTiming,
    #[error("in-minutes must be between 1 and 525600, got {0}.")]
    InMinutesOutOfRange(i64),
    #[error("every-minutes must be between 60 and 525600, got {0}.")]
    EveryMinutesOutOfRange(i64),
}

/// Validate a `/schedule` invocation (legacy `putScheduled` checks plus the
/// handler's one-of rule). Count UTF-16 units and require sendable text after
/// the shared outbound rendering, preserving the original body for storage.
pub fn validate_schedule(input: &ScheduleInput<'_>) -> Result<ValidatedSchedule, ScheduleError> {
    let body = input.body;
    if crate::message_safety::text_len(body) > MAX_BODY_CHARS {
        return Err(ScheduleError::BodyTooLong);
    }
    if !crate::message_safety::has_message_text(&crate::message_safety::content(body)) {
        return Err(ScheduleError::BodyEmpty);
    }
    if let Some(in_minutes) = input.in_minutes {
        if !(IN_MINUTES_MIN..=IN_MINUTES_MAX).contains(&in_minutes) {
            return Err(ScheduleError::InMinutesOutOfRange(in_minutes));
        }
    }
    if let Some(every_minutes) = input.every_minutes {
        if !(EVERY_MINUTES_MIN..=EVERY_MINUTES_MAX).contains(&every_minutes) {
            return Err(ScheduleError::EveryMinutesOutOfRange(every_minutes));
        }
    }
    match (input.in_minutes, input.every_minutes) {
        (Some(in_minutes), _) => Ok(ValidatedSchedule {
            body: body.to_owned(),
            delay_minutes: in_minutes,
            interval_seconds: input.every_minutes.map(|m| m * 60),
        }),
        (None, Some(every_minutes)) => Ok(ValidatedSchedule {
            body: body.to_owned(),
            delay_minutes: every_minutes,
            interval_seconds: Some(every_minutes * 60),
        }),
        (None, None) => Err(ScheduleError::MissingTiming),
    }
}

/// First-run instant for a validated schedule, as epoch millis (legacy: `Date.now()
/// + (inMinutes ?? everyMinutes) * 60_000`).
#[must_use]
pub fn next_run_at_ms(now_ms: u64, delay_minutes: i64) -> u64 {
    now_ms.saturating_add(delay_minutes as u64 * 60_000)
}

/// Advance a recurring row from the run time, so a bot that was down for an
/// hour does not fire a burst of catch-up posts (legacy `markScheduledRun`).
#[must_use]
pub fn advance_next_run_ms(ran_at_ms: u64, interval_seconds: i64) -> u64 {
    ran_at_ms.saturating_add(interval_seconds as u64 * 1_000)
}

/// Park a claimed occurrence this far in the future (legacy `leaseUntil`).
#[must_use]
pub fn lease_until_ms(now_ms: u64) -> u64 {
    now_ms.saturating_add(CLAIM_LEASE_MS)
}

/// Render epoch millis as ISO-8601 UTC with milliseconds (legacy
/// `Date.toISOString()`, e.g. `2026-01-01T00:10:00.000Z`). The store column
/// is `TEXT`, so every writer must use exactly this shape — mixed shapes
/// (with/without millis) would break the lexicographic `next_run_at`
/// ordering the ticker relies on. Pure integer civil-date math (Hinnant),
/// no date dependency.
#[must_use]
pub fn format_iso_ms(ms: u64) -> String {
    let secs = ms / 1_000;
    let millis = (ms % 1_000) as u32;
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
            if leap {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse an ISO-8601 UTC timestamp in this module's writer shape
/// (`YYYY-MM-DDTHH:MM:SS[.mmm]Z` — legacy `Date.toISOString()` always carries
/// millis; bare seconds are tolerated for defence) back to epoch millis.
/// Returns `None` on any shape or range violation; the store treats that as a
/// corrupt row, never as epoch zero.
#[must_use]
pub fn parse_iso_ms(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    let millis: u64 = match b.len() {
        20 => {
            if b[19] != b'Z' {
                return None;
            }
            0
        }
        24 => {
            if b[19] != b'.' || b[23] != b'Z' {
                return None;
            }
            s[20..23].parse().ok()?
        }
        _ => return None,
    };
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let year: i64 = s[0..4].parse().ok()?;
    let month: u32 = s[5..7].parse().ok()?;
    let day: u32 = s[8..10].parse().ok()?;
    let hour: u64 = s[11..13].parse().ok()?;
    let minute: u64 = s[14..16].parse().ok()?;
    let second: u64 = s[17..19].parse().ok()?;
    if !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    if day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    if days < 0 {
        return None;
    }
    let secs = days as u64 * 86_400 + hour * 3_600 + minute * 60 + second;
    Some(secs * 1_000 + millis)
}

/// Advance a recurring row's `next_run_at` from the run instant (legacy
/// `markScheduledRun`: `new Date(Date.parse(ranAt) + interval * 1000)`).
/// `None` when the run instant does not parse — the store surfaces that as a
/// corrupt-row error rather than silently keeping the old time.
#[must_use]
pub fn advance_next_run_iso(ran_at_iso: &str, interval_seconds: i64) -> Option<String> {
    parse_iso_ms(ran_at_iso).map(|ms| format_iso_ms(advance_next_run_ms(ms, interval_seconds)))
}

/// Whether a post failure is worth a bounded retry (legacy
/// `DiscordPostError.retryable`: no response, 429, or 5xx).
#[must_use]
pub fn post_failure_retryable(status: Option<u16>) -> bool {
    match status {
        None => true,
        Some(429) => true,
        Some(s) => s >= 500,
    }
}

/// Clamp a `retry-after` hint into the bounded retry window (legacy
/// `retryDelayMs`: default 30 s, clamped to 1 s–15 min).
#[must_use]
pub fn clamp_retry_delay_ms(retry_after_ms: Option<u64>) -> u64 {
    retry_after_ms
        .unwrap_or(RETRY_DEFAULT_MS)
        .clamp(RETRY_MIN_MS, RETRY_MAX_MS)
}

/// Prefix-resolution verdict for `/schedule-remove` (legacy
/// `resolveScheduledId`: the single row matching `id = ? OR id LIKE prefix%`,
/// `ORDER BY id LIMIT 2` — zero rows and ambiguous prefixes both refuse).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdResolution<'a> {
    Unique(&'a str),
    Missing,
    Ambiguous,
}

/// Resolve an id-or-prefix against one guild's ids, mirroring the SQL rule:
/// exact or prefix match, ordered, first two decide. Note an exact id with a
/// longer sibling sharing the prefix is still ambiguous — same as legacy.
pub fn resolve_scheduled_id<'a, I>(ids: I, prefix: &str) -> IdResolution<'a>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut matches: Vec<&'a str> = ids
        .into_iter()
        .filter(|id| *id == prefix || id.starts_with(prefix))
        .collect();
    matches.sort_unstable();
    match matches.as_slice() {
        [] => IdResolution::Missing,
        [only] => IdResolution::Unique(only),
        _ => IdResolution::Ambiguous,
    }
}

/// `/schedule` confirmation (legacy handler reply).
#[must_use]
pub fn schedule_confirm_text(
    created: bool,
    id: &str,
    every_minutes: Option<i64>,
    next_run_at_iso: &str,
) -> String {
    let verb = if created { "Scheduled" } else { "Replaced" };
    match every_minutes {
        Some(m) => format!("{verb} message `{id}` every {m}m."),
        None => format!("{verb} message `{id}` at {next_run_at_iso}."),
    }
}

/// `/schedule-remove` refusal for zero or ambiguous prefix matches (legacy:
/// both cases share this reply).
#[must_use]
pub fn no_unique_match_text(id_or_prefix: &str) -> String {
    format!(
        "No unique scheduled message matches `{id_or_prefix}`. \
         Use the full id from /schedule-list."
    )
}

/// `/schedule-remove` outcome replies (legacy handler).
#[must_use]
pub fn schedule_cancelled_text() -> &'static str {
    "Cancelled."
}

/// Reply when the resolved id is already gone.
#[must_use]
pub fn no_such_schedule_text(id: &str) -> String {
    format!("No scheduled message `{id}`.")
}

/// One `/schedule-list` row (legacy: `` `id` `<#channel>` `next` `[every Xm]`
/// `[ (disabled)]` ``). Intervals from `/schedule` are always whole minutes
/// (`every-minutes * 60`), which render exactly; hand-written second-level
/// intervals floor rather than round — unreachable from the slash path.
#[must_use]
pub fn schedule_list_line(
    id: &str,
    channel_id: &str,
    next_run_at: &str,
    interval_seconds: Option<i64>,
    enabled: bool,
) -> String {
    let mut line = format!("`{id}` `<#{channel_id}>` {next_run_at}");
    if let Some(seconds) = interval_seconds {
        line.push_str(&format!(" every {}m", seconds.div_euclid(60)));
    }
    if !enabled {
        line.push_str(" (disabled)");
    }
    line
}

/// `/schedule-list` body for a guild's rows (legacy: `Nothing scheduled.`
/// when empty; callers cap the joined text at 2000 chars like every reply).
#[must_use]
pub fn schedule_list_text(lines: &[String]) -> String {
    if lines.is_empty() {
        return "Nothing scheduled.".to_owned();
    }
    lines.join("\n")
}

/// What posting one claimed occurrence produced. The future REST executor
/// (TOG-10076) computes this; the store methods persist it. Audit facts travel
/// with the outcome so the executor writes the same `scheduled.run` rows
/// legacy does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OccurrenceOutcome {
    /// Posted and the run recorded; recurring rows advance, one-shots disable.
    Posted { message_id: Option<String> },
    /// Transient failure (`retryable`): re-queue at `retry_at_ms`, keeping the
    /// occurrence nonce so the retry cannot double-post.
    Retryable { retry_at_ms: u64 },
    /// Permanent failure (deleted channel, 4xx): advance/disable like a run so
    /// a dead row cannot wedge the queue.
    FailedPermanent,
    /// The definition changed or was cancelled while Discord was posting: the
    /// completion no longer owns the row (legacy `stale_completion`).
    StaleCompletion,
    /// Discord accepted the post but recording the run failed: the orphan was
    /// (`cleaned`) deleted, or the delete failed and the nonce is retained so
    /// the retry stays idempotent.
    PostedUnrecorded { cleaned: bool },
}

impl OccurrenceOutcome {
    /// Legacy `scheduled.run` audit outcome for this result.
    #[must_use]
    pub fn audit_outcome(&self) -> &'static str {
        match self {
            Self::Posted { .. } => "ok",
            Self::Retryable { .. } => "retry_scheduled",
            Self::FailedPermanent => "post_failed",
            Self::StaleCompletion => "stale_completion",
            Self::PostedUnrecorded { cleaned: true } => "persistence_failed_cleaned",
            Self::PostedUnrecorded { cleaned: false } => "persistence_failed_retry_idempotent",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(body: &str, in_m: Option<i64>, every_m: Option<i64>) -> ScheduleInput<'_> {
        ScheduleInput {
            body,
            in_minutes: in_m,
            every_minutes: every_m,
        }
    }

    #[test]
    fn one_shot_validates_and_sets_no_interval() {
        let v = validate_schedule(&input("hello", Some(10), None)).expect("valid");
        assert_eq!(
            v,
            ValidatedSchedule {
                body: "hello".to_owned(),
                delay_minutes: 10,
                interval_seconds: None,
            }
        );
    }

    #[test]
    fn recurring_uses_every_for_delay_and_interval() {
        let v = validate_schedule(&input("hi", None, Some(60))).expect("valid");
        assert_eq!(v.delay_minutes, 60);
        assert_eq!(v.interval_seconds, Some(3600));
    }

    #[test]
    fn in_minutes_wins_the_first_run_when_both_given() {
        // Legacy `(inMinutes ?? everyMinutes)`: one-shot timing, recurring tail.
        let v = validate_schedule(&input("hi", Some(5), Some(120))).expect("valid");
        assert_eq!(v.delay_minutes, 5);
        assert_eq!(v.interval_seconds, Some(7200));
    }

    #[test]
    fn bounds_match_slash_and_store_limits() {
        assert_eq!(
            validate_schedule(&input("x", None, None)),
            Err(ScheduleError::MissingTiming)
        );
        assert_eq!(
            validate_schedule(&input("x", Some(0), None)),
            Err(ScheduleError::InMinutesOutOfRange(0))
        );
        assert_eq!(
            validate_schedule(&input("x", Some(525_601), None)),
            Err(ScheduleError::InMinutesOutOfRange(525_601))
        );
        assert_eq!(
            validate_schedule(&input("x", None, Some(59))),
            Err(ScheduleError::EveryMinutesOutOfRange(59))
        );
        assert_eq!(
            validate_schedule(&input("x", None, Some(525_601))),
            Err(ScheduleError::EveryMinutesOutOfRange(525_601))
        );
        assert!(validate_schedule(&input("x", Some(1), None)).is_ok());
        assert!(validate_schedule(&input("x", Some(525_600), None)).is_ok());
        assert!(validate_schedule(&input("x", None, Some(60))).is_ok());
        assert!(validate_schedule(&input("x", None, Some(525_600))).is_ok());
        // Store interval ceiling: a year in minutes is 31.5M s, inside it.
        let v = validate_schedule(&input("x", None, Some(525_600))).expect("valid");
        assert!(v
            .interval_seconds
            .is_some_and(|s| s <= INTERVAL_SECONDS_MAX));
    }

    #[test]
    fn body_must_be_1_to_2000_chars() {
        assert_eq!(
            validate_schedule(&input("", Some(1), None)),
            Err(ScheduleError::BodyEmpty)
        );
        assert_eq!(
            validate_schedule(&input(&"x".repeat(2001), Some(1), None)),
            Err(ScheduleError::BodyTooLong)
        );
        assert!(validate_schedule(&input(&"x".repeat(2000), Some(1), None)).is_ok());
    }

    #[test]
    fn body_bounds_count_utf16_units_without_truncating_stored_text() {
        for body in [
            "é".repeat(2000),
            "😀".repeat(1000),
            format!("{}😀", "x".repeat(1998)),
        ] {
            let validated = validate_schedule(&input(&body, Some(1), None)).expect("at limit");
            assert_eq!(validated.body, body);
            assert_eq!(
                crate::message_safety::text_len(&validated.body),
                MAX_BODY_CHARS
            );
            assert_eq!(crate::message_safety::content(&validated.body), body);
        }
        for body in ["😀".repeat(1001), format!("{}😀", "x".repeat(1999))] {
            assert_eq!(
                validate_schedule(&input(&body, Some(1), None)),
                Err(ScheduleError::BodyTooLong)
            );
        }
    }

    #[test]
    fn body_requires_effective_message_text_and_preserves_meaningful_joiners() {
        for body in [
            " ",
            "\t\r\n",
            "\u{a0}",
            "\u{200b}",
            "\u{200c}",
            "\u{200d}",
            "\u{feff}",
            " \u{200b}\u{200c}\u{200d}\u{feff}\n",
        ] {
            assert_eq!(
                validate_schedule(&input(body, Some(1), None)),
                Err(ScheduleError::BodyEmpty),
                "{body:?}"
            );
        }
        for body in [
            "  hello \n".to_owned(),
            "👩\u{200d}💻".to_owned(),
            format!("{}x", "\u{200b}".repeat(1999)),
        ] {
            assert_eq!(
                validate_schedule(&input(&body, Some(1), None))
                    .expect("sendable")
                    .body,
                body
            );
        }
    }

    #[test]
    fn timing_math_matches_legacy() {
        assert_eq!(next_run_at_ms(1_000_000, 10), 1_000_000 + 600_000);
        assert_eq!(advance_next_run_ms(1_000_000, 3600), 1_000_000 + 3_600_000);
        assert_eq!(lease_until_ms(1_000_000), 1_000_000 + 60_000);
    }

    #[test]
    fn iso_format_matches_to_iso_string_shape() {
        // Epoch values hand-verified: 2026-01-01T00:00:00Z = 1,767,225,600 s
        // (56 y * 365 + 14 leap days), 2024-02-29 = 1,709,164,800 s.
        assert_eq!(format_iso_ms(1_767_226_200_000), "2026-01-01T00:10:00.000Z");
        assert_eq!(format_iso_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_iso_ms(1), "1970-01-01T00:00:00.001Z");
        // Leap day renders correctly.
        assert_eq!(format_iso_ms(1_709_164_800_000), "2024-02-29T00:00:00.000Z");
        let all = [
            format_iso_ms(1_767_226_200_000),
            format_iso_ms(1_767_226_200_001),
            format_iso_ms(1_767_226_260_000),
        ];
        let mut sorted = all.clone();
        sorted.sort();
        assert_eq!(
            all, sorted,
            "fixed-width shape must order lexicographically"
        );
    }

    #[test]
    fn iso_round_trips_through_writer_shape() {
        for ms in [
            0,
            1,
            999,
            1_000,
            1_709_164_800_000, // 2024-02-29T00:00:00.000Z
            1_767_226_200_000, // 2026-01-01T00:10:00.000Z
            1_767_226_200_001,
            4_102_444_800_000, // 2100-01-01T00:00:00.000Z (non-leap century)
        ] {
            let iso = format_iso_ms(ms);
            assert_eq!(parse_iso_ms(&iso), Some(ms), "{iso}");
        }
        // Bare seconds tolerated; offsets and junk refused.
        assert_eq!(
            parse_iso_ms("2026-01-01T00:10:00Z"),
            parse_iso_ms("2026-01-01T00:10:00.000Z")
        );
        for bad in [
            "",
            "yesterday",
            "2026-01-01T00:10:00",       // no zone
            "2026-01-01T00:10:00+00:00", // offset form legacy never writes
            "2026-13-01T00:00:00.000Z",  // month 13
            "2026-02-30T00:00:00.000Z",  // Feb 30
            "2025-02-29T00:00:00.000Z",  // non-leap Feb 29
            "2026-01-01T24:00:00.000Z",  // hour 24
            "2026-01-01T00:10:00.00Z",   // short millis
            "1969-12-31T23:59:59.999Z",  // pre-epoch: never a valid run time
        ] {
            assert_eq!(parse_iso_ms(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn advance_iso_matches_legacy_date_arithmetic() {
        assert_eq!(
            advance_next_run_iso("2026-01-01T00:10:00.000Z", 3600),
            Some("2026-01-01T01:10:00.000Z".to_owned())
        );
        // Month boundary rolls over.
        assert_eq!(
            advance_next_run_iso("2026-01-31T23:00:00.000Z", 3600),
            Some("2026-02-01T00:00:00.000Z".to_owned())
        );
        assert_eq!(advance_next_run_iso("not-a-time", 3600), None);
    }

    #[test]
    fn retry_matrix_matches_legacy() {
        assert!(post_failure_retryable(None));
        assert!(post_failure_retryable(Some(429)));
        assert!(post_failure_retryable(Some(500)));
        assert!(post_failure_retryable(Some(503)));
        assert!(!post_failure_retryable(Some(400)));
        assert!(!post_failure_retryable(Some(403)));
        assert!(!post_failure_retryable(Some(404)));
        assert_eq!(clamp_retry_delay_ms(None), 30_000);
        assert_eq!(clamp_retry_delay_ms(Some(500)), 1_000);
        assert_eq!(clamp_retry_delay_ms(Some(45_000)), 45_000);
        assert_eq!(clamp_retry_delay_ms(Some(3_600_000)), 900_000);
    }

    #[test]
    fn prefix_resolution_refuses_missing_and_ambiguous() {
        let ids = ["abc123", "abc1234", "abc456", "def789"];
        assert_eq!(
            resolve_scheduled_id(ids.iter().copied(), "abc123"),
            IdResolution::Ambiguous,
            "exact id with a longer sibling sharing the prefix still refuses"
        );
        assert_eq!(
            resolve_scheduled_id(ids.iter().copied(), "abc45"),
            IdResolution::Unique("abc456")
        );
        assert_eq!(
            resolve_scheduled_id(ids.iter().copied(), "abc"),
            IdResolution::Ambiguous
        );
        assert_eq!(
            resolve_scheduled_id(ids.iter().copied(), "zzz"),
            IdResolution::Missing
        );
        assert_eq!(
            resolve_scheduled_id(ids.iter().copied(), "def789"),
            IdResolution::Unique("def789")
        );
    }

    #[test]
    fn replies_match_legacy_shapes() {
        assert_eq!(
            schedule_confirm_text(true, "id1", None, "2026-01-01T00:10:00.000Z"),
            "Scheduled message `id1` at 2026-01-01T00:10:00.000Z."
        );
        assert_eq!(
            schedule_confirm_text(false, "id1", Some(60), "ignored"),
            "Replaced message `id1` every 60m."
        );
        assert!(no_unique_match_text("abc").contains("Use the full id from /schedule-list."));
        assert_eq!(schedule_cancelled_text(), "Cancelled.");
        assert_eq!(no_such_schedule_text("id1"), "No scheduled message `id1`.");
        assert_eq!(schedule_list_text(&[]), "Nothing scheduled.");
        assert_eq!(
            schedule_list_line("id1", "chan9", "2026-01-01T00:10:00.000Z", Some(3600), true),
            "`id1` `<#chan9>` 2026-01-01T00:10:00.000Z every 60m"
        );
        assert_eq!(
            schedule_list_line("id2", "chan9", "2026-01-01T00:10:00.000Z", None, false),
            "`id2` `<#chan9>` 2026-01-01T00:10:00.000Z (disabled)"
        );
    }

    #[test]
    fn outcome_audit_names_match_legacy() {
        assert_eq!(
            OccurrenceOutcome::Posted { message_id: None }.audit_outcome(),
            "ok"
        );
        assert_eq!(
            OccurrenceOutcome::Retryable { retry_at_ms: 1 }.audit_outcome(),
            "retry_scheduled"
        );
        assert_eq!(
            OccurrenceOutcome::FailedPermanent.audit_outcome(),
            "post_failed"
        );
        assert_eq!(
            OccurrenceOutcome::StaleCompletion.audit_outcome(),
            "stale_completion"
        );
        assert_eq!(
            OccurrenceOutcome::PostedUnrecorded { cleaned: true }.audit_outcome(),
            "persistence_failed_cleaned"
        );
        assert_eq!(
            OccurrenceOutcome::PostedUnrecorded { cleaned: false }.audit_outcome(),
            "persistence_failed_retry_idempotent"
        );
    }
}
