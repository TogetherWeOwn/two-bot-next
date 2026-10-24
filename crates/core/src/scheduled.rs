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
/// `/schedule-inspect` view selector ceiling: `metadata` (8 units) is the
/// longest accepted value, so the registry bound doubles as the parser bound.
pub const MAX_INSPECT_VIEW_CHARS: usize = 8;
/// `/schedule-edit` revision token ceiling: the inspect-issued optimistic
/// concurrency token is an opaque ASCII string that must fit a bounded
/// option. Tokens are content-derived (see `scheduled_store`), never stored.
pub const MAX_SCHEDULE_REVISION_CHARS: usize = 128;
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

/// Neutralize mass mentions and invite links in a schedule body so the
/// ticker can never post a ping or a clickable invite pill.
///
/// Mass mentions (`@everyone`/`@here`, including zero-width-split
/// obfuscation) are broken with the shared mention neutralizer; every
/// case-insensitive invite host (`discord.gg`, `discord.com/invite`,
/// `discordapp.com/invite` — the same three hosts as the automod invite
/// policy) is split with a zero-width space after its dot
/// (`discord.\u{200b}gg`) so Discord never linkifies it. Plain text
/// without any of these patterns passes through byte-identical, and the
/// transform is idempotent: already-neutralized text is returned unchanged.
///
/// Applied at the validate boundary (stored bodies leave neutralized) and
/// again at the ticker post boundary (rows written before any validate-side
/// screen are defused on the way to the channel; the ticker additionally
/// truncates to the message ceiling so neutralization growth on legacy rows
/// cannot turn a post into a permanent failure).
#[must_use]
pub fn neutralize_scheduled_body(body: &str) -> String {
    split_invite_host(&crate::message_safety::neutralize_mentions(body))
}

/// Split the dot of every case-insensitive invite host with a zero-width
/// space so Discord does not linkify it. Covers the same three hosts as the
/// automod invite policy: `discord.gg`, `discord.com/invite`, and
/// `discordapp.com/invite`. Original casing is preserved (`DISCORD.GG`
/// becomes `DISCORD.\u{200b}GG`); text without an invite host passes through
/// byte-identical. Longest host wins at each position so
/// `discordapp.com/invite` is never partially matched.
fn split_invite_host(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((pos, needle_len, dot_len)) = find_invite_host(rest) {
        let (head, tail) = rest.split_at(pos);
        out.push_str(head);
        out.push_str(&tail[..dot_len]);
        out.push('\u{200b}');
        out.push_str(&tail[dot_len..needle_len]);
        rest = &tail[needle_len..];
    }
    out.push_str(rest);
    out
}

/// Byte offset, needle length and dot-prefix length of the earliest
/// case-insensitive invite host in `text`, if any. All needles are pure
/// ASCII, so every match starts and ends on a character boundary.
fn find_invite_host(text: &str) -> Option<(usize, usize, usize)> {
    const HOSTS: [(&[u8], usize, usize); 3] = [
        (b"discordapp.com/invite", 21, 11), // `discordapp.` prefix length
        (b"discord.com/invite", 18, 8),     // `discord.` prefix length
        (b"discord.gg", 10, 8),             // `discord.` prefix length
    ];
    let bytes = text.as_bytes();
    bytes.iter().enumerate().find_map(|(pos, _)| {
        // Mid-char byte positions can never start an all-ASCII match;
        // skip them so `split_at` below only sees char boundaries.
        if !text.is_char_boundary(pos) {
            return None;
        }
        HOSTS.iter().find_map(|(needle, len, dot)| {
            if bytes.len() - pos < *len {
                return None;
            }
            let matches = needle.iter().enumerate().all(|(i, b)| {
                if *b == b'.' || *b == b'/' {
                    bytes[pos + i] == *b
                } else {
                    bytes[pos + i].eq_ignore_ascii_case(b)
                }
            });
            matches.then_some((pos, *len, *dot))
        })
    })
}

/// Validate a `/schedule` invocation (legacy `putScheduled` checks plus the
/// handler's one-of rule). Count UTF-16 units and require sendable text after
/// the shared outbound rendering, preserving the original body for storage.
/// Raw body shape: UTF-16 ceiling plus sendable text after the shared
/// outbound rendering. Shared by creation and body-only correction so the
/// two can never drift apart.
fn check_body_shape(body: &str) -> Result<(), ScheduleError> {
    if crate::message_safety::text_len(body) > MAX_BODY_CHARS {
        return Err(ScheduleError::BodyTooLong);
    }
    if !crate::message_safety::has_message_text(&crate::message_safety::content(body)) {
        return Err(ScheduleError::BodyEmpty);
    }
    Ok(())
}

/// Defuse mass mentions and invite pills before storage; benign bodies pass
/// through byte-identical. A neutralization that grows the text past the
/// ceiling refuses instead of handing storage a body the wire would reject,
/// and a neutralization with no sendable text left refuses too.
fn neutralize_and_recheck(body: &str) -> Result<String, ScheduleError> {
    let neutralized = neutralize_scheduled_body(body);
    if crate::message_safety::text_len(&neutralized) > MAX_BODY_CHARS {
        return Err(ScheduleError::BodyTooLong);
    }
    if !crate::message_safety::has_message_text(&neutralized) {
        return Err(ScheduleError::BodyEmpty);
    }
    Ok(neutralized)
}

