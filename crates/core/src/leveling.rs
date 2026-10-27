//! Leveling domain: XP curves, award adjudication, profiles, reward plans,
//! and reply text.
//!
//! Ports the DB-free core of `src/leveling/service.ts` + `src/leveling/discord.ts`
//! from two-bot. The curve is MEE6-compatible (`totalXpForLevel`) so imported
//! XP (TOG-9882) and organic XP share one ladder.
//!
//! Runtime shape: pure adjudication and planning live here (inputs →
//! plain-data outcomes, unit-testable); the sqlx award/profile/leaderboard
//! reads and writes live in `leveling_store` behind the crate `db` feature
//! (same split as `moderation` + its store). The S4 interaction router and
//! REST executor consume the outcome types here; until they land, nothing in
//! this module publishes commands or touches Discord.
//!
//! Legacy posts no channel message on level-up; `applyLevelRoles` grants
//! rewards and `removeLevelRoles` is a separate staging-only operation.
//! [`RewardRolePlan`] preserves that separation. The executor must enforce
//! the staging fence, Manage Roles and role hierarchy before any revocation.

/// XP per qualifying message (legacy `MESSAGE_XP`).
pub const MESSAGE_XP: u64 = 15;
/// Per-source award cooldown (legacy `MESSAGE_COOLDOWN_SECONDS` /
/// `VOICE_COOLDOWN_SECONDS`, both 60s).
pub const AWARD_COOLDOWN_SECONDS: u64 = 60;
/// Voice XP per whole minute (legacy `VOICE_XP_PER_MINUTE`, MEE6-compat 5/min).
pub const VOICE_XP_PER_MINUTE: u64 = 5;
/// Storage ceiling (legacy `MAX_STORED_XP` = `Number.MAX_SAFE_INTEGER`).
pub const MAX_STORED_XP: u64 = 9_007_199_254_740_991;
/// Anti-gaming daily cap: message XP per guild+member UTC day (AC2).
pub const MESSAGE_DAILY_CAP_XP: u64 = 150;
/// Anti-gaming daily cap: voice XP per guild+member UTC day, 24 min × 5 XP (AC2).
pub const VOICE_DAILY_CAP_XP: u64 = 120;
/// Anti-gaming short-message floor: trimmed bodies below this length earn 0 XP
/// unless they carry an embed or attachment (AC2).
pub const MESSAGE_SHORT_MIN_LEN: usize = 8;
/// Seasonal decay grace: no decay within this many inactive days (AC2).
pub const SEASONAL_DECAY_GRACE_DAYS: u64 = 28;

/// XP source channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XpSource {
    Message,
    Voice,
}

impl XpSource {
    /// Storage value (`xp_cooldowns.source` / `xp_awards.source` CHECK).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Voice => "voice",
        }
    }
}

/// Total XP required to *reach* `level` (MEE6 curve, legacy `totalXpForLevel`).
///
/// `totalXpForLevel(0) == 0`; panics on negative input like the legacy throw.
#[must_use]
pub fn total_xp_for_level(level: u64) -> u64 {
    // floor((5/6) * L * (2L² + 27L + 91)) — integer arithmetic, no float drift.
    (5 * level * (2 * level * level + 27 * level + 91)) / 6
}

/// Highest level whose floor is at or below `xp` (legacy `levelForXp`).
#[must_use]
pub fn level_for_xp(xp: u64) -> u64 {
    let mut low = 0u64;
    let mut high = 1u64;
    while total_xp_for_level(high) <= xp {
        high *= 2;
    }
    while low + 1 < high {
        let mid = (low + high) / 2;
        if total_xp_for_level(mid) <= xp {
            low = mid;
        } else {
            high = mid;
        }
    }
    low
}

/// Outcome of one award attempt (legacy `XpAward`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XpAward {
    pub awarded: u64,
    pub total_xp: u64,
    pub level: u64,
    pub previous_level: u64,
    pub leveled_up: bool,
}

/// Pure award adjudication: given the member's current total, the amount, the
/// seconds since the last award from this source, and the remaining headroom
/// to the ceiling, decide the award. Cooldown and ceiling rejections award 0
/// and never level (legacy `award` early-returns + `XpCeilingReached` path).
#[must_use]
pub fn adjudicate_award(
    previous_xp: u64,
    amount: u64,
    seconds_since_last_award: Option<u64>,
    headroom: u64,
) -> XpAward {
    let previous_level = level_for_xp(previous_xp);
    let cooldown_ok = seconds_since_last_award.is_none_or(|s| s >= AWARD_COOLDOWN_SECONDS);
    if amount == 0 || !cooldown_ok || amount > headroom {
        return XpAward {
            awarded: 0,
            total_xp: previous_xp,
            level: previous_level,
            previous_level,
            leveled_up: false,
        };
    }
    let total_xp = previous_xp + amount;
    let level = level_for_xp(total_xp);
    XpAward {
        awarded: amount,
        total_xp,
        level,
        previous_level,
        leveled_up: level > previous_level,
    }
}

/// Anti-gaming inputs for one message award attempt (AC2). The store derives
/// the day-scoped counters (`daily_award_index`, `day_total_xp`), the
/// cooldown clock (`seconds_since_last_award`) and the trailing channel
/// history (`same_channel_awards_in_10min`) from its tables; the gateway
/// supplies the content shape. Every counter is an explicit input so the
/// policy stays pure and unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageAwardInput {
    pub previous_xp: u64,
    /// 0-based count of message awards already granted today (UTC,
    /// per guild+member day). Drives the 15/5/0 ladder.
    pub daily_award_index: usize,
    /// Message XP already granted today (UTC). At/above the daily cap the
    /// award is 0.
    pub day_total_xp: u64,
    pub seconds_since_last_award: Option<u64>,
    /// Trimmed message chars (embeds/attachments excluded; see next field).
    pub content_len: usize,
    pub has_embed_or_attachment: bool,
    /// Prior awards from the same channel in the trailing 10 minutes.
    /// At >= 2 the ladder amount halves (floor).
    pub same_channel_awards_in_10min: usize,
    pub headroom: u64,
}

