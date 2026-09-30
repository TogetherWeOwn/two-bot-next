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
//! Parity notes: legacy posts no channel message on level-up — `onLevelUp`
//! only grants reward roles (`applyLevelRoles`). The port matches that: a
//! level-up produces a [`RewardRolePlan`] (role grant/revoke plan + audit
//! reasons) for the executor, not a message. Legacy revokes nothing, but the
//! staging-only TOG-4444 apply path proved grant/readback/revoke, and this
//! card explicitly scopes grant/revoke — so the plan revokes held ladder
//! roles the member no longer qualifies for, and never touches other roles.

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

/// Format a count the way legacy `Number.toLocaleString()` (en-US) does:
/// thousands grouped with `,`, no decimals. Legacy applies it to every number
/// in the rank/leaderboard replies, so the port must too (acceptance: replies
/// match legacy text).
#[must_use]
pub fn grouped_number(n: u64) -> String {
    let digits = n.to_string().into_bytes();
    let mut out = Vec::with_capacity(digits.len() + digits.len() / 3);
    for (i, b) in digits.iter().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(b',');
        }
        out.push(*b);
    }
    String::from_utf8(out).expect("grouped digits are ASCII")
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
        level + 1,
        rank = grouped_number(rank),
        member_count = grouped_number(member_count),
        xp = grouped_number(xp),
        progress = grouped_number(progress),
        span = grouped_number(span),
        to_next = grouped_number(to_next),
    )
}

/// One `/leaderboard` row (legacy mapping, top 10, mentions without parsing).
#[must_use]
pub fn leaderboard_line(rank: u64, member_id: u64, level: u64, xp: u64) -> String {
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
    /// 1-based rank with XP ties broken by member id ascending (legacy
    /// `COUNT(...) + 1` over strictly-greater rows, so a member with no row
    /// ranks below every member holding XP).
    pub rank: u64,
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
        .map(|e| {
            let member_id: u64 = e.member_id.parse().unwrap_or(0);
            leaderboard_line(e.rank, member_id, e.level, e.xp)
        })
        .collect();
    LeaderboardReply {
        content: leaderboard_text(&rows),
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

/// Audit reason for revoking ladder roles the member no longer qualifies for.
/// New string — legacy never revokes — namespaced the same way so the audit
/// trail reads as one leveling actor.
#[must_use]
pub fn revoke_audit_reason(level: u64) -> String {
    format!("TWO leveling: removed unqualified reward roles at level {level}")
}

/// Idempotent level-up reward plan: role ids to grant and to revoke, for the
/// S4 REST executor to apply. Grants cover every ladder rung at or below the
/// member's level that they do not hold; revokes cover held ladder roles
/// above their level (e.g. after the reward configuration moved). Roles
/// outside the ladder are never touched, and re-planning after applying
/// yields an empty plan (acceptance: reward grants idempotent). With role
/// writes suppressed (session onboarding mode) the plan is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewardRolePlan {
    pub grant: Vec<String>,
    pub revoke: Vec<String>,
    pub grant_reason: String,
    pub revoke_reason: Option<String>,
}

#[must_use]
pub fn plan_reward_roles(
    level: u64,
    rewards: &[LevelRoleReward],
    member_role_ids: &[String],
    role_writes: bool,
) -> RewardRolePlan {
    let empty = RewardRolePlan {
        grant: Vec::new(),
        revoke: Vec::new(),
        grant_reason: grant_audit_reason(level),
        revoke_reason: None,
    };
    if !role_writes {
        return empty;
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
            ladder.contains(id) && !rewards.iter().any(|r| r.role_id == id && r.level <= level)
        })
        .cloned()
        .collect();
    grant.sort();
    grant.dedup();
    revoke.sort();
    revoke.dedup();
    let revoke_reason = if revoke.is_empty() {
        None
    } else {
        Some(revoke_audit_reason(level))
    };
    RewardRolePlan {
        grant,
        revoke,
        grant_reason: grant_audit_reason(level),
        revoke_reason,
    }
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
        let text = rank_text("Big Earner", 86, 7, 250, 1_234_567);
        assert_eq!(
            text,
            "**Big Earner**\nLevel **86** · Rank **#7** of **250**\nXP **1,234,567** · 1,542/41,380 this level · **39,838** to level 87"
        );
    }

    #[test]
    fn leaderboard_line_groups_large_xp_like_legacy() {
        assert_eq!(
            leaderboard_line(3, 100000000000000003, 42, 9_876_543),
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
            rank: 3,
            member_count: 50,
            next_level_xp: total_xp_for_level(2),
        };
        let reply = rank_reply(&profile, "Test");
        assert!(reply.ephemeral, "legacy answers /rank ephemerally");
        assert!(reply.content.starts_with("**Test**\nLevel **1**"));
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
        let plan = plan_reward_roles(10, &rewards, &[], true);
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
        let replan = plan_reward_roles(10, &rewards, &plan.grant, true);
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
        );
        assert!(plan.grant.is_empty());
        assert_eq!(plan.revoke, vec!["400000000000000010".to_owned()]);
        assert_eq!(
            plan.revoke_reason.as_deref(),
            Some("TWO leveling: removed unqualified reward roles at level 5")
        );
        // Session mode: no writes at all.
        let plan = plan_reward_roles(10, &rewards, &[], false);
        assert!(plan.grant.is_empty() && plan.revoke.is_empty());
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