/// Validate a replacement schedule body through the exact creation policy:
/// raw shape, then neutralization, then the ceiling and text checks again
/// on the stored shape. Returns the neutralized body. `/schedule-edit`
/// shares this with creation so a correction can never store what creation
/// would refuse.
pub fn validate_schedule_body(body: &str) -> Result<String, ScheduleError> {
    check_body_shape(body)?;
    neutralize_and_recheck(body)
}

pub fn validate_schedule(input: &ScheduleInput<'_>) -> Result<ValidatedSchedule, ScheduleError> {
    let body = input.body;
    check_body_shape(body)?;
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
    let neutralized = neutralize_and_recheck(body)?;
    match (input.in_minutes, input.every_minutes) {
        (Some(in_minutes), _) => Ok(ValidatedSchedule {
            body: neutralized,
            delay_minutes: in_minutes,
            interval_seconds: input.every_minutes.map(|m| m * 60),
        }),
        (None, Some(every_minutes)) => Ok(ValidatedSchedule {
            body: neutralized,
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
    if prefix.is_empty()
        || prefix
            .encode_utf16()
            .take(crate::commands::MAX_RESOURCE_ID_CHARS + 1)
            .count()
            > crate::commands::MAX_RESOURCE_ID_CHARS
    {
        return IdResolution::Missing;
    }
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

/// `/schedule-inspect` / `/schedule-edit` refusal for an empty, overlong,
/// unknown, foreign-guild or ambiguous id. Unlike
/// [`no_unique_match_text`] this never echoes the attempted value: inspect
/// and edit refusals must stay mention-safe and body-free even for hostile
/// input, and the fixed text still names the canonical-id source.
#[must_use]
pub fn schedule_unknown_id_text() -> &'static str {
    "No unique scheduled message matches that id. Use the full id from /schedule-list."
}

/// `/schedule-inspect` private views. `Metadata` reports identity, timing
/// and delivery state plus the edit revision; `Body` renders the whole
/// saved definition verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectView {
    Metadata,
    Body,
}

/// Parse `/schedule-inspect`'s optional `view` selector. Missing selects
/// metadata; anything but the two exact lowercase values refuses with the
/// corrected invocation (echoes nothing).
pub fn parse_inspect_view(view: Option<&str>) -> Result<InspectView, &'static str> {
    match view {
        None | Some("metadata") => Ok(InspectView::Metadata),
        Some("body") => Ok(InspectView::Body),
        Some(_) => {
            Err("Unknown view. Use /schedule-inspect id:<schedule-id> view:<metadata|body>.")
        }
    }
}

/// Truthful one-line delivery state for `/schedule-inspect` metadata. A
/// live claim wins over uncertainty, uncertainty over a bare pending
/// nonce, and any fence over the enabled flag — but a disabled row always
/// says so, so a fenced disabled row names both facts.
#[must_use]
pub fn schedule_delivery_state_word(
    claimed: bool,
    nonce_pending: bool,
    uncertain: bool,
    enabled: bool,
) -> String {
    let word = if claimed {
        "claim active"
    } else if uncertain {
        "delivery uncertain"
    } else if nonce_pending {
        "nonce pending"
    } else if enabled {
        "idle"
    } else {
        "disabled"
    };
    if enabled || word == "disabled" {
        word.to_owned()
    } else {
        format!("{word} (disabled)")
    }
}

/// One-shot/recurrence fragment for `/schedule-inspect` metadata. Renders
/// the raw stored seconds so hand-written sub-minute intervals stay
/// truthful (unlike the list line, which floors to whole minutes).
#[must_use]
pub fn schedule_inspect_cadence_text(interval_seconds: Option<i64>) -> String {
    match interval_seconds {
        None => "one-shot".to_owned(),
        Some(seconds) => format!("every {seconds}s"),
    }
}

/// Historical last-run evidence for `/schedule-inspect` metadata: the run
/// instant plus the posted message id when both are known, never a claim
/// about a future delivery.
#[must_use]
pub fn schedule_inspect_last_run_text(
    last_run_at: Option<&str>,
    last_message_id: Option<&str>,
) -> String {
    match (last_run_at, last_message_id) {
        (Some(ran), Some(message)) => format!("{ran} (message {message})"),
        (Some(ran), None) => ran.to_owned(),
        (None, _) => "never".to_owned(),
    }
}

/// `/schedule-inspect` metadata view: canonical id, saved destination, due
/// instant, cadence, truthful delivery state, last-run evidence and the
/// opaque edit revision. Never the body — the separate body view renders
/// that verbatim, so a maximum-length definition loses no tail to headers.
#[must_use]
pub fn schedule_inspect_metadata_text(
    id: &str,
    channel_id: &str,
    next_run_at: &str,
    cadence: &str,
    state: &str,
    last_run: &str,
    revision: &str,
) -> String {
    format!(
        "`{id}` in `<#{channel_id}>` — due {next_run_at} ({cadence})\n\
         State: {state}\n\
         Last run: {last_run}\n\
         Revision: {revision}"
    )
}

/// `/schedule-edit` success: the persisted state only — a fresh revision
/// for chained corrections, never a promise of future delivery.
#[must_use]
pub fn schedule_edit_saved_text(id: &str, revision: &str) -> String {
    format!("Saved the corrected body for `{id}`.\nRevision: {revision}")
}

/// `/schedule-edit` idempotent no-op: the stored body already matches, so
/// no row, timestamp, revision or audit churn happened.
#[must_use]
pub fn schedule_edit_unchanged_text() -> &'static str {
    "Body unchanged: the stored text already matches."
}