/// Pure anti-gaming message adjudication (AC2). Evaluation order: cooldown
/// (< 60 s → 0) → short-message (`content_len` below the floor with no
/// embed/attachment → 0; the store still records cooldown) → daily cap
/// (`day_total_xp` at/above cap → 0) → ladder (index < 5 → 15, < 10 → 5,
/// else 0) → burst halving (same-channel history >= 2 → halve, floor) →
/// ceiling (`amount > headroom` → 0). Lifetime XP is never decayed here;
/// seasonal decay is [`seasonal_xp_after_decay`].
#[must_use]
pub fn adjudicate_message_award(input: MessageAwardInput) -> XpAward {
    let previous_level = level_for_xp(input.previous_xp);
    let zero = || XpAward {
        awarded: 0,
        total_xp: input.previous_xp,
        level: previous_level,
        previous_level,
        leveled_up: false,
    };
    if input
        .seconds_since_last_award
        .is_some_and(|s| s < AWARD_COOLDOWN_SECONDS)
    {
        return zero();
    }
    if input.content_len < MESSAGE_SHORT_MIN_LEN && !input.has_embed_or_attachment {
        return zero();
    }
    if input.day_total_xp >= MESSAGE_DAILY_CAP_XP {
        return zero();
    }
    let ladder = if input.daily_award_index < 5 {
        MESSAGE_XP
    } else if input.daily_award_index < 10 {
        5
    } else {
        return zero();
    };
    let amount = if input.same_channel_awards_in_10min >= 2 {
        ladder / 2
    } else {
        ladder
    };
    if amount == 0 || amount > input.headroom {
        return zero();
    }
    let total_xp = input.previous_xp + amount;
    let level = level_for_xp(total_xp);
    XpAward {
        awarded: amount,
        total_xp,
        level,
        previous_level,
        leveled_up: level > previous_level,
    }
}

/// Whole minutes of voice credit for a session duration (legacy floors after
/// clamping negatives to zero; here the input is already `u64` seconds).
#[must_use]
pub fn voice_minutes(duration_seconds: u64) -> u64 {
    duration_seconds / 60
}

/// Voice XP for a session duration, clamped to the anti-gaming daily cap
/// (AC2: 24 min × 5 XP = 120 XP per guild+member UTC day). The per-day fence
/// itself lives in the store, which tracks the day total; this clamp keeps
/// the pure conversion honest for a full-rate day.
#[must_use]
pub fn voice_xp_for_duration(duration_seconds: u64) -> u64 {
    (voice_minutes(duration_seconds) * VOICE_XP_PER_MINUTE).min(VOICE_DAILY_CAP_XP)
}

/// Seasonal XP after inactivity decay (AC2): no decay within the 28-day
/// grace window, then 10 % per overdue week, multiplicative
/// (`value = value * 9 / 10` per week, integer floor). Lifetime XP is never
/// decayed — decay applies to the seasonal projection only, written by a
/// batch job alongside `source='decay'` audit rows.
#[must_use]
pub fn seasonal_xp_after_decay(seasonal_xp: u64, days_inactive: u64) -> u64 {
    seasonal_xp_after_decay_weeks(seasonal_xp, seasonal_decay_total_weeks(days_inactive))
}

/// Overdue weeks for `days_inactive` under the seasonal-decay grace: 0 inside
/// the 28-day grace, then one per full 7-day week past it. One definition for
/// the pure curve above and the runtime tick, so the grace math cannot drift.
#[must_use]
pub fn seasonal_decay_total_weeks(days_inactive: u64) -> u64 {
    if days_inactive <= SEASONAL_DECAY_GRACE_DAYS {
        0
    } else {
        (days_inactive - SEASONAL_DECAY_GRACE_DAYS) / 7
    }
}

/// Newly overdue weeks since `already_applied` (the `decayed_weeks` watermark
/// on the seasonal row): the tick applies only these, so a rerun of the same
/// tick applies zero and consecutive Monday ticks apply exactly one each.
#[must_use]
pub fn seasonal_decay_new_weeks(days_inactive: u64, already_applied: u64) -> u64 {
    seasonal_decay_total_weeks(days_inactive).saturating_sub(already_applied)
}

/// Apply `weeks` of 10 % multiplicative decay to `seasonal_xp`.
#[must_use]
pub fn seasonal_xp_after_decay_weeks(seasonal_xp: u64, weeks: u64) -> u64 {
    let mut value = seasonal_xp;
    for _ in 0..weeks {
        value = value * 9 / 10;
    }
    value
}

/// Days since the Unix epoch for a civil date (Howard Hinnant's
/// `days_from_civil`): integer math over the full i64 day range, no date crate.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Civil `(year, month)` for days since the Unix epoch (the matching
/// `civil_from_days` inverse).
fn civil_year_month_from_days(days: i64) -> (i64, u32) {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let year = if mp >= 10 { y + 1 } else { y };
    let month = if mp >= 10 { mp - 9 } else { mp + 3 };
    (year, month as u32)
}

/// First Monday 00:00 UTC on or after the first day of quarter `q` in `year`,
/// as days since the epoch. 1970-01-01 was a Thursday, so Monday-zero
/// weekday is `(days + 3) % 7`.
fn monday_aligned_season_start_days(year: i64, quarter: u32) -> i64 {
    let first = days_from_civil(year, (quarter - 1) * 3 + 1, 1);
    let weekday_monday_zero = (first + 3).rem_euclid(7);
    first + (7 - weekday_monday_zero) % 7
}

/// Season scope (`YYYY-QN`) for epoch millis (AC3). Seasons start Monday
/// 00:00 UTC on/after the quarter's first day so every season boundary
/// coincides with a Monday scorecard week boundary: days before a quarter's
/// Monday start still belong to the previous season.
#[must_use]
pub fn season_id_for_ms(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let (year, month) = civil_year_month_from_days(days);
    let quarter = (month - 1) / 3 + 1;
    if days >= monday_aligned_season_start_days(year, quarter) {
        format!("{year:04}-Q{quarter}")
    } else if quarter > 1 {
        format!("{year:04}-Q{}", quarter - 1)
    } else {
        format!("{:04}-Q4", year - 1)
    }
}

/// Format a count the way legacy `Number.toLocaleString()` (en-US) does:
/// thousands grouped with `,`, no decimals. Legacy groups XP/progress counts,
/// but leaves level, rank and member count ungrouped.
#[must_use]
pub fn grouped_number(n: u64) -> String {
    let digits = n.to_string().into_bytes();
    let mut out = Vec::with_capacity(digits.len() + digits.len() / 3);
    for (i, b) in digits.iter().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(b',');
        }
        out.push(*b);
    }
    String::from_utf8(out).expect("grouped digits are ASCII")
}

/// Rendered display-name bound for the `/rank` reply, in characters. Par with
/// the vote-grade public bound (`VOTE_KICK_PUBLIC_REASON_LIMIT` in the bot
/// crate, also `GOODBYE_USERNAME_LIMIT`): applied to the escaped output so the
/// bound holds on the wire.
pub const RANK_DISPLAY_NAME_LIMIT: usize = 512;

