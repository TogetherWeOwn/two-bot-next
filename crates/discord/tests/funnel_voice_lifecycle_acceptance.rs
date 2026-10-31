//! Protected acceptance: lifecycle gaps invalidate measured voice accounting.
//!
//! Breaks caught: missing scoped invalidation, roster-created starts, cross-gap
//! duration/XP, and over-broad invalidation that destroys valid foreign sessions.
//! This drives the real Pipeline/cache/handlers with explicit receipt times.
//! Facts and leveling are recording boundary sinks, not replacement accounting.
//! No database, gateway, room ownership, or reward-rate policy is exercised.
//!
//! Evidence limit: build_cache retains cached voice state on all three delete
//! variants below; the later null frame is a channel-to-None transition, not an
//! evicted-cache None-to-None reproduction. Invalidation may omit an end or emit
//! an honestly unknown end, but must never invent a measured boundary. The XP
//! assertion pins hook inputs, not persisted rewards. Open measured-start counts
//! are public; chain-map counts and concurrent lifecycle races are not covered.

use std::sync::Mutex;

use serde_json::json;
use twilight_model::{gateway::event::Event, id::Id};
use two_bot_core::{
    EventType, FactsSink, LevelOutcome, LevelingHook, MemStore, MemberJoinFact, MessageFact,
    MessageShape, RulesAcceptedFact, StoredRow, VoiceEndedFact, VoiceStartedFact,
};
use two_bot_discord::{NoClassification, Pipeline, ScriptedInvites};

const G: u64 = 100_000_000_000_000_001;
const FOREIGN: u64 = 100_000_000_000_000_002;
const A: u64 = 900_000_000_000_001_111;
const B: u64 = 900_000_000_000_002_222;
const ROOM: u64 = 300_000_000_000_000_001;
const OTHER_ROOM: u64 = 300_000_000_000_000_002;
const FOREIGN_ROOM: u64 = 300_000_000_000_000_003;