/// `/schedule-edit` refusal for a missing, empty or overlong revision
/// option: without a token the edit cannot name the inspected state, so
/// it refuses before any store read. Carries no id — the caller has not
/// resolved one yet.
#[must_use]
pub fn schedule_edit_revision_required_text() -> &'static str {
    "That edit is missing its Revision token. Run /schedule-inspect for the schedule, then retry /schedule-edit with the fresh Revision."
}

/// `/schedule-edit` refusal for a missing, malformed, forged or stale
/// revision: the definition or its delivery evidence moved since the
/// inspect, so the edit cannot prove what it would overwrite.
#[must_use]
pub fn schedule_edit_stale_text(id: &str) -> String {
    format!(
        "That schedule changed since you inspected it. Run /schedule-inspect \
         id:`{id}` for a fresh Revision, then retry /schedule-edit."
    )
}

/// `/schedule-edit` refusal while a delivery fence is armed. Names the
/// fence truthfully and points back at inspect: this slice corrects idle
/// bodies only and never clears claims, nonces or uncertainty.
#[must_use]
pub fn schedule_edit_fenced_text(id: &str, fence: &str) -> String {
    format!(
        "Cannot correct `{id}` while {fence}. Run /schedule-inspect id:`{id}` \
         to review its delivery state, then retry /schedule-edit with a fresh Revision."
    )
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

/// Discord's message ceiling in UTF-16 units: every `/schedule-list` reply
/// must fit, holding only whole rows so no configured id is ever split or
/// silently dropped.
pub const SCHEDULE_LIST_MESSAGE_LIMIT: usize = 2000;

/// `/schedule-list` page indicator (`Page <current> of <total>`): every page
/// of a multi-page list carries it so staff can walk the pages and enumerate
/// every configured id exactly once.
#[must_use]
pub fn schedule_list_page_indicator(current: i64, total: i64) -> String {
    format!("Page {current} of {total}")
}

/// `/schedule-list` refusal for `page < 1` (Discord's registry minimum is 1,
/// but the runtime still refuses a bypassed value with this exact text).
#[must_use]
pub fn schedule_list_page_underflow_text() -> &'static str {
    "Page must be at least 1."
}

/// `/schedule-list` refusal for `page > total`. Lists no rows and never
/// overflows on extreme page values.
#[must_use]
pub fn schedule_list_page_out_of_range_text(page: i64, total: i64) -> String {
    format!("Page {page} is out of range: this server has {total} page(s) of schedules.")
}

/// `/schedule-list` overflow line for a guild holding more rows than the
/// capped read returns: the final line names how many earliest rows were
/// cut (`remaining = total - cap`, always positive at the call site).
/// Byte-pinned by acceptance — keep the exact `(truncated, N more)` shape.
#[must_use]
pub fn schedule_list_overflow_line(remaining: i64) -> String {
    format!("(truncated, {remaining} more)")
}

/// `/schedule-list` fallback marker for a guild whose capped read came back
/// full while the overflow count failed: the reply still names the cut, so a
/// failed second query never renders as a silent cut. Static shape (no
/// count); the counted form stays [`schedule_list_overflow_line`].
#[must_use]
pub fn schedule_list_truncated_text() -> &'static str {
    "(truncated)"
}

/// Overflow line for a capped guild read, given the overflow count outcome:
/// `None` means the count itself failed (fail-safe `(truncated)` marker);
/// `Some(total)` names the cut when `total` exceeds `cap` and yields nothing
/// otherwise. The caller only consults this when the capped read came back
/// full, so under-cap guilds never reach it. Extracted so the failed-count
/// arm is unit-pinned: deleting the push in the caller breaks the
/// over-cap acceptance (which expects the counted line), and remapping
/// `None` here breaks `overflow_for_count_names_a_failed_count`.
#[must_use]
pub fn schedule_list_overflow_for_count(total: Option<i64>, cap: i64) -> Option<String> {
    match total {
        None => Some(schedule_list_truncated_text().to_owned()),
        Some(total) if total > cap => Some(schedule_list_overflow_line(total - cap)),
        Some(_) => None,
    }
}

