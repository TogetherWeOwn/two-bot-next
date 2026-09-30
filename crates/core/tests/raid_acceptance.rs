//! Scripted staff-message acceptance only. The store below is a sequential
//! fixture, NOT proof of PostgreSQL transactions, durable claims or concurrency.
//! No HTTP client, gateway, live database, guild or token is used.

use std::collections::HashMap;

use two_bot_core::{
    count_recent_join_risks, JoinRiskEvidence, JoinRiskInput, JoinRiskObservation, JoinRiskPolicy,
    MentionPolicy, RaidTuning, RaidWatch, RecordedJoinRisk, StaffAlertMessage,
};

#[derive(Default)]
struct MockDiscord {
    attempts: Vec<StaffAlertMessage>,
    fail: bool,
}

impl MockDiscord {
    fn post(&mut self, message: StaffAlertMessage) -> Result<(), &'static str> {
        assert_eq!(message.mentions, MentionPolicy::None);
        self.attempts.push(message);
        if self.fail {
            Err("scripted send failure")
        } else {
            Ok(())
        }
    }
}

#[derive(Default)]
struct ClaimedRowsFixture {
    evidence: HashMap<String, JoinRiskEvidence>,
    rows: Vec<RecordedJoinRisk>,
}

impl ClaimedRowsFixture {
    fn claim(
        &mut self,
        observation: JoinRiskObservation,
        now_ms: i64,
    ) -> Option<StaffAlertMessage> {
        if self.evidence.contains_key(&observation.event_id) {
            return None;
        }
        let prior = count_recent_join_risks(
            &self.rows,
            &observation.guild_id,
            now_ms,
            observation.window_seconds(),
        );
        let evidence = observation.score(prior);
        self.rows.push(RecordedJoinRisk {
            guild_id: evidence.observation.guild_id.clone(),
            created_at_ms: now_ms,
        });
        let message = evidence.staff_message(true);
        self.evidence
            .insert(evidence.observation.event_id.clone(), evidence);
        message
    }
}

fn policy(bulk_until: Option<i64>) -> JoinRiskPolicy {
    JoinRiskPolicy::new("g".into(), 60.0, 5.0, bulk_until).unwrap()
}

fn join(member_id: &str, joined_ms: i64, age_ms: i64) -> JoinRiskInput {
    JoinRiskInput {
        guild_id: "g".into(),
        member_id: member_id.into(),
        member_is_bot: false,
        account_created_at_ms: joined_ms - age_ms,
        joined_at_ms: Some(joined_ms),
        source: "unknown".into(),
    }
}

#[test]
fn raid_failure_consumes_cooldown_without_retries_or_member_effects() {
    let mut watch = RaidWatch::default();
    let mut discord = MockDiscord {
        fail: true,
        ..Default::default()
    };
    for i in 0..4 {
        assert!(watch
            .observe("g", &i.to_string(), i * 1000, RaidTuning::default())
            .is_none());
    }
    let alert = watch
        .observe("g", "4", 4000, RaidTuning::default())
        .unwrap();
    assert!(discord.post(alert.staff_message()).is_err());
    assert!(watch
        .observe("g", "5", 5000, RaidTuning::default())
        .is_none());
    assert_eq!(discord.attempts.len(), 1);
    assert!(discord.attempts[0]
        .content
        .contains("the bot has kicked, banned and messaged nobody"));
    // The only effect this interface can emit is a staff-message proposal.
}

#[test]
fn sequential_burst_flags_only_the_current_member_and_persists_every_join() {
    let scorer = policy(None);
    let mut store = ClaimedRowsFixture::default();
    let mut discord = MockDiscord::default();
    for i in 0..5 {
        let input = join(&i.to_string(), 1_000_000_000 + i, 2 * 86_400_000);
        if let Some(message) = store.claim(scorer.prepare(&input, 0).unwrap(), 1_000_000_000 + i) {
            discord.post(message).unwrap();
        }
    }
    let mut scores = store.evidence.values().map(|e| e.score).collect::<Vec<_>>();
    scores.sort();
    assert_eq!(scores, [1, 1, 1, 1, 3]);
    assert_eq!(store.rows.len(), 5);
    assert_eq!(discord.attempts.len(), 1);
    assert!(discord.attempts[0].content.contains("Member: `4`"));
    assert!(discord.attempts[0].content.contains("Flag only"));
}

#[test]
fn duplicate_after_scorer_recreation_never_repairs_failed_announcement() {
    let input = join("young", 1_000_000_000, 1000);
    let mut store = ClaimedRowsFixture::default();
    let mut discord = MockDiscord {
        fail: true,
        ..Default::default()
    };
    let message = store
        .claim(policy(None).prepare(&input, 0).unwrap(), 1_000_000_000)
        .unwrap();
    assert!(discord.post(message).is_err());
    assert!(store
        .claim(policy(None).prepare(&input, 1).unwrap(), 1_000_000_001)
        .is_none());
    assert_eq!(store.rows.len(), 1);
    assert_eq!(discord.attempts.len(), 1);
    let rejoin = join("young", 1_000_000_001, 1000);
    assert!(store
        .claim(policy(None).prepare(&rejoin, 0).unwrap(), 1_000_000_002)
        .is_some());
    assert_eq!(store.rows.len(), 2);
}

#[test]
fn delayed_occurrences_count_by_processing_time_including_bulk_suppressed_rows() {
    let scorer = policy(Some(1_000_000_000));
    let mut store = ClaimedRowsFixture::default();
    for i in 0..4 {
        let mut input = join(&i.to_string(), 1_000_000_000 - i, 2 * 86_400_000);
        // Attribution is evidence only, not a suppression prerequisite.
        input.source = "web_one_click".into();
        assert!(store
            .claim(scorer.prepare(&input, 0).unwrap(), 2_000_000_000 + i)
            .is_none());
    }
    let input = join("after-bulk", 1_000_000_001, 2 * 86_400_000);
    let message = store
        .claim(scorer.prepare(&input, 0).unwrap(), 2_000_000_005)
        .unwrap();
    assert!(message.content.contains("5 joins inside 60s"));
    assert_eq!(store.rows.len(), 5);
    assert_eq!(store.evidence.values().filter(|e| e.flagged).count(), 1);
}

#[test]
fn raid_restart_loses_repeat_state_but_not_the_explicit_join_risk_fixture() {
    let tuning = RaidTuning::new(60.0, 1.0).unwrap();
    let first = RaidWatch::default()
        .observe("g", "same", 1000, tuning)
        .unwrap();
    let after_restart = RaidWatch::default()
        .observe("g", "same", 1000, tuning)
        .unwrap();
    assert!(!first.repeat);
    assert!(!after_restart.repeat);
    let input = join("same", 1000, 1);
    let mut store = ClaimedRowsFixture::default();
    assert!(store
        .claim(policy(None).prepare(&input, 0).unwrap(), 1000)
        .is_some());
    assert!(store
        .claim(policy(None).prepare(&input, 0).unwrap(), 1000)
        .is_none());
}

#[test]
fn ignored_members_never_reach_the_claim_or_alert_fixture() {
    let scorer = policy(None);
    let mut store = ClaimedRowsFixture::default();
    let mut bot = join("bot", 1000, 1);
    bot.member_is_bot = true;
    let mut other_guild = join("other", 1000, 1);
    other_guild.guild_id = "other".into();
    for input in [bot, other_guild] {
        if let Some(observation) = scorer.prepare(&input, 0) {
            store.claim(observation, 1000);
        }
    }
    assert!(store.rows.is_empty());
    assert!(store.evidence.is_empty());
}