/// Sanitize a display name for the `/rank` reply name slot (goodbye grade):
/// fold newlines to spaces so the reply keeps its three template lines,
/// neutralize mass mentions with the shared neutralizer, split mention pills,
/// timestamps, slash-command and guild-navigation tokens (`<@`, `<#`, `<:`,
/// `<a:`, `<t:`, `</`, `<id:`) so they render literally, escape the
/// vote-reason markdown set, and cap the escaped output at
/// [`RANK_DISPLAY_NAME_LIMIT`] characters. Plain names pass through
/// byte-identical.
fn sanitize_rank_name(display_name: &str) -> String {
    let mut single_line = String::with_capacity(display_name.len());
    for chunk in display_name.split(['\r', '\n']) {
        if !single_line.is_empty() {
            single_line.push(' ');
        }
        single_line.push_str(chunk);
    }
    let neutralized = crate::message_safety::neutralize_mentions(&single_line);
    let pills = neutralized
        .replace("<@", "<\u{200b}@")
        .replace("<#", "<\u{200b}#")
        .replace("<:", "<\u{200b}:")
        .replace("<a:", "<\u{200b}a:")
        .replace("<t:", "<\u{200b}t:")
        .replace("</", "<\u{200b}/")
        .replace("<id:", "<\u{200b}id:");
    let mut escaped = String::with_capacity(pills.len());
    for ch in pills.chars() {
        if matches!(ch, '\\' | '`' | '*' | '_' | '~' | '|' | '[' | ']') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    let mut capped: String = escaped.chars().take(RANK_DISPLAY_NAME_LIMIT).collect();
    // A cut mid-escape would leave a lone backslash that eats the closing bold
    // delimiter, so trim on escape-pair boundaries instead.
    if capped.bytes().rev().take_while(|b| *b == b'\\').count() % 2 == 1 {
        capped.pop();
    }
    capped
}

/// `/rank` reply body (legacy `rankText`).
///
/// `display_name` is untrusted text rendered through goodbye-grade sanitizing
/// (single-line, mention-neutralized, markdown-escaped, length-capped); plain
/// names render byte-identical to legacy.
#[must_use]
pub fn rank_text(
    display_name: &str,
    level: u64,
    rank: Option<u64>,
    member_count: u64,
    xp: u64,
) -> String {
    let rank_label = match rank {
        Some(rank) => format!("Rank **#{rank}** of **{member_count}**"),
        None => "Rank **Unranked** (no XP recorded)".to_owned(),
    };
    let floor = total_xp_for_level(level);
    let next_floor = total_xp_for_level(level + 1);
    let progress = xp - floor;
    let span = next_floor - floor;
    let to_next = next_floor - xp;
    let safe = sanitize_rank_name(display_name);
    format!(
        "**{safe}**\nLevel **{level}** · {rank_label}\nXP **{xp}** · {progress}/{span} this level · **{to_next}** to level {}",
        level + 1,
        xp = grouped_number(xp),
        progress = grouped_number(progress),
        span = grouped_number(span),
        to_next = grouped_number(to_next),
    )
}

/// One `/leaderboard` row (legacy mapping, top 10, mentions without parsing).
#[must_use]
pub fn leaderboard_line(
    rank: u64,
    member_id: impl std::fmt::Display,
    level: u64,
    xp: u64,
) -> String {
    format!(
        "**{rank}.** <@{member_id}> · level **{level}** · {xp} XP",
        xp = grouped_number(xp)
    )
}

/// `/leaderboard` reply body; empty board gets the legacy fallback string.
#[must_use]
pub fn leaderboard_text(rows: &[String]) -> String {
    if rows.is_empty() {
        return "No XP has been earned yet.".to_owned();
    }
    std::iter::once("**TWO XP Leaderboard**".to_owned())
        .chain(rows.iter().cloned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Default `/leaderboard` page size (legacy `leaderboard(guildId, limit = 10)`).
pub const LEADERBOARD_DEFAULT_LIMIT: u64 = 10;
/// Legacy leaderboard page clamp (`Math.max(1, Math.min(25, ...))`).
pub const LEADERBOARD_MAX_LIMIT: u64 = 25;

/// Clamp a leaderboard page size to the legacy bounds (1–25).
#[must_use]
pub fn clamp_leaderboard_limit(limit: u64) -> u64 {
    limit.clamp(1, LEADERBOARD_MAX_LIMIT)
}

/// A member's full leveling read model (legacy `LevelProfile`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LevelProfile {
    pub guild_id: String,
    pub member_id: String,
    pub xp: u64,
    pub level: u64,
    pub message_xp: u64,
    pub voice_xp: u64,
    pub imported_xp: u64,
    /// 1-based rank with XP ties broken by member id ascending. A member
    /// with no XP row is unranked (`None`); a stored zero-XP row still ranks.
    pub rank: Option<u64>,
    pub member_count: u64,
    /// `totalXpForLevel(level + 1)`.
    pub next_level_xp: u64,
}

/// One leaderboard row (legacy `LeaderboardEntry`); `rank` is the 1-based
/// position on the returned page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderboardEntry {
    pub member_id: String,
    pub xp: u64,
    pub level: u64,
    pub rank: u64,
}

/// A `/rank` reply: legacy answers ephemerally (`MessageFlags.Ephemeral`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankReply {
    pub content: String,
    pub ephemeral: bool,
}

/// Build the `/rank` reply for a profile. `display_name` is
/// `globalName ?? username` — resolving it is the adapter's job.
#[must_use]
pub fn rank_reply(profile: &LevelProfile, display_name: &str) -> RankReply {
    RankReply {
        content: rank_text(
            display_name,
            profile.level,
            profile.rank,
            profile.member_count,
            profile.xp,
        ),
        ephemeral: true,
    }
}

/// Build the `/rank details:true` reply: the default card plus one labeled
/// line per XP source and the authoritative stored total. The sources are
/// labeled independently and never re-summed here — adjustments can leave
/// them differing from the stored total, and the reply says so instead of
/// assuming they add up.
#[must_use]
pub fn rank_reply_detailed(profile: &LevelProfile, display_name: &str) -> RankReply {
    let mut content = rank_text(
        display_name,
        profile.level,
        profile.rank,
        profile.member_count,
        profile.xp,
    );
    content.push_str(
        format!(
            "\nMessage XP **{}** · Voice XP **{}** · Imported XP **{}**\nStored total XP **{}** (sources can differ from the stored total after adjustments)",
            grouped_number(profile.message_xp),
            grouped_number(profile.voice_xp),
            grouped_number(profile.imported_xp),
            grouped_number(profile.xp),
        )
        .as_str(),
    );
    RankReply {
        content,
        ephemeral: true,
    }
}

/// A `/leaderboard` reply: legacy posts it publicly with mentions suppressed
/// (`allowedMentions: { parse: [] }`), never ephemerally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderboardReply {
    pub content: String,
    /// Always true: the executor must suppress mention parsing (legacy
    /// `allowedMentions: { parse: [] }`), so `<@id>` rows never ping.
    pub suppress_mentions: bool,
}