/// Split whole-row lines into bounded pages, ordered as given. Each page
/// holds as many whole rows as fit in [`SCHEDULE_LIST_MESSAGE_LIMIT`]
/// UTF-16 units; no id handed to this helper is ever split or dropped
/// (the guild read upstream is capped — see `schedule_list_truncated_text`).
/// An empty guild yields one
/// page (`Nothing scheduled.`). A single page carries no indicator (the
/// legacy shape); every page of a multi-page list ends with
/// [`schedule_list_page_indicator`].
#[must_use]
pub fn schedule_list_pages(lines: &[String]) -> Vec<String> {
    if lines.is_empty() {
        return vec!["Nothing scheduled.".to_owned()];
    }
    // Converge on the page count: the indicator length depends on the total,
    // and the total depends on the indicator length. At most 25 rows, so this
    // settles in one or two passes.
    let mut total_guess: usize = 1;
    loop {
        let overhead = if total_guess <= 1 {
            0
        } else {
            let widest = format!(
                "\n{}",
                schedule_list_page_indicator(total_guess as i64, total_guess as i64)
            );
            crate::message_safety::text_len(&widest)
        };
        let rows_budget = SCHEDULE_LIST_MESSAGE_LIMIT.saturating_sub(overhead);
        let packed = pack_schedule_rows(lines, rows_budget);
        if packed.len() == total_guess {
            if packed.len() == 1 {
                return vec![packed[0]
                    .iter()
                    .map(|s| (*s).as_str())
                    .collect::<Vec<_>>()
                    .join("\n")];
            }
            let total = packed.len() as i64;
            return packed
                .into_iter()
                .enumerate()
                .map(|(index, rows)| {
                    let current = index as i64 + 1;
                    format!(
                        "{}\n{}",
                        rows.iter()
                            .map(|s| (*s).as_str())
                            .collect::<Vec<_>>()
                            .join("\n"),
                        schedule_list_page_indicator(current, total)
                    )
                })
                .collect();
        }
        total_guess = packed.len().max(1);
    }
}

/// `/schedule-list` continuation hint for the last page of the first capped
/// window when hidden rows follow: names the next (global) page explicitly,
/// so the local `Page F of F` footer never reads as the end of the list. A
/// separate content line after the rows, leaving the byte-pinned
/// [`schedule_list_overflow_line`] untouched; the page indicator stays the
/// final line, as on every multi-page reply.
#[must_use]
pub fn schedule_list_continuation_hint(next_page: i64) -> String {
    format!("Continued on page {next_page}.")
}

/// Pack the first capped window when hidden rows may follow (`has_hidden`).
/// Behaves exactly like [`schedule_list_pages`] for single-page windows —
/// no indicator, no hint — because the `(truncated, …)` overflow line
/// already signals continuation there (that shape is acceptance-pinned).
/// A multi-page window instead names the next page on its last page (see
/// [`schedule_list_continuation_hint`]): the local `Page F of F` footer would
/// otherwise claim the list ends while hidden rows sit on page `F + 1`. The
/// hint is the final content line and participates in packing — repacked
/// until the page count settles — so the reply still fits
/// [`SCHEDULE_LIST_MESSAGE_LIMIT`]. Appending one line only ever touches the
/// last page, so the count is non-decreasing and bounded by one page per
/// line: the loop always converges.
#[must_use]
pub fn schedule_list_first_pages(lines: &[String], has_hidden: bool) -> Vec<String> {
    let plain = schedule_list_pages(lines);
    if !has_hidden || plain.len() <= 1 {
        return plain;
    }
    let mut total = plain.len() as i64;
    loop {
        let mut hinted = lines.to_vec();
        hinted.push(schedule_list_continuation_hint(total + 1));
        let repacked = schedule_list_pages(&hinted);
        if repacked.len() as i64 == total {
            return repacked;
        }
        total = repacked.len() as i64;
    }
}

/// Pack hidden-window rows with global page numbers continuing from
/// `page_offset` (the first window's page count): page `k` of this window
/// reads `Page <offset + k> of <offset + n>`. A zero offset behaves exactly
/// like [`schedule_list_pages`]. A single page carries no indicator (the
/// legacy shape, acceptance-pinned for the short-id hidden windows); every
/// page of a multi-page hidden window ends with
/// [`schedule_list_page_indicator`]. Packing reserves the widest global
/// footer up front, so renumbered pages still fit
/// [`SCHEDULE_LIST_MESSAGE_LIMIT`].
#[must_use]
pub fn schedule_list_pages_global(lines: &[String], page_offset: i64) -> Vec<String> {
    if page_offset <= 0 {
        return schedule_list_pages(lines);
    }
    if lines.is_empty() {
        return vec!["Nothing scheduled.".to_owned()];
    }
    // Converge like `schedule_list_pages`, but the footer width is driven by
    // the largest global number (`offset + total`), not the local one. At
    // most 25 rows plus an overflow line, so this settles in one or two
    // passes like the plain pack.
    let mut total_guess: usize = 1;
    loop {
        let overhead = if total_guess <= 1 {
            0
        } else {
            let widest = format!(
                "\n{}",
                schedule_list_page_indicator(
                    page_offset + total_guess as i64,
                    page_offset + total_guess as i64
                )
            );
            crate::message_safety::text_len(&widest)
        };
        let rows_budget = SCHEDULE_LIST_MESSAGE_LIMIT.saturating_sub(overhead);
        let packed = pack_schedule_rows(lines, rows_budget);
        if packed.len() == total_guess {
            if packed.len() == 1 {
                return vec![packed[0]
                    .iter()
                    .map(|s| (*s).as_str())
                    .collect::<Vec<_>>()
                    .join("\n")];
            }
            let total = page_offset + packed.len() as i64;
            return packed
                .into_iter()
                .enumerate()
                .map(|(index, rows)| {
                    let current = page_offset + index as i64 + 1;
                    format!(
                        "{}\n{}",
                        rows.iter()
                            .map(|s| (*s).as_str())
                            .collect::<Vec<_>>()
                            .join("\n"),
                        schedule_list_page_indicator(current, total)
                    )
                })
                .collect();
        }
        total_guess = packed.len().max(1);
    }
}

