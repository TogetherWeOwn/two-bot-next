//! Join-burst detection and join-risk evidence, without runtime activation.
//!
//! Frozen legacy source: `two-bot@d5d1179348feb9157bcac8c875de9399d4f5c76a`,
//! `analytics/raidWatch.ts` and `moderation/{containment,containmentStore}.ts`.
//! Raid state is deliberately volatile; join-risk counts and persistence claims
//! must come from a serialized durable store. Neither path mutates members.
//! See `docs/raid-port.md` for clocks, gates and the unimplemented runtime seams.

use std::collections::HashMap;

use crate::funnel::format_iso_millis;
use crate::onboarding::MentionPolicy;

pub const DEFAULT_RAID_WINDOW_SECONDS: f64 = 60.0;
pub const DEFAULT_RAID_THRESHOLD: f64 = 5.0;
pub const DEFAULT_RAID_COOLDOWN_SECONDS: f64 = 900.0;
pub const DEFAULT_RAID_MAX_IDS: usize = 50;
pub const DEFAULT_JOIN_RISK_WINDOW_SECONDS: f64 = 60.0;
pub const DEFAULT_JOIN_RISK_THRESHOLD: f64 = 5.0;
const DAY_MS: i128 = 86_400_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RaidConfigError {
    #[error("join window must be finite and positive")]
    Window,
    #[error("join threshold must be finite and positive")]
    Threshold,
    #[error("raid cooldown must be finite and nonnegative")]
    Cooldown,
}

/// One live-settings snapshot, supplied once per observation. Fractional
/// thresholds/windows are supported, as in legacy numeric readers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RaidTuning {
    window_seconds: f64,
    threshold: f64,
}

impl Default for RaidTuning {
    fn default() -> Self {
        Self {
            window_seconds: DEFAULT_RAID_WINDOW_SECONDS,
            threshold: DEFAULT_RAID_THRESHOLD,
        }
    }
}

impl RaidTuning {
    pub fn new(window_seconds: f64, threshold: f64) -> Result<Self, RaidConfigError> {
        if !window_seconds.is_finite() || window_seconds <= 0.0 {
            return Err(RaidConfigError::Window);
        }
        if !threshold.is_finite() || threshold <= 0.0 {
            return Err(RaidConfigError::Threshold);
        }
        Ok(Self {
            window_seconds,
            threshold,
        })
    }

    #[must_use]
    pub fn window_seconds(self) -> f64 {
        self.window_seconds
    }

    #[must_use]
    pub fn threshold(self) -> f64 {
        self.threshold
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RaidAlert {
    pub guild_id: String,
    pub count: usize,
    pub window_seconds: f64,
    pub first_join_at: String,
    pub last_join_at: String,
    pub span_seconds: u64,
    pub member_ids: Vec<String>,
    pub truncated: bool,
    pub repeat: bool,
}

/// A staff/log message proposal, not permission to post. The shared executor
/// must preserve `MentionPolicy::None` (empty parse AND no explicit recipients).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaffAlertMessage {
    pub content: String,
    pub mentions: MentionPolicy,
}

impl RaidAlert {
    #[must_use]
    pub fn staff_message(&self) -> StaffAlertMessage {
        let when = if self.span_seconds <= 1 {
            "within a second".to_owned()
        } else {
            format!("in {}s", self.span_seconds)
        };
        // Legacy prints the default threshold, not the live threshold, and
        // "more joins" is the current window count rather than a delta.
        let head = if self.repeat {
            format!(
                "**Join burst still going** - {} more joins {when}.",
                self.count
            )
        } else {
            format!(
                "**Join burst** - {} accounts joined {when} (threshold: 5 in {}s).",
                self.count, self.window_seconds
            )
        };
        let ids = self
            .member_ids
            .iter()
            .map(|id| format!("`{id}`"))
            .collect::<Vec<_>>()
            .join(" ");
        let more = if self.truncated {
            format!(" ...and {} more", self.count - self.member_ids.len())
        } else {
            String::new()
        };
        StaffAlertMessage {
            content: [
                head,
                format!("First {}, last {}.", self.first_join_at, self.last_join_at),
                String::new(),
                format!("IDs: {ids}{more}"),
                String::new(),
                "This is an alert only - the bot has kicked, banned and messaged nobody.".into(),
                // The legacy Node roster script is not part of the Rust port.
                "Next: 1. Review join-attribution evidence to see which invite sent them.".into(),
                "2. If it is a raid, Server Settings -> Safety Setup -> pause invites.".into(),
            ]
            .join("\n"),
            mentions: MentionPolicy::None,
        }
    }
}

#[derive(Debug, Clone)]
struct RecentJoin {
    member_id: String,
    at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct RaidWatch {
    cooldown_ms: f64,
    max_ids: usize,
    recent: HashMap<String, Vec<RecentJoin>>,
    last_alert_at: HashMap<String, i64>,
}

impl Default for RaidWatch {
    fn default() -> Self {
        Self::new(DEFAULT_RAID_COOLDOWN_SECONDS, DEFAULT_RAID_MAX_IDS)
            .expect("default raid configuration is valid")
    }
}

impl RaidWatch {
    pub fn new(cooldown_seconds: f64, max_ids: usize) -> Result<Self, RaidConfigError> {
        if !cooldown_seconds.is_finite() || cooldown_seconds < 0.0 {
            return Err(RaidConfigError::Cooldown);
        }
        Ok(Self {
            cooldown_ms: cooldown_seconds * 1000.0,
            max_ids,
            recent: HashMap::new(),
            last_alert_at: HashMap::new(),
        })
    }