/// Build the `/leaderboard` reply for one page of entries (legacy
/// `handleLeaderboard`: top-10 header shape, empty-board fallback).
#[must_use]
pub fn leaderboard_reply(entries: &[LeaderboardEntry]) -> LeaderboardReply {
    let rows: Vec<String> = entries
        .iter()
        .map(|e| leaderboard_line(e.rank, &e.member_id, e.level, e.xp))
        .collect();
    LeaderboardReply {
        content: leaderboard_text(&rows),
        suppress_mentions: true,
    }
}

/// First season served by the seasonal board. The seasonal mirror shipped
/// mid-quarter (2026-Q4 opened 2026-10-05, the mirror landed 2026-10-13),
/// so 2026-Q4 omits XP earned before the mirror. The board marks it partial
/// until a backfill lands or the quarter rolls over; later quarters track
/// from their first day and render the plain header.
pub const FIRST_PARTIAL_SEASON: &str = "2026-Q4";
/// Partial-season marker for the first quarter. Kept inside the header line
/// so the top-10 shape (header plus 10 rows) is unchanged while Discord
/// members see the gap, not just operators reading docs.
pub const PARTIAL_SEASON_NOTE: &str = " — partial: early-season XP not counted";

/// Build the seasonal `/leaderboard` reply (AC3): same 1–25 clamp and top-10
/// public shape as [`leaderboard_reply`], scoped to one season. Lifetime
/// standings render through [`leaderboard_reply_lifetime`] instead. The
/// first partial quarter renders the partial marker inside its header; an
/// empty first quarter keeps the marker on its header plus the honest empty
/// body, never lifetime totals.
#[must_use]
pub fn leaderboard_reply_for_season(
    entries: &[LeaderboardEntry],
    season_id: &str,
) -> LeaderboardReply {
    let rows: Vec<String> = entries
        .iter()
        .map(|e| leaderboard_line(e.rank, &e.member_id, e.level, e.xp))
        .collect();
    let body = if rows.is_empty() {
        "No XP has been earned yet.".to_owned()
    } else {
        rows.join("\n")
    };
    let header = if season_id == FIRST_PARTIAL_SEASON {
        format!("**TWO XP Leaderboard ({season_id}{PARTIAL_SEASON_NOTE})**")
    } else {
        format!("**TWO XP Leaderboard ({season_id})**")
    };
    LeaderboardReply {
        content: format!("{header}\n{body}"),
        suppress_mentions: true,
    }
}

/// Build the explicit-lifetime `/leaderboard` reply: same 1–25 clamp and
/// top-10 public shape as [`leaderboard_reply_for_season`], reading lifetime
/// totals instead of one season. The header always labels the lifetime
/// scope and never a season id or the partial marker; an empty guild keeps
/// the honest empty body.
#[must_use]
pub fn leaderboard_reply_lifetime(entries: &[LeaderboardEntry]) -> LeaderboardReply {
    let rows: Vec<String> = entries
        .iter()
        .map(|e| leaderboard_line(e.rank, &e.member_id, e.level, e.xp))
        .collect();
    let body = if rows.is_empty() {
        "No XP has been earned yet.".to_owned()
    } else {
        rows.join("\n")
    };
    LeaderboardReply {
        content: format!("**TWO XP Leaderboard (Lifetime)**\n{body}"),
        suppress_mentions: true,
    }
}

/// One level → role pair as stored (legacy `LevelRoleReward`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LevelRoleReward {
    pub level: u64,
    pub role_id: String,
}

/// Invalid reward configuration (legacy `replaceRoleRewards` throws).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RewardConfigError {
    #[error("reward level must be a positive integer")]
    InvalidLevel,
    #[error("invalid Discord role id: {0}")]
    InvalidRoleId(String),
}

fn is_snowflake(value: &str) -> bool {
    (17..=20).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_digit())
}

/// Normalize a reward configuration: last row per level wins (legacy
/// `Map.set` overwrite), levels ascending. Rejects non-positive levels and
/// non-snowflake role ids before any write (legacy `replaceRoleRewards`).
pub fn normalize_role_rewards(
    rewards: &[LevelRoleReward],
) -> Result<Vec<LevelRoleReward>, RewardConfigError> {
    let mut normalized: std::collections::BTreeMap<u64, &str> = std::collections::BTreeMap::new();
    for reward in rewards {
        if reward.level == 0 || reward.level > i32::MAX as u64 {
            return Err(RewardConfigError::InvalidLevel);
        }
        if !is_snowflake(&reward.role_id) {
            return Err(RewardConfigError::InvalidRoleId(reward.role_id.clone()));
        }
        normalized.insert(reward.level, reward.role_id.as_str());
    }
    Ok(normalized
        .into_iter()
        .map(|(level, role_id)| LevelRoleReward {
            level,
            role_id: role_id.to_owned(),
        })
        .collect())
}

/// Whether level-up role writes apply. Legacy session onboarding suppresses
/// them (`levelRoleWritesForOnboardingMode`: `mode !== 'session'`); XP,
/// level-ups and `/rank` are unaffected — only `member.roles.add` is gated.
#[must_use]
pub fn level_role_writes_allowed(session_mode: bool) -> bool {
    !session_mode
}

/// Audit reason for reward grants (legacy `applyLevelRoles` reason).
#[must_use]
pub fn grant_audit_reason(level: u64) -> String {
    format!("TWO leveling: reached level {level}")
}

/// Audit reason for staging revocations (legacy `removeLevelRoles`).
#[must_use]
pub fn revoke_audit_reason(level: u64) -> String {
    format!("TWO leveling: below level thresholds at level {level}")
}

/// Idempotent level-up reward plan: role ids to grant and to revoke, for the
/// S4 REST executor to apply. Grants cover every ladder rung at or below the
/// member's level that they do not hold; revokes cover held ladder roles
/// above their level (e.g. after the reward configuration moved). Roles
/// outside the ladder are never touched, and re-planning after applying
/// yields an empty plan (acceptance: reward grants idempotent). With role
/// writes suppressed (session onboarding mode) the plan is empty. Revocation
/// additionally requires `staging_revoke_allowed`; ordinary level-ups only
/// grant. This is a plan, not authorization: executor permission/hierarchy
/// checks still apply to the whole revoke set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewardRolePlan {
    pub grant: Vec<String>,
    pub revoke: Vec<String>,
    pub grant_reason: String,
    pub revoke_reason: Option<String>,
}

/// Most role writes one level-up plan may carry per list. Grants and revokes
/// are capped separately, so a plan may hold up to twice this many writes in
/// total. A ladder larger than this refuses the whole role-write portion
/// instead of fanning out an unbounded number of paced REST calls.
pub const ROLE_WRITE_CAP: usize = 10;

/// A reward plan refused before any role write.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RewardPlanError {
    #[error("level role {list} cap exceeded: {count} planned over the cap of {cap}")]
    CapExceeded {
        /// Which list hit the cap: `grant` or `revoke`.
        list: &'static str,
        count: usize,
        cap: usize,
    },
}