/// Render one 1-based page of whole-row lines. Omitting the option means page
/// 1; callers pass the parsed `page` (default 1) straight through.
#[must_use]
pub fn schedule_list_page(lines: &[String], page: i64) -> String {
    if page < 1 {
        return schedule_list_page_underflow_text().to_owned();
    }
    let pages = schedule_list_pages(lines);
    let total = pages.len() as i64;
    if page > total {
        return schedule_list_page_out_of_range_text(page, total);
    }
    pages[(page - 1) as usize].clone()
}

/// Greedily pack row lines into page buckets whose joined (`\n`) UTF-16
/// length fits `rows_budget`. A single row always fits on its own page (rows
/// are bounded far below the message ceiling); rows are never split.
fn pack_schedule_rows(lines: &[String], rows_budget: usize) -> Vec<Vec<&String>> {
    let mut pages: Vec<Vec<&String>> = vec![Vec::new()];
    let mut current_len: usize = 0;
    for line in lines {
        let line_len = crate::message_safety::text_len(line);
        let add = if pages.last().expect("at least one page").is_empty() {
            line_len
        } else {
            1 + line_len
        };
        if current_len + add > rows_budget && !pages.last().expect("at least one page").is_empty() {
            pages.push(vec![line]);
            current_len = line_len;
        } else {
            pages.last_mut().expect("at least one page").push(line);
            current_len += add;
        }
    }
    pages
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
    /// Proven no-effect transient refusal: safely re-queue at `retry_at_ms`.
    Retryable { retry_at_ms: u64 },
    /// Possibly accepted delivery: hold the occurrence without resend/advance.
    DeliveryUncertain,
    /// Permanent failure (deleted channel, 4xx): advance/disable like a run so
    /// a dead row cannot wedge the queue.
    FailedPermanent,
    /// The definition changed or was cancelled while Discord was posting: the
    /// completion no longer owns the row (legacy `stale_completion`).
    StaleCompletion,
    /// Discord accepted the post but recording the run failed: confirmed
    /// cleanup permits retry; otherwise retain the durable uncertainty fence.
    PostedUnrecorded { cleaned: bool },
}

