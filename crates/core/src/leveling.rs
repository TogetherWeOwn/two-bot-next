//! Leveling domain: XP curves, award adjudication, and reply text.
//!
//! Ports the DB-free core of `src/leveling/service.ts` + `src/leveling/discord.ts`
//! from two-bot. The curve is MEE6-compatible (`totalXpForLevel`) so imported
//! XP (TOG-9882) and organic XP share one ladder. Storage stays behind the
//! caller: this module adjudicates awards and formats replies; sqlx rows land
//! with S6 (TOG-9811), which calls into these pure functions.

/// XP per qualifying message (legacy `MESSAGE_XP`).
pub const MESSAGE_XP: u64 = 15;
/// Per-source award cooldown (legacy `MESSAGE_COOLDOWN_SECONDS` /
/// `VOICE_COOLDOWN_SECONDS`, both 60s).
pub const AWARD_COOLDOWN_SECONDS: u64 = 60;
/// Voice XP per whole minute (legacy `VOICE_XP_PER_MINUTE`, MEE6-compat 5/min).
pub const VOICE_XP_PER_MINUTE: u64 = 5;
/// Storage ceiling (legacy `MAX_STORED_XP` = `Number.MAX_SAFE_INTEGER`).
pub const MAX_STORED_XP: u64 = 9_007_199_254_740_991;

/// XP source channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XpSource {
    Message,
    Voice,
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

/// Whole minutes of voice credit for a session duration (legacy floors after
/// clamping negatives to zero; here the input is already `u64` seconds).
#[must_use]
pub fn voice_minutes(duration_seconds: u64) -> u64 {
    duration_seconds / 60
}

/// Voice XP for a session duration.
#[must_use]
pub fn voice_xp_for_duration(duration_seconds: u64) -> u64 {
    voice_minutes(duration_seconds) * VOICE_XP_PER_MINUTE
}

/// `/rank` reply body (legacy `rankText`).
#[must_use]
pub fn rank_text(display_name: &str, level: u64, rank: u64, member_count: u64, xp: u64) -> String {
    let floor = total_xp_for_level(level);
    let next_floor = total_xp_for_level(level + 1);
    let progress = xp - floor;
    let span = next_floor - floor;
    let to_next = next_floor - xp;
    format!(
        "**{display_name}**\nLevel **{level}** · Rank **#{rank}** of **{member_count}**\nXP **{xp}** · {progress}/{span} this level · **{to_next}** to level {}",
        level + 1
    )
}

/// One `/leaderboard` row (legacy mapping, top 10, mentions without parsing).
#[must_use]
pub fn leaderboard_line(rank: u64, member_id: u64, level: u64, xp: u64) -> String {
    format!("**{rank}.** <@{member_id}> · level **{level}** · {xp} XP")
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
        let text = rank_text("Test", 1, 3, 50, 114);
        assert!(text.starts_with("**Test**\nLevel **1** · Rank **#3** of **50**"));
        assert!(text.contains("XP **114** · 14/155 this level · **141** to level 2"));
    }

    #[test]
    fn leaderboard_falls_back_when_empty() {
        assert_eq!(leaderboard_text(&[]), "No XP has been earned yet.");
        let text = leaderboard_text(&[leaderboard_line(1, 20, 5, 1200)]);
        assert!(text.starts_with("**TWO XP Leaderboard**\n**1.** <@20>"));
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
}