/// Refuse a plan list longer than [`ROLE_WRITE_CAP`].
fn check_role_write_cap(list: &'static str, entries: &[String]) -> Result<(), RewardPlanError> {
    if entries.len() > ROLE_WRITE_CAP {
        return Err(RewardPlanError::CapExceeded {
            list,
            count: entries.len(),
            cap: ROLE_WRITE_CAP,
        });
    }
    Ok(())
}

/// Plan the level-up role writes. Each list is capped at [`ROLE_WRITE_CAP`]:
/// a plan over the cap refuses with [`RewardPlanError::CapExceeded`] and the
/// caller applies no role write. Suppressed role writes plan nothing and never
/// refuse.
pub fn plan_reward_roles(
    level: u64,
    rewards: &[LevelRoleReward],
    member_role_ids: &[String],
    role_writes: bool,
    staging_revoke_allowed: bool,
) -> Result<RewardRolePlan, RewardPlanError> {
    let empty = RewardRolePlan {
        grant: Vec::new(),
        revoke: Vec::new(),
        grant_reason: grant_audit_reason(level),
        revoke_reason: None,
    };
    if !role_writes {
        return Ok(empty);
    }
    let held: std::collections::HashSet<&str> =
        member_role_ids.iter().map(String::as_str).collect();
    let mut grant: Vec<String> = Vec::new();
    let mut ladder: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for reward in rewards {
        ladder.insert(reward.role_id.as_str());
        if reward.level <= level && !held.contains(reward.role_id.as_str()) {
            grant.push(reward.role_id.clone());
        }
    }
    let mut revoke: Vec<String> = member_role_ids
        .iter()
        .filter(|id| {
            let id = id.as_str();
            staging_revoke_allowed
                && ladder.contains(id)
                && !rewards.iter().any(|r| r.role_id == id && r.level <= level)
        })
        .cloned()
        .collect();
    grant.sort();
    grant.dedup();
    revoke.sort();
    revoke.dedup();
    check_role_write_cap("grant", &grant)?;
    check_role_write_cap("revoke", &revoke)?;
    let revoke_reason = if revoke.is_empty() {
        None
    } else {
        Some(revoke_audit_reason(level))
    };
    Ok(RewardRolePlan {
        grant,
        revoke,
        grant_reason: grant_audit_reason(level),
        revoke_reason,
    })
}