#[derive(Debug, Clone, PartialEq, Eq)]
struct End {
    guild: u64,
    member: u64,
    channel: u64,
    at: String,
    start: Option<String>,
    seconds: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Award {
    guild: u64,
    member: u64,
    channel: u64,
    at: String,
    seconds: u64,
}

#[derive(Default)]
struct Recording {
    starts: Mutex<Vec<(u64, u64, u64, String)>>,
    ends: Mutex<Vec<End>>,
    awards: Mutex<Vec<Award>>,
}

impl FactsSink for &Recording {
    fn record_member_join(&self, _: MemberJoinFact<'_>) {}
    fn record_rules_accepted(&self, _: RulesAcceptedFact<'_>) {}
    fn record_message(&self, _: MessageFact<'_>) {}
    fn record_voice_started(&self, fact: VoiceStartedFact<'_>) -> Option<String> {
        self.starts.lock().expect("starts").push((
            fact.guild_id,
            fact.member_id,
            fact.channel_id,
            fact.occurred_at.to_owned(),
        ));
        Some(format!(
            "{}:{}:{}:{}",
            fact.guild_id, fact.member_id, fact.channel_id, fact.occurred_at
        ))
    }
    fn record_voice_ended(&self, fact: VoiceEndedFact<'_>) {
        self.ends.lock().expect("ends").push(End {
            guild: fact.guild_id,
            member: fact.member_id,
            channel: fact.channel_id,
            at: fact.occurred_at.to_owned(),
            start: fact.started_at.map(str::to_owned),
            seconds: fact.duration_seconds,
        });
    }
}

impl LevelingHook for &Recording {
    fn award_message(&self, _: u64, _: u64, _: &str, _: u64, _: MessageShape) -> LevelOutcome {
        LevelOutcome {
            leveled_up: false,
            level: 0,
        }
    }
    fn award_voice(
        &self,
        guild: u64,
        member: u64,
        seconds: u64,
        at: &str,
        channel: u64,
    ) -> LevelOutcome {
        self.awards.lock().expect("awards").push(Award {
            guild,
            member,
            channel,
            at: at.to_owned(),
            seconds,
        });
        LevelOutcome {
            leveled_up: false,
            level: 0,
        }
    }
}

type Replay<'a> =
    Pipeline<MemStore, &'a Recording, &'a Recording, ScriptedInvites, NoClassification>;

fn replay(recording: &Recording) -> Replay<'_> {
    let p = Pipeline::new(
        MemStore::new(),
        Some(recording),
        Some(recording),
        ScriptedInvites::new(),
        NoClassification,
    );
    p.handle_at(&guild_create(G, &[]), &at(0));
    p.handle_at(&guild_create(FOREIGN, &[]), &at(0));
    p
}

fn at(seconds: u64) -> String {
    format!("2026-10-19T12:{:02}:{:02}.000Z", seconds / 60, seconds % 60)
}

fn user(member: u64) -> serde_json::Value {
    json!({"id": member.to_string(), "username": "fixture", "discriminator": "0", "bot": false})
}

fn voice_state(guild: u64, member: u64, channel: Option<u64>) -> serde_json::Value {
    json!({
        "guild_id": guild.to_string(), "user_id": member.to_string(),
        "channel_id": channel.map(|id| id.to_string()),
        "session_id": "fixture-session", "deaf": false, "mute": false,
        "self_deaf": false, "self_mute": false, "self_stream": false,
        "self_video": false, "suppress": false,
        "member": {"user": user(member), "roles": [], "deaf": false, "mute": false,
                   "joined_at": "2026-10-19T11:00:00.000+00:00", "flags": 0}
    })
}

fn voice(guild: u64, member: u64, channel: Option<u64>) -> Event {
    Event::VoiceStateUpdate(Box::new(
        serde_json::from_value(voice_state(guild, member, channel)).expect("voice fixture"),
    ))
}

fn guild_delete(guild: u64, unavailable: bool) -> Event {
    let mut value = json!({"id": guild.to_string()});
    if unavailable {
        value["unavailable"] = json!(true);
    }
    // Permanent removal omits unavailable, as in a real gateway payload.
    Event::GuildDelete(serde_json::from_value(value).expect("guild delete fixture"))
}

fn channel_delete(guild: u64, channel: u64) -> Event {
    Event::ChannelDelete(Box::new(
        serde_json::from_value(json!({
            "id": channel.to_string(), "guild_id": guild.to_string(), "type": 2,
            "name": "fixture-voice", "position": 0, "permission_overwrites": []
        }))
        .expect("channel delete fixture"),
    ))
}

fn guild_create(guild: u64, roster: &[(u64, u64)]) -> Event {
    let mut value: serde_json::Value =
        serde_json::from_str(include_str!("../../bot/tests/fixtures/voice_guild.json"))
            .expect("existing guild fixture");
    value["id"] = json!(guild.to_string());
    value["roles"][0]["id"] = json!(guild.to_string());
    value["channels"] = json!([ROOM, OTHER_ROOM, FOREIGN_ROOM].map(|channel| json!({
        "id": channel.to_string(), "guild_id": guild.to_string(), "type": 2,
        "name": "fixture-voice", "permission_overwrites": []
    })));
    value["members"] = json!(roster
        .iter()
        .map(|(member, channel)| { voice_state(guild, *member, Some(*channel))["member"].clone() })
        .collect::<Vec<_>>());
    value["voice_states"] = json!(roster
        .iter()
        .map(|(member, channel)| { voice_state(guild, *member, Some(*channel)) })
        .collect::<Vec<_>>());
    Event::GuildCreate(Box::new(
        twilight_model::gateway::payload::incoming::GuildCreate::Available(
            serde_json::from_value(value).expect("available guild fixture"),
        ),
    ))
}

fn ready() -> Event {
    Event::Ready(serde_json::from_value(json!({
        "v": 10, "user": {"id": "999", "username": "fixture-bot", "discriminator": "0", "bot": true, "mfa_enabled": false},
        "session_id": "fixture-ready", "resume_gateway_url": "wss://gateway.discord.gg",
        "guilds": [{"id": G.to_string(), "unavailable": true}, {"id": FOREIGN.to_string(), "unavailable": true}],
        "application": {"id": "1111", "flags": 0}
    })).expect("existing READY fixture shape"))
}

fn remove(guild: u64, member: u64) -> Event {
    Event::MemberRemove(
        serde_json::from_value(json!({
            "guild_id": guild.to_string(), "user": user(member)
        }))
        .expect("member remove fixture"),
    )
}

fn open(p: &Replay<'_>) -> usize {
    p.handlers()
        .voice_sessions
        .lock()
        .expect("voice tracker")
        .open_count()
}

fn boundaries(p: &Replay<'_>) -> Vec<StoredRow> {
    p.handlers()
        .store()
        .rows()
        .into_iter()
        .filter(|row| {
            matches!(
                row.event_type,
                EventType::VoiceSessionStart | EventType::VoiceSessionEnd
            )
        })
        .collect()
}

fn assert_no_measured_gap(p: &Replay<'_>, recording: &Recording, guild: u64, member: u64) {
    for row in boundaries(p).iter().filter(|row| {
        row.guild_id == guild
            && row.member_id == Some(member)
            && row.event_type == EventType::VoiceSessionEnd
    }) {
        let metadata = row.metadata.as_ref().expect("voice end metadata");
        assert_eq!(
            metadata["startKnown"], false,
            "cross-gap measured end: {row:?}"
        );
        assert!(metadata["startedAt"].is_null(), "cross-gap start: {row:?}");
        assert!(
            metadata["durationSeconds"].is_null(),
            "cross-gap duration: {row:?}"
        );
    }
    let ends = recording.ends.lock().expect("ends");
    for end in ends
        .iter()
        .filter(|end| end.guild == guild && end.member == member)
    {
        assert_eq!(end.start, None, "cross-gap fact: {end:?}");
        assert_eq!(end.seconds, None, "cross-gap fact: {end:?}");
    }
    assert!(
        recording
            .awards
            .lock()
            .expect("awards")
            .iter()
            .all(|award| award.guild != guild || award.member != member),
        "invalidated member must not reach XP hook"
    );
}

/// Narrow characterization of the upstream eviction assumption. This uses the
/// same cache configuration as Pipeline, independently of any accounting fix.
#[test]
fn cache_lifecycle_characterization() {
    for (label, event) in [
        ("unavailable", guild_delete(G, true)),
        ("removed", guild_delete(G, false)),
        ("channel", channel_delete(G, ROOM)),
    ] {
        let cache = two_bot_discord::build_cache();
        cache.update(&guild_create(G, &[]));
        cache.update(&voice(G, A, Some(ROOM)));
        assert_eq!(
            cache
                .voice_state(Id::new(A), Id::new(G))
                .map(|state| state.channel_id().get()),
            Some(ROOM)
        );
        cache.update(&event);
        let after = cache
            .voice_state(Id::new(A), Id::new(G))
            .map(|state| state.channel_id().get());
        assert_eq!(
            after,
            Some(ROOM),
            "pinned cache retains voice state after {label}"
        );
        cache.update(&voice(G, A, None));
        assert!(cache.voice_state(Id::new(A), Id::new(G)).is_none());
    }
}

#[test]
fn unavailable_return_roster_does_not_resurrect_start_or_award_across_gap() {
    let recording = Recording::default();
    let p = replay(&recording);
    p.handle_at(&voice(G, A, Some(ROOM)), &at(0));
    p.handle_at(&voice(FOREIGN, A, Some(FOREIGN_ROOM)), &at(0));
    p.handle_at(&guild_delete(G, true), &at(10));
    p.handle_at(&guild_delete(G, true), &at(11));
    p.handle_at(&guild_create(G, &[(A, ROOM), (B, OTHER_ROOM)]), &at(20));
    assert_eq!(
        recording.starts.lock().expect("starts").len(),
        2,
        "roster does not manufacture measured starts"
    );
    // Same-channel/mute frame after the roster is not evidence of a new join.
    p.handle_at(&voice(G, A, Some(ROOM)), &at(30));
    p.handle_at(&voice(G, A, None), &at(60));
    p.handle_at(&remove(G, B), &at(60));
    p.handle_at(&voice(FOREIGN, A, None), &at(60));
    assert_no_measured_gap(&p, &recording, G, A);
    assert_no_measured_gap(&p, &recording, G, B);
    assert_eq!(
        *recording.awards.lock().expect("awards"),
        vec![Award {
            guild: FOREIGN,
            member: A,
            channel: FOREIGN_ROOM,
            at: at(60),
            seconds: 60
        }]
    );
    assert_eq!(open(&p), 0);
}

#[test]
fn permanent_removal_and_return_roster_cannot_credit_old_start_on_member_leave() {
    let recording = Recording::default();
    let p = replay(&recording);
    p.handle_at(&voice(G, A, Some(ROOM)), &at(0));
    p.handle_at(&guild_delete(G, false), &at(10));
    p.handle_at(&guild_create(G, &[(A, ROOM)]), &at(20));
    p.handle_at(&remove(G, A), &at(60));
    assert_no_measured_gap(&p, &recording, G, A);
    assert_eq!(open(&p), 0);
}

#[test]
fn guild_return_roster_never_manufactures_a_measured_start() {
    let recording = Recording::default();
    let p = replay(&recording);
    p.handle_at(&guild_delete(G, true), &at(10));
    p.handle_at(&guild_create(G, &[(A, ROOM)]), &at(20));
    assert_eq!(open(&p), 0, "snapshot occupancy is not a measured join");
    assert!(recording.starts.lock().expect("starts").is_empty());
    assert!(boundaries(&p).is_empty());
    p.handle_at(&voice(G, A, None), &at(60));
    assert_no_measured_gap(&p, &recording, G, A);
}

#[test]
fn fresh_join_after_removal_only_measures_the_new_interval() {
    let recording = Recording::default();
    let p = replay(&recording);
    p.handle_at(&voice(G, A, Some(ROOM)), &at(0));
    p.handle_at(&guild_delete(G, false), &at(10));
    p.handle_at(&guild_create(G, &[]), &at(20));
    assert_eq!(open(&p), 0, "empty return cannot resurrect the old start");
    p.handle_at(&voice(G, A, Some(OTHER_ROOM)), &at(40));
    p.handle_at(&voice(G, A, None), &at(60));
    assert_eq!(
        *recording.awards.lock().expect("awards"),
        vec![Award {
            guild: G,
            member: A,
            channel: OTHER_ROOM,
            at: at(60),
            seconds: 20
        }]
    );
    assert_eq!(
        *recording.ends.lock().expect("ends"),
        vec![End {
            guild: G,
            member: A,
            channel: OTHER_ROOM,
            at: at(60),
            start: Some(at(40)),
            seconds: Some(20)
        }]
    );
    assert_eq!(open(&p), 0);
}

#[test]
fn ready_still_invalidates_all_guilds_without_snapshot_starts() {
    let recording = Recording::default();
    let p = replay(&recording);
    p.handle_at(&voice(G, A, Some(ROOM)), &at(0));
    p.handle_at(&voice(FOREIGN, A, Some(FOREIGN_ROOM)), &at(0));
    p.handle_at(&ready(), &at(10));
    assert_eq!(open(&p), 0);
    p.handle_at(&guild_create(G, &[(A, ROOM)]), &at(20));
    p.handle_at(&guild_create(FOREIGN, &[(A, FOREIGN_ROOM)]), &at(20));
    for guild in [G, FOREIGN] {
        p.handle_at(&voice(guild, A, None), &at(60));
        assert_no_measured_gap(&p, &recording, guild, A);
    }
    assert_eq!(recording.starts.lock().expect("starts").len(), 2);
}

#[test]
fn guild_unavailability_discards_starts_without_inventing_boundaries() {
    let recording = Recording::default();
    let p = replay(&recording);
    p.handle_at(&voice(G, A, Some(ROOM)), &at(0));
    p.handle_at(&voice(G, B, Some(OTHER_ROOM)), &at(0));
    p.handle_at(&voice(FOREIGN, A, Some(FOREIGN_ROOM)), &at(0));
    let before = boundaries(&p);
    for t in [10, 11] {
        p.handle_at(&guild_delete(G, true), &at(t));
    }
    assert_eq!(
        boundaries(&p),
        before,
        "invalidation is not an observed leave"
    );
    assert!(recording.ends.lock().expect("ends").is_empty());
    assert!(recording.awards.lock().expect("awards").is_empty());
    assert_eq!(
        open(&p),
        1,
        "only the foreign guild's measured start survives"
    );
}

#[test]
fn permanent_guild_removal_retires_starts_without_member_leave_frames() {
    let recording = Recording::default();
    let p = replay(&recording);
    for member in A..A + 32 {
        p.handle_at(&voice(G, member, Some(ROOM)), &at(0));
    }
    assert_eq!(open(&p), 32);
    p.handle_at(&guild_delete(G, false), &at(10));
    p.handle_at(&guild_delete(G, false), &at(11));
    assert_eq!(
        open(&p),
        0,
        "removed guild must not retain unbounded orphan starts"
    );
    assert!(recording.ends.lock().expect("ends").is_empty());
    assert!(recording.awards.lock().expect("awards").is_empty());
}

#[test]
fn channel_deletion_invalidates_only_occupants_of_that_channel() {
    let recording = Recording::default();
    let p = replay(&recording);
    for (guild, member, channel) in [(G, A, ROOM), (G, B, OTHER_ROOM), (FOREIGN, A, FOREIGN_ROOM)] {
        p.handle_at(&voice(guild, member, Some(channel)), &at(0));
    }
    let before = boundaries(&p);
    for t in [10, 11] {
        p.handle_at(&channel_delete(G, ROOM), &at(t));
    }
    assert_eq!(
        boundaries(&p),
        before,
        "delete must not manufacture end rows"
    );
    assert_eq!(
        open(&p),
        2,
        "same-guild other channel and foreign guild survive"
    );
    let tracker = p.handlers().voice_sessions.lock().expect("tracker");
    assert!(tracker.peek(G, A).is_none());
    assert_eq!(
        tracker.peek(G, B).expect("other channel").channel_id,
        OTHER_ROOM
    );
    assert_eq!(
        tracker.peek(FOREIGN, A).expect("foreign").channel_id,
        FOREIGN_ROOM
    );
}

#[test]
fn channel_delete_then_null_frame_cannot_credit_the_pre_delete_interval() {
    let recording = Recording::default();
    let p = replay(&recording);
    p.handle_at(&voice(G, A, Some(ROOM)), &at(0));
    p.handle_at(&channel_delete(G, ROOM), &at(10));
    p.handle_at(&voice(G, A, None), &at(60));
    let awards = recording.awards.lock().expect("awards").clone();
    assert!(
        awards.is_empty(),
        "deleted-channel gap must not reach the XP hook: {awards:?}"
    );
    assert_no_measured_gap(&p, &recording, G, A);
    assert_eq!(
        open(&p),
        0,
        "null frame must not strand a deleted-channel start"
    );
}

#[test]
fn channel_delete_then_member_removal_cannot_credit_the_pre_delete_interval() {
    let recording = Recording::default();
    let p = replay(&recording);
    p.handle_at(&voice(G, A, Some(ROOM)), &at(0));
    p.handle_at(&channel_delete(G, ROOM), &at(10));
    p.handle_at(&remove(G, A), &at(60));
    assert_no_measured_gap(&p, &recording, G, A);
    assert_eq!(open(&p), 0);
    assert!(
        p.handlers()
            .store()
            .rows()
            .iter()
            .any(|row| row.event_type == EventType::MemberLeave && row.member_id == Some(A)),
        "ordinary member leave is still recorded"
    );
}

#[test]
fn channel_delete_preserves_unaffected_members_full_duration_and_xp() {
    let recording = Recording::default();
    let p = replay(&recording);
    for (guild, member, channel) in [(G, A, ROOM), (G, B, OTHER_ROOM), (FOREIGN, A, FOREIGN_ROOM)] {
        p.handle_at(&voice(guild, member, Some(channel)), &at(0));
    }
    p.handle_at(&channel_delete(G, ROOM), &at(10));
    for (guild, member) in [(G, A), (G, B), (FOREIGN, A)] {
        p.handle_at(&voice(guild, member, None), &at(60));
    }
    assert_no_measured_gap(&p, &recording, G, A);
    assert_eq!(
        *recording.awards.lock().expect("awards"),
        vec![
            Award {
                guild: G,
                member: B,
                channel: OTHER_ROOM,
                at: at(60),
                seconds: 60
            },
            Award {
                guild: FOREIGN,
                member: A,
                channel: FOREIGN_ROOM,
                at: at(60),
                seconds: 60
            },
        ]
    );
    for (guild, member, channel) in [(G, B, OTHER_ROOM), (FOREIGN, A, FOREIGN_ROOM)] {
        let rows = boundaries(&p);
        let end = rows
            .iter()
            .find(|row| {
                row.guild_id == guild
                    && row.member_id == Some(member)
                    && row.event_type == EventType::VoiceSessionEnd
            })
            .expect("unaffected end");
        assert_eq!(end.source, format!("channel:{channel}"));
        assert_eq!(
            end.metadata,
            Some(json!({"startKnown": true, "startedAt": at(0), "durationSeconds": 60}))
        );
        assert!(recording.ends.lock().expect("ends").contains(&End {
            guild,
            member,
            channel,
            at: at(60),
            start: Some(at(0)),
            seconds: Some(60)
        }));
    }
    assert_eq!(open(&p), 0);
}

#[test]
fn move_after_channel_deletion_only_credits_the_new_observed_interval() {
    let recording = Recording::default();
    let p = replay(&recording);
    p.handle_at(&voice(G, A, Some(ROOM)), &at(0));
    p.handle_at(&channel_delete(G, ROOM), &at(10));
    p.handle_at(&voice(G, A, Some(OTHER_ROOM)), &at(40));
    // The old-channel end may be omitted or honestly unknown, never measured.
    assert_no_measured_gap(&p, &recording, G, A);
    p.handle_at(&voice(G, A, None), &at(60));
    assert_eq!(
        *recording.awards.lock().expect("awards"),
        vec![Award {
            guild: G,
            member: A,
            channel: OTHER_ROOM,
            at: at(60),
            seconds: 20
        }]
    );
    assert!(recording.ends.lock().expect("ends").contains(&End {
        guild: G,
        member: A,
        channel: OTHER_ROOM,
        at: at(60),
        start: Some(at(40)),
        seconds: Some(20)
    }));
    assert_eq!(open(&p), 0);
}

#[test]
fn unrelated_lifecycle_events_do_not_discard_a_valid_start() {
    let recording = Recording::default();
    let p = replay(&recording);
    p.handle_at(&voice(G, A, Some(ROOM)), &at(0));
    p.handle_at(&guild_delete(FOREIGN, true), &at(10));
    p.handle_at(&guild_delete(FOREIGN, false), &at(11));
    p.handle_at(&channel_delete(G, OTHER_ROOM), &at(12));
    p.handle_at(&voice(G, A, None), &at(60));
    assert_eq!(
        *recording.awards.lock().expect("awards"),
        vec![Award {
            guild: G,
            member: A,
            channel: ROOM,
            at: at(60),
            seconds: 60
        }]
    );
    assert_eq!(
        *recording.ends.lock().expect("ends"),
        vec![End {
            guild: G,
            member: A,
            channel: ROOM,
            at: at(60),
            start: Some(at(0)),
            seconds: Some(60)
        }]
    );
}

#[test]
fn normal_move_mute_and_member_removal_keep_order_and_measured_durations() {
    let recording = Recording::default();
    let p = replay(&recording);
    p.handle_at(&voice(G, A, Some(ROOM)), &at(0));
    let mut mute = voice(G, A, Some(ROOM));
    if let Event::VoiceStateUpdate(ref mut state) = mute {
        state.0.self_mute = true;
    }
    p.handle_at(&mute, &at(5));
    assert_eq!(boundaries(&p).len(), 1, "mute does not split a session");
    p.handle_at(&voice(G, A, Some(OTHER_ROOM)), &at(10));
    p.handle_at(&remove(G, A), &at(30));
    let rows = boundaries(&p);
    assert_eq!(
        rows.iter()
            .map(|row| (row.event_type, row.source.clone(), row.occurred_at.clone()))
            .collect::<Vec<_>>(),
        vec![
            (
                EventType::VoiceSessionStart,
                format!("channel:{ROOM}"),
                at(0)
            ),
            (
                EventType::VoiceSessionEnd,
                format!("channel:{ROOM}"),
                at(10)
            ),
            (
                EventType::VoiceSessionStart,
                format!("channel:{OTHER_ROOM}"),
                at(10)
            ),
            (
                EventType::VoiceSessionEnd,
                format!("channel:{OTHER_ROOM}"),
                at(30)
            ),
        ]
    );
    assert_eq!(
        *recording.awards.lock().expect("awards"),
        vec![
            Award {
                guild: G,
                member: A,
                channel: ROOM,
                at: at(10),
                seconds: 10
            },
            Award {
                guild: G,
                member: A,
                channel: OTHER_ROOM,
                at: at(30),
                seconds: 20
            },
        ]
    );
    assert_eq!(recording.starts.lock().expect("starts").len(), 2);
    assert_eq!(open(&p), 0);
}

#[test]
fn resumed_remains_a_global_accounting_gap() {
    let recording = Recording::default();
    let p = replay(&recording);
    p.handle_at(&voice(G, A, Some(ROOM)), &at(0));
    p.handle_at(&voice(FOREIGN, A, Some(FOREIGN_ROOM)), &at(0));
    p.handle_at(&Event::Resumed, &at(10));
    assert_eq!(open(&p), 0);
    for guild in [G, FOREIGN] {
        p.handle_at(&voice(guild, A, None), &at(60));
        assert_no_measured_gap(&p, &recording, guild, A);
    }
}