    /// The adapter supplies non-bot joins and valid epoch milliseconds. Pruning
    /// uses the newest occurrence, not the processing clock. Returning an alert
    /// consumes its cooldown, even if delivery later fails or is disabled.
    pub fn observe(
        &mut self,
        guild_id: &str,
        member_id: &str,
        at_ms: i64,
        tuning: RaidTuning,
    ) -> Option<RaidAlert> {
        let list = self.recent.entry(guild_id.to_owned()).or_default();
        // This check intentionally precedes pruning: a retained member cannot
        // advance the window by rejoining with a later timestamp.
        if list.iter().any(|row| row.member_id == member_id) {
            return None;
        }
        list.push(RecentJoin {
            member_id: member_id.to_owned(),
            at_ms,
        });
        list.sort_by_key(|row| row.at_ms);
        let newest = list.last().expect("the new join exists").at_ms;
        let cutoff = newest as f64 - tuning.window_seconds * 1000.0;
        list.retain(|row| row.at_ms as f64 > cutoff);
        // Tiny fractional windows may round the cutoff up to newest.
        if list.is_empty() || (list.len() as f64) < tuning.threshold {
            return None;
        }
        let last = self.last_alert_at.get(guild_id).copied();
        if last
            .is_some_and(|last| ((i128::from(newest) - i128::from(last)) as f64) < self.cooldown_ms)
        {
            return None;
        }
        self.last_alert_at.insert(guild_id.to_owned(), newest);
        let first = list[0].at_ms;
        Some(RaidAlert {
            guild_id: guild_id.to_owned(),
            count: list.len(),
            window_seconds: tuning.window_seconds,
            first_join_at: format_iso_millis(first),
            last_join_at: format_iso_millis(newest),
            span_seconds: ((i128::from(newest) - i128::from(first) + 500) / 1000) as u64,
            member_ids: list
                .iter()
                .take(self.max_ids)
                .map(|row| row.member_id.clone())
                .collect(),
            truncated: list.len() > self.max_ids,
            repeat: last.is_some(),
        })
    }