/// Validate a custom-command name (legacy `NAME_PATTERN = /^[a-z0-9_-]{1,32}$/`).
#[must_use]
pub fn valid_command_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Validate a `!trigger` form (legacy `TRIGGER_PATTERN = /^![a-z0-9_-]{1,32}$/`).
#[must_use]
pub fn valid_text_trigger(trigger: &str) -> bool {
    trigger.len() >= 2
        && trigger.len() <= 33
        && trigger.starts_with('!')
        && valid_command_name(&trigger[1..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curve_matches_legacy_spot_values() {
        // floor((5/6)·L·(2L²+27L+91)): L=0→0, L=1→100, L=2→255, L=5→1150.
        assert_eq!(total_xp_for_level(0), 0);
        assert_eq!(total_xp_for_level(1), 100);
        assert_eq!(total_xp_for_level(2), 255);
        assert_eq!(total_xp_for_level(5), 1150);
        assert_eq!(level_for_xp(0), 0);
        assert_eq!(level_for_xp(99), 0);
        assert_eq!(level_for_xp(100), 1);
        assert_eq!(level_for_xp(254), 1);
        assert_eq!(level_for_xp(255), 2);
    }

    #[test]
    fn award_honours_cooldown_and_ceiling() {
        // First award ever (no prior timestamp) grants.
        let first = adjudicate_award(0, MESSAGE_XP, None, MAX_STORED_XP);
        assert_eq!(first.awarded, MESSAGE_XP);
        assert!(!first.leveled_up);
        // 59s later the same source is still cooling down.
        let cooled = adjudicate_award(first.total_xp, MESSAGE_XP, Some(59), MAX_STORED_XP);
        assert_eq!(cooled.awarded, 0);
        assert_eq!(cooled.total_xp, first.total_xp);
        // At exactly 60s it fires again (legacy `<=` boundary).
        let fired = adjudicate_award(first.total_xp, MESSAGE_XP, Some(60), MAX_STORED_XP);
        assert_eq!(fired.awarded, MESSAGE_XP);
        // No headroom: ceiling rejection awards nothing.
        let capped = adjudicate_award(MAX_STORED_XP, MESSAGE_XP, Some(3600), 0);
        assert_eq!(capped.awarded, 0);
        assert!(!capped.leveled_up);
    }

    #[test]
    fn level_up_detected_on_threshold_cross() {
        // 99 XP + 15 → 114 crosses the L1 floor at 100.
        let award = adjudicate_award(99, MESSAGE_XP, Some(3600), MAX_STORED_XP);
        assert_eq!(
            (award.total_xp, award.level, award.previous_level),
            (114, 1, 0)
        );
        assert!(award.leveled_up);
    }

    #[test]
    fn voice_credit_floors_to_whole_minutes() {
        assert_eq!(voice_minutes(59), 0);
        assert_eq!(voice_minutes(60), 1);
        assert_eq!(voice_xp_for_duration(300), 25);
    }

    #[test]
    fn rank_text_matches_legacy_shape() {
        let text = rank_text("Test", 1, Some(3), 50, 114);
        assert!(text.starts_with("**Test**\nLevel **1** · Rank **#3** of **50**"));
        assert!(text.contains("XP **114** · 14/155 this level · **141** to level 2"));
    }

    fn assert_rank_name_slot(name: &str, expected_slot: &str) {
        let profile = LevelProfile {
            guild_id: "g".to_owned(),
            member_id: "100000000000000001".to_owned(),
            xp: 114,
            level: 1,
            message_xp: 114,
            voice_xp: 0,
            imported_xp: 0,
            rank: Some(3),
            member_count: 50,
            next_level_xp: total_xp_for_level(2),
        };
        let expected = format!(
            "**{expected_slot}**\nLevel **1** · Rank **#3** of **50**\nXP **114** · 14/155 this level · **141** to level 2"
        );
        assert_eq!(rank_text(name, 1, Some(3), 50, 114), expected);
        let reply = rank_reply(&profile, name);
        assert!(reply.ephemeral);
        assert_eq!(reply.content, expected);
        let rendered_slot = reply
            .content
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("**"))
            .and_then(|line| line.strip_suffix("**"))
            .expect("rank name inside bold markers");
        assert!(rendered_slot.chars().count() <= RANK_DISPLAY_NAME_LIMIT);
        for pill in ["<@", "<#", "<:", "<a:", "<t:", "</", "<id:"] {
            assert!(!rendered_slot.contains(pill));
        }
        let mut escaped = false;
        for ch in rendered_slot.chars() {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else {
                assert!(!matches!(ch, '`' | '*' | '_' | '~' | '|' | '[' | ']'));
            }
        }
        assert!(
            !escaped,
            "a lone escape must not eat the closing bold marker"
        );
    }

    #[test]
    fn rank_name_splits_timestamp_tokens_exactly() {
        for suffix in ["", ":t", ":T", ":d", ":D", ":f", ":F", ":R"] {
            assert_rank_name_slot(
                &format!("before <t:1618953630{suffix}> after"),
                &format!("before <\u{200b}t:1618953630{suffix}> after"),
            );
        }
        assert_rank_name_slot(
            "<t:0> <t:1618953630:R>",
            "<\u{200b}t:0> <\u{200b}t:1618953630:R>",
        );
    }

    #[test]
    fn rank_name_splits_slash_command_and_navigation_tokens_exactly() {
        assert_rank_name_slot(
            "</rank:123456789012345678>",
            "<\u{200b}/rank:123456789012345678>",
        );
        assert_rank_name_slot(
            "</rank stats:123456789012345678> <id:guide> <id:customize> <id:browse> <id:linked-roles>",
            "<\u{200b}/rank stats:123456789012345678> <\u{200b}id:guide> <\u{200b}id:customize> <\u{200b}id:browse> <\u{200b}id:linked-roles>",
        );
    }

    #[test]
    fn rank_name_cap_trims_partial_escape_for_every_markdown_character() {
        for prefix in ["a".repeat(511), "界".repeat(511)] {
            for ch in ['\\', '`', '*', '_', '~', '|', '[', ']'] {
                assert_rank_name_slot(&format!("{prefix}{ch}"), &prefix);
            }
        }
    }

    #[test]
    fn rank_name_cap_preserves_complete_trailing_escape_pairs() {
        assert_rank_name_slot(&"\\".repeat(256), &"\\\\".repeat(256));
        assert_rank_name_slot(
            &format!("a{}", "\\".repeat(256)),
            &format!("a{}", "\\\\".repeat(255)),
        );
        assert_rank_name_slot(
            &format!("{}*", "a".repeat(510)),
            &format!("{}\\*", "a".repeat(510)),
        );
    }

    #[test]
    fn curve_round_trips_level_thresholds() {
        // Legacy `MEE6 curve round-trips level thresholds` (unit.leveling).
        for level in 0..=100u64 {
            let threshold = total_xp_for_level(level);
            assert_eq!(level_for_xp(threshold), level);
            if level > 0 {
                assert_eq!(level_for_xp(threshold - 1), level - 1);
            }
        }
    }

    #[test]
    fn grouped_numbers_match_to_locale_string() {
        assert_eq!(grouped_number(0), "0");
        assert_eq!(grouped_number(114), "114");
        assert_eq!(grouped_number(1150), "1,150");
        assert_eq!(grouped_number(1_234_567), "1,234,567");
        assert_eq!(grouped_number(9_876_543), "9,876,543");
        assert_eq!(grouped_number(MAX_STORED_XP), "9,007,199,254,740,991");
    }

    #[test]
    fn rank_text_groups_large_numbers_like_legacy() {
        // Legacy golden (service.ts `rankText` + `toLocaleString`): level 86,
        // rank 7 of 250, 1,234,567 XP (floor 1,233,025, next 1,274,405).
        let text = rank_text("Big Earner", 86, Some(7), 250, 1_234_567);
        assert_eq!(
            text,
            "**Big Earner**\nLevel **86** · Rank **#7** of **250**\nXP **1,234,567** · 1,542/41,380 this level · **39,838** to level 87"
        );
    }

    #[test]
    fn leaderboard_line_groups_large_xp_like_legacy() {
        assert_eq!(
            leaderboard_line(3, 100000000000000003u64, 42, 9_876_543),
            "**3.** <@100000000000000003> · level **42** · 9,876,543 XP"
        );
    }

    #[test]
    fn leaderboard_limit_clamped_to_legacy_bounds() {
        assert_eq!(clamp_leaderboard_limit(0), 1);
        assert_eq!(clamp_leaderboard_limit(10), 10);
        assert_eq!(clamp_leaderboard_limit(25), 25);
        assert_eq!(clamp_leaderboard_limit(100), 25);
        assert_eq!(LEADERBOARD_DEFAULT_LIMIT, 10);
    }

    #[test]
    fn rank_reply_is_ephemeral() {
        let profile = LevelProfile {
            guild_id: "g".to_owned(),
            member_id: "100000000000000001".to_owned(),
            xp: 114,
            level: 1,
            message_xp: 114,
            voice_xp: 0,
            imported_xp: 0,
            rank: Some(3),
            member_count: 50,
            next_level_xp: total_xp_for_level(2),
        };
        let reply = rank_reply(&profile, "Test");
        assert!(reply.ephemeral, "legacy answers /rank ephemerally");
        assert!(reply.content.starts_with("**Test**\nLevel **1**"));
    }

    #[test]
    fn rank_reply_without_xp_row_is_unranked() {
        let profile = LevelProfile {
            guild_id: "g".to_owned(),
            member_id: "100000000000000001".to_owned(),
            xp: 0,
            level: 0,
            message_xp: 0,
            voice_xp: 0,
            imported_xp: 0,
            rank: None,
            member_count: 2,
            next_level_xp: total_xp_for_level(1),
        };
        let reply = rank_reply(&profile, "New Member");
        assert!(reply.ephemeral);
        assert_eq!(
            reply.content,
            "**New Member**\nLevel **0** · Rank **Unranked** (no XP recorded)\nXP **0** · 0/100 this level · **100** to level 1"
        );
    }

    #[test]
    fn rank_text_with_stored_zero_xp_keeps_numeric_rank() {
        assert_eq!(
            rank_text("Zero XP", 0, Some(2), 3, 0),
            "**Zero XP**\nLevel **0** · Rank **#2** of **3**\nXP **0** · 0/100 this level · **100** to level 1"
        );
    }

    #[test]
    fn rank_reply_detailed_labels_sources_without_resumming_divergent_total() {
        // Sources can diverge from the stored total after adjustments; the
        // renderer labels each independently and keeps the stored total
        // authoritative instead of re-summing (137 + 59 + 1038 = 1234 ≠ 1200).
        let profile = LevelProfile {
            guild_id: "g".to_owned(),
            member_id: "100000000000000001".to_owned(),
            xp: 1200,
            level: level_for_xp(1200),
            message_xp: 137,
            voice_xp: 59,
            imported_xp: 1038,
            rank: Some(2),
            member_count: 3,
            next_level_xp: total_xp_for_level(level_for_xp(1200) + 1),
        };
        let reply = rank_reply_detailed(&profile, "Diverged");
        assert!(reply.ephemeral, "details keep /rank private");
        let default = rank_reply(&profile, "Diverged");
        assert!(
            reply.content.starts_with(&default.content),
            "details extend the default card, never replace it"
        );
        // Each label is bound to its own value: a swapped pair (message vs
        // voice) would still mention every number, so assert the adjacent
        // `label **value**` rendering.
        for pair in [
            "Message XP **137**",
            "Voice XP **59**",
            "Imported XP **1,038**",
            "Stored total XP **1,200**",
        ] {
            assert!(
                reply.content.contains(pair),
                "details must bind `{pair}`: {}",
                reply.content
            );
        }
        assert!(
            !reply.content.contains("1,234"),
            "sources are never re-summed: {}",
            reply.content
        );
        assert!(
            reply.content.contains("differ") || reply.content.contains("adjust"),
            "details explain the gap: {}",
            reply.content
        );
    }

    #[test]
    fn leaderboard_reply_lifetime_labels_scope_without_season_or_partial() {
        let entries = vec![LeaderboardEntry {
            member_id: "100000000000000001".to_owned(),
            xp: 1200,
            level: 5,
            rank: 1,
        }];
        let reply = leaderboard_reply_lifetime(&entries);
        assert!(reply.suppress_mentions, "lifetime parses no mentions");
        assert!(
            reply
                .content
                .starts_with("**TWO XP Leaderboard (Lifetime)**\n**1.** <@100000000000000001>"),
            "lifetime header plus rows: {}",
            reply.content
        );
        assert!(!reply.content.contains("2026-Q"), "no season id leaks in");
        assert!(
            !reply.content.to_lowercase().contains("partial"),
            "lifetime is never partial"
        );
        let empty = leaderboard_reply_lifetime(&[]);
        assert_eq!(
            empty.content,
            "**TWO XP Leaderboard (Lifetime)**\nNo XP has been earned yet."
        );
    }

    #[test]
    fn leaderboard_reply_suppresses_mentions() {
        let entries = vec![LeaderboardEntry {
            member_id: "100000000000000001".to_owned(),
            xp: 1200,
            level: 5,
            rank: 1,
        }];
        let reply = leaderboard_reply(&entries);
        assert!(reply.suppress_mentions, "legacy parses no mentions");
        assert!(reply
            .content
            .starts_with("**TWO XP Leaderboard**\n**1.** <@100000000000000001>"));
        let empty = leaderboard_reply(&[]);
        assert_eq!(empty.content, "No XP has been earned yet.");
        assert!(empty.suppress_mentions);
    }

    #[test]
    fn reward_config_rejects_bad_rows_and_dedupes_levels() {
        assert_eq!(
            normalize_role_rewards(&[LevelRoleReward {
                level: 0,
                role_id: "400000000000000005".to_owned()
            }]),
            Err(RewardConfigError::InvalidLevel)
        );
        assert_eq!(
            normalize_role_rewards(&[LevelRoleReward {
                level: 5,
                role_id: "not-an-id".to_owned()
            }]),
            Err(RewardConfigError::InvalidRoleId("not-an-id".to_owned()))
        );
        // Last row per level wins, ascending by level (legacy Map.set).
        let normalized = normalize_role_rewards(&[
            LevelRoleReward {
                level: 10,
                role_id: "400000000000000010".to_owned(),
            },
            LevelRoleReward {
                level: 5,
                role_id: "400000000000000005".to_owned(),
            },
            LevelRoleReward {
                level: 5,
                role_id: "400000000000000006".to_owned(),
            },
        ])
        .expect("normalizes");
        assert_eq!(
            normalized,
            vec![
                LevelRoleReward {
                    level: 5,
                    role_id: "400000000000000006".to_owned()
                },
                LevelRoleReward {
                    level: 10,
                    role_id: "400000000000000010".to_owned()
                },
            ]
        );
    }

    #[test]
    fn reward_plan_refuses_a_list_over_the_cap_with_a_fixed_error() {
        let ladder: Vec<LevelRoleReward> = (1..=ROLE_WRITE_CAP as u64 + 1)
            .map(|level| LevelRoleReward {
                level,
                role_id: format!("4000000000000000{level:02}"),
            })
            .collect();
        let level = ROLE_WRITE_CAP as u64 + 1;
        let error = plan_reward_roles(level, &ladder, &[], true, false).unwrap_err();
        assert_eq!(
            error,
            RewardPlanError::CapExceeded {
                list: "grant",
                count: ROLE_WRITE_CAP + 1,
                cap: ROLE_WRITE_CAP
            }
        );
        assert_eq!(
            error.to_string(),
            "level role grant cap exceeded: 11 planned over the cap of 10"
        );
        let held: Vec<String> = ladder.iter().map(|r| r.role_id.clone()).collect();
        let error = plan_reward_roles(0, &ladder, &held, true, true).unwrap_err();
        assert!(error
            .to_string()
            .starts_with("level role revoke cap exceeded"));
        // Suppressed role writes never refuse, even over the cap.
        assert!(plan_reward_roles(level, &ladder, &[], false, true)
            .unwrap()
            .grant
            .is_empty());
    }

    #[test]
    fn level_role_writes_follow_onboarding_mode() {
        assert!(
            level_role_writes_allowed(false),
            "legacy mode applies rewards"
        );
        assert!(
            !level_role_writes_allowed(true),
            "session mode suppresses role writes"
        );
    }

    #[test]
    fn reward_plan_grants_earned_and_revokes_unqualified() {
        let rewards = vec![
            LevelRoleReward {
                level: 5,
                role_id: "400000000000000005".to_owned(),
            },
            LevelRoleReward {
                level: 10,
                role_id: "400000000000000010".to_owned(),
            },
        ];
        // Level 10 holding nothing: grants both rungs.
        let plan = plan_reward_roles(10, &rewards, &[], true, false).expect("under cap");
        assert_eq!(
            plan.grant,
            vec![
                "400000000000000005".to_owned(),
                "400000000000000010".to_owned()
            ]
        );
        assert!(plan.revoke.is_empty());
        assert_eq!(plan.grant_reason, "TWO leveling: reached level 10");
        assert!(plan.revoke_reason.is_none());
        // Re-planning after applying is empty (idempotent grants).
        let replan = plan_reward_roles(10, &rewards, &plan.grant, true, false).expect("under cap");
        assert!(replan.grant.is_empty() && replan.revoke.is_empty());
        // Level 5 holding the level-10 role: keeps 5, revokes 10, grants
        // nothing; non-ladder roles are never touched.
        let plan = plan_reward_roles(
            5,
            &rewards,
            &[
                "400000000000000005".to_owned(),
                "400000000000000010".to_owned(),
                "999999999999999999".to_owned(),
            ],
            true,
            true,
        )
        .expect("under cap");
        assert!(plan.grant.is_empty());
        assert_eq!(plan.revoke, vec!["400000000000000010".to_owned()]);
        assert_eq!(
            plan.revoke_reason.as_deref(),
            Some("TWO leveling: below level thresholds at level 5")
        );
        // Session mode: no writes at all.
        let plan = plan_reward_roles(10, &rewards, &[], false, true).expect("suppressed");
        assert!(plan.grant.is_empty() && plan.revoke.is_empty());
    }

    #[test]
    fn leaderboard_falls_back_when_empty() {
        assert_eq!(leaderboard_text(&[]), "No XP has been earned yet.");
        let text = leaderboard_text(&[leaderboard_line(1, 20, 5, 1200)]);
        assert!(text.starts_with("**TWO XP Leaderboard**\n**1.** <@20>"));
    }

    #[test]
    fn seasonal_board_marks_first_partial_quarter_in_header() {
        let entries = [LeaderboardEntry {
            member_id: "100000000000000001".to_owned(),
            xp: 1200,
            level: 5,
            rank: 1,
        }];
        let partial = leaderboard_reply_for_season(&entries, FIRST_PARTIAL_SEASON);
        assert!(
            partial.content.starts_with(
                "**TWO XP Leaderboard (2026-Q4 — partial: early-season XP not counted)**\n"
            ),
            "2026-Q4 members must see the partial gap in the board header"
        );
        assert_eq!(
            partial.content.lines().count(),
            2,
            "partial marker stays inside the header line, not an extra row"
        );
        assert!(partial.suppress_mentions);
        let empty_partial = leaderboard_reply_for_season(&[], FIRST_PARTIAL_SEASON);
        assert!(
            empty_partial
                .content
                .contains("partial: early-season XP not counted")
                && empty_partial.content.contains("No XP has been earned yet."),
            "empty first quarter stays honest and marked, never lifetime"
        );
        let later = leaderboard_reply_for_season(&entries, "2027-Q1");
        assert!(
            later
                .content
                .starts_with("**TWO XP Leaderboard (2027-Q1)**\n"),
            "later quarters render the plain header once tracking is whole"
        );
    }

    #[test]
    fn ordinary_level_ups_never_plan_staging_revocations() {
        let rewards = [LevelRoleReward {
            level: 10,
            role_id: "400000000000000010".to_owned(),
        }];
        let held = ["400000000000000010".to_owned()];
        let plan = plan_reward_roles(5, &rewards, &held, true, false).expect("under cap");
        assert!(plan.grant.is_empty() && plan.revoke.is_empty());
        assert!(plan.revoke_reason.is_none());
    }

    #[test]
    fn leaderboard_preserves_stored_member_ids() {
        let entries = [LeaderboardEntry {
            member_id: "99999999999999999999".to_owned(),
            xp: 15,
            level: 0,
            rank: 1,
        }];
        assert!(leaderboard_reply(&entries)
            .content
            .contains("<@99999999999999999999>"));
    }

    #[test]
    fn reward_levels_fit_legacy_integer_column() {
        assert_eq!(
            normalize_role_rewards(&[LevelRoleReward {
                level: u64::MAX,
                role_id: "400000000000000010".to_owned(),
            }]),
            Err(RewardConfigError::InvalidLevel)
        );
    }

    #[test]
    fn name_and_trigger_patterns_match_legacy() {
        assert!(valid_command_name("faq"));
        assert!(valid_command_name("a-b_c9"));
        assert!(!valid_command_name(""));
        assert!(!valid_command_name("FAQ"));
        assert!(!valid_command_name("has space"));
        assert!(!valid_command_name(&"x".repeat(33)));
        assert!(valid_text_trigger("!faq"));
        assert!(!valid_text_trigger("faq"));
        assert!(!valid_text_trigger("!"));
        assert!(!valid_text_trigger("!HAS-SPACE"));
    }

    #[test]
    fn decay_weeks_apply_only_newly_overdue_weeks() {
        // Grace: 28 d or less never decays, whatever was applied before.
        assert_eq!(seasonal_decay_total_weeks(0), 0);
        assert_eq!(seasonal_decay_total_weeks(28), 0);
        assert_eq!(seasonal_decay_total_weeks(34), 0);
        assert_eq!(seasonal_decay_total_weeks(35), 1);
        assert_eq!(seasonal_decay_total_weeks(42), 2);
        // A rerun of the same tick applies nothing; consecutive Monday ticks
        // apply exactly one week each; a missed Monday applies two at once.
        assert_eq!(seasonal_decay_new_weeks(35, 1), 0);
        assert_eq!(seasonal_decay_new_weeks(42, 1), 1);
        assert_eq!(seasonal_decay_new_weeks(49, 2), 1);
        assert_eq!(seasonal_decay_new_weeks(49, 1), 2);
        assert_eq!(seasonal_decay_new_weeks(7, 3), 0);
        // Weekly factors compose: two single-week ticks equal one double tick.
        assert_eq!(
            seasonal_xp_after_decay_weeks(seasonal_xp_after_decay_weeks(1000, 1), 1),
            seasonal_xp_after_decay_weeks(1000, 2)
        );
        assert_eq!(seasonal_xp_after_decay_weeks(1000, 1), 900);
        assert_eq!(seasonal_xp_after_decay(1000, 35), 900);
        assert_eq!(seasonal_xp_after_decay(1000, 42), 810);
    }

    #[test]
    fn season_ids_start_on_the_monday_on_or_after_each_quarter() {
        // 2026-10-01 is a Thursday; Q4 starts the following Monday 2026-10-05.
        let day = |y, m, d| days_from_civil(y, m, d) * 86_400_000;
        assert_eq!(season_id_for_ms(day(2026, 10, 1)), "2026-Q3");
        assert_eq!(season_id_for_ms(day(2026, 10, 4)), "2026-Q3");
        assert_eq!(season_id_for_ms(day(2026, 10, 5)), "2026-Q4");
        assert_eq!(season_id_for_ms(day(2026, 8, 15)), "2026-Q3");
        assert_eq!(season_id_for_ms(day(2026, 8, 31)), "2026-Q3");
        // 2026-01-01 is a Thursday; the first days of January still belong to
        // 2025-Q4, and Q1 starts Monday 2026-01-05.
        assert_eq!(season_id_for_ms(day(2026, 1, 1)), "2025-Q4");
        assert_eq!(season_id_for_ms(day(2026, 1, 4)), "2025-Q4");
        assert_eq!(season_id_for_ms(day(2026, 1, 5)), "2026-Q1");
        // 2026-07-01 is a Wednesday; Q3 starts Monday 2026-07-06.
        assert_eq!(season_id_for_ms(day(2026, 7, 5)), "2026-Q2");
        assert_eq!(season_id_for_ms(day(2026, 7, 6)), "2026-Q3");
        // Mid-quarter days keep their calendar quarter.
        assert_eq!(season_id_for_ms(day(2026, 11, 20)), "2026-Q4");
        assert_eq!(season_id_for_ms(day(2026, 2, 14)), "2026-Q1");
    }
}