impl OccurrenceOutcome {
    /// Legacy `scheduled.run` audit outcome for this result.
    #[must_use]
    pub fn audit_outcome(&self) -> &'static str {
        match self {
            Self::Posted { .. } => "ok",
            Self::Retryable { .. } => "retry_scheduled",
            Self::DeliveryUncertain => "delivery_uncertain",
            Self::FailedPermanent => "post_failed",
            Self::StaleCompletion => "stale_completion",
            Self::PostedUnrecorded { cleaned: true } => "persistence_failed_cleaned",
            Self::PostedUnrecorded { cleaned: false } => "persistence_failed_uncertain",
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
    fn validation_refusal_displays_are_pinned() {
        // Every ScheduleError variant, reached through the real validate path,
        // with its member-visible Display text byte-pinned.
        let err = validate_schedule(&input("", Some(1), None)).expect_err("empty body");
        assert_eq!(err, ScheduleError::BodyEmpty);
        assert_eq!(
            err.to_string(),
            "Message body must be between 1 and 2000 characters."
        );

        let err =
            validate_schedule(&input(&"x".repeat(2001), Some(1), None)).expect_err("long body");
        assert_eq!(err, ScheduleError::BodyTooLong);
        assert_eq!(
            err.to_string(),
            "Message body must be between 1 and 2000 characters."
        );

        let err = validate_schedule(&input("x", None, None)).expect_err("missing timing");
        assert_eq!(err, ScheduleError::MissingTiming);
        assert_eq!(
            err.to_string(),
            "Give either in-minutes (one-shot) or every-minutes (recurring)."
        );

        for value in [0, 525_601] {
            let err =
                validate_schedule(&input("x", Some(value), None)).expect_err("in-minutes range");
            assert_eq!(err, ScheduleError::InMinutesOutOfRange(value));
            assert_eq!(
                err.to_string(),
                format!("in-minutes must be between 1 and 525600, got {value}.")
            );
        }

        for value in [59, 525_601] {
            let err =
                validate_schedule(&input("x", None, Some(value))).expect_err("every-minutes range");
            assert_eq!(err, ScheduleError::EveryMinutesOutOfRange(value));
            assert_eq!(
                err.to_string(),
                format!("every-minutes must be between 60 and 525600, got {value}.")
            );
        }
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
    fn neutralize_splits_every_invite_host_and_stays_idempotent() {
        // Same three hosts as the automod invite policy; casing preserved.
        for (hostile, broken) in [
            ("join discord.gg/abc", "join discord.\u{200b}gg/abc"),
            ("see DISCORD.GG/abc", "see DISCORD.\u{200b}GG/abc"),
            (
                "see https://discord.com/invite/xyz",
                "see https://discord.\u{200b}com/invite/xyz",
            ),
            (
                "see https://DISCORDAPP.COM/INVITE/xyz",
                "see https://DISCORDAPP.\u{200b}COM/INVITE/xyz",
            ),
            ("@everyone party", "@\u{200b}everyone party"),
        ] {
            let safe = neutralize_scheduled_body(hostile);
            assert_eq!(safe, broken, "{hostile:?}");
            assert_eq!(
                neutralize_scheduled_body(&safe),
                safe,
                "replay must be idempotent: {hostile:?}"
            );
        }
        // Benign bodies pass through byte-identical.
        for benign in [
            "hello world",
            "game night bring snacks",
            "Meeting at 5pm",
            "plain discord chat with no link",
        ] {
            assert_eq!(neutralize_scheduled_body(benign), benign);
        }
        // Multibyte text around a host never panics and still defuses.
        let mixed = format!("{}discord.gg/abc{}", "界".repeat(10), "😀".repeat(5));
        let safe = neutralize_scheduled_body(&mixed);
        assert!(safe.contains("discord.\u{200b}gg"));
        assert_eq!(neutralize_scheduled_body(&safe), safe);
    }

    #[test]
    fn neutralize_growth_can_be_truncated_back_to_the_ceiling() {
        // A legacy row at the old pre-neutralize limit grows by one unit per
        // mass mention; the ticker's truncate-then-post keeps it sendable.
        let body = format!("{} @everyone", "x".repeat(1990));
        assert_eq!(crate::message_safety::text_len(&body), MAX_BODY_CHARS);
        let neutralized = neutralize_scheduled_body(&body);
        assert_eq!(
            crate::message_safety::text_len(&neutralized),
            MAX_BODY_CHARS + 1
        );
        let posted =
            crate::message_safety::truncate(&neutralized, crate::message_safety::CONTENT_LIMIT);
        assert_eq!(
            crate::message_safety::text_len(&posted),
            crate::message_safety::CONTENT_LIMIT
        );
        assert!(!posted.contains("@everyone"));
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
        assert_eq!(
            no_unique_match_text("abc"),
            "No unique scheduled message matches `abc`. Use the full id from /schedule-list."
        );
        assert_eq!(
            no_unique_match_text("a1b2c3d4"),
            "No unique scheduled message matches `a1b2c3d4`. Use the full id from /schedule-list."
        );
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
    fn overflow_line_names_the_cut_row_count() {
        // Pinned by the bounded-listing acceptance tests: keep byte-exact.
        assert_eq!(schedule_list_overflow_line(2), "(truncated, 2 more)");
        assert_eq!(schedule_list_overflow_line(1), "(truncated, 1 more)");
    }

    #[test]
    fn truncated_marker_names_a_cut_without_a_count() {
        // Fallback for a failed overflow count: static shape, no count.
        assert_eq!(schedule_list_truncated_text(), "(truncated)");
    }

    #[test]
    fn overflow_for_count_names_a_failed_count() {
        // The failed-count arm of the capped read: a full window whose
        // count query fails still marks the cut. Deleting the caller's
        // push breaks the over-cap acceptance; remapping None here breaks
        // this pin.
        assert_eq!(
            schedule_list_overflow_for_count(None, 25),
            Some("(truncated)".to_owned())
        );
        assert_eq!(
            schedule_list_overflow_for_count(Some(27), 25),
            Some("(truncated, 2 more)".to_owned())
        );
        assert_eq!(schedule_list_overflow_for_count(Some(25), 25), None);
        assert_eq!(schedule_list_overflow_for_count(Some(3), 25), None);
    }

    /// One 32-hex-id fixture-shaped row (~84 UTF-16 units): thirty of these
    /// always need more than one 2,000-unit page, so the multi-page pager
    /// branches stay covered without a database.
    fn long_list_row(n: u32) -> String {
        schedule_list_line(
            &format!("a1b2c3d4e5f60718293a4b5c{n:08x}"),
            "1234567890123456789",
            "2030-01-01T00:00:00.000Z",
            None,
            true,
        )
    }

    #[test]
    fn continuation_hint_names_the_next_page() {
        assert_eq!(schedule_list_continuation_hint(3), "Continued on page 3.");
        assert_eq!(schedule_list_continuation_hint(11), "Continued on page 11.");
    }

    #[test]
    fn global_pages_continue_numbering_from_the_first_window() {
        let rows: Vec<String> = (0..30).map(long_list_row).collect();
        let pages = schedule_list_pages_global(&rows, 2);
        assert!(
            pages.len() >= 2,
            "thirty eighty-four-unit rows need several pages"
        );
        let total = 2 + pages.len() as i64;
        for (index, page) in pages.iter().enumerate() {
            let current = 2 + index as i64 + 1;
            assert!(
                page.ends_with(&format!("\nPage {current} of {total}")),
                "hidden page carries its global number: {page}"
            );
            assert!(
                crate::message_safety::text_len(page) <= SCHEDULE_LIST_MESSAGE_LIMIT,
                "renumbered page still fits the message ceiling"
            );
        }
        // Every row listed exactly once, in order, none split or dropped.
        let listed: Vec<&str> = pages
            .iter()
            .flat_map(|page| page.lines().filter(|line| line.starts_with('`')))
            .collect();
        assert_eq!(listed, rows.iter().map(String::as_str).collect::<Vec<_>>());
    }

    #[test]
    fn global_pages_leave_a_single_page_unlabelled() {
        // Acceptance-pinned: short-id hidden windows render bare, and an
        // empty window keeps the legacy empty-guild text.
        let rows = vec![
            "`cap-25` `<#3333>` 2030-01-26T00:00:00.000Z".to_owned(),
            "`cap-26` `<#3333>` 2030-01-27T00:00:00.000Z".to_owned(),
        ];
        assert_eq!(schedule_list_pages_global(&rows, 1), vec![rows.join("\n")]);
        assert_eq!(
            schedule_list_pages_global(&[], 1),
            vec!["Nothing scheduled.".to_owned()]
        );
    }

    #[test]
    fn global_pages_without_an_offset_match_the_plain_pack() {
        let rows: Vec<String> = (0..30).map(long_list_row).collect();
        assert_eq!(
            schedule_list_pages_global(&rows, 0),
            schedule_list_pages(&rows)
        );
    }

    #[test]
    fn first_pages_pass_through_single_page_and_unhidden_windows() {
        // Single-page windows keep the acceptance-pinned shape even with
        // hidden rows behind them; unhidden windows never gain a hint.
        let multi: Vec<String> = (0..30).map(long_list_row).collect();
        assert_eq!(
            schedule_list_first_pages(&multi, false),
            schedule_list_pages(&multi)
        );
        let single = vec!["`a` `<#1>` 2030-01-01T00:00:00.000Z".to_owned()];
        assert_eq!(
            schedule_list_first_pages(&single, true),
            schedule_list_pages(&single)
        );
    }

    #[test]
    fn first_pages_point_past_a_multi_page_window() {
        // Twenty-six rows pack to two plain pages with a small last page, so
        // the hint fits without growing the count.
        let rows: Vec<String> = (0..26).map(long_list_row).collect();
        let plain = schedule_list_pages(&rows);
        assert!(plain.len() >= 2, "twenty-six rows need several pages");
        let pages = schedule_list_first_pages(&rows, true);
        assert_eq!(pages.len(), plain.len(), "the hint fits the last page");
        assert_eq!(pages[0], plain[0], "earlier pages are untouched");
        let total = pages.len() as i64;
        assert!(
            pages.last().expect("at least one page").ends_with(&format!(
                "\n{}\n{}",
                schedule_list_continuation_hint(total + 1),
                schedule_list_page_indicator(total, total)
            )),
            "last first-window page names the next page: {}",
            pages.last().expect("at least one page")
        );
        for page in &pages {
            assert!(
                crate::message_safety::text_len(page) <= SCHEDULE_LIST_MESSAGE_LIMIT,
                "hinted page still fits the message ceiling"
            );
        }
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
            OccurrenceOutcome::DeliveryUncertain.audit_outcome(),
            "delivery_uncertain"
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
            "persistence_failed_uncertain"
        );
    }

    #[test]
    fn body_only_correction_shares_the_creation_policy() {
        // Ceilings pass byte-identical; one past any ceiling refuses.
        assert_eq!(
            validate_schedule_body(&"x".repeat(2000)).expect("ceiling ok"),
            "x".repeat(2000)
        );
        assert_eq!(
            validate_schedule_body(&"😀".repeat(1000)).expect("emoji ceiling ok"),
            "😀".repeat(1000)
        );
        for invalid in [
            "".to_owned(),
            " \n\t\u{200b}".to_owned(),
            "x".repeat(2001),
            "😀".repeat(1001),
            format!("{}@everyone", "x".repeat(1991)),
        ] {
            assert!(
                validate_schedule_body(&invalid).is_err(),
                "{invalid:?} refuses like creation"
            );
        }
        // Same neutralization creation stores: mentions split, invites
        // delinkified, already-neutral text untouched.
        assert_eq!(
            validate_schedule_body("@everyone @here discord.gg/abc").expect("neutralizes"),
            "@\u{200b}everyone @\u{200b}here discord.\u{200b}gg/abc"
        );
        // Byte-identical refusal text: the handler replies with it.
        assert_eq!(
            validate_schedule_body("").expect_err("empty").to_string(),
            "Message body must be between 1 and 2000 characters."
        );
        assert_eq!(
            validate_schedule_body(&"x".repeat(2001))
                .expect_err("long")
                .to_string(),
            "Message body must be between 1 and 2000 characters."
        );
    }

    #[test]
    fn inspect_view_defaults_to_metadata_and_names_corrections() {
        assert_eq!(parse_inspect_view(None), Ok(InspectView::Metadata));
        assert_eq!(
            parse_inspect_view(Some("metadata")),
            Ok(InspectView::Metadata)
        );
        assert_eq!(parse_inspect_view(Some("body")), Ok(InspectView::Body));
        for unknown in [
            "".to_owned(),
            "METADATA".to_owned(),
            "summary".to_owned(),
            "metadata ".to_owned(),
            "body ".to_owned(),
            "x".repeat(9),
        ] {
            let refusal = parse_inspect_view(Some(unknown.as_str())).expect_err("unknown view");
            assert!(
                refusal.contains("/schedule-inspect") && refusal.contains("view:<metadata|body>"),
                "{unknown:?} corrects: {refusal}"
            );
            assert!(!refusal.contains(&unknown) || unknown.is_empty());
        }
    }

    #[test]
    fn inspect_state_words_are_truthful_and_always_name_disabled() {
        // (claimed, nonce, uncertain, enabled) -> state word.
        for (fences, word) in [
            ((false, false, false, true), "idle"),
            ((false, false, false, false), "disabled"),
            ((true, true, false, true), "claim active"),
            ((true, false, true, true), "claim active"),
            ((false, true, false, true), "nonce pending"),
            ((false, true, true, true), "delivery uncertain"),
            ((false, false, true, true), "delivery uncertain"),
            ((true, false, false, false), "claim active (disabled)"),
            ((false, true, true, false), "delivery uncertain (disabled)"),
        ] {
            let (claimed, nonce, uncertain, enabled) = fences;
            assert_eq!(
                schedule_delivery_state_word(claimed, nonce, uncertain, enabled),
                word,
                "{fences:?}"
            );
        }
    }

    #[test]
    fn inspect_fragments_render_raw_stored_facts() {
        assert_eq!(schedule_inspect_cadence_text(None), "one-shot");
        assert_eq!(schedule_inspect_cadence_text(Some(3600)), "every 3600s");
        assert_eq!(schedule_inspect_cadence_text(Some(90)), "every 90s");
        assert_eq!(
            schedule_inspect_last_run_text(Some("2026-09-30T00:00:00.000Z"), Some("9000")),
            "2026-09-30T00:00:00.000Z (message 9000)"
        );
        assert_eq!(
            schedule_inspect_last_run_text(Some("2026-09-30T00:00:00.000Z"), None),
            "2026-09-30T00:00:00.000Z"
        );
        assert_eq!(schedule_inspect_last_run_text(None, None), "never");
        assert_eq!(schedule_inspect_last_run_text(None, Some("9000")), "never");
    }

    #[test]
    fn inspect_and_edit_replies_carry_guidance_without_promises() {
        let metadata = schedule_inspect_metadata_text(
            "abc",
            "3333",
            "2030-01-01T00:00:00.000Z",
            "every 3600s",
            "idle",
            "2026-09-30T00:00:00.000Z (message 9000)",
            "rev",
        );
        for fact in [
            "abc",
            "3333",
            "2030-01-01T00:00:00.000Z",
            "3600",
            "idle",
            "9000",
            "2026-09-30",
            "Revision: rev",
        ] {
            assert!(metadata.contains(fact), "metadata keeps {fact}: {metadata}");
        }
        let saved = schedule_edit_saved_text("abc", "rev2");
        assert!(saved.to_ascii_lowercase().contains("saved"));
        assert!(saved.contains("rev2"));
        for promise in [
            "will be delivered",
            "will be posted",
            "delivery guaranteed",
            "sent successfully",
        ] {
            assert!(!saved.to_ascii_lowercase().contains(promise));
        }
        assert!(schedule_edit_unchanged_text()
            .to_ascii_lowercase()
            .contains("unchanged"));
        for refusal in [
            schedule_edit_stale_text("abc"),
            schedule_edit_fenced_text("abc", "a delivery claim is active"),
            schedule_edit_revision_required_text().to_owned(),
            schedule_unknown_id_text().to_owned(),
        ] {
            assert!(
                refusal.contains("/schedule-inspect") || refusal.contains("/schedule-list"),
                "guidance: {refusal}"
            );
            assert!(
                !refusal.to_ascii_lowercase().contains("saved"),
                "refusal is not a save: {refusal}"
            );
        }
        assert!(schedule_edit_stale_text("abc").contains("`abc`"));
        // The unknown-id text never echoes hostile input.
        assert!(!schedule_unknown_id_text().contains("@everyone"));
    }
}