    #[must_use]
    pub fn window_size(&self, guild_id: &str) -> usize {
        self.recent.get(guild_id).map_or(0, Vec::len)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinRiskInput {
    pub guild_id: String,
    pub member_id: String,
    pub member_is_bot: bool,
    pub account_created_at_ms: i64,
    pub joined_at_ms: Option<i64>,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JoinRiskPolicy {
    guild_id: String,
    tuning: RaidTuning,
    bulk_join_window_until_ms: Option<i64>,
}

impl JoinRiskPolicy {
    pub fn new(
        guild_id: String,
        window_seconds: f64,
        join_threshold: f64,
        bulk_join_window_until_ms: Option<i64>,
    ) -> Result<Self, RaidConfigError> {
        Ok(Self {
            guild_id,
            tuning: RaidTuning::new(window_seconds, join_threshold)?,
            bulk_join_window_until_ms,
        })
    }

    /// No executor trusted/protected list applies to joining members. Legacy
    /// neither rejects stale/future joins nor clamps negative account ages.
    #[must_use]
    pub fn prepare(
        &self,
        input: &JoinRiskInput,
        fallback_now_ms: i64,
    ) -> Option<JoinRiskObservation> {
        if input.member_is_bot || input.guild_id != self.guild_id {
            return None;
        }
        let joined_at_ms = input.joined_at_ms.unwrap_or(fallback_now_ms);
        let age = i128::from(joined_at_ms) - i128::from(input.account_created_at_ms);
        let (account_score, account_reasons) = if age < DAY_MS {
            (3, vec!["account younger than 24 hours".to_owned()])
        } else if age < 7 * DAY_MS {
            (1, vec!["account younger than 7 days".to_owned()])
        } else {
            (0, vec![])
        };
        let joined_at = format_iso_millis(joined_at_ms);
        Some(JoinRiskObservation {
            event_id: format!("{}:{}:{joined_at}", input.guild_id, input.member_id),
            guild_id: input.guild_id.clone(),
            member_id: input.member_id.clone(),
            account_created_at: format_iso_millis(input.account_created_at_ms),
            joined_at,
            source: input.source.clone(),
            account_score,
            account_reasons,
            bulk_join_window: self
                .bulk_join_window_until_ms
                .is_some_and(|until| until >= joined_at_ms),
            tuning: self.tuning,
        })
    }
}

/// Proposal for the store's guild-serialized transaction: check `event_id`,
/// count prior rows, score and insert. A proposal is not a durable claim.
#[derive(Debug, Clone, PartialEq)]
pub struct JoinRiskObservation {
    pub event_id: String,
    pub guild_id: String,
    pub member_id: String,
    pub account_created_at: String,
    pub joined_at: String,
    pub source: String,
    pub account_score: u8,
    pub account_reasons: Vec<String>,
    pub bulk_join_window: bool,
    tuning: RaidTuning,
}

impl JoinRiskObservation {
    #[must_use]
    pub fn window_seconds(&self) -> f64 {
        self.tuning.window_seconds
    }

    /// `prior_join_count` includes unflagged and bulk-suppressed observations
    /// inside (processing-now - window, processing-now], never occurrence time.
    /// The current observation is not in that count; it contributes one here.
    #[must_use]
    pub fn score(self, prior_join_count: u64) -> JoinRiskEvidence {
        let join_count = prior_join_count.saturating_add(1);
        let mut score = self.account_score;
        let mut reasons = self.account_reasons.clone();
        if join_count as f64 >= self.tuning.threshold {
            score += 2;
            reasons.push(format!(
                "{join_count} joins inside {}s",
                self.tuning.window_seconds
            ));
        }
        let flagged = !self.bulk_join_window && score >= 3;
        JoinRiskEvidence {
            observation: self,
            join_count,
            score,
            reasons,
            flagged,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct JoinRiskEvidence {
    pub observation: JoinRiskObservation,
    pub join_count: u64,
    pub score: u8,
    pub reasons: Vec<String>,
    pub flagged: bool,
}

impl JoinRiskEvidence {
    /// Call only with the actual insert/duplicate disposition from the store.
    /// A failed send must not turn a duplicate replay into a second alert.
    #[must_use]
    pub fn staff_message(&self, persisted: bool) -> Option<StaffAlertMessage> {
        if !persisted || !self.flagged {
            return None;
        }
        let reasons = if self.reasons.is_empty() {
            "none".to_owned()
        } else {
            self.reasons.join("; ")
        };
        Some(StaffAlertMessage {
            content: format!(
                "**Join risk flag** — score {}.\nMember: `{}` · reasons: {}.\nFlag only: Owen did not kick, ban, timeout, or message this member.",
                self.score, self.observation.member_id, reasons
            ),
            mentions: MentionPolicy::None,
        })
    }
}

/// Already-claimed rows, including unflagged and bulk-suppressed evidence.
/// The production store must ensure event-ID uniqueness and serial ordering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedJoinRisk {
    pub guild_id: String,
    pub created_at_ms: i64,
}

/// Pure equivalent of the legacy store's recent-row WHERE clause. The caller
/// supplies claimed rows, not raw gateway deliveries. This does not claim IDs.
#[must_use]
pub fn count_recent_join_risks(
    rows: &[RecordedJoinRisk],
    guild_id: &str,
    processing_now_ms: i64,
    window_seconds: f64,
) -> u64 {
    let cutoff = processing_now_ms as f64 - window_seconds * 1000.0;
    rows.iter()
        .filter(|row| {
            row.guild_id == guild_id
                && row.created_at_ms as f64 > cutoff
                && row.created_at_ms <= processing_now_ms
        })
        .count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join(age_ms: i64) -> JoinRiskInput {
        JoinRiskInput {
            guild_id: "g1".into(),
            member_id: "m1".into(),
            member_is_bot: false,
            account_created_at_ms: 1_000_000_000 - age_ms,
            joined_at_ms: Some(1_000_000_000),
            source: "unknown".into(),
        }
    }

    fn policy(bulk_until: Option<i64>) -> JoinRiskPolicy {
        JoinRiskPolicy::new("g1".into(), 60.0, 5.0, bulk_until).unwrap()
    }

    #[test]
    fn defaults_and_numeric_validation() {
        let tuning = RaidTuning::default();
        assert_eq!(tuning.window_seconds(), 60.0);
        assert_eq!(tuning.threshold(), 5.0);
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(RaidTuning::new(bad, 5.0), Err(RaidConfigError::Window));
            assert_eq!(RaidTuning::new(60.0, bad), Err(RaidConfigError::Threshold));
        }
        assert!(RaidWatch::new(0.0, 0).is_ok());
        for bad in [-1.0, f64::NAN, f64::INFINITY] {
            assert!(matches!(
                RaidWatch::new(bad, 50),
                Err(RaidConfigError::Cooldown)
            ));
        }
    }

    #[test]
    fn ordinary_joins_do_not_alert_and_fifth_join_does() {
        let mut watch = RaidWatch::default();
        for i in 0..4 {
            assert!(watch
                .observe("g1", &i.to_string(), i * 1000, RaidTuning::default())
                .is_none());
        }
        let alert = watch
            .observe("g1", "4", 4000, RaidTuning::default())
            .unwrap();
        assert_eq!(alert.count, 5);
        assert_eq!(alert.span_seconds, 4);
        assert!(!alert.repeat);
        assert_eq!(alert.member_ids, ["0", "1", "2", "3", "4"]);
    }

    #[test]
    fn window_is_strict_and_late_joins_use_newest_occurrence() {
        let mut watch = RaidWatch::new(0.0, 50).unwrap();
        let tuning = RaidTuning::new(60.0, 3.0).unwrap();
        assert!(watch.observe("g", "new", 60_000, tuning).is_none());
        assert!(watch.observe("g", "boundary", 0, tuning).is_none());
        assert_eq!(watch.window_size("g"), 1);
        assert!(watch.observe("g", "inside", 1, tuning).is_none());
        let alert = watch.observe("g", "middle", 30_000, tuning).unwrap();
        assert_eq!(alert.member_ids, ["inside", "middle", "new"]);
        assert_eq!(alert.first_join_at, "1970-01-01T00:00:00.001Z");
        assert_eq!(alert.last_join_at, "1970-01-01T00:01:00.000Z");
    }

    #[test]
    fn dedupe_precedes_pruning_and_another_member_releases_rejoin() {
        let mut watch = RaidWatch::default();
        let tuning = RaidTuning::default();
        watch.observe("g", "same", 0, tuning);
        assert!(watch.observe("g", "same", 100_000, tuning).is_none());
        assert_eq!(watch.window_size("g"), 1);
        watch.observe("g", "other", 100_001, tuning);
        watch.observe("g", "same", 100_002, tuning);
        assert_eq!(watch.window_size("g"), 2);
    }

    #[test]
    fn cooldown_equality_and_repeat_after_quiet_period() {
        let mut watch = RaidWatch::default();
        let tuning = RaidTuning::new(60.0, 1.0).unwrap();
        assert!(!watch.observe("g", "first", 0, tuning).unwrap().repeat);
        assert!(watch.observe("g", "early", 899_999, tuning).is_none());
        let alert = watch.observe("g", "equal", 900_000, tuning).unwrap();
        assert!(alert.repeat);
        assert_eq!(alert.count, 2);
        assert!(
            watch
                .observe("g", "much-later", 9_000_000, tuning)
                .unwrap()
                .repeat
        );
    }

    #[test]
    fn guilds_are_isolated_and_id_cap_does_not_cap_count() {
        let mut watch = RaidWatch::new(0.0, 2).unwrap();
        let tuning = RaidTuning::new(60.0, 3.0).unwrap();
        for id in ["a", "b"] {
            watch.observe("g1", id, 0, tuning);
        }
        watch.observe("g2", "c", 0, tuning);
        assert_eq!(watch.window_size("g1"), 2);
        assert_eq!(watch.window_size("g2"), 1);
        let alert = watch.observe("g1", "c", 0, tuning).unwrap();
        assert_eq!(alert.count, 3);
        assert_eq!(alert.member_ids, ["a", "b"]);
        assert!(alert.truncated);
        assert_eq!(watch.window_size("g1"), 3);
        let text = alert.staff_message();
        assert!(text.content.contains("threshold: 5 in 60s"));
        assert!(text.content.contains("...and 1 more"));
        assert!(text.content.contains("kicked, banned and messaged nobody"));
        assert_eq!(text.mentions, MentionPolicy::None);
    }

    #[test]
    fn tuning_is_read_per_call_and_fractional_values_survive() {
        let mut watch = RaidWatch::new(0.0, 50).unwrap();
        let old = RaidTuning::new(60.0, 5.0).unwrap();
        watch.observe("g", "a", 0, old);
        watch.observe("g", "b", 1000, old);
        let next = RaidTuning::new(1.5, 1.5).unwrap();
        let alert = watch.observe("g", "c", 2000, next).unwrap();
        assert_eq!(alert.member_ids, ["b", "c"]);
        assert_eq!(alert.window_seconds, 1.5);
        assert!(alert
            .staff_message()
            .content
            .contains("threshold: 5 in 1.5s"));
    }

    #[test]
    fn span_rounds_like_legacy_math_round() {
        for (span, rounded) in [(499, 0), (500, 1), (1499, 1), (1500, 2)] {
            let mut watch = RaidWatch::default();
            let tuning = RaidTuning::new(60.0, 2.0).unwrap();
            watch.observe("g", "a", 0, tuning);
            let alert = watch.observe("g", "b", span, tuning).unwrap();
            assert_eq!(alert.span_seconds, rounded);
        }
    }

    #[test]
    fn account_age_boundaries_and_negative_age() {
        for (age, expected) in [
            (-1, 3),
            (86_399_999, 3),
            (86_400_000, 1),
            (604_799_999, 1),
            (604_800_000, 0),
        ] {
            let evidence = policy(None).prepare(&join(age), 0).unwrap().score(0);
            assert_eq!(evidence.score, expected, "age {age}");
            assert_eq!(evidence.flagged, expected >= 3);
        }
    }

    #[test]
    fn bots_and_other_guilds_are_ignored() {
        let mut input = join(1000);
        input.member_is_bot = true;
        assert!(policy(None).prepare(&input, 0).is_none());
        input.member_is_bot = false;
        input.guild_id = "g2".into();
        assert!(policy(None).prepare(&input, 0).is_none());
    }

    #[test]
    fn burst_includes_current_join_and_does_not_retroactively_flag() {
        for prior in 0..5 {
            let evidence = policy(None)
                .prepare(&join(2 * 86_400_000), 0)
                .unwrap()
                .score(prior);
            assert_eq!(evidence.score, if prior >= 4 { 3 } else { 1 });
            assert_eq!(evidence.flagged, prior >= 4);
            if prior == 4 {
                assert_eq!(
                    evidence.reasons,
                    ["account younger than 7 days", "5 joins inside 60s"]
                );
            }
        }
        let old = policy(None)
            .prepare(&join(10 * 86_400_000), 0)
            .unwrap()
            .score(4);
        assert_eq!(old.score, 2);
        assert!(!old.flagged);
    }

    #[test]
    fn bulk_suppression_is_inclusive_uses_join_time_and_preserves_score() {
        let input = join(1000);
        for (until, suppressed) in [
            (999_999_999, false),
            (1_000_000_000, true),
            (1_000_000_001, true),
        ] {
            let evidence = policy(Some(until))
                .prepare(&input, 9_000_000_000)
                .unwrap()
                .score(4);
            assert_eq!(evidence.score, 5);
            assert_eq!(evidence.observation.bulk_join_window, suppressed);
            assert_eq!(evidence.flagged, !suppressed);
            assert_eq!(evidence.observation.source, "unknown");
        }
    }

    #[test]
    fn event_identity_distinguishes_rejoins_and_fallback_clocks() {
        let mut input = join(1000);
        let first = policy(None).prepare(&input, 0).unwrap();
        let duplicate = policy(None).prepare(&input, 999).unwrap();
        assert_eq!(first.event_id, duplicate.event_id);
        assert_eq!(first.event_id, "g1:m1:1970-01-12T13:46:40.000Z");
        input.joined_at_ms = Some(1_000_000_001);
        assert_ne!(
            first.event_id,
            policy(None).prepare(&input, 0).unwrap().event_id
        );
        input.joined_at_ms = None;
        assert_ne!(
            policy(None).prepare(&input, 0).unwrap().event_id,
            policy(None).prepare(&input, 1).unwrap().event_id
        );
    }

    #[test]
    fn recent_count_uses_strict_processing_window_and_guild_isolation() {
        let rows = [
            RecordedJoinRisk {
                guild_id: "g".into(),
                created_at_ms: 0,
            },
            RecordedJoinRisk {
                guild_id: "g".into(),
                created_at_ms: 1,
            },
            RecordedJoinRisk {
                guild_id: "g".into(),
                created_at_ms: 60_000,
            },
            RecordedJoinRisk {
                guild_id: "g".into(),
                created_at_ms: 60_001,
            },
            RecordedJoinRisk {
                guild_id: "other".into(),
                created_at_ms: 1,
            },
        ];
        assert_eq!(count_recent_join_risks(&rows, "g", 60_000, 60.0), 2);
    }

    #[test]
    fn join_risk_fractional_threshold_and_window_are_not_truncated() {
        let policy = JoinRiskPolicy::new("g1".into(), 1.5, 2.5, None).unwrap();
        assert!(
            !policy
                .prepare(&join(2 * 86_400_000), 0)
                .unwrap()
                .score(1)
                .flagged
        );
        let evidence = policy.prepare(&join(2 * 86_400_000), 0).unwrap().score(2);
        assert!(evidence.flagged);
        assert_eq!(evidence.reasons[1], "3 joins inside 1.5s");
    }

    #[test]
    fn join_risk_alert_requires_a_real_insert_and_is_flag_only() {
        let evidence = policy(None).prepare(&join(1000), 0).unwrap().score(0);
        assert!(evidence.staff_message(false).is_none());
        let message = evidence.staff_message(true).unwrap();
        assert_eq!(message.mentions, MentionPolicy::None);
        assert_eq!(message.content, "**Join risk flag** — score 3.\nMember: `m1` · reasons: account younger than 24 hours.\nFlag only: Owen did not kick, ban, timeout, or message this member.");
    }
}
